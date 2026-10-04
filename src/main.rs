mod acp;
mod agent;
mod api;
mod attach;
mod autopilot;
mod commands;
mod config;
mod crash;
mod ctf;
mod ctfd;
mod events;
mod harness;
mod memory;
mod metrics;
mod project;
mod provider;
mod provider_cli;
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
    #[arg(long, default_value = "anthropic")]
    provider: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "claude")]
    harness: String,
    #[arg(long, default_value_t = 3)]
    repair_retries: usize,
    #[arg(long)]
    check_command: Option<String>,
    #[arg(long = "skills")]
    skill_dirs: Vec<PathBuf>,
    #[arg(long)]
    mcp_bin: Option<String>,
    #[arg(long = "mcp-arg")]
    mcp_args: Vec<String>,
    #[arg(long)]
    quiet: bool,
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
    #[arg(long)]
    summary: Option<String>,
    #[arg(long)]
    headless: bool,
    #[arg(long)]
    web: bool,
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: String,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    fork: bool,
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
    /// CTF autopilot: run budget — bare steps, or steps=…,tokens=…,seconds=…
    #[arg(long, value_name = "STEPS|steps=…,tokens=…,seconds=…")]
    budget: Option<String>,
    /// CTF autopilot: max parallel hypothesis subagents
    #[arg(long, default_value_t = 0)]
    parallel: usize,
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
    let mut exec_json = false;
    let mut ctf_mode = false;
    let cli_args: Vec<String> = if raw_args.get(1).is_some_and(|argument| argument == "exec") {
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
    } else {
        raw_args.clone()
    };
    let cli = Cli::parse_from(cli_args);
    let root = std::env::current_dir()?;
    let store = store::Store::open_default()?;
    if let Some(path) = &cli.import_session {
        let imported = session::Session::load(path)?;
        store.save_session(&imported)?;
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
    let settings = settings::Settings::load()?;
    let selected_model = cli.model.unwrap_or_else(|| {
        settings
            .profile(&cli.provider)
            .map(|profile| profile.model.clone())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| {
                if cli.provider == "anthropic" {
                    "claude-sonnet-5".into()
                } else {
                    "local-model".into()
                }
            })
    });
    // Thinking mode precedence: --think > the provider profile's think key
    // (model-specific) > [agent].think from config.toml > derived from the
    // legacy numeric thinking_level. An explicit mode rewrites the effective
    // 0–20 strength to its anchor; without one the legacy number stands.
    let configured_think = cli
        .think
        .or_else(|| {
            settings
                .profile(&cli.provider)
                .and_then(|profile| profile.think)
        })
        .or(runtime.agent.think);
    let think = configured_think
        .unwrap_or_else(|| ThinkLevel::from_level(runtime.agent.thinking_level.min(20)));
    let config = Arc::new(Config {
        root,
        permission: cli.permission.unwrap_or(runtime.agent.permission),
        model: selected_model,
        provider: cli.provider,
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
        redis_url: runtime
            .cache
            .redis_url
            .filter(|_| runtime.cache.backend.eq_ignore_ascii_case("redis")),
        budget_usd: runtime.agent.budget_usd.max(0.0),
        qdrant_url: runtime.cache.qdrant_url,
        ui_theme: runtime.ui.theme,
        verbosity: runtime.ui.verbosity,
        alternate_screen: runtime.ui.alternate_screen,
        mouse_capture: runtime.ui.mouse_capture,
        alert_bell: runtime.ui.alert_bell,
        smooth_scroll_lines: runtime.ui.smooth_scroll_lines.clamp(1, 20),
        sandbox: runtime.sandbox.policy(),
    });
    // Take the terminal and paint the splash before provider setup, skill
    // discovery, metrics, MCP servers, and session restore: those are what
    // would otherwise sit between the user and their first frame.
    let tui_mode = cli.prompt.is_none()
        && !cli.headless
        && !ctf_mode
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let guard = if tui_mode {
        let profile = tui::TerminalProfile::detect(
            config.alternate_screen,
            config.mouse_capture.as_deref(),
            cli.no_color,
        );
        let guard = tui::TerminalGuard::enter(profile)?;
        splash::paint(profile, &boot)?;
        Some(guard)
    } else {
        None
    };
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(8)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
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
    let mut agent = Agent::new(config, provider, client)?;
    let resumed_session = if let Some(id) = &cli.session {
        let loaded = store
            .load_session(id)?
            .with_context(|| format!("session {id} was not found"))?;
        agent.messages = loaded.messages.clone();
        Some(if cli.fork {
            let mut forked = session::Session::fresh();
            forked.summary = format!("Fork of {}", loaded.name);
            forked.messages = loaded.messages;
            forked.transcript = loaded.transcript;
            forked
        } else {
            loaded
        })
    } else {
        None
    };
    if let Some(binary) = cli.mcp_bin.as_deref() {
        agent.tools.mcps.push((
            "cli".into(),
            tools::mcp::Mcp::connect(binary, &cli.mcp_args).await?,
        ));
    }
    for server in &settings.mcps {
        if let Ok(mcp) = tools::mcp::Mcp::connect(&server.bin, &server.args).await {
            agent.tools.mcps.push((server.name.clone(), mcp));
        }
    }
    let mut event_sink = attach_events(&mut agent, cli.events.as_deref())?;
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
        store.save_session(&session)?;
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
    if cli.prompt.as_deref() == Some("acp") {
        return acp::serve(agent).await;
    }
    if cli.web {
        return api::serve(agent, store, &cli.listen).await;
    }
    if let Some(prompt) = cli.prompt {
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
        let mut verified = cli.goal.as_ref().is_none_or(|goal| response.contains(goal));
        if let Some(pattern) = &cli.until {
            verified = regex::Regex::new(pattern)?.is_match(&response);
        }
        if let Some(command) = &cli.until_cmd {
            verified = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&agent.config.root)
                .status()
                .await?
                .success();
        }
        if let Some(stream) = &mut event_sink {
            stream.quiesce(&mut agent).await;
        }
        write_event(
            &event_sink,
            &events::result_frame(&response, verified, &agent.usage, agent.model_turns),
        );
        let json_summary = cli.summary.as_deref() == Some("json");
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
        } else if !cli.quiet && !json_summary {
            println!("{response}");
        }
        if json_summary && !exec_json {
            println!(
                "{}",
                serde_json::json!({
                    "type": "TaskComplete",
                    "status": if verified { "verified" } else { "unverified" },
                    "answer": &response,
                    "usage": agent.usage,
                    "model_turns": agent.model_turns,
                    "tool_cache_hits": agent.tools.cache_stats().0,
                    "elapsed_ms": agent.started_at.elapsed().as_millis(),
                })
            );
        } else if !cli.no_summary && !exec_json {
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
        store.save_session(&session)?;
        return Ok(());
    }
    if cli.headless {
        anyhow::bail!("--headless requires a prompt argument or piped input");
    }
    if let Some(guard) = guard {
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
        agents.push(Agent::new(Arc::new(config), provider, client.clone())?);
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
                let _ = sink.lock().expect("event sink").write(&frame);
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
        let _ = stream.sink.lock().expect("event sink").write(frame);
    }
}
