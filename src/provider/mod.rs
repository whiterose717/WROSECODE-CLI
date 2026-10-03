pub mod anthropic;
pub mod openai_compat;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum Progress {
    ToolBegin {
        id: String,
        title: String,
    },
    ToolEnd {
        id: String,
        ok: bool,
        output: String,
        elapsed_ms: u128,
    },
    Tool(String),
    ResetText,
    TextDelta(String),
    Usage(Usage),
    Metrics(crate::metrics::MetricsSnapshot),
    FlagFound {
        flag: String,
        source: String,
    },
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub estimated: bool,
}

impl Usage {
    pub fn add(&mut self, other: Self) {
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.estimated |= other.estimated;
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    Call(ToolCall),
    Result {
        id: String,
        output: String,
        is_error: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<Content>,
}
#[derive(Clone, Debug)]
pub struct Response {
    pub content: Vec<Content>,
    pub usage: Usage,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat_stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&mpsc::UnboundedSender<Progress>>,
    ) -> anyhow::Result<Response>;
    async fn list_models(&self) -> anyhow::Result<Vec<String>>;
    async fn probe_chat(&self) -> anyhow::Result<()> {
        let messages = [Message {
            role: "user".into(),
            content: vec![Content::Text("ping".into())],
        }];
        self.chat_stream("Connection test.", &messages, &[], false, None)
            .await?;
        Ok(())
    }
    async fn test(&self) -> anyhow::Result<Duration> {
        let start = Instant::now();
        match self.list_models().await {
            Ok(_) => {}
            Err(error) if error.to_string().contains("404") => {
                self.probe_chat().await?;
            }
            Err(error) => return Err(error),
        }
        Ok(start.elapsed())
    }
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&mpsc::UnboundedSender<Progress>>,
    ) -> anyhow::Result<Response> {
        self.chat_stream(system, messages, tools, require_tool, progress)
            .await
    }
}

pub fn create(
    name: &str,
    model: &str,
    client: reqwest::Client,
) -> anyhow::Result<Arc<dyn Provider>> {
    let settings = crate::settings::Settings::load()?;
    let profile = settings
        .profile(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    create_profile(profile, settings.key(profile), model, client)
}

pub fn create_profile(
    profile: &crate::settings::ProviderProfile,
    key: Option<String>,
    model: &str,
    client: reqwest::Client,
) -> anyhow::Result<Arc<dyn Provider>> {
    let base = profile.base_url.trim_end_matches('/');
    if base.is_empty() {
        anyhow::bail!("{} needs a base URL", profile.name);
    }
    let headers: BTreeMap<String, String> = profile.headers.clone();
    match profile.kind.as_str() {
        "anthropic" => {
            let endpoint = if base.ends_with("/v1/messages") {
                base.into()
            } else if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            };
            Ok(Arc::new(anthropic::Anthropic {
                client,
                key: key.unwrap_or_default(),
                model: model.into(),
                endpoint,
                headers,
            }))
        }
        "openai_compat" | "openai-compatible" | "openai" | "ollama" => {
            let endpoint = if base.ends_with("/chat/completions") {
                base.into()
            } else {
                format!("{base}/chat/completions")
            };
            Ok(Arc::new(openai_compat::OpenAiCompat {
                client,
                key,
                model: model.into(),
                endpoint,
                headers,
            }))
        }
        other => anyhow::bail!("unsupported provider kind: {other}"),
    }
}

pub async fn send_retry(builder: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
    for attempt in 0..3 {
        let cloned = builder
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("request cannot be retried"))?;
        let response = cloned
            .send()
            .await
            .map_err(|error| anyhow::anyhow!(classify_network(&error)))?;
        if (response.status().as_u16() == 429 || response.status().is_server_error()) && attempt < 2
        {
            tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
            continue;
        }
        return Ok(response);
    }
    unreachable!()
}
pub fn classify_status(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        401 | 403 => "bad or missing API key".into(),
        404 => "Base URL returned 404. Did you forget /v1?".into(),
        429 => "rate limited (429)".into(),
        code => format!("HTTP {code}"),
    }
}
fn classify_network(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "provider request timed out".into()
    } else if error.is_connect() {
        "cannot connect to provider (DNS, TLS, or server error)".into()
    } else {
        "provider request failed".into()
    }
}

