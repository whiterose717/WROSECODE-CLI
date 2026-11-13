//! Documented Sign in with ChatGPT flow for local open-source agents.
//! This store and its credentials are independent from API Platform keys.
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use rand::TryRng as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub client_id: String,
    pub subject: String,
    pub email: String,
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub scopes: Vec<String>,
    pub expires_at: u64,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub ext_agent_host_id: String,
}

impl Credentials {
    pub fn plan_enabled(&self) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope == "chatgpt.tokens.use.direct")
            && self.scopes.iter().any(|scope| scope == "resource.invoke")
    }
    pub fn expires_soon(&self) -> bool {
        now().saturating_add(300) >= self.expires_at
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    active: Option<String>,
    accounts: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn current() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(Self {
            dir: PathBuf::from(home).join(".wrosecode/chatgpt"),
        })
    }
    fn index(&self) -> Result<Index> {
        match std::fs::read(self.dir.join("accounts.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("ChatGPT accounts are corrupted"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(error) => Err(error.into()),
        }
    }
    fn credential_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
    pub fn accounts(&self) -> Result<Vec<(String, bool, Option<Credentials>)>> {
        let index = self.index()?;
        index
            .accounts
            .iter()
            .map(|(label, id)| {
                let credentials = self.load_id(id)?;
                Ok((
                    label.clone(),
                    index.active.as_deref() == Some(label),
                    credentials,
                ))
            })
            .collect()
    }
    fn load_id(&self, id: &str) -> Result<Option<Credentials>> {
        match std::fs::read(self.credential_path(id)) {
            Ok(bytes) => {
                let value: Credentials = serde_json::from_slice(&bytes)
                    .context("ChatGPT credentials are corrupted; sign in again")?;
                if value.access_token.is_empty() || value.client_id.is_empty() {
                    bail!("ChatGPT credentials are incomplete; sign in again");
                }
                crate::crash::register_secret(&value.access_token);
                crate::crash::register_secret(&value.refresh_token);
                crate::crash::register_secret(&value.id_token);
                Ok(Some(value))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn active(&self) -> Result<Option<(String, Credentials)>> {
        let index = self.index()?;
        let Some(label) = index.active else {
            return Ok(None);
        };
        let Some(id) = index.accounts.get(&label) else {
            return Ok(None);
        };
        Ok(self.load_id(id)?.map(|credentials| (label, credentials)))
    }
    pub fn account(&self, label: &str) -> Result<Option<Credentials>> {
        let index = self.index()?;
        match index.accounts.get(label) {
            Some(id) => self.load_id(id),
            None => Ok(None),
        }
    }

    /// Provider profiles can use a different name from the active ChatGPT
    /// account. Return the actual credential-store label to allow future
    /// refreshes to use the right account without corrupting account state.
    pub fn account_for_profile(&self, profile: &str) -> Result<Option<(String, Credentials)>> {
        if let Some(credentials) = self.account(profile)? {
            return Ok(Some((profile.to_owned(), credentials)));
        }
        self.active()
    }

    #[cfg(test)]
    pub(crate) fn test_in(dir: PathBuf) -> Self {
        Self { dir }
    }
    pub fn host_id(&self) -> Result<String> {
        let path = self.dir.join("host-id");
        if let Ok(value) = std::fs::read_to_string(&path) {
            let value = value.trim().to_string();
            if value.starts_with("urn:uuid:") {
                return Ok(value);
            }
            bail!("ChatGPT host ID is invalid");
        }
        let mut bytes = [0_u8; 16];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .context("secure random source unavailable")?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let raw: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let id = format!(
            "urn:uuid:{}-{}-{}-{}-{}",
            &raw[..8],
            &raw[8..12],
            &raw[12..16],
            &raw[16..20],
            &raw[20..]
        );
        std::fs::create_dir_all(&self.dir)?;
        write_private(&path, id.as_bytes())?;
        Ok(id)
    }
    pub fn save(&self, label: &str, credentials: &Credentials) -> Result<()> {
        crate::settings::Settings::validate_name(label)?;
        let mut index = self.index()?;
        let id = index.accounts.get(label).cloned().unwrap_or_else(|| {
            let digest = ring::digest::digest(
                &ring::digest::SHA256,
                format!("{}:{}", credentials.client_id, credentials.subject).as_bytes(),
            );
            format!(
                "account-{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest.as_ref()[..12])
            )
        });
        write_private(
            &self.credential_path(&id),
            &serde_json::to_vec(credentials)?,
        )?;
        index.accounts.insert(label.into(), id);
        index.active = Some(label.into());
        write_private(
            &self.dir.join("accounts.json"),
            &serde_json::to_vec(&index)?,
        )?;
        crate::crash::register_secret(&credentials.access_token);
        crate::crash::register_secret(&credentials.refresh_token);
        crate::crash::register_secret(&credentials.id_token);
        Ok(())
    }
    pub fn switch(&self, label: &str) -> Result<()> {
        let mut index = self.index()?;
        if !index.accounts.contains_key(label) {
            bail!("unknown ChatGPT account: {label}")
        }
        index.active = Some(label.into());
        write_private(
            &self.dir.join("accounts.json"),
            &serde_json::to_vec(&index)?,
        )
    }
    pub fn clear_tokens(&self, label: &str) -> Result<()> {
        let mut index = self.index()?;
        if let Some(id) = index.accounts.get(label) {
            let path = self.credential_path(id);
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        if index.active.as_deref() == Some(label) {
            index.active = None;
        }
        write_private(
            &self.dir.join("accounts.json"),
            &serde_json::to_vec(&index)?,
        )
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("invalid credentials path")?)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, path)?;
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug)]
struct Callback {
    code: String,
    client_id: Option<String>,
    scope: Option<String>,
}

pub struct Pending {
    pub url: String,
    verifier: String,
    nonce: String,
    redirect_uri: String,
    previous: Option<Credentials>,
    receiver: oneshot::Receiver<Result<Callback>>,
    cancel: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

pub async fn begin(store: &Store, previous: Option<Credentials>) -> Result<Pending> {
    let host_id = store.host_id()?;
    let (verifier, challenge) = crate::oauth::pkce()?;
    let state = crate::oauth::random_state()?;
    let nonce = crate::oauth::random_state()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
    let client_id = previous
        .as_ref()
        .map_or(DYNAMIC_CLIENT, |p| p.client_id.as_str());
    let mut url = url::Url::parse("https://auth.openai.com/api/accounts/authorize")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge)
            .append_pair("ext_agent_host_id", &host_id);
        if let Some(existing) = &previous {
            query.append_pair("id_token_hint", &existing.id_token);
            if !existing.email.is_empty() {
                query.append_pair("login_hint", &existing.email);
            }
        } else {
            query.append_pair("agent_name_hint", "WROSECODE");
        }
    }
    let (tx, receiver) = oneshot::channel();
    let (cancel, cancel_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        use axum::{extract::Query, response::Html, routing::get, Router};
        use std::{
            collections::HashMap,
            sync::{Arc, Mutex},
        };
        let shared = Arc::new(Mutex::new(Some(tx)));
        let app = Router::new().route(
            "/auth/callback",
            get(move |Query(params): Query<HashMap<String, String>>| {
                let shared = Arc::clone(&shared);
                let state = state.clone();
                async move {
                    let result = if params.get("state") != Some(&state) {
                        Err(anyhow::anyhow!("OAuth callback state mismatch"))
                    } else if params.contains_key("error") {
                        Err(anyhow::anyhow!("ChatGPT authorization declined or expired"))
                    } else {
                        params
                            .get("code")
                            .filter(|code| !code.is_empty())
                            .map(|code| Callback {
                                code: code.clone(),
                                client_id: params.get("client_id").cloned(),
                                scope: params.get("scope").cloned(),
                            })
                            .context("OAuth callback did not contain an authorization code")
                    };
                    let ok = result.is_ok();
                    if let Ok(mut slot) = shared.lock() {
                        if let Some(tx) = slot.take() {
                            let _ = tx.send(result);
                        }
                    }
                    Html(if ok {
                        "<h1>WROSECODE</h1><p>Authentication successful.</p><p>You can return to your terminal. This window may be closed.</p>"
                    } else {
                        "<h1>WROSECODE</h1><p>Authentication could not be completed.</p><p>Return to your terminal for details.</p>"
                    })
                }
            }),
        );
        tokio::select! {
            _ = axum::serve(listener, app) => {}
            _ = cancel_rx => {}
        }
    });
    Ok(Pending {
        url: url.into(),
        verifier,
        nonce,
        redirect_uri,
        previous,
        receiver,
        cancel,
        server,
    })
}

