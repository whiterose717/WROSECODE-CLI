use crate::think::ThinkLevel;
use clap::ValueEnum;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    Ask,
    AutoSafe,
    Yolo,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    pub permission: Permission,
    pub model: String,
    pub provider: String,
    pub harness: String,
    pub repair_retries: usize,
    pub check_command: Option<String>,
    pub skill_dirs: Vec<PathBuf>,
    /// The configured thinking mode (`off | low | medium | high | max | auto`).
    /// `thinking_level` below carries the live 0–20 strength this mode maps
    /// to; `auto` rewrites it as the run progresses.
    pub think: ThinkLevel,
    pub thinking_level: u8,
    pub max_parallel_tasks: usize,
    pub shell_timeout_seconds: u64,
    pub tool_retries: usize,
    pub fallback_provider: String,
    pub fallback_model: String,
    /// The goose planner/worker split: `planner_provider`/`planner_model`
    /// name the model that runs the read-only `plan` mode. Empty = the
    /// worker (main provider) runs every mode.
    pub planner_provider: String,
    pub planner_model: String,
    /// aider-style git auto-commit: after an edit turn that passed its
    /// checks, commit exactly the paths the agent touched.
    pub auto_commit: bool,
    pub redis_url: Option<String>,
    pub budget_usd: f64,
    pub qdrant_url: Option<String>,
    pub ui_theme: String,
    pub verbosity: String,
    pub alternate_screen: bool,
    /// "auto" lets WROSECODE detect terminal quirks, "on"/"off" force mouse capture.
    pub mouse_capture: Option<String>,
    pub alert_bell: bool,
    pub smooth_scroll_lines: usize,
    pub sandbox: crate::sandbox::SandboxPolicy,
    /// Language-server feedback after edits (`[lsp]` in config.toml).
    pub lsp: crate::lsp::LspSettings,
    /// open-interpreter parity: expose the opt-in `computer` tool (OS
    /// control). Set by `[tools] computer = true` or `--computer`; its
    /// schema is only advertised to the model when this is on.
    pub computer_tools: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub ui: UiRuntimeConfig,
    #[serde(default)]
    pub agent: AgentRuntimeConfig,
    #[serde(default)]
    pub cache: CacheRuntimeConfig,
    #[serde(default)]
    pub sandbox: SandboxRuntimeConfig,
    #[serde(default)]
    pub lsp: LspRuntimeConfig,
    #[serde(default)]
    pub tools: ToolsRuntimeConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct UiRuntimeConfig {
    pub theme: String,
    pub verbosity: String,
    pub alternate_screen: bool,
    /// "auto" detects VS Code / tmux quirks, "on" forces capture, "off" disables it.
    pub mouse_capture: Option<String>,
    pub alert_bell: bool,
    pub smooth_scroll_lines: usize,
}

impl Default for UiRuntimeConfig {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            verbosity: "normal".into(),
            alternate_screen: true,
            mouse_capture: Some("auto".into()),
            alert_bell: true,
            smooth_scroll_lines: 1,
        }
    }
}

/// `[tools]` in config.toml — opt-in tool surfaces. Everything here ships
/// off so a fresh install never offers capability the user did not ask for.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ToolsRuntimeConfig {
    /// open-interpreter parity: register the `computer` tool (screenshot /
    /// click / type / key / scroll on this machine's display, through the
    /// grim-family and xdotool backends). `--computer` turns the same
    /// switch on from the CLI.
    pub computer: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct CacheRuntimeConfig {
    /// "memory" keeps every cache entry inside this process. "redis" additionally
    /// allows a shared cache when `redis_url` is set.
    pub backend: String,
    pub redis_url: Option<String>,
    pub qdrant_url: Option<String>,
}

