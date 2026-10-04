//! OAuth sign-in as an alternative to pasting an API key (Codex-style
//! "sign in with ChatGPT" alongside the plain key path).
//!
//! Two flows, both ending in tokens stored at
//! `~/.wrosecode/credentials/<provider>.json` with `0600` permissions:
//! - browser + short-lived loopback callback (authorization code + PKCE
//!   S256, with a state check and a manual paste fallback);
//! - device-code flow for headless/remote setups where no browser can reach
//!   a local callback.
//!
//! The issuer, client ID, and scope are operator configuration — passed via
//! the provider profile, `--oauth-*` flags, or `WROSECODE_OAUTH_*` env vars
//! — never bundled first-party credentials. Nothing here logs tokens; error
//! strings go through the credential redactor.

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default issuer for the OpenAI-flavoured flow; override per provider.
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";
/// Loopback callback port both the listener and the redirect URI use.
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
/// How long a browser flow waits for the callback before giving up.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything needed to run either flow. `client_id` must come from the
/// operator's own OAuth client registration.
#[derive(Clone, Debug)]
pub struct OAuthConfig {
    pub issuer: String,
    pub client_id: String,
    pub scope: String,
    pub callback_port: u16,
}

impl OAuthConfig {
    pub fn new(client_id: &str, issuer: &str, scope: &str) -> Self {
        let issuer = issuer.trim().trim_end_matches('/').to_string();
        Self {
            issuer: if issuer.is_empty() {
                DEFAULT_ISSUER.into()
            } else {
                issuer
            },
            client_id: client_id.trim().to_string(),
            scope: scope.trim().to_string(),
            callback_port: DEFAULT_CALLBACK_PORT,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.client_id.is_empty() {
            anyhow::bail!(
                "OAuth needs a client ID from your own OAuth client registration \
                 (provider `oauth_client_id`, --oauth-client-id, or WROSECODE_OAUTH_CLIENT_ID)"
            );
        }
        Ok(())
    }

    fn endpoint(&self, path: &str) -> Result<url::Url> {
        let base: url::Url = self.issuer.parse().context("invalid OAuth issuer")?;
        base.join(path).context("bad OAuth endpoint")
    }

    pub fn authorize_endpoint(&self) -> Result<url::Url> {
        self.endpoint("oauth/authorize")
    }

    pub fn token_endpoint(&self) -> Result<url::Url> {
        self.endpoint("oauth/token")
    }

    pub fn device_endpoint(&self) -> Result<url::Url> {
        self.endpoint("oauth/device/code")
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/auth/callback", self.callback_port)
    }
}

/// Tokens from a completed flow. `expires_at` is unix seconds, `0` when the
/// issuer did not say.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl OAuthTokens {
    /// Expired, or expiring within the grace window so callers refresh
    /// before the first 401 instead of after it.
    pub fn expired(&self) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        now_secs() + 300 >= self.expires_at
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// PKCE verifier + S256 challenge, both base64url without padding.
pub fn pkce() -> Result<(String, String)> {
    use rand::TryRng as _;
    let mut raw = [0u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut raw)
        .map_err(|error| anyhow::anyhow!("randomness failed: {error}"))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref());
    Ok((verifier, challenge))
}

pub fn random_state() -> Result<String> {
    use rand::TryRng as _;
    let mut raw = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut raw)
        .map_err(|error| anyhow::anyhow!("randomness failed: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw))
}

/// The browser URL that starts the loopback flow.
pub fn authorize_url(cfg: &OAuthConfig, challenge: &str, state: &str) -> Result<String> {
    cfg.validate()?;
    let mut url = cfg.authorize_endpoint()?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &cfg.redirect_uri())
        .append_pair("scope", &cfg.scope)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

/// Open the system browser, best-effort: callers always print the URL too,
/// so a missing opener degrades to copy-paste instead of failing.
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "cmd";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let program = "xdg-open";
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new(program)
        .args(["/C", "start", "", url])
        .status()?;
    #[cfg(not(target_os = "windows"))]
    let status = std::process::Command::new(program).arg(url).status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("could not open a browser (copy the URL instead)")
    }
}

