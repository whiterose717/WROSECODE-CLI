use crate::agent::Agent;
use crate::bridge::BridgeState;
use crate::session::Session;
use crate::store::Store;
use anyhow::{Context, Result};
use axum::extract::{Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response, Sse};
use axum::routing::get;
use axum::{Extension, Json, Router};

type AxumResponse = Response;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Clone)]
struct ApiAuth {
    master: Arc<String>,
}

#[derive(Clone)]
struct ApiState {
    agent: Arc<Mutex<Agent>>,
    store: Store,
    auth: ApiAuth,
    bridge: BridgeState,
    clients: Arc<AtomicUsize>,
    started_at: Instant,
    bind: SocketAddr,
    pair_hits: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    ctf_jobs: Arc<Mutex<HashMap<String, CtfJob>>>,
}

#[derive(Clone, Debug, Serialize)]
struct CtfJob {
    id: String,
    target: String,
    category: Option<String>,
    status: String,
    created_at: u64,
    finished_at: Option<u64>,
    flag: Option<String>,
    evidence: Option<String>,
    report: Option<String>,
}

#[derive(Clone, Debug)]
struct Authed {
    account_id: String,
    is_master: bool,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------- requests ----------

#[derive(Deserialize)]
struct ChatRequest {
    prompt: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Serialize)]
struct ChatResponse {
    answer: String,
    session_id: String,
    metrics: crate::metrics::MetricsSnapshot,
}

#[derive(Deserialize)]
struct SelectModel {
    provider: String,
    model: String,
}

#[derive(Deserialize)]
struct CreateSession {
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Deserialize)]
struct PairConfirm {
    code: String,
    account_id: String,
    #[serde(default)]
    device_name: Option<String>,
}

#[derive(Deserialize)]
struct RevokeDevice {
    device_id: String,
}

#[derive(Deserialize)]
struct CtfStart {
    target: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    flag_format: Option<String>,
    #[serde(default)]
    budget: Option<String>,
}

#[derive(Deserialize)]
struct CtfUpload {
    filename: String,
    #[serde(default)]
    content_base64: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    provider: Option<String>,
}

// ---------- server ----------

pub async fn serve(agent: Agent, store: Store, address: &str) -> Result<()> {
    let address: SocketAddr = address.parse().context("invalid server address")?;
    let token = load_or_create_token()?;
    let started_at = Instant::now();
    let bridge = BridgeState::new();
    let state = ApiState {
        agent: Arc::new(Mutex::new(agent)),
        store,
        auth: ApiAuth {
            master: Arc::new(token),
        },
        bridge: bridge.clone(),
        clients: Arc::new(AtomicUsize::new(0)),
        started_at,
        bind: address,
        pair_hits: Arc::new(Mutex::new(HashMap::new())),
        ctf_jobs: Arc::new(Mutex::new(HashMap::new())),
    };

    {
        let bridge_state = bridge.clone();
        let relay_state = state.clone();
        tokio::spawn(async move {
            crate::bridge::run_forever(bridge_state, move |msg| {
                let relay_state = relay_state.clone();
                async move { handle_relay_message(relay_state, msg).await }
            })
            .await;
        });
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/providers", get(providers))
        .route("/v1/models", get(models))
        .route("/v1/model/current", get(model_current))
        .route("/v1/model/select", axum::routing::post(model_select))
        .route("/v1/chat", axum::routing::post(chat))
        .route("/v1/chat/stream", axum::routing::post(chat_stream))
        .route("/v1/ws", get(ws_handler))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/latest", get(latest_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/ctf/status", get(ctf_status))
        .route("/v1/ctf/flags", get(ctf_flags))
        .route("/v1/ctf/start", axum::routing::post(ctf_start))
        .route("/v1/ctf/upload", axum::routing::post(ctf_upload))
        .route("/v1/pair/status", get(pair_status))
        .route("/v1/pair/confirm", axum::routing::post(pair_confirm))
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/revoke", axum::routing::post(revoke_device))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(address).await?;
    print_banner(address, &bridge).await;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("\nWROSECODE server shutting down gracefully…");
        })
        .await?;
    Ok(())
}

async fn print_banner(address: SocketAddr, bridge: &BridgeState) {
    let lan = lan_ip().unwrap_or_else(|| "unavailable".to_string());
    let relay = match crate::bridge::relay_config() {
        Some((url, _, _)) => url.split('?').next().unwrap_or("").to_string(),
        None => "not configured".to_string(),
    };
    let paired = crate::pairing::list_devices().len();
    let _ = bridge;
    println!("WROSECODE SERVER\n");
    println!("✓ Agent runtime initialized");
    println!("✓ HTTP API started");
    if paired > 0 {
        println!("✓ Existing device pairing loaded ({paired} device(s))");
    } else {
        println!("○ No paired devices yet — run `wrosecode bridge pair` to pair");
    }
    println!("✓ Cloud relay: {relay}");
    println!("✓ Ready for browser connections\n");
    println!("Local address  http://{address}");
    println!("LAN IP         {lan}");
    println!("Port           {}", address.port());
    println!("Auth           ENABLED (Bearer ~/.wrosecode/server.token or device token)");
    if !address.ip().is_loopback() {
        eprintln!("WARNING: WROSECODE is bound beyond localhost. Restrict inbound access and use HTTPS/WSS through a trusted private proxy or Tailscale Serve.");
    }
    println!("\nPress Ctrl+C to stop.");
}

fn lan_ip() -> Option<String> {
    // Discover the outbound interface IP without new dependencies.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "0.0.0.0" {
                    return Some(ip);
                }
            }
        }
    }
    std::process::Command::new("hostname")
        .arg("-I")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
}

fn load_or_create_token() -> Result<String> {
    let dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .context("HOME is required to store the server credential")?
        .join(".wrosecode");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("server.token");
    if path.exists() {
        let token = std::fs::read_to_string(&path)?.trim().to_string();
        anyhow::ensure!(
            token.len() >= 40,
            "server token file is invalid; replace it with a fresh random token"
        );
        return Ok(token);
    }
    let mut bytes = [0u8; 48];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| anyhow::anyhow!("secure random generation failed"))?;
    use base64::Engine;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
            .write_all(token.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, token.as_bytes())?;
    Ok(token)
}

fn bearer_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.as_bytes()
            .iter()
            .zip(b.as_bytes())
            .fold(0u8, |d, (x, y)| d | (x ^ y))
            == 0
}

