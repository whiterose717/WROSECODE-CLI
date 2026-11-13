use crate::config::Config;
use crate::ctf::CtfEngine;
use crate::harness::Harness;
use crate::memory::Memory;
use crate::metrics::Metrics;
use crate::provider::{Content, Message, Progress, Provider, ToolCall, Usage};
use crate::repo_map::RepoMap;
use crate::skills::{self, Skill};
use crate::think::{auto_think, AutoAction, ThinkLevel};
use crate::tools::{self, Tools};
use anyhow::{bail, Result};
use futures::future::join_all;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;

/// Transcript size (characters) at which the next turn summarizes the
/// conversation first: roughly 45k tokens of history, well inside every model
/// we ship. `WROSECODE_COMPACT_CHARS` overrides it (smaller context windows,
/// and the end-to-end test).
const DEFAULT_COMPACT_AT_CHARS: usize = 180_000;
/// Auto-compaction only fires once a conversation is long enough to be worth
/// summarizing; `/compact` bypasses this.
const MIN_COMPACT_MESSAGES: usize = 6;
/// Per-message cap on what the summarizer is shown, so one huge tool result
/// cannot crowd out the rest of the history.
const SUMMARY_INPUT_CAP: usize = 6_000;
/// The newest user request is copied through compaction verbatim (capped here
/// so one giant prompt cannot dominate the summary's wrapper).
const PINNED_REQUEST_CAP: usize = 8_000;

pub struct Agent {
    pub config: Arc<Config>,
    pub provider: Arc<dyn Provider>,
    pub tools: Tools,
    pub harness: Harness,
    pub messages: Vec<Message>,
    pub repo_map: Arc<Mutex<RepoMap>>,
    pub memory: Memory,
    pub skills: HashMap<String, Skill>,
    pub event_tx: Option<mpsc::UnboundedSender<Progress>>,
    pub mode: String,
    pub active_skill: Option<String>,
    pub last_api_latency: Option<Duration>,
    pub ctf: CtfEngine,
    /// The configured thinking mode (`off | low | medium | high | max | auto`).
    pub think: ThinkLevel,
    /// The live 0–20 strength this mode maps to — rewritten by the `auto`
    /// controller (and by `/think`, `Ctrl+T`, `[`, `]`).
    pub thinking_level: u8,
    /// Consecutive tool-failure steps in `auto` (escalates at 3).
    think_fail_streak: u8,
    /// The last three dispatched tool calls as `(name, canonical JSON
    /// input)` for the current query — a fourth identical call in a row
    /// trips the loop detector (see [`Agent::register_call`]).
    recent_calls: VecDeque<(String, String)>,
    /// An `auto` escalation is still awaiting its progress step.
    auto_escalated: bool,
    /// Reasoning tokens of the most recent model turn, for the dashboard.
    pub last_turn_reasoning: u64,
    pub usage: Usage,
    pub model_turns: usize,
    pub started_at: Instant,
    pub metrics: Metrics,
    pub task_results: Arc<Mutex<HashMap<String, String>>>,
    pub store: crate::store::Store,
    /// The Codex-style instruction chain (`AGENTS.md` ancestors +
    /// `WROSECODE.md`), read once at startup and injected into every system
    /// prompt.
    pub instructions: String,
    /// Transcript size (characters) that triggers auto-compaction before the
    /// next request; see `WROSECODE_COMPACT_CHARS`.
    pub compact_at_chars: usize,
    /// System prompt of the active user-defined agent
    /// (`.wrosecode/agents/*.md`), injected into every system prompt; empty
    /// for the built-in modes.
    pub agent_system: String,
    /// Display name of that agent; empty for the built-in modes.
    pub agent_name: String,
    /// Files pinned with `/add` (aider-style explicit file context): their
    /// contents ride along in every system prompt, capped, so the model never
    /// has to re-read them. Project-relative paths.
    pub pinned: Vec<String>,
    /// `--trace` timing sink: per-step API/tool timings as NDJSON. A default
    /// sink logs nowhere, so instrumentation never branches on the flag.
    pub trace: crate::trace::TraceSink,
    /// One short line per tool call executed by the current turn, for the
    /// end-of-task summary block (TUI result block and headless outcome).
    pub step_log: Vec<String>,
    /// Per-query goal, strike ledger, and search trigger. The recitation is
    /// rebuilt from this on every request and never enters the transcript.
    pub prompt: crate::prompt::PromptState,
}

/// Whether this turn runs on the configured planner model: the goose
/// planner/worker split applies to the read-only `plan` mode and only
/// when `[agent] planner` named a model.
fn plan_phase_uses_planner(mode: &str, config: &Config) -> bool {
    mode == "plan" && !config.planner_model.is_empty() && !config.planner_provider.is_empty()
}

/// Dispatch independent read-only probes and CTF hypothesis workers without
/// unnecessarily serializing them when the thinking level is low. Mutating
/// batches keep the existing conservative limits.
fn tool_batch_parallelism(
    mode: &str,
    thinking_level: u8,
    configured_max: usize,
    calls: &[ToolCall],
) -> usize {
    // All mutations in a model-issued batch must keep original call order.
    // At high think levels the old base could be 20, contradicting the
    // dispatcher's assumption that mutating batches are serialized. Concurrent
    // write_file/apply_patch/shell mutations can corrupt each other's state
    // and make the final answer inaccurate.
    if calls
        .iter()
        .any(|call| matches!(tools::classify(call), tools::ToolClass::Mutating))
    {
        return 1;
    }
    let base = match thinking_level {
        0..=3 => 1,
        4..=7 => 3,
        8..=12 => 5,
        _ => configured_max,
    };
    let read_only = calls.iter().all(|call| {
        matches!(
            call.name.as_str(),
            "read_file"
                | "grep"
                | "glob"
                | "decode"
                | "web_search"
                | "web_fetch"
                | "search_writeups"
        )
    });
    let ctf_hypotheses = mode == "ctf" && calls.iter().all(|call| call.name == "delegate_task");
    let floor = if read_only {
        8
    } else if ctf_hypotheses {
        4
    } else {
        1
    };
    base.max(floor)
        .min(configured_max.max(1))
        .min(calls.len().max(1))
}

