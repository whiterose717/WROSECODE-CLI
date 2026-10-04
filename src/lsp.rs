//! A minimal language-server client (opencode parity): after an edit, the
//! touched file is opened on its server and whatever diagnostics come back
//! are appended to the tool result, so the agent sees what the checker thinks
//! of its change without running the build itself.
//!
//! Design constraints:
//! - no dependencies beyond what we already ship (tokio + serde_json speak
//!   the JSON-RPC framing just fine),
//! - servers are spawned lazily, one per server command, and stay alive for
//!   the session so only the first edit of a language pays startup,
//! - every failure mode — missing binary, hung initialize, protocol garbage —
//!   degrades to "no diagnostics"; an edit must never break because a
//!   checker is unhappy.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// How long the first edit on a language waits for `initialize` before the
/// server is declared broken.
const INIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Diagnostics appended per file, and the cap on one message's length.
const MAX_DIAGNOSTICS: usize = 20;
const MAX_MESSAGE: usize = 300;

/// `[lsp]` from config.toml, resolved against the built-in defaults.
#[derive(Clone, Debug)]
pub struct LspSettings {
    /// Master switch; when off nothing is ever spawned.
    pub enabled: bool,
    /// How long an edit waits for the file's diagnostics.
    pub wait_ms: u64,
    /// Extension → server argv. Built-in defaults, overridden entry by entry.
    pub commands: BTreeMap<String, Vec<String>>,
}

impl Default for LspSettings {
    fn default() -> Self {
        let mut commands = BTreeMap::new();
        commands.insert("rs".to_string(), vec!["rust-analyzer".to_string()]);
        for ext in ["ts", "tsx", "js", "jsx"] {
            commands.insert(
                ext.to_string(),
                vec![
                    "typescript-language-server".to_string(),
                    "--stdio".to_string(),
                ],
            );
        }
        commands.insert("py".to_string(), vec!["pylsp".to_string()]);
        commands.insert("go".to_string(), vec!["gopls".to_string()]);
        for ext in ["c", "h", "cc", "cpp", "hpp", "cxx"] {
            commands.insert(ext.to_string(), vec!["clangd".to_string()]);
        }
        Self {
            enabled: true,
            wait_ms: 1_200,
            commands,
        }
    }
}

impl LspSettings {
    pub fn command_for(&self, extension: &str) -> Option<&[String]> {
        self.commands.get(extension).map(Vec::as_slice)
    }
}

/// One live language server plus everything we know about its documents.
struct Server {
    child: Child,
    stdin: ChildStdin,
    rx: UnboundedReceiver<Value>,
    init_id: i64,
    ready: bool,
    /// URI → version of the last didOpen/didClose cycle.
    open: HashMap<String, u32>,
    /// URI → last diagnostics the server published for it.
    diagnostics: HashMap<String, Vec<Value>>,
}

pub struct Lsp {
    root: PathBuf,
    settings: LspSettings,
    /// Keyed by the joined argv, so `ts`/`tsx`/`js` share one server.
    servers: HashMap<String, Server>,
    /// Servers that failed to start or initialize — never retried.
    dead: HashSet<String>,
}

impl Lsp {
    pub fn new(root: PathBuf, settings: LspSettings) -> Self {
        Self {
            root,
            settings,
            servers: HashMap::new(),
            dead: HashSet::new(),
        }
    }

    /// Fast warm-up: spawn the server for this file's language and write the
    /// `initialize` request, without waiting for the reply. Called when a
    /// file is read, which usually happens before the edit that needs the
    /// diagnostics — by then the reply is already in.
    pub async fn spawn_for(&mut self, path: &Path) {
        if !self.settings.enabled {
            return;
        }
        let Some(ext) = extension(path) else {
            return;
        };
        let Some(argv) = self.settings.command_for(&ext).map(<[String]>::to_vec) else {
            return;
        };
        let key = argv.join(" ");
        if self.servers.contains_key(&key) || self.dead.contains(&key) {
            return;
        }
        if let Err(error) = self.spawn(&key, &argv).await {
            // A missing binary is normal; remember it so every later edit
            // does not pay the spawn attempt again.
            eprintln!("lsp: {error:#}");
            self.dead.insert(key);
        }
    }

