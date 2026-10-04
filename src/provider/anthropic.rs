use super::{Content, Message, Progress, Provider, Response, ToolCall, Usage};
use crate::think::{ThinkLevel, ThinkValue};
use anyhow::{bail, Context};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct Anthropic {
    pub client: reqwest::Client,
    pub key: String,
    pub model: String,
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    /// Per-level thinking budgets from providers.toml; when set the map is
    /// authoritative (a level missing from it sends no thinking control).
    pub think_map: Option<BTreeMap<String, ThinkValue>>,
}

impl Anthropic {
    fn messages(messages: &[Message]) -> Vec<Value> {
        messages.iter().map(|m| {
            let blocks: Vec<Value> = m.content.iter().map(|c| match c {
                Content::Text(text) => json!({"type":"text","text":text}),
                Content::Call(call) => json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.input}),
                Content::Result { id, output, is_error } => json!({"type":"tool_result","tool_use_id":id,"content":output,"is_error":is_error}),
            }).collect();
            json!({"role":m.role,"content":blocks})
        }).collect()
    }

    /// The native extended-thinking control. A configured `think_map` wins;
    /// otherwise these per-level defaults apply. Anthropic requires budgets
    /// of at least 1024 tokens, and they must stay under `max_tokens` (8192).
    fn thinking_param(
        think: ThinkLevel,
        map: Option<&BTreeMap<String, ThinkValue>>,
    ) -> Option<Value> {
        if think == ThinkLevel::Off {
            return None;
        }
        let budget = match map {
            Some(map) => match map.get(think.name())? {
                ThinkValue::Number(number) => *number,
                // A reasoning_effort-style string says nothing about budgets.
                ThinkValue::Text(_) => return None,
            },
            None => match think.name() {
                "low" => 1024,
                "medium" => 2048,
                "high" => 4096,
                "max" => 6144,
                _ => return None,
            },
        };
        (budget >= 1024).then(|| json!({"type":"enabled","budget_tokens":budget.min(8191)}))
    }

    fn body(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        think: ThinkLevel,
    ) -> Value {
        // The agent marks the stable/volatile split in the system prompt;
        // turn it into a prompt-cache breakpoint: the stable block carries
        // `cache_control` and the volatile tail rides after it unmarked, so
        // recall/repo-map/skill/think changes never bust the cached prefix.
        let system = match system.split_once(super::SYSTEM_VOLATILE_MARK) {
            Some((stable, volatile)) if stable.trim().is_empty() => json!(volatile),
            Some((stable, volatile)) if volatile.trim().is_empty() => json!(stable),
            Some((stable, volatile)) => json!([
                {"type": "text", "text": stable, "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": volatile},
            ]),
            None => json!(system),
        };
        let mut body = json!({"model":self.model,"max_tokens":8192,"system":system,
            "messages":Self::messages(messages),"tools":tools,"stream":true});
        if require_tool && !tools.is_empty() {
            body["tool_choice"] = json!({"type":"any"});
        }
        if let Some(thinking) = Self::thinking_param(think, self.think_map.as_ref()) {
            body["thinking"] = thinking;
        }
        body
    }

    fn request(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .post(&self.endpoint)
            .header("anthropic-version", "2023-06-01")
            .json(body);
        if !self.key.is_empty() {
            request = request.header("x-api-key", &self.key);
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
    }
}