pub async fn finish(
    store: &Store,
    label: &str,
    pending: Pending,
    client: &reqwest::Client,
) -> Result<Credentials> {
    let Pending {
        verifier,
        nonce,
        redirect_uri,
        previous,
        receiver,
        cancel,
        server,
        ..
    } = pending;
    let callback = tokio::time::timeout(Duration::from_secs(300), receiver)
        .await
        .context("ChatGPT sign-in timed out")?
        .context("OAuth callback stopped")??;
    server.abort();
    drop(cancel);
    let client_id = match previous.as_ref() {
        Some(existing) => {
            if callback
                .client_id
                .as_deref()
                .is_some_and(|id| id != existing.client_id)
            {
                bail!("OAuth callback changed the registered client ID");
            }
            existing.client_id.clone()
        }
        None => callback
            .client_id
            .context("OpenAI did not return an issued client ID")?,
    };
    if client_id == DYNAMIC_CLIENT || client_id.is_empty() {
        bail!("invalid issued client ID")
    }
    let response = client
        .post("https://auth.openai.com/api/accounts/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", callback.code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .context("ChatGPT code exchange failed")?;
    if !response.status().is_success() {
        bail!("ChatGPT code exchange failed ({})", response.status())
    }
    let reply: Value = response
        .json()
        .await
        .context("invalid ChatGPT token response")?;
    let id_token = reply["id_token"]
        .as_str()
        .context("OpenAI did not return an ID token")?;
    let identity = verify_id_token(client, id_token, &client_id, &nonce).await?;
    let subject = identity["sub"]
        .as_str()
        .context("ID token has no subject")?;
    if previous.as_ref().is_some_and(|old| old.subject != subject) {
        bail!("ChatGPT account changed during reauthorization")
    }
    let scopes = reply["scope"]
        .as_str()
        .or(callback.scope.as_deref())
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let credentials = Credentials {
        client_id,
        subject: subject.into(),
        email: identity["email"].as_str().unwrap_or("").into(),
        id_token: id_token.into(),
        access_token: reply["access_token"]
            .as_str()
            .context("missing access token")?
            .into(),
        refresh_token: reply["refresh_token"].as_str().unwrap_or("").into(),
        scopes,
        expires_at: now().saturating_add(reply["expires_in"].as_u64().unwrap_or(0)),
        issuer: ISSUER.into(),
        ext_agent_host_id: store.host_id()?,
    };
    store.save(label, &credentials)?;
    Ok(credentials)
}

async fn verify_id_token(
    client: &reqwest::Client,
    jwt: &str,
    audience: &str,
    nonce: &str,
) -> Result<Value> {
    let mut parts = jwt.split('.');
    let (header, payload, signature) = (
        parts.next().context("invalid ID token")?,
        parts.next().context("invalid ID token")?,
        parts.next().context("invalid ID token")?,
    );
    if parts.next().is_some() {
        bail!("invalid ID token")
    }
    let decoder = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header: Value = serde_json::from_slice(&decoder.decode(header)?)?;
    if header["alg"] != "RS256" {
        bail!("unsupported ID token signature algorithm")
    }
    let kid = header["kid"].as_str().context("ID token has no key ID")?;
    let keys: Value = client
        .get("https://auth.openai.com/.well-known/jwks.json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let key = keys["keys"]
        .as_array()
        .and_then(|keys| {
            keys.iter()
                .find(|key| key["kid"] == kid && key["kty"] == "RSA")
        })
        .context("ID token signing key was not published")?;
    let n = decoder.decode(key["n"].as_str().context("invalid RSA modulus")?)?;
    let e = decoder.decode(key["e"].as_str().context("invalid RSA exponent")?)?;
    let signed = jwt.rsplit_once('.').context("invalid ID token")?.0;
    ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &decoder.decode(signature)?,
        )
        .map_err(|_| anyhow::anyhow!("ID token signature verification failed"))?;
    let claims: Value = serde_json::from_slice(&decoder.decode(payload)?)?;
    validate_claims(&claims, audience, nonce, now())?;
    Ok(claims)
}

const CLOCK_SKEW_SECS: u64 = 60;

fn validate_claims(claims: &Value, audience: &str, nonce: &str, now: u64) -> Result<()> {
    let issuer = claims["iss"]
        .as_str()
        .context("ID token issuer is missing")?;
    if issuer != ISSUER {
        auth_debug(format!(
            "issuer: INVALID (actual {issuer:?}, expected {ISSUER:?})"
        ));
        bail!("ID token issuer mismatch (expected {ISSUER})");
    }
    let audience_ok = claims["aud"].as_str() == Some(audience)
        || claims["aud"]
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(audience)));
    if !audience_ok {
        auth_debug(format!(
            "audience: INVALID (actual {}, expected {audience})",
            safe_json(&claims["aud"])
        ));
        bail!("ID token audience mismatch");
    }
    if claims["nonce"].as_str() != Some(nonce) {
        auth_debug("nonce: INVALID".into());
        bail!("ID token nonce mismatch; restart ChatGPT authentication");
    }
    let exp = claims["exp"]
        .as_u64()
        .context("ID token expiration is missing")?;
    if exp.saturating_add(CLOCK_SKEW_SECS) < now {
        auth_debug(format!("lifetime: EXPIRED (exp {exp}, now {now})"));
        bail!("ID token expired; check your system clock and restart authentication");
    }
    if let Some(nbf) = claims["nbf"].as_u64() {
        if nbf > now.saturating_add(CLOCK_SKEW_SECS) {
            auth_debug(format!("lifetime: NOT-YET-VALID (nbf {nbf}, now {now})"));
            bail!("ID token is not valid yet; check your system clock");
        }
    }
    if let Some(iat) = claims["iat"].as_u64() {
        if iat > now.saturating_add(CLOCK_SKEW_SECS) {
            auth_debug(format!("lifetime: ISSUED-IN-FUTURE (iat {iat}, now {now})"));
            bail!("ID token issued-at time is in the future; check your system clock");
        }
    }
    auth_debug(format!(
        "issuer: valid; audience: valid; nonce: valid; exp: {exp}; now: {now}"
    ));
    Ok(())
}

