use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderProfile {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub model: String,
    pub key_ref: String,
    pub headers: BTreeMap<String, String>,
    pub builtin: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServerDef {
    pub name: String,
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct McpFile {
    #[serde(default)]
    servers: Vec<McpServerDef>,
}

pub struct Settings {
    pub dir: PathBuf,
    pub providers: Vec<ProviderProfile>,
    pub mcps: Vec<McpServerDef>,
    pub last_tests: HashMap<String, std::result::Result<Duration, String>>,
}

const DEFAULTS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "anthropic",
        "anthropic",
        "https://api.anthropic.com",
        "claude-sonnet-5",
        "ANTHROPIC_API_KEY",
    ),
    (
        "openai",
        "openai_compat",
        "https://api.openai.com/v1",
        "",
        "OPENAI_API_KEY",
    ),
    (
        "azure-openai",
        "openai_compat",
        "",
        "",
        "AZURE_OPENAI_API_KEY",
    ),
    (
        "gemini",
        "openai_compat",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "",
        "GEMINI_API_KEY",
    ),
    ("vertex-ai", "openai_compat", "", "", "VERTEX_API_KEY"),
    ("bedrock", "openai_compat", "", "", "BEDROCK_API_KEY"),
    (
        "groq",
        "openai_compat",
        "https://api.groq.com/openai/v1",
        "",
        "GROQ_API_KEY",
    ),
    (
        "ollama",
        "openai_compat",
        "http://localhost:11434/v1",
        "",
        "",
    ),
    (
        "openrouter",
        "openai_compat",
        "https://openrouter.ai/api/v1",
        "",
        "OPENROUTER_API_KEY",
    ),
    ("databricks", "openai_compat", "", "", "DATABRICKS_TOKEN"),
    (
        "litellm",
        "openai_compat",
        "http://localhost:4000/v1",
        "",
        "",
    ),
    (
        "github-copilot",
        "openai_compat",
        "https://api.githubcopilot.com",
        "",
        "GITHUB_COPILOT_TOKEN",
    ),
];

