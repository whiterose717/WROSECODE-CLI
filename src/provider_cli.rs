use crate::provider;
use crate::settings::{ProviderProfile, Settings};
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum ProviderAction {
    List,
    Add(AddArgs),
    SetKey(KeyArgs),
    Edit(EditArgs),
    Remove { name: String },
    Test { name: Option<String> },
}
#[derive(Parser)]
pub struct ProviderCli {
    #[command(subcommand)]
    pub action: ProviderAction,
}
#[derive(Args)]
pub struct AddArgs {
    pub name: String,
    #[arg(long)]
    pub base_url: String,
    #[arg(long, conflicts_with = "stdin")]
    pub api_key: Option<String>,
    #[arg(long)]
    pub stdin: bool,
    #[arg(long, default_value = "openai_compat")]
    pub kind: String,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long = "header")]
    pub headers: Vec<String>,
    /// Sign in with OAuth (browser loopback or device flow) instead of an
    /// API key; the client ID must come from your own OAuth registration.
    #[arg(long)]
    pub oauth: bool,
    /// Device-code flow (no browser callback needed).
    #[arg(long)]
    pub device: bool,
    #[arg(long)]
    pub oauth_client_id: Option<String>,
    #[arg(long)]
    pub oauth_issuer: Option<String>,
    #[arg(long)]
    pub oauth_scope: Option<String>,
}
#[derive(Args)]
pub struct KeyArgs {
    pub name: String,
    #[arg(long, conflicts_with = "stdin")]
    pub api_key: Option<String>,
    #[arg(long)]
    pub stdin: bool,
}
#[derive(Args)]
pub struct EditArgs {
    pub name: String,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
}

pub async fn run(action: ProviderAction) -> Result<()> {
    let mut settings = Settings::load()?;
    match action {
        ProviderAction::List => {
            for profile in &settings.providers {
                println!(
                    "{}\t{}\t{}\t{}",
                    profile.name,
                    profile.kind,
                    profile.base_url,
                    settings.status(profile)
                );
            }
        }
        ProviderAction::Add(args) => {
            Settings::validate_name(&args.name)?;
            if settings.profile(&args.name).is_some() {
                bail!("provider {} already exists", args.name);
            }
            if args.oauth {
                return oauth_add(&mut settings, &args).await;
            }
            let key = read_key(args.api_key, args.stdin)?;
            let mut headers = BTreeMap::new();
            for header in args.headers {
                let (name, value) = header.split_once('=').context("header must be K=V")?;
                if name.is_empty() {
                    bail!("header name is empty");
                }
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "authorization" | "x-api-key" | "proxy-authorization"
                ) {
                    bail!("use --api-key for auth headers");
                }
                headers.insert(name.into(), value.into());
            }
            let profile = ProviderProfile {
                name: args.name.clone(),
                kind: args.kind,
                base_url: args.base_url,
                model: args.model.unwrap_or_default(),
                key_ref: String::new(),
                headers,
                think: None,
                think_map: None,
                builtin: false,
                auth_method: "api_key".into(),
                oauth_client_id: String::new(),
                oauth_issuer: String::new(),
                oauth_scope: String::new(),
            };
            if profile.base_url.is_empty() {
                bail!("base URL is required");
            }
            if key.is_none() && !settings.no_key_needed(&profile) {
                bail!("API key is required; use --api-key, --stdin, or env:VAR");
            }
            settings.upsert_provider(profile)?;
            if let Some(key) = key {
                settings.set_key(&args.name, &key)?;
            }
            println!("Added {}", args.name);
        }
        ProviderAction::SetKey(args) => {
            let key =
                read_key(args.api_key, args.stdin)?.context("provide --api-key or --stdin")?;
            settings.set_key(&args.name, &key)?;
            println!("Updated key for {}", args.name);
        }
        ProviderAction::Edit(args) => {
            let mut profile = settings
                .profile(&args.name)
                .cloned()
                .with_context(|| format!("unknown provider: {}", args.name))?;
            if let Some(url) = args.base_url {
                profile.base_url = url;
            }
            if let Some(model) = args.model {
                profile.model = model;
            }
            settings.upsert_provider(profile)?;
            println!("Updated {}", args.name);
        }
        ProviderAction::Remove { name } => {
            settings.remove_provider(&name)?;
            println!("Removed {name}");
        }
        ProviderAction::Test { name } => {
            let names: Vec<_> = if let Some(name) = name {
                vec![name]
            } else {
                settings.providers.iter().map(|p| p.name.clone()).collect()
            };
            let client = provider::shared_client(10, 15)?;
            let mut failed = false;
            for name in names {
                match test_one(&mut settings, &name, &client).await {
                    Ok(latency) => println!("{name}: connected ({} ms)", latency.as_millis()),
                    Err(error) => {
                        eprintln!("{name}: error — {error}");
                        failed = true;
                    }
                }
            }
            if failed {
                bail!("one or more provider tests failed");
            }
        }
    }
    Ok(())
}