impl Default for CacheRuntimeConfig {
    fn default() -> Self {
        Self {
            backend: "memory".into(),
            redis_url: None,
            qdrant_url: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AgentRuntimeConfig {
    /// The `[agent] think` key from config.toml — the default thinking mode
    /// when neither `--think` nor a providers.toml profile sets one.
    pub think: Option<ThinkLevel>,
    pub thinking_level: u8,
    pub max_parallel_tasks: usize,
    pub shell_timeout_seconds: u64,
    pub tool_retries: usize,
    pub fallback_provider: String,
    pub fallback_model: String,
    /// `planner = "provider/model"` (or just `"model"` for the main
    /// provider) in `[agent]`: the model that thinks in `plan` mode.
    pub planner: Option<String>,
    /// Commit the agent's edits after each checked turn (aider's
    /// autocommit). Default on, matching aider; `[agent] auto_commit = false`
    /// keeps commits manual via `/commit`.
    pub auto_commit: bool,
    pub budget_usd: f64,
    pub permission: Permission,
}

impl Default for AgentRuntimeConfig {
    fn default() -> Self {
        Self {
            think: None,
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: 30,
            tool_retries: 3,
            fallback_provider: String::new(),
            fallback_model: String::new(),
            planner: None,
            auto_commit: true,
            budget_usd: 0.0,
            permission: Permission::Ask,
        }
    }
}

impl AgentRuntimeConfig {
    /// Split `[agent] planner` into `(provider, model)`. A bare model name
    /// keeps `main_provider` (the provider the worker runs on); a
    /// `provider/model` pair overrides it; anything malformed disables the
    /// split so a typo falls back to the worker instead of failing startup.
    pub fn planner_split(&self, main_provider: &str) -> Option<(String, String)> {
        let spec = self.planner.as_deref()?.trim();
        if spec.is_empty() {
            return None;
        }
        match spec.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                Some((provider.trim().to_string(), model.trim().to_string()))
            }
            Some(_) => None,
            None => Some((main_provider.to_string(), spec.to_string())),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SandboxRuntimeConfig {
    pub engine: String,
    pub image: String,
    pub persistent: bool,
    pub name: String,
    pub network: String,
    pub memory: String,
    pub cpus: String,
    pub auto_build: bool,
}

impl Default for SandboxRuntimeConfig {
    fn default() -> Self {
        let policy = crate::sandbox::SandboxPolicy::default();
        Self {
            engine: policy.engine,
            image: policy.image,
            persistent: policy.persistent,
            name: policy.name,
            network: policy.network,
            memory: policy.memory,
            cpus: policy.cpus,
            auto_build: policy.auto_build,
        }
    }
}

impl SandboxRuntimeConfig {
    pub fn policy(&self) -> crate::sandbox::SandboxPolicy {
        crate::sandbox::SandboxPolicy {
            engine: self.engine.clone(),
            image: self.image.clone(),
            persistent: self.persistent,
            name: self.name.clone(),
            network: self.network.clone(),
            memory: self.memory.clone(),
            cpus: self.cpus.clone(),
            auto_build: self.auto_build,
        }
    }
}

/// `[lsp]` in config.toml. Only the keys a user actually sets are listed here;
/// `into_settings` folds them over the built-in per-language defaults, so
/// `[lsp.commands] rs = ["rust-analyzer", "--log-file", "ra.log"]` replaces
/// just the Rust entry while the other languages keep theirs.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct LspRuntimeConfig {
    pub enabled: bool,
    pub wait_ms: u64,
    /// extension → server argv; each entry overrides the built-in one.
    pub commands: std::collections::BTreeMap<String, Vec<String>>,
}

impl Default for LspRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wait_ms: 1_200,
            commands: std::collections::BTreeMap::new(),
        }
    }
}

impl LspRuntimeConfig {
    pub fn into_settings(self) -> crate::lsp::LspSettings {
        let mut settings = crate::lsp::LspSettings {
            enabled: self.enabled,
            wait_ms: self.wait_ms,
            ..Default::default()
        };
        for (extension, argv) in self.commands {
            if argv.is_empty() {
                settings.commands.remove(&extension);
            } else {
                settings.commands.insert(extension, argv);
            }
        }
        settings
    }
}

impl RuntimeConfig {
    pub fn load(root: &std::path::Path) -> Self {
        std::fs::read_to_string(root.join("config.toml"))
            .ok()
            .and_then(|source| toml::from_str(&source).ok())
            .unwrap_or_else(|| Self {
                ui: UiRuntimeConfig::default(),
                agent: AgentRuntimeConfig::default(),
                cache: CacheRuntimeConfig::default(),
                sandbox: SandboxRuntimeConfig::default(),
                lsp: LspRuntimeConfig::default(),
                tools: ToolsRuntimeConfig::default(),
            })
    }
}