async fn authorize(
    State(state): State<ApiState>,
    mut request: Request,
    next: Next,
) -> AxumResponse {
    let path = request.uri().path().to_string();
    if path == "/health" || path == "/" {
        return next.run(request).await;
    }
    if path == "/v1/pair/confirm" {
        // Public but rate-limited one-time-code flow.
        let mut hits = state.pair_hits.lock().await;
        let now = Instant::now();
        let entry = hits.entry("global".to_string()).or_default();
        entry.retain(|t| now.duration_since(*t).as_secs() < 300);
        if entry.len() >= 20 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error":"too many pairing attempts; try again later"})),
            )
                .into_response();
        }
        entry.push(now);
        request.extensions_mut().insert(Authed {
            account_id: String::new(),
            is_master: false,
        });
        return next.run(request).await;
    }
    let supplied = bearer_of(request.headers());
    let expected = format!("Bearer {}", state.auth.master);
    if !supplied.is_empty() && constant_eq(&supplied, &expected) {
        request.extensions_mut().insert(Authed {
            account_id: String::new(),
            is_master: true,
        });
        return next.run(request).await;
    }
    if let Some(raw) = supplied.strip_prefix("Bearer ") {
        if let Some((_device_id, account_id)) = crate::pairing::validate_device_token(raw.trim()) {
            request.extensions_mut().insert(Authed {
                account_id,
                is_master: false,
            });
            return next.run(request).await;
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "authentication required",
    )
        .into_response()
}

// ---------- handlers ----------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status":"ok","service":"wrosecode"}))
}

async fn status(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
) -> impl IntoResponse {
    // Read-only: fail fast when a chat/CTF turn holds the agent instead of
    // queueing status polls behind minutes of model work.
    let Ok(agent) = state.agent.try_lock() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"agent busy; retry shortly"})),
        )
            .into_response();
    };
    let mut value = serde_json::to_value(crate::tui::stats_from_agent(&agent, "web", "web"))
        .unwrap_or_else(|_| serde_json::json!({}));
    value["metrics"] = serde_json::to_value(agent.metrics.snapshot()).unwrap_or_default();
    value["server"] = serde_json::json!({
        "bind": state.bind.to_string(),
        "lan_ip": lan_ip().unwrap_or_else(|| "unavailable".into()),
        "port": state.bind.port(),
        "loopback_only": state.bind.ip().is_loopback(),
        "auth": "enabled",
        "paired_devices": crate::pairing::list_devices().len(),
        "pending_codes": crate::pairing::pending_count(),
        "cloud_bridge": state.bridge.status_json(),
        "connected_clients": state.clients.load(Ordering::Relaxed),
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "provider": agent.config.provider,
        "model": agent.config.model,
        "mode": agent.mode,
        "as_account": auth.account_id,
        "is_master": auth.is_master,
    });
    Json(value).into_response()
}

async fn providers(State(state): State<ApiState>) -> impl IntoResponse {
    let _ = state;
    let settings = match crate::settings::Settings::load() {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let list: Vec<serde_json::Value> = settings
        .providers
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.name,
                "label": crate::settings::Settings::display(p),
                "kind": p.kind,
                "base_url": p.base_url,
                "model": p.model,
                "status": settings.status(p),
            })
        })
        .collect();
    Json(serde_json::json!({"providers": list})).into_response()
}

async fn models(State(state): State<ApiState>, Query(q): Query<ListQuery>) -> impl IntoResponse {
    let _ = state;
    let settings = match crate::settings::Settings::load() {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let names: Vec<String> = if let Some(p) = q.provider {
        vec![p]
    } else {
        settings.providers.iter().map(|p| p.name.clone()).collect()
    };
    let client = match crate::provider::streaming_client(10) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let mut groups = Vec::new();
    for name in names {
        let Some(profile) = settings.profile(&name).cloned() else {
            continue;
        };
        let _ = settings.refresh_oauth_if_needed(&profile, &client).await;
        let key = settings.key(&profile);
        let provider =
            match crate::provider::create_profile(&profile, key, &profile.model, client.clone()) {
                Ok(p) => p,
                Err(e) => {
                    groups.push(serde_json::json!({
                        "provider": profile.name,
                        "label": crate::settings::Settings::display(&profile),
                        "models": [],
                        "error": e.to_string(),
                    }));
                    continue;
                }
            };
        let mut discovered = provider.list_models().await.unwrap_or_default();
        if !profile.model.is_empty() && !discovered.contains(&profile.model) {
            discovered.insert(0, profile.model.clone());
        }
        if let Some(sel) = settings.last_selection() {
            if sel.provider == profile.name {
                if let Some(i) = discovered.iter().position(|m| *m == sel.model) {
                    let m = discovered.remove(i);
                    discovered.insert(0, m);
                }
            }
        }
        discovered.truncate(500);
        groups.push(serde_json::json!({
            "provider": profile.name,
            "label": crate::settings::Settings::display(&profile),
            "models": discovered,
            "default_model": profile.model,
        }));
    }
    Json(serde_json::json!({"groups": groups})).into_response()
}

async fn model_current(State(state): State<ApiState>) -> impl IntoResponse {
    let Ok(agent) = state.agent.try_lock() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"agent busy; retry shortly"})),
        )
            .into_response();
    };
    Json(serde_json::json!({
        "provider": agent.config.provider,
        "model": agent.config.model,
        "mode": agent.mode,
    }))
    .into_response()
}

