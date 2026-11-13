pub mod browser;
pub mod burp;
pub mod cache;
pub mod computer;
pub mod decode;
pub mod fs;
pub mod http;
pub mod mcp;
pub mod patch;
pub mod search;
pub mod shell;
pub mod vector;

use crate::config::{Config, Permission};
use crate::provider::ToolCall;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// Keep memory growth bounded as the agent reads more files in long sessions.
const RESULT_CACHE_MAX_ENTRIES: usize = 64;
const RESULT_CACHE_MAX_BYTES: usize = 1024 * 1024;

/// Resource class used by the per-batch scheduler. The aggregate batch limit
/// still applies; these gates stop one class (for example four CPU-heavy
/// delegates) from consuming every permit while independent reads wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolClass {
    ReadOnlyIo,
    Mutating,
    CpuHeavy,
    Network,
}

impl ToolClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnlyIo => "read_only_io",
            Self::Mutating => "mutating",
            Self::CpuHeavy => "cpu_heavy",
            Self::Network => "network",
        }
    }
}

/// Classify a tool call conservatively. Anything unknown is treated as
/// mutating so a new tool can never gain concurrency by accident.
pub fn classify(call: &ToolCall) -> ToolClass {
    match call.name.as_str() {
        "read_file" | "grep" | "glob" | "decode" | "web_fetch" | "web_search"
        | "search_writeups" | "burp_import" => ToolClass::ReadOnlyIo,
        "shell" => match call.input["command"].as_str() {
            Some(command) if !shell::destructive(command) && shell::read_only(command) => {
                ToolClass::ReadOnlyIo
            }
            _ => ToolClass::Mutating,
        },
        "delegate_task" => ToolClass::CpuHeavy,
        "http" | "browser_capture" | "burp_export" | "computer" | "mcp" | "archive_writeup" => {
            ToolClass::Network
        }
        _ => ToolClass::Mutating,
    }
}

/// Semaphores for one dispatch batch. Created per batch because the agent
/// already serializes user turns; a batch-local gate is enough to enforce
/// resource-class budgets without adding cross-turn lock contention.
#[derive(Clone)]
pub struct ToolClassLimits {
    read: Arc<tokio::sync::Semaphore>,
    cpu: Arc<tokio::sync::Semaphore>,
    network: Arc<tokio::sync::Semaphore>,
    mutate: Arc<tokio::sync::Semaphore>,
}

impl ToolClassLimits {
    pub fn new(configured_max: usize) -> Self {
        let max = configured_max.max(1);
        let gate = |capacity: usize| Arc::new(tokio::sync::Semaphore::new(capacity.min(max)));
        Self {
            read: gate(max),
            cpu: gate(2),
            network: gate(4),
            mutate: gate(1),
        }
    }

    pub fn for_call(&self, class: ToolClass) -> Arc<tokio::sync::Semaphore> {
        match class {
            ToolClass::ReadOnlyIo => self.read.clone(),
            ToolClass::CpuHeavy => self.cpu.clone(),
            ToolClass::Network => self.network.clone(),
            ToolClass::Mutating => self.mutate.clone(),
        }
    }

    /// Remaining permits for a class. Tests use this to pin the budgets.
    #[cfg(test)]
    pub fn available(&self, class: ToolClass) -> usize {
        self.for_call(class).available_permits()
    }
}

#[derive(Default)]
struct BoundedResultCache {
    entries: HashMap<String, String>,
    oldest_first: VecDeque<String>,
    bytes: usize,
}

impl BoundedResultCache {
    fn get(&self, key: &str) -> Option<&String> {
        self.entries.get(key)
    }

    fn insert(&mut self, key: String, value: String) {
        let size = key.len().saturating_add(value.len());
        if size > RESULT_CACHE_MAX_BYTES {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(key.len() + previous.len());
            self.oldest_first.retain(|existing| existing != &key);
        }
        while self.entries.len() >= RESULT_CACHE_MAX_ENTRIES
            || self.bytes.saturating_add(size) > RESULT_CACHE_MAX_BYTES
        {
            let Some(oldest) = self.oldest_first.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(oldest.len() + removed.len());
            }
        }
        self.bytes += size;
        self.oldest_first.push_back(key.clone());
        self.entries.insert(key, value);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.oldest_first.clear();
        self.bytes = 0;
    }
}

pub struct PermissionRequest {
    pub action: String,
    pub response: oneshot::Sender<bool>,
}

#[derive(Clone)]
pub struct Tools {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
    prompt_lock: Arc<Mutex<()>>,
    read_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub created_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub referenced_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub plan: bool,
    /// `/bugbounty` mode is active: advertise the evidence-gated tool even
    /// before the workspace file exists.
    pub bugbounty_mode: bool,
    pub mcps: Vec<(String, Arc<mcp::Mcp>)>,
    pub permission_tx: Option<mpsc::UnboundedSender<PermissionRequest>>,
    result_cache: Arc<Mutex<BoundedResultCache>>,
    cache_hits: Arc<AtomicUsize>,
    cache_saved_bytes: Arc<AtomicUsize>,
    /// The visible plan checklist maintained by the `update_plan` tool.
    pub checklist: Arc<Mutex<Vec<(String, bool)>>>,
    redis_addr: Option<String>,
    pub sandbox: Arc<crate::sandbox::Sandbox>,
    /// CTF autopilot allowlist: `Some` means every call is checked against it
    /// (writes and network outside it need approval; in-scope mutations are
    /// auto-approved per spec 4).
    pub scope: Option<Arc<crate::autopilot::Scope>>,
    /// Language servers for this workspace; each file write appends whatever
    /// the file's server says about the change (opencode parity). A tokio
    /// lock because diagnostics wait on the server across await points.
    pub lsp: Arc<tokio::sync::Mutex<crate::lsp::Lsp>>,
}

impl Tools {
    pub fn new(config: Arc<Config>, client: reqwest::Client) -> Self {
        let redis_addr = config.redis_url.as_deref().and_then(cache::redis_address);
        let sandbox = Arc::new(crate::sandbox::Sandbox::new(
            config.sandbox.clone(),
            config.root.clone(),
        ));
        let lsp = Arc::new(tokio::sync::Mutex::new(crate::lsp::Lsp::new(
            config.root.clone(),
            config.lsp.clone(),
        )));
        Self {
            config,
            client,
            prompt_lock: Arc::new(Mutex::new(())),
            read_files: Arc::new(Mutex::new(HashSet::new())),
            created_files: Arc::new(Mutex::new(HashSet::new())),
            referenced_files: Arc::new(Mutex::new(HashSet::new())),
            plan: false,
            bugbounty_mode: false,
            mcps: Vec::new(),
            permission_tx: None,
            result_cache: Arc::new(Mutex::new(BoundedResultCache::default())),
            cache_hits: Arc::new(AtomicUsize::new(0)),
            cache_saved_bytes: Arc::new(AtomicUsize::new(0)),
            checklist: Arc::new(Mutex::new(Vec::new())),
            redis_addr,
            sandbox,
            scope: None,
            lsp,
        }
    }

    /// Arm (or clear) the CTF autopilot scope guard.
    pub fn set_scope(&mut self, scope: Option<crate::autopilot::Scope>) {
        self.scope = scope.map(Arc::new);
    }

