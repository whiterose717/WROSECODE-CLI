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
}

/// Whether this turn runs on the configured planner model: the goose
/// planner/worker split applies to the read-only `plan` mode and only
/// when `[agent] planner` named a model.
fn plan_phase_uses_planner(mode: &str, config: &Config) -> bool {
    mode == "plan" && !config.planner_model.is_empty() && !config.planner_provider.is_empty()
}

impl Agent {
    pub fn set_mode(&mut self, mode: &str) -> Result<()> {
        if !matches!(mode, "build" | "plan" | "general") {
            bail!("unknown agent: {mode}");
        }
        self.mode = mode.into();
        self.tools.plan = mode != "build";
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
        Ok(resolved
            .strip_prefix(&self.config.root)
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
        let harness = Harness::parse(&config.harness).unwrap_or(Harness::Claude);
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
        })
    }

    pub async fn turn(&mut self, text: &str) -> Result<String> {
        self.step_log.clear();
        let _trace = self
            .trace
            .guard("turn", text.chars().take(80).collect::<String>());
        self.compact_if_needed().await;
        if !self.ctf.category_locked() {
            self.ctf.category = crate::ctf::categorize(&self.config.root, text);
        }
        if self.messages.is_empty() {
            for hit in self.ctf.scan_project() {
                if let Some(tx) = &self.event_tx {
                    let _ = tx.send(Progress::FlagFound {
                        flag: hit.flag,
                        source: hit.source,
                    });
                }
            }
        }
        self.messages.push(Message {
            role: "user".into(),
            content: vec![Content::Text(text.into())],
        });
        if let Some(skill) = self
            .active_skill
            .as_ref()
            .and_then(|name| self.skills.get(name))
            .or_else(|| skills::match_skill(&self.skills, text))
            .filter(|skill| skill.fork)
            .cloned()
        {
            let mut child_skills = self.skills.clone();
            if let Some(child_skill) = child_skills.get_mut(&skill.name) {
                child_skill.fork = false;
            }
            let task = format!("{}\n\nTask: {text}", skill.body);
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

    async fn run(&mut self, query: &str) -> Result<String> {
        // The loop detector is per query: a repeated setup across user
        // turns is not a stuck model.
        self.recent_calls.clear();
        let mut last_text = String::new();
        let mut repairs = 0;
        let mut used_tools_this_turn = false;
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
            let map = {
                let mut guard = self
                    .repo_map
                    .lock()
                    .map_err(|_| anyhow::anyhow!("repo map poisoned"))?;
                guard.update(&self.config.root)?;
                guard.render(query)
            };
            let memory = self.memory.recall(query)?;
            let skill = self
                .active_skill
                .as_ref()
                .and_then(|name| self.skills.get(name))
                .or_else(|| skills::match_skill(&self.skills, query));
            let skill_text = skill
                .map(|s| {
                    if s.fork {
                        format!(
                            "Skill {} is forked; use delegate_task for it. {}",
                            s.name, s.description
                        )
                    } else {
                        s.body.clone()
                    }
                })
                .unwrap_or_default();
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
                "{}\n{}{}{}Project: {}\nMode: {}\nCTF category: {}\nRules:\n{}\n",
                self.harness.prompt(),
                overlay,
                instructions,
                pinned,
                self.config.root.display(),
                self.mode,
                self.ctf.category,
                AGENTIC_ORCHESTRATOR_DOCTRINE,
            );
            let volatile = format!(
                "Thinking level: {}/20 (mode {}). At higher levels, use independent delegate_task calls, checker tasks, and race strategies when useful; never exceed 20 concurrent tasks.\nRelevant memory:\n{}\nRepo map:\n{}\nSkill:\n{}",
                self.thinking_level,
                self.think.name(),
                memory,
                map,
                skill_text
            );
            let system = format!(
                "{stable}{}{volatile}",
                crate::provider::SYSTEM_VOLATILE_MARK
            );
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(Progress::ResetText);
            }
            let started = Instant::now();
            let schemas = self.tools.schemas();
            let mut selected_provider = planner.clone().unwrap_or_else(|| self.provider.clone());
            let mut response = None;
            let mut last_error = None;
            for attempt in 0..3 {
                let prompt = if attempt == 2 {
                    format!(
                        "{}\nProject: {}\nComplete the user task directly. Use tools only when required.",
                        self.harness.prompt(),
                        self.config.root.display()
                    )
                } else {
                    system.clone()
                };
                match selected_provider
                    .complete_with_think(
                        &prompt,
                        &self.messages,
                        &schemas,
                        false,
                        self.event_tx.as_ref(),
                        self.provider_think(),
                    )
                    .await
                {
                    Ok(value) => {
                        response = Some(value);
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                        if attempt == 1
                            && !self.config.fallback_provider.is_empty()
                            && !self.config.fallback_model.is_empty()
                        {
                            if let Ok(provider) = crate::provider::create(
                                &self.config.fallback_provider,
                                &self.config.fallback_model,
                                self.tools.client.clone(),
                            ) {
                                selected_provider = provider;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
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
            // another identical round-trip.
            let mut looped = HashSet::new();
            for call in &calls {
                if self.register_call(call) {
                    looped.insert(call.id.clone());
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
                self.emit_flags("model response", &last_text).await;
            }
            self.messages.push(Message {
                role: "assistant".into(),
                content: response.content,
            });
            if calls.is_empty() {
                // Answer-only runs leave `auto` at low so trivial reads and
                // short replies never sit above it (spec PHASE 4).
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
            let dynamic_parallel = match live_level {
                0..=3 => 1,
                4..=7 => 3,
                8..=12 => 5,
                _ => self.config.max_parallel_tasks,
            };
            let parallel_limit = Arc::new(Semaphore::new(
                dynamic_parallel.min(self.config.max_parallel_tasks),
            ));
            let task_results = self.task_results.clone();
            let metrics = self.metrics.clone();
            let store = self.store.clone();
            let agent_system = self.agent_system.clone();
            let agent_name = self.agent_name.clone();
            let trace = self.trace.clone();
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
                let looped = looped.clone();
                async move {
                    if looped.contains(&call.id) {
                        return (
                            call.id.clone(),
                            Err(anyhow::anyhow!(
                                "loop detected: the identical `{}` call has now run three times in a row with no change — try a different approach",
                                call.name
                            )),
                            0,
                        );
                    }
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
                                mode: "build".into(),
                                active_skill: None,
                                last_api_latency: None,
                                ctf: CtfEngine::new(&config.root)
                                    .with_alert_bell(config.alert_bell),
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
                    (call.id.clone(), result, started.elapsed().as_millis())
                }
            }))
            .await;
            self.trace.log(
                "tools",
                batch_started.elapsed().as_millis(),
                &format!("{} calls", calls.len()),
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
                    self.emit_flags(&format!("tool:{}", call.name), output)
                        .await;
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
            // Failures drive the `auto` thinking controller: three
            // consecutive failing steps escalate the level.
            let failures = results
                .iter()
                .filter(|(_, result, _)| result.is_err())
                .count();
            let mut blocks: Vec<Content> = results
                .into_iter()
                .map(|(id, result, _)| match result {
                    Ok(output) => Content::Result {
                        id,
                        output,
                        is_error: false,
                    },
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
            self.messages.push(Message {
                role: "user".into(),
                content: blocks,
            });
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

    async fn emit_flags(&self, source: &str, text: &str) {
        if let Ok(hits) = self.ctf.scan(source, text) {
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
                if matches!(verdict, crate::report::Verdict::Verified) {
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

fn task_requires_tool(query: &str) -> bool {
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

/// Stable operating doctrine for the coding loop. Kept as one constant so it
/// sits above the volatile prompt boundary and remains prefix-cache friendly.
const AGENTIC_ORCHESTRATOR_DOCTRINE: &str = "\
Research first: use the repo map, hot memory, and explicit file reads; do not mutate from guesses.
Gate protocol: before read_file/grep/glob, check whether the answer is already in memory, pinned files, or the repo map.
Parallelism: batch independent read-only tools and independent delegate_task calls in one response. Sequential calls are only for true dependencies.
Delegation: for broad exploration, spawn focused delegate_task children and consume their summaries instead of flooding the main context.
Editing: prefer search_replace for code changes. Read the file first, use a unique exact search_block with surrounding context, preserve indentation, and keep edits atomic. Use apply_patch only for multi-file add/delete/move patches.
Verification: after edits, run the relevant build, test, linter, or configured check. If it fails, ingest the error and repair without waiting.
Latency discipline: keep narration minimal, avoid repeated unchanged calls, choose cheap probes first, and change strategy after two low-information steps.
Safety: destructive shell commands still require approval. Plan mode is read-only.
Completion: finish with a concise summary of what changed and what passed.";

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
    fn action_requests_require_a_tool_but_chat_does_not() {
        assert!(task_requires_tool("Use the shell tool to run printf ok"));
        assert!(task_requires_tool("fix the scroll bug"));
        assert!(!task_requires_tool("hii"));
        assert!(!task_requires_tool("explain Rust ownership"));
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
        inflight: std::sync::atomic::AtomicUsize,
        peak_inflight: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Response>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into_iter().collect()),
                inflight: std::sync::atomic::AtomicUsize::new(0),
                peak_inflight: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn peak(&self) -> usize {
            self.peak_inflight.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn chat_stream_with_think(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[serde_json::Value],
            _require_tool: bool,
            _progress: Option<&mpsc::UnboundedSender<Progress>>,
            _think: ThinkLevel,
        ) -> anyhow::Result<Response> {
            use std::sync::atomic::Ordering;
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
                    id: "call-1".into(),
                    name: "delegate_task".into(),
                    input: serde_json::json!({"task": "summarize part a", "task_id": "part-a"}),
                }),
                Content::Call(ToolCall {
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

    #[test]
    fn step_titles_name_the_call_and_cap_long_commands() {
        let call = ToolCall {
            id: "1".into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "echo hi"}),
        };
        assert_eq!(step_title(&call, true), "shell echo hi");
        assert_eq!(step_title(&call, false), "shell echo hi (failed)");
        let long = ToolCall {
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
}
