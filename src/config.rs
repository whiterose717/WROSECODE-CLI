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
    pub thinking_level: u8,
    pub max_parallel_tasks: usize,
    pub shell_timeout_seconds: u64,
    pub tool_retries: usize,
    pub fallback_provider: String,
    pub fallback_model: String,
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
    pub thinking_level: u8,
    pub max_parallel_tasks: usize,
    pub shell_timeout_seconds: u64,
    pub tool_retries: usize,
    pub fallback_provider: String,
    pub fallback_model: String,
    pub budget_usd: f64,
    pub permission: Permission,
}

impl Default for AgentRuntimeConfig {
    fn default() -> Self {
        Self {
            thinking_level: 5,
            max_parallel_tasks: 20,
            shell_timeout_seconds: 30,
            tool_retries: 3,
            fallback_provider: String::new(),
            fallback_model: String::new(),
            budget_usd: 0.0,
            permission: Permission::Ask,
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

        assert_eq!(runtime.agent.thinking_level, 5);
        assert_eq!(runtime.agent.max_parallel_tasks, 20);
        assert_eq!(runtime.agent.shell_timeout_seconds, 30);
        assert_eq!(runtime.agent.tool_retries, 3);
        assert_eq!(runtime.agent.permission, Permission::Ask);
        assert_eq!(runtime.agent.budget_usd, 0.0);

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