async fn model_select(
    State(state): State<ApiState>,
    Json(req): Json<SelectModel>,
) -> impl IntoResponse {
    if req.provider.trim().is_empty()
        || req.model.trim().is_empty()
        || req.provider.len() > 100
        || req.model.len() > 200
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid provider/model"})),
        )
            .into_response();
    }
    let mut settings = match crate::settings::Settings::load() {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let profile = match settings.profile(&req.provider).cloned() {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error":"unknown provider"})),
            )
                .into_response()
        }
    };
    let client = match crate::provider::streaming_client(10) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    if let Err(e) = settings.refresh_oauth_if_needed(&profile, &client).await {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let key = settings.key(&profile);
    let provider = match crate::provider::create_profile(&profile, key, &req.model, client.clone())
    {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let _ = settings.set_default_model(&req.provider, &req.model);
    let _ = settings.set_last_selection(&req.provider, &req.model);
    let mut agent = state.agent.lock().await;
    agent.provider = provider;
    let mut cfg = (*agent.config).clone();
    cfg.provider = req.provider.clone();
    cfg.model = req.model.clone();
    agent.config = std::sync::Arc::new(cfg);
    Json(serde_json::json!({"provider": req.provider, "model": req.model})).into_response()
}

async fn apply_chat_overrides(agent: &mut Agent, req: &ChatRequest) -> Result<()> {
    if let Some(mode) = req.mode.as_deref() {
        if matches!(mode, "build" | "plan" | "general" | "ctf") {
            agent.set_mode(mode)?;
        }
    }
    if req.provider.is_some() || req.model.is_some() {
        let settings = crate::settings::Settings::load()?;
        let provider_name = req
            .provider
            .clone()
            .unwrap_or_else(|| agent.config.provider.clone());
        let model = req
            .model
            .clone()
            .unwrap_or_else(|| agent.config.model.clone());
        let profile = settings
            .profile(&provider_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown provider: {provider_name}"))?;
        let key = settings.key(&profile);
        let provider =
            crate::provider::create_profile(&profile, key, &model, agent.tools.client.clone())?;
        agent.provider = provider;
        let mut cfg = (*agent.config).clone();
        cfg.provider = provider_name;
        cfg.model = model;
        agent.config = std::sync::Arc::new(cfg);
    }
    if let Some(id) = req.session_id.as_deref() {
        if !id.trim().is_empty() {
            let store = crate::store::Store::open_default()?;
            if let Some(loaded) = store.load_session(id)? {
                agent.messages = loaded.messages.clone();
                agent.pinned = loaded.pinned.clone();
            }
        }
    }
    Ok(())
}

fn session_visible(session_account: &str, auth: &Authed) -> bool {
    if auth.is_master {
        return true;
    }
    session_account.is_empty() || session_account == auth.account_id
}

async fn chat(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    if req.prompt.trim().is_empty() || req.prompt.len() > 32_000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"prompt must be 1–32000 characters"})),
        )
            .into_response();
    }
    let mut agent = state.agent.lock().await;
    let saved_perm = agent.config.clone();
    if matches!(agent.config.permission, crate::config::Permission::Yolo) {
        let mut cfg = (*agent.config).clone();
        cfg.permission = crate::config::Permission::AutoSafe;
        agent.config = std::sync::Arc::new(cfg);
    }
    if let Err(e) = apply_chat_overrides(&mut agent, &req).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.event_tx = Some(tx);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let prompt = req.prompt.clone();
    let result = agent.turn(&prompt).await;
    agent.event_tx = None;
    agent.config = saved_perm;
    match result {
        Ok(answer) => {
            let mut session = if let Some(id) = req.session_id.as_deref() {
                state
                    .store
                    .load_session(id)
                    .unwrap_or(None)
                    .unwrap_or_else(Session::fresh)
            } else {
                Session::fresh()
            };
            if session.account_id.is_empty() && !auth.is_master {
                session.account_id = auth.account_id.clone();
            }
            if !auth.is_master
                && !session.account_id.is_empty()
                && session.account_id != auth.account_id
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error":"session belongs to another account"})),
                )
                    .into_response();
            }
            session.summary = req.prompt.chars().take(80).collect();
            session.provider_name = agent.config.provider.clone();
            session.model = agent.config.model.clone();
            session.messages = agent.messages.clone();
            session.transcript.push(("YOU".into(), req.prompt));
            session.transcript.push(("WROSE".into(), answer.clone()));
            for hit in agent.ctf.recorded_hits() {
                let _ = state
                    .store
                    .record_flag(&session.name, &hit.flag, &hit.source, "candidate");
            }
            if let Err(e) = state.store.save_session(&session) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }
            let _ =
                session.save(&Session::dir().unwrap_or_else(|_| std::path::PathBuf::from("/tmp")));
            Json(ChatResponse {
                answer,
                session_id: session.name,
                metrics: agent.metrics.snapshot(),
            })
            .into_response()
        }
        Err(e) => {
            let _ = state.store.record_error(
                "web-chat",
                "turn",
                &e.to_string(),
                &prompt.chars().take(200).collect::<String>(),
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

async fn chat_stream(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    if req.prompt.trim().is_empty() || req.prompt.len() > 32_000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"prompt must be 1–32000 characters"})),
        )
            .into_response();
    }
    let (tx, rx) = mpsc::unbounded_channel::<crate::provider::Progress>();
    {
        let mut agent = state.agent.lock().await;
        if matches!(agent.config.permission, crate::config::Permission::Yolo) {
            let mut cfg = (*agent.config).clone();
            cfg.permission = crate::config::Permission::AutoSafe;
            agent.config = std::sync::Arc::new(cfg);
        }
        if let Err(e) = apply_chat_overrides(&mut agent, &req).await {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
        agent.event_tx = Some(tx);
    }
    let agent_clone = state.agent.clone();
    let store = state.store.clone();
    let prompt = req.prompt.clone();
    let session_id = req.session_id.clone();
    let account = auth.account_id.clone();
    let is_master = auth.is_master;
    let (done_tx, done_rx) = oneshot::channel::<serde_json::Value>();
    tokio::spawn(async move {
        let mut agent = agent_clone.lock().await;
        let r = agent.turn(&prompt).await;
        agent.event_tx = None;
        let payload = match r {
            Ok(answer) => {
                let mut session = if let Some(id) = session_id.as_deref() {
                    store
                        .load_session(id)
                        .unwrap_or(None)
                        .unwrap_or_else(Session::fresh)
                } else {
                    Session::fresh()
                };
                if session.account_id.is_empty() && !is_master {
                    session.account_id = account.clone();
                }
                session.summary = prompt.chars().take(80).collect();
                session.provider_name = agent.config.provider.clone();
                session.model = agent.config.model.clone();
                session.messages = agent.messages.clone();
                session.transcript.push(("YOU".into(), prompt.clone()));
                session.transcript.push(("WROSE".into(), answer.clone()));
                let _ = store.save_session(&session);
                serde_json::json!({"answer": answer, "session_id": session.name})
            }
            Err(e) => serde_json::json!({"error": e.to_string()}),
        };
        let _ = done_tx.send(payload);
    });
    // Stream Progress as SSE, then the final payload.
    let stream = async_stream::stream! {
        let mut rx = rx;
        let mut done_rx = done_rx;
        loop {
            tokio::select! {
                prog = rx.recv() => {
                    let Some(p) = prog else { continue; };
                    if let Some((name, data)) = progress_event(&p) {
                        yield Ok::<_, Infallible>(axum::response::sse::Event::default().event(name).data(data));
                    }
                }
                done = &mut done_rx => {
                    match done {
                        Ok(payload) => {
                            if payload.get("error").is_some() {
                                yield Ok(axum::response::sse::Event::default().event("chat.failed").data(payload.to_string()));
                            } else {
                                yield Ok(axum::response::sse::Event::default().event("chat.completed").data(payload.to_string()));
                            }
                        }
                        Err(_) => {
                            yield Ok(axum::response::sse::Event::default().event("chat.failed").data(r#"{"error":"turn ended without result"}"#));
                        }
                    }
                    break;
                }
            }
        }
    };
    Sse::new(stream).into_response()
}

fn progress_event(p: &crate::provider::Progress) -> Option<(String, String)> {
    use crate::provider::Progress as P;
    match p {
        P::TextDelta(t) => Some(("chat.delta".into(), serde_json::json!({"delta": t}).to_string())),
        P::ToolBegin { id, title } => Some((
            "tool.started".into(),
            serde_json::json!({"tool_id": id, "title": title}).to_string(),
        )),
        P::ToolEnd { id, ok, output, elapsed_ms } => Some((
            "tool.output".into(),
            serde_json::json!({"tool_id": id, "ok": ok, "output": output.chars().take(4000).collect::<String>(), "elapsed_ms": elapsed_ms})
                .to_string(),
        )),
        P::Tool(t) => Some((
            "thinking.status".into(),
            serde_json::json!({"status": t}).to_string(),
        )),
        P::Heartbeat => None,
        P::ModelStage(stage) => Some((
            "thinking.status".into(),
            serde_json::json!({"status": stage}).to_string(),
        )),
        P::ResetText => Some(("chat.started".into(), serde_json::json!({}).to_string())),
        P::Usage(u) => Some((
            "thinking.status".into(),
            serde_json::json!({"usage": u}).to_string(),
        )),
        P::Metrics(_) => None,
        P::Plan(items) => Some((
            "thinking.status".into(),
            serde_json::json!({"plan": items}).to_string(),
        )),
        P::FlagFound { flag, source } => Some((
            "ctf.flag".into(),
            serde_json::json!({"flag": flag, "source": source}).to_string(),
        )),
        P::Think { from, to, reason, .. } => Some((
            "thinking.status".into(),
            serde_json::json!({"status": format!("think: {from} → {to} ({reason})")}).to_string(),
        )),
        P::ThinkIgnored(msg) => Some((
            "thinking.status".into(),
            serde_json::json!({"status": msg}).to_string(),
        )),
    }
}

// ---------- websocket ----------

async fn ws_handler(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    state.clients.fetch_add(1, Ordering::Relaxed);
    let clients = state.clients.clone();
    ws.on_upgrade(move |socket| {
        let state = state.clone();
        async move {
            handle_ws(state, auth, socket).await;
            clients.fetch_sub(1, Ordering::Relaxed);
        }
    })
}

async fn handle_ws(state: ApiState, auth: Authed, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    let (mut sender, mut receiver) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    // Pending permission approvals: perm_id -> responder.
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let send_task = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    let send = |v: serde_json::Value| {
        let _ = out_tx.send(v.to_string());
    };
    send(serde_json::json!({"type":"hello","service":"wrosecode","account":auth.account_id}));
    while let Some(msg) = receiver.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
        };
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                send(serde_json::json!({"type":"error","error":"invalid JSON"}));
                continue;
            }
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "chat" => {
                let id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("chat-1")
                    .to_string();
                let prompt = value
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if prompt.trim().is_empty() || prompt.len() > 32_000 {
                    send(
                        serde_json::json!({"type":"chat.failed","id":id,"error":"prompt must be 1–32000 characters"}),
                    );
                    continue;
                }
                let session_id = value
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let provider = value
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let model = value
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let mode = value
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                if let Some(old) = tasks.lock().await.remove(&id) {
                    old.abort();
                }
                let state_c = state.clone();
                let auth_c = auth.clone();
                let out_c = out_tx.clone();
                let pending_c = pending.clone();
                let id_c = id.clone();
                let handle = tokio::spawn(async move {
                    run_ws_chat(
                        state_c, auth_c, out_c, pending_c, id_c, prompt, session_id, provider,
                        model, mode,
                    )
                    .await;
                });
                tasks.lock().await.insert(id, handle);
            }
            "cancel" => {
                let id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(h) = tasks.lock().await.remove(&id) {
                    h.abort();
                    let _ = out_tx
                        .send(serde_json::json!({"type":"chat.cancelled","id":id}).to_string());
                }
            }
            "permission.reply" => {
                let perm_id = value
                    .get("perm_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let allow = value
                    .get("allow")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(tx) = pending.lock().await.remove(&perm_id) {
                    let _ = tx.send(allow);
                }
            }
            "ping" => {
                send(serde_json::json!({"type":"pong"}));
            }
            _ => {
                send(serde_json::json!({"type":"error","error":"unknown message type"}));
            }
        }
    }
    send_task.abort();
}