fn safe_json(value: &Value) -> String {
    match value {
        Value::String(value) => format!("{value:?}"),
        Value::Array(values) => format!("array(len={})", values.len()),
        _ => "missing-or-invalid".into(),
    }
}

fn auth_debug(message: String) {
    if std::env::var_os("WROSECODE_AUTH_DEBUG").is_some() {
        eprintln!("[wrosecode auth] {message}");
    }
}

/// Retry only a failure to establish a connection. A refresh token may
/// rotate as soon as an OAuth server accepts a request, so retrying after
/// receiving HTTP status or after an ambiguous body/read failure risks
/// accidentally reusing an already-consumed refresh token.
///
/// A second attempt uses an IPv4-bound client: on some Kali VPN/NAT64
/// setups the first DNS-selected address times out while curl succeeds over
/// IPv4. The normal client, its credentials, and the persisted account
/// settings are left untouched.
fn retry_refresh_connect_error(is_connect: bool, attempt: usize) -> bool {
    is_connect && attempt == 0
}

async fn send_refresh_with_connect_fallback(
    client: &reqwest::Client,
    old: &Credentials,
) -> Result<reqwest::Response> {
    const ENDPOINT: &str = "https://auth.openai.com/api/accounts/oauth/token";
    let request = |client: &reqwest::Client| {
        client.post(ENDPOINT).form(&[
            ("grant_type", "refresh_token"),
            ("client_id", old.client_id.as_str()),
            ("refresh_token", old.refresh_token.as_str()),
            ("resource", RESOURCE),
        ])
    };
    match request(client).send().await {
        Ok(response) => Ok(response),
        Err(error) if retry_refresh_connect_error(error.is_connect(), 0) => {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let ipv4_client = reqwest::Client::builder()
                .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(15))
                .build()
                .context("build IPv4 recovery client for ChatGPT OAuth")?;
            request(&ipv4_client)
                .send()
                .await
                .with_context(|| {
                    "ChatGPT OAuth connect failed (including IPv4 retry). Check access to auth.openai.com; retry when network is stable"
                })
        }
        Err(error) => Err(error).context("ChatGPT token refresh request failed"),
    }
}