impl Config {
    pub fn check_command(&self) -> Option<String> {
        if let Some(cmd) = &self.check_command {
            return Some(cmd.clone());
        }
        if self.root.join("Cargo.toml").exists() {
            return Some("cargo test --quiet".into());
        }
        if self.root.join("package.json").exists() {
            let package = std::fs::read_to_string(self.root.join("package.json")).ok()?;
            let json: serde_json::Value = serde_json::from_str(&package).ok()?;
            if json["scripts"]["test"].is_string() {
                return Some("npm test --silent".into());
            }
            if json["scripts"]["lint"].is_string() {
                return Some("npm run lint --silent".into());
            }
            return None;
        }
        if self.root.join("pyproject.toml").exists() {
            return Some("python -m pytest -q".into());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> RuntimeConfig {
        toml::from_str(source).expect("config.toml must parse")
    }

    #[test]
    fn missing_config_falls_back_to_defaults() {
        let runtime = RuntimeConfig::load(std::path::Path::new("/nonexistent"));
        assert_eq!(runtime.ui.theme, "dark");
        assert_eq!(runtime.agent.thinking_level, 5);
        assert_eq!(runtime.agent.permission, Permission::Ask);
        assert_eq!(runtime.cache.backend, "memory");
        assert_eq!(runtime.sandbox.engine, "none");
        assert!(!runtime.tools.computer, "OS control ships off");
    }

    #[test]
    fn planner_split_reads_pairs_bare_models_and_rejects_junk() {
        let pair = AgentRuntimeConfig {
            planner: Some("anthropic/claude-haiku".into()),
            ..AgentRuntimeConfig::default()
        };
        assert_eq!(
            pair.planner_split("openai").unwrap(),
            ("anthropic".into(), "claude-haiku".into())
        );
        // A model name may itself contain slashes: only the first one
        // separates the provider.
        let nested = AgentRuntimeConfig {
            planner: Some("openai/org/model-x".into()),
            ..AgentRuntimeConfig::default()
        };
        assert_eq!(
            nested.planner_split("anthropic").unwrap(),
            ("openai".into(), "org/model-x".into())
        );
        // A bare model keeps the worker's provider.
        let bare = AgentRuntimeConfig {
            planner: Some("  gpt-5-codex ".into()),
            ..AgentRuntimeConfig::default()
        };
        assert_eq!(
            bare.planner_split("openai").unwrap(),
            ("openai".into(), "gpt-5-codex".into())
        );
        // Malformed and absent specs disable the split instead of failing
        // startup — a typo falls back to the worker.
        for junk in [
            Some("anthropic/".into()),
            Some("/m".into()),
            Some("".into()),
            None,
        ] {
            let config = AgentRuntimeConfig {
                planner: junk,
                ..AgentRuntimeConfig::default()
            };
            assert!(config.planner_split("openai").is_none());
        }
    }

    #[test]
    fn every_shipped_section_is_honoured() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml"),
        )
        .expect("config.toml ships with the crate");
        let runtime = parse(&source);

        assert_eq!(runtime.ui.theme, "wrose-dark");
        assert_eq!(runtime.ui.verbosity, "normal");
        assert!(runtime.ui.alternate_screen);
        assert_eq!(runtime.ui.smooth_scroll_lines, 1);

        assert_eq!(runtime.agent.think, Some(ThinkLevel::Medium));
        assert_eq!(runtime.agent.thinking_level, 5);
        assert_eq!(runtime.agent.max_parallel_tasks, 20);
        assert_eq!(runtime.agent.shell_timeout_seconds, 30);
        assert_eq!(runtime.agent.tool_retries, 3);
        assert_eq!(runtime.agent.permission, Permission::Ask);
        assert_eq!(runtime.agent.budget_usd, 0.0);
        // The planner/worker split ships commented out: one model per mode
        // until the user names a planner.
        assert_eq!(runtime.agent.planner, None);
        assert!(runtime.agent.planner_split("openai").is_none());
        assert!(
            runtime.agent.auto_commit,
            "aider-style auto-commit ships on"
        );

        assert_eq!(runtime.cache.backend, "memory");
        assert!(runtime.cache.redis_url.is_some());
        assert!(runtime.cache.qdrant_url.is_some());

        assert_eq!(runtime.sandbox.engine, "none");
        assert_eq!(runtime.sandbox.image, "wrosecode-sandbox:latest");
        assert!(runtime.sandbox.persistent);
        assert_eq!(runtime.sandbox.name, "wrosecode-sandbox");
        assert_eq!(runtime.sandbox.network, "bridge");
        assert_eq!(runtime.sandbox.memory, "2g");
        assert_eq!(runtime.sandbox.cpus, "2.0");
        assert!(!runtime.sandbox.auto_build);

        let policy = runtime.sandbox.policy();
        assert!(!policy.engine.is_empty());

        assert!(runtime.lsp.enabled);
        assert_eq!(runtime.lsp.wait_ms, 1200);
        assert!(
            !runtime.tools.computer,
            "the shipped file keeps OS control opt-in"
        );
        let settings = runtime.lsp.into_settings();
        assert!(settings.enabled, "the shipped file keeps diagnostics on");
        assert_eq!(settings.wait_ms, 1200);
        assert_eq!(
            settings.command_for("rs"),
            Some(["rust-analyzer".to_string()].as_slice()),
            "an untouched extension keeps its built-in server"
        );
    }