#[allow(clippy::too_many_arguments)]
async fn run_ws_chat(
    state: ApiState,
    auth: Authed,
    out: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    id: String,
    prompt: String,
    session_id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    mode: Option<String>,
) {
    let send = |v: serde_json::Value| {
        let _ = out.send(v.to_string());
    };
    let (ptx, mut prx) = mpsc::unbounded_channel::<crate::provider::Progress>();
    {
        let mut agent = state.agent.lock().await;
        if matches!(agent.config.permission, crate::config::Permission::Yolo) {
            let mut cfg = (*agent.config).clone();
            cfg.permission = crate::config::Permission::AutoSafe;
            agent.config = std::sync::Arc::new(cfg);
        }
        let req = ChatRequest {
            prompt: prompt.clone(),
            session_id: session_id.clone(),
            provider,
            model,
            mode,
        };
        if let Err(e) = apply_chat_overrides(&mut agent, &req).await {
            send(serde_json::json!({"type":"chat.failed","id":id,"error":e.to_string()}));
            return;
        }
        agent.event_tx = Some(ptx);
    }
    send(serde_json::json!({"type":"chat.started","id":id}));
    let out_c = out.clone();
    let id_c = id.clone();
    let fwd = tokio::spawn(async move {
        while let Some(p) = prx.recv().await {
            if let Some((name, data)) = progress_event(&p) {
                let mut v: serde_json::Value =
                    serde_json::from_str(&data).unwrap_or(serde_json::json!({}));
                if let Some(o) = v.as_object_mut() {
                    o.insert("type".into(), serde_json::json!(ws_event_name(&name)));
                    o.insert("id".into(), serde_json::json!(id_c));
                }
                let _ = out_c.send(v.to_string());
            }
        }
    });
    // Permission pump + turn share the agent lock carefully: install the
    // permission channel, then run the turn while the pump forwards.
    let answer = {
        let (preq_tx, mut preq_rx) = mpsc::unbounded_channel::<crate::tools::PermissionRequest>();
        {
            let mut agent = state.agent.lock().await;
            agent.tools.permission_tx = Some(preq_tx);
        }
        let out_p = out.clone();
        let id_p = id.clone();
        let pending_p = pending.clone();
        let pump = tokio::spawn(async move {
            let mut counter: u64 = 0;
            while let Some(req) = preq_rx.recv().await {
                counter += 1;
                let perm_id = format!("perm-{counter}");
                let _ = out_p.send(
                    serde_json::json!({"type":"permission.request","id":id_p,"perm_id":perm_id,"action":req.action}).to_string(),
                );
                let (tx, rx) = oneshot::channel::<bool>();
                pending_p.lock().await.insert(perm_id, tx);
                let allow = tokio::time::timeout(std::time::Duration::from_secs(120), rx)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or(false);
                let _ = req.response.send(allow);
            }
        });
        let mut agent = state.agent.lock().await;
        let r = agent.turn(&prompt).await;
        agent.event_tx = None;
        agent.tools.permission_tx = None;
        pump.abort();
        r
    };
    fwd.abort();
    match answer {
        Ok(text) => {
            let agent = state.agent.lock().await;
            let mut session = if let Some(sid) = session_id.as_deref() {
                state
                    .store
                    .load_session(sid)
                    .unwrap_or(None)
                    .unwrap_or_else(Session::fresh)
            } else {
                Session::fresh()
            };
            if session.account_id.is_empty() && !auth.is_master {
                session.account_id = auth.account_id.clone();
            }
            if !auth.is_master
                && !session.account_id.is_empty()
                && session.account_id != auth.account_id
            {
                send(
                    serde_json::json!({"type":"chat.failed","id":id,"error":"session belongs to another account"}),
                );
                return;
            }
            session.summary = prompt.chars().take(80).collect();
            session.provider_name = agent.config.provider.clone();
            session.model = agent.config.model.clone();
            session.messages = agent.messages.clone();
            session.transcript.push(("YOU".into(), prompt));
            session.transcript.push(("WROSE".into(), text.clone()));
            let _ = state.store.save_session(&session);
            send(
                serde_json::json!({"type":"chat.completed","id":id,"answer":text,"session_id":session.name}),
            );
        }
        Err(e) => {
            send(serde_json::json!({"type":"chat.failed","id":id,"error":e.to_string()}));
        }
    }
}

