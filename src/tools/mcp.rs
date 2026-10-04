use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// An MCP client speaking one of the two transports the spec defines:
/// newline-delimited JSON-RPC over a child process (`connect`), or
/// streamable HTTP where every message is a POST and replies arrive as
/// either one JSON document or an SSE stream (`connect_http`).
///
/// The handshake, the tool list, and `call` are identical on both — only
/// where the bytes go differs.
pub struct Mcp {
    state: Mutex<State>,
    pub schemas: Vec<Value>,
}

enum Wire {
    Stdio {
        // Boxed so the enum stays small next to the HTTP variant
        // (clippy's large_enum_variant).
        _child: Box<Child>,
        stdin: ChildStdin,
        lines: Lines<BufReader<ChildStdout>>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        /// The `Mcp-Session-Id` the server pinned us with, replayed on
        /// every later POST.
        session: Option<String>,
    },
}

struct State {
    wire: Wire,
    id: u64,
}

impl Mcp {
    pub async fn connect(binary: &str, args: &[String]) -> Result<Arc<Self>> {
        let mut child = Command::new(binary)
            .args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start MCP server {binary}"))?;
        let stdin = child.stdin.take().context("MCP stdin unavailable")?;
        let stdout = child.stdout.take().context("MCP stdout unavailable")?;
        let client = Arc::new(Self {
            state: Mutex::new(State {
                wire: Wire::Stdio {
                    _child: Box::new(child),
                    stdin,
                    lines: BufReader::new(stdout).lines(),
                },
                id: 0,
            }),
            schemas: Vec::new(),
        });
        handshake(client).await
    }

    /// The streamable-HTTP transport: POSTs to `url`, session-scoped by the
    /// `Mcp-Session-Id` header when the server issues one.
    pub async fn connect_http(url: &str, headers: &[(String, String)]) -> Result<Arc<Self>> {
        let client = crate::provider::shared_client(10, 120).context("build MCP HTTP client")?;
        let client = Arc::new(Self {
            state: Mutex::new(State {
                wire: Wire::Http {
                    client,
                    url: url.to_string(),
                    headers: headers.to_vec(),
                    session: None,
                },
                id: 0,
            }),
            schemas: Vec::new(),
        });
        handshake(client).await
    }