    pub fn schemas(&self) -> Vec<Value> {
        let mut schemas = vec![
            schema(
                "shell",
                "Run an external executable in the project (build, tests, git, diagnostics, scripts). \
                 For file discovery/read/search prefer separate glob, read_file and grep calls, \
                 which show distinct live progress in the UI. Default 60s; pass timeout up to 3600s.",
                json!({"command":"string","timeout":"string"}),
            ),
            schema("read_file", "Read a UTF-8 file", json!({"path":"string"})),
            schema(
                "write_file",
                "Create or replace a UTF-8 file",
                json!({"path":"string","content":"string"}),
            ),
            schema(
                "edit_file",
                "Compatibility alias for search_replace. Replace text matching exactly once; fails cleanly if the old block is absent or ambiguous.",
                json!({"path":"string","old":"string","new":"string"}),
            ),
            schema(
                "search_replace",
                "Preferred editing tool. Exact semantic search-and-replace: read_file first, provide a unique existing search_block with enough surrounding context, and a replace_block preserving indentation. Fails cleanly on drift or ambiguity.",
                json!({"path":"string","search_block":"string","replace_block":"string"}),
            ),
            schema(
                "apply_patch",
                "Fallback only for multi-file moves/adds/deletes. Prefer search_replace for ordinary code edits because it anchors on exact current source.",
                json!({"patch":"string"}),
            ),
            schema(
                "grep",
                "Search text in project files",
                json!({"pattern":"string"}),
            ),
            schema(
                "glob",
                "Find project files by glob",
                json!({"pattern":"string"}),
            ),
            schema(
                "decode",
                "Encoding sweep: base64/32/58/85, hex, rot/Caesar, atbash, XOR, URL, gzip/zlib/bz2/xz, morse, binary, nested chains",
                json!({"text":"string","path":"string","depth":"number"}),
            ),
            schema("web_search", "Search the web", json!({"query":"string"})),
            schema("web_fetch", "Fetch a web page", json!({"url":"string"})),
            schema(
                "browser_capture",
                "Capture a web page screenshot with installed Playwright",
                json!({"url":"string","output":"string"}),
            ),
            schema(
                "burp_import",
                "Parse a Burp raw HTTP request file",
                json!({"path":"string"}),
            ),
            schema(
                "burp_export",
                "Write a raw HTTP request for Burp Repeater",
                json!({"path":"string","request":"string"}),
            ),
            schema(
                "archive_writeup",
                "Store a solved writeup in the optional Qdrant knowledge cache",
                json!({"title":"string","text":"string"}),
            ),
            schema(
                "search_writeups",
                "Search past writeups in the optional Qdrant knowledge cache",
                json!({"query":"string"}),
            ),
            schema(
                "http",
                "Send an HTTP request",
                json!({"method":"string","url":"string"}),
            ),
            json!({"name":"delegate_task","description":"Run or resume a parallel child agent; optional provider/model routing and stable task_id",
                "input_schema":{"type":"object","properties":{"task":{"type":"string"},"task_id":{"type":"string"},"resume":{"type":"boolean"},"provider":{"type":"string"},"model":{"type":"string"}}}}),
            json!({"name":"update_plan","description":"Replace the visible plan checklist; items are {text, done} objects",
                "input_schema":{"type":"object","properties":{"items":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"done":{"type":"boolean"}},"required":["text"]}}},"required":["items"]}}),
            json!({"name":"coverage","description":"Engagement coverage checklist (persisted in .wrosecode/coverage.json): list what has been tested, add an item, or mark one done/undone. Check items off as you verify them so skipped areas stay visible across sessions.",
                "input_schema":{"type":"object","properties":{"action":{"type":"string","enum":["list","add","done","undone"],"description":"list, add, done, or undone"},"item":{"type":"string","description":"the checklist item (for add/done; substring match works)"}},"required":["action"]}}),
        ];
        // The bug-bounty tool is only advertised where it can work: an armed
        // `/bugbounty` run or an existing `--program` workspace.
        if self.bugbounty_mode || crate::bugbounty::Workspace::path(&self.config.root).exists() {
            schemas.push(crate::bugbounty::schema());
        }
        // open-interpreter parity: OS control is opt-in, so its schema is
        // only ever advertised when the user asked for it.
        if self.config.computer_tools {
            schemas.push(json!({"name":"computer","description":"Control this machine's desktop (opt-in OS tool): screenshot the display, click, type, or press keys via xdotool/grim-family backends. Every call needs approval like `shell` does.",
                "input_schema":{"type":"object","properties":{
                    "action":{"type":"string","enum":["screenshot","click","type","key","scroll"],"description":"what to do"},
                    "path":{"type":"string","description":"screenshot only: output PNG (default .wrosecode/computer/shot-<time>.png, inside the project)"},
                    "x":{"type":"number","description":"click only: X coordinate (0..=10000)"},
                    "y":{"type":"number","description":"click only: Y coordinate (0..=10000)"},
                    "button":{"type":"string","description":"click only: left|middle|right or 1..=5 (default left)"},
                    "text":{"type":"string","description":"type only: the literal text to type (sent as one argument, never a shell string)"},
                    "keys":{"type":"string","description":"key only: keysym or combo, e.g. Return, ctrl+c"},
                    "amount":{"type":"number","description":"scroll only: positive scrolls up, negative down, |amount| capped at 50 (default 1)"}
                },"required":["action"]}}));
        }
        for (server, mcp) in &self.mcps {
            schemas.extend(mcp.schemas.iter().map(|schema| {
                let mut schema = schema.clone();
                if let Some(name) = schema["name"].as_str() {
                    schema["name"] =
                        format!("mcp__{server}__{}", name.trim_start_matches("mcp__")).into();
                }
                schema
            }));
        }
        schemas
    }

    pub async fn execute(&self, call: &ToolCall) -> Result<String> {
        // Argument-decoding failures are reported by the provider as a
        // reserved input object. Refuse before scope checks, authorization,
        // caching, or dispatch: nothing may run from arguments that were
        // never valid JSON.
        if let Some(detail) = call
            .input
            .get(crate::provider::ARG_PARSE_ERROR)
            .and_then(serde_json::Value::as_str)
        {
            anyhow::bail!(
                "`{}` was not executed: {detail}. Re-issue the call with complete, valid JSON arguments.",
                call.name
            );
        }
        let input = &call.input;
        self.scope_check(&call.name, input).await?;
        let input_text = input.to_string();
        let created = self
            .created_files
            .lock()
            .map_err(|_| anyhow::anyhow!("file tracking lock poisoned"))?
            .clone();
        {
            let mut referenced = self
                .referenced_files
                .lock()
                .map_err(|_| anyhow::anyhow!("reference tracking lock poisoned"))?;
            for path in created {
                let relative = path
                    .strip_prefix(&self.config.root)
                    .unwrap_or(&path)
                    .to_string_lossy();
                if !relative.is_empty() && input_text.contains(relative.as_ref()) {
                    referenced.insert(path);
                }
            }
        }
        let cache_key = self.cache_key(call);
        if let Some(key) = &cache_key {
            if let Some(cached) = self
                .result_cache
                .lock()
                .map_err(|_| anyhow::anyhow!("result cache lock poisoned"))?
                .get(key)
                .cloned()
            {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                self.cache_saved_bytes
                    .fetch_add(cached.len(), Ordering::Relaxed);
                return Ok(format!("[cached: identical call was not rerun]\n{cached}"));
            }
            if let Some(address) = &self.redis_addr {
                if let Ok(Some(cached)) = cache::redis_get(address, key).await {
                    self.cache_hits.fetch_add(1, Ordering::Relaxed);
                    self.cache_saved_bytes
                        .fetch_add(cached.len(), Ordering::Relaxed);
                    return Ok(format!("[redis cache hit]\n{cached}"));
                }
            }
        }
        let output = match call.name.as_str() {
            "shell" => {
                let command = arg(input, "command")?;
                self.authorize(&call.name, Some(command)).await?;
                self.sandbox
                    .run(
                        command,
                        shell_timeout(input, self.config.shell_timeout_seconds),
                    )
                    .await?
            }
            "read_file" => {
                let path = arg(input, "path")?;
                let content = fs::read(&self.config.root, path).await?;
                let resolved = fs::resolve(&self.config.root, path)?;
                self.referenced_files
                    .lock()
                    .map_err(|_| anyhow::anyhow!("reference tracking lock poisoned"))?
                    .insert(resolved.clone());
                // Reading a file is usually the step before editing it, so
                // this is where its language server gets started.
                self.lsp.lock().await.spawn_for(&resolved).await;
                self.read_files
                    .lock()
                    .map_err(|_| anyhow::anyhow!("read tracking lock poisoned"))?
                    .insert(resolved);
                content
            }
            "write_file" => {
                self.authorize(&call.name, None).await?;
                let path = arg(input, "path")?;
                self.require_read(path)?;
                let resolved = fs::resolve(&self.config.root, path)?;
                let created = !resolved.exists();
                let new_dirs: Vec<_> = resolved
                    .parent()
                    .into_iter()
                    .flat_map(|parent| parent.ancestors())
                    .take_while(|dir| !dir.exists())
                    .map(PathBuf::from)
                    .collect();
                let result = fs::write(&self.config.root, path, arg(input, "content")?).await?;
                let mut tracked = self
                    .created_files
                    .lock()
                    .map_err(|_| anyhow::anyhow!("file tracking lock poisoned"))?;
                if created {
                    tracked.insert(resolved);
                }
                tracked.extend(new_dirs);
                self.forget_read(path)?;
                result
            }
            "edit_file" | "search_replace" => {
                self.authorize(&call.name, None).await?;
                let path = arg(input, "path")?;
                self.require_read(path)?;
                let old = input
                    .get("search_block")
                    .and_then(|value| value.as_str())
                    .or_else(|| input.get("old").and_then(|value| value.as_str()))
                    .ok_or_else(|| anyhow::anyhow!("missing search_block"))?;
                let new = input
                    .get("replace_block")
                    .and_then(|value| value.as_str())
                    .or_else(|| input.get("new").and_then(|value| value.as_str()))
                    .ok_or_else(|| anyhow::anyhow!("missing replace_block"))?;
                let result = fs::edit(&self.config.root, path, old, new).await?;
                self.forget_read(path)?;
                result
            }
            "apply_patch" => {
                self.authorize(&call.name, None).await?;
                let patch = arg(input, "patch")?;
                for path in patch::paths(patch) {
                    self.require_read(&path)?;
                }
                let (result, created) = patch::apply(&self.config.root, patch).await?;
                for path in patch::paths(patch) {
                    self.forget_read(&path)?;
                }
                if !created.is_empty() {
                    let mut tracked = self
                        .created_files
                        .lock()
                        .map_err(|_| anyhow::anyhow!("file tracking lock poisoned"))?;
                    tracked.extend(created);
                }
                result
            }
            "grep" => search::grep(&self.config.root, arg(input, "pattern")?)?,
            "glob" => search::glob(&self.config.root, arg(input, "pattern")?)?,
            "decode" => {
                decode::sweep(
                    input.get("text").and_then(|value| value.as_str()),
                    input.get("path").and_then(|value| value.as_str()),
                    input
                        .get("depth")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(3),
                    &self.config.root,
                )
                .await?
            }
            "web_search" => http::web_search(&self.client, arg(input, "query")?).await?,
            "web_fetch" => {
                if self.bugbounty_mode {
                    http::request_bounty("GET", arg(input, "url")?, None).await?
                } else {
                    http::request(&self.client, "GET", arg(input, "url")?, None).await?
                }
            }
            "browser_capture" => {
                self.authorize(&call.name, None).await?;
                browser::capture(
                    &self.config.root,
                    arg(input, "url")?,
                    arg(input, "output")?,
                    self.config.shell_timeout_seconds,
                )
                .await?
            }
            "burp_import" => burp::import(&self.config.root, arg(input, "path")?)?,
            "computer" => {
                if !self.config.computer_tools {
                    bail!(
                        "computer tools are off; set [tools] computer = true in config.toml \
                         or pass --computer"
                    );
                }
                let action = computer::action_of(input)?;
                self.authorize(&call.name, None).await?;
                computer::perform(
                    &self.config.root,
                    input,
                    action,
                    self.config.shell_timeout_seconds,
                )
                .await?
            }
            "burp_export" => {
                self.authorize(&call.name, None).await?;
                burp::export(
                    &self.config.root,
                    arg(input, "path")?,
                    arg(input, "request")?,
                )?
            }
            "archive_writeup" => {
                let url = self
                    .config
                    .qdrant_url
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("cache.qdrant_url is not configured"))?;
                vector::archive(&self.client, url, arg(input, "title")?, arg(input, "text")?)
                    .await?
            }
            "search_writeups" => {
                let url = self
                    .config
                    .qdrant_url
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("cache.qdrant_url is not configured"))?;
                vector::search(&self.client, url, arg(input, "query")?).await?
            }
            "http" => {
                let method = input["method"].as_str().unwrap_or("GET");
                if method != "GET" && method != "HEAD" {
                    self.authorize(&call.name, None).await?;
                }
                if self.bugbounty_mode {
                    http::request_bounty(method, arg(input, "url")?, input["body"].as_str()).await?
                } else {
                    http::request(
                        &self.client,
                        method,
                        arg(input, "url")?,
                        input["body"].as_str(),
                    )
                    .await?
                }
            }
            "update_plan" => {
                let items = input["items"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("items must be an array"))?;
                let mut plan: Vec<(String, bool)> = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(text) = item.as_str() {
                        plan.push((text.to_string(), false));
                        continue;
                    }
                    let text = item["text"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("plan item needs a text string"))?;
                    plan.push((text.to_string(), item["done"].as_bool().unwrap_or(false)));
                }
                let total = plan.len();
                let done = plan.iter().filter(|(_, done)| *done).count();
                *self
                    .checklist
                    .lock()
                    .map_err(|_| anyhow::anyhow!("checklist lock poisoned"))? = plan;
                format!("Plan updated: {done} of {total} steps done")
            }
            "coverage" => {
                let action = input["action"].as_str().ok_or_else(|| {
                    anyhow::anyhow!("action must be one of: list, add, done, undone")
                })?;
                let mut coverage = crate::coverage::Coverage::load(&self.config.root)?;
                let item = input["item"].as_str().unwrap_or_default();
                match action {
                    "list" => coverage.render(),
                    "add" => {
                        if item.trim().is_empty() {
                            bail!("coverage add needs an item");
                        }
                        if coverage.add(item) {
                            coverage.save(&self.config.root)?;
                            format!("Added. {}", coverage.render())
                        } else {
                            format!("Already tracked. {}", coverage.render())
                        }
                    }
                    "done" | "undone" => {
                        if item.trim().is_empty() {
                            bail!("coverage {action} needs an item");
                        }
                        let index = coverage.find(item)?;
                        let checked = action == "done";
                        let text = coverage.set_done(index, checked).text.clone();
                        coverage.save(&self.config.root)?;
                        format!(
                            "{}: {text}\n{}",
                            if checked { "Checked off" } else { "Reopened" },
                            coverage.render()
                        )
                    }
                    other => {
                        bail!("unknown coverage action {other:?} (use list, add, done, undone)")
                    }
                }
            }
            "bugbounty" => {
                let action = input["action"].as_str().unwrap_or("status");
                if !matches!(action, "status" | "list") {
                    self.authorize(&call.name, None).await?;
                }
                crate::bugbounty::tool_action(
                    &self.config.root,
                    self.scope.as_deref(),
                    action,
                    input,
                )?
            }
            "delegate_task" => bail!("delegate_task is handled by the agent"),
            _ if call.name.starts_with("mcp__") => {
                self.authorize("mcp", None).await?;
                let rest = call.name.trim_start_matches("mcp__");
                let (server, tool) = rest
                    .split_once("__")
                    .ok_or_else(|| anyhow::anyhow!("MCP tool must include server name"))?;
                self.mcps
                    .iter()
                    .find(|(name, _)| name == server)
                    .ok_or_else(|| anyhow::anyhow!("MCP server {server} is not connected"))?
                    .1
                    .call(tool, input.clone())
                    .await?
            }
            _ => bail!("unknown tool: {}", call.name),
        };
        // Whatever the file's language server makes of the change comes
        // back with the write itself, so the agent can fix its mistake
        // before the next turn instead of after the run fails.
        let output = if matches!(
            call.name.as_str(),
            "write_file" | "edit_file" | "search_replace" | "apply_patch"
        ) {
            match self.lsp_diagnostics(&call.name, input).await {
                Some(feedback) => format!("{output}\n\n{feedback}"),
                None => output,
            }
        } else {
            output
        };
        // Bounded observation window (SWE-agent's max_observation_length plus
        // PentestGPT's parsing module): a huge shell observation becomes a
        // head+tail summary plus a file pointer instead of tens of thousands
        // of characters of context burn. The pointer lets the model grep or
        // read the evidence instead of rerunning the command.
        let output = if call.name == "shell" && output.len() > 32_000 {
            match dump_overflow(&self.config.root, &output) {
                Some(note) => format!("{output}{note}"),
                None => output,
            }
        } else {
            output
        };
        let output = truncate(output, 32_000);
        if let Some(key) = cache_key {
            self.result_cache
                .lock()
                .map_err(|_| anyhow::anyhow!("result cache lock poisoned"))?
                .insert(key.clone(), output.clone());
            if let Some(address) = &self.redis_addr {
                let _ = cache::redis_set(address, &key, &output).await;
            }
        } else if matches!(
            call.name.as_str(),
            "write_file" | "edit_file" | "search_replace" | "apply_patch" | "shell" | "bugbounty"
        ) {
            self.result_cache
                .lock()
                .map_err(|_| anyhow::anyhow!("result cache lock poisoned"))?
                .clear();
        }
        Ok(output)
    }

    /// The diagnostics for the files a write just touched, joined for the
    /// tool result. Silent when no server runs for them, none answers in
    /// time, or nothing is wrong — a clean edit reads exactly as before.
    async fn lsp_diagnostics(&self, name: &str, input: &Value) -> Option<String> {
        let paths: Vec<String> = match name {
            "write_file" | "edit_file" | "search_replace" => input["path"]
                .as_str()
                .map(|path| vec![path.to_string()])
                .unwrap_or_default(),
            "apply_patch" => {
                let patch = input["patch"].as_str()?;
                patch::paths(patch)
            }
            _ => return None,
        };
        let mut lsp = self.lsp.lock().await;
        let mut sections = Vec::new();
        for path in paths.iter().take(3) {
            let Ok(resolved) = fs::resolve(&self.config.root, path) else {
                continue;
            };
            if let Some(section) = lsp.diagnostics_after_edit(&resolved).await {
                sections.push(section);
            }
        }
        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n"))
        }
    }

    pub fn cache_stats(&self) -> (usize, usize) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_saved_bytes.load(Ordering::Relaxed),
        )
    }

    pub fn retryable(&self, call: &ToolCall) -> bool {
        if call.input.get(crate::provider::ARG_PARSE_ERROR).is_some() {
            return false;
        }
        matches!(
            call.name.as_str(),
            "read_file" | "grep" | "glob" | "web_search" | "web_fetch"
        ) || (call.name == "shell" && call.input["command"].as_str().is_some_and(shell::read_only))
    }

    fn cache_key(&self, call: &ToolCall) -> Option<String> {
        // Directory listings, grep/glob and arbitrary read-only shell probes
        // can become stale after another process edits the workspace. Cache
        // only file reads keyed by that exact file's modification timestamp.
        let cacheable = call.name == "read_file";
        if !cacheable {
            return None;
        }
        let mut key = format!("{}:{}", call.name, call.input);
        if call.name == "read_file" {
            let path = call.input["path"].as_str()?;
            let resolved = fs::resolve(&self.config.root, path).ok()?;
            let modified = std::fs::metadata(resolved)
                .and_then(|meta| meta.modified())
                .ok();
            key.push_str(&format!(":{modified:?}"));
        }
        Some(key)
    }

    /// The doctrine class a tool call belongs to. Exposed so the agent and
    /// tests can reason about scope requirements without duplicating the
    /// classification table.
    pub fn action_class(&self, name: &str, input: &Value) -> crate::bugbounty::ActionClass {
        crate::bugbounty::classify_action(name, input)
    }

    /// Reject (or ask about) any call that leaves the CTF autopilot
    /// allowlist. Headless runs have no permission channel, so an
    /// out-of-scope action is denied with the reason instead of hanging on a
    /// prompt nobody will answer.
    async fn scope_check(&self, name: &str, input: &Value) -> Result<()> {
        // Bug-bounty doctrine is enforced before the scope lookup: without an
        // armed scope a remote action is denied rather than silently allowed
        // by the generic "no scope configured" path.
        if self.bugbounty_mode {
            let class = self.action_class(name, input);
            crate::trace::global_event(
                "bugbounty_action",
                &format!(
                    "{name} class={} scope={}",
                    class.as_str(),
                    self.scope.is_some()
                ),
            );
            if class == crate::bugbounty::ActionClass::Prohibited {
                bail!(
                    "bug-bounty doctrine prohibits `{name}`: it can act outside the evidence \
                     model or the authorized host scope"
                );
            }
            if class.requires_scope() && self.scope.is_none() {
                bail!(
                    "bug-bounty mode needs an armed scope for `{name}` ({}); run \
                     /bugbounty <target> --scope <host> first",
                    class.as_str()
                );
            }
            // Arbitrary scripts are not parseable as network requests.
            // Do not bypass approvals merely because an allowlist is armed:
            // a shell can launch Python, scanners, or child processes that
            // never reveal their outbound hosts to the command-line parser.
            if name == "shell" && class == crate::bugbounty::ActionClass::Elevated {
                if let Some(tx) = &self.permission_tx {
                    let (response, answer) = oneshot::channel();
                    tx.send(PermissionRequest {
                        action: "bug-bounty elevated shell command (review network targets and side effects)".into(),
                        response,
                    }).map_err(|_| anyhow::anyhow!("permission UI closed"))?;
                    if !answer.await.unwrap_or(false) {
                        bail!("elevated bug-bounty shell command denied");
                    }
                } else {
                    bail!("elevated shell requires interactive approval in bug-bounty mode; use the scoped HTTP tool for low-impact requests");
                }
            }
            if matches!(name, "http" | "web_fetch" | "browser_capture") {
                let scope = self.scope.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("bug-bounty network tool needs an armed scope")
                })?;
                let url = input
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("bug-bounty network tool needs a target URL"))?;
                crate::bugbounty::check_network_url(scope, url)?;
            }
        }
        let Some(scope) = &self.scope else {
            return Ok(());
        };
        let Some(reason) = crate::autopilot::violation(scope, &self.config.root, name, input)
        else {
            return Ok(());
        };
        let action = format!("out-of-scope {reason}");
        if let Some(tx) = &self.permission_tx {
            let (response, answer) = oneshot::channel();
            tx.send(PermissionRequest {
                action: action.clone(),
                response,
            })
            .map_err(|_| anyhow::anyhow!("permission UI closed"))?;
            if !answer.await.unwrap_or(false) {
                bail!("{action}: denied");
            }
            return Ok(());
        }
        bail!("{action}: denied (headless run cannot approve it; widen --remote/--flag scope)");
    }

    pub async fn authorize(&self, name: &str, command: Option<&str>) -> Result<()> {
        let destructive = command.is_some_and(shell::destructive);
        let mutating = matches!(
            name,
            "write_file"
                | "edit_file"
                | "search_replace"
                | "apply_patch"
                | "http"
                | "mcp"
                | "browser_capture"
                | "burp_export"
                | "computer"
                | "bugbounty"
        ) || command.is_some_and(|c| !shell::read_only(c));
        if self.plan && mutating {
            bail!("plan mode is read-only");
        }
        // Inside a CTF scope, non-destructive work runs unattended (spec 4);
        // out-of-scope actions were already handled by `scope_check`.
        let needs_prompt = destructive
            || (self.scope.is_none()
                && match self.config.permission {
                    Permission::Ask => mutating,
                    Permission::AutoSafe => {
                        mutating && command.is_some_and(|c| !shell::read_only(c))
                    }
                    Permission::Yolo => false,
                });
        if needs_prompt {
            let action = format!(
                "{name}{}",
                command.map(|c| format!(" ({c})")).unwrap_or_default()
            );
            if let Some(tx) = &self.permission_tx {
                let (response, answer) = oneshot::channel();
                tx.send(PermissionRequest { action, response })
                    .map_err(|_| anyhow::anyhow!("permission UI closed"))?;
                if !answer.await.unwrap_or(false) {
                    bail!("permission denied");
                }
                return Ok(());
            }
            let _guard = self
                .prompt_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("prompt lock poisoned"))?;
            eprint!("Allow {action}? [y/N] ");
            io::stderr().flush()?;
            let mut answer = String::new();
            #[cfg(unix)]
            {
                use std::io::BufRead;
                let tty = std::fs::File::open("/dev/tty")
                    .map_err(|_| anyhow::anyhow!("permission prompt requires a terminal"))?;
                io::BufReader::new(tty).read_line(&mut answer)?;
            }
            #[cfg(not(unix))]
            io::stdin().read_line(&mut answer)?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                bail!("permission denied");
            }
        }
        Ok(())
    }

    fn require_read(&self, path: &str) -> Result<()> {
        let resolved = fs::resolve(&self.config.root, path)?;
        if resolved.exists()
            && !self
                .read_files
                .lock()
                .map_err(|_| anyhow::anyhow!("read tracking lock poisoned"))?
                .contains(&resolved)
        {
            bail!("read_file must be used before editing {path}");
        }
        Ok(())
    }

    /// Forget every read-before-edit claim and the read cache: `/undo` just
    /// restored different bytes, so those files must be read again.
    pub fn clear_reads(&self) -> Result<()> {
        self.read_files
            .lock()
            .map_err(|_| anyhow::anyhow!("read tracking lock poisoned"))?
            .clear();
        self.result_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("result cache lock poisoned"))?
            .clear();
        Ok(())
    }

    fn forget_read(&self, path: &str) -> Result<()> {
        self.read_files
            .lock()
            .map_err(|_| anyhow::anyhow!("read tracking lock poisoned"))?
            .remove(&fs::resolve(&self.config.root, path)?);
        Ok(())
    }
}