/// Wait for the provider's redirect to `GET /auth/callback?code=…&state=…`.
/// A state mismatch or provider error is refused; the server lives only for
/// this call and is aborted on timeout.
pub async fn await_callback(port: u16, expected_state: &str, timeout: Duration) -> Result<String> {
    use axum::{extract::Query, response::Html, routing::get, Router};
    use tokio::sync::oneshot;

    let (tx, rx) = oneshot::channel::<Result<String>>();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    let expected = expected_state.to_string();
    let app = Router::new().route(
        "/auth/callback",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let shared = std::sync::Arc::clone(&shared);
            let expected = expected.clone();
            async move {
                let result = match params.get("error").cloned() {
                    Some(error) => Err(anyhow::anyhow!(
                        "provider refused sign-in: {}",
                        crate::crash::redact(&error)
                    )),
                    None => match params.get("code").cloned() {
                        None => Err(anyhow::anyhow!("callback carried no code")),
                        Some(code) => {
                            if params.get("state").map(String::as_str).unwrap_or("") == expected {
                                Ok(code)
                            } else {
                                Err(anyhow::anyhow!(
                                    "callback state mismatch (possible CSRF): sign-in aborted"
                                ))
                            }
                        }
                    },
                };
                if let Some(tx) = shared
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take())
                {
                    let _ = tx.send(result);
                }
                Html(
                    "<html><body><h1>Signed in with ChatGPT.</h1><p>You may close this window and return to the terminal.</p></body></html>",
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "cannot listen on 127.0.0.1:{port} ({error}); use the device-code flow when the loopback callback is blocked"
            )
        })?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });
    let outcome = tokio::time::timeout(timeout, rx)
        .await
        .map_err(|_| anyhow::anyhow!("sign-in timed out waiting for the browser callback"));
    server.abort();
    match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => anyhow::bail!("sign-in listener died before the callback arrived"),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

fn tokens_from(reply: TokenReply) -> OAuthTokens {
    let expires_at = reply
        .expires_in
        .map(|secs| now_secs().saturating_add(secs))
        .unwrap_or(0);
    // Best-effort account id from the ID token payload (never fatal).
    let account_id = reply.account_id.or_else(|| {
        reply.id_token.as_deref().and_then(|token| {
            let payload = token.split('.').nth(1)?;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            value
                .get("chatgpt_account_id")
                .or_else(|| value.get("sub"))
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
    });
    OAuthTokens {
        access_token: reply.access_token,
        refresh_token: reply.refresh_token,
        token_type: reply.token_type.unwrap_or_else(|| "Bearer".into()),
        expires_at,
        account_id,
    }
}

async fn post_token(
    client: &reqwest::Client,
    cfg: &OAuthConfig,
    form: &[(&str, &str)],
) -> Result<OAuthTokens> {
    let endpoint = cfg.token_endpoint()?;
    let response = client.post(endpoint).form(form).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "token endpoint refused the exchange ({status}): {}",
            crate::crash::redact(&body.chars().take(300).collect::<String>())
        );
    }
    let reply: TokenReply = response.json().await?;
    if reply.access_token.is_empty() {
        anyhow::bail!("token endpoint returned no access token");
    }
    Ok(tokens_from(reply))
}

/// Exchange a loopback-flow `code` for tokens.
pub async fn exchange_code(
    client: &reqwest::Client,
    cfg: &OAuthConfig,
    code: &str,
    verifier: &str,
) -> Result<OAuthTokens> {
    cfg.validate()?;
    post_token(
        client,
        cfg,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &cfg.redirect_uri()),
            ("client_id", &cfg.client_id),
            ("code_verifier", verifier),
        ],
    )
    .await
}

/// Refresh an access token (proactive: call when [`OAuthTokens::expired`]).
pub async fn refresh(
    client: &reqwest::Client,
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    cfg.validate()?;
    post_token(
        client,
        cfg,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &cfg.client_id),
        ],
    )
    .await
}

