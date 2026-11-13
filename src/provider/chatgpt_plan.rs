//! OpenAI's documented Sign in with ChatGPT plan-usage transport.
//! This is intentionally separate from the API-key Chat Completions provider.

use super::{Content, Message, Progress, Provider, Response, ToolCall, Usage};
use crate::chatgpt_auth::{Credentials, Store};
use crate::think::ThinkLevel;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const MODELS: &str = "https://api.openai.com/v1/models";
const MAX_PROVIDER_DIAGNOSTIC: usize = 4096;
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(25);

/// The provider may stream harmless SSE keep-alives indefinitely. A deadline
/// for the FIRST actionable output bounds that wait, without limiting the
/// total length of real CTF/tool generations once output starts.
fn first_output_deadline(think: ThinkLevel) -> Duration {
    match think {
        ThinkLevel::Off => Duration::from_secs(16),
        ThinkLevel::Low => Duration::from_secs(28),
        ThinkLevel::Medium | ThinkLevel::Auto => Duration::from_secs(45),
        ThinkLevel::High => Duration::from_secs(100),
        ThinkLevel::Max => Duration::from_secs(150),
    }
}

#[derive(Debug, Default)]
struct PendingFunctionCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: Option<u64>,
}

impl PendingFunctionCall {
    fn key(item_id: &str, call_id: &str) -> Option<String> {
        let item_id = item_id.trim();
        if !item_id.is_empty() {
            Some(item_id.into())
        } else if !call_id.trim().is_empty() {
            Some(call_id.into())
        } else {
            None
        }
    }

    fn upsert(
        calls: &mut BTreeMap<String, Self>,
        item_id: &str,
        call_id: &str,
        name: Option<&str>,
        arguments: Option<&str>,
        output_index: Option<u64>,
    ) -> Option<String> {
        let key = Self::key(item_id, call_id)?;
        let entry = calls.entry(key.clone()).or_default();
        if !item_id.trim().is_empty() {
            entry.item_id = item_id.trim().into();
        }
        if !call_id.trim().is_empty() {
            entry.call_id = call_id.trim().into();
        }
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            entry.name = name.into();
        }
        if let Some(arguments) = arguments {
            entry.arguments = arguments.into();
        }
        if output_index.is_some() {
            entry.output_index = output_index;
        }
        Some(key)
    }

    fn append_arguments(
        calls: &mut BTreeMap<String, Self>,
        item_id: &str,
        call_id: &str,
        delta: &str,
    ) -> Option<String> {
        let key = Self::key(item_id, call_id)?;
        let entry = calls.entry(key.clone()).or_default();
        if !item_id.trim().is_empty() {
            entry.item_id = item_id.trim().into();
        }
        if !call_id.trim().is_empty() {
            entry.call_id = call_id.trim().into();
        }
        entry.arguments.push_str(delta);
        Some(key)
    }
}

pub struct ChatGptPlan {
    pub client: reqwest::Client,
    pub model: String,
    store: Store,
    label: String,
    refresh_lock: Arc<Mutex<()>>,
}

impl ChatGptPlan {
    pub fn new(
        client: reqwest::Client,
        model: &str,
        _credentials: Credentials,
        store: Store,
        label: &str,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            store,
            label: label.into(),
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }

    async fn access_token(&self) -> Result<String> {
        // A long-lived CLI process, another terminal, or an OAuth login may
        // have rotated its credentials after this provider was constructed.
        // Always use the current on-disk token rather than a login snapshot.
        let current = self
            .store
            .account(&self.label)?
            .context("ChatGPT credentials are missing; run /auth login chatgpt")?;
        if !current.expires_soon() {
            return Ok(current.access_token);
        }
        let _guard = self.refresh_lock.lock().await;
        // Recheck under the lock. One caller may already have refreshed and
        // saved a rotating token while another caller was waiting.
        let current = self
            .store
            .account(&self.label)?
            .context("ChatGPT credentials are missing; run /auth login chatgpt")?;
        if current.expires_soon() {
            crate::chatgpt_auth::refresh_if_needed(&self.client, &self.store, &self.label).await?;
        }
        Ok(self
            .store
            .account(&self.label)?
            .context("ChatGPT credentials are missing; run /auth login chatgpt")?
            .access_token)
    }