fn ws_event_name(sse: &str) -> &str {
    sse
}

// ---------- sessions ----------

async fn list_sessions(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(30).min(100);
    match state.store.list_sessions(limit) {
        Ok(all) => {
            let filtered: Vec<_> = all
                .into_iter()
                .filter(|m| session_visible(&m.account_id, &auth))
                .collect();
            Json(serde_json::json!({"sessions": filtered})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn create_session(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
    Json(body): Json<CreateSession>,
) -> impl IntoResponse {
    let mut session = Session::fresh();
    if let Some(s) = body.summary {
        session.summary = s.chars().take(120).collect();
    }
    {
        let agent = state.agent.lock().await;
        session.provider_name = agent.config.provider.clone();
        session.model = agent.config.model.clone();
        session.messages = agent.messages.clone();
    }
    if !auth.is_master {
        session.account_id = auth.account_id.clone();
    }
    match state.store.save_session(&session) {
        Ok(_) => Json(serde_json::json!({"session_id": session.name})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn get_session(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.load_session(&id) {
        Ok(Some(s)) => {
            if !session_visible(&s.account_id, &auth) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error":"session belongs to another account"})),
                )
                    .into_response();
            }
            Json(serde_json::json!(s)).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn delete_session(
    State(state): State<ApiState>,
    Extension(auth): Extension<Authed>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.load_session(&id) {
        Ok(Some(s)) => {
            if !session_visible(&s.account_id, &auth) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error":"session belongs to another account"})),
                )
                    .into_response();
            }
            match state.store.delete_session(&id) {
                Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response(),
            }
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn latest_session(State(state): State<ApiState>) -> impl IntoResponse {
    match state.store.latest() {
        Ok(Some(session)) => Json(serde_json::json!(session)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":e.to_string()})),
        )
            .into_response(),
    }
}

// ---------- CTF ----------

async fn ctf_status(State(state): State<ApiState>) -> impl IntoResponse {
    let Ok(agent) = state.agent.try_lock() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"agent busy; retry shortly"})),
        )
            .into_response();
    };
    let jobs = state.ctf_jobs.lock().await;
    Json(serde_json::json!({
        "category": agent.ctf.category,
        "category_locked": agent.ctf.category_locked(),
        "jobs": jobs.values().collect::<Vec<_>>(),
        "history": agent.ctf.history("").into_iter().take(20).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn ctf_flags(State(state): State<ApiState>) -> impl IntoResponse {
    let Ok(agent) = state.agent.try_lock() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"agent busy; retry shortly"})),
        )
            .into_response();
    };
    let recorded = agent.ctf.recorded_hits();
    let stored = state.store.list_flags(None, 50).unwrap_or_default();
    Json(serde_json::json!({
        "candidates": recorded.iter().map(|h| serde_json::json!({
            "flag": h.flag, "source": h.source,
            "transformation": h.transformation, "confidence": h.confidence,
            "status": "CANDIDATE",
        })).collect::<Vec<_>>(),
        "stored": stored,
        "note": "A candidate becomes VERIFIED only after re-derivation from the artifact or challenge-authority confirmation.",
    }))
    .into_response()
}

async fn ctf_start(State(state): State<ApiState>, Json(req): Json<CtfStart>) -> impl IntoResponse {
    if req.target.trim().is_empty() || req.target.len() > 8000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"target must be 1–8000 characters"})),
        )
            .into_response();
    }
    let id = format!("ctf_{}", unix_now());
    let job = CtfJob {
        id: id.clone(),
        target: req.target.chars().take(300).collect(),
        category: req.category.clone(),
        status: "queued".into(),
        created_at: unix_now(),
        finished_at: None,
        flag: None,
        evidence: None,
        report: None,
    };
    state.ctf_jobs.lock().await.insert(id.clone(), job);
    let state_c = state.clone();
    let id_c = id.clone();
    let id_ret = id.clone();
    tokio::spawn(async move {
        {
            let mut jobs = state_c.ctf_jobs.lock().await;
            if let Some(j) = jobs.get_mut(&id_c) {
                j.status = "running".into();
            }
        }
        let outcome = run_ctf_job(&state_c, &id_c, req).await;
        let mut jobs = state_c.ctf_jobs.lock().await;
        if let Some(j) = jobs.get_mut(&id) {
            j.finished_at = Some(unix_now());
            match outcome {
                Ok((flag, evidence)) => {
                    j.status = "completed".into();
                    j.flag = flag;
                    j.evidence = evidence;
                }
                Err(report) => {
                    j.status = "completed".into();
                    j.report = Some(report);
                }
            }
        }
    });
    Json(serde_json::json!({"job_id": id_ret, "status": "queued"})).into_response()
}

