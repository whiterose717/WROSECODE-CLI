mod acp;
mod agent;
mod api;
mod attach;
mod autopilot;
#[cfg(test)]
mod benchmarks;
mod bridge;
mod bugbounty;
mod chatgpt_auth;
mod commands;
mod config;
mod coverage;
mod crash;
mod ctf;
mod ctfd;
mod events;
mod harness;
mod lsp;
mod markdown;
mod memory;
mod metrics;
mod oauth;
mod package;
mod pairing;
mod project;
mod prompt;
mod provider;
mod provider_cli;
mod recipe;
mod repo_map;
mod report;
mod sandbox;
mod session;
mod settings;
mod skills;
mod snapshot;
mod splash;
mod store;
mod telemetry;
mod think;
mod tools;
mod trace;
mod tui;

use agent::Agent;
use anyhow::{Context, Result};
use clap::Parser;
use config::{Config, Permission};
use futures::{stream::FuturesUnordered, StreamExt};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use think::ThinkLevel;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser)]
#[command(version, about = "WROSECODE coding agent")]
struct Cli {
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,
    #[arg(long, value_enum)]
    permission: Option<Permission>,
    /// Thinking level: off | low | medium | high | max | auto (auto starts
    /// at medium and adapts to progress). Cycles live with Ctrl+T.
    #[arg(long, value_enum)]
    think: Option<ThinkLevel>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "minimal")]
    harness: String,
    /// Agent mode: general | ctf | bounty | build | plan. The interactive
    /// equivalent is `/mode`.
    #[arg(long)]
    mode: Option<String>,
    #[arg(long, default_value_t = 3)]
    repair_retries: usize,
    #[arg(long)]
    check_command: Option<String>,
    #[arg(long = "skills")]
    skill_dirs: Vec<PathBuf>,
    /// Install a skill package before the run: a git URL (`url#ref` pins a
    /// branch/tag/commit) or a local directory. Repeatable.
    #[arg(long = "install-skill", value_name = "SOURCE")]
    install_skill: Vec<String>,
    #[arg(long)]
    mcp_bin: Option<String>,
    #[arg(long = "mcp-arg")]
    mcp_args: Vec<String>,
    #[arg(long)]
    quiet: bool,
    /// Tool and process visibility: on/verbose (default), off/normal, compact.
    /// An explicit flag overrides a project's config.toml for this launch.
    #[arg(long, value_parser = ["on", "off", "compact", "normal", "verbose"])]
    verbosity: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    until_cmd: Option<String>,
    #[arg(long)]
    goal: Option<String>,
    #[arg(long, default_value_t = 0)]
    max_tokens: usize,
    #[arg(long, default_value_t = 0)]
    max_wall_time: u64,
    #[arg(long)]
    no_summary: bool,
    /// Write a framed NDJSON submission stream (start/step/text/result) to
    /// PATH, or `-` for stdout
    #[arg(long, value_name = "PATH")]
    events: Option<PathBuf>,
    /// Log per-step timings (API latency, tool time, slow renders) as NDJSON
    /// to `~/.wrosecode/trace.log` for speed work
    #[arg(long)]
    trace: bool,
    /// Activate a user-defined agent from `.wrosecode/agents/<name>.md`
    #[arg(long, value_name = "NAME")]
    agent: Option<String>,
    /// Pin a file's contents into every prompt (aider-style explicit file
    /// context). Repeatable; the same thing the `/add` command does live.
    #[arg(long = "add", value_name = "PATH")]
    add: Vec<String>,
    /// Internal: run the named recipe (front-end for `wrosecode recipe run`)
    #[arg(long, hide = true, value_name = "NAME")]
    recipe: Option<String>,
    /// Internal: recipe parameter as KEY=VALUE (repeatable)
    #[arg(long = "recipe-set", hide = true, value_name = "KEY=VALUE")]
    recipe_set: Vec<String>,
    /// Internal: list discoverable recipes (front-end for `recipe list`)
    #[arg(long, hide = true)]
    recipe_list: bool,
    #[arg(long)]
    summary: Option<String>,
    #[arg(long)]
    headless: bool,
    #[arg(long)]
    web: bool,
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: String,
    /// Resume a saved session by ID (`wrosecode --session <id>`).
    #[arg(long, short = 's')]
    session: Option<String>,
    #[arg(long)]
    fork: bool,
    /// Fork --session at message N instead of its end (implies --fork)
    #[arg(long, value_name = "N", requires = "session")]
    fork_at: Option<usize>,
    /// Enable the opt-in `computer` tool (screenshot / click / type / key /
    /// scroll on this machine's display); same as `[tools] computer = true`
    #[arg(long)]
    computer: bool,
    #[arg(long)]
    export_session: Option<PathBuf>,
    #[arg(long)]
    import_session: Option<PathBuf>,
    #[arg(long = "race", value_name = "PROVIDER:MODEL")]
    race: Vec<String>,
    /// Disable all colour output (same as setting `NO_COLOR`)
    #[arg(long)]
    no_color: bool,
    /// CTF autopilot: flag-format regex the verifier must match
    #[arg(long, value_name = "REGEX")]
    flag_format: Option<String>,
    /// CTF autopilot: category override (crypto/web/pwn/rev/forensics/…)
    #[arg(long)]
    category: Option<String>,
    /// CTF autopilot: allow this remote host (host or host:port)
    #[arg(long, value_name = "HOST[:PORT]")]
    remote: Option<String>,
    /// CTF autopilot: optional limit — unlimited by default; use steps=…,tokens=…,seconds=…
    #[arg(long, value_name = "unlimited|STEPS|steps=…,tokens=…,seconds=…")]
    budget: Option<String>,
    /// CTF autopilot: max parallel hypothesis subagents
    #[arg(long, default_value_t = 3)]
    parallel: usize,
    /// Bug bounty: in-scope host, URL, or `*.wildcard` (repeatable or comma-separated)
    #[arg(long = "scope", value_name = "HOST")]
    scope: Vec<String>,
    /// Bug bounty: scope file with one include/exclude per line
    #[arg(long = "scope-file", value_name = "PATH")]
    scope_file: Option<PathBuf>,
    /// Bug bounty: out-of-scope host or `*.wildcard` (repeatable or comma-separated)
    #[arg(long = "exclude", value_name = "HOST")]
    exclude: Vec<String>,
    /// Bug bounty: program name recorded in `.bugbounty/findings.json`
    #[arg(long)]
    program: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Stamped before anything else so the splash can report an honest
    // time-to-first-paint (see src/splash.rs).
    let boot = std::time::Instant::now();
    crash::install_panic_hook(std::env::current_dir().ok());
    crash::install_signal_handlers();
    // Test hook: lets the e2e suite prove the crash path (terminal restore +
    // ~/.wrosecode/crash report) end to end. Never set in normal operation.
    if std::env::var_os("WROSECODE_FORCE_PANIC").is_some() {
        panic!("forced panic for crash-report coverage");
    }
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args
        .get(1)
        .is_some_and(|argument| argument == "providers")
    {
        let mut provider_args = vec!["wrosecode-providers".to_string()];
        provider_args.extend(raw_args.into_iter().skip(2));
        let provider_cli = provider_cli::ProviderCli::parse_from(provider_args);
        if let Err(error) = provider_cli::run(provider_cli.action).await {
            eprintln!(
                "error: {}",
                error
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or("provider operation failed")
            );
            std::process::exit(1);
        }
        return Ok(());
    }
    if raw_args.get(1).is_some_and(|argument| argument == "ctfd") {
        let mut ctfd_args = vec!["wrosecode-ctfd".to_string()];
        ctfd_args.extend(raw_args.into_iter().skip(2));
        let command = ctfd::CtfdCli::parse_from(ctfd_args);
        ctfd::run(command.action).await?;
        return Ok(());
    }
    // `wrosecode dashboard …` attaches to a running session (spec 3.2);
    // `wrosecode exec [--json] <prompt>` runs headless and, with --json,
    // emits the same DashStats the dashboard renders.
    if raw_args
        .get(1)
        .is_some_and(|argument| argument == "dashboard")
    {
        let mut attach_args = vec!["wrosecode-dashboard".to_string()];
        attach_args.extend(raw_args.iter().skip(2).cloned());
        let attach_cli = attach::AttachCli::parse_from(attach_args);
        attach::run(attach_cli).await?;
        return Ok(());
    }
    // `wrosecode bridge pair [--name NAME]` issues a one-time pairing code;
    // `wrosecode bridge login --relay WSS_URL [--id ID --secret SECRET]`
    // stores cloud-relay credentials for automatic reconnects;
    // `wrosecode bridge status` shows paired devices and relay config.
    if raw_args.get(1).is_some_and(|argument| argument == "bridge") {
        let mut name = std::env::var("WROSECODE_DEVICE_NAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Kali Linux".to_string());
        let mut args = raw_args.iter().skip(2).peekable();
        let sub = args.next().map(String::as_str).unwrap_or("status");
        // Allow `bridge --name X pair` or `bridge pair --name X`.
        let extra: Vec<String> = raw_args.iter().skip(2).cloned().collect();
        if extra.iter().any(|a| a == "--name") {
            if let Some(i) = extra.iter().position(|a| a == "--name") {
                if let Some(v) = extra.get(i + 1) {
                    name = v.clone();
                }
            }
        }
        let flag_value = |flag: &str| -> Option<String> {
            extra
                .iter()
                .position(|a| a == flag)
                .and_then(|i| extra.get(i + 1).cloned())
        };
        match sub {
            "pair" | "--name" => {
                if sub == "--name" {
                    // `bridge --name X pair` form: find `pair` token.
                    if !extra.iter().any(|a| a == "pair") {
                        eprintln!("usage: wrosecode bridge pair [--name NAME]");
                        std::process::exit(1);
                    }
                }
                let (device, pending) = pairing::new_pairing_code(&name)?;
                println!("PAIR WROSECODE DEVICE\n");
                println!("Device        {}", name);
                println!("Device ID     {}", device.device_id);
                println!("Pairing Code  {}", pending.code);
                println!("Expires       In 10 minutes (one-time use)");
                println!("\nEnter this code on your authenticated");
                println!("WROSECODE website to pair this device.");
                println!("\nThen start the server:");
                println!("  wrosecode server --hostname 127.0.0.1 --port 4076");
                return Ok(());
            }
            "status" => {
                let devices = pairing::list_devices();
                println!("WROSECODE BRIDGE STATUS\n");
                match bridge::relay_config() {
                    Some((url, device_id, _)) => {
                        let host = url.split('?').next().unwrap_or(&url);
                        println!("Cloud relay   CONFIGURED ({host})");
                        println!("Device ID     {device_id}");
                    }
                    None => {
                        println!("Cloud relay   NOT CONFIGURED");
                        println!("Set WROSECODE_RELAY_URL, WROSECODE_DEVICE_ID, WROSECODE_DEVICE_SECRET,");
                        println!(
                            "or run `wrosecode bridge login --relay wss://<worker>/relay/device`."
                        );
                    }
                }
                println!("Paired devices: {}", devices.len());
                for d in devices {
                    println!(
                        "  • {} ({}) account={} last_seen={}",
                        d.device_name, d.device_id, d.account_id, d.last_seen
                    );
                }
                println!("Pending codes: {}", pairing::pending_count());
                return Ok(());
            }
            "login" => {
                let url = flag_value("--relay")
                    .unwrap_or_else(|| std::env::var("WROSECODE_RELAY_URL").unwrap_or_default());
                if url.trim().is_empty() {
                    eprintln!(
                        "usage: wrosecode bridge login --relay wss://<your-worker>/relay/device [--id ID --secret SECRET]"
                    );
                    std::process::exit(1);
                }
                let device = pairing::local_device(&name)?;
                let id = flag_value("--id").unwrap_or(device.device_id.clone());
                let secret = match flag_value("--secret") {
                    Some(s) if !s.trim().is_empty() => s,
                    _ => {
                        let mut bytes = [0u8; 32];
                        ring::rand::SecureRandom::fill(
                            &ring::rand::SystemRandom::new(),
                            &mut bytes,
                        )
                        .map_err(|_| anyhow::anyhow!("secure random generation failed"))?;
                        use base64::Engine;
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
                    }
                };
                bridge::save_relay_config(&url, &id, &secret)?;
                println!("Relay credentials saved to ~/.wrosecode/relay.json");
                println!("Device ID  {id}");
                println!("Relay URL  {}", url.split('?').next().unwrap_or(&url));
                println!("\nStart the server to connect automatically:");
                println!("  wrosecode server --hostname 127.0.0.1 --port 4076");
                return Ok(());
            }
            _ => {
                eprintln!("usage: wrosecode bridge pair [--name NAME]");
                eprintln!(
                    "       wrosecode bridge login --relay WSS_URL [--id ID --secret SECRET]"
                );
                eprintln!("       wrosecode bridge status");
                std::process::exit(1);
            }
        }
    }
    let mut exec_json = false;
    let mut ctf_mode = false;
    let mut bugbounty_mode = false;
    let cli_args: Vec<String> = if raw_args.get(1).is_some_and(|argument| argument == "server") {
        if raw_args.iter().skip(2).any(|a| a == "--help" || a == "-h") {
            println!("wrosecode server — start the local agent API server\n");
            println!("USAGE:");
            println!("  wrosecode server [--hostname HOST] [--port PORT]\n");
            println!("EXAMPLES:");
            println!(
                "  wrosecode server --hostname 127.0.0.1 --port 4076   # loopback only (default)"
            );
            println!("  wrosecode server --hostname 0.0.0.0 --port 4076     # LAN (warns; keep auth on)\n");
            println!("AUTH:");
            println!("  Bearer token in ~/.wrosecode/server.token, or per-device tokens from `wrosecode bridge pair`.");
            println!("  Binding beyond localhost prints a network-exposure warning.");
            return Ok(());
        }
        // `wrosecode server --hostname 0.0.0.0 --port 4076` is the stable
        // server spelling. Keep the older `--web --listen` flags working.
        let mut hostname = "127.0.0.1".to_string();
        let mut port = "4076".to_string();
        let mut extra = Vec::new();
        let mut args = raw_args.iter().skip(2);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--hostname" => {
                    hostname = args.next().context("--hostname requires a value")?.clone()
                }
                "--port" => port = args.next().context("--port requires a value")?.clone(),
                other => extra.push(other.to_string()),
            }
        }
        let address = format!("{hostname}:{port}");
        address
            .parse::<std::net::SocketAddr>()
            .context("invalid server host or port")?;
        let mut rewritten = vec![
            "wrosecode".to_string(),
            "--web".to_string(),
            "--listen".to_string(),
            address,
        ];
        rewritten.extend(extra);
        rewritten
    } else if raw_args.get(1).is_some_and(|argument| argument == "exec") {
        let mut rewritten = vec!["wrosecode".to_string(), "--headless".to_string()];
        for argument in raw_args.iter().skip(2) {
            if argument == "--json" {
                exec_json = true;
            } else {
                rewritten.push(argument.clone());
            }
        }
        rewritten
    } else if raw_args.get(1).is_some_and(|argument| argument == "ctf") {
        // Spec PHASE 5: `wrosecode ctf <file|dir|url|description>` always runs
        // headless (the TUI path is `/ctf`), so mirror the `exec` rewrite.
        ctf_mode = true;
        let mut rewritten = vec!["wrosecode".to_string(), "--headless".to_string()];
        rewritten.extend(raw_args.iter().skip(2).cloned());
        rewritten
    } else if raw_args
        .get(1)
        .is_some_and(|argument| argument == "bugbounty" || argument == "bug-bounty")
    {
        // Same contract as `ctf`: the subcommand always runs headless and the
        // interactive workflow lives behind `/bugbounty` in the TUI.
        bugbounty_mode = true;
        let mut rewritten = vec!["wrosecode".to_string(), "--headless".to_string()];
        rewritten.extend(raw_args.iter().skip(2).cloned());
        rewritten
    } else if raw_args.get(1).is_some_and(|argument| argument == "recipe") {
        // `wrosecode recipe list` and `wrosecode recipe run <name> [--set
        // KEY=VALUE …]` rewrite into hidden flags, the same way `exec` does.
        match raw_args.get(2).map(String::as_str) {
            Some("list") => {
                // Global flags (`--provider`, `--model`, …) ride along.
                let mut rewritten = vec!["wrosecode".to_string(), "--recipe-list".to_string()];
                rewritten.extend(raw_args.iter().skip(3).cloned());
                rewritten
            }
            Some("run") => {
                let mut rewritten = vec!["wrosecode".to_string(), "--headless".to_string()];
                let mut arguments = raw_args.iter().skip(3).peekable();
                let mut want_name = true;
                while let Some(argument) = arguments.next() {
                    if want_name {
                        rewritten.push("--recipe".to_string());
                        rewritten.push(argument.clone());
                        want_name = false;
                    } else if argument == "--set" {
                        if let Some(value) = arguments.next() {
                            rewritten.push("--recipe-set".to_string());
                            rewritten.push(value.clone());
                        }
                    } else if !argument.starts_with('-') && argument.contains('=') {
                        rewritten.push("--recipe-set".to_string());
                        rewritten.push(argument.clone());
                    } else {
                        rewritten.push(argument.clone());
                    }
                }
                if want_name {
                    eprintln!("usage: wrosecode recipe run <name> [--set KEY=VALUE …]");
                    std::process::exit(1);
                }
                rewritten
            }
            _ => {
                eprintln!("usage: wrosecode recipe list");
                eprintln!("       wrosecode recipe run <name> [--set KEY=VALUE …]");
                std::process::exit(1);
            }
        }
    } else {
        raw_args.clone()
    };
    let cli = Cli::parse_from(cli_args);
    let root = std::env::current_dir()?;
    let store = store::Store::open_default()?;
    if let Some(path) = &cli.import_session {
        let imported = session::Session::load(path)?;
        persist_session(&store, &imported)?;
        println!("imported session {}", imported.name);
        return Ok(());
    }
    if let Some(path) = &cli.export_session {
        let id = cli
            .session
            .as_deref()
            .context("--export-session requires --session ID")?;
        let session = store
            .load_session(id)?
            .with_context(|| format!("session {id} was not found"))?;
        std::fs::write(path, serde_json::to_vec_pretty(&session)?)?;
        println!("exported session {id} to {}", path.display());
        return Ok(());
    }
    let runtime = config::RuntimeConfig::load(&root);
    let effective_verbosity = cli
        .verbosity
        .as_deref()
        .map(config::normalize_verbosity)
        .unwrap_or(runtime.ui.verbosity.as_str())
        .to_owned();
    let mut settings = settings::Settings::load()?;
    let provider_name = cli
        .provider
        .clone()
        .or_else(|| {
            settings
                .last_selection()
                .filter(|selection| settings.profile(&selection.provider).is_some())
                .map(|selection| selection.provider)
        })
        .or_else(|| settings.first_usable().map(|profile| profile.name.clone()))
        // Keep a fresh install's TUI usable even before a model is selected.
        // This is only an empty-model shell; it never silently selects a
        // model or changes a user's saved provider.
        .or_else(|| {
            settings
                .profile("openai")
                .map(|profile| profile.name.clone())
        })
        .unwrap_or_else(|| "openai".into());
    let explicit_model = cli.model.clone();
    let selected_model = cli.model.unwrap_or_else(|| {
        settings
            .last_selection()
            .filter(|selection| selection.provider == provider_name)
            .map(|selection| selection.model)
            .or_else(|| {
                settings
                    .profile(&provider_name)
                    .map(|profile| profile.model.clone())
                    .filter(|model| !model.is_empty())
            })
            .unwrap_or_default()
    });
    // An explicit model is a deliberate selection. Save it so a later
    // invocation without `--model` starts with the model actually used.
    if explicit_model.is_some() && settings.profile(&provider_name).is_some() {
        settings.set_default_model(&provider_name, &selected_model)?;
    }
    // Thinking mode precedence: --think > the provider profile's think key
    // (model-specific) > [agent].think from config.toml > derived from the
    // legacy numeric thinking_level. An explicit mode rewrites the effective
    // 0–20 strength to its anchor; without one the legacy number stands.
    let configured_think = cli
        .think
        .or_else(|| {
            settings
                .profile(&provider_name)
                .and_then(|profile| profile.think)
        })
        .or(runtime.agent.think);
    let think = configured_think
        .unwrap_or_else(|| ThinkLevel::from_level(runtime.agent.thinking_level.min(20)));
    // goose's planner/worker split: [agent] planner = "provider/model"
    // (or a bare model, keeping the worker's provider).
    let (planner_provider, planner_model) = runtime
        .agent
        .planner_split(&provider_name)
        .unwrap_or_else(|| (String::new(), String::new()));
    let config = Arc::new(Config {
        root,
        permission: cli.permission.unwrap_or(runtime.agent.permission),
        model: selected_model,
        provider: provider_name,
        harness: cli.harness,
        repair_retries: cli.repair_retries,
        check_command: cli.check_command,
        skill_dirs: cli.skill_dirs,
        think,
        thinking_level: configured_think
            .map(ThinkLevel::anchor)
            .unwrap_or_else(|| runtime.agent.thinking_level.min(20)),
        max_parallel_tasks: if cli.parallel > 0 {
            cli.parallel.min(20)
        } else {
            runtime.agent.max_parallel_tasks.clamp(1, 20)
        },
        shell_timeout_seconds: runtime.agent.shell_timeout_seconds.max(1),
        tool_retries: runtime.agent.tool_retries.clamp(1, 3),
        fallback_provider: runtime.agent.fallback_provider,
        fallback_model: runtime.agent.fallback_model,
        planner_provider,
        planner_model,
        auto_commit: runtime.agent.auto_commit,
        redis_url: runtime
            .cache
            .redis_url
            .filter(|_| runtime.cache.backend.eq_ignore_ascii_case("redis")),
        budget_usd: runtime.agent.budget_usd.max(0.0),
        qdrant_url: runtime.cache.qdrant_url,
        ui_theme: runtime.ui.theme,
        verbosity: effective_verbosity,
        alternate_screen: runtime.ui.alternate_screen,
        mouse_capture: runtime.ui.mouse_capture,
        alert_bell: runtime.ui.alert_bell,
        smooth_scroll_lines: runtime.ui.smooth_scroll_lines.clamp(1, 20),
        sandbox: runtime.sandbox.policy(),
        lsp: runtime.lsp.into_settings(),
        computer_tools: cli.computer || runtime.tools.computer,
    });
    // Take the terminal and paint the splash before provider setup, skill
    // discovery, metrics, MCP servers, and session restore: those are what
    // would otherwise sit between the user and their first frame.
    let tui_mode = cli.prompt.is_none()
        && !cli.headless
        && !ctf_mode
        && !bugbounty_mode
        && cli.recipe.is_none()
        && !cli.recipe_list
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let profile = tui::TerminalProfile::detect(
        config.alternate_screen,
        config.mouse_capture.as_deref(),
        cli.no_color,
    );
    let guard = if tui_mode {
        let guard = tui::TerminalGuard::enter_painting(profile)?;
        splash::paint_stage(profile, &boot, 0)?;
        Some(guard)
    } else {
        None
    };
    // Do not enable raw mode until startup I/O is done. In raw mode Ctrl+C is
    // a key event rather than SIGINT; entering raw here would make a slow
    // OAuth/MCP/provider startup impossible to interrupt. Painting the splash
    // still happens immediately — only keyboard ownership waits.
    let control_client = provider::shared_client(5, 15)?;
    let client = provider::streaming_client(7)?;
    if let Some(profile) = settings.profile(&config.provider) {
        // OAuth tokens refresh here so the first turn never starts on an
        // expired credential, but a broken identity endpoint cannot freeze
        // terminal startup indefinitely.
        settings
            .refresh_oauth_if_needed(profile, &control_client)
            .await
            .context("OAuth token refresh failed")?;
        if profile.kind == "openai_chatgpt_plan" {
            chatgpt_auth::refresh_if_needed(
                &control_client,
                &chatgpt_auth::Store::current()?,
                &profile.name,
            )
            .await
            .context("ChatGPT token refresh failed")?;
        }
    }
    let mut config = (*config).clone();
    if settings.profile(&config.provider).is_none() {
        // The configured provider is gone (a removed default, a renamed
        // entry): fall back to another configured provider instead of
        // failing to start, and say so out loud.
        if let Some(fallback) = settings.first_usable() {
            eprintln!(
                "provider `{}` is not configured; using `{}/{}` instead",
                config.provider, fallback.name, fallback.model
            );
            config.provider = fallback.name.clone();
            config.model = fallback.model.clone();
        } else {
            anyhow::bail!(
                "provider `{}` is not configured and no usable provider remains (add one with /connect or `wrosecode providers add`)",
                config.provider
            );
        }
    }
    let config = Arc::new(config);
    let provider = if let Some(profile) = settings.profile(&config.provider) {
        provider::create_profile(
            profile,
            settings.key(profile),
            &config.model,
            client.clone(),
        )?
    } else {
        provider::create(&config.provider, &config.model, client.clone())?
    };
    if tui_mode {
        // The reveal costs nothing itself: it repaints between init steps
        // that were already going to run.
        let _ = splash::paint_stage(profile, &boot, 1);
    }
    for source in &cli.install_skill {
        let names = package::install(&skills::default_skills_dir(), source)
            .await
            .with_context(|| format!("--install-skill {source}"))?;
        if !cli.quiet {
            eprintln!("installed {} skill(s): {}", names.len(), names.join(", "));
        }
    }
    if !cli.install_skill.is_empty() {
        skills::invalidate_cache(&config.root, &config.skill_dirs);
    }
    let mut agent = Agent::new(config, provider, client)?;
    if let Some(mode) = cli.mode.as_deref() {
        agent
            .set_mode(mode)
            .with_context(|| format!("--mode {mode}"))?;
        // Headless mode selection has to arm the same scope the TUI's
        // `/mode` does, or every CTF tool call is denied before the model
        // sees a turn. Bounty scopes still come only from `--scope`/
        // `--scope-file` via the `bugbounty` subcommand.
        if matches!(mode, "ctf") {
            let root = agent.config.root.clone();
            let scope = autopilot::Scope::for_target("", &root, None);
            agent.tools.set_scope(Some(scope));
        }
    }
    if ctf_mode {
        agent.set_mode("ctf")?;
    }
    if bugbounty_mode {
        agent.set_mode("bugbounty")?;
    }
    agent.trace = crate::trace::TraceSink::open(cli.trace);
    if agent.trace.enabled() && !cli.quiet {
        eprintln!("tracing per-step timings to ~/.wrosecode/trace.log");
    }
    let resumed_session = if let Some(id) = &cli.session {
        let loaded = store
            .load_session(id)?
            .with_context(|| format!("session {id} was not found"))?;
        if cli.fork || cli.fork_at.is_some() {
            // `--fork` branches at the current end; `--fork-at N` keeps the
            // first N messages (the 1-based numbers the TUI `/history`
            // prints) so the new session starts earlier.
            let forked = loaded
                .branch(cli.fork_at)
                .with_context(|| match cli.fork_at {
                    Some(keep) => format!("--fork-at {keep}"),
                    None => format!("--session {id} --fork"),
                })?;
            agent.messages = forked.messages.clone();
            agent.pinned = forked.pinned.clone();
            Some(forked)
        } else {
            agent.messages = loaded.messages.clone();
            agent.pinned = loaded.pinned.clone();
            Some(loaded)
        }
    } else {
        None
    };
    // A resumed session brings its provider and model back: the agent keeps
    // working where it left off instead of silently switching backends.
    if let Some(resumed) = &resumed_session {
        let switch = !resumed.provider_name.is_empty()
            && !resumed.model.is_empty()
            && (resumed.provider_name != agent.config.provider
                || resumed.model != agent.config.model);
        if switch {
            if let Some(profile) = settings.profile(&resumed.provider_name) {
                if let Ok(restored) = provider::create_profile(
                    profile,
                    settings.key(profile),
                    &resumed.model,
                    agent.tools.client.clone(),
                ) {
                    agent.provider = restored;
                    agent.config = Arc::new(Config {
                        provider: resumed.provider_name.clone(),
                        model: resumed.model.clone(),
                        ..(*agent.config).clone()
                    });
                }
            }
        }
    }
    for path in &cli.add {
        agent.pin(path).with_context(|| format!("--add {path}"))?;
    }
    if let Some(binary) = cli.mcp_bin.as_deref() {
        agent.tools.mcps.push((
            "cli".into(),
            tools::mcp::Mcp::connect(binary, &cli.mcp_args).await?,
        ));
    }
    // MCP servers connect concurrently (spawn + handshake each) instead of
    // one by one: with N servers this turns N sequential round trips into
    // one. Order and error tolerance are unchanged — failures are skipped,
    // successes land in configuration order.
    if !settings.mcps.is_empty() {
        let connections = futures::future::join_all(settings.mcps.iter().map(|server| {
            let name = server.name.clone();
            let definition = server.clone();
            async move {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tools::mcp::Mcp::connect_def(&definition),
                )
                .await
                .map_err(|_| anyhow::anyhow!("MCP startup timed out after 5s"))
                .and_then(|result| result);
                (name, result)
            }
        }))
        .await;
        for (name, connection) in connections {
            if let Ok(mcp) = connection {
                agent.tools.mcps.push((name, mcp));
            }
        }
    }
    if let Some(name) = cli.agent.as_deref() {
        let user_agents = markdown::discover_agents(&agent.config.root);
        match user_agents.iter().find(|candidate| candidate.name == name) {
            Some(user) => agent.set_user_agent(user)?,
            None => {
                let available: Vec<&str> = user_agents
                    .iter()
                    .map(|candidate| candidate.name.as_str())
                    .collect();
                anyhow::bail!(
                    "unknown agent `{name}` ({})",
                    if available.is_empty() {
                        "no .wrosecode/agents/*.md files found".to_string()
                    } else {
                        format!("available: {}", available.join(", "))
                    }
                );
            }
        }
    }
    let mut event_sink = attach_events(&mut agent, cli.events.as_deref())?;
    if cli.recipe_list {
        let recipes = recipe::discover(&agent.config.root)?;
        if recipes.is_empty() {
            println!("no recipes found (looked in .wrosecode/recipes/ and ~/.wrosecode/recipes/)");
        }
        for plan in recipes {
            let version = if plan.version.is_empty() {
                "-"
            } else {
                plan.version.as_str()
            };
            println!("{:<24} {:<8} {}", plan.name, version, plan.description);
        }
        return Ok(());
    }
    if let Some(name) = cli.recipe.as_deref() {
        let plan = recipe::find(&agent.config.root, name)?;
        let values = recipe::resolve(&plan, &recipe::parse_sets(&cli.recipe_set)?)?;
        write_event(
            &event_sink,
            &events::start_frame(&format!("recipe {}", plan.name), "recipe"),
        );
        let quiet = cli.quiet;
        let response = recipe::run(&mut agent, &plan, &values, |line| {
            if !quiet {
                eprintln!("• {line}");
            }
        })
        .await?;
        let verified = check_goal(
            &agent,
            cli.goal.as_ref(),
            cli.until.as_ref(),
            cli.until_cmd.as_ref(),
            &response,
        )
        .await?;
        if let Some(stream) = &mut event_sink {
            stream.quiesce(&mut agent).await;
        }
        write_event(
            &event_sink,
            &events::result_frame(&response, verified, &agent.usage, agent.model_turns),
        );
        print_outcome(
            &agent,
            &response,
            verified,
            cli.quiet,
            cli.no_summary,
            cli.summary.as_deref() == Some("json"),
        );
        let mut session = resumed_session.unwrap_or_else(session::Session::fresh);
        session.messages = agent.messages.clone();
        session.provider_name = agent.config.provider.clone();
        session.model = agent.config.model.clone();
        session.summary = format!("recipe {}", plan.name);
        session
            .transcript
            .push(("YOU".into(), format!("recipe {}", plan.name)));
        session.transcript.push(("WROSE".into(), response.clone()));
        persist_session(&store, &session)?;
        if !verified && (cli.until.is_some() || cli.until_cmd.is_some() || cli.goal.is_some()) {
            std::process::exit(2);
        }
        return Ok(());
    }
    if ctf_mode {
        // Spec PHASE 5: exit 0 verified, exit 2 budget-exhausted, exit 1
        // (anyhow) on error.
        let options = autopilot::Options {
            target: cli.prompt.clone().unwrap_or_else(|| ".".into()),
            flag_format: cli.flag_format.clone(),
            category: cli.category.clone(),
            remote: cli.remote.clone(),
            budget: autopilot::Budget::parse(cli.budget.as_deref().unwrap_or_default())?,
            parallel: cli.parallel,
        };
        write_event(
            &event_sink,
            &events::start_frame(&format!("ctf {}", options.target), "ctf"),
        );
        let outcome = autopilot::run(&mut agent, &options).await?;
        let mut session = resumed_session.unwrap_or_else(session::Session::fresh);
        session.messages = agent.messages.clone();
        session.provider_name = agent.config.provider.clone();
        session.model = agent.config.model.clone();
        session.summary = format!("ctf {}", options.target);
        persist_session(&store, &session)?;
        // Spec 5.8: every autopilot run leaves `writeups/<challenge>.md`.
        let flags: Vec<String> = agent
            .ctf
            .recorded_hits()
            .into_iter()
            .map(|hit| hit.flag)
            .collect();
        let notes = std::fs::read_to_string(autopilot::notes_path(&agent.config.root)).ok();
        let writeup =
            report::writeup_challenge(&agent.config.root, &session, &flags, notes.as_deref())?;
        println!("WRITEUP {}", writeup.display());
        if let Some(stream) = &mut event_sink {
            stream.quiesce(&mut agent).await;
        }
        let (verified, answer) = match &outcome {
            autopilot::Outcome::Verified { flag, .. } => (true, flag.clone()),
            autopilot::Outcome::Unsolved { report, .. } => (false, report.clone()),
        };
        write_event(
            &event_sink,
            &events::result_frame(&answer, verified, &agent.usage, agent.model_turns),
        );
        match outcome {
            autopilot::Outcome::Verified { flag, evidence } => {
                println!("FLAG    {flag}");
                println!("EVIDENCE {evidence}");
                return Ok(());
            }
            autopilot::Outcome::Unsolved { report, candidate } => {
                if let Some(candidate) = candidate {
                    println!("⚠ {candidate}");
                }
                println!("{report}");
                std::process::exit(2);
            }
        }
    }
    if bugbounty_mode {
        // Exit 0 when at least one finding verified with intact evidence,
        // exit 2 when the run ended without one, exit 1 on a hard error.
        let mut options = bugbounty::Options::with_default_budget();
        options.target = cli.prompt.clone().unwrap_or_else(|| ".".into());
        options.includes = cli.scope.clone();
        options.excludes = cli.exclude.clone();
        options.scope_file = cli.scope_file.clone();
        options.program = cli.program.clone().unwrap_or_default();
        options.parallel = cli.parallel.max(1);
        if let Some(budget) = &cli.budget {
            options.budget = autopilot::Budget::parse(budget)?;
        }
        let options = bugbounty::finalize(options)?;
        write_event(
            &event_sink,
            &events::start_frame(&format!("bugbounty {}", options.target), "bugbounty"),
        );
        let outcome = bugbounty::run(&mut agent, &options).await?;
        let mut session = resumed_session.unwrap_or_else(session::Session::fresh);
        session.messages = agent.messages.clone();
        session.provider_name = agent.config.provider.clone();
        session.model = agent.config.model.clone();
        session.summary = format!("bugbounty {}", options.target);
        persist_session(&store, &session)?;
        let answer = bugbounty::stop_report(&outcome);
        if let Some(stream) = &mut event_sink {
            stream.quiesce(&mut agent).await;
        }
        write_event(
            &event_sink,
            &events::result_frame(
                &answer,
                outcome.verified > 0,
                &agent.usage,
                agent.model_turns,
            ),
        );
        println!(
            "VERIFIED {} · REPRODUCED {} · OPEN {}",
            outcome.verified, outcome.reproduced, outcome.potential
        );
        for path in &outcome.report {
            println!("REPORT  {}", path.display());
        }
        if outcome.verified == 0 {
            std::process::exit(2);
        }
        return Ok(());
    }
    if cli.prompt.as_deref() == Some("acp") {
        return acp::serve(agent).await;
    }
    if cli.web {
        return api::serve(agent, store, &cli.listen).await;
    }
    if let Some(prompt) = cli.prompt {
        // A headless prompt that is a user command (`/ship the fix`) expands
        // just like it does in the TUI.
        let prompt = markdown::expand_prompt(&agent.config.root, &prompt);
        write_event(
            &event_sink,
            &events::start_frame(
                &prompt,
                resumed_session
                    .as_ref()
                    .map(|session| session.name.clone())
                    .unwrap_or_else(|| "new".into())
                    .as_str(),
            ),
        );
        let (raced_agent, response) = if !cli.race.is_empty() {
            race_models(agent, &settings, &prompt, &cli.race).await?
        } else {
            let response = if cli.max_wall_time > 0 {
                tokio::time::timeout(
                    std::time::Duration::from_secs(cli.max_wall_time),
                    agent.turn(&prompt),
                )
                .await
                .context("maximum wall time exceeded")??
            } else {
                agent.turn(&prompt).await?
            };
            (agent, response)
        };
        agent = raced_agent;
        let verified = check_goal(
            &agent,
            cli.goal.as_ref(),
            cli.until.as_ref(),
            cli.until_cmd.as_ref(),
            &response,
        )
        .await?;
        if let Some(stream) = &mut event_sink {
            stream.quiesce(&mut agent).await;
        }
        write_event(
            &event_sink,
            &events::result_frame(&response, verified, &agent.usage, agent.model_turns),
        );
        if exec_json {
            // Spec 3.2: `exec --json` emits the dashboard snapshot, with the
            // answer and verification folded in.
            let mut value = serde_json::to_value(tui::stats_from_agent(
                &agent,
                "exec",
                if verified { "verified" } else { "unverified" },
            ))?;
            value["answer"] = serde_json::Value::String(response.clone());
            value["verified"] = serde_json::Value::Bool(verified);
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            print_outcome(
                &agent,
                &response,
                verified,
                cli.quiet,
                cli.no_summary,
                cli.summary.as_deref() == Some("json"),
            );
        }
        if !verified && (cli.until.is_some() || cli.until_cmd.is_some() || cli.goal.is_some()) {
            std::process::exit(2);
        }
        let mut session = resumed_session.unwrap_or_else(session::Session::fresh);
        session.messages = agent.messages.clone();
        session.provider_name = agent.config.provider.clone();
        session.model = agent.config.model.clone();
        session.summary = prompt.chars().take(80).collect();
        session.transcript.push(("YOU".into(), prompt));
        session.transcript.push(("WROSE".into(), response.clone()));
        persist_session(&store, &session)?;
        return Ok(());
    }
    if cli.headless {
        anyhow::bail!("--headless requires a prompt argument or piped input");
    }
    if let Some(guard) = guard {
        // Startup network/disk work has completed. From this point onward the
        // TUI event loop owns Ctrl+C and TerminalGuard guarantees restoration.
        guard.start_input()?;
        splash::paint_stage(profile, &boot, 2)?;
        return tui::run(agent, resumed_session, store, guard).await;
    }
    println!("WROSECODE  /plan /build /harness NAME /memory /quit");
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let Some(line) = input.next_line().await? else {
            break;
        };
        let line = line.trim();
        match line {
            "/quit" | "/exit" => break,
            "/plan" => {
                agent.tools.plan = true;
                println!("plan mode");
            }
            "/build" => {
                agent.tools.plan = false;
                println!("build mode");
            }
            "/memory" => println!("{}", agent.memory.list()?.join("\n")),
            _ if line.starts_with("/harness ") => {
                let name = line.trim_start_matches("/harness ").trim();
                if let Some(harness) = harness::Harness::parse(name) {
                    agent.harness = harness;
                    println!("harness: {}", harness.name());
                } else {
                    println!("unknown harness");
                }
            }
            _ if line.starts_with("#!fact ") => {
                println!("{}", agent.memory.save(&line[7..], true)?)
            }
            _ if line.starts_with("#fact ") => {
                println!("{}", agent.memory.save(&line[6..], false)?)
            }
            "" => {}
            _ => match agent.turn(line).await {
                Ok(reply) => println!("{reply}"),
                Err(error) => eprintln!("error: {error:#}"),
            },
        }
    }
    Ok(())
}