impl Settings {
    pub fn load() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Self::load_at(home.join(".wrosecode"))
    }
    pub fn load_at(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("providers.toml");
        let source = std::fs::read_to_string(&file).unwrap_or_default();
        let mut doc = source
            .parse::<DocumentMut>()
            .context("invalid providers.toml")?;
        let legacy: Vec<ProviderProfile> = doc
            .get("providers")
            .and_then(Item::as_array_of_tables)
            .map(|array| {
                array
                    .iter()
                    .filter_map(|table| {
                        let name = table["name"].as_str()?.to_string();
                        Some(ProviderProfile {
                            builtin: Self::builtin(&name),
                            name,
                            kind: match table["kind"].as_str().unwrap_or("openai-compatible") {
                                "openai-compatible" | "openai" | "ollama" => "openai_compat",
                                other => other,
                            }
                            .into(),
                            base_url: table["base_url"].as_str().unwrap_or_default().into(),
                            model: table["default_model"].as_str().unwrap_or_default().into(),
                            key_ref: table["api_key_env"]
                                .as_str()
                                .map(|env| format!("env:{env}"))
                                .unwrap_or_default(),
                            headers: BTreeMap::new(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !doc.contains_key("provider") {
            doc["provider"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        let array = doc["provider"]
            .as_array_of_tables_mut()
            .context("provider must be an array of tables")?;
        let mut changed = !file.exists();
        for profile in legacy {
            if !array
                .iter()
                .any(|table| table["name"].as_str() == Some(&profile.name))
            {
                array.push(profile_table(&profile));
                changed = true;
            }
        }
        for &(name, kind, base_url, model, env) in DEFAULTS {
            if !array
                .iter()
                .any(|table| table["name"].as_str() == Some(name))
            {
                array.push(profile_table(&ProviderProfile {
                    name: name.into(),
                    kind: kind.into(),
                    base_url: base_url.into(),
                    model: model.into(),
                    key_ref: if env.is_empty() {
                        "".into()
                    } else {
                        format!("env:{env}")
                    },
                    headers: BTreeMap::new(),
                    builtin: true,
                }));
                changed = true;
            }
        }
        if changed {
            std::fs::write(&file, doc.to_string())?;
        }
        let providers = parse_profiles(&doc)?;
        let mcps = read_toml::<McpFile>(&dir.join("mcps.toml"))?.servers;
        Ok(Self {
            dir,
            providers,
            mcps,
            last_tests: HashMap::new(),
        })
    }
    pub fn profile(&self, name: &str) -> Option<&ProviderProfile> {
        self.providers
            .iter()
            .find(|p| p.name == name)
            .or_else(|| match name {
                "openai-compat" => self.providers.iter().find(|p| p.name == "openai"),
                "lm-studio" => self.providers.iter().find(|p| p.name == "litellm"),
                _ => None,
            })
    }
    pub fn builtin(name: &str) -> bool {
        DEFAULTS.iter().any(|entry| entry.0 == name)
    }
    pub fn validate_name(name: &str) -> Result<()> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            bail!("provider name must match [a-z0-9-_]+");
        }
        Ok(())
    }
    pub fn normalize_url(url: &str) -> Result<String> {
        if url.is_empty() {
            return Ok(String::new());
        }
        let parsed = reqwest::Url::parse(url).context("base URL is invalid")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("base URL must use http or https");
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!("base URL cannot contain a query or fragment");
        }
        Ok(url.trim_end_matches('/').to_string())
    }
    pub fn insecure_http(url: &str) -> bool {
        reqwest::Url::parse(url).ok().is_some_and(|u| {
            u.scheme() == "http" && !matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
        })
    }
    pub fn upsert_provider(&mut self, mut profile: ProviderProfile) -> Result<()> {
        Self::validate_name(&profile.name)?;
        profile.base_url = Self::normalize_url(&profile.base_url)?;
        if !matches!(profile.kind.as_str(), "openai_compat" | "anthropic") {
            bail!("kind must be openai_compat or anthropic");
        }
        let path = self.dir.join("providers.toml");
        let mut doc = std::fs::read_to_string(&path)?.parse::<DocumentMut>()?;
        let array = doc["provider"]
            .as_array_of_tables_mut()
            .context("provider must be an array of tables")?;
        let index = array
            .iter()
            .position(|table| table["name"].as_str() == Some(&profile.name));
        if let Some(index) = index {
            update_table(
                array.get_mut(index).context("provider index missing")?,
                &profile,
            );
        } else {
            array.push(profile_table(&profile));
        }
        std::fs::write(path, doc.to_string())?;
        self.providers = parse_profiles(&doc)?;
        self.last_tests.remove(&profile.name);
        Ok(())
    }
    pub fn remove_provider(&mut self, name: &str) -> Result<()> {
        if Self::builtin(name) {
            bail!("built-in provider cannot be removed");
        }
        if self.profile(name).is_none() {
            bail!("unknown provider: {name}");
        }
        let path = self.dir.join("providers.toml");
        let mut doc = std::fs::read_to_string(&path)?.parse::<DocumentMut>()?;
        let array = doc["provider"]
            .as_array_of_tables_mut()
            .context("provider must be an array of tables")?;
        let index = array
            .iter()
            .position(|table| table["name"].as_str() == Some(name))
            .context("provider not found")?;
        array.remove(index);
        std::fs::write(path, doc.to_string())?;
        self.providers = parse_profiles(&doc)?;
        self.last_tests.remove(name);
        self.purge_secret(name);
        Ok(())
    }
    pub fn key(&self, profile: &ProviderProfile) -> Option<String> {
        let custom_env = format!(
            "WROSECODE_{}_API_KEY",
            profile.name.to_ascii_uppercase().replace('-', "_")
        );
        if let Ok(key) = std::env::var(&custom_env) {
            if !key.is_empty() {
                return Some(key);
            }
        }
        if let Some(env) = default_env(&profile.name) {
            if let Ok(key) = std::env::var(env) {
                if !key.is_empty() {
                    return Some(key);
                }
            }
        }
        if let Some(env) = profile.key_ref.strip_prefix("env:") {
            return std::env::var(env).ok().filter(|key| !key.is_empty());
        }
        if profile.key_ref.starts_with("keyring:") {
            if let Ok(entry) = keyring::Entry::new("wrosecode", &profile.name) {
                if let Ok(key) = entry.get_password() {
                    return Some(key);
                }
            }
        }
        if profile.key_ref == "file" {
            return self
                .auth_file()
                .ok()
                .and_then(|map| map.get(&profile.name).cloned());
        }
        None
    }
    pub fn set_key(&mut self, name: &str, value: &str) -> Result<()> {
        let mut profile = self
            .profile(name)
            .cloned()
            .with_context(|| format!("unknown provider: {name}"))?;
        let previous = profile.key_ref.clone();
        if let Some(env) = value.strip_prefix("env:") {
            if env.is_empty() || !env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("invalid environment variable name");
            }
            profile.key_ref = value.into();
        } else {
            if value.is_empty() && !self.no_key_needed(&profile) {
                bail!("API key is required for {name}");
            }
            if value.is_empty() {
                profile.key_ref.clear();
            } else if keyring::Entry::new("wrosecode", name)
                .and_then(|entry| entry.set_password(value))
                .is_ok()
            {
                profile.key_ref = format!("keyring:{name}");
            } else {
                let mut auth = self.auth_file()?;
                auth.insert(name.into(), value.into());
                self.save_auth(&auth)?;
                profile.key_ref = "file".into();
            }
        }
        self.upsert_provider(profile)?;
        let current = &self.profile(name).context("provider disappeared")?.key_ref;
        if previous != *current && !previous.is_empty() {
            self.purge_old_secret(name, &previous);
        }
        Ok(())
    }
    fn purge_secret(&self, name: &str) {
        let _ = keyring::Entry::new("wrosecode", name).and_then(|entry| entry.delete_credential());
        if let Ok(mut auth) = self.auth_file() {
            if auth.remove(name).is_some() {
                let _ = self.save_auth(&auth);
            }
        }
    }
    fn purge_old_secret(&self, name: &str, reference: &str) {
        if reference == "file" {
            if let Ok(mut auth) = self.auth_file() {
                if auth.remove(name).is_some() {
                    let _ = self.save_auth(&auth);
                }
            }
        }
        if reference.starts_with("keyring:") {
            let _ =
                keyring::Entry::new("wrosecode", name).and_then(|entry| entry.delete_credential());
        }
    }
    pub fn no_key_needed(&self, profile: &ProviderProfile) -> bool {
        matches!(profile.name.as_str(), "ollama" | "litellm")
            || reqwest::Url::parse(&profile.base_url)
                .ok()
                .is_some_and(|u| matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "::1")))
    }
    pub fn status(&self, profile: &ProviderProfile) -> String {
        if profile.base_url.is_empty() {
            return "needs setup".into();
        }
        if let Some(Err(reason)) = self.last_tests.get(&profile.name) {
            return format!("error — {reason}");
        }
        if self.no_key_needed(profile) && self.key(profile).is_none() {
            return "no key needed".into();
        }
        if self.key(profile).is_none() {
            return "key missing".into();
        }
        "connected".into()
    }
    fn auth_file(&self) -> Result<BTreeMap<String, String>> {
        let path = self.dir.join("auth.json");
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&path)?.permissions().mode() & 0o077 != 0 {
                eprintln!(
                    "warning: {} permissions are too open; changing to 0600",
                    path.display()
                );
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
    fn save_auth(&self, auth: &BTreeMap<String, String>) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("auth.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?;
            std::io::Write::write_all(&mut file, &serde_json::to_vec(auth)?)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        std::fs::write(path, serde_json::to_vec(auth)?)?;
        Ok(())
    }
    pub fn upsert_mcp(&mut self, server: McpServerDef) -> Result<()> {
        self.mcps.retain(|old| old.name != server.name);
        self.mcps.push(server);
        self.save_mcps()
    }
    pub fn remove_mcp(&mut self, name: &str) -> Result<()> {
        self.mcps.retain(|server| server.name != name);
        self.save_mcps()
    }
    fn save_mcps(&self) -> Result<()> {
        std::fs::write(
            self.dir.join("mcps.toml"),
            toml::to_string_pretty(&McpFile {
                servers: self.mcps.clone(),
            })?,
        )?;
        Ok(())
    }
}

fn default_env(name: &str) -> Option<&'static str> {
    DEFAULTS
        .iter()
        .find(|entry| entry.0 == name)
        .and_then(|entry| (!entry.4.is_empty()).then_some(entry.4))
}
fn profile_table(profile: &ProviderProfile) -> Table {
    let mut table = Table::new();
    update_table(&mut table, profile);
    table
}
fn update_table(table: &mut Table, profile: &ProviderProfile) {
    table["name"] = value(&profile.name);
    table["kind"] = value(&profile.kind);
    table["base_url"] = value(&profile.base_url);
    table["model"] = value(&profile.model);
    table["key_ref"] = value(&profile.key_ref);
    if !profile.headers.is_empty() {
        let mut headers = Table::new();
        for (key, value_text) in &profile.headers {
            headers[key] = value(value_text);
        }
        table["headers"] = Item::Table(headers);
    }
}
fn parse_profiles(doc: &DocumentMut) -> Result<Vec<ProviderProfile>> {
    let mut profiles = Vec::new();
    if let Some(array) = doc["provider"].as_array_of_tables() {
        for table in array.iter() {
            let name = table["name"]
                .as_str()
                .context("provider missing name")?
                .to_string();
            let headers = table
                .get("headers")
                .and_then(Item::as_table)
                .map(|headers| {
                    headers
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.into(), v.into())))
                        .collect()
                })
                .unwrap_or_default();
            profiles.push(ProviderProfile {
                builtin: Settings::builtin(&name),
                name,
                kind: table["kind"].as_str().unwrap_or("openai_compat").into(),
                base_url: table["base_url"].as_str().unwrap_or_default().into(),
                model: table["model"].as_str().unwrap_or_default().into(),
                key_ref: table["key_ref"].as_str().unwrap_or_default().into(),
                headers,
            });
        }
    }
    Ok(profiles)
}
fn read_toml<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    toml::from_str(&std::fs::read_to_string(path)?)
        .with_context(|| format!("invalid {}", path.display()))
}

