use super::{Content, Message, Progress, Provider, Response, ToolCall, Usage};
use crate::think::{ThinkLevel, ThinkValue};
use anyhow::{bail, Context};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct OpenAiCompat {
    pub client: reqwest::Client,
    pub endpoint: String,
    pub key: Option<String>,
    pub model: String,
    pub headers: BTreeMap<String, String>,
    /// Per-level `reasoning_effort` values from providers.toml; when set the
    /// map is authoritative (a level missing from it sends no control).
    pub think_map: Option<BTreeMap<String, ThinkValue>>,
}

impl OpenAiCompat {
    /// The native `reasoning_effort` control. A configured `think_map` wins;
    /// otherwise low/medium/high map directly and `max` uses the highest
    /// effort the API offers. Wrong-typed or empty values disable the
    /// control for that level — the turn continues without it.
    fn reasoning_effort(
        think: ThinkLevel,
        map: Option<&BTreeMap<String, ThinkValue>>,
    ) -> Option<String> {
        if think == ThinkLevel::Off {
            return None;
        }
        match map {
            Some(map) => match map.get(think.name())? {
                ThinkValue::Text(text) => (!text.is_empty()).then(|| text.clone()),
                ThinkValue::Number(_) => None,
            },
            None => match think.name() {
                "low" | "medium" | "high" => Some(think.name().into()),
                "max" => Some("high".into()),
                _ => None,
            },
        }
    }