#[async_trait]
impl Provider for Anthropic {
    async fn chat_stream_with_think(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&tokio::sync::mpsc::UnboundedSender<Progress>>,
        think: ThinkLevel,
    ) -> anyhow::Result<Response> {
        let body = self.body(system, messages, tools, require_tool, think);
        let mut resp = super::send_retry(self.request(&body))
            .await
            .context("Anthropic request failed")?;
        if resp.status().as_u16() == 400 && body.get("thinking").is_some() {
            // This endpoint rejected extended thinking — retry once without
            // it so the turn still goes through, and let the caller report
            // that the level was ignored (spec PHASE 4).
            let mut fallback = body.clone();
            if let Some(object) = fallback.as_object_mut() {
                object.remove("thinking");
            }
            resp = super::send_retry(self.request(&fallback))
                .await
                .context("Anthropic request failed")?;
            if !resp.status().is_success() {
                bail!("Anthropic {}", super::classify_status(resp.status()));
            }
            if let Some(tx) = progress {
                let _ = tx.send(Progress::ThinkIgnored(
                    "anthropic rejected extended thinking".into(),
                ));
            }
        } else if !resp.status().is_success() {
            bail!("Anthropic {}", super::classify_status(resp.status()));
        }
        let mut stream = resp.bytes_stream();
        let mut pending = Vec::new();
        let mut blocks: BTreeMap<usize, StreamBlock> = BTreeMap::new();
        let mut usage = Usage::default();
        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("provider stream timed out while waiting for output")
                })?;
            let Some(chunk) = next else {
                break;
            };
            pending.extend_from_slice(&chunk?);
            while let Some(event) = super::take_sse_event(&mut pending) {
                for line in event.lines().filter_map(|line| line.strip_prefix("data: ")) {
                    let data: Value =
                        serde_json::from_str(line).context("invalid Anthropic stream event")?;
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    if data["type"] == "message_start" {
                        usage.input = data["message"]["usage"]["input_tokens"]
                            .as_u64()
                            .unwrap_or(0);
                        usage.cache_read = data["message"]["usage"]["cache_read_input_tokens"]
                            .as_u64()
                            .unwrap_or(0);
                        usage.cache_write = data["message"]["usage"]["cache_creation_input_tokens"]
                            .as_u64()
                            .unwrap_or(0);
                    }
                    if data["type"] == "message_delta" {
                        usage.output = data["usage"]["output_tokens"].as_u64().unwrap_or(0);
                    }
                    match data["type"].as_str() {
                        Some("content_block_start") => {
                            let block = &data["content_block"];
                            blocks.insert(
                                index,
                                if block["type"] == "tool_use" {
                                    StreamBlock::Tool {
                                        id: block["id"].as_str().unwrap_or_default().into(),
                                        name: block["name"].as_str().unwrap_or_default().into(),
                                        input: String::new(),
                                    }
                                } else {
                                    StreamBlock::Text(String::new())
                                },
                            );
                        }
                        Some("content_block_delta") => {
                            let delta = &data["delta"];
                            if let Some(block) = blocks.get_mut(&index) {
                                match block {
                                    StreamBlock::Text(text) => {
                                        let piece = delta["text"].as_str().unwrap_or_default();
                                        text.push_str(piece);
                                        if let Some(tx) = progress {
                                            let _ = tx.send(Progress::TextDelta(piece.into()));
                                        }
                                    }
                                    StreamBlock::Tool { input, .. } => input.push_str(
                                        delta["partial_json"].as_str().unwrap_or_default(),
                                    ),
                                }
                            }
                        }
                        Some("error") => bail!("Anthropic stream error: {data}"),
                        _ => {}
                    }
                }
            }
        }
        if !pending.is_empty() {
            for line in String::from_utf8_lossy(&pending)
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
            {
                let data: Value =
                    serde_json::from_str(line).context("invalid final Anthropic stream event")?;
                if data["type"] == "message_stop" {
                    continue;
                }
            }
        }
        let mut content = Vec::new();
        for (_, block) in blocks {
            content.push(match block {
                StreamBlock::Text(text) => Content::Text(text),
                StreamBlock::Tool { id, name, input } => Content::Call(ToolCall {
                    id,
                    name,
                    input: serde_json::from_str(if input.is_empty() { "{}" } else { &input })
                        .context("invalid streamed tool input")?,
                }),
            });
        }
        if usage.input == 0 && usage.output == 0 {
            usage.input = ((system.len()
                + serde_json::to_string(messages).unwrap_or_default().len())
                / 4) as u64;
            usage.output = (serde_json::to_string(&content).unwrap_or_default().len() / 4) as u64;
            usage.estimated = true;
        }
        Ok(Response { content, usage })
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let base = self.endpoint.trim_end_matches("/messages");
        let mut request = self
            .client
            .get(format!("{base}/models"))
            .header("anthropic-version", "2023-06-01");
        if !self.key.is_empty() {
            request = request.header("x-api-key", &self.key);
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = super::send_retry(request).await?;
        if !response.status().is_success() {
            bail!("{}", super::classify_status(response.status()));
        }
        let data: Value = response.json().await.context("invalid models response")?;
        Ok(data["data"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn probe_chat(&self) -> anyhow::Result<()> {
        if self.model.is_empty() {
            bail!("set a model to test this provider");
        }
        let body = json!({"model":self.model,"max_tokens":1,"messages":[{"role":"user","content":"ping"}]});
        let mut request = self
            .client
            .post(&self.endpoint)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        if !self.key.is_empty() {
            request = request.header("x-api-key", &self.key);
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = super::send_retry(request).await?;
        if !response.status().is_success() {
            bail!("{}", super::classify_status(response.status()));
        }
        Ok(())
    }
}

enum StreamBlock {
    Text(String),
    Tool {
        id: String,
        name: String,
        input: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn the_system_split_becomes_a_prompt_cache_breakpoint() {
        let provider = Anthropic {
            client: reqwest::Client::new(),
            key: "test".into(),
            model: "test".into(),
            endpoint: "http://127.0.0.1:0".into(),
            headers: BTreeMap::new(),
            think_map: None,
        };
        let marked = format!(
            "stable prefix{}volatile tail",
            crate::provider::SYSTEM_VOLATILE_MARK
        );
        let body = provider.body(&marked, &[], &[], false, ThinkLevel::Off);
        let blocks = body["system"].as_array().expect("split into blocks");
        assert_eq!(blocks.len(), 2, "{body}");
        assert_eq!(blocks[0]["text"], "stable prefix");
        assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(blocks[1]["text"], "volatile tail");

        // Probe/compact calls carry no marker and stay a plain string.
        let plain = provider.body("Connection test.", &[], &[], false, ThinkLevel::Off);
        assert_eq!(plain["system"], json!("Connection test."));
    }

    #[tokio::test]
    async fn parses_streamed_tool_call() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let n = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..n]).contains("POST /v1/messages"));
            let payload = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"read_file\",\"input\":{}}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/main.rs\\\"}\"}}\n\n";
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}", payload.len(), payload);
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let provider = Anthropic {
            client: reqwest::Client::new(),
            key: "test".into(),
            model: "test".into(),
            endpoint,
            headers: BTreeMap::new(),
            think_map: None,
        };
        let response = provider
            .chat_stream(
                "system",
                &[Message {
                    role: "user".into(),
                    content: vec![Content::Text("read main".into())],
                }],
                &[],
                false,
                None,
            )
            .await
            .unwrap();
        match &response.content[0] {
            Content::Call(call) => {
                assert_eq!(call.name, "read_file");
                assert_eq!(call.input["path"], "src/main.rs");
            }
            _ => panic!("expected tool call"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn emits_text_deltas_while_streaming() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let payload = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n";
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}", payload.len(), payload);
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let provider = Anthropic {
            client: reqwest::Client::new(),
            key: "test".into(),
            model: "test".into(),
            endpoint,
            headers: BTreeMap::new(),
            think_map: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let response = provider
            .chat_stream(
                "system",
                &[Message {
                    role: "user".into(),
                    content: vec![Content::Text("hi".into())],
                }],
                &[],
                false,
                Some(&tx),
            )
            .await
            .unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Progress::TextDelta(text) if text == "hello"));
        assert!(matches!(&response.content[0], Content::Text(text) if text == "hello"));
        server.await.unwrap();
    }

    #[test]
    fn thinking_param_maps_each_level_to_a_budget() {
        for (level, budget) in [
            (ThinkLevel::Low, 1024),
            (ThinkLevel::Medium, 2048),
            (ThinkLevel::High, 4096),
            (ThinkLevel::Max, 6144),
        ] {
            assert_eq!(
                Anthropic::thinking_param(level, None),
                Some(json!({"type":"enabled","budget_tokens":budget})),
                "{level:?}"
            );
        }
        assert_eq!(Anthropic::thinking_param(ThinkLevel::Off, None), None);
        assert_eq!(Anthropic::thinking_param(ThinkLevel::Auto, None), None);
    }

    #[test]
    fn thinking_param_map_wins_and_clamps() {
        let map = |level: &str, value: ThinkValue| {
            let mut map = BTreeMap::new();
            map.insert(level.to_string(), value);
            map
        };
        // Number budgets pass through and clamp under max_tokens.
        assert_eq!(
            Anthropic::thinking_param(
                ThinkLevel::High,
                Some(&map("high", ThinkValue::Number(3000)))
            ),
            Some(json!({"type":"enabled","budget_tokens":3000}))
        );
        assert_eq!(
            Anthropic::thinking_param(
                ThinkLevel::Max,
                Some(&map("max", ThinkValue::Number(99_999)))
            ),
            Some(json!({"type":"enabled","budget_tokens":8191}))
        );
        // Below Anthropic's 1024 floor the control is dropped entirely.
        assert_eq!(
            Anthropic::thinking_param(ThinkLevel::Low, Some(&map("low", ThinkValue::Number(512)))),
            None
        );
        // A reasoning_effort-style string is not a budget.
        assert_eq!(
            Anthropic::thinking_param(
                ThinkLevel::Medium,
                Some(&map("medium", ThinkValue::Text("medium".into())))
            ),
            None
        );
        // A level missing from the map sends no control.
        assert_eq!(
            Anthropic::thinking_param(
                ThinkLevel::High,
                Some(&map("low", ThinkValue::Number(1024)))
            ),
            None
        );
    }
}