    /// The diagnostics for a file right after we wrote it, rendered for the
    /// tool result. `None` when no server runs for it, none answers in time,
    /// or everything it says is fine.
    pub async fn diagnostics_after_edit(&mut self, path: &Path) -> Option<String> {
        if !self.settings.enabled {
            return None;
        }
        let ext = extension(path)?;
        let argv = self.settings.command_for(&ext)?.to_vec();
        let key = argv.join(" ");
        if self.dead.contains(&key) {
            return None;
        }
        if !self.servers.contains_key(&key) {
            if let Err(error) = self.spawn(&key, &argv).await {
                eprintln!("lsp: {error:#}");
                self.dead.insert(key);
                return None;
            }
        }
        self.await_initialize(&key).await?;

        let uri = file_uri(path);
        let text = std::fs::read_to_string(path).ok()?;
        let server = self.servers.get_mut(&key)?;
        // Re-open on every edit: universally supported whatever sync kind
        // the server advertised, and we only ever show one version anyway.
        if let Some(version) = server.open.get(&uri).copied() {
            send(
                &mut server.stdin,
                &notification(
                    "textDocument/didClose",
                    json!({
                        "textDocument": {"uri": uri}
                    }),
                ),
            )
            .await
            .ok()?;
            server.open.insert(uri.clone(), version + 1);
        } else {
            server.open.insert(uri.clone(), 1);
        }
        send(
            &mut server.stdin,
            &notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id(&ext),
                        "version": server.open[&uri],
                        "text": text,
                    }
                }),
            ),
        )
        .await
        .ok()?;

        // Wait for this file's publishDiagnostics, draining anything else
        // (other files, log noise) into the per-URI store on the way.
        let wait = Duration::from_millis(self.settings.wait_ms.max(50));
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let server = self.servers.get_mut(&key)?;
            let message = match tokio::time::timeout(remaining, server.rx.recv()).await {
                Ok(Some(message)) => message,
                Ok(None) | Err(_) => break,
            };
            let hit = stash(server, &message);
            if hit {
                break;
            }
        }

        let server = self.servers.get(&key)?;
        let diagnostics = server.diagnostics.get(&uri)?;
        if diagnostics.is_empty() {
            return None;
        }
        Some(render(
            server_name(&argv),
            &path.display().to_string(),
            diagnostics,
        ))
    }

    async fn spawn(&mut self, key: &str, argv: &[String]) -> Result<()> {
        let (program, args) = argv
            .split_first()
            .context("empty language server command")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // A chatty server must not fill a pipe nobody drains.
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {program}"))?;
        let stdin = child.stdin.take().context("server stdin")?;
        let stdout: ChildStdout = child.stdout.take().context("server stdout")?;

        let (tx, rx) = unbounded_channel::<Value>();
        tokio::spawn(drain(stdout, tx));

        let mut server = Server {
            child,
            stdin,
            rx,
            init_id: 1,
            ready: false,
            open: HashMap::new(),
            diagnostics: HashMap::new(),
        };
        // Write initialize now, await the reply later (spawn_for wants this
        // call to stay cheap).
        let request = request(
            server.init_id,
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": file_uri(&self.root),
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": {"relatedInformation": false},
                        "synchronization": {"didSave": true}
                    }
                },
                "workspaceFolders": [
                    {"uri": file_uri(&self.root), "name": "root"}
                ],
            }),
        );
        send(&mut server.stdin, &request).await?;
        self.servers.insert(key.to_string(), server);
        Ok(())
    }

    /// Finish the handshake if it is still pending. Returns None (and kills
    /// the server) when it never completes.
    async fn await_initialize(&mut self, key: &str) -> Option<()> {
        let deadline = tokio::time::Instant::now() + INIT_TIMEOUT;
        loop {
            let server = self.servers.get_mut(key)?;
            if server.ready {
                return Some(());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.kill(key);
                return None;
            }
            let message = match tokio::time::timeout(remaining, server.rx.recv()).await {
                Ok(Some(message)) => message,
                Ok(None) | Err(_) => {
                    self.kill(key);
                    return None;
                }
            };
            let answered = message.get("id").and_then(Value::as_i64) == Some(server.init_id)
                && message.get("method").is_none();
            stash(server, &message);
            if answered {
                let server = self.servers.get_mut(key)?;
                let message = notification("initialized", json!({}));
                send(&mut server.stdin, &message).await.ok()?;
                server.ready = true;
                return Some(());
            }
        }
    }

    fn kill(&mut self, key: &str) {
        if let Some(mut server) = self.servers.remove(key) {
            let _ = server.child.start_kill();
        }
        self.dead.insert(key.to_string());
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        for (_, mut server) in self.servers.drain() {
            let _ = server.child.start_kill();
        }
    }
}

/// Park a publishDiagnostics notification in the right URI slot; returns
/// true when the message was one (so callers can stop waiting).
fn stash(server: &mut Server, message: &Value) -> bool {
    if message.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
        return false;
    }
    let Some(uri) = message["params"]["uri"].as_str().map(str::to_string) else {
        return false;
    };
    let diagnostics = message["params"]["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    server.diagnostics.insert(uri, diagnostics);
    true
}