    fn request(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut request = self.client.post(&self.endpoint).json(body);
        if let Some(key) = &self.key {
            if !key.is_empty() {
                request = request.bearer_auth(key);
            }
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn capabilities(&self) -> super::ProviderCapabilities {
        // Chat-completions compatible endpoints support automatic prefix
        // caching; parallel tool calls and strict schemas are model/endpoint
        // dependent, so they stay off until an adapter can prove them.
        super::ProviderCapabilities::baseline()
            .cached()
            .with_reasoning_controls()
    }

    async fn chat_stream_with_think(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&tokio::sync::mpsc::UnboundedSender<Progress>>,
        think: ThinkLevel,
    ) -> anyhow::Result<Response> {
        // Splice the agent's volatile-tail boundary back into one block:
        // OpenAI-style prefix caches match on byte identity and the stable
        // part already leads the text.
        let system = system.replace(super::SYSTEM_VOLATILE_MARK, "\n");
        let mut wire = vec![json!({"role":"system","content":system})];
        for message in messages {
            let text = message
                .content
                .iter()
                .filter_map(|content| {
                    if let Content::Text(text) = content {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let calls: Vec<Value> = message.content.iter().filter_map(|content| if let Content::Call(call) = content { Some(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.input.to_string()}})) } else { None }).collect();
            let has_calls = !calls.is_empty();
            if has_calls {
                wire.push(json!({"role":"assistant","content":text.clone(),"tool_calls":calls}));
            }
            for content in &message.content {
                if let Content::Result { id, output, .. } = content {
                    wire.push(json!({"role":"tool","tool_call_id":id,"content":output}));
                }
            }
            if !has_calls && !text.is_empty() {
                wire.push(json!({"role":message.role,"content":text}));
            }
        }
        let defs: Vec<Value> = tools.iter().map(|tool| json!({"type":"function","function":{"name":tool["name"],"description":tool["description"],"parameters":tool["input_schema"]}})).collect();
        let mut body = json!({"model":self.model,"messages":wire,"tools":defs,"stream":true,
            "tool_choice":"auto","parallel_tool_calls":true,
            "stream_options":{"include_usage":true}});
        if require_tool && !tools.is_empty() {
            body["tool_choice"] = json!("required");
        }
        if let Some(effort) = Self::reasoning_effort(think, self.think_map.as_ref()) {
            body["reasoning_effort"] = json!(effort);
        }
        let mut response = super::send_retry(self.request(&body)).await?;
        if response.status().as_u16() == 400 && body.get("reasoning_effort").is_some() {
            // This endpoint rejected `reasoning_effort` — retry once without
            // it so the turn still goes through, and let the caller report
            // that the level was ignored (spec PHASE 4).
            let mut fallback = body.clone();
            if let Some(object) = fallback.as_object_mut() {
                object.remove("reasoning_effort");
            }
            response = super::send_retry(self.request(&fallback)).await?;
            if !response.status().is_success() {
                bail!("{}", super::classify_status(response.status()));
            }
            if let Some(sender) = progress {
                let _ = sender.send(Progress::ThinkIgnored(
                    "provider rejected reasoning_effort".into(),
                ));
            }
        } else if !response.status().is_success() {
            bail!("{}", super::classify_status(response.status()));
        }
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .contains("text/event-stream")
        {
            let data: Value = response.json().await.context("invalid provider response")?;
            return parse_response(&data);
        }
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut text = String::new();
        let mut first_delta = false;
        let mut calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut usage = Usage::default();
        let mut finished = false;
        'stream: loop {
            let next = tokio::time::timeout(super::STREAM_IDLE_TIMEOUT, stream.next())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("provider stream timed out while waiting for output")
                })?;
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk?;
            if let Some(sender) = progress {
                let _ = sender.send(Progress::Heartbeat);
            }
            pending.extend_from_slice(&chunk);
            for event in super::take_sse_events(&mut pending)? {
                for line in event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                {
                    if line.trim() == "[DONE]" {
                        finished = true;
                        break 'stream;
                    }
                    let value: Value =
                        serde_json::from_str(line).context("invalid streamed response")?;
                    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
                        bail!(
                            "provider stream error: {}",
                            error["message"]
                                .as_str()
                                .unwrap_or("unknown provider error")
                        );
                    }
                    if value["choices"].as_array().is_some_and(|choices| {
                        choices.iter().any(|choice| {
                            choice
                                .get("finish_reason")
                                .is_some_and(|reason| !reason.is_null())
                        })
                    }) {
                        finished = true;
                    }
                    update_usage(&mut usage, &value["usage"]);
                    let delta = &value["choices"][0]["delta"];
                    if let Some(piece) = delta["content"].as_str() {
                        text.push_str(piece);
                        if !first_delta {
                            first_delta = true;
                            crate::trace::global_event("provider_first_delta", "openai_compat");
                        }
                        if let Some(sender) = progress {
                            let _ = sender.send(Progress::TextDelta(piece.into()));
                        }
                    }
                    if let Some(chunks) = delta["tool_calls"].as_array() {
                        for chunk in chunks {
                            let index = chunk["index"].as_u64().unwrap_or(0) as usize;
                            let entry = calls.entry(index).or_default();
                            if let Some(id) = chunk["id"].as_str() {
                                entry.0.push_str(id);
                            }
                            if let Some(name) = chunk["function"]["name"].as_str() {
                                entry.1.push_str(name);
                            }
                            if let Some(args) = chunk["function"]["arguments"].as_str() {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
            }
        }
        if !finished && !pending.is_empty() {
            let event = String::from_utf8_lossy(&pending);
            for line in event.lines().filter_map(|line| line.strip_prefix("data: ")) {
                if line == "[DONE]" {
                    continue;
                }
                let value: Value =
                    serde_json::from_str(line).context("invalid final streamed response")?;
                update_usage(&mut usage, &value["usage"]);
                let delta = &value["choices"][0]["delta"];
                if let Some(piece) = delta["content"].as_str() {
                    text.push_str(piece);
                }
            }
        }
        if !finished {
            bail!("provider stream ended before completion; partial response discarded");
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text(text));
        }
        for (_, (id, name, args)) in calls {
            content.push(Content::Call(ToolCall {
                item_id: None,
                id,
                name,
                input: super::decode_tool_args(&args),
            }));
        }
        if content.is_empty() {
            bail!("provider returned an empty streamed response");
        }
        if usage.input == 0 && usage.output == 0 {
            usage = estimate_usage(&system, messages, &content);
        }
        Ok(Response { content, usage })
    }
    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let base = self.endpoint.trim_end_matches("/chat/completions");
        let mut models = Vec::new();
        // OpenAI-style pagination (`has_more` + `after`): most servers
        // answer on page one, but large catalogs (proxies, aggregators)
        // span pages. Bounded so a lying pager cannot loop forever.
        let mut after: Option<String> = None;
        for _ in 0..10 {
            let mut url = format!("{base}/models?limit=100");
            if let Some(cursor) = &after {
                url.push_str(&format!("&after={cursor}"));
            }
            let mut request = self.client.get(&url);
            if let Some(key) = &self.key {
                if !key.is_empty() {
                    request = request.bearer_auth(key);
                }
            }
            for (name, value) in &self.headers {
                request = request.header(name, value);
            }
            let response = super::send_retry(request).await?;
            if !response.status().is_success() {
                bail!("{}", super::classify_status(response.status()));
            }
            let data: Value = response.json().await.context("invalid models response")?;
            let (page, next) = parse_models_page(&data);
            models.extend(page);
            match next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        Ok(models)
    }

    async fn probe_chat(&self) -> anyhow::Result<()> {
        if self.model.is_empty() {
            bail!("set a model to test this provider");
        }
        let body = json!({"model":self.model,"messages":[{"role":"user","content":"ping"}],"max_tokens":1,"stream":false});
        let mut request = self.client.post(&self.endpoint).json(&body);
        if let Some(key) = &self.key {
            if !key.is_empty() {
                request = request.bearer_auth(key);
            }
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

/// Split one `GET /models` page into model IDs plus the next-page cursor.
/// Accepts `{"data": [...]}`, `{"models": [...]}`, and bare `[...]` bodies;
/// IDs come from `id`, falling back to `name`. The cursor follows OpenAI
/// pagination (`has_more` + last id) and is `None` on the final page.
fn parse_models_page(data: &Value) -> (Vec<String>, Option<String>) {
    let items = data
        .as_array()
        .or_else(|| data["data"].as_array())
        .or_else(|| data["models"].as_array());
    let mut models = items
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item["id"]
                        .as_str()
                        .or_else(|| item["name"].as_str())
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    models.retain(|id| !id.trim().is_empty());
    let next = data["has_more"]
        .as_bool()
        .unwrap_or(false)
        .then(|| models.last().cloned())
        .flatten();
    (models, next)
}

fn parse_response(data: &Value) -> anyhow::Result<Response> {
    let message = &data["choices"][0]["message"];
    if message.is_null() {
        bail!("provider response omitted choices[0].message");
    }
    let mut content = Vec::new();
    if let Some(text) = message["content"].as_str() {
        content.push(Content::Text(text.into()));
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        for call in calls {
            content.push(Content::Call(ToolCall {
                item_id: None,
                id: call["id"].as_str().context("tool id missing")?.into(),
                name: call["function"]["name"]
                    .as_str()
                    .context("function name missing")?
                    .into(),
                input: super::decode_tool_args(
                    call["function"]["arguments"].as_str().unwrap_or("{}"),
                ),
            }));
        }
    }
    let mut usage = Usage::default();
    update_usage(&mut usage, &data["usage"]);
    Ok(Response { content, usage })
}

fn update_usage(usage: &mut Usage, value: &Value) {
    if value.is_null() {
        return;
    }
    usage.input = value["prompt_tokens"].as_u64().unwrap_or(usage.input);
    usage.output = value["completion_tokens"].as_u64().unwrap_or(usage.output);
    usage.reasoning = value["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(usage.reasoning);
    usage.cache_read = value["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(usage.cache_read);
}

fn estimate_usage(system: &str, messages: &[Message], content: &[Content]) -> Usage {
    let message_chars: usize = messages
        .iter()
        .flat_map(|message| &message.content)
        .map(|item| match item {
            Content::Text(text) | Content::Result { output: text, .. } => text.len(),
            Content::Call(call) => call.name.len() + call.input.to_string().len(),
        })
        .sum();
    let output_chars: usize = content
        .iter()
        .map(|item| match item {
            Content::Text(text) => text.len(),
            Content::Call(call) => call.name.len() + call.input.to_string().len(),
            Content::Result { output, .. } => output.len(),
        })
        .sum();
    Usage {
        input: ((system.len() + message_chars) / 4) as u64,
        output: (output_chars / 4) as u64,
        estimated: true,
        ..Usage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn map(level: &str, value: ThinkValue) -> BTreeMap<String, ThinkValue> {
        let mut map = BTreeMap::new();
        map.insert(level.to_string(), value);
        map
    }

    #[test]
    fn reasoning_effort_maps_defaults() {
        assert_eq!(
            OpenAiCompat::reasoning_effort(ThinkLevel::Low, None).as_deref(),
            Some("low")
        );
        assert_eq!(
            OpenAiCompat::reasoning_effort(ThinkLevel::Medium, None).as_deref(),
            Some("medium")
        );
        assert_eq!(
            OpenAiCompat::reasoning_effort(ThinkLevel::High, None).as_deref(),
            Some("high")
        );
        // `max` is the highest effort the API offers.
        assert_eq!(
            OpenAiCompat::reasoning_effort(ThinkLevel::Max, None).as_deref(),
            Some("high")
        );
        assert_eq!(OpenAiCompat::reasoning_effort(ThinkLevel::Off, None), None);
        assert_eq!(OpenAiCompat::reasoning_effort(ThinkLevel::Auto, None), None);
    }

    #[test]
    fn reasoning_effort_map_wins() {
        // Text values pass through, empty ones disable the control.
        assert_eq!(
            OpenAiCompat::reasoning_effort(
                ThinkLevel::High,
                Some(&map("high", ThinkValue::Text("minimal".into())))
            )
            .as_deref(),
            Some("minimal")
        );
        assert_eq!(
            OpenAiCompat::reasoning_effort(
                ThinkLevel::High,
                Some(&map("high", ThinkValue::Text(String::new())))
            ),
            None
        );
        // A token budget says nothing about effort levels.
        assert_eq!(
            OpenAiCompat::reasoning_effort(
                ThinkLevel::Medium,
                Some(&map("medium", ThinkValue::Number(2048)))
            ),
            None
        );
        // A level missing from the map sends no control.
        assert_eq!(
            OpenAiCompat::reasoning_effort(
                ThinkLevel::High,
                Some(&map("low", ThinkValue::Text("low".into())))
            ),
            None
        );
    }

    #[test]
    fn models_pages_accept_every_list_shape() {
        // `{"data": [...]}` with `id`.
        let data = serde_json::json!({
            "data": [{"id": "a"}, {"id": "b"}],
            "has_more": true,
        });
        assert_eq!(
            parse_models_page(&data),
            (
                vec!["a".to_string(), "b".to_string()],
                Some("b".to_string())
            ),
            "cursor falls back to the last id"
        );
        // `{"models": [...]}` with `name` fallback, blank IDs dropped.
        let named = serde_json::json!({
            "models": [{"name": "x"}, {"id": ""}, {"id": "y"}],
        });
        assert_eq!(
            parse_models_page(&named),
            (vec!["x".to_string(), "y".to_string()], None),
            "name fallback, blanks dropped, no cursor on last page"
        );
        // Bare `[...]` bodies some gateways emit.
        let bare = serde_json::json!([{"id": "z"}]);
        assert_eq!(
            parse_models_page(&bare),
            (vec!["z".to_string()], None),
            "root arrays parse"
        );
    }

    #[tokio::test]
    async fn list_models_follows_pagination() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for page in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let n = socket.read(&mut request).await.unwrap();
                let head = String::from_utf8_lossy(&request[..n]).into_owned();
                let payload = if page == 0 {
                    assert!(head.starts_with("GET /v1/models"), "{head}");
                    assert!(head.contains("limit=100"), "{head}");
                    assert!(!head.contains("after="), "{head}");
                    r#"{"data":[{"id":"m1"},{"id":"m2"}],"has_more":true}"#
                } else {
                    assert!(head.contains("after=m2"), "{head}");
                    r#"{"data":[{"id":"m3"}],"has_more":false}"#
                };
                let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload);
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let provider = OpenAiCompat {
            client: reqwest::Client::new(),
            endpoint: format!("{base}/chat/completions"),
            key: None,
            model: String::new(),
            headers: BTreeMap::new(),
            think_map: None,
        };
        let models = provider.list_models().await.unwrap();
        assert_eq!(models, vec!["m1", "m2", "m3"], "both pages merge in order");
        server.await.unwrap();
    }
    async fn mock_stream(
        body: &str,
        keep_open: bool,
    ) -> (OpenAiCompat, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let body = body.to_owned();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 65536];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            socket
                .write_all(format!("{:x}\r\n{}\r\n", body.len(), body).as_bytes())
                .await
                .unwrap();
            if keep_open {
                std::future::pending::<()>().await;
            } else {
                socket.write_all(b"0\r\n\r\n").await.unwrap();
            }
        });
        (
            OpenAiCompat {
                client: reqwest::Client::new(),
                endpoint,
                key: None,
                model: "mock".into(),
                headers: BTreeMap::new(),
                think_map: None,
            },
            server,
        )
    }

    #[tokio::test]
    async fn done_finishes_without_waiting_for_socket_close() {
        let (provider, server) = mock_stream(
            "data:{\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata:[DONE]\n\n",
            true,
        )
        .await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            provider.chat_stream("test", &[], &[], false, None),
        )
        .await;
        server.abort();
        let reply = result
            .expect("DONE must finish while the socket remains open")
            .unwrap();
        assert!(matches!(&reply.content[0], Content::Text(text) if text == "Hello"));
    }

    #[tokio::test]
    async fn truncated_stream_does_not_execute_partial_tool_calls() {
        let (provider, server) = mock_stream(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{}\"}}]}}]}\n\n", false
        ).await;
        let error = provider
            .chat_stream("test", &[], &[], false, None)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("before completion"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn streamed_provider_error_preserves_diagnostic() {
        let (provider, server) = mock_stream(
            "data: {\"error\":{\"message\":\"model unavailable\"}}\n\n",
            false,
        )
        .await;
        let error = provider
            .chat_stream("test", &[], &[], false, None)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("model unavailable"), "{error}");
        server.await.unwrap();
    }
}
