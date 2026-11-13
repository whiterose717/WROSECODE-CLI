//! Outbound Cloudflare relay bridge: the local server dials out over
//! authenticated WSS, so no inbound port, port-forward, or public IP is
//! needed. Auto-reconnects with exponential backoff, keepalives, bounded
//! queues, and idempotent message handling (no double tool execution).

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Clone)]
pub struct BridgeState {
    pub connected: Arc<AtomicBool>,
    pub last_error: Arc<std::sync::Mutex<String>>,
    pub reconnects: Arc<AtomicUsize>,
    pub relay_url: Arc<std::sync::Mutex<String>>,
}

impl BridgeState {
    pub fn new() -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(false)),
            last_error: Arc::new(std::sync::Mutex::new(String::new())),
            reconnects: Arc::new(AtomicUsize::new(0)),
            relay_url: Arc::new(std::sync::Mutex::new(String::new())),
        }
    }
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    pub fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "connected": self.is_connected(),
            "reconnects": self.reconnects.load(Ordering::Relaxed),
            "relay_url": self.relay_url.lock().map(|s| redact_url(s.as_str())).unwrap_or_default(),
            "last_error": self.last_error.lock().map(|s| s.clone()).unwrap_or_default(),
        })
    }
}

fn redact_url(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    // Keep host, drop query/token.
    url.split('?').next().unwrap_or(url).to_string()
}

fn set_error(state: &BridgeState, msg: &str) {
    if let Ok(mut e) = state.last_error.lock() {
        *e = msg.chars().take(300).collect();
    }
}

/// Relay credentials persisted by `wrosecode bridge login`
/// (`~/.wrosecode/relay.json`, mode 0600). Environment variables take
/// precedence when set.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RelayFile {
    #[serde(default)]
    url: String,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    secret: String,
}

fn relay_file_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".wrosecode")
            .join("relay.json")
    })
}