    #[test]
    fn lsp_commands_override_one_language_and_empty_removes_it() {
        let runtime = parse(
            r#"
            [lsp]
            enabled = false
            wait_ms = 50
            [lsp.commands]
            rs = ["rust-analyzer", "--log-file", "ra.log"]
            py = []
            "#,
        );
        assert!(!runtime.lsp.enabled);
        assert_eq!(runtime.lsp.wait_ms, 50);
        let settings = runtime.lsp.into_settings();
        assert_eq!(
            settings.command_for("rs"),
            Some(
                [
                    "rust-analyzer".to_string(),
                    "--log-file".to_string(),
                    "ra.log".to_string()
                ]
                .as_slice()
            ),
            "the configured argv replaces the built-in for that language"
        );
        assert_eq!(settings.command_for("py"), None, "empty list removes it");
        assert_eq!(
            settings.command_for("go"),
            Some(["gopls".to_string()].as_slice()),
            "languages nobody configured keep their defaults"
        );
    }

    #[test]
    fn unknown_sections_and_keys_are_ignored() {
        let runtime = parse(
            r#"
            [ui]
            theme = "dracula"
            made_up = 1
            [future]
            anything = true
            "#,
        );
        assert_eq!(runtime.ui.theme, "dracula");
        assert_eq!(runtime.agent.thinking_level, 5);
    }

    #[test]
    fn auto_commit_defaults_on_and_can_be_disabled() {
        let runtime = parse("[ui]\n");
        assert!(runtime.agent.auto_commit, "absent key defaults to on");
        let runtime = parse("[agent]\nauto_commit = false\n");
        assert!(!runtime.agent.auto_commit, "the gate turns it off");
        // A wrong type is an error, not a silent default.
        assert!(toml::from_str::<RuntimeConfig>("[agent]\nauto_commit = \"yes\"\n").is_err());
    }

    #[test]
    fn computer_tools_ship_off_and_can_be_enabled() {
        assert!(!parse("[ui]\n").tools.computer, "absent key stays off");
        assert!(parse("[tools]\ncomputer = true\n").tools.computer);
        // A wrong type is an error, not a silent default.
        assert!(toml::from_str::<RuntimeConfig>("[tools]\ncomputer = \"yes\"\n").is_err());
    }

    #[test]
    fn thinking_mode_parses_every_level_and_rejects_typos() {
        for (name, level) in [
            ("off", ThinkLevel::Off),
            ("low", ThinkLevel::Low),
            ("medium", ThinkLevel::Medium),
            ("high", ThinkLevel::High),
            ("max", ThinkLevel::Max),
            ("auto", ThinkLevel::Auto),
        ] {
            let runtime = parse(&format!("[agent]\nthink = \"{name}\"\n"));
            assert_eq!(runtime.agent.think, Some(level));
        }
        // An unknown level is a config error, not a silent default.
        let source = "[agent]\nthink = \"deep\"\n";
        assert!(toml::from_str::<RuntimeConfig>(source).is_err());
        // Without the key, the legacy numeric level still decides the mode.
        let runtime = parse("[agent]\nthinking_level = 17\n");
        assert_eq!(runtime.agent.think, None);
        assert_eq!(runtime.agent.thinking_level, 17);
        assert_eq!(
            ThinkLevel::from_level(runtime.agent.thinking_level),
            ThinkLevel::High
        );
    }

    #[test]
    fn redis_is_only_used_when_backend_says_redis() {
        let runtime = parse(
            r#"
            [cache]
            backend = "memory"
            redis_url = "redis://127.0.0.1/"
            "#,
        );
        assert_eq!(runtime.cache.backend, "memory");
        assert!(runtime.cache.redis_url.is_some());

        let runtime = parse(
            r#"
            [cache]
            backend = "redis"
            redis_url = "redis://127.0.0.1/"
            "#,
        );
        assert_eq!(runtime.cache.backend, "redis");
    }

    #[test]
    fn permission_accepts_kebab_case_from_toml() {
        assert_eq!(
            parse("[agent]\npermission = \"auto-safe\"\n")
                .agent
                .permission,
            Permission::AutoSafe
        );
        assert_eq!(
            parse("[agent]\npermission = \"yolo\"\n").agent.permission,
            Permission::Yolo
        );
        assert_eq!(
            parse("[agent]\npermission = \"ask\"\n").agent.permission,
            Permission::Ask
        );
    }

    #[test]
    fn ctf_section_is_parsed_by_the_engine_config() {
        #[derive(serde::Deserialize)]
        struct CtfOnly {
            ctf: CtfSection,
        }
        #[derive(serde::Deserialize, Default)]
        struct CtfSection {
            #[serde(default)]
            flag_patterns: Vec<String>,
            #[serde(default)]
            auto_copy: Option<bool>,
            #[serde(default)]
            auto_submit: Option<bool>,
        }
        let parsed: CtfOnly = toml::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.ctf.flag_patterns.len(), 4);
        assert_eq!(parsed.ctf.auto_copy, Some(true));
        assert_eq!(parsed.ctf.auto_submit, Some(false));
    }
}
