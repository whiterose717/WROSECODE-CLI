use crate::config::Config;
use crate::ctf::CtfEngine;
use crate::harness::Harness;
use crate::memory::Memory;
use crate::metrics::Metrics;
use crate::provider::{Content, Message, Progress, Provider, ToolCall, Usage};
use crate::repo_map::RepoMap;
use crate::skills::{self, Skill};
use crate::tools::{self, Tools};
use anyhow::{bail, Result};
use futures::future::join_all;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;

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
    pub thinking_level: u8,
    pub usage: Usage,
    pub model_turns: usize,
    pub started_at: Instant,
    pub metrics: Metrics,
    pub task_results: Arc<Mutex<HashMap<String, String>>>,
    pub store: crate::store::Store,
}

impl Agent {
    pub fn set_mode(&mut self, mode: &str) -> Result<()> {
        if !matches!(mode, "build" | "plan" | "general") {
            bail!("unknown agent: {mode}");
        }
        self.mode = mode.into();
        self.tools.plan = mode != "build";
        Ok(())
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
            .complete(system, &messages, &[], false, None)
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
            .complete(system, &messages, &[], false, None)
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
            thinking_level: config.thinking_level,
            usage: Usage::default(),
            model_turns: 0,
            started_at: Instant::now(),
            metrics: Metrics::load(&config.root),
            task_results: Arc::new(Mutex::new(HashMap::new())),
            store: crate::store::Store::open_default()?,
        })
    }

    pub async fn turn(&mut self, text: &str) -> Result<String> {
        self.ctf.category = crate::ctf::categorize(&self.config.root, text);
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
                thinking_level: self.thinking_level,
                usage: Usage::default(),
                model_turns: 0,
                started_at: Instant::now(),
                metrics: self.metrics.clone(),
                task_results: self.task_results.clone(),
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

    async fn run(&mut self, query: &str) -> Result<String> {
        let mut last_text = String::new();
        let mut repairs = 0;
        let mut used_tools_this_turn = false;
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
            let system = format!(
                "{}\nProject: {}\nMode: {}\nCTF category: {}\nThinking level: {}/20. At higher levels, use independent delegate_task calls, checker tasks, and race strategies when useful; never exceed 20 concurrent tasks.\nRules: Plan once silently, batch independent read-only tools in one response, choose the cheapest probe first, never repeat an unchanged call, and change strategy after two steps without new information. Prefer rg over grep, fd over find, and feroxbuster over gobuster when installed. Keep narration to one short preamble per tool batch. Work until the answer is verified. For CTF work, actively search for and verify flag formats; do not stop after describing navigation steps.\nRelevant memory:\n{}\nRepo map:\n{}\nSkill:\n{}",
                self.harness.prompt(),
                self.config.root.display(),
                self.mode,
                self.ctf.category,
                self.thinking_level,
                memory,
                map,
                skill_text
            );
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(Progress::ResetText);
            }
            let started = Instant::now();
            let schemas = self.tools.schemas();
            let mut selected_provider = self.provider.clone();
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
                    .complete(
                        &prompt,
                        &self.messages,
                        &schemas,
                        false,
                        self.event_tx.as_ref(),
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
                    .complete(
                        &system,
                        &self.messages,
                        &schemas,
                        true,
                        self.event_tx.as_ref(),
                    )
                    .await?;
                forced.usage.add(first_usage);
                response = forced;
            }
            self.last_api_latency = Some(started.elapsed());
            self.model_turns += 1;
            self.usage.add(response.usage);
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
                return Ok(last_text);
            }
            used_tools_this_turn = true;
            for call in &calls {
                let detail = call.input["path"]
                    .as_str()
                    .or_else(|| call.input["pattern"].as_str())
                    .or_else(|| call.input["command"].as_str())
                    .or_else(|| call.input["url"].as_str())
                    .or_else(|| call.input["query"].as_str())
                    .or_else(|| call.input["task"].as_str())
                    .unwrap_or("");
                if let Some(tx) = &self.event_tx {
                    let _ = tx.send(Progress::ToolBegin {
                        id: call.id.clone(),
                        title: format_tool_title(&call.name, detail),
                    });
                }
            }
            let provider = self.provider.clone();
            let config = self.config.clone();
            let tools = self.tools.clone();
            let harness = self.harness;
            let map = self.repo_map.clone();
            let event_tx = self.event_tx.clone();
            let dynamic_parallel = match self.thinking_level {
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
                async move {
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
                                event_tx,
                                mode: "build".into(),
                                active_skill: None,
                                last_api_latency: None,
                                ctf: CtfEngine::new(&config.root)
                                    .with_alert_bell(config.alert_bell),
                                thinking_level: config.thinking_level,
                                usage: Usage::default(),
                                model_turns: 0,
                                started_at: Instant::now(),
                                metrics: metrics.clone(),
                                task_results: task_results.clone(),
                                store,
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
            let edited_paths: Vec<String> = calls
                .iter()
                .zip(results.iter())
                .filter(|(call, (_, result, _))| {
                    matches!(call.name.as_str(), "write_file" | "edit_file") && result.is_ok()
                })
                .filter_map(|(call, _)| call.input["path"].as_str().map(str::to_owned))
                .collect();
            for (call, (_, result, elapsed_ms)) in calls.iter().zip(results.iter()) {
                self.metrics.record_tool(result.is_ok());
                if let Ok(output) = result {
                    self.emit_flags(&format!("tool:{}", call.name), output)
                        .await;
                }
                if let Some(tx) = &self.event_tx {
                    let _ = tx.send(Progress::ToolEnd {
                        id: call.id.clone(),
                        ok: result.is_ok(),
                        output: match result {
                            Ok(text) => text.clone(),
                            Err(error) => error.to_string(),
                        },
                        elapsed_ms: *elapsed_ms,
                    });
                }
            }
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
                    let commit = auto_commit(&self.config.root, &edited_paths, &last_text).await;
                    if let Err(error) = commit {
                        blocks.push(Content::Text(format!(
                            "Automatic git commit failed: {error}"
                        )));
                    }
                }
            }
            self.messages.push(Message {
                role: "user".into(),
                content: blocks,
            });
        }
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
    let verb = match name {
        "shell" => "Ran",
        "read_file" => "Read",
        "grep" | "glob" => "Searched",
        "edit_file" | "write_file" => "Edited",
        "web_fetch" | "web_search" => "Fetched",
        "delegate_task" => "Delegated",
        _ => "Ran",
    };
    format!("{verb} {name} {detail}").trim().to_string()
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

async fn auto_commit(root: &std::path::Path, paths: &[String], summary: &str) -> Result<()> {
    let probe = tokio::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .await?;
    if !probe.status.success() || probe.stdout != b"true\n" {
        return Ok(());
    }
    let message = format!(
        "wrosecode: {}",
        summary
            .lines()
            .next()
            .unwrap_or("update files")
            .chars()
            .take(70)
            .collect::<String>()
    );
    let output = tokio::process::Command::new("git")
        .arg("add")
        .arg("--")
        .args(paths)
        .current_dir(root)
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = tokio::process::Command::new("git")
        .arg("commit")
        .arg("--only")
        .arg("-m")
        .arg(message)
        .arg("--")
        .args(paths)
        .current_dir(root)
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::task_requires_tool;

    #[test]
    fn action_requests_require_a_tool_but_chat_does_not() {
        assert!(task_requires_tool("Use the shell tool to run printf ok"));
        assert!(task_requires_tool("fix the scroll bug"));
        assert!(!task_requires_tool("hii"));
        assert!(!task_requires_tool("explain Rust ownership"));
    }
}
