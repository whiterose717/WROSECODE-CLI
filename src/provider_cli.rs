use crate::provider;
use crate::settings::{ProviderProfile, Settings};
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::Read;

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
            let client = reqwest::Client::builder()
                .pool_max_idle_per_host(8)
                .timeout(std::time::Duration::from_secs(15))
                .build()?;
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