pub fn take_sse_event(pending: &mut Vec<u8>) -> Option<String> {
    let lf = pending
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = pending
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    let (end, separator) = match (lf, crlf) {
        (Some(a), Some(b)) => {
            if a.0 < b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return None,
    };
    let event = String::from_utf8_lossy(&pending[..end]).replace("\r\n", "\n");
    pending.drain(..end + separator);
    Some(event)
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::provider_cli;
    use crate::session::Session;
    use crate::settings::{ProviderProfile, Settings};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn mock_server(kind: &str, key: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let kind = kind.to_string();
        let handle = tokio::spawn(async move {
            for request_number in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0; 8192];
                    let count = socket.read(&mut buf).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..count]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes);
                assert!(request.contains(key));
                if request_number == 0 {
                    assert!(request.starts_with("GET /v1/models"));
                    let body = r#"{"data":[{"id":"mock-model"}]}"#;
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                    socket.write_all(reply.as_bytes()).await.unwrap();
                } else {
                    assert!(request.starts_with("POST /v1/"));
                    let body = if kind == "anthropic" {
                        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"read_file\",\"input\":{}}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"partial_json\":\"{\\\"path\\\":\\\"x\\\"}\"}}\n\n"
                    } else {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\",\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"x\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n"
                    };
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                    socket.write_all(reply.as_bytes()).await.unwrap();
                }
            }
        });
        (base, handle)
    }

    async fn lifecycle(kind: &str) {
        let secret = "sk-test-secret-abcdef";
        let (base, server) = mock_server(kind, secret).await;
        let dir =
            std::env::temp_dir().join(format!("wrose-provider-test-{}-{kind}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut settings = Settings::load_at(dir.clone()).unwrap();
        let name = format!("mock-{kind}");
        let profile = ProviderProfile {
            name: name.clone(),
            kind: kind.into(),
            base_url: base,
            model: "mock-model".into(),
            key_ref: String::new(),
            headers: BTreeMap::new(),
            builtin: false,
        };
        settings.upsert_provider(profile).unwrap();
        settings.set_key(&name, secret).unwrap();
        let client = reqwest::Client::new();
        assert!(provider_cli::test_one(&mut settings, &name, &client)
            .await
            .is_ok());
        let profile = settings.profile(&name).unwrap();
        let active =
            create_profile(profile, settings.key(profile), &profile.model, client).unwrap();
        let response = active
            .chat_stream(
                "system",
                &[Message {
                    role: "user".into(),
                    content: vec![Content::Text("hello".into())],
                }],
                &[],
                false,
                None,
            )
            .await
            .unwrap();
        assert!(response.content.iter().any(|content| matches!(content, Content::Call(call) if call.name == "read_file" && call.input["path"] == "x")));
        server.await.unwrap();
        let mut session = Session::fresh();
        session.provider_name = name.clone();
        session.model = "mock-model".into();
        session.save(&dir.join("sessions")).unwrap();
        assert!(!std::fs::read_to_string(dir.join("providers.toml"))
            .unwrap()
            .contains(secret));
        assert!(
            !std::fs::read_to_string(session.path(&dir.join("sessions")))
                .unwrap()
                .contains(secret)
        );
        settings.remove_provider(&name).unwrap();
        assert!(settings.profile(&name).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn openai_custom_lifecycle() {
        lifecycle("openai_compat").await;
    }
    #[tokio::test]
    async fn anthropic_custom_lifecycle() {
        lifecycle("anthropic").await;
    }
}
