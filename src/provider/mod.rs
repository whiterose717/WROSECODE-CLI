pub mod anthropic;
pub mod chatgpt_plan;
pub mod openai_compat;

use anyhow::Context as _;
use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Models that reason silently for tens of seconds are healthy. Keep a bounded
/// inactivity ceiling to catch truly dead transports without cutting off high
/// effort generations (the old 30s ceiling caused false failures).
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// What an adapter can actually do, discovered rather than assumed. The agent
/// uses this for diagnostics and never to silently change a user's provider
/// semantics: a capability reported as absent simply is not attempted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub strict_tool_schema: bool,
    /// Provider-managed prefix cache (`cache_control` or automatic caching).
    pub prompt_cache: bool,
    pub context_limit: Option<u64>,
    /// The provider reports token usage instead of leaving us to estimate.
    pub usage_reporting: bool,
    pub structured_output: bool,
    pub reasoning_controls: bool,
    pub max_parallel_requests: Option<u32>,
}

impl ProviderCapabilities {
    /// Conservative capabilities shared by every adapter: streaming tool use
    /// with provider-reported usage, no prompt cache or parallel calls.
    pub fn baseline() -> Self {
        Self {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: false,
            strict_tool_schema: false,
            prompt_cache: false,
            context_limit: None,
            usage_reporting: true,
            structured_output: false,
            reasoning_controls: false,
            max_parallel_requests: None,
        }
    }

    pub fn cached(mut self) -> Self {
        self.prompt_cache = true;
        self
    }

    pub fn with_reasoning_controls(mut self) -> Self {
        self.reasoning_controls = true;
        self
    }
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self::baseline()
    }
}

/// Coarse provider failure taxonomy. Diagnostics only: retry policy keeps its
/// own deny-list so a classification change can never make an unsafe call
/// repeatable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProviderErrorKind {
    Auth,
    BadRequest,
    ModelNotFound,
    ContextLength,
    RateLimited,
    Overloaded,
    ConnectTimeout,
    ReadIdleTimeout,
    Timeout,
    Transport,
    Unknown,
}

impl ProviderErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "provider_auth",
            Self::BadRequest => "provider_bad_request",
            Self::ModelNotFound => "provider_model_not_found",
            Self::ContextLength => "provider_context_length",
            Self::RateLimited => "provider_rate_limited",
            Self::Overloaded => "provider_overloaded",
            Self::ConnectTimeout => "provider_connect_timeout",
            Self::ReadIdleTimeout => "provider_read_idle_timeout",
            Self::Timeout => "provider_timeout",
            Self::Transport => "provider_transport",
            Self::Unknown => "provider_unknown",
        }
    }
}