async fn race_models(
    primary: Agent,
    settings: &settings::Settings,
    prompt: &str,
    specs: &[String],
) -> Result<(Agent, String)> {
    let mut agents = vec![primary];
    let client = agents[0].tools.client.clone();
    for spec in specs.iter().take(3) {
        let (provider_name, model) = spec
            .split_once(':')
            .with_context(|| format!("race model must be PROVIDER:MODEL: {spec}"))?;
        let profile = settings
            .profile(provider_name)
            .with_context(|| format!("unknown race provider {provider_name}"))?;
        let provider =
            provider::create_profile(profile, settings.key(profile), model, client.clone())?;
        let mut config = (*agents[0].config).clone();
        config.provider = provider_name.into();
        config.model = model.into();
        let mut racer = Agent::new(Arc::new(config), provider, client.clone())?;
        racer.trace = agents
            .first()
            .map(|agent| agent.trace.clone())
            .unwrap_or_default();
        agents.push(racer);
    }
    let mut futures = FuturesUnordered::new();
    for mut candidate in agents {
        let prompt = prompt.to_string();
        futures.push(async move {
            let result = candidate.turn(&prompt).await;
            (candidate, result)
        });
    }
    let flag = regex::Regex::new(r"(?:flag|CTF|picoCTF|HTB)\{[^}\r\n]+\}")?;
    let mut first_success = None;
    let mut errors = Vec::new();
    while let Some((candidate, result)) = futures.next().await {
        match result {
            Ok(answer) if flag.is_match(&answer) => return Ok((candidate, answer)),
            Ok(answer) if first_success.is_none() => first_success = Some((candidate, answer)),
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    first_success.ok_or_else(|| anyhow::anyhow!("all race models failed: {}", errors.join("; ")))
}

/// Shared handle to the framed submission stream written by `--events`.
type EventSink = std::sync::Arc<std::sync::Mutex<events::EventWriter>>;

/// The live submission stream: a sink for `start`/`result` frames and the
/// forwarder task that appends every `Progress` event as it happens.
struct EventStream {
    sink: EventSink,
    forwarder: Option<tokio::task::JoinHandle<()>>,
}

impl EventStream {
    /// Stop forwarding and wait until every queued frame is written, so the
    /// `result` frame always lands last.
    async fn quiesce(&mut self, agent: &mut Agent) {
        drop(agent.event_tx.take());
        if let Some(forwarder) = self.forwarder.take() {
            let _ = forwarder.await;
        }
    }
}

/// Attach the framed submission stream (`--events PATH`): live `Progress`
/// events are framed and appended as they happen, while the `start` and
/// `result` frames come from the caller. Returns `None` without the flag.
/// The answer itself (`--summary json` gets one `TaskComplete` line, plain
/// runs get the answer plus the RESULT block) — shared by the prompt and
/// `recipe run` branches so both emit the same framed output.
fn print_outcome(
    agent: &Agent,
    response: &str,
    verified: bool,
    quiet: bool,
    no_summary: bool,
    json_summary: bool,
) {
    if !quiet && !json_summary {
        println!("{response}");
    }
    if json_summary {
        println!(
            "{}",
            serde_json::json!({
                "type": "TaskComplete",
                "status": if verified { "verified" } else { "unverified" },
                "answer": response,
                "usage": agent.usage,
                "model_turns": agent.model_turns,
                "tool_cache_hits": agent.tools.cache_stats().0,
                "elapsed_ms": agent.started_at.elapsed().as_millis(),
            })
        );
    } else if !no_summary {
        let (cache_hits, saved_bytes) = agent.tools.cache_stats();
        eprintln!("────────────────────────────────────────────────────────────");
        eprintln!(
            " RESULT   {}",
            if verified {
                "✔ Solved & verified"
            } else {
                "⚠ Finished unverified"
            }
        );
        eprintln!(" Time     {:.1}s", agent.started_at.elapsed().as_secs_f64());
        eprintln!(
            " Steps    {} model turns · {} cached tool calls",
            agent.model_turns, cache_hits
        );
        for (index, step) in agent.step_log.iter().enumerate().take(8) {
            eprintln!(" {}. {step}", index + 1);
        }
        if agent.step_log.len() > 8 {
            eprintln!(" … +{} more", agent.step_log.len() - 8);
        }
        eprintln!(
            " Tokens   {} in · {} out · {} reasoning{}",
            agent.usage.input,
            agent.usage.output,
            agent.usage.reasoning,
            if agent.usage.estimated {
                " · estimated"
            } else {
                ""
            }
        );
        eprintln!(
            " Cache    {} read · {} written",
            agent.usage.cache_read, agent.usage.cache_write
        );
        eprintln!(" Saved    ~{} tokens via result cache", saved_bytes / 4);
        eprintln!("────────────────────────────────────────────────────────────");
    }
}

/// Persist a session both ways the app reads them: SQLite (what `--session`
/// resumes) and the JSON file under `~/.wrosecode/sessions/` that `/sessions`
/// and `/tree` list, so headless runs show up in the TUI like TUI ones do.
fn persist_session(store: &store::Store, session: &session::Session) -> Result<()> {
    store.save_session(session)?;
    session.save(&session::Session::dir()?)?;
    Ok(())
}

/// `--goal`, `--until`, and `--until-cmd` over a model answer — the same
/// verdict the prompt branch, `exec`, and `recipe run` all report.
fn text_goals_match(goal: Option<&str>, until: Option<&str>, response: &str) -> Result<bool> {
    // The criteria are conjunctive: a successful verification command must
    // not overwrite a failing flag-pattern test, and vice versa.
    if goal.is_some_and(|text| !response.contains(text)) {
        return Ok(false);
    }
    if let Some(pattern) = until {
        return Ok(regex::Regex::new(pattern)?.is_match(response));
    }
    Ok(true)
}

fn verification_requested(
    goal: Option<&String>,
    until: Option<&String>,
    until_cmd: Option<&String>,
) -> bool {
    goal.is_some() || until.is_some() || until_cmd.is_some()
}

async fn run_bounded_goal_command(
    root: &Path,
    command: &str,
    max_duration: std::time::Duration,
) -> Result<bool> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Isolate child scripts in their own process group so a blocked
        // shell, Python interpreter, and descendants all get terminated.
        cmd.as_std_mut().process_group(0);
    }
    let mut child = cmd
        .spawn()
        .context("verification command could not execute")?;
    let result = tokio::time::timeout(max_duration, child.wait()).await;
    match result {
        Ok(Ok(status)) => Ok(status.success()),
        Ok(Err(error)) => Err(error).context("verification command wait failed"),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // A new group was created with the child's PID above.
                // No arbitrary user process groups are ever targeted.
                if pid <= i32::MAX as u32 {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
            eprintln!(
                "verification command exceeded {}s; goal remains unverified",
                max_duration.as_secs()
            );
            Ok(false)
        }
    }
}