pub async fn test_one(
    settings: &mut Settings,
    name: &str,
    client: &reqwest::Client,
) -> Result<std::time::Duration> {
    let profile = settings
        .profile(name)
        .cloned()
        .with_context(|| format!("unknown provider: {name}"))?;
    if profile.base_url.is_empty() {
        bail!("needs setup: base URL is empty");
    }
    settings.refresh_oauth_if_needed(&profile, client).await?;
    let key = settings.key(&profile);
    if key.is_none() && !settings.no_key_needed(&profile) {
        bail!("key missing");
    }
    let provider = provider::create_profile(&profile, key, &profile.model, client.clone())?;
    let result = provider.test().await;
    settings.last_tests.insert(
        name.into(),
        result.as_ref().map(|d| *d).map_err(|e| e.to_string()),
    );
    result
}
fn read_key(value: Option<String>, stdin: bool) -> Result<Option<String>> {
    if stdin {
        let mut key = String::new();
        std::io::stdin().read_to_string(&mut key)?;
        return Ok(Some(key.trim_end_matches(['\r', '\n']).into()));
    }
    Ok(value)
}

/// `providers add --oauth`: run the browser loopback or device-code flow,
/// store the tokens owner-only, and record the `oauth` auth method on the
/// profile instead of an API key.
async fn oauth_add(settings: &mut Settings, args: &AddArgs) -> Result<()> {
    let client = provider::shared_client(10, 30)?;
    let cfg = crate::oauth::OAuthConfig::new(
        &args.oauth_client_id.clone().unwrap_or_default(),
        &args.oauth_issuer.clone().unwrap_or_default(),
        &args.oauth_scope.clone().unwrap_or_default(),
    );
    cfg.validate()?;
    let tokens = if args.device {
        let pending = crate::oauth::start_device(&client, &cfg).await?;
        println!("Open {} and enter code {}", pending.url, pending.user_code);
        crate::oauth::await_device(&client, &pending).await?
    } else {
        let (verifier, challenge) = crate::oauth::pkce()?;
        let state = crate::oauth::random_state()?;
        let url = crate::oauth::authorize_url(&cfg, &challenge, &state)?;
        println!("Open this URL to sign in:\n{url}");
        let _ = crate::oauth::open_browser(&url);
        println!("Waiting for the browser callback (or paste the redirect URL here):");
        let code = tokio::select! {
            result = crate::oauth::await_callback(
                cfg.callback_port,
                &state,
                crate::oauth::CALLBACK_TIMEOUT,
            ) => result?,
            pasted = read_redirect_line() => pasted?,
        };
        crate::oauth::exchange_code(&client, &cfg, &code, &verifier).await?
    };
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let path = crate::oauth::store_tokens(&home, &args.name, &tokens)?;
    let mut headers = BTreeMap::new();
    for header in &args.headers {
        let (name, value) = header.split_once('=').context("header must be K=V")?;
        if name.is_empty() {
            bail!("header name is empty");
        }
        headers.insert(name.into(), value.into());
    }
    settings.upsert_provider(ProviderProfile {
        name: args.name.clone(),
        kind: args.kind.clone(),
        base_url: args.base_url.clone(),
        model: args.model.clone().unwrap_or_default(),
        key_ref: String::new(),
        headers,
        think: None,
        think_map: None,
        builtin: false,
        auth_method: "oauth".into(),
        oauth_client_id: cfg.client_id.clone(),
        oauth_issuer: args.oauth_issuer.clone().unwrap_or_default(),
        oauth_scope: args.oauth_scope.clone().unwrap_or_default(),
    })?;
    println!(
        "Signed in{}; tokens stored owner-only at {}",
        tokens
            .account_id
            .as_deref()
            .map(|id| format!(" as {id}"))
            .unwrap_or_default(),
        path.display()
    );
    Ok(())
}

/// One pasted line from stdin: the full redirect URL, or the bare code.
/// A closed stdin never resolves, so the browser callback can still win.
async fn read_redirect_line() -> Result<String> {
    use tokio::io::AsyncBufReadExt as _;
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            std::future::pending::<()>().await;
            unreachable!("pending never resolves");
        }
        if let Some(code) = crate::oauth::code_from_pasted(&line) {
            return Ok(code);
        }
    }
}