pub fn redact(key: &str) -> String {
    if key.is_empty() {
        return "".into();
    }
    format!(
        "••••{}",
        key.chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation_and_format_preservation() {
        assert_eq!(
            Settings::normalize_url("https://example.com/v1/").unwrap(),
            "https://example.com/v1"
        );
        assert!(Settings::normalize_url("ftp://example.com").is_err());
        assert!(Settings::validate_name("Bad Name").is_err());
        assert_eq!(redact("sk-secretabcd"), "••••abcd");
        let dir = std::env::temp_dir().join(format!("wrose-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Settings::load_at(dir.clone()).unwrap();
        let path = dir.join("providers.toml");
        let source = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("# my comment\n{source}")).unwrap();
        s.upsert_provider(ProviderProfile {
            name: "test".into(),
            kind: "openai_compat".into(),
            base_url: "https://example.com/v1".into(),
            model: "x".into(),
            key_ref: "env:TEST".into(),
            headers: BTreeMap::new(),
            builtin: false,
        })
        .unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("# my comment"));
        assert_eq!(
            Settings::load_at(dir.clone())
                .unwrap()
                .profile("test")
                .unwrap()
                .model,
            "x"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn secret_file_is_private_and_not_in_provider_file() {
        let dir = std::env::temp_dir().join(format!("wrose-auth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = Settings::load_at(dir.clone()).unwrap();
        let mut auth = BTreeMap::new();
        auth.insert("mock".into(), "sk-private-1234".into());
        settings.save_auth(&auth).unwrap();
        assert_eq!(
            settings.auth_file().unwrap().get("mock").unwrap(),
            "sk-private-1234"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("auth.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(!std::fs::read_to_string(dir.join("providers.toml"))
            .unwrap()
            .contains("sk-private-1234"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