/// Classify a provider error message into the taxonomy above. Order matters:
/// authentication and context-limit failures must not be swallowed by the
/// generic timeout/transport branches.
pub fn classify_error(message: &str) -> ProviderErrorKind {
    let lower = message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if has(&[
        "bad or missing api key",
        "not signed in",
        "not authorized",
        "unauthorized",
        "invalid api key",
        "http 401",
        "http 403",
    ]) {
        ProviderErrorKind::Auth
    } else if has(&[
        "context length exceeded",
        "context window",
        "maximum context",
    ]) {
        ProviderErrorKind::ContextLength
    } else if has(&["model not found", "model does not exist", "unknown model"]) {
        ProviderErrorKind::ModelNotFound
    } else if has(&[
        "rate limited",
        "rate-limit",
        "rate_limit",
        "429",
        "too many requests",
    ]) {
        ProviderErrorKind::RateLimited
    } else if has(&["overloaded", "http 529", "http 503", "service unavailable"]) {
        ProviderErrorKind::Overloaded
    } else if has(&[
        "connect timeout",
        "connection timed out",
        "connection timeout",
    ]) {
        ProviderErrorKind::ConnectTimeout
    } else if has(&["read timeout", "idle timeout", "stalled", "stream idle"]) {
        ProviderErrorKind::ReadIdleTimeout
    } else if has(&["timed out", "timeout"]) {
        ProviderErrorKind::Timeout
    } else if has(&[
        "error sending request",
        "connection reset",
        "connection refused",
        "broken pipe",
        "dns",
        "network is unreachable",
        "transport",
    ]) {
        ProviderErrorKind::Transport
    } else if has(&[
        "http 400",
        "http 402",
        "http 405",
        "http 413",
        "http 422",
        "payload validation failed",
        "base url returned 404",
        "needs a base url",
        "unknown provider",
        "unsupported provider",
        "model is empty",
        "empty tool name",
        "unknown tool requested",
    ]) {
        ProviderErrorKind::BadRequest
    } else {
        ProviderErrorKind::Unknown
    }
}

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
    /// Transport activity that may not yet contain visible text (reasoning,
    /// keep-alives, tool JSON). Used by the TUI watchdog to distinguish a
    /// thinking model from a dead connection.
    Heartbeat,
    /// Explicit model transport phase, shown without adding transcript noise.
    ModelStage(String),
    ResetText,
    TextDelta(String),
    Usage(Usage),
    Metrics(crate::metrics::MetricsSnapshot),
    /// Full replacement of the visible plan checklist (text, done).
    Plan(Vec<(String, bool)>),
    FlagFound {
        flag: String,
        source: String,
    },
    /// The `auto` thinking controller changed the level — rendered in the
    /// transcript as `think: medium → high (no progress ×3)`.
    Think {
        from: String,
        to: String,
        to_level: u8,
        reason: String,
    },
    /// The provider rejected our thinking control (HTTP 400 on the thinking
    /// parameter); the turn continues without it and the status bar says so.
    ThinkIgnored(String),
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
    /// Provider output-item identity, when the protocol exposes one (for
    /// example OpenAI Responses `fc_...`). It is never used for dispatch.
    #[serde(default)]
    pub item_id: Option<String>,
    /// Provider invocation correlation ID used to return the tool result.
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Reserved input key that marks a tool call whose arguments could not be
/// decoded. [`crate::tools::Tools::execute`] turns it into a normal error
/// result without running anything, so one malformed emission neither aborts
/// the turn nor risks a partial side effect.
pub(crate) const ARG_PARSE_ERROR: &str = "__wrosecode_arg_parse_error";

/// Decode the JSON arguments of a tool call.
///
/// Models occasionally wrap arguments in a markdown fence, prefix them with
/// prose, or leave a trailing comma. Those are syntax-only defects — every
/// value is complete — so normalizing them is safe and keeps the call
/// executable. Truncated values are deliberately **not** repaired: closing a
/// cut-off string would invent data and could run a command the model never
/// finished writing. Those become [`ARG_PARSE_ERROR`] instead.
pub(crate) fn decode_tool_args(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    if let Ok(value) = serde_json::from_str(trimmed) {
        return value;
    }
    if let Some(value) = repair_complete_json(trimmed) {
        return value;
    }
    let mut error = serde_json::Map::new();
    error.insert(
        ARG_PARSE_ERROR.into(),
        Value::String(format!(
            "arguments were not valid JSON and the call was not executed; raw: {}",
            clip_snippet(trimmed)
        )),
    );
    Value::Object(error)
}

/// Repair only defects that leave every emitted value intact: surrounding
/// prose/fences, trailing commas, and containers whose contents are complete
/// but whose closing brackets were cut off. Returns `None` for anything that
/// would require inventing data (an unterminated string, a dangling key, or
/// a trailing comma right after an unfinished value).
fn repair_complete_json(raw: &str) -> Option<Value> {
    // Any JSON tool input is an object or array; skip prose before it. Try
    // each candidate opener so a stray brace in the prose cannot hide the
    // real payload.
    for (index, ch) in raw.char_indices() {
        if ch == '{' || ch == '[' {
            if let Some(value) = repair_from(&raw[index..]) {
                return Some(value);
            }
        }
    }
    None
}