async fn check_goal(
    agent: &Agent,
    goal: Option<&String>,
    until: Option<&String>,
    until_cmd: Option<&String>,
    response: &str,
) -> Result<bool> {
    // A successful model response is not proof of a tested solution.
    // Without an explicit goal/pattern/check, finish normally but report
    // unverified rather than a misleading Solved & verified.
    if !verification_requested(goal, until, until_cmd) {
        return Ok(false);
    }
    if !text_goals_match(
        goal.map(String::as_str),
        until.map(String::as_str),
        response,
    )? {
        return Ok(false);
    }
    if let Some(command) = until_cmd {
        // Verification is bounded independently from the main model turn.
        return run_bounded_goal_command(
            &agent.config.root,
            command,
            std::time::Duration::from_secs(30),
        )
        .await;
    }
    Ok(true)
}

fn attach_events(agent: &mut Agent, path: Option<&Path>) -> Result<Option<EventStream>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let writer = std::sync::Arc::new(std::sync::Mutex::new(events::EventWriter::open(path)?));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = writer.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Some(frame) = events::frame(&event) {
                // A poisoned sink drops the frame instead of aborting the
                // whole process mid-turn.
                if let Ok(mut sink) = sink.lock() {
                    let _ = sink.write(&frame);
                }
            }
        }
    });
    agent.event_tx = Some(tx);
    Ok(Some(EventStream {
        sink: writer,
        forwarder: Some(forwarder),
    }))
}