/// A device-code grant in flight: show `url` + `user_code` to the user,
/// then [`await_device`] polls until they approve (or it expires).
pub struct DevicePending {
    pub url: String,
    pub user_code: String,
    device_code: String,
    interval: Duration,
    expires_at: Instant,
    config: OAuthConfig,
}

#[derive(Debug, Deserialize)]
struct DeviceReply {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

/// Start a device-code grant.
pub async fn start_device(client: &reqwest::Client, cfg: &OAuthConfig) -> Result<DevicePending> {
    cfg.validate()?;
    let endpoint = cfg.device_endpoint()?;
    let response = client
        .post(endpoint)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("scope", cfg.scope.as_str()),
        ])
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("device endpoint refused ({})", response.status());
    }
    let reply: DeviceReply = response.json().await?;
    let url = reply
        .verification_uri_complete
        .or(reply.verification_uri)
        .context("device endpoint returned no verification URL")?;
    Ok(DevicePending {
        url,
        user_code: reply.user_code,
        device_code: reply.device_code,
        interval: Duration::from_secs(reply.interval.unwrap_or(5).max(1)),
        expires_at: Instant::now() + Duration::from_secs(reply.expires_in.unwrap_or(900)),
        config: cfg.clone(),
    })
}

/// Poll until the user approves the device grant (or it expires).
pub async fn await_device(
    client: &reqwest::Client,
    pending: &DevicePending,
) -> Result<OAuthTokens> {
    let mut interval = pending.interval;
    loop {
        if Instant::now() >= pending.expires_at {
            anyhow::bail!("device grant expired before approval");
        }
        tokio::time::sleep(interval).await;
        let endpoint = pending.config.token_endpoint()?;
        let response = client
            .post(endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", pending.device_code.as_str()),
                ("client_id", pending.config.client_id.as_str()),
            ])
            .send()
            .await?;
        if response.status().is_success() {
            let reply: TokenReply = response.json().await?;
            if reply.access_token.is_empty() {
                anyhow::bail!("token endpoint returned no access token");
            }
            return Ok(tokens_from(reply));
        }
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        match body.get("error").and_then(|error| error.as_str()) {
            // Still waiting: keep polling.
            Some("authorization_pending") => {}
            // Back off when asked.
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("expired_token") => anyhow::bail!("device grant expired before approval"),
            Some("access_denied") => anyhow::bail!("sign-in was denied in the browser"),
            Some(other) => anyhow::bail!("device grant failed: {other}"),
            None => anyhow::bail!("device grant failed (status {status})"),
        }
    }
}

/// `~/.wrosecode/credentials/<name>.json`.
pub fn credential_path(home: &Path, name: &str) -> PathBuf {
    home.join(".wrosecode")
        .join("credentials")
        .join(format!("{name}.json"))
}

/// Persist tokens with owner-only permissions and register both values for
/// redaction, so a later transcript or report can never echo them.
pub fn store_tokens(home: &Path, name: &str, tokens: &OAuthTokens) -> Result<PathBuf> {
    let path = credential_path(home, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        serde_json::to_writer_pretty(file, tokens)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, serde_json::to_vec_pretty(tokens)?)?;
    }
    crate::crash::register_secret(&tokens.access_token);
    if let Some(refresh) = &tokens.refresh_token {
        crate::crash::register_secret(refresh);
    }
    Ok(path)
}