pub async fn refresh(
    client: &reqwest::Client,
    store: &Store,
    label: &str,
    old: &Credentials,
) -> Result<Credentials> {
    if old.refresh_token.is_empty() {
        bail!("ChatGPT session expired; run /auth login chatgpt")
    }
    let response = send_refresh_with_connect_fallback(client, old).await?;
    if !response.status().is_success() {
        bail!(
            "ChatGPT token refresh failed ({}); run /auth login chatgpt",
            response.status()
        )
    }
    let reply: Value = response.json().await?;
    let mut fresh = old.clone();
    fresh.access_token = reply["access_token"]
        .as_str()
        .context("refresh response has no access token")?
        .into();
    fresh.refresh_token = reply["refresh_token"]
        .as_str()
        .context("refresh response has no rotating refresh token")?
        .into();
    if let Some(scope) = reply["scope"].as_str() {
        fresh.scopes = scope.split_whitespace().map(str::to_owned).collect();
    }
    fresh.expires_at = now().saturating_add(
        reply["expires_in"]
            .as_u64()
            .context("refresh response has no expiry")?,
    );
    store.save(label, &fresh)?;
    Ok(fresh)
}

/// Refresh only when the access token is expired or close to expiry. This is
/// deliberately outside the rendering loop so startup and provider switching
/// do not carry stale credentials into the first request.
pub async fn refresh_if_needed(client: &reqwest::Client, store: &Store, label: &str) -> Result<()> {
    let Some(credentials) = store.account(label)? else {
        return Ok(());
    };
    if credentials.expires_soon() {
        let _ = refresh(client, store, label, &credentials).await?;
    }
    Ok(())
}