fn repair_from(text: &str) -> Option<Value> {
    let mut out: Vec<char> = Vec::with_capacity(text.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    // Index in `out` just past the top-level value, when it finished before
    // the end of the text (trailing prose is dropped).
    let mut complete_at: Option<usize> = None;
    // Position of a comma followed only by whitespace so far.
    let mut pending_comma: Option<usize> = None;
    let mut last_significant = '\0';
    for ch in text.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                last_significant = '"';
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                pending_comma = None;
                out.push(ch);
            }
            '{' | '[' => {
                stack.push(ch);
                pending_comma = None;
                out.push(ch);
            }
            '}' | ']' => {
                let want = if ch == '}' { '{' } else { '[' };
                if stack.pop() != Some(want) {
                    return None;
                }
                if let Some(index) = pending_comma.take() {
                    out.remove(index);
                }
                out.push(ch);
                last_significant = ch;
                if stack.is_empty() {
                    complete_at = Some(out.len());
                    break;
                }
            }
            ',' => {
                pending_comma = Some(out.len());
                out.push(ch);
                last_significant = ',';
            }
            ':' => {
                pending_comma = None;
                out.push(ch);
                last_significant = ':';
            }
            c if c.is_whitespace() => out.push(c),
            c => {
                pending_comma = None;
                out.push(c);
                last_significant = c;
            }
        }
    }
    if in_string {
        // A cut-off string cannot be closed without inventing characters.
        return None;
    }
    if let Some(end) = complete_at {
        out.truncate(end);
    } else {
        // The container is still open. Only close it when the last emitted
        // token is a complete value; `,`/`:`/`{`/`[` mean data is missing.
        if stack.is_empty() || matches!(last_significant, '\0' | ',' | ':' | '{' | '[') {
            return None;
        }
        if let Some(index) = pending_comma.take() {
            out.remove(index);
        }
        for open in stack.iter().rev() {
            out.push(if *open == '{' { '}' } else { ']' });
        }
    }
    let repaired: String = out.into_iter().collect();
    serde_json::from_str(&repaired).ok()
}

/// Clip an argument snippet for an error message without dumping a whole
/// streamed payload into the transcript.
fn clip_snippet(text: &str) -> String {
    const LIMIT: usize = 240;
    let mut clipped: String = text.chars().take(LIMIT).collect();
    let total = text.chars().count();
    if total > LIMIT {
        clipped.push_str(&format!("… ({total} chars)"));
    }
    clipped
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

/// Boundary between the byte-stable prefix of a system prompt and its
/// volatile tail (think level, memory recall, repo map, matched skill).
/// The agent emits the marker; the Anthropic transport turns it into a
/// prompt-cache breakpoint (`cache_control` on the stable block, volatile
/// block after it) and every other transport splices it back into a single
/// text block. Prompt caches key on byte-identical prefixes, so everything
/// before this marker must stay constant for the life of the session.
pub const SYSTEM_VOLATILE_MARK: &str = "\n\u{1}wrose:volatile\u{1}\n";

#[async_trait]
pub trait Provider: Send + Sync {
    /// Declared adapter capabilities. Defaults are the conservative baseline;
    /// adapters override only what they can actually sustain.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::baseline()
    }

    /// Chat with an explicit thinking level (spec PHASE 4): concrete levels
    /// map to the provider's native control through the model's `think_map`,
    /// `Off` sends none. Implementations report a rejected control through
    /// [`Progress::ThinkIgnored`] and continue without it.
    async fn chat_stream_with_think(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&mpsc::UnboundedSender<Progress>>,
        think: crate::think::ThinkLevel,
    ) -> anyhow::Result<Response>;

    /// Thinking-free chat (probes, tests, and other trivial calls).
    async fn chat_stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&mpsc::UnboundedSender<Progress>>,
    ) -> anyhow::Result<Response> {
        self.chat_stream_with_think(
            system,
            messages,
            tools,
            require_tool,
            progress,
            crate::think::ThinkLevel::Off,
        )
        .await
    }
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
    /// The thinking-level variant the agent's run loop uses.
    async fn complete_with_think(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        require_tool: bool,
        progress: Option<&mpsc::UnboundedSender<Progress>>,
        think: crate::think::ThinkLevel,
    ) -> anyhow::Result<Response> {
        self.chat_stream_with_think(system, messages, tools, require_tool, progress, think)
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
        "openai_chatgpt_plan" | "openai-chatgpt-plan" | "chatgpt" => {
            let store = crate::chatgpt_auth::Store::current()?;
            // The profile may use the active account under a different label.
            // Keep that actual account label for later OAuth token rotation.
            let (account_label, account) = store
                .account_for_profile(&profile.name)?
                .context("ChatGPT is not signed in; run /auth login chatgpt")?;
            if !account.plan_enabled() {
                anyhow::bail!("ChatGPT plan usage is not authorized; run /auth login chatgpt")
            }
            Ok(Arc::new(chatgpt_plan::ChatGptPlan::new(
                client,
                model,
                account,
                store,
                &account_label,
            )))
        }
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
                think_map: profile.think_map.clone(),
            }))
        }
        "openai_compat" | "openai-compatible" | "openai" => {
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
                think_map: profile.think_map.clone(),
            }))
        }
        other => anyhow::bail!("unsupported provider kind: {other}"),
    }
}