fn write_event(stream: &Option<EventStream>, frame: &serde_json::Value) {
    if let Some(stream) = stream {
        if let Ok(mut sink) = stream.sink.lock() {
            let _ = sink.write(frame);
        }
    }
}

#[cfg(test)]
mod verbosity_cli_tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn model_answer_alone_is_not_externally_verified() {
        use super::verification_requested;
        assert!(!verification_requested(None, None, None));
        let goal = "HTB{".to_string();
        assert!(verification_requested(Some(&goal), None, None));
        let checker = "python3 solve.py".to_string();
        assert!(verification_requested(None, None, Some(&checker)));
    }

    #[test]
    fn all_requested_ctf_proof_checks_must_match() {
        use super::text_goals_match;
        assert!(text_goals_match(
            Some("HTB{"),
            Some(r"HTB\{[a-z0-9]+\}"),
            "verified HTB{a1b2}"
        )
        .unwrap());
        assert!(!text_goals_match(Some("HTB{"), Some(r"FLAG\{[a-z0-9]+\}"), "HTB{a1b2}").unwrap());
        assert!(!text_goals_match(Some("HTB{"), Some(r"FLAG\{[a-z0-9]+\}"), "FLAG{a1b2}").unwrap());
        assert!(text_goals_match(None, None, "ordinary chat").unwrap());
        assert!(text_goals_match(None, Some(r"success"), "success").unwrap());
    }

    #[tokio::test]
    async fn verification_processes_do_not_freeze_on_hanging_scripts() {
        use super::run_bounded_goal_command;
        use std::time::{Duration, Instant};
        let root = std::path::Path::new("/tmp");
        assert!(
            run_bounded_goal_command(root, "true", Duration::from_secs(2))
                .await
                .unwrap()
        );
        assert!(
            !run_bounded_goal_command(root, "false", Duration::from_secs(2))
                .await
                .unwrap()
        );
        let started = Instant::now();
        assert!(
            !run_bounded_goal_command(root, "while :; do :; done", Duration::from_millis(70))
                .await
                .unwrap()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "verification timeout failed to interrupt a blocked shell"
        );
    }

    #[test]
    fn users_can_override_global_verbose_default_per_launch() {
        let default = Cli::try_parse_from(["wrosecode"]).expect("default CLI parses");
        assert_eq!(default.verbosity, None);
        let off = Cli::try_parse_from(["wrosecode", "--verbosity", "off"])
            .expect("explicit opt-out parses");
        assert_eq!(off.verbosity.as_deref(), Some("off"));
        let on = Cli::try_parse_from(["wrosecode", "--verbosity", "on"])
            .expect("explicit opt-in parses");
        assert_eq!(on.verbosity.as_deref(), Some("on"));
        assert!(Cli::try_parse_from(["wrosecode", "--verbosity", "invalid"]).is_err());
    }
}