pub async fn logout(client: &reqwest::Client, store: &Store, label: &str) -> Result<bool> {
    let old = store.account(label)?;
    let mut revoked = true;
    if let Some(old) = old {
        if !old.refresh_token.is_empty() {
            let endpoint = match client
                .get("https://auth.openai.com/.well-known/openid-configuration")
                .send()
                .await
            {
                Ok(response) => response.json::<Value>().await.ok().and_then(|metadata| {
                    metadata["revocation_endpoint"].as_str().map(str::to_owned)
                }),
                Err(_) => None,
            };
            revoked = if let Some(endpoint) = endpoint {
                client
                    .post(endpoint)
                    .form(&[
                        ("token", old.refresh_token.as_str()),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", old.client_id.as_str()),
                    ])
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
            } else {
                false
            };
        }
    }
    store.clear_tokens(label)?;
    Ok(revoked)
}

#[cfg(test)]
mod tests {
    use super::{validate_claims, CLOCK_SKEW_SECS};
    use serde_json::json;

    fn claims(aud: serde_json::Value) -> serde_json::Value {
        json!({
            "iss": "https://auth.openai.com",
            "aud": aud,
            "nonce": "nonce",
            "iat": 1_000,
            "nbf": 1_000,
            "exp": 2_000
        })
    }

