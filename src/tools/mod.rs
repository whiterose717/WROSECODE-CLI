pub mod browser;
pub mod burp;
pub mod cache;
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
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

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
    pub mcps: Vec<(String, Arc<mcp::Mcp>)>,
    pub permission_tx: Option<mpsc::UnboundedSender<PermissionRequest>>,
    result_cache: Arc<Mutex<HashMap<String, String>>>,
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
}

impl Tools {
    pub fn new(config: Arc<Config>, client: reqwest::Client) -> Self {
        let redis_addr = config.redis_url.as_deref().and_then(cache::redis_address);
        let sandbox = Arc::new(crate::sandbox::Sandbox::new(
            config.sandbox.clone(),
            config.root.clone(),
        ));
        Self {
            config,
            client,
            prompt_lock: Arc::new(Mutex::new(())),
            read_files: Arc::new(Mutex::new(HashSet::new())),
            created_files: Arc::new(Mutex::new(HashSet::new())),
            referenced_files: Arc::new(Mutex::new(HashSet::new())),
            plan: false,
            mcps: Vec::new(),
            permission_tx: None,
            result_cache: Arc::new(Mutex::new(HashMap::new())),
            cache_hits: Arc::new(AtomicUsize::new(0)),
            cache_saved_bytes: Arc::new(AtomicUsize::new(0)),
            checklist: Arc::new(Mutex::new(Vec::new())),
            redis_addr,
            sandbox,
            scope: None,
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
                "Run a shell command in the project",
                json!({"command":"string"}),
            ),
            schema("read_file", "Read a UTF-8 file", json!({"path":"string"})),
            schema(
                "write_file",
                "Create or replace a UTF-8 file",
                json!({"path":"string","content":"string"}),
            ),
            schema(
                "edit_file",
                "Replace text matching exactly once",
                json!({"path":"string","old":"string","new":"string"}),
            ),
            schema(
                "apply_patch",
                "Apply a multi-file patch document (*** Begin Patch ... *** End Patch) with @@ hunks, adds, deletes, and moves",
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
        ];
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
                    .run(command, self.config.shell_timeout_seconds)
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
            "edit_file" => {
                self.authorize(&call.name, None).await?;
                let path = arg(input, "path")?;
                self.require_read(path)?;
                let result = fs::edit(
                    &self.config.root,
                    path,
                    arg(input, "old")?,
                    arg(input, "new")?,
                )
                .await?;
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
            "web_fetch" => http::request(&self.client, "GET", arg(input, "url")?, None).await?,
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
                http::request(
                    &self.client,
                    method,
                    arg(input, "url")?,
                    input["body"].as_str(),
                )
                .await?
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
            "write_file" | "edit_file" | "apply_patch" | "shell"
        ) {
            self.result_cache
                .lock()
                .map_err(|_| anyhow::anyhow!("result cache lock poisoned"))?
                .clear();
        }
        Ok(output)
    }

    pub fn cache_stats(&self) -> (usize, usize) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_saved_bytes.load(Ordering::Relaxed),
        )
    }

    pub fn retryable(&self, call: &ToolCall) -> bool {
        matches!(
            call.name.as_str(),
            "read_file" | "grep" | "glob" | "web_search" | "web_fetch"
        ) || (call.name == "shell" && call.input["command"].as_str().is_some_and(shell::read_only))
    }

    fn cache_key(&self, call: &ToolCall) -> Option<String> {
        let cacheable = matches!(call.name.as_str(), "read_file" | "grep" | "glob")
            || (call.name == "shell"
                && call.input["command"].as_str().is_some_and(shell::read_only));
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

    /// Reject (or ask about) any call that leaves the CTF autopilot
    /// allowlist. Headless runs have no permission channel, so an
    /// out-of-scope action is denied with the reason instead of hanging on a
    /// prompt nobody will answer.
    async fn scope_check(&self, name: &str, input: &Value) -> Result<()> {
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
                | "apply_patch"
                | "http"
                | "mcp"
                | "browser_capture"
                | "burp_export"
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

fn schema(name: &str, description: &str, fields: Value) -> Value {
    let props = fields
        .as_object()
        .expect("schema fields")
        .iter()
        .map(|(k, _)| (k.clone(), json!({"type":"string"})))
        .collect::<serde_json::Map<_, _>>();
    let required = props.keys().cloned().collect::<Vec<_>>();
    json!({"name":name,"description":description,"input_schema":{"type":"object","properties":props,"required":required}})
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
        });
        let tools = Tools::new(config, reqwest::Client::new());
        let edit = ToolCall {
            id: "1".into(),
            name: "edit_file".into(),
            input: json!({"path":"main.rs","old":"old","new":"new"}),
        };
        assert!(tools.execute(&edit).await.is_err());
        let read = ToolCall {
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
        assert!(tools
            .execute(&ToolCall {
                id: "3".into(),
                name: "edit_file".into(),
                input: json!({"path":"main.rs","old":"new","new":"newer"})
            })
            .await
            .is_err());
        std::fs::remove_dir_all(dir).unwrap();
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
        });
        let tools = Tools::new(config, reqwest::Client::new());
        let patch = ToolCall {
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
}