/// Drain the server's stdout, decoding Content-Length frames until EOF.
async fn drain(stdout: ChildStdout, tx: UnboundedSender<Value>) {
    let mut reader = BufReader::new(stdout);
    while let Ok(Some(message)) = read_frame(&mut reader).await {
        if tx.send(message).is_err() {
            break;
        }
    }
}

/// Decode one JSON-RPC frame: `Content-Length: N\r\n\r\n` + N bytes.
async fn read_frame<R>(reader: &mut R) -> Result<Option<Value>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok();
            }
        }
    }
    let length = content_length.context("frame without Content-Length")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    let message = serde_json::from_slice(&body).context("frame is not JSON")?;
    Ok(Some(message))
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

async fn send<W: AsyncWriteExt + Unpin>(sink: &mut W, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    sink.write_all(header.as_bytes()).await?;
    sink.write_all(&body).await?;
    sink.flush().await.context("flush to language server")
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
}

fn language_id(ext: &str) -> String {
    match ext {
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        other => other,
    }
    .to_string()
}

/// `file://` URI with the handful of characters that must be escaped in one.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for ch in path.to_string_lossy().chars() {
        match ch {
            ' ' => uri.push_str("%20"),
            '%' => uri.push_str("%25"),
            '#' => uri.push_str("%23"),
            '?' => uri.push_str("%3F"),
            _ => uri.push(ch),
        }
    }
    uri
}

fn server_name(argv: &[String]) -> &str {
    argv.first()
        .map(|arg| arg.rsplit('/').next().unwrap_or(arg))
        .unwrap_or("language server")
}

/// The diagnostics as the agent reads them in the tool result.
fn render(server: &str, path: &str, diagnostics: &[Value]) -> String {
    let mut out = format!("LSP diagnostics ({server}) in {path}:");
    for diagnostic in diagnostics.iter().take(MAX_DIAGNOSTICS) {
        let severity = match diagnostic["severity"].as_u64().unwrap_or(1) {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        };
        let line = diagnostic["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
        let column = diagnostic["range"]["start"]["character"]
            .as_u64()
            .unwrap_or(0)
            + 1;
        let mut message = diagnostic["message"].as_str().unwrap_or("").to_string();
        if message.chars().count() > MAX_MESSAGE {
            message = message.chars().take(MAX_MESSAGE).collect();
            message.push('…');
        }
        out.push_str(&format!("\n  [{severity}] {line}:{column} {message}"));
    }
    if diagnostics.len() > MAX_DIAGNOSTICS {
        out.push_str(&format!(
            "\n  … {} more not shown",
            diagnostics.len() - MAX_DIAGNOSTICS
        ));
    }
    out
}

/// A fake server written against the same framing, so the whole
/// handshake → didOpen → publishDiagnostics path runs for real without
/// needing an actual language server installed. Shared with the tools
/// integration test, which drives it through the write path.
#[cfg(test)]
pub(crate) const FAKE_SERVER: &str = r#"
import json, sys

def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, _, value = line.partition(b":")
        if key.lower() == b"content-length":
            length = int(value.strip())
    if length is None:
        return None
    return json.loads(sys.stdin.buffer.read(length))

def send(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize" and "id" in message:
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        uri = message["params"]["textDocument"]["uri"]
        send({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {"start": {"line": 3, "character": 7},
                              "end": {"line": 3, "character": 8}},
                    "severity": 1,
                    "message": "fake error from the stub server",
                }],
            },
        })