    #[test]
    fn provider_alias_uses_active_account_label_without_losing_refresh() {
        use super::{Credentials, Store};
        let directory = std::env::temp_dir().join(format!(
            "wrose-auth-profile-{}-{}",
            std::process::id(),
            super::now()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        let store = Store::test_in(directory.clone());
        let mut credentials = Credentials {
            client_id: "client_test".into(),
            subject: "subject_test".into(),
            email: String::new(),
            id_token: String::new(),
            access_token: "token_a".into(),
            refresh_token: "refresh_a".into(),
            scopes: vec!["resource.invoke".into(), "chatgpt.tokens.use.direct".into()],
            expires_at: u64::MAX,
            issuer: String::new(),
            ext_agent_host_id: String::new(),
        };
        store.save("signed_in", &credentials).unwrap();
        let (label, found) = store
            .account_for_profile("different_profile")
            .unwrap()
            .unwrap();
        assert_eq!(label, "signed_in");
        assert_eq!(found.access_token, "token_a");
        credentials.access_token = "token_b".into();
        store.save("different_profile", &credentials).unwrap();
        let (label, found) = store
            .account_for_profile("different_profile")
            .unwrap()
            .unwrap();
        assert_eq!(label, "different_profile");
        assert_eq!(found.access_token, "token_b");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refresh_recovery_never_replays_ambiguous_or_consumed_tokens() {
        use super::retry_refresh_connect_error;
        assert!(retry_refresh_connect_error(true, 0));
        assert!(!retry_refresh_connect_error(true, 1));
        assert!(!retry_refresh_connect_error(false, 0));
        assert!(!retry_refresh_connect_error(false, 1));
    }

    #[test]
    fn accepts_issued_client_id_and_audience_array() {
        assert!(validate_claims(
            &claims(json!(["other", "oaiapp_test"])),
            "oaiapp_test",
            "nonce",
            1_500
        )
        .is_ok());
    }

    #[test]
    fn rejects_dynamic_registration_entrypoint_as_audience() {
        let result = validate_claims(
            &claims(json!("dynamic_agent_client")),
            "oaiapp_test",
            "nonce",
            1_500,
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("audience mismatch"));
    }

    #[test]
    fn reports_specific_claim_failures() {
        let mut value = claims(json!("oaiapp_test"));
        value["iss"] = json!("https://example.invalid");
        assert!(validate_claims(&value, "oaiapp_test", "nonce", 1_500)
            .unwrap_err()
            .to_string()
            .contains("issuer mismatch"));
        value["iss"] = json!("https://auth.openai.com");
        value["nonce"] = json!("different");
        assert!(validate_claims(&value, "oaiapp_test", "nonce", 1_500)
            .unwrap_err()
            .to_string()
            .contains("nonce mismatch"));
    }

    #[test]
    fn allows_small_clock_skew_but_rejects_future_tokens() {
        let mut value = claims(json!("oaiapp_test"));
        value["exp"] = json!(1_500 - (CLOCK_SKEW_SECS - 1));
        assert!(validate_claims(&value, "oaiapp_test", "nonce", 1_500).is_ok());
        value["nbf"] = json!(1_500 + CLOCK_SKEW_SECS + 1);
        assert!(validate_claims(&value, "oaiapp_test", "nonce", 1_500)
            .unwrap_err()
            .to_string()
            .contains("not valid yet"));
    }
}