/// The one tuned HTTP client shape every part of the app should use:
/// pooled connections plus explicit connect/request timeouts, so no call
/// site drifts back to an untimed `Client::new()`.
pub fn shared_client(connect_secs: u64, timeout_secs: u64) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .connect_timeout(Duration::from_secs(connect_secs))
        .read_timeout(Duration::from_secs(timeout_secs.max(5)))
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .context("build HTTP client")
}

/// The client for long-lived model streams: pooled connections and a
/// connect timeout, but deliberately NO total timeout. Reqwest's total
/// timeout covers the entire body, so it would abort legitimate long
/// generations mid-stream ("operation timed out" on a healthy request).
/// Stall detection stays per-chunk: every stream loop bounds each read
/// with its own inactivity timeout instead.
pub fn streaming_client(connect_secs: u64) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .connect_timeout(Duration::from_secs(connect_secs))
        // No total timeout: long generations are valid. Read timeout is an
        // inactivity timeout and resets whenever bytes arrive.
        .read_timeout(STREAM_IDLE_TIMEOUT + Duration::from_secs(5))
        .build()
        .context("build streaming HTTP client")
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
            // Providers commonly return a much longer, model-specific retry
            // window than our old fixed 200/400 ms delay. Honour it when
            // present, otherwise use a bounded exponential backoff. This
            // prevents the outer agent retry loop from hammering a throttled
            // endpoint and makes transient 429s recover without user action.
            tokio::time::sleep(retry_delay(response.headers(), attempt)).await;
            continue;
        }
        return Ok(response);
    }
    unreachable!()
}

/// ChatGPT-plan requests must inspect 429 bodies before retrying: a 429 can
/// be a documented plan usage limit rather than a transient transport fault.
/// Keep bounded retries for server failures without hiding that diagnostic.
pub async fn send_retry_server_errors(
    builder: reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::Response> {
    for attempt in 0..3 {
        let cloned = builder
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("request cannot be retried"))?;
        // Retrying a POST after reading a response is unsafe: the model may
        // have started a generation. A connection-establishment failure is
        // different—the request was not delivered. Retry that only once.
        let response = match cloned.send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() && attempt == 0 => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            Err(error) => return Err(anyhow::anyhow!(classify_network(&error))),
        };
        if response.status().is_server_error() && attempt < 2 {
            tokio::time::sleep(retry_delay(response.headers(), attempt)).await;
            continue;
        }
        return Ok(response);
    }
    unreachable!()
}

const MAX_PROVIDER_BACKOFF: Duration = Duration::from_secs(60);

fn retry_delay(headers: &HeaderMap, attempt: usize) -> Duration {
    let fallback = Duration::from_secs(1_u64 << attempt.min(5));
    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let reset_at = headers
        .get("x-ratelimit-reset-requests")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|epoch| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            epoch.checked_sub(now).map(Duration::from_secs)
        });
    retry_after
        .or(reset_at)
        .unwrap_or(fallback)
        .min(MAX_PROVIDER_BACKOFF)
}

#[cfg(test)]
mod retry_tests {
    use super::retry_delay;
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::time::Duration;

    #[test]
    fn retry_after_header_controls_provider_backoff() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("17"));
        assert_eq!(retry_delay(&headers, 0), Duration::from_secs(17));
    }

    #[test]
    fn missing_retry_after_uses_bounded_exponential_backoff() {
        let headers = HeaderMap::new();
        assert_eq!(retry_delay(&headers, 0), Duration::from_secs(1));
        assert_eq!(retry_delay(&headers, 1), Duration::from_secs(2));
        assert_eq!(retry_delay(&headers, 8), Duration::from_secs(32));
    }

    #[test]
    fn provider_backoff_is_capped() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("3600"));
        assert_eq!(retry_delay(&headers, 0), Duration::from_secs(60));
    }
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
    // Never stringify a raw request in user-facing diagnostics; URLs may
    // contain private query parameters. Structured categories preserve the
    // reason, so a user can distinguish connectivity from a bad payload.
    if error.is_connect() && error.is_timeout() {
        "provider connection timed out during DNS/TCP/TLS setup".into()
    } else if error.is_connect() {
        "cannot connect to provider (DNS/TCP/TLS setup failed)".into()
    } else if error.is_timeout() {
        "provider HTTP request or response timed out".into()
    } else if error.is_builder() {
        "provider HTTP request could not be constructed (configuration or URL)".into()
    } else if error.is_body() {
        "provider request body transfer failed".into()
    } else if error.is_decode() {
        "provider response decoding failed".into()
    } else if error.is_request() {
        "provider HTTP request transport/protocol failure".into()
    } else {
        "provider network request failed (unclassified transport failure)".into()
    }
}