"#;
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wrosecode-lsp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn encode(message: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(message).unwrap();
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend(body);
        frame
    }

    #[tokio::test]
    async fn frames_are_headers_then_exactly_that_many_body_bytes() {
        let first = encode(&json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        let second = encode(&json!({"jsonrpc": "2.0", "method": "x"}));
        let mut bytes = first.clone();
        bytes.extend(&second);
        let mut reader = BufReader::new(bytes.as_slice());

        let first_message = read_frame(&mut reader)
            .await
            .expect("first frame")
            .expect("first");
        assert_eq!(first_message["id"], 1);
        let second_message = read_frame(&mut reader)
            .await
            .expect("second frame")
            .expect("second");
        assert_eq!(second_message["method"], "x");
        assert!(
            read_frame(&mut reader)
                .await
                .is_ok_and(|message| message.is_none()),
            "clean eof"
        );
    }

    #[tokio::test]
    async fn a_frame_without_a_length_is_rejected_rather_than_guessed() {
        let bytes = b"Content-Type: application/vscode-jsonrpc\r\n\r\n{}";
        let mut reader = BufReader::new(bytes.as_slice());
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[test]
    fn uri_escaping_survives_the_characters_that_break_a_frame() {
        assert_eq!(
            file_uri(Path::new("/tmp/my dir/main.rs")),
            "file:///tmp/my%20dir/main.rs"
        );
        assert_eq!(file_uri(Path::new("/a%b")), "file:///a%25b");
        assert_eq!(file_uri(Path::new("/a#b?c")), "file:///a%23b%3Fc");
    }

    #[test]
    fn diagnostics_render_with_severity_and_one_based_positions() {
        let diagnostics = vec![
            json!({
                "severity": 1,
                "message": "cannot find value `x` in this scope",
                "range": {"start": {"line": 0, "character": 0}}
            }),
            json!({
                "severity": 2,
                "message": "unused variable",
                "range": {"start": {"line": 11, "character": 4}}
            }),
            json!({
                "message": "no severity means error",
                "range": {"start": {"line": 2, "character": 9}}
            }),
        ];
        let text = render("rust-analyzer", "src/main.rs", &diagnostics);
        assert!(
            text.starts_with("LSP diagnostics (rust-analyzer) in src/main.rs:"),
            "{text}"
        );
        assert!(
            text.contains("[error] 1:1 cannot find value `x` in this scope"),
            "{text}"
        );
        assert!(text.contains("[warning] 12:5 unused variable"), "{text}");
        assert!(
            text.contains("[error] 3:10 no severity means error"),
            "{text}"
        );
    }

    #[test]
    fn long_messages_and_ensembles_are_capped() {
        let diagnostics: Vec<Value> = (0..MAX_DIAGNOSTICS + 3)
            .map(|index| {
                json!({
                    "severity": 1,
                    "message": if index == 0 { "e".repeat(MAX_MESSAGE + 40) } else { format!("m{index}") },
                    "range": {"start": {"line": 0, "character": 0}}
                })
            })
            .collect();
        let text = render("pylsp", "a.py", &diagnostics);
        assert!(text.contains("… 3 more not shown"), "{text}");
        let first = text.lines().nth(1).expect("first diagnostic");
        assert!(
            first.len() < MAX_MESSAGE + 30,
            "message should be truncated: {first}"
        );
    }

    #[tokio::test]
    async fn a_disabled_client_stays_silent() {
        let root = dir("disabled");
        let file = root.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let settings = LspSettings {
            enabled: false,
            ..Default::default()
        };
        let mut lsp = Lsp::new(root.clone(), settings);
        assert!(lsp.diagnostics_after_edit(&file).await.is_none());
        lsp.spawn_for(&file).await;
        assert!(lsp.servers.is_empty(), "disabled means never spawned");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn a_missing_binary_fails_once_and_is_never_retried() {
        let root = dir("missing");
        let file = root.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let mut settings = LspSettings::default();
        settings.commands.insert(
            "rs".to_string(),
            vec!["wrosecode-no-such-language-server".to_string()],
        );
        let mut lsp = Lsp::new(root.clone(), settings);
        assert!(lsp.diagnostics_after_edit(&file).await.is_none());
        assert!(lsp.diagnostics_after_edit(&file).await.is_none());
        assert!(lsp.servers.is_empty(), "a dead server is not retried");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn a_real_handshake_returns_the_servers_diagnostic() {
        let root = dir("handshake");
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let script = root.join("fake_server.py");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        let file = root.join("main.rs");
        std::fs::write(&file, "fn main() {\n    let value = 1;\n    value;\n}\n").unwrap();

        let mut settings = LspSettings {
            wait_ms: 4_000,
            ..Default::default()
        };
        settings.commands.insert(
            "rs".to_string(),
            vec!["python3".to_string(), script.display().to_string()],
        );
        let mut lsp = Lsp::new(root.clone(), settings);

        // The read-side warm start brings the server up before the edit.
        lsp.spawn_for(&file).await;
        let rendered = lsp
            .diagnostics_after_edit(&file)
            .await
            .expect("the fake server publishes one error");
        assert!(
            rendered.contains("[error] 4:8 fake error from the stub server"),
            "{rendered}"
        );
        assert!(rendered.contains("main.rs"), "{rendered}");

        // The second edit reuses the same server and its diagnostic again.
        let again = lsp
            .diagnostics_after_edit(&file)
            .await
            .expect("same server, same diagnostic");
        assert!(again.contains("fake error from the stub server"), "{again}");
        assert!(lsp.servers.len() == 1, "one server per command");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