    fn request(&self, body: &Value, token: &str) -> reqwest::RequestBuilder {
        self.client.post(ENDPOINT).bearer_auth(token).json(body)
    }

    fn valid_name(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
    }

    fn input(system: &str, messages: &[Message], registered_names: &HashSet<String>) -> Vec<Value> {
        let valid_call_ids: HashSet<&str> = messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                Content::Call(call)
                    if Self::valid_name(Some(&call.name))
                        .is_some_and(|name| registered_names.contains(&name)) =>
                {
                    Some(call.id.as_str())
                }
                _ => None,
            })
            .collect();
        let mut input = vec![
            json!({"role":"system", "content": system.replace(super::SYSTEM_VOLATILE_MARK, "\n")}),
        ];
        for message in messages {
            for content in &message.content {
                match content {
                    Content::Text(text) => {
                        input.push(json!({"role": message.role, "content": text}))
                    }
                    Content::Call(call) => {
                        if let Some(name) = Self::valid_name(Some(&call.name))
                            .filter(|name| registered_names.contains(name))
                        {
                            input.push(json!({
                                "type":"function_call", "call_id":call.id, "name":name,
                                "arguments":call.input.to_string()
                            }));
                        }
                    }
                    Content::Result { id, output, .. } if valid_call_ids.contains(id.as_str()) => {
                        input.push(json!({
                            "type":"function_call_output", "call_id":id, "output":output
                        }));
                    }
                    Content::Result { .. } => {}
                }
            }
        }
        input
    }

    fn tools(tools: &[Value]) -> Vec<Value> {
        tools
            .iter()
            .filter_map(|tool| {
                let name = Self::valid_name(tool["name"].as_str())?;
                Some(json!({
                    "type":"function", "name":name,
                    "description":tool["description"], "parameters":tool["input_schema"]
                }))
            })
            .collect()
    }

    fn registered_names(tools: &[Value]) -> HashSet<String> {
        tools
            .iter()
            .filter_map(|tool| Self::valid_name(tool["name"].as_str()))
            .collect()
    }

    fn validate_payload(body: &Value) -> Result<()> {
        let model = body["model"].as_str().unwrap_or_default().trim();
        if model.is_empty() {
            bail!("provider payload validation failed: model is empty");
        }
        if body["stream"] != true {
            bail!("provider payload validation failed: stream must be true");
        }
        if body["store"] != false {
            bail!("provider payload validation failed: store must be false");
        }
        let items = body["input"]
            .as_array()
            .context("provider payload validation failed: input is not an array")?;
        for (index, item) in items.iter().enumerate() {
            if item.get("name").is_some() && Self::valid_name(item["name"].as_str()).is_none() {
                bail!(
                    "invalid item index: {index}; item type: {}; empty field: name",
                    item["type"].as_str().unwrap_or("unknown")
                );
            }
        }
        if let Some(tools) = body["tools"].as_array() {
            for (index, tool) in tools.iter().enumerate() {
                if tool.get("name").is_some() && Self::valid_name(tool["name"].as_str()).is_none() {
                    bail!("invalid tool index: {index}; empty field: name");
                }
            }
        }
        Ok(())
    }

    fn provider_error(value: &Value) -> (Option<String>, Option<String>, Option<String>) {
        let nested = value
            .get("error")
            .filter(|error| error.is_object())
            .unwrap_or(value);
        let code = nested["code"]
            .as_str()
            .or_else(|| value["code"].as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let message = nested["message"]
            .as_str()
            .or_else(|| nested["detail"].as_str())
            .or_else(|| value["detail"].as_str())
            .or_else(|| value["message"].as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let param = nested["param"]
            .as_str()
            .or_else(|| value["param"].as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        (code, message, param)
    }

    fn diagnostic_text(body: &str) -> String {
        let body = body.trim();
        if body.is_empty() {
            return String::new();
        }
        let mut text: String = body.chars().take(MAX_PROVIDER_DIAGNOSTIC).collect();
        if body.chars().count() > MAX_PROVIDER_DIAGNOSTIC {
            text.push('…');
        }
        text
    }

    fn format_provider_failure(
        prefix: &str,
        status: Option<&str>,
        request_id: Option<&str>,
        value: &Value,
        raw_body: &str,
    ) -> String {
        let (code, message, param) = Self::provider_error(value);
        let message = message
            .or_else(|| {
                let raw = Self::diagnostic_text(raw_body);
                (!raw.is_empty()).then_some(raw)
            })
            .unwrap_or_else(|| "provider returned no diagnostic details".into());
        let mut details = Vec::new();
        if let Some(status) = status {
            details.push(format!("HTTP {status}"));
        }
        if let Some(code) = code {
            details.push(format!("code {code}"));
        }
        if let Some(param) = param {
            details.push(format!("param {param}"));
        }
        if let Some(request_id) = request_id.filter(|id| !id.is_empty()) {
            details.push(format!("request {request_id}"));
        }
        if details.is_empty() {
            format!("{prefix}: {message}")
        } else {
            format!("{prefix} ({}): {message}", details.join("; "))
        }
    }
}

#[async_trait]
impl Provider for ChatGptPlan {
    fn capabilities(&self) -> super::ProviderCapabilities {
        super::ProviderCapabilities {
            strict_tool_schema: true,
            structured_output: true,
            ..super::ProviderCapabilities::baseline()
        }
        .cached()
    }

    async fn chat_stream_with_think(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&tokio::sync::mpsc::UnboundedSender<Progress>>,
        think: ThinkLevel,
    ) -> Result<Response> {
        let registered_names = Self::registered_names(tools);
        let mut body = json!({
            "model": self.model,
            "input": Self::input(system, messages, &registered_names),
            "tools": Self::tools(tools),
            "stream": true,
            "store": false
        });
        if require_tool && !registered_names.is_empty() {
            body["tool_choice"] = json!("required");
        }
        if think != ThinkLevel::Off {
            body["reasoning"] = json!({"effort": match think {
                ThinkLevel::Low => "low", ThinkLevel::Medium => "medium",
                ThinkLevel::High | ThinkLevel::Max | ThinkLevel::Auto => "high",
                ThinkLevel::Off => "low",
            }});
        }
        Self::validate_payload(&body)?;
        if let Some(sender) = progress {
            let _ = sender.send(Progress::ModelStage("Connecting to ChatGPT…".into()));
        }
        let token = self.access_token().await?;
        let response = tokio::time::timeout(
            RESPONSE_HEADERS_TIMEOUT,
            super::send_retry_server_errors(self.request(&body, &token)),
        )
        .await
        .context("ChatGPT plan response headers timed out after 25s")??;
        if !response.status().is_success() {
            let status = response.status();
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let raw = response.text().await.unwrap_or_default();
            let data: Value = serde_json::from_str(&raw).unwrap_or_default();
            bail!(
                "{}",
                Self::format_provider_failure(
                    "ChatGPT plan request failed",
                    Some(status.as_str()),
                    request_id.as_deref(),
                    &data,
                    &raw,
                )
            );
        }
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut text = String::new();
        let mut first_delta = false;
        let mut calls: BTreeMap<String, PendingFunctionCall> = BTreeMap::new();
        let mut usage = Usage::default();
        let mut completed = false;
        let waiting_since = Instant::now();
        let first_deadline = first_output_deadline(think);
        let mut first_output = false;
        if let Some(sender) = progress {
            let _ = sender.send(Progress::ModelStage("Awaiting first model output…".into()));
        }
        loop {
            // Keep-alives and reasoning events do not count as actionable
            // output. Once text/function-call bytes arrive, use the generous
            // ordinary stream-idle ceiling for long CTF workloads.
            let wait = if first_output {
                super::STREAM_IDLE_TIMEOUT
            } else {
                let left = first_deadline.saturating_sub(waiting_since.elapsed());
                if left.is_zero() {
                    bail!(
                        "ChatGPT plan first-output timeout after {}s (retrying with less thinking)",
                        first_deadline.as_secs()
                    );
                }
                left
            };
            let chunk = tokio::time::timeout(wait, stream.next())
                .await
                .map_err(|_| {
                    if first_output {
                        anyhow::anyhow!("ChatGPT plan stream idle timeout after {}s", wait.as_secs())
                    } else {
                        anyhow::anyhow!("ChatGPT plan first-output timeout after {}s (retrying with less thinking)", first_deadline.as_secs())
                    }
                })?;
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.context("ChatGPT plan stream connection failed")?;
            if let Some(sender) = progress {
                let _ = sender.send(Progress::Heartbeat);
            }
            pending.extend_from_slice(&chunk);
            for event in super::take_sse_events(&mut pending)? {
                for line in event.lines().filter_map(|line| line.strip_prefix("data: ")) {
                    if line == "[DONE]" {
                        continue;
                    }
                    let value: Value =
                        serde_json::from_str(line).context("invalid Responses event")?;
                    let kind = value["type"].as_str().unwrap_or("");
                    match kind {
                        "response.output_text.delta" => {
                            if let Some(delta) = value["delta"].as_str() {
                                if !delta.is_empty() {
                                    first_output = true;
                                }
                                text.push_str(delta);
                                if !first_delta {
                                    first_delta = true;
                                    crate::trace::global_event(
                                        "provider_first_delta",
                                        "chatgpt_plan",
                                    );
                                }
                                if let Some(sender) = progress {
                                    let _ = sender.send(Progress::TextDelta(delta.into()));
                                }
                            }
                        }
                        "response.output_item.added" | "response.output_item.done" => {
                            let item = &value["item"];
                            if item["type"] == "function_call" {
                                first_output = true;
                                let item_id = item["id"].as_str().unwrap_or_default();
                                let call_id = item["call_id"].as_str().unwrap_or_default();
                                let key = PendingFunctionCall::upsert(
                                    &mut calls,
                                    item_id,
                                    call_id,
                                    item["name"].as_str(),
                                    item["arguments"].as_str(),
                                    value["output_index"].as_u64(),
                                )
                                .with_context(|| {
                                    "ChatGPT returned a function call without an item ID or call ID"
                                })?;
                                responses_debug(format!(
                                    "event={kind} type=function_call item_id={} call_id={} name={}",
                                    item_id,
                                    call_id,
                                    item["name"].as_str().unwrap_or("<pending>")
                                ));
                                if kind == "response.output_item.done" {
                                    responses_debug(format!(
                                        "event=response.output_item.done key={key}"
                                    ));
                                }
                            }
                        }
                        "response.function_call_arguments.delta" => {
                            let item_id = value["item_id"].as_str().unwrap_or_default();
                            let call_id = value["call_id"].as_str().unwrap_or_default();
                            let delta = value["delta"].as_str().unwrap_or_default();
                            if !delta.is_empty() {
                                first_output = true;
                            }
                            PendingFunctionCall::append_arguments(
                                &mut calls, item_id, call_id, delta,
                            )
                            .with_context(|| {
                                "ChatGPT returned an argument delta without an item ID or call ID"
                            })?;
                        }
                        "response.function_call_arguments.done" => {
                            let item_id = value["item_id"].as_str().unwrap_or_default();
                            let call_id = value["call_id"].as_str().unwrap_or_default();
                            PendingFunctionCall::upsert(
                                &mut calls,
                                item_id,
                                call_id,
                                value["name"].as_str(),
                                value["arguments"].as_str(),
                                value["output_index"].as_u64(),
                            )
                            .with_context(|| {
                                "ChatGPT returned completed arguments without an item ID or call ID"
                            })?;
                            responses_debug(format!(
                                "event=response.function_call_arguments.done item_id={} call_id={} name={}",
                                item_id,
                                call_id,
                                value["name"].as_str().unwrap_or("<pending>")
                            ));
                        }
                        "response.completed" => {
                            completed = true;
                            update_usage(&mut usage, &value["response"]["usage"]);
                        }
                        "response.failed" => {
                            bail!(
                                "{}",
                                Self::format_provider_failure(
                                    "ChatGPT plan response failed",
                                    None,
                                    None,
                                    &value["response"]["error"],
                                    "response.failed contained no diagnostic details",
                                )
                            );
                        }
                        "response.incomplete" => {
                            let reason = value["response"]["incomplete_details"]["reason"]
                                .as_str()
                                .or_else(|| value["response"]["status"].as_str())
                                .unwrap_or("provider ended the response before completion");
                            bail!("ChatGPT plan response incomplete: {reason}");
                        }
                        "error" => {
                            bail!(
                                "{}",
                                Self::format_provider_failure(
                                    "ChatGPT plan stream error",
                                    None,
                                    None,
                                    &value,
                                    "SSE error contained no diagnostic details",
                                )
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
        if !completed {
            bail!("ChatGPT plan stream ended before response.completed");
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text(text));
        }
        for call in calls.into_values() {
            let name = Self::valid_name(Some(&call.name)).with_context(|| {
                format!(
                    "ChatGPT returned a function call with an empty tool name (call_id: {}, item_id: {})",
                    call.call_id, call.item_id
                )
            })?;
            if !registered_names.contains(&name) {
                let mut registered: Vec<_> = registered_names.iter().cloned().collect();
                registered.sort();
                bail!(
                    "Unknown tool requested: {name:?}; call_id: {}; item_id: {}; registered tools: {}",
                    call.call_id,
                    call.item_id,
                    if registered.is_empty() {
                        "<none>".into()
                    } else {
                        registered.join(", ")
                    }
                );
            }
            responses_debug(format!(
                "dispatch name={} registered=true call_id={} item_id={}",
                name, call.call_id, call.item_id
            ));
            let input: Value = super::decode_tool_args(&call.arguments);
            content.push(Content::Call(ToolCall {
                item_id: (!call.item_id.is_empty()).then_some(call.item_id),
                id: call.call_id,
                name,
                input,
            }));
        }
        Ok(Response { content, usage })
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let token = self.access_token().await?;
        let response =
            super::send_retry_server_errors(self.client.get(MODELS).bearer_auth(token)).await?;
        if !response.status().is_success() {
            let status = response.status();
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let raw = response.text().await.unwrap_or_default();
            let data: Value = serde_json::from_str(&raw).unwrap_or_default();
            bail!(
                "{}",
                Self::format_provider_failure(
                    "ChatGPT model discovery failed",
                    Some(status.as_str()),
                    request_id.as_deref(),
                    &data,
                    &raw,
                )
            );
        }
        let data: Value = response
            .json()
            .await
            .context("invalid ChatGPT model catalog")?;
        let catalog = data["models"]
            .as_array()
            .or_else(|| data["data"].as_array())
            .context("ChatGPT model catalog has no models array")?;
        let mut models: Vec<String> = catalog
            .iter()
            // ChatGPT-plan catalogs mark user-visible entries explicitly.
            // Keep the fallback for compatible catalog responses that omit
            // visibility, while never showing entries marked hidden.
            .filter(|model| model["visibility"].as_str() != Some("hidden"))
            .filter(|model| {
                model["visibility"].as_str().is_none()
                    || model["visibility"].as_str() == Some("list")
            })
            .filter_map(|model| {
                model["slug"]
                    .as_str()
                    .or_else(|| model["id"].as_str())
                    .map(str::to_owned)
            })
            .collect();
        models.sort();
        models.dedup();
        Ok(models)
    }
}

fn responses_debug(message: String) {
    if std::env::var_os("WROSECODE_DEBUG").is_some() {
        eprintln!("[responses] {message}");
    }
}

fn update_usage(usage: &mut Usage, value: &Value) {
    usage.input = value["input_tokens"].as_u64().unwrap_or(usage.input);
    usage.output = value["output_tokens"].as_u64().unwrap_or(usage.output);
    usage.reasoning = value["output_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(usage.reasoning);
    usage.cache_read = value["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(usage.cache_read);
}

#[cfg(test)]
mod tests {
    use super::{first_output_deadline, ChatGptPlan, PendingFunctionCall};
    use crate::provider::{Content, Message, ToolCall};
    use crate::think::ThinkLevel;
    use serde_json::json;
    use std::collections::HashSet;
    use std::time::Duration;

    #[tokio::test]
    async fn provider_uses_freshly_rotated_token_from_store_on_every_call() {
        use crate::chatgpt_auth::{Credentials, Store};
        let directory = std::env::temp_dir().join(format!(
            "wrose-chatgpt-stale-provider-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let store = Store::test_in(directory.clone());
        let mut old = Credentials {
            client_id: "client_test".into(),
            subject: "provider_test".into(),
            email: String::new(),
            id_token: String::new(),
            access_token: "old_token".into(),
            refresh_token: "old_refresh".into(),
            scopes: vec!["resource.invoke".into(), "chatgpt.tokens.use.direct".into()],
            expires_at: u64::MAX,
            issuer: String::new(),
            ext_agent_host_id: String::new(),
        };
        store.save("test_provider", &old).unwrap();
        let provider = ChatGptPlan::new(
            reqwest::Client::new(),
            "test-model",
            old.clone(),
            store.clone(),
            "test_provider",
        );
        assert_eq!(provider.access_token().await.unwrap(), "old_token");

        // Simulate another WROSECODE terminal refreshing account credentials
        // while this provider is already running. No HTTP call is needed.
        old.access_token = "rotated_token".into();
        old.refresh_token = "rotated_refresh".into();
        store.save("test_provider", &old).unwrap();
        assert_eq!(provider.access_token().await.unwrap(), "rotated_token");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_output_timeout_scales_with_reasoning_and_is_bounded() {
        assert!(first_output_deadline(ThinkLevel::Off) <= Duration::from_secs(20));
        assert!(
            first_output_deadline(ThinkLevel::Medium) < first_output_deadline(ThinkLevel::High)
        );
        assert!(first_output_deadline(ThinkLevel::Max) <= super::super::STREAM_IDLE_TIMEOUT);
    }

    fn registered(name: &str) -> HashSet<String> {
        [name.to_string()].into_iter().collect()
    }

    fn call(name: &str, id: &str) -> Content {
        Content::Call(ToolCall {
            item_id: None,
            id: id.into(),
            name: name.into(),
            input: json!({"path":"src/lib.rs"}),
        })
    }

    #[test]
    fn empty_and_whitespace_tool_names_are_sanitized() {
        let messages = [Message {
            role: "assistant".into(),
            content: vec![call("", "bad-empty"), call("   ", "bad-space")],
        }];
        let input = ChatGptPlan::input("system", &messages, &registered("read_file"));
        assert!(input.iter().all(|item| item["name"].as_str() != Some("")));
        assert!(!input.iter().any(|item| item["type"] == "function_call"));
    }

    #[test]
    fn valid_registered_tool_name_is_preserved() {
        let messages = [Message {
            role: "assistant".into(),
            content: vec![call(" read_file ", "call-1")],
        }];
        let input = ChatGptPlan::input("system", &messages, &registered("read_file"));
        assert_eq!(input[1]["name"], "read_file");
    }

    #[test]
    fn plain_text_does_not_gain_a_name_field() {
        let messages = [Message {
            role: "user".into(),
            content: vec![Content::Text("hii".into())],
        }];
        let input = ChatGptPlan::input("system", &messages, &registered("read_file"));
        assert!(input.iter().all(|item| item.get("name").is_none()));
    }

    #[test]
    fn orphaned_legacy_tool_result_is_dropped_with_bad_call() {
        let messages = [Message {
            role: "assistant".into(),
            content: vec![
                call("", "bad-call"),
                Content::Result {
                    id: "bad-call".into(),
                    output: "legacy output".into(),
                    is_error: false,
                },
            ],
        }];
        let input = ChatGptPlan::input("system", &messages, &registered("read_file"));
        assert!(input
            .iter()
            .all(|item| item["type"] != "function_call_output"));
    }

    #[test]
    fn payload_validation_rejects_any_empty_name() {
        let body = json!({
            "model":"test-model",
            "stream":true,
            "store":false,
            "input":[{"type":"function_call","name":"  "}],
            "tools":[]
        });
        let error = ChatGptPlan::validate_payload(&body)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid item index: 0"));
        assert!(error.contains("empty field: name"));
    }

    #[test]
    fn generated_payload_contains_no_empty_names() {
        let messages = [Message {
            role: "assistant".into(),
            content: vec![call("", "bad"), call("read_file", "good")],
        }];
        let tools = vec![
            json!({"name":"", "description":"bad", "input_schema":{}}),
            json!({"name":"read_file", "description":"good", "input_schema":{}}),
        ];
        let names = ChatGptPlan::registered_names(&tools);
        let body = json!({
            "model":"test-model",
            "stream":true,
            "store":false,
            "input": ChatGptPlan::input("system", &messages, &names),
            "tools": ChatGptPlan::tools(&tools)
        });
        assert!(ChatGptPlan::validate_payload(&body).is_ok());
        assert_eq!(body["input"][1]["name"], "read_file");
    }

    #[test]
    fn responses_ids_and_tool_name_are_kept_separate() {
        let mut calls = std::collections::BTreeMap::new();
        PendingFunctionCall::upsert(
            &mut calls,
            "fc_abc",
            "call_xyz",
            Some("shell"),
            None,
            Some(0),
        );
        PendingFunctionCall::append_arguments(&mut calls, "fc_abc", "", "{\"command\":");
        PendingFunctionCall::append_arguments(&mut calls, "fc_abc", "", "\"pwd\"}");
        let call = calls.into_values().next().unwrap();
        assert_eq!(call.item_id, "fc_abc");
        assert_eq!(call.call_id, "call_xyz");
        assert_eq!(call.name, "shell");
        assert_eq!(call.arguments, r#"{"command":"pwd"}"#);
    }

    #[test]
    fn multiple_response_function_calls_do_not_overwrite_each_other() {
        let mut calls = std::collections::BTreeMap::new();
        PendingFunctionCall::upsert(
            &mut calls,
            "fc_a",
            "call_a",
            Some("read_file"),
            Some(r#"{"path":"a"}"#),
            Some(0),
        );
        PendingFunctionCall::upsert(
            &mut calls,
            "fc_b",
            "call_b",
            Some("shell"),
            Some(r#"{"command":"pwd"}"#),
            Some(1),
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls["fc_a"].name, "read_file");
        assert_eq!(calls["fc_a"].call_id, "call_a");
        assert_eq!(calls["fc_b"].name, "shell");
        assert_eq!(calls["fc_b"].call_id, "call_b");
    }

    #[test]
    fn argument_delta_requires_a_stable_call_identifier() {
        let mut calls = std::collections::BTreeMap::new();
        assert!(PendingFunctionCall::append_arguments(&mut calls, "", "", "{}").is_none());
        assert!(calls.is_empty());
    }
}