fn arg<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing {key}"))
}

/// Per-call shell timeout override in seconds, clamped to 1..=3600.
/// Missing, zero, negative, or unparseable values fall back to the
/// configured default, so old and terse model calls behave exactly as
/// before. Accepts JSON numbers and strings.
fn shell_timeout(input: &Value, default_secs: u64) -> u64 {
    let overridden = input.get("timeout").and_then(|value| {
        value.as_u64().or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<u64>().ok())
        })
    });
    match overridden {
        Some(0) | None => default_secs.max(1),
        Some(secs) => secs.clamp(1, 3600),
    }
}

fn schema(name: &str, description: &str, fields: Value) -> Value {
    let props = fields
        .as_object()
        .expect("schema fields")
        .iter()
        .map(|(k, _)| (k.clone(), json!({"type":"string"})))
        .collect::<serde_json::Map<_, _>>();
    let required = props.keys().cloned().collect::<Vec<_>>();
    json!({"name":name,"description":description,"input_schema":{"type":"object","properties":props,"required":required,"additionalProperties":false}})
}

/// Persist an oversized shell observation next to the workspace and return
/// the pointer note appended to the clipped text. The file is the evidence
/// the model can grep or read; the observation stays a compact summary.
fn dump_overflow(root: &std::path::Path, text: &str) -> Option<String> {
    let dir = root.join(".wrosecode/cache/tool-outputs");
    std::fs::create_dir_all(&dir).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = dir.join(format!("{stamp}-shell.txt"));
    std::fs::write(&path, text).ok()?;
    let display = path
        .strip_prefix(root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    Some(format!(
        "\n[Output ({} bytes) saved to {display} — grep or read_file it instead of rerunning the command]\n",
        text.len()
    ))
}

pub fn truncate(mut output: String, max: usize) -> String {
    output = output.replace('\u{1b}', "");
    let mut compact = Vec::new();
    for line in output.lines().map(str::trim_end) {
        if compact.last().is_some_and(|last: &String| last == line) {
            continue;
        }
        compact.push(line.to_string());
    }
    output = compact.join("\n");
    if output.len() > max {
        let head = max / 2;
        let tail = max.saturating_sub(head);
        let mut end = head;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        let mut start = output.len().saturating_sub(tail);
        while start < output.len() && !output.is_char_boundary(start) {
            start += 1;
        }
        output = format!(
            "{}\n[...{} bytes/lines omitted...]\n{}",
            &output[..end],
            output.lines().count().saturating_sub(2),
            &output[start..]
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_shell_output_is_dumped_and_pointed() {
        let dir = std::env::temp_dir().join(format!(
            "wrosecode-overflow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let big = format!("line\n{}", "x".repeat(40_000));
        let note = dump_overflow(&dir, &big).expect("dump note");
        assert!(
            note.contains("saved to .wrosecode/cache/tool-outputs/"),
            "{note}"
        );
        let clipped = truncate(format!("{big}{note}"), 32_000);
        // The pointer must survive the head+tail clip so the model can find
        // the evidence file instead of rerunning the command.
        assert!(
            clipped.contains("saved to .wrosecode/cache/tool-outputs/"),
            "{clipped}"
        );
        let written = std::fs::read_dir(dir.join(".wrosecode/cache/tool-outputs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read_to_string(&written).unwrap(), big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_tools(root: &std::path::Path, permission: Permission) -> Tools {
        let config = Config {
            root: root.to_path_buf(),
            permission,
            model: "test".into(),
            provider: "test".into(),
            harness: "minimal".into(),
            repair_retries: 0,
            check_command: None,
            skill_dirs: Vec::new(),
            think: crate::think::ThinkLevel::Medium,
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: crate::config::DEFAULT_SHELL_TIMEOUT_SECONDS,
            tool_retries: 3,
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
            alert_bell: true,
            smooth_scroll_lines: 1,
            sandbox: crate::sandbox::SandboxPolicy::default(),
            lsp: crate::lsp::LspSettings {
                enabled: false,
                ..Default::default()
            },
            computer_tools: false,
        };
        Tools::new(Arc::new(config), reqwest::Client::new())
    }

    #[tokio::test]
    async fn bounty_elevated_shell_and_cross_host_browser_are_not_autorun() {
        let dir = std::env::temp_dir().join(format!("wrose-bounty-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut tools = test_tools(&dir, Permission::Yolo);
        tools.bugbounty_mode = true;
        tools.set_scope(Some(crate::autopilot::Scope::for_bounty(
            &dir,
            "",
            ["app.example.test"],
            std::iter::empty::<&str>(),
        )));
        let shell = ToolCall {
            item_id: None,
            id: "shell-denied".into(),
            name: "shell".into(),
            input: json!({"command": "python3 sample_script.py"}),
        };
        let error = tools.execute(&shell).await.unwrap_err().to_string();
        assert!(error.contains("requires interactive approval"), "{error}");
        let browser = ToolCall {
            item_id: None,
            id: "browser-denied".into(),
            name: "browser_capture".into(),
            input: json!({"url": "https://app.example.test", "output": "x.png"}),
        };
        let error = tools.execute(&browser).await.unwrap_err().to_string();
        assert!(error.contains("doctrine prohibits"), "{error}");
        let malformed = ToolCall {
            item_id: None,
            id: "bad-url".into(),
            name: "http".into(),
            input: json!({"url": "not-a-url", "method": "GET"}),
        };
        let error = tools.execute(&malformed).await.unwrap_err().to_string();
        assert!(error.contains("invalid HTTP(S) target URL"), "{error}");
        let unauthorized = ToolCall {
            item_id: None,
            id: "outside".into(),
            name: "http".into(),
            input: json!({"url": "https://evil.test", "method": "GET"}),
        };
        let error = tools.execute(&unauthorized).await.unwrap_err().to_string();
        assert!(error.contains("outside the armed program scope"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn bugbounty_tool_dispatch_enforces_plan_scope_and_state() {
        let dir = std::env::temp_dir().join(format!(
            "wrose-bounty-tool-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let plain_root = dir.join("plain");
        std::fs::create_dir_all(&plain_root).unwrap();
        assert!(
            !test_tools(&plain_root, Permission::Yolo)
                .schemas()
                .iter()
                .any(|schema| schema["name"] == "bugbounty"),
            "the bug-bounty tool must not be advertised outside its workspace"
        );

        let scope = crate::autopilot::Scope::for_bounty(
            &dir,
            "https://app.example.test",
            ["app.example.test"],
            std::iter::empty::<&str>(),
        );
        crate::bugbounty::Workspace::load_or_init(
            &dir,
            "Example",
            "https://app.example.test",
            &scope,
        )
        .unwrap();

        let mut tools = test_tools(&dir, Permission::Yolo);
        tools.set_scope(Some(scope));
        tools.bugbounty_mode = true;
        assert!(tools
            .schemas()
            .iter()
            .any(|schema| schema["name"] == "bugbounty"));

        let call = |input: Value| ToolCall {
            item_id: None,
            id: format!("call-{input}"),
            name: "bugbounty".into(),
            input,
        };

        let status = tools
            .execute(&call(json!({"action": "status"})))
            .await
            .expect("status is ungated");
        assert!(status.contains("Example"), "{status}");

        let error = tools
            .execute(&call(json!({
                "action": "add",
                "title": "leak",
                "asset": "https://evil.test/x"
            })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the armed program scope"), "{error}");

        tools.plan = true;
        let error = tools
            .execute(&call(json!({
                "action": "add",
                "title": "leak",
                "asset": "https://app.example.test/x"
            })))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("plan mode is read-only"), "{error}");

        tools.plan = false;
        let added = tools
            .execute(&call(json!({
                "action": "add",
                "title": "IDOR in invoice endpoint",
                "asset": "https://app.example.test/api/invoices/42",
                "severity": "high"
            })))
            .await
            .expect("in-scope add runs");
        assert!(added.contains("BB-0001"), "{added}");

        let listed = tools
            .execute(&call(json!({"action": "list"})))
            .await
            .unwrap();
        assert!(listed.contains("BB-0001"), "{listed}");
        assert!(tools
            .execute(&call(json!({"action": "bogus"})))
            .await
            .is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let workspace_mode = std::fs::metadata(crate::bugbounty::Workspace::dir(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let findings_mode = std::fs::metadata(crate::bugbounty::Workspace::path(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(workspace_mode, 0o700, "workspace must be owner-only");
            assert_eq!(findings_mode, 0o600, "findings must be owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_result_cache_keeps_recent_entries() {
        let mut cache = BoundedResultCache::default();
        for index in 0..(RESULT_CACHE_MAX_ENTRIES + 3) {
            cache.insert(format!("file-{index}"), "content".to_string());
        }
        assert_eq!(cache.entries.len(), RESULT_CACHE_MAX_ENTRIES);
        assert!(cache.get("file-0").is_none());
        let latest = format!("file-{}", RESULT_CACHE_MAX_ENTRIES + 2);
        assert_eq!(cache.get(&latest).map(String::as_str), Some("content"));
        assert!(cache.bytes <= RESULT_CACHE_MAX_BYTES);
    }

    #[test]
    fn read_result_cache_caps_bytes_and_clear_resets_state() {
        let mut cache = BoundedResultCache::default();
        for index in 0..10 {
            cache.insert(
                format!("large-file-{index}"),
                "x".repeat(RESULT_CACHE_MAX_BYTES / 4),
            );
        }
        assert!(cache.bytes <= RESULT_CACHE_MAX_BYTES);
        assert!(cache.entries.len() <= 4);
        cache.insert(
            "oversized".to_string(),
            "x".repeat(RESULT_CACHE_MAX_BYTES + 1),
        );
        assert!(cache.get("oversized").is_none());
        cache.clear();
        assert!(cache.entries.is_empty());
        assert!(cache.oldest_first.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[tokio::test]
    async fn a_write_hands_the_agent_what_the_language_server_said() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("wrosecode-lsp-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        let script = dir.join("fake_server.py");
        std::fs::write(&script, crate::lsp::FAKE_SERVER).unwrap();

        let mut settings = crate::lsp::LspSettings {
            wait_ms: 4_000,
            ..Default::default()
        };
        settings.commands.insert(
            "rs".to_string(),
            vec!["python3".to_string(), script.display().to_string()],
        );
        let active_config = Config {
            root: dir.clone(),
            permission: Permission::Yolo,
            model: "test".into(),
            provider: "test".into(),
            harness: "minimal".into(),
            repair_retries: 0,
            check_command: None,
            skill_dirs: Vec::new(),
            think: crate::think::ThinkLevel::Medium,
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: crate::config::DEFAULT_SHELL_TIMEOUT_SECONDS,
            tool_retries: 3,
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
            alert_bell: true,
            smooth_scroll_lines: 1,
            sandbox: crate::sandbox::SandboxPolicy::default(),
            lsp: settings.clone(),
            computer_tools: false,
        };
        let tools = Tools::new(Arc::new(active_config), reqwest::Client::new());

        // Reading warms the server; the edit's result carries its verdict.
        let read = ToolCall {
            item_id: None,
            id: "1".into(),
            name: "read_file".into(),
            input: json!({"path": "main.rs"}),
        };
        tools.execute(&read).await.expect("read");

        let edit = ToolCall {
            item_id: None,
            id: "2".into(),
            name: "edit_file".into(),
            input: json!({
                "path": "main.rs",
                "old": "fn main() {}",
                "new": "fn main() { let value = 1; }",
            }),
        };
        let output = tools.execute(&edit).await.expect("edit");
        assert!(
            output.contains("LSP diagnostics (python3) in"),
            "diagnostics should ride along with the edit: {output}"
        );
        assert!(
            output.contains("[error] 4:8 fake error from the stub server"),
            "{output}"
        );

        // With the client switched off the very same edit reads as before.
        settings.enabled = false;
        let quiet = Config {
            lsp: settings,
            ..active_config_for(&dir)
        };
        let tools = Tools::new(Arc::new(quiet), reqwest::Client::new());
        let read = ToolCall {
            item_id: None,
            id: "3".into(),
            name: "read_file".into(),
            input: json!({"path": "main.rs"}),
        };
        tools.execute(&read).await.expect("read");
        let edit = ToolCall {
            item_id: None,
            id: "4".into(),
            name: "edit_file".into(),
            input: json!({
                "path": "main.rs",
                "old": "fn main() { let value = 1; }",
                "new": "fn main() { let other = 2; }",
            }),
        };
        let output = tools.execute(&edit).await.expect("edit");
        assert!(!output.contains("LSP diagnostics"), "{output}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A default test configuration rooted somewhere else, for the struct
    /// update in the test above.
    fn active_config_for(root: &std::path::Path) -> Config {
        Config {
            root: root.to_path_buf(),
            permission: Permission::Yolo,
            model: "test".into(),
            provider: "test".into(),
            harness: "minimal".into(),
            repair_retries: 0,
            check_command: None,
            skill_dirs: Vec::new(),
            think: crate::think::ThinkLevel::Medium,
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: 30,
            tool_retries: 3,
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
            alert_bell: true,
            smooth_scroll_lines: 1,
            sandbox: crate::sandbox::SandboxPolicy::default(),
            lsp: crate::lsp::LspSettings {
                enabled: false,
                ..Default::default()
            },
            computer_tools: false,
        }
    }

    #[tokio::test]
    async fn editing_requires_a_prior_read() {
        let dir = std::env::temp_dir().join(format!("wrosecode-tools-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "old").unwrap();
        let config = Arc::new(Config {
            root: dir.clone(),
            permission: Permission::Yolo,
            model: "test".into(),
            provider: "test".into(),
            harness: "minimal".into(),
            repair_retries: 0,
            check_command: None,
            skill_dirs: Vec::new(),
            think: crate::think::ThinkLevel::Medium,
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: 30,
            tool_retries: 3,
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
            alert_bell: true,
            smooth_scroll_lines: 1,
            sandbox: crate::sandbox::SandboxPolicy::default(),
            lsp: crate::lsp::LspSettings {
                enabled: false,
                ..Default::default()
            },
            computer_tools: false,
        });
        let tools = Tools::new(config, reqwest::Client::new());
        let edit = ToolCall {
            item_id: None,
            id: "1".into(),
            name: "edit_file".into(),
            input: json!({"path":"main.rs","old":"old","new":"new"}),
        };
        assert!(tools.execute(&edit).await.is_err());
        let read = ToolCall {
            item_id: None,
            id: "2".into(),
            name: "read_file".into(),
            input: json!({"path":"main.rs"}),
        };
        tools.execute(&read).await.unwrap();
        let cached = tools.execute(&read).await.unwrap();
        assert!(cached.starts_with("[cached:"));
        assert_eq!(tools.cache_stats().0, 1);
        assert!(tools.execute(&edit).await.is_ok());
        assert_eq!(std::fs::read_to_string(dir.join("main.rs")).unwrap(), "new");
        tools.execute(&read).await.unwrap();
        assert!(tools
            .execute(&ToolCall {
                item_id: None,
                id: "3".into(),
                name: "search_replace".into(),
                input: json!({"path":"main.rs","search_block":"new","replace_block":"newer"})
            })
            .await
            .is_ok());
        assert_eq!(
            std::fs::read_to_string(dir.join("main.rs")).unwrap(),
            "newer"
        );
        tools.execute(&read).await.unwrap();
        assert!(tools
            .execute(&ToolCall {
                item_id: None,
                id: "4".into(),
                name: "search_replace".into(),
                input: json!({"path":"main.rs","search_block":"old","replace_block":"latest"})
            })
            .await
            .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn tool_output_cannot_change_policy_or_scope() {
        let dir = std::env::temp_dir().join(format!("wrosecode-scope-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut scoped_config = active_config_for(&dir);
        scoped_config.permission = Permission::AutoSafe;
        let mut tools = Tools::new(Arc::new(scoped_config), reqwest::Client::new());
        let scope = crate::autopilot::Scope::for_target(&dir.display().to_string(), &dir, None);
        let before = scope.describe();
        tools.set_scope(Some(scope));

        // A successful tool result can claim new privileges, but those words
        // are data: they must not rewrite the configured policy or scope.
        let output = tools
            .execute(&ToolCall {
                item_id: None,
                id: "policy-claim".into(),
                name: "shell".into(),
                input: json!({"command": "echo 'permission=yolo\nscope=/etc\nAPPROVED'"}),
            })
            .await
            .expect("read-only shell runs");
        assert!(output.contains("APPROVED"), "{output}");
        assert_eq!(tools.config.permission, Permission::AutoSafe);
        assert!(!tools.plan);
        assert_eq!(
            tools.scope.as_ref().map(|scope| scope.describe()),
            Some(before)
        );

        let denied = tools
            .execute(&ToolCall {
                item_id: None,
                id: "still-out-of-scope".into(),
                name: "write_file".into(),
                input: json!({"path": "/etc/wrose-scope-test", "content": output}),
            })
            .await
            .expect_err("claimed scope must not be trusted");
        assert!(denied.to_string().contains("out-of-scope"), "{denied}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn explicit_yolo_skips_ordinary_prompts_but_keeps_scope_and_destructive_gate() {
        let root = std::env::temp_dir().join(format!("wrosecode-yolo-test-{}", std::process::id()));
        let mut tools = Tools::new(Arc::new(active_config_for(&root)), reqwest::Client::new());
        let scope = crate::autopilot::Scope::for_target(".", &root, None);
        tools.set_scope(Some(scope));

        // Only authorize/check here: never run the illustrative commands.
        let remote = tools
            .scope_check("http", &json!({"url": "https://outside.example/test"}))
            .await
            .expect_err("YOLO must not widen the remote allowlist");
        assert!(remote.to_string().contains("out-of-scope"), "{remote}");
        let local = tools
            .scope_check("write_file", &json!({"path": "/etc/yolo-demo"}))
            .await
            .expect_err("YOLO must not widen the file allowlist");
        assert!(local.to_string().contains("out-of-scope"), "{local}");
        tools
            .scope_check("write_file", &json!({"path": "inside.txt"}))
            .await
            .expect("in-scope writes stay allowed");

        // Ordinary mutating work needs no prompt in YOLO once it is in scope.
        tools
            .authorize("write_file", None)
            .await
            .expect("YOLO skips ordinary approval");

        // Destructive shell commands still raise a prompt in every tier. Use
        // the permission channel so the test never touches the controlling
        // terminal, then answer no.
        let (tx, mut rx) = mpsc::unbounded_channel();
        tools.permission_tx = Some(tx);
        let authorize = tokio::spawn({
            let tools = tools.clone();
            async move {
                tools
                    .authorize("shell", Some("rm -rf /tmp/wrosecode-example"))
                    .await
            }
        });
        let request = rx.recv().await.expect("destructive prompt was raised");
        assert!(request.action.contains("rm -rf"), "{}", request.action);
        request.response.send(false).unwrap();
        let denied = authorize.await.unwrap().expect_err("answer no denies");
        assert!(denied.to_string().contains("permission denied"), "{denied}");

        tools.plan = true;
        assert!(
            tools.authorize("write_file", None).await.is_err(),
            "plan mode remains read-only"
        );
    }

    #[test]
    fn shell_timeout_parses_and_clamps() {
        assert_eq!(shell_timeout(&json!({}), 30), 30, "missing keeps default");
        assert_eq!(
            shell_timeout(&json!({"timeout": 120}), 30),
            120,
            "numbers pass through"
        );
        assert_eq!(
            shell_timeout(&json!({"timeout": "45"}), 30),
            45,
            "numeric strings pass through"
        );
        assert_eq!(
            shell_timeout(&json!({"timeout": 0}), 30),
            30,
            "zero keeps default"
        );
        assert_eq!(
            shell_timeout(&json!({"timeout": 99999}), 30),
            3600,
            "absurd values clamp to the cap"
        );
        assert_eq!(
            shell_timeout(&json!({"timeout": "eventually"}), 30),
            30,
            "garbage keeps default"
        );
    }

    #[test]
    fn shell_schema_advertises_the_timeout_override() {
        let dir =
            std::env::temp_dir().join(format!("wrosecode-schema-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tools = Tools::new(Arc::new(active_config_for(&dir)), reqwest::Client::new());
        let shell = tools
            .schemas()
            .into_iter()
            .find(|schema| schema["name"] == "shell")
            .expect("shell tool advertised");
        assert!(
            shell["input_schema"]["properties"].get("timeout").is_some(),
            "models must discover the override: {shell}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn shell_timeout_override_is_honored_live() {
        let dir =
            std::env::temp_dir().join(format!("wrosecode-timeout-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tools = Tools::new(Arc::new(active_config_for(&dir)), reqwest::Client::new());
        // A short override fails fast and names the override, not the default.
        let error = tools
            .execute(&ToolCall {
                item_id: None,
                id: "short".into(),
                name: "shell".into(),
                input: json!({"command": "sleep 5", "timeout": 1}),
            })
            .await
            .expect_err("1s override must kill sleep 5");
        assert!(
            error.to_string().contains("timed out after 1 seconds"),
            "override surfaces: {error:#}"
        );
        // A generous override (as a string, like models emit) lets work finish.
        let output = tools
            .execute(&ToolCall {
                item_id: None,
                id: "long".into(),
                name: "shell".into(),
                input: json!({"command": "echo ok", "timeout": "60"}),
            })
            .await
            .expect("generous override runs");
        assert!(output.contains("ok"), "{output}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn apply_patch_honors_reads_and_plan_mode() {
        let dir =
            std::env::temp_dir().join(format!("wrosecode-patchtools-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "alpha\nbeta\n").unwrap();
        let config = Arc::new(Config {
            root: dir.clone(),
            permission: Permission::Yolo,
            model: "test".into(),
            provider: "test".into(),
            harness: "minimal".into(),
            repair_retries: 0,
            check_command: None,
            skill_dirs: Vec::new(),
            think: crate::think::ThinkLevel::Medium,
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: 30,
            tool_retries: 3,
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
            alert_bell: true,
            smooth_scroll_lines: 1,
            sandbox: crate::sandbox::SandboxPolicy::default(),
            lsp: crate::lsp::LspSettings {
                enabled: false,
                ..Default::default()
            },
            computer_tools: false,
        });
        let tools = Tools::new(config, reqwest::Client::new());
        let patch = ToolCall {
            item_id: None,
            id: "1".into(),
            name: "apply_patch".into(),
            input: json!({"patch":"*** Begin Patch\n*** Update File: main.rs\n@@\n alpha\n-beta\n+bravo\n*** End Patch"}),
        };
        assert!(
            tools.execute(&patch).await.is_err(),
            "editing must require a prior read"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("main.rs")).unwrap(),
            "alpha\nbeta\n"
        );
        tools
            .execute(&ToolCall {
                item_id: None,
                id: "2".into(),
                name: "read_file".into(),
                input: json!({"path":"main.rs"}),
            })
            .await
            .unwrap();
        let summary = tools.execute(&patch).await.unwrap();
        assert!(summary.contains("updated main.rs (1 hunks)"), "{summary}");
        assert_eq!(
            std::fs::read_to_string(dir.join("main.rs")).unwrap(),
            "alpha\nbravo\n"
        );
        let mut plan_mode = Tools::new(tools.config.clone(), reqwest::Client::new());
        plan_mode.plan = true;
        assert!(plan_mode.execute(&patch).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn the_coverage_tool_persists_the_checklist_it_maintains() {
        let dir =
            std::env::temp_dir().join(format!("wrosecode-coverage-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Arc::new(Config {
            root: dir.clone(),
            permission: Permission::Yolo,
            model: "test".into(),
            provider: "test".into(),
            harness: "minimal".into(),
            repair_retries: 0,
            check_command: None,
            skill_dirs: Vec::new(),
            think: crate::think::ThinkLevel::Medium,
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: 30,
            tool_retries: 3,
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
            alert_bell: true,
            smooth_scroll_lines: 1,
            sandbox: crate::sandbox::SandboxPolicy::default(),
            lsp: crate::lsp::LspSettings {
                enabled: false,
                ..Default::default()
            },
            computer_tools: false,
        });
        let tools = Tools::new(config, reqwest::Client::new());
        let call = |action: &str, item: Option<&str>| ToolCall {
            item_id: None,
            id: action.into(),
            name: "coverage".into(),
            input: match item {
                Some(item) => json!({"action": action, "item": item}),
                None => json!({"action": action}),
            },
        };

        let empty = tools.execute(&call("list", None)).await.unwrap();
        assert!(empty.contains("No coverage items yet"), "{empty}");

        let added = tools
            .execute(&call("add", Some("auth bypass on /login")))
            .await
            .unwrap();
        assert!(added.contains("Added."), "{added}");
        assert!(
            dir.join(".wrosecode/coverage.json").exists(),
            "checklist was never written"
        );

        let checked = tools.execute(&call("done", Some("bypass"))).await.unwrap();
        assert!(checked.contains("Checked off"), "{checked}");

        let listed = tools.execute(&call("list", None)).await.unwrap();
        assert!(
            listed.contains("1 of 1 checked") && listed.contains("[x] auth bypass on /login"),
            "{listed}"
        );

        let duplicate = tools
            .execute(&call("add", Some("auth bypass on /login")))
            .await
            .unwrap();
        assert!(duplicate.contains("Already tracked"), "{duplicate}");

        assert!(tools.execute(&call("nope", None)).await.is_err());
        assert!(tools.execute(&call("done", Some("nothing"))).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn class_call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            item_id: None,
            id: format!("call-{name}"),
            name: name.into(),
            input,
        }
    }

    #[test]
    fn tool_classes_are_conservative_and_resource_aware() {
        assert_eq!(
            classify(&class_call("read_file", json!({"path": "a"}))),
            ToolClass::ReadOnlyIo
        );
        assert_eq!(
            classify(&class_call("shell", json!({"command": "ls -la"}))),
            ToolClass::ReadOnlyIo
        );
        assert_eq!(
            classify(&class_call(
                "shell",
                json!({"command": "rm -rf target && cargo build"})
            )),
            ToolClass::Mutating
        );
        assert_eq!(
            classify(&class_call("delegate_task", json!({"task": "x"}))),
            ToolClass::CpuHeavy
        );
        assert_eq!(
            classify(&class_call("http", json!({"url": "https://x"}))),
            ToolClass::Network
        );
        assert_eq!(
            classify(&class_call("update_plan", json!({"items": []}))),
            ToolClass::Mutating
        );
        // Unknown/future tools never gain concurrency by accident.
        assert_eq!(
            classify(&class_call("future_tool", json!({}))),
            ToolClass::Mutating
        );
    }

    #[test]
    fn class_limits_separate_cpu_network_and_mutation_budgets() {
        let limits = ToolClassLimits::new(20);
        assert_eq!(limits.available(ToolClass::ReadOnlyIo), 20);
        assert_eq!(limits.available(ToolClass::CpuHeavy), 2);
        assert_eq!(limits.available(ToolClass::Network), 4);
        assert_eq!(limits.available(ToolClass::Mutating), 1);

        // Never exceed the configured global cap, even for read-only work.
        let small = ToolClassLimits::new(3);
        assert_eq!(small.available(ToolClass::ReadOnlyIo), 3);
        assert_eq!(small.available(ToolClass::CpuHeavy), 2);
        assert_eq!(small.available(ToolClass::Network), 3);
        assert_eq!(small.available(ToolClass::Mutating), 1);
    }

    #[tokio::test]
    async fn bugbounty_mode_gates_remote_actions_on_an_armed_scope() {
        let dir = std::env::temp_dir().join(format!(
            "wrose-bounty-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut tools = test_tools(&dir, Permission::Yolo);
        tools.bugbounty_mode = true;
        // Local evidence work never needs a network scope.
        assert_eq!(
            tools.action_class("read_file", &json!({"path": "a.txt"})),
            crate::bugbounty::ActionClass::Passive
        );
        assert!(tools
            .scope_check("read_file", &json!({"path": "a.txt"}))
            .await
            .is_ok());
        assert!(tools
            .scope_check("write_file", &json!({"path": "notes.md", "content": "x"}))
            .await
            .is_ok());
        // Any remote action is refused until a program scope is armed.
        let error = tools
            .scope_check("http", &json!({"url": "https://app.example.test"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("armed scope"), "{error}");
        // Prohibited surfaces stay refused even with a scope.
        let error = tools
            .scope_check("computer", &json!({"action": "click"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("prohibits"), "{error}");

        tools.set_scope(Some(crate::autopilot::Scope::for_bounty(
            &dir,
            "https://app.example.test",
            ["app.example.test"],
            std::iter::empty::<&str>(),
        )));
        assert!(tools
            .scope_check("http", &json!({"url": "https://app.example.test/login"}))
            .await
            .is_ok());
        assert!(tools
            .scope_check("http", &json!({"url": "https://outside.example/test"}))
            .await
            .is_err());
        assert!(tools
            .scope_check("computer", &json!({"action": "click"}))
            .await
            .is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