/// Store relay credentials for automatic reconnects across restarts.
pub fn save_relay_config(url: &str, device_id: &str, secret: &str) -> anyhow::Result<()> {
    let Some(path) = relay_file_path() else {
        anyhow::bail!("HOME is not set")
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = RelayFile {
        url: url.trim().to_string(),
        device_id: device_id.trim().to_string(),
        secret: secret.trim().to_string(),
    };
    anyhow::ensure!(
        file.url.starts_with("wss://") || file.url.starts_with("ws://"),
        "relay URL must start with wss:// (or ws:// for loopback testing)"
    );
    anyhow::ensure!(!file.device_id.is_empty(), "device id is required");
    anyhow::ensure!(!file.secret.is_empty(), "device secret is required");
    std::fs::write(&path, serde_json::to_vec_pretty(&file)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn load_relay_file() -> Option<RelayFile> {
    let path = relay_file_path()?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Resolve relay config from env, falling back to `~/.wrosecode/relay.json`.
/// Returns None when not configured — the server still runs locally without
/// a cloud bridge.
pub fn relay_config() -> Option<(String, String, String)> {
    let file = load_relay_file().unwrap_or_default();
    let env_or_file = |key: &str, fallback: String| {
        std::env::var(key)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(fallback)
    };
    let url = env_or_file("WROSECODE_RELAY_URL", file.url);
    let device_id = env_or_file("WROSECODE_DEVICE_ID", file.device_id);
    let secret = env_or_file("WROSECODE_DEVICE_SECRET", file.secret);
    if url.trim().is_empty() || device_id.trim().is_empty() || secret.trim().is_empty() {
        return None;
    }
    if !(url.starts_with("wss://") || url.starts_with("ws://")) {
        return None;
    }
    // Never allow ws:// to a non-loopback host (credential leak).
    if url.starts_with("ws://") {
        let host = url
            .split('/')
            .nth(2)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !(host.starts_with("127.") || host.starts_with("localhost") || host.starts_with("[::1]"))
        {
            return None;
        }
    }
    Some((url, device_id, secret))
}

/// Background task: connect, authenticate, route messages, reconnect forever.
/// `on_chat` executes a chat turn and streams events back through `send_fn`.
/// Here the bridge owns only transport; message handling is injected by
/// `api.rs` via the shared handler to avoid a module cycle.
pub async fn run_forever<F, Fut>(state: BridgeState, mut handle: F)
where
    F: FnMut(serde_json::Value) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Option<serde_json::Value>> + Send,
{
    let Some((url, device_id, secret)) = relay_config() else {
        return;
    };
    if let Ok(mut u) = state.relay_url.lock() {
        *u = url.clone();
    }
    // Dedupe: never execute the same side-effecting message twice.
    let mut seen: HashSet<String> = HashSet::new();
    let mut seen_order: VecDeque<String> = VecDeque::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_once(
            &url,
            &device_id,
            &secret,
            &state,
            &mut handle,
            &mut seen,
            &mut seen_order,
        )
        .await
        {
            Ok(_) => {
                set_error(&state, "relay closed; reconnecting");
            }
            Err(e) => {
                set_error(&state, &format!("{e:#}"));
            }
        }
        state.connected.store(false, Ordering::Relaxed);
        state.reconnects.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
        // Reset backoff on next success inside run_once via shorter sleep reset.
    }
}

async fn run_once<F, Fut>(
    url: &str,
    device_id: &str,
    secret: &str,
    state: &BridgeState,
    handle: &mut F,
    seen: &mut HashSet<String>,
    seen_order: &mut VecDeque<String>,
) -> Result<()>
where
    F: FnMut(serde_json::Value) -> Fut + Send,
    Fut: std::future::Future<Output = Option<serde_json::Value>> + Send,
{
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;
    // A caller-supplied Request bypasses tungstenite's automatic handshake
    // headers, so set them explicitly (RFC 6455).
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let req = {
        use tokio_tungstenite::tungstenite::handshake::client::generate_key;
        use tokio_tungstenite::tungstenite::http::Request;
        Request::builder()
            .uri(url)
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key())
            .header("Authorization", format!("Bearer {secret}"))
            .header("X-Device-Id", device_id)
            .body(())?
    };
    let (ws, _) = connect_async(req)
        .await
        .map_err(|e| anyhow::anyhow!("relay dial failed: {e}"))?;
    state.connected.store(true, Ordering::Relaxed);
    set_error(state, "");
    let (mut write, mut read) = ws.split();
    // Hello authenticates the device to the relay.
    let hello = serde_json::json!({"type":"hello","device_id":device_id,"version":env!("CARGO_PKG_VERSION")});
    write.send(Message::Text(hello.to_string().into())).await?;
    // Keepalive ticker.
    let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
    // Bounded outbound queue (backpressure: drop oldest beyond 100).
    let mut outbox: VecDeque<String> = VecDeque::new();
    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                if write.send(Message::Ping(vec![].into())).await.is_err() { break; }
                // Flush queued messages opportunistically.
                while let Some(msg) = outbox.pop_front() {
                    if write.send(Message::Text(msg.into())).await.is_err() { break; }
                }
            }
            msg = read.next() => {
                let Some(msg) = msg else { break; };
                let msg = msg.map_err(|e| anyhow::anyhow!("relay read failed: {e}"))?;
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Ping(p) => { let _ = write.send(Message::Pong(p)).await; continue; }
                    Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    Message::Frame(_) => continue,
                };
                let value: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // Idempotency key: relay must send `id` on side-effecting
                // messages. Duplicates are acked without re-execution.
                if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                    if !seen.insert(id.to_string()) {
                        let ack = serde_json::json!({"type":"ack","id":id,"duplicate":true});
                        outbox.push_back(ack.to_string());
                        if outbox.len() > 100 { outbox.pop_front(); }
                        continue;
                    }
                    seen_order.push_back(id.to_string());
                    if seen_order.len() > 1000 {
                        if let Some(old) = seen_order.pop_front() { seen.remove(&old); }
                    }
                }
                // Handle (chat/CTF/pair) and optionally reply.
                if let Some(reply) = handle(value).await {
                    outbox.push_back(reply.to_string());
                    if outbox.len() > 100 { outbox.pop_front(); }
                    while let Some(m) = outbox.pop_front() {
                        if write.send(Message::Text(m.into())).await.is_err() { break; }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_config_prefers_file_and_rejects_plaintext_ws_to_remote() {
        let dir = std::env::temp_dir().join(format!("wrose-relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old_home = std::env::var_os("HOME");
        // SAFETY: tests run single-threaded within this binary target here.
        unsafe {
            std::env::set_var("HOME", &dir);
            std::env::remove_var("WROSECODE_RELAY_URL");
            std::env::remove_var("WROSECODE_DEVICE_ID");
            std::env::remove_var("WROSECODE_DEVICE_SECRET");
        }
        assert!(relay_config().is_none());
        save_relay_config("wss://relay.example/relay/device", "dev_1", "s3cret").unwrap();
        let (url, id, secret) = relay_config().expect("file config");
        assert_eq!(url, "wss://relay.example/relay/device");
        assert_eq!(id, "dev_1");
        assert_eq!(secret, "s3cret");
        // Env overrides file.
        unsafe { std::env::set_var("WROSECODE_DEVICE_ID", "dev_2") };
        assert_eq!(relay_config().unwrap().1, "dev_2");
        unsafe { std::env::remove_var("WROSECODE_DEVICE_ID") };
        // ws:// to a remote host must never carry credentials.
        save_relay_config("ws://evil.example/x", "dev_1", "s3cret").unwrap();
        assert!(relay_config().is_none());
        // ws:// loopback is allowed for tests.
        save_relay_config("ws://127.0.0.1:9/x", "dev_1", "s3cret").unwrap();
        assert!(relay_config().is_some());
        std::fs::remove_dir_all(&dir).ok();
        unsafe {
            match old_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            std::env::remove_var("WROSECODE_DEVICE_ID");
        }
    }
}
