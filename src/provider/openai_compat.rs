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
    async fn chat_stream_with_think(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&tokio::sync::mpsc::UnboundedSender<Progress>>,
        think: ThinkLevel,
    ) -> anyhow::Result<Response> {
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
        let mut calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
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
                    if line == "[DONE]" {
                        continue;
                    }
                    let value: Value =
                        serde_json::from_str(line).context("invalid streamed response")?;
                    update_usage(&mut usage, &value["usage"]);
                    let delta = &value["choices"][0]["delta"];
                    if let Some(piece) = delta["content"].as_str() {
                        text.push_str(piece);
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
        if !pending.is_empty() {
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
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text(text));
        }
        for (_, (id, name, args)) in calls {
            content.push(Content::Call(ToolCall {
                id,
                name,
                input: serde_json::from_str(if args.is_empty() { "{}" } else { &args })
                    .context("invalid streamed tool input")?,
            }));
        }
        if content.is_empty() {
            bail!("provider returned an empty streamed response");
        }
        if usage.input == 0 && usage.output == 0 {
            usage = estimate_usage(system, messages, &content);
        }
        Ok(Response { content, usage })
    }
    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let base = self.endpoint.trim_end_matches("/chat/completions");
        let mut request = self.client.get(format!("{base}/models"));
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
        let items = data["data"]
            .as_array()
            .or_else(|| data["models"].as_array());
        Ok(items
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item["id"]
                            .as_str()
                            .or_else(|| item["name"].as_str())
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default())
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
                id: call["id"].as_str().context("tool id missing")?.into(),
                name: call["function"]["name"]
                    .as_str()
                    .context("function name missing")?
                    .into(),
                input: serde_json::from_str(
                    call["function"]["arguments"].as_str().unwrap_or("{}"),
                )?,
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
}