    /// Connect a configured server on whichever transport its definition
    /// names: a `url` speaks streamable HTTP, otherwise the binary runs
    /// stdio with its arguments.
    pub async fn connect_def(def: &crate::settings::McpServerDef) -> Result<Arc<Self>> {
        match &def.url {
            Some(url) => {
                let headers: Vec<(String, String)> = def
                    .headers
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                Self::connect_http(url, &headers).await
            }
            None => Self::connect(&def.bin, &def.args).await,
        }
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({"name":name,"arguments":arguments}))
            .await?;
        let output = result["content"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result["isError"].as_bool().unwrap_or(false) {
            bail!("MCP tool error: {output}");
        }
        Ok(output)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut state = self.state.lock().await;
        let message = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &mut state.wire {
            Wire::Stdio { stdin, .. } => {
                stdin.write_all(format!("{message}\n").as_bytes()).await?;
                stdin.flush().await?;
                Ok(())
            }
            Wire::Http {
                client,
                url,
                headers,
                session,
            } => {
                let response = post(client, url, headers, session, &message).await?;
                let status = response.status();
                if status.is_success() || status.as_u16() == 202 {
                    Ok(())
                } else {
                    bail!("MCP HTTP {status} for {method}");
                }
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut state = self.state.lock().await;
        state.id += 1;
        let id = state.id;
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let reply = match &mut state.wire {
            Wire::Stdio { stdin, lines, .. } => {
                stdin.write_all(format!("{message}\n").as_bytes()).await?;
                stdin.flush().await?;
                loop {
                    let Some(line) = lines.next_line().await? else {
                        bail!("MCP server closed stdout");
                    };
                    let reply: Value =
                        serde_json::from_str(&line).context("invalid MCP JSON line")?;
                    if reply["id"].as_u64() == Some(id) {
                        break reply;
                    }
                }
            }
            Wire::Http {
                client,
                url,
                headers,
                session,
            } => {
                let response = post(client, url, headers, session, &message).await?;
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    bail!(
                        "MCP HTTP {status}: {}",
                        body.chars().take(300).collect::<String>()
                    );
                }
                read_reply(response, id).await?
            }
        };
        if reply["id"].as_u64() != Some(id) {
            bail!("MCP reply for a different id: {reply}");
        }
        if !reply["error"].is_null() {
            bail!("MCP error: {}", reply["error"]);
        }
        Ok(reply["result"].clone())
    }
}

/// initialize → notifications/initialized → tools/list, shared by both
/// transports so neither can drift from the other.
async fn handshake(client: Arc<Mcp>) -> Result<Arc<Mcp>> {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        client.request(
            "initialize",
            json!({"protocolVersion":"2025-03-26","capabilities":{},
            "clientInfo":{"name":"wrosecode","version":env!("CARGO_PKG_VERSION")}}),
        ),
    )
    .await??;
    client
        .notify("notifications/initialized", json!({}))
        .await?;
    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        client.request("tools/list", json!({})),
    )
    .await??;
    let schemas = listed["tools"].as_array().context("MCP tools/list omitted tools")?.iter().filter_map(|tool| {
        let name = tool["name"].as_str()?;
        Some(json!({"name":format!("mcp__{name}"),"description":tool["description"].as_str().unwrap_or("MCP tool"),
            "input_schema":tool["inputSchema"]}))
    }).collect();
    let mut client =
        Arc::try_unwrap(client).map_err(|_| anyhow::anyhow!("MCP client still shared"))?;
    client.schemas = schemas;
    Ok(Arc::new(client))
}

/// One HTTP round trip: POST the message, capture the session id the
/// server hands back, and leave the body for the caller to decode.
async fn post(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    session: &mut Option<String>,
    message: &Value,
) -> Result<reqwest::Response> {
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        // The streamable-HTTP spec wants the client to advertise both.
        .header("Accept", "application/json, text/event-stream")
        .json(message);
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    if let Some(session) = session.as_deref() {
        request = request.header("Mcp-Session-Id", session);
    }
    let response = request.send().await.context("MCP HTTP request failed")?;
    // The server may pin (or re-pin) us on any response.
    if let Some(value) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
    {
        *session = Some(value.to_string());
    }
    Ok(response)
}

/// Decode a reply to a request: a plain JSON document, or an SSE stream
/// whose `data:` events carry JSON-RPC messages (the one we asked for may
/// be behind notifications or other ids).
async fn read_reply(response: reqwest::Response, id: u64) -> Result<Value> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let chunk = chunk.context("MCP SSE stream broke")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            buffer = buffer.replace("\r\n", "\n");
            while let Some(end) = buffer.find("\n\n") {
                let event: String = buffer.drain(..=end).collect();
                let Some(data) = sse_data(&event) else {
                    continue;
                };
                let value: Value =
                    serde_json::from_str(&data).context("MCP SSE event is not JSON")?;
                if value["id"].as_u64() == Some(id) {
                    return Ok(value);
                }
            }
        }
        bail!("MCP SSE stream ended without a reply to id {id}");
    }
    let body = response.text().await.context("MCP HTTP body")?;
    if body.trim().is_empty() {
        bail!("MCP HTTP response had no body for id {id}");
    }
    serde_json::from_str(&body).context("MCP HTTP reply is not JSON")
}