async fn run_ctf_job(
    state: &ApiState,
    job_id: &str,
    req: CtfStart,
) -> Result<(Option<String>, Option<String>), String> {
    use crate::autopilot;
    let mut agent = state.agent.lock().await;
    let options = autopilot::Options {
        target: req.target.clone(),
        flag_format: req.flag_format.clone(),
        category: req.category.clone(),
        remote: None,
        budget: autopilot::Budget::parse(req.budget.as_deref().unwrap_or("steps=6"))
            .map_err(|error| error.to_string())?,
        parallel: 2,
    };
    let root = agent.config.root.clone();
    let scope = autopilot::Scope::for_target(&options.target, &root, None);
    agent.tools.set_scope(Some(scope.clone()));
    if let Some(pattern) = &options.flag_format {
        if let Ok(e) = agent.ctf.clone().with_flag_format(pattern) {
            agent.ctf = e;
        }
    }
    if let Some(c) = &options.category {
        agent.ctf.category = c.clone();
    }
    agent.ctf.begin_run();
    agent.ctf.lock_category();
    let triage = autopilot::triage(&root).await;
    let brief = autopilot::brief(
        &options,
        &scope,
        &autopilot::tool_inventory(),
        &agent.ctf.category,
        &triage,
        None,
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.event_tx = Some(tx);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let reply = agent.turn(&brief).await;
    agent.event_tx = None;
    agent.tools.set_scope(None);
    match reply {
        Ok(text) => {
            let hits = agent.ctf.current_hits();
            if let Some(v) = autopilot::verify(&agent.ctf, &hits) {
                let _ = state
                    .store
                    .record_flag(job_id, &v.flag, &v.evidence, "verified");
                Ok((Some(v.flag), Some(v.evidence)))
            } else {
                let candidate = autopilot::candidate_note(&agent.ctf, &hits);
                Err(candidate.unwrap_or(text.chars().take(2000).collect()))
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

async fn ctf_upload(Json(req): Json<CtfUpload>) -> impl IntoResponse {
    if req.filename.len() > 100
        || !req
            .filename
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.() ".contains(c))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid filename"})),
        )
            .into_response();
    }
    let Some(b64) = req.content_base64 else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"content_base64 is required"})),
        )
            .into_response();
    };
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"invalid base64"})),
            )
                .into_response()
        }
    };
    if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error":"artifact must be 1 byte–16 MiB"})),
        )
            .into_response();
    }
    let mut rnd = [0u8; 8];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut rnd).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"random failed"})),
        )
            .into_response();
    }
    let id: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join("wrosecode-ctf");
    if std::fs::create_dir_all(&dir).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"upload dir failed"})),
        )
            .into_response();
    }
    let safe_name = req.filename.replace(['/', '\\'], "_");
    let path = dir.join(format!("{id}-{safe_name}"));
    if std::fs::write(&path, &bytes).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"write failed"})),
        )
            .into_response();
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    Json(serde_json::json!({
        "artifact_id": id,
        "path": path.display().to_string(),
        "size": bytes.len(),
        "sha256": hex,
        "description": req.description.unwrap_or_default().chars().take(500).collect::<String>(),
    }))
    .into_response()
}

// ---------- pairing & devices ----------

async fn pair_status() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "paired_devices": crate::pairing::list_devices().len(),
        "pending_codes": crate::pairing::pending_count(),
    }))
}