/// Load stored tokens, if any.
pub fn load_tokens(home: &Path, name: &str) -> Option<OAuthTokens> {
    let bytes = std::fs::read(credential_path(home, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Pull an authorization code out of pasted input: either the full
/// redirect URL the browser landed on, or the bare code itself. Used when
/// the browser cannot reach the loopback listener (SSH, containers).
pub fn code_from_pasted(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Ok(url) = input.parse::<url::Url>() {
        if let Some(code) = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
        {
            if !code.is_empty() {
                return Some(code);
            }
        }
    }
    Some(input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_pkce_state_and_redirect() {
        let cfg = OAuthConfig::new("client-123", "https://auth.example.test", "codex");
        let url = authorize_url(&cfg, "challenge-abc", "state-xyz").unwrap();
        assert!(url.contains("auth.example.test/oauth/authorize"), "{url}");
        assert!(url.contains("code_challenge=challenge-abc"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("state=state-xyz"), "{url}");
        assert!(url.contains("127.0.0.1%3A1455"), "{url}");
    }

    #[test]
    fn blank_config_fails_validation() {
        let cfg = OAuthConfig::new("", "", "");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn pasted_redirects_yield_the_code() {
        assert_eq!(
            code_from_pasted("http://127.0.0.1:1455/auth/callback?code=abc123&state=x"),
            Some("abc123".into())
        );
        assert_eq!(
            code_from_pasted("  bare-code-9  "),
            Some("bare-code-9".into())
        );
        assert_eq!(code_from_pasted("   "), None);
    }

    #[test]
    fn expiry_uses_a_grace_window() {
        let fresh = OAuthTokens {
            expires_at: now_secs() + 3_600,
            ..OAuthTokens::default()
        };
        assert!(!fresh.expired());
        let stale = OAuthTokens {
            expires_at: now_secs() + 60,
            ..OAuthTokens::default()
        };
        assert!(stale.expired());
        assert!(!OAuthTokens::default().expired());
    }

    #[test]
    fn tokens_store_owner_only_and_redact() {
        let dir = std::env::temp_dir().join(format!("wrose-oauth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tokens = OAuthTokens {
            access_token: "oauth-access-test-secret".into(),
            refresh_token: Some("oauth-refresh-test-secret".into()),
            token_type: "Bearer".into(),
            expires_at: now_secs() + 100,
            account_id: None,
        };
        let path = store_tokens(&dir, "openai", &tokens).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let loaded = load_tokens(&dir, "openai").expect("round trip");
        assert_eq!(loaded.access_token, tokens.access_token);
        assert!(!crate::crash::redact("bearer oauth-access-test-secret")
            .contains("oauth-access-test-secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn loopback_callback_accepts_matching_state() {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let state = random_state().unwrap();
        let url = format!("http://127.0.0.1:{port}/auth/callback?code=code-1&state={state}");
        let (result, _) = tokio::join!(
            await_callback(port, &state, Duration::from_secs(10)),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = reqwest::get(url).await;
            }
        );
        assert_eq!(result.unwrap(), "code-1");
    }

    #[tokio::test]
    async fn loopback_callback_rejects_state_mismatch() {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let url = format!("http://127.0.0.1:{port}/auth/callback?code=code-1&state=wrong");
        let state = random_state().unwrap();
        let (result, _) = tokio::join!(
            await_callback(port, &state, Duration::from_secs(10)),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = reqwest::get(url).await;
            }
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn device_flow_completes_against_a_mock_issuer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server_base = base.clone();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let base = server_base;
            // Device-code grant, then one pending poll, then tokens.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            let body = serde_json::json!({
                "device_code": "dev-1",
                "user_code": "ABCD-EFGH",
                "verification_uri": format!("{base}/verify"),
                "expires_in": 60,
                "interval": 0,
            });
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.to_string().len(),
                body
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            let pending = r#"{"error":"authorization_pending"}"#;
            let reply = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                pending.len(),
                pending
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await.unwrap();
            let tokens = r#"{"access_token":"mock-access","refresh_token":"mock-refresh","expires_in":3600}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                tokens.len(),
                tokens
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let cfg = OAuthConfig {
            issuer: base.clone(),
            client_id: "test-client".into(),
            scope: "codex".into(),
            callback_port: 1,
        };
        let pending = start_device(&client, &cfg).await.unwrap();
        assert_eq!(pending.user_code, "ABCD-EFGH");
        let tokens = await_device(&client, &pending).await.unwrap();
        assert_eq!(tokens.access_token, "mock-access");
        assert!(!tokens.expired());
        server.await.unwrap();
    }
}