/// Find the earliest SSE separator in one forward pass. Searching for LF
/// first rescans the entire remaining buffer for every CRLF-framed event,
/// making a batch of CRLF events quadratic even though draining is batched.
fn sse_separator(bytes: &[u8]) -> Option<(usize, usize)> {
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if index >= 1 && bytes[index - 1] == b'\n' {
            return Some((index - 1, 2));
        }
        if index >= 3 && &bytes[index - 3..index] == b"\r\n\r" {
            return Some((index - 3, 4));
        }
    }
    None
}

fn normalize_sse_event(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text.into_owned()
    }
}

/// Compatibility single-event parser for existing callers and fuzz fixtures.
#[cfg(test)]
pub fn take_sse_event(pending: &mut Vec<u8>) -> Option<String> {
    let (end, separator) = sse_separator(pending)?;
    let event = normalize_sse_event(&pending[..end]);
    pending.drain(..end + separator);
    Some(event)
}

/// Drain all complete SSE events with one buffer shift. Avoids O(n²) moves
/// when a provider delivers thousands of events in one network chunk.
/// The final partial event is kept for the next chunk.
pub fn take_sse_events(pending: &mut Vec<u8>) -> anyhow::Result<Vec<String>> {
    const MAX_INCOMPLETE_EVENT: usize = 4 * 1024 * 1024;
    let mut events = Vec::new();
    let mut consumed = 0;
    while let Some((end, separator)) = sse_separator(&pending[consumed..]) {
        let absolute_end = consumed + end;
        events.push(normalize_sse_event(&pending[consumed..absolute_end]));
        consumed = absolute_end + separator;
    }
    if consumed != 0 {
        pending.drain(..consumed);
    }
    if pending.len() > MAX_INCOMPLETE_EVENT {
        anyhow::bail!("provider SSE event exceeds 4 MiB without a delimiter");
    }
    Ok(events)
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
            display_name: String::new(),
            kind: kind.into(),
            base_url: base,
            model: "mock-model".into(),
            key_ref: String::new(),
            headers: BTreeMap::new(),
            think: None,
            think_map: None,
            builtin: false,
            auth_method: "api_key".into(),
            oauth_client_id: String::new(),
            oauth_issuer: String::new(),
            oauth_scope: String::new(),
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

#[cfg(test)]
mod sse_event_tests {
    use super::{sse_separator, take_sse_event, take_sse_events};

    #[test]
    fn drains_lf_terminated_events() {
        let mut pending = b"data: {\"i\":1}\n\ndata: {\"i\":2}\n\n".to_vec();
        assert_eq!(
            take_sse_event(&mut pending).as_deref(),
            Some("data: {\"i\":1}")
        );
        assert_eq!(
            take_sse_event(&mut pending).as_deref(),
            Some("data: {\"i\":2}")
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn drains_crlf_terminated_events() {
        let mut pending = b"data: x\r\n\r\n".to_vec();
        assert_eq!(take_sse_event(&mut pending).as_deref(), Some("data: x"));
        assert!(pending.is_empty());
    }

    #[test]
    fn crlf_before_a_later_lf_wins_the_earlier_separator() {
        let mut pending = b"data: a\r\n\r\ndata: b\n\n".to_vec();
        assert_eq!(take_sse_event(&mut pending).as_deref(), Some("data: a"));
        assert_eq!(take_sse_event(&mut pending).as_deref(), Some("data: b"));
        assert!(pending.is_empty());
    }

    #[test]
    fn an_incomplete_event_stays_buffered() {
        let mut pending = b"data: half".to_vec();
        assert_eq!(take_sse_event(&mut pending), None);
        assert_eq!(pending, b"data: half".to_vec());
        pending.extend_from_slice(b"\n\n");
        assert_eq!(take_sse_event(&mut pending).as_deref(), Some("data: half"));
        assert!(pending.is_empty());
    }

    #[test]
    fn a_boundary_overlapping_crlf_still_beats_the_lf() {
        // \r\n\r\n immediately followed by \n\n: the CRLF starts first and
        // must win even though it byte-overlaps the LF hit by three.
        let mut pending = b"\r\n\r\n\n\n".to_vec();
        assert_eq!(take_sse_event(&mut pending).as_deref(), Some(""));
        assert_eq!(take_sse_event(&mut pending).as_deref(), Some(""));
        assert!(pending.is_empty());
    }

    #[test]
    fn separator_matches_independent_reference_for_mixed_delimiters() {
        // Independent oracle: do not compare two parsers that both call
        // sse_separator, since they could share the same precedence bug.
        let mut rng = Rng::new(0xcafef00d);
        for _ in 0..2_000 {
            let bytes = fuzz_buffer(&mut rng, 256);
            let lf = bytes.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
            let crlf = bytes
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| (i, 4));
            let expected = match (lf, crlf) {
                (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
                (a, b) => a.or(b),
            };
            assert_eq!(sse_separator(&bytes), expected, "{bytes:?}");
        }
    }

    #[test]
    fn mixed_sse_frames_preserve_unicode_order_and_partial_tail() {
        let payload = "data: සිංහල\r\n\r\ndata: α\n\ndata: 😀\r\n\r\ndata: unfinished";
        for step in 1..=payload.len() {
            let mut pending = Vec::new();
            let mut actual = Vec::new();
            for chunk in payload.as_bytes().chunks(step) {
                pending.extend_from_slice(chunk);
                actual.extend(take_sse_events(&mut pending).unwrap());
            }
            assert_eq!(
                actual,
                ["data: සිංහල", "data: α", "data: 😀"],
                "chunk size {step}"
            );
            assert_eq!(pending, b"data: unfinished", "chunk size {step}");
        }
    }

    #[test]
    fn batched_sse_matches_single_event_parser_for_arbitrary_chunks() {
        let payload = b"data: a\r\n\r\ndata: b\n\ndata: c\r\n\r\ndata: partial";
        for step in 1..=payload.len() {
            let mut one = Vec::new();
            let mut batch = Vec::new();
            let mut expected = Vec::new();
            let mut actual = Vec::new();
            for chunk in payload.chunks(step) {
                one.extend_from_slice(chunk);
                batch.extend_from_slice(chunk);
                while let Some(event) = take_sse_event(&mut one) {
                    expected.push(event);
                }
                actual.extend(take_sse_events(&mut batch).unwrap());
            }
            assert_eq!(actual, expected, "chunk size {step}");
            assert_eq!(batch, one, "partial tail differs at {step}");
        }
    }

    #[test]
    fn oversized_unterminated_sse_event_is_bounded() {
        let mut pending = vec![b'x'; 4 * 1024 * 1024 + 1];
        let error = take_sse_events(&mut pending).unwrap_err();
        assert!(error.to_string().contains("4 MiB"));
    }

    /// Deterministic xorshift: the fuzz corpus is identical on every run, so
    /// a stream that ever misbehaves reproduces forever.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    /// Separator-heavy alphabet: random buffers frequently contain `\n\n`,
    /// `\r\n\r\n`, and boundary-overlapping mixes of the two.
    const FUZZ_ALPHABET: &[u8] = b"data: {\n\n\r\n,}01 x";

    fn fuzz_buffer(rng: &mut Rng, max_len: usize) -> Vec<u8> {
        (0..rng.below(max_len + 1))
            .map(|_| FUZZ_ALPHABET[rng.below(FUZZ_ALPHABET.len())])
            .collect()
    }

    fn feed(pending: &mut Vec<u8>, chunk: &[u8]) -> Vec<String> {
        pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(event) = take_sse_event(pending) {
            events.push(event);
        }
        events
    }

    #[test]
    fn fuzzed_streams_never_panic() {
        let mut rng = Rng::new(0x5eed);
        for case in 0..400 {
            let buffer = fuzz_buffer(&mut rng, 96);
            let mut whole = Vec::new();
            let whole_events = feed(&mut whole, &buffer);

            // Byte-by-byte delivery must produce the same event sequence and
            // leave the same incomplete remainder buffered.
            let mut incremental = Vec::new();
            let mut incremental_events = Vec::new();
            for chunk in buffer.chunks(1) {
                incremental_events.extend(feed(&mut incremental, chunk));
            }
            assert_eq!(
                incremental_events,
                whole_events,
                "byte delivery changed case {case}: {:?}",
                String::from_utf8_lossy(&buffer)
            );
            assert_eq!(
                incremental,
                whole,
                "byte delivery changed the remainder in case {case}: {:?}",
                String::from_utf8_lossy(&buffer)
            );
        }
    }

    #[test]
    fn any_chunking_delivers_the_same_events() {
        let mut rng = Rng::new(0xbeef);
        for _ in 0..150 {
            let buffer = fuzz_buffer(&mut rng, 40);
            let mut pending = Vec::new();
            let expected = feed(&mut pending, &buffer);

            // Every two-way split.
            for split in 0..=buffer.len() {
                let mut pending = Vec::new();
                let mut got = feed(&mut pending, &buffer[..split]);
                got.extend(feed(&mut pending, &buffer[split..]));
                assert_eq!(
                    got,
                    expected,
                    "split {split} of {:?}",
                    String::from_utf8_lossy(&buffer)
                );
            }

            // One random three-way partition.
            let first = rng.below(buffer.len() + 1);
            let second = rng.below(buffer.len() + 1);
            let (low, high) = if first <= second {
                (first, second)
            } else {
                (second, first)
            };
            let mut pending = Vec::new();
            let mut got = feed(&mut pending, &buffer[..low]);
            got.extend(feed(&mut pending, &buffer[low..high]));
            got.extend(feed(&mut pending, &buffer[high..]));
            assert_eq!(
                got,
                expected,
                "partition {low}/{high} of {:?}",
                String::from_utf8_lossy(&buffer)
            );
        }
    }
}

#[cfg(test)]
mod tool_arg_tests {
    use super::{decode_tool_args, ARG_PARSE_ERROR};
    use serde_json::json;

    #[test]
    fn valid_arguments_pass_through() {
        assert_eq!(
            decode_tool_args(r#"{"path":"a.txt","limit":5}"#),
            json!({"path": "a.txt", "limit": 5})
        );
        assert_eq!(decode_tool_args(""), json!({}));
    }

    #[test]
    fn syntax_only_repairs_keep_every_value() {
        // Markdown fence and prose wrapping.
        assert_eq!(
            decode_tool_args("```json\n{\"command\":\"ls\"}\n```"),
            json!({"command": "ls"})
        );
        assert_eq!(
            decode_tool_args("Here are the arguments: {\"command\":\"ls\"}"),
            json!({"command": "ls"})
        );
        // Trailing comma before a closer.
        assert_eq!(
            decode_tool_args(r#"{"path":"a.txt",}"#),
            json!({"path": "a.txt"})
        );
        // Complete values with only the closing brackets missing.
        assert_eq!(
            decode_tool_args(r#"{"path":"a.txt""#),
            json!({"path": "a.txt"})
        );
        assert_eq!(
            decode_tool_args(r#"{"command":"ls","timeout":5"#),
            json!({"command": "ls", "timeout": 5})
        );
        // Nested containers close in the right order.
        assert_eq!(
            decode_tool_args(r#"{"paths":["a","b"],"count":2"#),
            json!({"paths": ["a", "b"], "count": 2})
        );
        // A brace in the prose must not hide the real payload.
        assert_eq!(
            decode_tool_args("prefix {not json} then {\"path\":\"a\"}"),
            json!({"path": "a"})
        );
        // Trailing prose after a complete object is dropped.
        assert_eq!(
            decode_tool_args(r#"{"path":"a"} and nothing else"#),
            json!({"path": "a"})
        );
    }

    #[test]
    fn truncated_values_are_never_invented() {
        for raw in [
            // Cut off inside a string: closing it would fabricate content.
            r#"{"command":"rm -rf /tmp/x"#,
            r#"{"path":"a.txt\#,
            // Cut off after a key or separator: a value is missing.
            r#"{"command":"ls","timeout":"#,
            r#"{"command":"ls","#,
            r#"{"path":"a","content":{"#,
            // Not JSON at all.
            "not json",
            "",
        ] {
            let value = decode_tool_args(raw);
            if raw.is_empty() {
                assert_eq!(value, json!({}), "empty arguments mean no options");
            } else {
                assert!(
                    value.get(ARG_PARSE_ERROR).is_some(),
                    "raw {raw:?} must surface a parse error, got {value}"
                );
            }
        }
    }
}