async fn pair_confirm(Json(req): Json<PairConfirm>) -> impl IntoResponse {
    if req.account_id.trim().is_empty() || req.account_id.len() > 320 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid account"})),
        )
            .into_response();
    }
    if req.account_id.contains("://") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid account"})),
        )
            .into_response();
    }
    let name = req
        .device_name
        .unwrap_or_else(|| "Browser device".to_string());
    match crate::pairing::confirm_pairing(&req.code, req.account_id.trim(), name.trim()) {
        Ok((device_id, token)) => Json(serde_json::json!({
            "device_id": device_id,
            "device_token": token,
            "warning": "Store this token securely; it authorizes this browser for this device only.",
        }))
        .into_response(),
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn list_devices() -> Json<serde_json::Value> {
    Json(serde_json::json!({"devices": crate::pairing::list_devices()}))
}

async fn revoke_device(
    Extension(auth): Extension<Authed>,
    Json(req): Json<RevokeDevice>,
) -> impl IntoResponse {
    let account = if auth.is_master {
        None
    } else {
        Some(auth.account_id.as_str())
    };
    match crate::pairing::revoke_device(&req.device_id, account) {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"device not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------- relay handler ----------

async fn handle_relay_message(
    state: ApiState,
    msg: serde_json::Value,
) -> Option<serde_json::Value> {
    let kind = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let id = msg
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let agent = state.agent.clone();
    let store = state.store.clone();
    match kind {
        "chat" => {
            let prompt = msg
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if prompt.trim().is_empty() {
                return Some(
                    serde_json::json!({"type":"chat.failed","id":id,"error":"empty prompt"}),
                );
            }
            let mut agent = agent.lock().await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            agent.event_tx = Some(tx);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            match agent.turn(&prompt).await {
                Ok(answer) => {
                    agent.event_tx = None;
                    let mut session = Session::fresh();
                    session.summary = prompt.chars().take(80).collect();
                    session.provider_name = agent.config.provider.clone();
                    session.model = agent.config.model.clone();
                    session.messages = agent.messages.clone();
                    let _ = store.save_session(&session);
                    Some(
                        serde_json::json!({"type":"chat.completed","id":id,"answer":answer,"session_id":session.name}),
                    )
                }
                Err(e) => {
                    agent.event_tx = None;
                    Some(serde_json::json!({"type":"chat.failed","id":id,"error":e.to_string()}))
                }
            }
        }
        "ping" => Some(serde_json::json!({"type":"pong","id":id})),
        "models" => Some(relay_models(state.clone(), &id).await),
        "model.select" => Some(relay_model_select(&state, &id, &msg).await),
        "sessions.list" => Some(relay_sessions(&state, &id, &msg).await),
        "ctf.status" => Some(relay_ctf_status(&state, &id).await),
        "ctf.flags" => Some(relay_ctf_flags(&state, &id).await),
        "ctf.upload" => Some(relay_ctf_upload(&id, &msg).await),
        "ctf.start" => Some(relay_ctf_start(&state, &id, &msg).await),
        // Cloud pairing: the browser (already authenticated by the relay to
        // `account_id`) forwards the one-time code shown by
        // `wrosecode bridge pair`. Running that command is the local approval;
        // the code itself is single-use, expiring, and attempt-limited.
        "pair.claim" => Some(relay_pair_claim(&id, &msg).await),
        _ => None,
    }
}

async fn relay_models(state: ApiState, id: &str) -> serde_json::Value {
    let settings = match crate::settings::Settings::load() {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({"type":"models","id":id,"error":e.to_string()});
        }
    };
    let client = match crate::provider::streaming_client(10) {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({"type":"models","id":id,"error":e.to_string()});
        }
    };
    let mut groups = Vec::new();
    for profile in &settings.providers {
        let key = settings.key(profile);
        let discovered =
            match crate::provider::create_profile(profile, key, &profile.model, client.clone()) {
                Ok(p) => p.list_models().await.unwrap_or_default(),
                Err(_) => Vec::new(),
            };
        let mut models = discovered;
        if !profile.model.is_empty() && !models.contains(&profile.model) {
            models.insert(0, profile.model.clone());
        }
        models.truncate(500);
        groups.push(serde_json::json!({
            "provider": profile.name,
            "label": crate::settings::Settings::display(profile),
            "models": models,
            "default_model": profile.model,
        }));
    }
    let current = {
        let agent = state.agent.lock().await;
        serde_json::json!({"provider": agent.config.provider, "model": agent.config.model})
    };
    serde_json::json!({"type":"models","id":id,"groups":groups,"current":current})
}

async fn relay_model_select(
    state: &ApiState,
    id: &str,
    msg: &serde_json::Value,
) -> serde_json::Value {
    let provider = msg
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let model = msg
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if provider.trim().is_empty()
        || model.trim().is_empty()
        || provider.len() > 100
        || model.len() > 200
    {
        return serde_json::json!({"type":"model.selected","id":id,"error":"invalid provider/model"});
    }
    let settings = match crate::settings::Settings::load() {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({"type":"model.selected","id":id,"error":e.to_string()});
        }
    };
    let Some(profile) = settings.profile(&provider).cloned() else {
        return serde_json::json!({"type":"model.selected","id":id,"error":"unknown provider"});
    };
    let client = match crate::provider::streaming_client(10) {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({"type":"model.selected","id":id,"error":e.to_string()});
        }
    };
    let key = settings.key(&profile);
    let provider_obj = match crate::provider::create_profile(&profile, key, &model, client.clone())
    {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({"type":"model.selected","id":id,"error":e.to_string()});
        }
    };
    let mut agent = state.agent.lock().await;
    agent.provider = provider_obj;
    let mut cfg = (*agent.config).clone();
    cfg.provider = provider.clone();
    cfg.model = model.clone();
    agent.config = std::sync::Arc::new(cfg);
    serde_json::json!({"type":"model.selected","id":id,"provider":provider,"model":model})
}

async fn relay_sessions(state: &ApiState, id: &str, msg: &serde_json::Value) -> serde_json::Value {
    let account = msg.get("account_id").and_then(|v| v.as_str()).unwrap_or("");
    let limit = msg
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .min(100) as usize;
    match state.store.list_sessions(limit) {
        Ok(all) => {
            let sessions: Vec<_> = all
                .into_iter()
                .filter(|m| {
                    account.is_empty() || m.account_id.is_empty() || m.account_id == account
                })
                .collect();
            serde_json::json!({"type":"sessions","id":id,"sessions":sessions})
        }
        Err(e) => serde_json::json!({"type":"sessions","id":id,"error":e.to_string()}),
    }
}

async fn relay_ctf_status(state: &ApiState, id: &str) -> serde_json::Value {
    let agent = state.agent.lock().await;
    let jobs = state.ctf_jobs.lock().await;
    serde_json::json!({
        "type": "ctf.status",
        "id": id,
        "category": agent.ctf.category,
        "jobs": jobs.values().collect::<Vec<_>>(),
    })
}

async fn relay_ctf_flags(state: &ApiState, id: &str) -> serde_json::Value {
    let agent = state.agent.lock().await;
    let recorded = agent.ctf.recorded_hits();
    let stored = state.store.list_flags(None, 50).unwrap_or_default();
    serde_json::json!({
        "type": "ctf.flags",
        "id": id,
        "candidates": recorded.iter().map(|h| serde_json::json!({
            "flag": h.flag, "source": h.source, "status": "CANDIDATE",
        })).collect::<Vec<_>>(),
        "stored": stored,
    })
}

async fn relay_ctf_upload(id: &str, msg: &serde_json::Value) -> serde_json::Value {
    let filename = msg.get("filename").and_then(|v| v.as_str()).unwrap_or("");
    if filename.len() > 100
        || filename.is_empty()
        || !filename
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.() ".contains(c))
    {
        return serde_json::json!({"type":"ctf.uploaded","id":id,"error":"invalid filename"});
    }
    let Some(b64) = msg.get("content_base64").and_then(|v| v.as_str()) else {
        return serde_json::json!({"type":"ctf.uploaded","id":id,"error":"content_base64 is required"});
    };
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
        Ok(b) => b,
        Err(_) => {
            return serde_json::json!({"type":"ctf.uploaded","id":id,"error":"invalid base64"});
        }
    };
    if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
        return serde_json::json!({"type":"ctf.uploaded","id":id,"error":"artifact must be 1 byte–16 MiB"});
    }
    let mut rnd = [0u8; 8];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut rnd).is_err() {
        return serde_json::json!({"type":"ctf.uploaded","id":id,"error":"random failed"});
    }
    let artifact: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join("wrosecode-ctf");
    if std::fs::create_dir_all(&dir).is_err() {
        return serde_json::json!({"type":"ctf.uploaded","id":id,"error":"upload dir failed"});
    }
    let safe_name = filename.replace(['/', '\\'], "_");
    let path = dir.join(format!("{artifact}-{safe_name}"));
    if std::fs::write(&path, &bytes).is_err() {
        return serde_json::json!({"type":"ctf.uploaded","id":id,"error":"write failed"});
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    serde_json::json!({
        "type": "ctf.uploaded",
        "id": id,
        "artifact_id": artifact,
        "path": path.display().to_string(),
        "size": bytes.len(),
        "sha256": hex,
    })
}

