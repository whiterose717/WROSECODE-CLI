use crate::agent::Agent;
use crate::session::Session;
use crate::store::Store;
use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
struct ApiState {
    agent: Arc<Mutex<Agent>>,
    store: Store,
}

#[derive(Deserialize)]
struct ChatRequest {
    prompt: String,
}

#[derive(Serialize)]
struct ChatResponse {
    answer: String,
    session_id: String,
    metrics: crate::metrics::MetricsSnapshot,
}

pub async fn serve(agent: Agent, store: Store, address: &str) -> Result<()> {
    let state = ApiState {
        agent: Arc::new(Mutex::new(agent)),
        store,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/chat", post(chat))
        .route("/v1/sessions/latest", get(latest_session))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("WROSECODE web dashboard: http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status":"ok","service":"wrosecode"}))
}

async fn status(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let agent = state.agent.lock().await;
    // Phase 3.2: the same `DashStats` shape the attach dashboard and
    // `exec --json` consume, plus `metrics` for the legacy web page.
    let mut value = serde_json::to_value(crate::tui::stats_from_agent(&agent, "web", "web"))
        .unwrap_or_else(|_| serde_json::json!({}));
    value["metrics"] = serde_json::to_value(agent.metrics.snapshot()).unwrap_or_default();
    Json(value)
}

async fn chat(
    State(state): State<ApiState>,
    Json(request): Json<ChatRequest>,
) -> impl IntoResponse {
    let mut agent = state.agent.lock().await;
    match agent.turn(&request.prompt).await {
        Ok(answer) => {
            let mut session = Session::fresh();
            session.summary = request.prompt.chars().take(80).collect();
            session.provider_name = agent.config.provider.clone();
            session.model = agent.config.model.clone();
            session.messages = agent.messages.clone();
            session.transcript.push(("YOU".into(), request.prompt));
            session.transcript.push(("WROSE".into(), answer.clone()));
            if let Err(error) = state.store.save_session(&session) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": error.to_string()})),
                )
                    .into_response();
            }
            Json(ChatResponse {
                answer,
                session_id: session.name,
                metrics: agent.metrics.snapshot(),
            })
            .into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn latest_session(State(state): State<ApiState>) -> impl IntoResponse {
    match state.store.latest() {
        Ok(Some(session)) => Json(serde_json::json!(session)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":error.to_string()})),
        )
            .into_response(),
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
