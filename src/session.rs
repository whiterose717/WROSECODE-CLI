use crate::provider::Message;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub created: u64,
    pub summary: String,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub model: String,
    pub messages: Vec<Message>,
    pub transcript: Vec<(String, String)>,
}

impl Session {
    pub fn fresh() -> Self {
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            name: format!(
                "{created}-{}-{}",
                std::process::id(),
                NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
            ),
            created,
            summary: "New session".into(),
            provider_name: String::new(),
            model: String::new(),
            messages: Vec::new(),
            transcript: Vec::new(),
        }
    }
    pub fn dir() -> Result<PathBuf> {
        Ok(
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".wrosecode/sessions"),
        )
    }
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.json", self.name))
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = self.path(dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
    pub fn list(dir: &Path) -> Result<Vec<Self>> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(session) = Self::load(&path) {
                    sessions.push(session);
                }
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.created));
        Ok(sessions)
    }
    pub fn rename(&mut self, dir: &Path, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            anyhow::bail!("session name must use letters, numbers, - or _");
        }
        let old = self.path(dir);
        let new = dir.join(format!("{name}.json"));
        if new.exists() && new != old {
            anyhow::bail!("session {name} already exists");
        }
        self.name = name.into();
        self.save(dir)?;
        if old != new && old.exists() {
            std::fs::remove_file(old)?;
        }
        Ok(())
    }
}