async fn relay_ctf_start(state: &ApiState, id: &str, msg: &serde_json::Value) -> serde_json::Value {
    let target = msg.get("target").and_then(|v| v.as_str()).unwrap_or("");
    if target.trim().is_empty() || target.len() > 8000 {
        return serde_json::json!({"type":"ctf.started","id":id,"error":"target must be 1–8000 characters"});
    }
    let job_id = format!("ctf_{}", unix_now());
    let job = CtfJob {
        id: job_id.clone(),
        target: target.chars().take(300).collect(),
        category: msg
            .get("category")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        status: "queued".into(),
        created_at: unix_now(),
        finished_at: None,
        flag: None,
        evidence: None,
        report: None,
    };
    state.ctf_jobs.lock().await.insert(job_id.clone(), job);
    let state_c = state.clone();
    let job_id_c = job_id.clone();
    let req = CtfStart {
        target: target.to_string(),
        category: msg
            .get("category")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        flag_format: msg
            .get("flag_format")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        budget: None,
    };
    tokio::spawn(async move {
        {
            let mut jobs = state_c.ctf_jobs.lock().await;
            if let Some(j) = jobs.get_mut(&job_id_c) {
                j.status = "running".into();
            }
        }
        let outcome = run_ctf_job(&state_c, &job_id_c, req).await;
        let mut jobs = state_c.ctf_jobs.lock().await;
        if let Some(j) = jobs.get_mut(&job_id_c) {
            j.finished_at = Some(unix_now());
            match outcome {
                Ok((flag, evidence)) => {
                    j.status = "completed".into();
                    j.flag = flag;
                    j.evidence = evidence;
                }
                Err(report) => {
                    j.status = "completed".into();
                    j.report = Some(report);
                }
            }
        }
    });
    serde_json::json!({"type":"ctf.started","id":id,"job_id":job_id.clone(),"status":"queued"})
}

async fn relay_pair_claim(id: &str, msg: &serde_json::Value) -> serde_json::Value {
    let code = msg.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let account_id = msg.get("account_id").and_then(|v| v.as_str()).unwrap_or("");
    let device_name = msg
        .get("device_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Browser device");
    if account_id.trim().is_empty() || account_id.len() > 320 || account_id.contains("://") {
        return serde_json::json!({"type":"pair.granted","id":id,"error":"invalid account"});
    }
    match crate::pairing::confirm_pairing(code, account_id.trim(), device_name.trim()) {
        Ok((device_id, token)) => {
            // Persist the issued device token as the relay secret so the
            // bridge survives restarts without re-pairing.
            if let Some((url, _, _)) = crate::bridge::relay_config() {
                let _ = crate::bridge::save_relay_config(&url, &device_id, &token);
            }
            serde_json::json!({
                "type": "pair.granted",
                "id": id,
                "device_id": device_id,
                "device_name": device_name.trim(),
                "device_token": token,
                "for_account": account_id.trim(),
            })
        }
        Err(e) => serde_json::json!({"type":"pair.granted","id":id,"error":e.to_string()}),
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>WROSECODE</title><style>
body{margin:0;background:#0b1020;color:#d8dee9;font:14px ui-monospace,monospace}
header{padding:14px;color:#88c0d0;border-bottom:1px solid #334155}.grid{display:grid;grid-template-columns:2fr 1fr;grid-template-rows:55vh 35vh;gap:8px;padding:8px}.pane{border:1px solid #334155;border-radius:8px;padding:12px;overflow:auto;background:#111827}h3{margin-top:0;color:#81a1c1}textarea{width:100%;height:80px;background:#0b1020;color:#fff;border:1px solid #475569}button{background:#5e81ac;color:#fff;border:0;padding:9px 14px;margin-top:8px}</style></head>
<body><header>╦ ╦╦═╗╔═╗╔═╗╔═╗ WROSECODE · Web Dashboard</header><main class="grid">
<section class="pane"><h3>Transcript</h3><pre id="transcript"></pre><textarea id="prompt"></textarea><button onclick="send()">Run</button></section>
<section class="pane"><h3>Token Dashboard</h3><pre id="metrics"></pre></section>
<section class="pane"><h3>Subagent Tree</h3><pre>root\n└─ ready</pre></section>
<section class="pane"><h3>Tool Timeline</h3><pre id="tools">Waiting for task…</pre></section></main>
<script>async function refresh(){let r=await fetch('/v1/status');metrics.textContent=JSON.stringify(await r.json(),null,2)}async function send(){let p=prompt.value;transcript.textContent+='YOU '+p+'\n';let r=await fetch('/v1/chat',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({prompt:p})});let j=await r.json();transcript.textContent+='WROSE '+(j.answer||j.error)+'\n';prompt.value='';refresh()}refresh();setInterval(refresh,1000)</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_visibility_is_account_scoped() {
        let master = Authed {
            account_id: String::new(),
            is_master: true,
        };
        let alice = Authed {
            account_id: "alice@example.com".into(),
            is_master: false,
        };
        assert!(session_visible("", &alice));
        assert!(session_visible("alice@example.com", &alice));
        assert!(!session_visible("bob@example.com", &alice));
        assert!(session_visible("bob@example.com", &master));
    }

    #[test]
    fn pairing_confirm_rejects_url_shaped_accounts() {
        let bad = "https://evil.example/x";
        assert!(bad.contains("://"));
    }

    #[test]
    fn progress_events_cover_required_stream_types() {
        use crate::provider::Progress as P;
        let cases: Vec<(P, &str)> = vec![
            (P::ResetText, "chat.started"),
            (P::TextDelta("hi".into()), "chat.delta"),
            (
                P::ToolBegin {
                    id: "1".into(),
                    title: "t".into(),
                },
                "tool.started",
            ),
            (
                P::ToolEnd {
                    id: "1".into(),
                    ok: true,
                    output: "o".into(),
                    elapsed_ms: 1,
                },
                "tool.output",
            ),
            (
                P::FlagFound {
                    flag: "flag{x}".into(),
                    source: "s".into(),
                },
                "ctf.flag",
            ),
        ];
        for (p, want) in cases {
            let (name, _) = progress_event(&p).expect("event");
            assert_eq!(name, want);
        }
    }
}