/// Provider failures caused by transport/server conditions may recover on a
/// retry. Authentication and configuration failures will not, so repeating
/// them only freezes the UI and burns latency.
fn retryable_provider_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    ![
        "bad or missing api key",
        "invalid api key",
        "invalid_api_key",
        "unauthorized",
        "forbidden",
        "insufficient_quota",
        "billing hard limit",
        "not signed in",
        "not authorized",
        "plan usage is not authorized",
        "base url returned 404",
        "unknown provider",
        "unsupported provider",
        "needs a base url",
        "provider payload validation failed",
        "model is empty",
        "empty tool name",
        "unknown tool requested",
        "http 400",
        "http 401",
        "http 402",
        "http 403",
        "http 405",
        "http 413",
        "http 422",
        "model not found",
        "model does not exist",
        "context length exceeded",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

impl Agent {
    pub fn set_mode(&mut self, mode: &str) -> Result<()> {
        // `/mode bounty` is the short form used by the mode table; the
        // internal name predates it and the tools key off `bugbounty`.
        let mode = if mode == "bounty" { "bugbounty" } else { mode };
        if !matches!(mode, "build" | "plan" | "general" | "ctf" | "bugbounty") {
            bail!("unknown agent: {mode}");
        }
        if mode != "ctf" {
            self.ctf.unlock_category();
        } else if self.mode != "ctf" {
            self.ctf.begin_run();
        }
        // CTF and bug-bounty runs arm their own allowlist after this call;
        // every other mode must start with no inherited scope.
        if mode != "ctf" && mode != "bugbounty" {
            self.tools.set_scope(None);
        }
        self.mode = mode.into();
        // Only Plan is read-only. General has always been able to edit; a
        // regression here silently bricked file changes in General mode.
        self.tools.plan = mode == "plan";
        self.tools.bugbounty_mode = mode == "bugbounty";
        // The built-in modes carry no extra system prompt: selecting one drops
        // an active user-defined agent.
        self.agent_name.clear();
        self.agent_system.clear();
        Ok(())
    }

    /// Activate a user-defined markdown agent: its body is injected into every
    /// system prompt, its `mode:` is applied, and a `thinking:` level named in
    /// the file takes over.
    pub fn set_user_agent(&mut self, user: &crate::markdown::UserAgent) -> Result<()> {
        self.set_mode(&user.mode)?;
        if let Some(level) = user.thinking.as_deref().and_then(ThinkLevel::parse) {
            self.set_think(level);
        }
        self.agent_name = user.name.clone();
        self.agent_system = user.body.clone();
        Ok(())
    }

    /// `/add <path>` — pin a project file so its contents are injected into
    /// every system prompt (aider's explicit file context).
    pub fn pin(&mut self, path: &str) -> Result<String> {
        let relative = self.pin_path(path)?;
        if !self.pinned.contains(&relative) {
            self.pinned.push(relative.clone());
        }
        Ok(format!(
            "Pinned {relative} ({} file(s) in context)",
            self.pinned.len()
        ))
    }

    /// `/drop <path>` — unpin one file; `/drop all` clears the whole list.
    pub fn unpin(&mut self, path: &str) -> Result<String> {
        if path.eq_ignore_ascii_case("all") {
            let count = self.pinned.len();
            self.pinned.clear();
            return Ok(format!("Dropped {count} pinned file(s)"));
        }
        let relative = self.pin_path(path)?;
        let before = self.pinned.len();
        self.pinned.retain(|entry| entry != &relative);
        if self.pinned.len() == before {
            bail!("{relative} is not pinned");
        }
        Ok(format!(
            "Dropped {relative} ({} file(s) left)",
            self.pinned.len()
        ))
    }

    /// Resolve a pin target to a project-relative path of an existing file.
    fn pin_path(&self, path: &str) -> Result<String> {
        let resolved = crate::tools::fs::resolve(&self.config.root, path)?;
        if !resolved.is_file() {
            bail!("{} is not a file", resolved.display());
        }
        // fs::resolve returns canonical paths on macOS and Windows.
        // Canonicalize the root too, to preserve relative pin identity.
        let canonical_root =
            std::fs::canonicalize(&self.config.root).unwrap_or_else(|_| self.config.root.clone());
        Ok(resolved
            .strip_prefix(&canonical_root)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|_| resolved.display().to_string()))
    }

    /// The pinned files as a system-prompt block: 6k characters per file and
    /// 24k in total, so pinning a large source file cannot blow the budget.
    fn pinned_context(&self) -> String {
        const PER_FILE: usize = 6_000;
        const TOTAL: usize = 24_000;
        if self.pinned.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "Pinned files (selected by the user with /add; use them as context, do not re-read them):\n",
        );
        for name in &self.pinned {
            if out.len() >= TOTAL {
                out.push_str("(remaining pins omitted: context budget reached)\n");
                break;
            }
            out.push_str(&format!("=== {name} ===\n"));
            match std::fs::read_to_string(self.config.root.join(name)) {
                Ok(mut text) => {
                    if text.len() > PER_FILE {
                        let mut cut = PER_FILE;
                        while cut > 0 && !text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        text.truncate(cut);
                        text.push_str("\n… (truncated)");
                    }
                    out.push_str(&text);
                    out.push('\n');
                }
                Err(error) => out.push_str(&format!("(unreadable: {error})\n")),
            }
        }
        out
    }

    pub async fn review(&mut self, diff: &str) -> Result<String> {
        let system = "Review this uncommitted diff. Report actionable findings with file and line references, then missing tests. Do not propose or apply tool calls. If there are no findings, say so.";
        let messages = [Message {
            role: "user".into(),
            content: vec![Content::Text(diff.into())],
        }];
        let started = Instant::now();
        let response = self
            .provider
            .complete_with_think(system, &messages, &[], false, None, self.provider_think())
            .await?;
        self.last_api_latency = Some(started.elapsed());
        Ok(response
            .content
            .into_iter()
            .filter_map(|content| match content {
                Content::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub async fn commit_message(&mut self, diff: &str) -> Result<String> {
        let system = "Summarize this git diff as one imperative commit subject, at most 72 characters. Return only that subject, without quotes, markdown, or explanation.";
        let messages = [Message {
            role: "user".into(),
            content: vec![Content::Text(diff.chars().take(24_000).collect())],
        }];
        let started = Instant::now();
        let response = self
            .provider
            .complete_with_think(system, &messages, &[], false, None, self.provider_think())
            .await?;
        self.last_api_latency = Some(started.elapsed());
        let subject = response
            .content
            .into_iter()
            .find_map(|content| match content {
                Content::Text(text) => Some(text),
                _ => None,
            })
            .unwrap_or_default();
        let subject = subject
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        if subject.is_empty() {
            bail!("provider returned an empty commit message");
        }
        Ok(subject.chars().take(72).collect())
    }

    pub fn new(
        config: Arc<Config>,
        provider: Arc<dyn Provider>,
        client: reqwest::Client,
    ) -> Result<Self> {
        let skills = skills::discover(&config.root, &config.skill_dirs)?;
        let memory = Memory::new(&config.root);
        let harness = Harness::parse(&config.harness).unwrap_or(Harness::Minimal);
        Ok(Self {
            config: config.clone(),
            provider,
            tools: Tools::new(config.clone(), client),
            harness,
            messages: Vec::new(),
            repo_map: Arc::new(Mutex::new(RepoMap::default())),
            memory,
            skills,
            event_tx: None,
            mode: "build".into(),
            active_skill: None,
            last_api_latency: None,
            ctf: CtfEngine::new(&config.root).with_alert_bell(config.alert_bell),
            think: config.think,
            thinking_level: config.thinking_level,
            think_fail_streak: 0,
            recent_calls: VecDeque::new(),
            auto_escalated: false,
            last_turn_reasoning: 0,
            usage: Usage::default(),
            model_turns: 0,
            started_at: Instant::now(),
            metrics: Metrics::load(&config.root),
            task_results: Arc::new(Mutex::new(HashMap::new())),
            store: crate::store::Store::open_default()?,
            instructions: crate::project::instruction_chain(&config.root),
            compact_at_chars: threshold_from_env(),
            agent_system: String::new(),
            agent_name: String::new(),
            pinned: Vec::new(),
            step_log: Vec::new(),
            trace: crate::trace::TraceSink::default(),
            prompt: crate::prompt::PromptState::default(),
        })
    }

    pub async fn turn(&mut self, text: &str) -> Result<String> {
        self.step_log.clear();
        let _turn = self
            .trace
            .begin_turn(&text.chars().take(80).collect::<String>());
        self.repair_tool_history();
        self.compact_if_needed().await;
        // General/build prompts may also be CTF tasks. New tasks must not
        // reuse a previous challenge's flag as evidence.
        if self.mode != "ctf" && crate::prompt::is_ctf_intent(text) {
            self.ctf.begin_run();
        }
        if !self.ctf.category_locked() {
            self.ctf.category = crate::ctf::categorize(&self.config.root, text);
        }
        // CTF entry points preflight the selected target. An eager scan of
        // the whole cwd here can mistake an unrelated solved challenge's
        // artifact for the current flag and delays the first model request.
        self.messages.push(Message {
            role: "user".into(),
            content: vec![Content::Text(text.into())],
        });
        if let Some(skill) = self
            .active_skill
            .as_ref()
            .and_then(|name| self.skills.get(name))
            .or_else(|| skills::match_skill(&self.skills, text))
            // Bug-bounty briefs mention network verbs; forking a CTF skill
            // there would spend model calls on an unrelated child agent.
            .filter(|skill| skill.fork && self.mode != "bugbounty")
            .cloned()
        {
            let mut child_skills = self.skills.clone();
            if let Some(child_skill) = child_skills.get_mut(&skill.name) {
                child_skill.fork = false;
            }
            let task = format!("{}\n\nTask: {text}", skill.body_text()?);
            let mut child = Agent {
                config: self.config.clone(),
                provider: self.provider.clone(),
                tools: self.tools.clone(),
                harness: self.harness,
                messages: vec![Message {
                    role: "user".into(),
                    content: vec![Content::Text(task.clone())],
                }],
                repo_map: self.repo_map.clone(),
                memory: self.memory.clone(),
                skills: child_skills,
                event_tx: self.event_tx.clone(),
                mode: self.mode.clone(),
                active_skill: None,
                last_api_latency: None,
                ctf: self.ctf.clone(),
                think: self.think,
                thinking_level: self.thinking_level,
                think_fail_streak: 0,
                recent_calls: VecDeque::new(),
                auto_escalated: false,
                last_turn_reasoning: 0,
                usage: Usage::default(),
                model_turns: 0,
                started_at: Instant::now(),
                metrics: self.metrics.clone(),
                task_results: self.task_results.clone(),
                instructions: self.instructions.clone(),
                compact_at_chars: self.compact_at_chars,
                // Skill-fork children start with an empty pin list: they get
                // their own focused context and can read files if they need to.
                agent_system: self.agent_system.clone(),
                agent_name: self.agent_name.clone(),
                pinned: Vec::new(),
                step_log: Vec::new(),
                trace: self.trace.clone(),
                store: self.store.clone(),
                prompt: crate::prompt::PromptState::default(),
            };
            let summary = child.run(&task).await?;
            self.messages.push(Message {
                role: "user".into(),
                content: vec![Content::Text(format!(
                    "Forked skill {} summary:\n{summary}",
                    skill.name
                ))],
            });
        }
        self.run(text).await
    }

    /// A cancelled tool batch may leave assistant calls without results.
    /// Close those protocol entries before the next request; never replay
    /// unknown side effects just to repair the provider's message format.
    fn repair_tool_history(&mut self) {
        let mut pending = std::collections::BTreeMap::new();
        for message in &self.messages {
            for content in &message.content {
                match content {
                    Content::Call(call) => {
                        pending.insert(call.id.clone(), call.name.clone());
                    }
                    Content::Result { id, .. } => {
                        pending.remove(id);
                    }
                    _ => {}
                }
            }
        }
        if !pending.is_empty() {
            self.messages.push(Message {
                role: "user".into(),
                content: pending.into_iter().map(|(id, name)| Content::Result {
                    id, is_error: true,
                    output: format!("{name} was interrupted before its result was recorded. Inspect actual state before retrying; side effects may already have occurred."),
                }).collect(),
            });
        }
    }

    /// True once the conversation has outgrown `compact_at_chars` and the
    /// next turn should summarize it first.
    pub fn needs_compact(&self) -> bool {
        self.messages.len() >= MIN_COMPACT_MESSAGES
            && transcript_chars(&self.messages) > self.compact_at_chars
    }

    /// Auto-compaction: summarize a large history before the new request goes
    /// out (Codex auto-compaction, clai's state-preserving compaction). A
    /// failed summary is never fatal — the turn proceeds with the history it
    /// still has.
    async fn compact_if_needed(&mut self) {
        if self.needs_compact() && self.compact("auto").await.is_err() {
            // Keep the conversation as it is; the provider will tell us if it
            // no longer fits.
        }
    }

    /// Summarize the conversation and replace it with that summary. The new
    /// history is two messages — the summary as the latest user turn and an
    /// acknowledgement — so the request that follows reads as a continuation.
    /// Leaves the history untouched when the summarizer fails or returns
    /// nothing.
    pub async fn compact(&mut self, reason: &str) -> Result<String> {
        if self.messages.is_empty() {
            bail!("nothing to compact — the conversation is empty");
        }
        let before = transcript_chars(&self.messages);
        let count = self.messages.len();
        // Pin the newest request in flight: whatever the summarizer drops, the
        // user's actual ask rides along verbatim (clai's state-preserving
        // compaction).
        let pinned = self.messages.iter().rev().find_map(|message| {
            if message.role != "user" {
                return None;
            }
            message.content.iter().find_map(|content| match content {
                Content::Text(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
                _ => None,
            })
        });
        let history = summarise_input(&self.messages);
        let response = self
            .provider
            .complete_with_think(COMPACT_SYSTEM, &history, &[], false, None, ThinkLevel::Off)
            .await?;
        self.usage.add(response.usage);
        let summary = text_of(response.content);
        let summary = summary.trim();
        if summary.is_empty() {
            bail!("the summarizer returned an empty summary");
        }
        let summary: String = summary.chars().take(32_000).collect();
        let mut wrapped = format!(
            "This conversation was compacted to fit the context window \
             ({reason}: {before} characters, {count} messages). What follows \
             summarizes everything so far — treat it as the full history and \
             continue from it.\n\n{summary}"
        );
        if let Some(pinned) = pinned {
            let pinned: String = pinned.chars().take(PINNED_REQUEST_CAP).collect();
            wrapped.push_str(&format!(
                "\n\nThe request in flight when the history was cut, kept \
                 verbatim through compaction:\n{pinned}"
            ));
        }
        self.messages = vec![
            Message {
                role: "user".into(),
                content: vec![Content::Text(wrapped)],
            },
            Message {
                role: "assistant".into(),
                content: vec![Content::Text(
                    "Understood — continuing from that summary.".into(),
                )],
            },
        ];
        let after = transcript_chars(&self.messages);
        Ok(format!(
            "compacted {before} → {after} characters ({count} messages → 2)"
        ))
    }

    /// The concrete level for the next provider call: `auto` resolves to its
    /// live anchor, so providers only ever see a concrete level.
    fn provider_think(&self) -> ThinkLevel {
        ThinkLevel::from_level(self.thinking_level)
    }

    /// A manual level change (Ctrl+T, `[`/`]`, `/think`): set the mode, snap
    /// the live level to its anchor, and reset any `auto` escalation state so
    /// a fresh manual choice starts clean.
    pub fn set_think(&mut self, level: ThinkLevel) {
        self.think = level;
        self.thinking_level = level.anchor();
        self.think_fail_streak = 0;
        self.auto_escalated = false;
    }

    /// One `auto` decision after a tool step: failing steps raise the streak,
    /// clean steps clear it, three failures escalate, and the first clean
    /// step after an escalation drops back one level (spec PHASE 4).
    fn auto_step(&mut self, failures: usize) {
        if !self.think.is_auto() {
            return;
        }
        if failures > 0 {
            self.think_fail_streak = self.think_fail_streak.saturating_add(1);
        } else {
            self.think_fail_streak = 0;
        }
        let action = auto_think(
            self.think_fail_streak,
            self.thinking_level,
            self.auto_escalated,
            failures == 0,
        );
        self.run_auto_action(action);
    }

    /// The `auto` decision at the end of a run: an answer-only task drops to
    /// `low`, and a finished task releases any pending escalation.
    fn auto_finish(&mut self, used_tools: bool) {
        if !self.think.is_auto() {
            return;
        }
        let action = if !used_tools {
            self.think_fail_streak = 0;
            self.auto_escalated = false;
            AutoAction::Drop {
                to: ThinkLevel::Low,
                reason: "trivial task".into(),
            }
        } else if self.auto_escalated {
            let mut action = AutoAction::None;
            if let Some(to) = ThinkLevel::from_level(self.thinking_level).relax() {
                action = AutoAction::Drop {
                    to,
                    reason: "progress".into(),
                };
            }
            self.think_fail_streak = 0;
            self.auto_escalated = false;
            action
        } else {
            AutoAction::None
        };
        self.run_auto_action(action);
    }

    /// Apply one `auto` action: rewrite the live level and report the
    /// transition to the transcript as `think: medium → high (…)`.
    fn run_auto_action(&mut self, action: AutoAction) {
        let (to, reason) = match action {
            AutoAction::None => return,
            AutoAction::Escalate { to, reason } => {
                self.think_fail_streak = 0;
                self.auto_escalated = true;
                (to, reason)
            }
            AutoAction::Drop { to, reason } => (to, reason),
        };
        let from = ThinkLevel::from_level(self.thinking_level);
        if to.anchor() == self.thinking_level {
            return;
        }
        self.thinking_level = to.anchor();
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(Progress::Think {
                from: from.name().into(),
                to: to.name().into(),
                to_level: to.anchor(),
                reason,
            });
        }
    }

    /// Slide a dispatched call through the loop detector. Input is
    /// canonicalised as sorted-key JSON (`serde_json` map ordering), and
    /// the ring keeps the last three `(name, input)` pairs: the fourth
    /// identical call in a row with no other tool in between reports
    /// `true` so the caller can fail it instead of burning another
    /// round-trip on a stuck model. The streak resets every user query.
    fn register_call(&mut self, call: &ToolCall) -> bool {
        let key = (call.name.clone(), call.input.to_string());
        let looped = self.recent_calls.len() >= 3
            && self
                .recent_calls
                .iter()
                .rev()
                .take(3)
                .all(|last| *last == key);
        self.recent_calls.push_back(key);
        while self.recent_calls.len() > 3 {
            self.recent_calls.pop_front();
        }
        looped
    }

    /// Handle narrowly defined, read-only directory queries with one real
    /// shell tool invocation. This avoids model/tool/model loops and returns
    /// the exact captured bytes instead of a potentially incomplete summary.
    async fn run_quick_directory(&mut self, command: &str) -> Result<String> {
        let call = ToolCall {
            item_id: None,
            id: format!("fast-directory-{}", self.messages.len()),
            name: "shell".into(),
            input: serde_json::json!({"command": command, "timeout": "10"}),
        };
        let started = Instant::now();
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(Progress::ToolBegin {
                id: call.id.clone(),
                title: "Reading directories".into(),
            });
        }
        let output = self.tools.execute(&call).await;
        let ok = output.as_ref().is_ok_and(|s| s.starts_with("exit=0\n"));
        self.metrics.record_tool(ok);
        self.step_log.push(step_title(&call, ok));
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(Progress::ToolEnd {
                id: call.id.clone(),
                ok,
                output: crate::crash::redact(match &output {
                    Ok(value) => value,
                    Err(_) => "directory probe failed",
                }),
                elapsed_ms: started.elapsed().as_millis(),
            });
        }
        let output = output?;
        let Some(exact) = output.strip_prefix("exit=0\n") else {
            bail!("directory probe failed: {}", tools::truncate(output, 1_000));
        };
        // A silent truncation would be an accuracy bug. Tell the caller to
        // narrow an unusually large listing instead of presenting partial data.
        if exact.len() > 64_000 {
            bail!("directory listing exceeds 64 KiB; narrow the target directory");
        }
        let answer = if exact.trim().is_empty() {
            "(no directories found)".to_string()
        } else {
            exact.trim_end_matches('\n').to_string()
        };
        self.last_api_latency = None;
        self.last_turn_reasoning = 0;
        self.messages.push(Message {
            role: "assistant".into(),
            content: vec![Content::Text(answer.clone())],
        });
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(Progress::TextDelta(answer.clone()));
        }
        self.auto_finish(true);
        Ok(answer)
    }

    async fn run(&mut self, query: &str) -> Result<String> {
        // The loop detector and the strike ledger are per query: a repeated
        // setup across user turns is not a stuck model.
        self.recent_calls.clear();
        self.prompt.begin(query);
        self.trace.event(
            "prompt_submitted",
            &format!("chars={} mode={}", query.chars().count(), self.mode),
        );
        // Preserve user-defined agent/skill behavior and CTF orchestration;
        // only standalone, unambiguous read-only probes take this fast path.
        if self.mode == "build"
            && self.agent_name.is_empty()
            && self.active_skill.is_none()
            && self.pinned.is_empty()
        {
            if let Some(command) = quick_directory_command(query, &self.config.root) {
                return self.run_quick_directory(&command).await;
            }
        }
        let mut last_text = String::new();
        let mut repairs = 0;
        let mut used_tools_this_turn = false;
        let ctf_task = self.mode == "ctf" || crate::prompt::is_ctf_intent(query);
        let mut ctf_verification_reprompted = false;
        // Greetings never need a repo walk, skill expansion, dozens of tool
        // schemas, or medium/high reasoning. Keep explicit /think unchanged
        // for substantive Build and CTF requests.
        let quick_greeting = self.agent_name.is_empty() && is_quick_greeting(query);
        let quick_listing =
            self.agent_name.is_empty() && self.mode != "ctf" && is_directory_listing(query);
        let lightweight = quick_greeting || quick_listing;
        let mut turn_map: Option<String> = None;
        // The goose planner/worker split: in the read-only `plan` mode a
        // separately configured planner model does the thinking; every
        // other mode runs the worker (the main provider). Built once per
        // run, so settings are read a single time per user turn.
        let planner = if plan_phase_uses_planner(&self.mode, &self.config) {
            crate::provider::create(
                &self.config.planner_provider,
                &self.config.planner_model,
                self.tools.client.clone(),
            )
            .ok()
        } else {
            None
        };
        loop {
            let context_started = Instant::now();
            // Filesystem walk plus PageRank stay off the async executor: a
            // cold tree costs seconds, and blocking here would stall model
            // streams, tool I/O, and cancellation on every worker. The map
            // is taken out of its mutex, moved through a blocking worker,
            // and restored — the render cache inside makes repeated turns
            // over unchanged trees nearly free.
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(Progress::ModelStage("Preparing request context…".into()));
            }
            let map = if lightweight || ctf_task {
                // Standalone CTF source/protocol problems do not benefit
                // from a broad repository symbol walk every model turn.
                String::new()
            } else if let Some(cached) = &turn_map {
                cached.clone()
            } else {
                let taken = {
                    let mut guard = self
                        .repo_map
                        .lock()
                        .map_err(|_| anyhow::anyhow!("repo map poisoned"))?;
                    std::mem::take(&mut *guard)
                };
                let root = self.config.root.clone();
                let query_owned = query.to_string();
                let maps = self.repo_map.clone();
                let _map_trace = self.trace.guard("repo_map", String::new());
                let worker = tokio::task::spawn_blocking(move || {
                    let mut taken = taken;
                    let result = taken.update(&root).map(|()| taken.render(&query_owned));
                    // Restore ownership inside the worker even if the caller
                    // cancels or its context deadline expires.
                    if let Ok(mut guard) = maps.lock() {
                        *guard = taken;
                    }
                    result
                });
                // Optional context cannot hold a user request hostage.
                let map = match tokio::time::timeout(Duration::from_millis(500), worker).await {
                    Ok(Ok(Ok(map))) => map,
                    _ => String::new(),
                };
                turn_map = Some(map.clone());
                map
            };
            let memory = if lightweight {
                String::new()
            } else {
                self.memory.recall(query)?
            };
            let skill = if lightweight || self.mode == "bugbounty" {
                None
            } else {
                self.active_skill
                    .as_ref()
                    .and_then(|name| self.skills.get(name))
                    .or_else(|| skills::match_skill(&self.skills, query))
            };
            let skill_text = match skill {
                Some(s) if s.fork => format!(
                    "Skill {} is forked; use delegate_task for it. {}",
                    s.name, s.description
                ),
                Some(s) => s.body_text()?,
                None => String::new(),
            };
            let instructions = if self.instructions.is_empty() {
                String::new()
            } else {
                format!(
                    "Project instructions (AGENTS.md chain):\n{}\n",
                    self.instructions
                )
            };
            let overlay = if self.agent_system.is_empty() {
                String::new()
            } else {
                format!(
                    "Active user agent ({}):\n{}\n",
                    self.agent_name, self.agent_system
                )
            };
            let pinned = self.pinned_context();
            // Byte-stable prefix first: harness, agent overlay, AGENTS.md
            // chain, pins, and session constants stay identical between
            // requests so prompt caches (Anthropic `cache_control`,
            // OpenAI-style automatic prefix caching) can hit. The volatile
            // tail — think level, recall, repo map, skill — follows the
            // boundary marker.
            let stable = format!(
                "{}\n{}{}{}Project: {}\nMode: {}\nCTF category: {}\n{}\n",
                self.harness.prompt(),
                overlay,
                instructions,
                pinned,
                self.config.root.display(),
                self.mode,
                self.ctf.category,
                crate::prompt::constitution(&self.mode),
            );
            let ctf_mode = if self.mode == "ctf" {
                "CTF MODE: operate only in the approved scope. Prioritize a working reproducible solver and observed flag evidence; verify the solver's exit status and recovered bytes, never claim a flag from model text alone.\n"
            } else if crate::prompt::is_ctf_intent(query) {
                crate::prompt::CTF_GENERAL_TASK_GUIDANCE
            } else {
                ""
            };
            let bounty_mode = if self.mode == "bugbounty" {
                crate::bugbounty::doctrine()
            } else {
                ""
            };
            let plan_items = self
                .tools
                .checklist
                .lock()
                .map(|items| items.clone())
                .unwrap_or_default();
            // Recency anchor: environment, task tree, and the one pending
            // directive sit at the very end of the prompt, after the cheap
            // volatile fields. Nothing here may move into the stable prefix
            // (the date, plan, and strike counts change between requests).
            let tail = crate::prompt::volatile_tail(
                &self.prompt,
                &plan_items,
                &self.config.root,
                &self.mode,
                self.tools.plan,
            );
            let volatile = format!(
                "{ctf_mode}{bounty_mode}Thinking level: {}/20 (mode {}). At higher levels, use independent delegate_task calls, checker tasks, and race strategies when useful; never exceed 20 concurrent tasks.\nRelevant memory:\n{}\nRepo map:\n{}\nSkill:\n{}\n{}",
                self.thinking_level,
                self.think.name(),
                memory,
                map,
                skill_text,
                tail
            );
            let system = if quick_greeting {
                format!("You are WROSECODE, a helpful coding and CTF assistant. Respond warmly and very briefly to this greeting. Do not use tools.\n{instructions}{overlay}{pinned}")
            } else if quick_listing {
                format!("You are WROSECODE. Inspect the requested directory using shell or glob and report actual results. Do not analyze repositories or delegate this simple listing.\nProject: {}\n{instructions}{overlay}{pinned}", self.config.root.display())
            } else {
                format!(
                    "{stable}{}{volatile}",
                    crate::provider::SYSTEM_VOLATILE_MARK
                )
            };
            self.trace.log(
                "context_build",
                context_started.elapsed().as_millis(),
                &format!(
                    "system_chars={} messages={} schemas_pending=1",
                    system.len(),
                    self.messages.len()
                ),
            );
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(Progress::ResetText);
            }
            let started = Instant::now();
            let schemas = if quick_greeting {
                Vec::new()
            } else if quick_listing {
                self.tools
                    .schemas()
                    .into_iter()
                    .filter(|schema| {
                        matches!(
                            schema["name"].as_str(),
                            Some("shell" | "glob" | "read_file")
                        )
                    })
                    .collect()
            } else {
                self.tools.schemas()
            };
            let mut selected_provider = planner.clone().unwrap_or_else(|| self.provider.clone());
            let mut retry_think = if lightweight {
                ThinkLevel::Off
            } else {
                self.provider_think()
            };
            let mut response = None;
            let mut last_error = None;
            let mut capabilities_logged = false;
            for attempt in 0..3 {
                if !capabilities_logged {
                    capabilities_logged = true;
                    let caps = selected_provider.capabilities();
                    self.trace.event(
                        "provider_capabilities",
                        &format!(
                            "model={} streaming={} tools={} parallel_tools={} cache={} reasoning={} usage={}",
                            self.config.model,
                            caps.streaming,
                            caps.tool_calling,
                            caps.parallel_tool_calls,
                            caps.prompt_cache,
                            caps.reasoning_controls,
                            caps.usage_reporting
                        ),
                    );
                }
                let attempt_started = Instant::now();
                self.trace.event(
                    "provider_request_started",
                    &format!(
                        "attempt={} schemas={} messages={} think={}",
                        attempt + 1,
                        schemas.len(),
                        self.messages.len(),
                        retry_think.name()
                    ),
                );
                if let Some(tx) = &self.event_tx {
                    let stage = if attempt == 0 {
                        "Sending prompt to model".to_string()
                    } else {
                        format!(
                            "Retrying model request ({}/3) · think {}",
                            attempt + 1,
                            retry_think.name()
                        )
                    };
                    let _ = tx.send(Progress::ModelStage(stage));
                }
                if attempt > 0 {
                    if let Some(tx) = &self.event_tx {
                        let _ = tx.send(Progress::ResetText);
                    }
                }
                // Recovery must preserve project instructions, CTF scope,
                // pinned files and the user's task context on every attempt.
                let prompt = system.clone();
                match selected_provider
                    .complete_with_think(
                        &prompt,
                        &self.messages,
                        &schemas,
                        quick_listing && !used_tools_this_turn && !schemas.is_empty(),
                        self.event_tx.as_ref(),
                        retry_think,
                    )
                    .await
                {
                    Ok(value) => {
                        let tool_calls = value
                            .content
                            .iter()
                            .filter(|content| matches!(content, Content::Call(_)))
                            .count();
                        self.trace.log(
                            "provider_response",
                            attempt_started.elapsed().as_millis(),
                            &format!(
                                "attempt={} tool_calls={} input={} output={} reasoning={} cache_read={} estimated={}",
                                attempt + 1,
                                tool_calls,
                                value.usage.input,
                                value.usage.output,
                                value.usage.reasoning,
                                value.usage.cache_read,
                                value.usage.estimated
                            ),
                        );
                        response = Some(value);
                        break;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let kind = crate::provider::classify_error(&message);
                        let lower = message.to_ascii_lowercase();
                        let rate_limited = lower.contains("rate limited") || lower.contains("429");
                        let retryable = retryable_provider_error(&message);
                        self.trace.log(
                            kind.as_str(),
                            attempt_started.elapsed().as_millis(),
                            &format!(
                                "attempt={} retryable={} {}",
                                attempt + 1,
                                retryable,
                                message
                            ),
                        );
                        last_error = Some(error);

                        // Fail over after the first failed primary request, not
                        // after two full failures. This is especially important
                        // when a dead endpoint consumes the inactivity timeout.
                        if attempt == 0
                            && !self.config.fallback_provider.is_empty()
                            && !self.config.fallback_model.is_empty()
                        {
                            if let Ok(provider) = crate::provider::create(
                                &self.config.fallback_provider,
                                &self.config.fallback_model,
                                self.tools.client.clone(),
                            ) {
                                selected_provider = provider;
                                retry_think = self.provider_think();
                                continue;
                            }
                        }

                        if !retryable {
                            break;
                        }
                        if !rate_limited
                            && (lower.contains("timeout")
                                || lower.contains("timed out")
                                || lower.contains("stalled"))
                        {
                            retry_think = match retry_think {
                                ThinkLevel::Max | ThinkLevel::High => ThinkLevel::Medium,
                                ThinkLevel::Medium => ThinkLevel::Low,
                                ThinkLevel::Low | ThinkLevel::Off | ThinkLevel::Auto => {
                                    ThinkLevel::Off
                                }
                            };
                        }
                        let delay = if rate_limited {
                            // A lower thinking budget often turns a provider
                            // token-rate 429 into a successful retry. Keep
                            // the configured level unchanged for later turns.
                            retry_think = ThinkLevel::from_level(
                                self.thinking_level.saturating_sub(5 * (attempt as u8 + 1)),
                            );
                            Duration::from_secs(2_u64 << attempt.min(4))
                        } else {
                            Duration::from_millis(250 * (1 << attempt))
                        };
                        if attempt < 2 {
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }
            let mut response = response.ok_or_else(|| {
                last_error.unwrap_or_else(|| anyhow::anyhow!("provider failed without an error"))
            })?;
            let has_tool_call = response
                .content
                .iter()
                .any(|content| matches!(content, Content::Call(_)));
            if !has_tool_call && !used_tools_this_turn && task_requires_tool(query) {
                if let Some(tx) = &self.event_tx {
                    let _ = tx.send(Progress::ResetText);
                }
                let first_usage = response.usage;
                let mut forced = selected_provider
                    .complete_with_think(
                        &system,
                        &self.messages,
                        &schemas,
                        true,
                        self.event_tx.as_ref(),
                        self.provider_think(),
                    )
                    .await?;
                forced.usage.add(first_usage);
                response = forced;
            }
            self.last_api_latency = Some(started.elapsed());
            self.trace.log(
                "provider",
                started.elapsed().as_millis(),
                &format!("{} turns", self.model_turns),
            );
            self.model_turns += 1;
            self.usage.add(response.usage);
            self.last_turn_reasoning = response.usage.reasoning;
            self.metrics.record_model(
                &self.config.model,
                response.usage,
                started.elapsed().as_millis() as u64,
            );
            crate::telemetry::export(&self.tools.client, &self.metrics.snapshot()).await;
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(Progress::Usage(response.usage));
                let _ = tx.send(Progress::Metrics(self.metrics.snapshot()));
            }
            let calls: Vec<ToolCall> = response
                .content
                .iter()
                .filter_map(|c| {
                    if let Content::Call(t) = c {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
                .collect();
            // Loop detector: the fourth identical (name, input) call in a
            // row fails locally with a clear message instead of burning
            // another identical round-trip. The strike ledger closes a path
            // that already failed three times while it stays in that state.
            let mut looped = HashSet::new();
            let mut tripped = HashSet::new();
            for call in &calls {
                if self.register_call(call) {
                    looped.insert(call.id.clone());
                } else if self
                    .prompt
                    .strikes
                    .exhausted(&crate::prompt::signature(&call.name, &call.input))
                {
                    tripped.insert(call.id.clone());
                }
            }
            let text = response
                .content
                .iter()
                .filter_map(|c| {
                    if let Content::Text(t) = c {
                        Some(t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                last_text = text;
                // Generic CTF prompts must not persist or auto-copy
                // model-only guesses as verified challenge flags.
                self.emit_flags(self.mode == "ctf", "model response", &last_text)
                    .await;
            }
            self.messages.push(Message {
                role: "assistant".into(),
                content: response.content,
            });
            if calls.is_empty() {
                if ctf_task {
                    // A model claim alone is not evidence. Require a
                    // matching recorded tool output or re-derived artifact.
                    if let Some(found) =
                        crate::autopilot::verify(&self.ctf, &self.ctf.current_hits())
                    {
                        self.auto_finish(used_tools_this_turn);
                        return Ok(format!(
                            "Verified flag: {} ({})",
                            found.flag, found.evidence
                        ));
                    }
                    if !self.ctf.detect("model response", &last_text).is_empty() {
                        if used_tools_this_turn && !ctf_verification_reprompted {
                            // One final chance to execute a real check. Do
                            // not incur extra model calls for plainly unsolved
                            // tasks, or repeat this verification indefinitely.
                            ctf_verification_reprompted = true;
                            self.messages.push(Message {
                                role: "user".into(),
                                content: vec![Content::Text(
                                    "CTF VERIFICATION GATE: The previous answer contains a flag                                      candidate but no successful tool execution or reproducible                                      artifact supports it. Run the authorized dynamic solver                                      or checker, inspect exit status and actual output.                                      If verification fails, report UNSOLVED with the error."
                                        .into(),
                                )],
                            });
                            continue;
                        }
                        self.auto_finish(used_tools_this_turn);
                        return Ok(format!(
                            "UNVERIFIED CTF candidate — no tool or artifact evidence.\n\n{}",
                            last_text
                        ));
                    }
                }
                // Answer-only turns can finish without an extra provider call.
                self.auto_finish(used_tools_this_turn);
                return Ok(last_text);
            }
            used_tools_this_turn = true;
            for call in &calls {
                let detail = if call.name == "update_plan" {
                    format!(
                        "{} items",
                        call.input["items"]
                            .as_array()
                            .map_or(0, |items| items.len())
                    )
                } else {
                    call.input["path"]
                        .as_str()
                        .or_else(|| call.input["pattern"].as_str())
                        .or_else(|| call.input["command"].as_str())
                        .or_else(|| call.input["url"].as_str())
                        .or_else(|| call.input["query"].as_str())
                        .or_else(|| call.input["task"].as_str())
                        .unwrap_or("")
                        .to_string()
                };
                if let Some(tx) = &self.event_tx {
                    let _ = tx.send(Progress::ToolBegin {
                        id: call.id.clone(),
                        title: crate::crash::redact(&format_tool_title(&call.name, &detail)),
                    });
                }
            }
            let provider = self.provider.clone();
            let config = self.config.clone();
            let tools = self.tools.clone();
            let harness = self.harness;
            let map = self.repo_map.clone();
            let event_tx = self.event_tx.clone();
            // Delegate children inherit the live thinking state, not the
            // startup defaults, so `auto` escalations reach them too.
            let think = self.think;
            let live_level = self.thinking_level;
            let parallel_limit = Arc::new(Semaphore::new(tool_batch_parallelism(
                &self.mode,
                live_level,
                self.config.max_parallel_tasks,
                &calls,
            )));
            // Per-resource-class gates apply only to batches that contain no
            // mutation. Mutating batches are already serialized by
            // `tool_batch_parallelism`, and keeping their dispatch on one
            // permit preserves the existing ordering guarantees.
            let batch_mutating = calls
                .iter()
                .any(|call| matches!(tools::classify(call), tools::ToolClass::Mutating));
            let class_limits = (!batch_mutating)
                .then(|| tools::ToolClassLimits::new(self.config.max_parallel_tasks));
            let task_results = self.task_results.clone();
            let metrics = self.metrics.clone();
            let store = self.store.clone();
            let agent_system = self.agent_system.clone();
            let agent_name = self.agent_name.clone();
            let trace = self.trace.clone();
            let child_mode = self.mode.clone();
            let child_ctf = self.ctf.clone();
            // Snapshot every file this batch is about to touch, so `/undo`
            // can restore the pre-edit state (`src/snapshot.rs`).
            let mut snapshot_paths: Vec<std::path::PathBuf> = Vec::new();
            for call in &calls {
                let touched: Vec<String> = match call.name.as_str() {
                    "write_file" | "edit_file" | "search_replace" => call.input["path"]
                        .as_str()
                        .map(|path| vec![path.to_string()])
                        .unwrap_or_default(),
                    "apply_patch" => call.input["patch"]
                        .as_str()
                        .map(crate::tools::patch::paths)
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                for path in touched {
                    if let Ok(resolved) = crate::tools::fs::resolve(&self.config.root, &path) {
                        if !snapshot_paths.contains(&resolved) {
                            snapshot_paths.push(resolved);
                        }
                    }
                }
            }
            let snapshot = if snapshot_paths.is_empty() || self.tools.plan {
                None
            } else {
                crate::snapshot::capture(&self.config.root, &snapshot_paths)
            };
            let batch_started = Instant::now();
            let results = join_all(calls.iter().map(|call| {
                let provider = provider.clone();
                let config = config.clone();
                let tools = tools.clone();
                let map = map.clone();
                let event_tx = event_tx.clone();
                let parallel_limit = parallel_limit.clone();
                let task_results = task_results.clone();
                let metrics = metrics.clone();
                let store = store.clone();
                let agent_system = agent_system.clone();
                let trace = trace.clone();
                let agent_name = agent_name.clone();
                let child_mode = child_mode.clone();
                let child_ctf = child_ctf.clone();
                let class_limits = class_limits.clone();
                let looped = looped.clone();
                let tripped = tripped.clone();
                async move {
                    if tripped.contains(&call.id) {
                        trace.event("tool_skipped", &format!("{} circuit_breaker", call.name));
                        return (
                            call.id.clone(),
                            Err(anyhow::anyhow!(
                                "circuit breaker: `{}` has already failed three times with these exact arguments and is refused for this query — mark the step blocked and pivot or call web_search for an alternative",
                                call.name
                            )),
                            0,
                        );
                    }
                    if looped.contains(&call.id) {
                        trace.event(
                            "tool_skipped",
                            &format!("{} loop_detected", call.name),
                        );
                        return (
                            call.id.clone(),
                            Err(anyhow::anyhow!(
                                "loop detected: the identical `{}` call has now run three times in a row with no change — try a different approach",
                                call.name
                            )),
                            0,
                        );
                    }
                    let class = crate::tools::classify(call);
                    let queued = Instant::now();
                    let _class_permit = match &class_limits {
                        Some(limits) => limits.for_call(class).acquire_owned().await.ok(),
                        None => None,
                    };
                    trace.log(
                        "tool_queue",
                        queued.elapsed().as_millis(),
                        &format!("{} class={}", call.name, class.as_str()),
                    );
                    let _permit = parallel_limit.acquire_owned().await.ok();
                    let started = Instant::now();
                    let result = if call.name == "delegate_task" {
                        let task = call.input["task"].as_str().unwrap_or("");
                        let task_id = call.input["task_id"]
                            .as_str()
                            .unwrap_or(&call.id)
                            .to_string();
                        let resumed = call.input["resume"].as_bool().unwrap_or(false);
                        if resumed {
                            task_results
                                .lock()
                                .ok()
                                .and_then(|results| results.get(&task_id).cloned())
                                .ok_or_else(|| anyhow::anyhow!("unknown task_id {task_id}"))
                        } else if task.is_empty() {
                            Err(anyhow::anyhow!("missing task"))
                        } else {
                            let selected_provider = if call.input["provider"].is_string()
                                || call.input["model"].is_string()
                            {
                                let name =
                                    call.input["provider"].as_str().unwrap_or(&config.provider);
                                let model = call.input["model"].as_str().unwrap_or(&config.model);
                                match crate::provider::create(name, model, tools.client.clone()) {
                                    Ok(provider) => provider,
                                    Err(error) => {
                                        return (
                                            call.id.clone(),
                                            Err(error),
                                            started.elapsed().as_millis(),
                                        )
                                    }
                                }
                            } else {
                                provider
                            };
                            // The child streams through an id-prefixed view of
                            // the parent channel so the UI can nest its tool
                            // cells under this delegate cell.
                            let child_tx = event_tx.clone().map(|forward| {
                                let (child_tx, mut child_rx) =
                                    tokio::sync::mpsc::unbounded_channel();
                                let parent_id = call.id.clone();
                                tokio::spawn(async move {
                                    while let Some(message) = child_rx.recv().await {
                                        let message = match message {
                                            Progress::ToolBegin { id, title } => {
                                                Progress::ToolBegin {
                                                    id: format!("{parent_id}/{id}"),
                                                    title,
                                                }
                                            }
                                            Progress::ToolEnd {
                                                id,
                                                ok,
                                                output,
                                                elapsed_ms,
                                            } => Progress::ToolEnd {
                                                id: format!("{parent_id}/{id}"),
                                                ok,
                                                output,
                                                elapsed_ms,
                                            },
                                            other => other,
                                        };
                                        let _ = forward.send(message);
                                    }
                                });
                                child_tx
                            });
                            let mut child = Agent {
                                config: config.clone(),
                                provider: selected_provider,
                                tools,
                                harness,
                                messages: vec![Message {
                                    role: "user".into(),
                                    content: vec![Content::Text(task.into())],
                                }],
                                repo_map: map,
                                memory: Memory::new(&config.root),
                                skills: skills::discover(&config.root, &config.skill_dirs)
                                    .unwrap_or_default(),
                                event_tx: child_tx,
                                // Delegated CTF hypotheses must keep the CTF
                                // doctrine, category, flag regex, and shared hit
                                // registry. Build/plan children keep their parent
                                // mode as well instead of silently becoming build.
                                mode: child_mode,
                                active_skill: None,
                                last_api_latency: None,
                                ctf: child_ctf,
                                think,
                                thinking_level: live_level,
                                think_fail_streak: 0,
                                recent_calls: VecDeque::new(),
                                auto_escalated: false,
                                last_turn_reasoning: 0,
                                usage: Usage::default(),
                                model_turns: 0,
                                started_at: Instant::now(),
                                metrics: metrics.clone(),
                                task_results: task_results.clone(),
                                store,
                                instructions: crate::project::instruction_chain(&config.root),
                                compact_at_chars: threshold_from_env(),
                                agent_system: agent_system.clone(),
                                agent_name: agent_name.clone(),
                                pinned: Vec::new(),
                                step_log: Vec::new(),
                                trace: trace.clone(),
                                prompt: crate::prompt::PromptState::default(),
                            };
                            let result = child.run(task).await;
                            if let Ok(summary) = &result {
                                if let Ok(mut results) = task_results.lock() {
                                    results.insert(task_id, summary.clone());
                                }
                            }
                            result
                        }
                    } else {
                        let mut result = tools.execute(call).await;
                        if tools.retryable(call) {
                            for attempt in 1..config.tool_retries {
                                if result.is_ok() {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(100 * attempt as u64))
                                    .await;
                                result = tools.execute(call).await;
                            }
                        }
                        result
                    };
                    let elapsed = started.elapsed().as_millis();
                    trace.log(
                        "tool",
                        elapsed,
                        &format!(
                            "{} class={} ok={}",
                            call.name,
                            class.as_str(),
                            result.is_ok()
                        ),
                    );
                    (call.id.clone(), result, elapsed)
                }
            }))
            .await;
            let mut class_counts: HashMap<&'static str, usize> = HashMap::new();
            for call in &calls {
                *class_counts
                    .entry(crate::tools::classify(call).as_str())
                    .or_default() += 1;
            }
            let class_summary = class_counts
                .iter()
                .map(|(class, count)| format!("{class}={count}"))
                .collect::<Vec<_>>()
                .join(",");
            self.trace.log(
                "tools",
                batch_started.elapsed().as_millis(),
                &format!("{} calls {class_summary}", calls.len()),
            );
            let mut edited_paths: Vec<String> = calls
                .iter()
                .zip(results.iter())
                .filter(|(call, (_, result, _))| {
                    matches!(
                        call.name.as_str(),
                        "write_file" | "edit_file" | "search_replace"
                    ) && result.is_ok()
                })
                .filter_map(|(call, _)| call.input["path"].as_str().map(str::to_owned))
                .collect();
            edited_paths.extend(
                calls
                    .iter()
                    .zip(results.iter())
                    .filter(|(call, (_, result, _))| call.name == "apply_patch" && result.is_ok())
                    .filter_map(|(call, _)| call.input["patch"].as_str())
                    .flat_map(crate::tools::patch::paths),
            );
            if snapshot.is_some() {
                if edited_paths.is_empty() {
                    crate::snapshot::discard_latest(&self.config.root);
                } else {
                    crate::snapshot::commit(&self.config.root);
                }
            }
            for (call, (_, result, elapsed_ms)) in calls.iter().zip(results.iter()) {
                self.metrics.record_tool(result.is_ok());
                self.step_log.push(step_title(call, result.is_ok()));
                if let Ok(output) = result {
                    // Shell may return Ok(exit=1 ...) for a failed process.
                    // Failed output is not successful CTF proof.
                    if usable_ctf_tool_evidence(&call.name, output) {
                        self.emit_flags(ctf_task, &format!("tool:{}", call.name), output)
                            .await;
                    }
                }
                if let Some(tx) = &self.event_tx {
                    let _ = tx.send(Progress::ToolEnd {
                        id: call.id.clone(),
                        ok: result.is_ok(),
                        output: crate::crash::redact(&match result {
                            Ok(text) => text.clone(),
                            Err(error) => error.to_string(),
                        }),
                        elapsed_ms: *elapsed_ms,
                    });
                    if call.name == "update_plan" && result.is_ok() {
                        let plan = self
                            .tools
                            .checklist
                            .lock()
                            .map(|plan| plan.clone())
                            .unwrap_or_default();
                        let _ = tx.send(Progress::Plan(plan));
                    }
                }
            }
            // Feed the circuit breaker and search detector before the next
            // request is built. Summaries are bounded: the breaker only needs
            // the error text or the banner, not the whole observation.
            let observations: Vec<crate::prompt::Observation> = calls
                .iter()
                .zip(results.iter())
                .map(|(call, (_, result, _))| crate::prompt::Observation {
                    signature: crate::prompt::signature(&call.name, &call.input),
                    name: call.name.clone(),
                    ok: result.is_ok(),
                    summary: match result {
                        Ok(output) => tools::truncate(output.clone(), 4_000),
                        Err(error) => error.to_string(),
                    },
                })
                .collect();
            self.prompt.observe(&observations, &self.mode);
            // Failures drive the `auto` thinking controller: three
            // consecutive failing steps escalate the level.
            let failures = results
                .iter()
                .filter(|(_, result, _)| result.is_err())
                .count();
            let mut blocks: Vec<Content> = calls
                .iter()
                .zip(results)
                .map(|(call, (id, result, _))| match result {
                    Ok(output) => {
                        // Network text is attacker-influenceable data. Frame
                        // it so the model treats it as evidence, not orders.
                        let output = if crate::prompt::is_external_tool(&call.name) {
                            crate::prompt::wrap_untrusted(&call.name, &call.input, output)
                        } else {
                            output
                        };
                        Content::Result {
                            id,
                            output,
                            is_error: false,
                        }
                    }
                    Err(error) => Content::Result {
                        id,
                        output: error.to_string(),
                        is_error: true,
                    },
                })
                .collect();
            if !edited_paths.is_empty() && !self.tools.plan {
                let mut checks_passed = true;
                if let Some(command) = self.config.check_command() {
                    self.emit(format!("checking {command}"));
                    let result = crate::tools::shell::run(&command, &self.config.root).await?;
                    let failed = !result.starts_with("exit=0\n");
                    blocks.push(Content::Text(format!(
                        "Automatic check `{command}`:\n{}",
                        tools::truncate(result, 20_000)
                    )));
                    if failed {
                        self.emit("checks failed; attempting repair".into());
                        checks_passed = false;
                        repairs += 1;
                        if repairs > self.config.repair_retries {
                            bail!(
                                "automatic check failed after {} retries",
                                self.config.repair_retries
                            );
                        }
                    }
                }
                if checks_passed {
                    self.emit("checks passed".into());
                    if self.config.auto_commit {
                        if let Err(error) = self.auto_commit_paths(&edited_paths, &last_text).await
                        {
                            blocks.push(Content::Text(format!(
                                "Automatic git commit failed: {error}"
                            )));
                        }
                    }
                }
            }
            if !edited_paths.is_empty()
                || calls
                    .iter()
                    .any(|call| matches!(call.name.as_str(), "shell" | "delegate_task"))
            {
                turn_map = None;
            }
            self.messages.push(Message {
                role: "user".into(),
                content: blocks,
            });
            self.trace.event(
                "continuation_ready",
                &format!(
                    "blocks={} failures={} repairs={}",
                    self.messages
                        .last()
                        .map_or(0, |message| message.content.len()),
                    failures,
                    repairs
                ),
            );
            // A verified artifact or deterministic decode is already a solve.
            // Skip another provider round-trip just to restate the flag.
            if self.mode == "ctf" {
                let verify_started = Instant::now();
                let verified = crate::autopilot::verify(&self.ctf, &self.ctf.current_hits());
                self.trace.log(
                    "verification",
                    verify_started.elapsed().as_millis(),
                    &format!("ctf found={}", verified.is_some()),
                );
                if let Some(verified) = verified {
                    self.auto_finish(used_tools_this_turn);
                    return Ok(format!(
                        "Verified flag: {} ({})",
                        verified.flag, verified.evidence
                    ));
                }
            }
            self.auto_step(failures);
        }
    }

    /// aider-style autocommit: stage exactly the paths this turn edited and
    /// commit them with a model-written subject drawn from the staged diff,
    /// falling back to a `wrosecode: <last reply line>` heuristic when the
    /// model has nothing better. `git commit --only` pins the commit to
    /// `paths`, so unrelated dirty files in the tree are never swept in.
    async fn auto_commit_paths(&mut self, paths: &[String], summary: &str) -> Result<()> {
        let root = self.config.root.clone();
        let probe = tokio::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&root)
            .output()
            .await?;
        if !probe.status.success() || probe.stdout != b"true\n" {
            return Ok(());
        }
        let output = tokio::process::Command::new("git")
            .arg("add")
            .arg("--")
            .args(paths)
            .current_dir(&root)
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "git add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // The staged diff — now including newly added files — is what the
        // commit message has to describe. A failed diff just drops us to
        // the heuristic; it must not fail the commit itself.
        let diff = tokio::process::Command::new("git")
            .args(["diff", "--no-ext-diff", "--cached", "--"])
            .args(paths)
            .current_dir(&root)
            .output()
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let subject = if diff.trim().is_empty() {
            None
        } else {
            // The subject call is not part of the turn's API timing.
            let saved_latency = self.last_api_latency;
            let subject = self.commit_message(&diff).await.ok();
            self.last_api_latency = saved_latency;
            subject
        };
        let message = match subject.filter(|subject| !subject.trim().is_empty()) {
            Some(subject) => format!("wrosecode: {}", subject.trim())
                .chars()
                .take(72)
                .collect::<String>(),
            None => format!(
                "wrosecode: {}",
                summary
                    .lines()
                    .next()
                    .unwrap_or("update files")
                    .chars()
                    .take(70)
                    .collect::<String>()
            ),
        };
        let output = tokio::process::Command::new("git")
            .arg("commit")
            .arg("--only")
            .arg("-m")
            .arg(&message)
            .arg("--")
            .args(paths)
            .current_dir(&root)
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        self.emit(format!("auto-commit {message}"));
        Ok(())
    }

    fn emit(&self, message: String) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(Progress::Tool(message));
        }
    }

    async fn emit_flags(&self, ctf_task: bool, source: &str, text: &str) {
        if !ctf_task {
            return;
        }
        if let Ok(hits) = self.ctf.scan_stream(source, text) {
            if let Some(tx) = &self.event_tx {
                for hit in &hits {
                    let _ = tx.send(Progress::FlagFound {
                        flag: hit.flag.clone(),
                        source: hit.source.clone(),
                    });
                }
            }
            for hit in hits {
                let verdict = crate::report::check_flag(&hit.flag, text);
                let _ = self.store.record_flag(
                    "active",
                    &hit.flag,
                    &hit.source,
                    &format!("{verdict:?}").to_ascii_uppercase(),
                );
                // Never auto-submit flags from generic-mode CTF prompts.
                if self.mode == "ctf" && matches!(verdict, crate::report::Verdict::Verified) {
                    if let Ok(Some(status)) =
                        self.ctf.auto_submit(&self.tools.client, &hit.flag).await
                    {
                        self.emit(format!("Checker VERIFIED · {status}"));
                    }
                }
            }
        }
    }

    pub fn stats_line(&self) -> String {
        let (cache_hits, saved_bytes) = self.tools.cache_stats();
        format!(
            "turns {} · tokens {} in / {} out / {} reasoning · cache {} · saved ~{} tokens · elapsed {}s",
            self.model_turns,
            self.usage.input,
            self.usage.output,
            self.usage.reasoning,
            cache_hits,
            saved_bytes / 4,
            self.started_at.elapsed().as_secs()
        )
    }
}

/// A tool transport may succeed even when a subprocess returned exit 1.
/// Do not treat that output as proof of a solved challenge. A checker must
/// actually complete successfully before its flag bytes are trusted.
fn usable_ctf_tool_evidence(name: &str, output: &str) -> bool {
    name != "shell" || output.starts_with("exit=0\n")
}

fn format_tool_title(name: &str, detail: &str) -> String {
    if name == "update_plan" {
        return format!("Plan {detail}").trim().to_string();
    }
    let verb = match name {
        "shell" => "Ran",
        "read_file" => "Read",
        "grep" | "glob" => "Searched",
        "edit_file" | "search_replace" | "write_file" => "Edited",
        "apply_patch" => "Patched",
        "web_fetch" | "web_search" => "Fetched",
        "delegate_task" => "Delegated",
        _ => "Ran",
    };
    format!("{verb} {name} {detail}").trim().to_string()
}

/// One line per executed tool call for the end-of-task summary: the call
/// name plus its most informative argument, capped with an explicit marker
/// so a long command cannot blow up the receipt.
pub(crate) fn step_title(call: &ToolCall, ok: bool) -> String {
    let detail = call.input["path"]
        .as_str()
        .or_else(|| call.input["pattern"].as_str())
        .or_else(|| call.input["command"].as_str())
        .or_else(|| call.input["url"].as_str())
        .or_else(|| call.input["query"].as_str())
        .or_else(|| call.input["task"].as_str())
        .unwrap_or("");
    let mut line = format!("{} {detail}", call.name);
    if !ok {
        line.push_str(" (failed)");
    }
    const CAP: usize = 72;
    let line = line.trim().to_string();
    if line.chars().count() > CAP {
        let head: String = line.chars().take(CAP.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        line
    }
}

/// Fixed, injection-free command templates for small directory requests.
/// Unknown paths, multi-action prompts and CTF sessions retain model routing.
fn quick_directory_command(query: &str, project_root: &std::path::Path) -> Option<String> {
    let query = query.trim().to_ascii_lowercase();
    if query.len() > 360 || query.contains([';', '\n', '/', '`']) || query.contains("..") {
        return None;
    }
    let words: Vec<&str> = query
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .collect();
    if words.iter().any(|word| {
        matches!(
            *word,
            "delete"
                | "remove"
                | "edit"
                | "write"
                | "create"
                | "fix"
                | "update"
                | "implement"
                | "recursive"
                | "recursively"
        )
    }) {
        return None;
    }
    let dirs = words.iter().any(|word| {
        matches!(
            *word,
            "dir" | "die" | "dirs" | "directory" | "directories" | "folder" | "folders"
        )
    });
    if !dirs || words.iter().any(|word| matches!(*word, "file" | "files")) {
        return None;
    }
    let count = words.contains(&"count");
    let list = words.contains(&"list") || words.contains(&"show");
    if !count && !list {
        return None;
    }
    let desktop = words.contains(&"desktop");
    if (query.contains(" in ") || query.contains(" inside ") || query.contains(" under "))
        && !desktop
        && !words.contains(&"current")
        && !words.contains(&"working")
        && !words.contains(&"this")
        && !words.contains(&"here")
    {
        return None;
    }
    let root = if desktop {
        if project_root.join("Desktop").is_dir() {
            "Desktop"
        } else {
            "\"$HOME/Desktop\""
        }
    } else {
        "."
    };
    if count {
        Some(format!(
            "find {root} -mindepth 1 -maxdepth 1 -type d | awk 'END {{ print NR }}'"
        ))
    } else {
        // 'list dir' is an ordinary listing of files and folders. Explicit
        // 'directories/dirs/folders' asks for folders only.
        let only_dirs = words
            .iter()
            .any(|word| matches!(*word, "dirs" | "directories" | "folders"));
        let filter = if only_dirs { "-type d " } else { "" };
        Some(format!(
            "find {root} -mindepth 1 -maxdepth 1 {filter}-exec basename {{}} \\; | LC_ALL=C sort"
        ))
    }
}

/// Only obvious salutations use the minimal no-tool request. Real CTF or
/// coding questions, even short ones, always retain the normal agent path.
fn is_quick_greeting(query: &str) -> bool {
    let cleaned = query
        .trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation());
    let cleaned = cleaned.to_ascii_lowercase();
    matches!(
        cleaned.trim(),
        "hi" | "hii"
            | "hiii"
            | "hiiii"
            | "hello"
            | "hey"
            | "yo"
            | "hey there"
            | "hello there"
            | "good morning"
            | "good evening"
    )
}

/// A narrow single-operation routing decision, not a canned answer:
/// the provider must still issue a real filesystem tool call.
fn is_directory_listing(query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.len() > 180 || query.split_whitespace().count() > 14 || query.contains([';', '\n']) {
        return false;
    }
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.iter().any(|word| {
        matches!(
            *word,
            "delete" | "remove" | "edit" | "write" | "create" | "fix" | "update" | "implement"
        )
    }) {
        return false;
    }
    matches!(words.first(), Some(&"list" | &"show"))
        && (words.iter().any(|word| {
            matches!(
                *word,
                "dir" | "dirs" | "directory" | "directories" | "files" | "folders"
            )
        }) || query.starts_with("list desktop")
            || query == "list die in desktop")
}

fn task_requires_tool(query: &str) -> bool {
    if is_directory_listing(query) {
        return true;
    }
    let query = query.to_ascii_lowercase();
    [
        "use the shell",
        "use shell",
        "run ",
        "execute ",
        "inspect ",
        "read the file",
        "read file",
        "search the",
        "fix ",
        "update ",
        "implement ",
        "build ",
        "test ",
        "solve ",
    ]
    .iter()
    .any(|needle| query.contains(needle))
}

/// The summarizer's instructions (Codex compaction, clai's state-preserving
/// summary): what to keep, in what order, and nothing else.
const COMPACT_SYSTEM: &str = "You are the compaction engine inside a coding agent. \
    Read the conversation below and rewrite it as one dense plain-text summary the \
    agent can continue from. Keep, in this order: (1) the user's goals and every \
    constraint or preference they stated; (2) project state — files read, edited, \
    created or deleted, and the decisions or contents that matter for what comes \
    next; (3) what has been verified so far and what is still unverified; (4) errors \
    and how each was resolved; (5) decisions made, including anything deliberately \
    skipped; (6) the most recent user request, verbatim. Plain text only: no \
    preamble, no commentary, no markdown headings.";

/// Approximate conversation size as the provider will see it.
fn transcript_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .map(|content| match content {
            Content::Text(text) => text.len(),
            Content::Result { output, .. } => output.len(),
            Content::Call(call) => call.input.to_string().len(),
        })
        .sum()
}

/// The history handed to the summarizer: the whole conversation, with every
/// text run capped so one giant tool result cannot crowd out the rest.
fn summarise_input(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|message| Message {
            role: message.role.clone(),
            content: message
                .content
                .iter()
                .map(|content| match content {
                    Content::Text(text) => Content::Text(cap_text(text)),
                    Content::Result {
                        id,
                        output,
                        is_error,
                    } => Content::Result {
                        id: id.clone(),
                        output: cap_text(output),
                        is_error: *is_error,
                    },
                    Content::Call(call) => Content::Call(call.clone()),
                })
                .collect(),
        })
        .collect()
}

fn cap_text(text: &str) -> String {
    if text.chars().count() <= SUMMARY_INPUT_CAP {
        return text.to_string();
    }
    let head: String = text.chars().take(SUMMARY_INPUT_CAP).collect();
    format!("{head}\n…[truncated to {SUMMARY_INPUT_CAP} characters]")
}

/// The plain-text part of a completion.
fn text_of(content: Vec<Content>) -> String {
    content
        .into_iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `WROSECODE_COMPACT_CHARS`, or the default threshold.
fn threshold_from_env() -> usize {
    std::env::var("WROSECODE_COMPACT_CHARS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_COMPACT_AT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Permission;
    use crate::provider::Response;
    use crate::sandbox::SandboxPolicy;
    use crate::store::Store;
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::Mutex;

    #[test]
    fn quick_directory_routing_rejects_ambiguous_or_mutating_tasks() {
        let root = Path::new(".");
        assert!(quick_directory_command("list dir in desktop", root).is_some());
        assert!(quick_directory_command("list die in desktop", root).is_some());
        assert!(quick_directory_command(
            "Use the shell tool to count the immediate child directories of the current working directory",
            root,
        ).is_some());
        assert!(quick_directory_command("list directories in /tmp", root).is_none());
        assert!(quick_directory_command("list files and folders", root).is_none());
        assert!(quick_directory_command("list and delete directories", root).is_none());
    }

    #[tokio::test]
    async fn fast_directory_queries_are_exact_and_do_not_call_the_model() {
        let root = std::env::temp_dir().join(format!(
            "wrosecode-fast-directory-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("not-a-directory.txt"), "ignored").unwrap();
        let (mut agent, mock) = agent_with_mock(&root);
        let listed = agent
            .turn("List directories directly inside the current working directory")
            .await
            .unwrap();
        assert_eq!(
            listed.lines().collect::<Vec<_>>(),
            vec![".hidden", "alpha", "beta"]
        );
        let counted = agent
            .turn("Use the shell tool to count immediate child directories of the current working directory")
            .await
            .unwrap();
        assert_eq!(counted, "3");
        assert_eq!(agent.model_turns, 0);
        assert!(mock.requests().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_only_and_ctf_hypothesis_batches_dispatch_in_parallel() {
        let call = |name: &str| ToolCall {
            item_id: None,
            id: format!("call-{name}"),
            name: name.into(),
            input: serde_json::Value::Null,
        };
        let reads = vec![call("read_file"), call("grep"), call("decode")];
        assert_eq!(tool_batch_parallelism("build", 0, 20, &reads), 3);
        assert_eq!(tool_batch_parallelism("build", 0, 2, &reads), 2);

        let hypotheses = vec![call("delegate_task"), call("delegate_task")];
        assert_eq!(tool_batch_parallelism("ctf", 0, 20, &hypotheses), 2);
        assert_eq!(tool_batch_parallelism("build", 0, 20, &hypotheses), 1);

        let mutations = vec![call("shell"), call("write_file")];
        assert_eq!(tool_batch_parallelism("ctf", 0, 20, &mutations), 1);
        assert_eq!(
            tool_batch_parallelism("build", 20, 20, &mutations),
            1,
            "high reasoning must not parallelize mutating files"
        );
        let mixed = vec![call("grep"), call("apply_patch"), call("read_file")];
        assert_eq!(
            tool_batch_parallelism("build", 20, 20, &mixed),
            1,
            "mixed batch needs mutation ordering"
        );
        assert_eq!(
            tool_batch_parallelism("build", 20, 20, &reads),
            reads.len(),
            "independent read-only calls remain parallel"
        );
    }

    #[test]
    fn ctf_evidence_rejects_failed_shell_and_model_only_flag_claims() {
        use crate::autopilot;
        let directory =
            std::env::temp_dir().join(format!("wrose-ctf-proof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let engine = crate::ctf::CtfEngine::new(&directory);
        let flag = "HTB{f93a1b2c3d4e5f6a7b8c9d0e1234abcd}";
        engine.scan("model response", flag).unwrap();
        assert!(
            autopilot::verify(&engine, &engine.current_hits()).is_none(),
            "a model's flag claim is never proof"
        );
        assert!(!usable_ctf_tool_evidence(
            "shell",
            &format!("exit=1\n{flag}")
        ));
        assert!(!usable_ctf_tool_evidence(
            "shell",
            &format!("exit=124\n{flag}")
        ));
        assert!(usable_ctf_tool_evidence(
            "shell",
            &format!("exit=0\n{flag}")
        ));
        engine
            .scan("tool:shell", &format!("exit=0\n{flag}"))
            .unwrap();
        let verified = autopilot::verify(&engine, &engine.current_hits()).unwrap();
        assert_eq!(verified.flag, flag);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn action_requests_require_a_tool_but_chat_does_not() {
        assert!(task_requires_tool("Use the shell tool to run printf ok"));
        assert!(task_requires_tool("fix the scroll bug"));
        assert!(!task_requires_tool("hii"));
        assert!(!task_requires_tool("explain Rust ownership"));
    }

    #[test]
    fn provider_retries_skip_permanent_configuration_failures() {
        assert!(!retryable_provider_error("bad or missing API key"));
        assert!(!retryable_provider_error(
            "Base URL returned 404. Did you forget /v1?"
        ));
        assert!(!retryable_provider_error("ChatGPT is not signed in"));
        assert!(!retryable_provider_error("HTTP 401 unauthorized"));
        assert!(!retryable_provider_error("HTTP 403 forbidden"));
        assert!(!retryable_provider_error("invalid api key"));
        assert!(!retryable_provider_error("insufficient_quota"));
        assert!(retryable_provider_error(
            "cannot connect to provider (DNS, TLS, or server error)"
        ));
        assert!(retryable_provider_error(
            "provider stream timed out while waiting for output"
        ));
        assert!(retryable_provider_error("rate limited (429)"));
    }

    /// Records what the summarizer was asked and answers with a fixed summary.
    struct MockProvider {
        seen: Mutex<Vec<(String, String)>>,
    }

    impl MockProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
            })
        }
        fn requests(&self) -> Vec<(String, String)> {
            self.seen.lock().expect("mock lock").clone()
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_stream_with_think(
            &self,
            system: &str,
            messages: &[Message],
            _tools: &[serde_json::Value],
            _require_tool: bool,
            _progress: Option<&mpsc::UnboundedSender<Progress>>,
            _think: ThinkLevel,
        ) -> anyhow::Result<Response> {
            let flat = messages
                .iter()
                .map(|message| format!("{}: {}", message.role, text_of(message.content.clone())))
                .collect::<Vec<_>>()
                .join("\n");
            self.seen
                .lock()
                .expect("mock lock")
                .push((system.to_string(), flat));
            Ok(Response {
                content: vec![Content::Text(
                    "SUMMARY-TOKEN-99: the user wants the parser rewritten.".into(),
                )],
                usage: Usage {
                    input: 5,
                    output: 7,
                    ..Usage::default()
                },
            })
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec!["mock-model".into()])
        }
    }

    #[test]
    fn the_planner_model_runs_only_the_plan_mode() {
        let dir = std::env::temp_dir().join(format!("wrose-planner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let (mut agent, _mock) = agent_with_mock(&dir);
        agent.config = Arc::new(Config {
            planner_provider: "mock".into(),
            planner_model: "plan-model".into(),
            ..(*agent.config).clone()
        });
        assert!(plan_phase_uses_planner("plan", &agent.config));
        assert!(!plan_phase_uses_planner("build", &agent.config));
        assert!(!plan_phase_uses_planner("general", &agent.config));

        let worker_only = Config {
            planner_provider: String::new(),
            planner_model: String::new(),
            ..(*agent.config).clone()
        };
        assert!(!plan_phase_uses_planner("plan", &worker_only));
        // Half a split never switches: create() needs both halves.
        let half = Config {
            planner_provider: String::new(),
            planner_model: "plan-model".into(),
            ..(*agent.config).clone()
        };
        assert!(!plan_phase_uses_planner("plan", &half));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plan_mode_still_answers_on_the_worker_when_the_planner_profile_is_missing() {
        let dir = std::env::temp_dir().join(format!("wrose-planner-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let (mut agent, mock) = agent_with_mock(&dir);
        agent.config = Arc::new(Config {
            // No such profile: provider::create fails and the run must
            // fall back to the worker rather than erroring out.
            planner_provider: "wrose-ghost-planner".into(),
            planner_model: "ghost-model".into(),
            ..(*agent.config).clone()
        });
        agent.set_mode("plan").expect("plan mode");
        let reply = agent.turn("say hello").await.expect("plan turn");
        assert!(!reply.is_empty());
        assert_eq!(mock.requests().len(), 1, "the worker answered the turn");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A provider that plays back a fixed script of responses, then keeps
    /// answering with a plain "done" once the script runs out. Every call
    /// parks for a moment while counted, so tests can prove two callers
    /// were in flight at once.
    struct ScriptedProvider {
        script: Mutex<std::collections::VecDeque<Response>>,
        systems: Mutex<Vec<String>>,
        inflight: std::sync::atomic::AtomicUsize,
        peak_inflight: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Response>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into_iter().collect()),
                systems: Mutex::new(Vec::new()),
                inflight: std::sync::atomic::AtomicUsize::new(0),
                peak_inflight: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn peak(&self) -> usize {
            self.peak_inflight.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn systems(&self) -> Vec<String> {
            self.systems.lock().expect("systems lock").clone()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn chat_stream_with_think(
            &self,
            system: &str,
            _messages: &[Message],
            _tools: &[serde_json::Value],
            _require_tool: bool,
            _progress: Option<&mpsc::UnboundedSender<Progress>>,
            _think: ThinkLevel,
        ) -> anyhow::Result<Response> {
            use std::sync::atomic::Ordering;
            self.systems
                .lock()
                .expect("systems lock")
                .push(system.to_string());
            let active = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_inflight.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            let next = self.script.lock().expect("script lock").pop_front();
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            Ok(next.unwrap_or(Response {
                content: vec![Content::Text("done".into())],
                usage: Usage::default(),
            }))
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec!["mock-model".into()])
        }
    }

    fn said(text: &str) -> Response {
        Response {
            content: vec![Content::Text(text.into())],
            usage: Usage::default(),
        }
    }

    fn writes(path: &str) -> Response {
        Response {
            content: vec![Content::Call(ToolCall {
                item_id: None,
                id: "call-1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": path, "content": "hello from the agent"}),
            })],
            usage: Usage::default(),
        }
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be on PATH for this test");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A one-commit repo whose `unrelated.txt` is dirty, ready for an agent
    /// to edit something else.
    fn git_repo_with_dirty_file(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).expect("temp repo dir");
        std::fs::write(dir.join("unrelated.txt"), "v1").expect("seed file");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "wrose@local"],
            vec!["config", "user.name", "wrose test"],
            vec!["add", "."],
            vec!["commit", "-q", "-m", "initial"],
        ] {
            git_out(dir, &args);
        }
        std::fs::write(dir.join("unrelated.txt"), "v2 dirty").expect("dirty file");
    }

    #[tokio::test]
    async fn autocommit_commits_only_the_paths_the_agent_edited() {
        let dir = std::env::temp_dir().join(format!("wrose-autocommit-{}", std::process::id()));
        git_repo_with_dirty_file(&dir);

        let (mut agent, _mock) = agent_with_mock(&dir);
        agent.config = Arc::new(Config {
            auto_commit: true,
            ..(*agent.config).clone()
        });
        agent.provider = ScriptedProvider::new(vec![
            writes("note.txt"),
            said("Write the note file with the parser output"),
            said("done"),
        ]) as Arc<dyn Provider>;

        let reply = agent.turn("write note.txt").await.expect("turn");
        assert!(!reply.is_empty());

        assert_eq!(
            git_out(&dir, &["log", "--format=%s", "-1"]).trim(),
            "wrosecode: Write the note file with the parser output",
            "the commit subject comes from the model, not the heuristic"
        );
        let files = git_out(&dir, &["show", "--name-only", "--format=", "-1"]);
        assert!(files.lines().any(|line| line == "note.txt"), "{files}");
        assert!(
            !files.lines().any(|line| line == "unrelated.txt"),
            "the pre-existing dirty file must stay out: {files}"
        );
        let status = git_out(&dir, &["status", "--porcelain"]);
        assert!(status.contains("unrelated.txt"), "still dirty: {status}");
        assert!(!status.contains("note.txt"), "note.txt committed: {status}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn autocommit_can_be_switched_off() {
        let dir = std::env::temp_dir().join(format!("wrose-no-autocommit-{}", std::process::id()));
        git_repo_with_dirty_file(&dir);

        let (mut agent, _mock) = agent_with_mock(&dir);
        assert!(!agent.config.auto_commit, "the test config ships it off");
        agent.provider = ScriptedProvider::new(vec![
            writes("note.txt"),
            said("Write the note file"),
            said("done"),
        ]) as Arc<dyn Provider>;

        agent.turn("write note.txt").await.expect("turn");

        assert_eq!(
            git_out(&dir, &["rev-list", "--count", "HEAD"]).trim(),
            "1",
            "no automatic commit was made"
        );
        let status = git_out(&dir, &["status", "--porcelain"]);
        assert!(
            status.contains("note.txt"),
            "edit left for /commit: {status}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn general_mode_writes_files_and_only_plan_is_read_only() {
        let dir = std::env::temp_dir().join(format!("wrose-general-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut agent, _mock) = agent_with_mock(&dir);
        agent.set_mode("general").expect("general mode");
        assert!(!agent.tools.plan, "General mode must be able to edit");
        assert!(!agent.tools.bugbounty_mode);

        agent.provider = ScriptedProvider::new(vec![
            writes("general.txt"),
            said("wrote general.txt"),
            said("done"),
        ]) as Arc<dyn Provider>;
        let reply = agent
            .turn("create general.txt with the known content")
            .await
            .expect("general turn");
        assert!(!reply.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join("general.txt")).unwrap(),
            "hello from the agent",
            "General mode silently stopped executing writes"
        );

        agent.set_mode("plan").expect("plan mode");
        assert!(agent.tools.plan, "Plan mode is read-only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_switch_sets_plan_and_bounds_scope_by_mode() {
        let dir = std::env::temp_dir().join(format!("wrose-mode-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut agent, _mock) = agent_with_mock(&dir);
        let scope = crate::autopilot::Scope::for_bounty(
            &dir,
            "https://app.example.test",
            ["app.example.test"],
            std::iter::empty::<&str>(),
        );
        agent.tools.set_scope(Some(scope));

        agent.set_mode("bugbounty").expect("bug-bounty mode");
        assert!(agent.tools.bugbounty_mode);
        assert!(
            !agent.tools.plan,
            "bug-bounty mode must be able to record evidence"
        );
        assert!(
            agent.tools.scope.is_some(),
            "bug-bounty mode must keep the armed program scope"
        );

        agent.set_mode("general").expect("general mode");
        assert!(!agent.tools.bugbounty_mode);
        assert!(!agent.tools.plan);
        assert!(
            agent.tools.scope.is_none(),
            "a program allowlist must not leak into General mode"
        );

        agent.set_mode("plan").expect("plan mode");
        assert!(agent.tools.plan);
        agent.set_mode("build").expect("build mode");
        assert!(!agent.tools.plan);
        assert!(agent.set_mode("bogus").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two `delegate_task` calls issued in one batch must actually overlap:
    /// both children run through the same provider, and the peak-inflight
    /// counter proves they were inside a chat call at the same time — the
    /// parallel-subagent bar the CTF mode promises.
    #[tokio::test]
    async fn delegate_children_execute_concurrently() {
        let dir = std::env::temp_dir().join(format!("wrose-delegate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let (mut agent, _mock) = agent_with_mock(&dir);
        let provider = ScriptedProvider::new(vec![Response {
            content: vec![
                Content::Call(ToolCall {
                    item_id: None,
                    id: "call-1".into(),
                    name: "delegate_task".into(),
                    input: serde_json::json!({"task": "summarize part a", "task_id": "part-a"}),
                }),
                Content::Call(ToolCall {
                    item_id: None,
                    id: "call-2".into(),
                    name: "delegate_task".into(),
                    input: serde_json::json!({"task": "summarize part b", "task_id": "part-b"}),
                }),
            ],
            usage: Usage::default(),
        }]);
        agent.provider = provider.clone() as Arc<dyn Provider>;

        let reply = agent
            .turn("run both delegations in parallel")
            .await
            .expect("turn");
        assert!(!reply.is_empty(), "the turn still answers");
        assert!(
            provider.peak() >= 2,
            "the two delegate children must overlap (peak inflight {})",
            provider.peak()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delegate_children_inherit_ctf_mode_and_category() {
        let dir = std::env::temp_dir().join(format!("wrose-delegate-ctf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut agent, _mock) = agent_with_mock(&dir);
        agent.set_mode("ctf").unwrap();
        agent.ctf.category = "crypto".into();
        agent.ctf.lock_category();
        let provider = ScriptedProvider::new(vec![Response {
            content: vec![Content::Call(ToolCall {
                item_id: None,
                id: "call-ctf".into(),
                name: "delegate_task".into(),
                input: serde_json::json!({"task": "test the crypto hypothesis", "task_id": "crypto-a"}),
            })],
            usage: Usage::default(),
        }]);
        agent.provider = provider.clone() as Arc<dyn Provider>;

        let _ = agent
            .turn("solve this challenge with a child hypothesis")
            .await
            .unwrap();
        let systems = provider.systems();
        assert!(
            systems
                .iter()
                .skip(1)
                .any(|system| system.contains("Mode: ctf")
                    && system.contains("CTF category: crypto")
                    && system.contains("CTF MODE:")),
            "child lost CTF doctrine/category: {systems:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_titles_name_the_call_and_cap_long_commands() {
        let call = ToolCall {
            item_id: None,
            id: "1".into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "echo hi"}),
        };
        assert_eq!(step_title(&call, true), "shell echo hi");
        assert_eq!(step_title(&call, false), "shell echo hi (failed)");
        let long = ToolCall {
            item_id: None,
            id: "2".into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "x".repeat(200)}),
        };
        let title = step_title(&long, false);
        assert!(title.ends_with('…'), "{title}");
        assert_eq!(title.chars().count(), 72, "{title}");
    }

    fn agent_with_mock(root: &Path) -> (Agent, Arc<MockProvider>) {
        let config = Arc::new(Config {
            root: root.to_path_buf(),
            permission: Permission::Yolo,
            model: "mock-model".into(),
            provider: "mock".into(),
            harness: "minimal".into(),
            repair_retries: 0,
            check_command: None,
            skill_dirs: Vec::new(),
            think: ThinkLevel::Medium,
            thinking_level: 5,
            max_parallel_tasks: 4,
            shell_timeout_seconds: 30,
            tool_retries: 1,
            fallback_provider: String::new(),
            fallback_model: String::new(),
            planner_provider: String::new(),
            planner_model: String::new(),
            auto_commit: false,
            redis_url: None,
            budget_usd: 0.0,
            qdrant_url: None,
            ui_theme: "dark".into(),
            verbosity: "normal".into(),
            alternate_screen: true,
            mouse_capture: Some("auto".into()),
            alert_bell: false,
            smooth_scroll_lines: 1,
            sandbox: SandboxPolicy::default(),
            lsp: crate::lsp::LspSettings {
                enabled: false,
                ..Default::default()
            },
            computer_tools: false,
        });
        let provider = MockProvider::new();
        let client = reqwest::Client::new();
        let agent = Agent {
            config: config.clone(),
            provider: provider.clone() as Arc<dyn Provider>,
            tools: Tools::new(config.clone(), client),
            harness: Harness::Claude,
            messages: Vec::new(),
            repo_map: Arc::new(Mutex::new(RepoMap::default())),
            memory: Memory::new(&config.root),
            skills: HashMap::new(),
            event_tx: None,
            mode: "build".into(),
            active_skill: None,
            last_api_latency: None,
            ctf: CtfEngine::new(&config.root),
            think: ThinkLevel::Medium,
            thinking_level: 5,
            think_fail_streak: 0,
            recent_calls: VecDeque::new(),
            auto_escalated: false,
            last_turn_reasoning: 0,
            usage: Usage::default(),
            model_turns: 0,
            started_at: Instant::now(),
            metrics: Metrics::load(&config.root),
            task_results: Arc::new(Mutex::new(HashMap::new())),
            store: Store::open(&config.root.join("state.db")).expect("store"),
            instructions: String::new(),
            compact_at_chars: DEFAULT_COMPACT_AT_CHARS,
            agent_system: String::new(),
            agent_name: String::new(),
            pinned: Vec::new(),
            step_log: Vec::new(),
            trace: crate::trace::TraceSink::default(),
            prompt: crate::prompt::PromptState::default(),
        };
        (agent, provider)
    }

    fn seed(agent: &mut Agent) {
        for index in 0..3 {
            agent.messages.push(Message {
                role: "user".into(),
                content: vec![Content::Text(format!(
                    "SEED-MARKER-{index}: rewrite the parser, keep the CLI stable"
                ))],
            });
            agent.messages.push(Message {
                role: "assistant".into(),
                content: vec![Content::Text(format!("working on step {index}"))],
            });
        }
    }

    fn joined(messages: &[Message]) -> String {
        messages
            .iter()
            .map(|message| text_of(message.content.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn compaction_replaces_the_history_with_a_summary() {
        let root =
            std::env::temp_dir().join(format!("wrosecode-compact-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (mut agent, provider) = agent_with_mock(&root);
        seed(&mut agent);

        let report = agent.compact("test").await.expect("compact");
        assert!(report.contains("compacted"), "{report}");
        assert!(report.contains("characters"), "{report}");

        let requests = provider.requests();
        assert_eq!(requests.len(), 1, "one summarizer call");
        let (system, history) = &requests[0];
        assert!(system.contains("compaction engine"), "{system}");
        assert!(history.contains("SEED-MARKER-0"), "history not sent");
        assert!(history.contains("rewrite the parser"), "history not sent");

        assert_eq!(agent.messages.len(), 2, "summary + acknowledgement");
        let after = joined(&agent.messages);
        assert!(after.contains("SUMMARY-TOKEN-99"), "{after}");
        assert!(after.contains("compacted"), "{after}");
        assert!(!after.contains("SEED-MARKER-1"), "old turns survived");
        assert_eq!(agent.usage.input, 5, "summarizer usage is accounted");
        assert_eq!(agent.usage.output, 7);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn the_system_prompt_marks_the_stable_volatile_boundary() {
        let root =
            std::env::temp_dir().join(format!("wrosecode-system-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (mut agent, provider) = agent_with_mock(&root);

        agent.run("check the marker order").await.expect("run");

        let requests = provider.requests();
        let (system, _) = requests.last().expect("one request");
        let (stable, volatile) = system
            .split_once(crate::provider::SYSTEM_VOLATILE_MARK)
            .expect("the agent must mark the stable/volatile boundary");
        assert!(
            stable.contains("Research first: use the repo map"),
            "rules belong to the stable prefix: {stable}"
        );
        assert!(
            stable.contains("prefer search_replace"),
            "the edit doctrine belongs to the stable prefix: {stable}"
        );
        assert!(
            !stable.contains("Thinking level"),
            "the think level busts the cache and belongs in the tail: {stable}"
        );
        assert!(
            volatile.starts_with("Thinking level: 5/20"),
            "volatile tail starts with the live think level: {volatile}"
        );
        assert!(volatile.contains("Relevant memory:"), "{volatile}");
        assert!(volatile.contains("Repo map:"), "{volatile}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn four_identical_calls_in_a_row_trip_the_loop_detector() {
        let root = std::env::temp_dir().join(format!("wrosecode-loop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (mut agent, _provider) = agent_with_mock(&root);

        let call = |id: &str| ToolCall {
            item_id: None,
            id: id.to_string(),
            name: "grep".into(),
            input: serde_json::json!({"path": "src", "pattern": "flag{"}),
        };
        // Three identical calls are tolerated; the fourth trips.
        assert!(!agent.register_call(&call("a")));
        assert!(!agent.register_call(&call("b")));
        assert!(!agent.register_call(&call("c")));
        assert!(agent.register_call(&call("d")));
        // A stuck model keeps tripping until it changes strategy...
        assert!(agent.register_call(&call("e")));
        // ...and one different call in between resets the streak: the
        // next three identical calls pass again, the fourth trips.
        let other = ToolCall {
            item_id: None,
            id: "f".into(),
            name: "glob".into(),
            input: serde_json::json!({"pattern": "*.md"}),
        };
        assert!(!agent.register_call(&other));
        assert!(!agent.register_call(&call("g")));
        assert!(!agent.register_call(&call("h")));
        assert!(!agent.register_call(&call("i")));
        assert!(agent.register_call(&call("j")));
        // Input is canonical JSON: key order must not matter.
        let reordered = ToolCall {
            item_id: None,
            id: "k".into(),
            name: "grep".into(),
            input: serde_json::json!({"pattern": "flag{", "path": "src"}),
        };
        assert!(agent.register_call(&reordered));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn the_newest_request_survives_compaction_verbatim() {
        let root =
            std::env::temp_dir().join(format!("wrosecode-compact-pin-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (mut agent, _provider) = agent_with_mock(&root);
        seed(&mut agent);
        agent.compact("test").await.expect("compact");

        // The summarizer (a fixed stub here) never mentions SEED-MARKER-2, yet
        // the newest request must ride through the cut on its own.
        let after = joined(&agent.messages);
        assert!(
            after.contains("SEED-MARKER-2: rewrite the parser, keep the CLI stable"),
            "latest request was lost in compaction: {after}"
        );
        assert!(
            !after.contains("SEED-MARKER-0") && !after.contains("SEED-MARKER-1"),
            "older turns leaked through: {after}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn an_empty_conversation_has_nothing_to_compact() {
        let root =
            std::env::temp_dir().join(format!("wrosecode-compact-empty-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (mut agent, provider) = agent_with_mock(&root);
        assert!(agent.compact("test").await.is_err());
        assert!(provider.requests().is_empty(), "no call was made");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn needs_compact_respects_size_and_length_thresholds() {
        let root =
            std::env::temp_dir().join(format!("wrosecode-compact-needs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (mut agent, _provider) = agent_with_mock(&root);
        agent.compact_at_chars = 100;

        seed(&mut agent);
        assert!(agent.needs_compact(), "six messages over the threshold");

        agent.compact_at_chars = 10_000_000;
        assert!(!agent.needs_compact(), "under the threshold");

        agent.compact_at_chars = 1;
        agent.messages.truncate(2);
        assert!(!agent.needs_compact(), "too short to be worth summarizing");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pinning_a_file_pins_it_once_and_puts_it_in_the_context() {
        let root = std::env::temp_dir().join(format!("wrosecode-pin-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "PIN-COUNT-7: port 4444").unwrap();
        let (mut agent, _provider) = agent_with_mock(&root);

        let first = agent.pin("notes.txt").expect("pin");
        assert!(first.contains("notes.txt"), "{first}");
        assert_eq!(agent.pinned, vec!["notes.txt".to_string()]);

        agent.pin("notes.txt").expect("re-pin");
        assert_eq!(agent.pinned.len(), 1, "pinning twice must not duplicate");

        let context = agent.pinned_context();
        assert!(context.contains("Pinned files"), "{context}");
        assert!(context.contains("PIN-COUNT-7"), "{context}");

        assert!(agent.pin("missing.txt").is_err(), "pinning a missing file");
        assert!(agent.pin(".").is_err(), "pinning a directory");

        let dropped = agent.unpin("notes.txt").expect("drop");
        assert!(dropped.contains("notes.txt"), "{dropped}");
        assert!(agent.pinned.is_empty());
        assert!(agent.pinned_context().is_empty(), "no pins, no block");
        assert!(
            agent.unpin("notes.txt").is_err(),
            "dropping an unpinned file must fail"
        );

        agent.pin("notes.txt").expect("re-pin");
        let cleared = agent.unpin("all").expect("clear");
        assert!(cleared.contains('1'), "{cleared}");
        assert!(agent.pinned.is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pinned_context_truncates_a_file_that_is_too_large() {
        let root = std::env::temp_dir().join(format!("wrosecode-pin-cap-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("huge.txt"), "HUGE-PIN-MARKER ".repeat(2_000)).unwrap();
        let (mut agent, _provider) = agent_with_mock(&root);
        agent.pin("huge.txt").expect("pin");

        let context = agent.pinned_context();
        assert!(context.contains("HUGE-PIN-MARKER"), "{context}");
        assert!(
            context.contains("truncated"),
            "the pin must be capped: {}",
            context.len()
        );
        assert!(
            context.len() < 9_000,
            "one file must stay near its 6k budget, got {}",
            context.len()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn interrupted_tool_history_is_closed_once_before_reuse() {
        let dir = std::env::temp_dir().join(format!("wrose-interrupted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut agent, _) = agent_with_mock(&dir);
        agent.messages.push(Message {
            role: "assistant".into(),
            content: vec![Content::Call(ToolCall {
                item_id: None,
                id: "interrupted-call".into(),
                name: "shell".into(),
                input: serde_json::json!({"command":"touch output.txt"}),
            })],
        });
        agent.repair_tool_history();
        let count = agent.messages.len();
        assert!(matches!(&agent.messages.last().unwrap().content[0],
            Content::Result { id, is_error: true, output }
            if id == "interrupted-call" && output.contains("side effects")));
        agent.repair_tool_history();
        assert_eq!(agent.messages.len(), count);
        let _ = std::fs::remove_dir_all(dir);
    }

    struct RecoveryProvider {
        prompts: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Provider for RecoveryProvider {
        async fn chat_stream_with_think(
            &self,
            system: &str,
            _messages: &[Message],
            _tools: &[serde_json::Value],
            _require_tool: bool,
            _progress: Option<&mpsc::UnboundedSender<Progress>>,
            _think: ThinkLevel,
        ) -> anyhow::Result<Response> {
            let mut prompts = self.prompts.lock().unwrap();
            prompts.push(system.into());
            if prompts.len() < 3 {
                anyhow::bail!("temporary connection failure");
            }
            Ok(Response {
                content: vec![Content::Text("Recovered".into())],
                usage: Usage::default(),
            })
        }
        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn retries_keep_project_instructions_on_third_attempt() {
        let dir = std::env::temp_dir().join(format!("wrose-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut agent, _) = agent_with_mock(&dir);
        let provider = Arc::new(RecoveryProvider {
            prompts: Mutex::new(Vec::new()),
        });
        agent.provider = provider.clone();
        agent.instructions = "PROJECT-INSTRUCTION-MUST-SURVIVE".into();
        assert_eq!(
            agent.turn("explain Rust ownership").await.unwrap(),
            "Recovered"
        );
        let prompts = provider.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 3);
        assert!(prompts
            .iter()
            .all(|p| p.contains("PROJECT-INSTRUCTION-MUST-SURVIVE")));
        assert_eq!(prompts[0], prompts[2]);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn leaving_ctf_restores_build_mode_state() {
        let dir = std::env::temp_dir().join(format!("wrose-mode-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut agent, _) = agent_with_mock(&dir);
        agent.set_mode("ctf").unwrap();
        agent.ctf.lock_category();
        agent.set_mode("build").unwrap();
        assert!(!agent.ctf.category_locked());
        assert!(!agent.tools.plan);
        assert_eq!(agent.mode, "build");
        let _ = std::fs::remove_dir_all(dir);
    }
}
