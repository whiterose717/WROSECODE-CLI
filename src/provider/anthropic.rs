use super::{Content, Message, Progress, Provider, Response, ToolCall, Usage};
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
}

#[async_trait]
impl Provider for Anthropic {
    async fn chat_stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&tokio::sync::mpsc::UnboundedSender<Progress>>,
    ) -> anyhow::Result<Response> {
        let mut body = json!({"model":self.model,"max_tokens":8192,"system":system,
            "messages":Self::messages(messages),"tools":tools,"stream":true});
        if require_tool && !tools.is_empty() {
            body["tool_choice"] = json!({"type":"any"});
        }
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
        let resp = super::send_retry(request)
            .await
            .context("Anthropic request failed")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("Anthropic {}", super::classify_status(status));
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
        };
        let response = provider
            .complete(
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
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let response = provider
            .complete(
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
}