/// The `data:` payloads of one SSE event, joined per the SSE spec.
fn sse_data(event: &str) -> Option<String> {
    let data: Vec<&str> = event
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|value| value.strip_prefix(' ').unwrap_or(value))
        })
        .collect();
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::header;
    use axum::http::HeaderMap;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A miniature streamable-HTTP MCP server: `initialize` and
    /// `tools/call` answer with JSON (and pin a session id),
    /// `tools/list` answers with an SSE stream, and every request must
    /// carry the API key and, after the handshake, the session id.
    #[derive(Default)]
    struct Server {
        posts: AtomicUsize,
    }

    fn rpc(id: Option<u64>, value: Value) -> Value {
        match id {
            Some(id) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
            None => json!({"jsonrpc": "2.0", "result": value}),
        }
    }

    async fn handler(
        State(server): State<Arc<Server>>,
        headers: HeaderMap,
        Json(message): Json<Value>,
    ) -> Response {
        if headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            != Some("secret")
        {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32001, "message": "bad api key"}
            }))
            .into_response();
        }
        let id = message["id"].as_u64();
        let session = headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let count = server.posts.fetch_add(1, Ordering::SeqCst);
        if count > 0 && session.as_deref() != Some("sess-1") {
            let mut response = Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": "missing Mcp-Session-Id"}
            }))
            .into_response();
            response
                .headers_mut()
                .insert("mcp-session-id", "sess-1".parse().unwrap());
            return response;
        }
        match message["method"].as_str() {
            Some("initialize") => {
                let mut response = Json(rpc(
                    id,
                    json!({"protocolVersion": "2025-03-26", "capabilities": {}}),
                ))
                .into_response();
                response
                    .headers_mut()
                    .insert("mcp-session-id", "sess-1".parse().unwrap());
                response
            }
            Some("tools/list") => {
                let body = rpc(
                    id,
                    json!({"tools": [{
                        "name": "echo",
                        "description": "echo the text back",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}}
                        }
                    }]}),
                );
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    format!("data: {body}\n\n"),
                )
                    .into_response()
            }
            Some("tools/call") => {
                let text = message["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap_or("");
                Json(rpc(
                    id,
                    json!({"content": [{"type": "text", "text": format!("echo:{text}")}]}),
                ))
                .into_response()
            }
            _ => Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "unknown method"}
            }))
            .into_response(),
        }
    }

    async fn serve() -> (String, Arc<Server>) {
        let state = Arc::new(Server::default());
        let app = Router::new()
            .route("/mcp", post(handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (format!("http://{address}/mcp"), state)
    }

    #[tokio::test]
    async fn http_transport_completes_the_handshake_and_calls_a_tool() {
        let (url, server) = serve().await;
        let mcp = Mcp::connect_http(&url, &[("x-api-key".to_string(), "secret".to_string())])
            .await
            .expect("connect");
        assert_eq!(mcp.schemas.len(), 1, "tools/list came back over SSE");
        assert_eq!(mcp.schemas[0]["name"], "mcp__echo");

        let output = mcp.call("echo", json!({"text": "hi"})).await.expect("call");
        assert_eq!(output, "echo:hi");
        assert!(
            server.posts.load(Ordering::SeqCst) >= 4,
            "initialize, initialized, list, call"
        );
    }

    #[tokio::test]
    async fn http_transport_fails_when_the_api_key_is_wrong() {
        let (url, _server) = serve().await;
        let error = match Mcp::connect_http(&url, &[]).await {
            Ok(_) => panic!("the server rejects a missing key"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("MCP error") || error.to_string().contains("HTTP"),
            "{error}"
        );
    }

    #[test]
    fn sse_events_join_multi_line_data_and_skip_empty_ones() {
        assert_eq!(
            sse_data("data: {\"a\":1}\n\n"),
            Some("{\"a\":1}".to_string())
        );
        assert_eq!(
            sse_data("event: message\ndata: line1\ndata: line2\n"),
            Some("line1\nline2".to_string())
        );
        assert_eq!(sse_data(": keep-alive\n\n"), None);
        assert_eq!(sse_data("event: ping\n"), None);
    }
}
