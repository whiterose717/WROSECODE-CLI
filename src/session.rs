use crate::provider::Message;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// Files pinned with `/add`; restored into the agent when the session
    /// resumes (older exports simply default to an empty list).
    #[serde(default)]
    pub pinned: Vec<String>,
    /// The session this one was forked from — the edge `/tree` draws.
    #[serde(default)]
    pub parent: Option<String>,
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
            pinned: Vec::new(),
            parent: None,
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
    /// The session forest for `/tree`: roots in creation order, forks
    /// indented under their parent, the active session marked. A `parent`
    /// that no longer exists is drawn as a root, so deleting a session never
    /// hides its orphans.
    pub fn tree(sessions: &[Session], current: &str) -> Vec<String> {
        let known = |parent: &str| sessions.iter().any(|other| other.name == parent);
        let mut children: BTreeMap<&str, Vec<&Session>> = BTreeMap::new();
        let mut roots: Vec<&Session> = Vec::new();
        for session in sessions {
            match session.parent.as_deref() {
                Some(parent) if parent != session.name && known(parent) => {
                    children.entry(parent).or_default().push(session);
                }
                _ => roots.push(session),
            }
        }
        roots.sort_by_key(|session| session.created);
        for branch in children.values_mut() {
            branch.sort_by_key(|session| session.created);
        }

        fn walk(
            session: &Session,
            line_prefix: &str,
            subtree_prefix: &str,
            children: &BTreeMap<&str, Vec<&Session>>,
            current: &str,
            visited: &mut Vec<String>,
            lines: &mut Vec<String>,
        ) {
            visited.push(session.name.clone());
            let marker = if session.name == current {
                "  (current)"
            } else {
                ""
            };
            lines.push(format!(
                "{line_prefix}{}  {}{marker}",
                session.name, session.summary
            ));
            let branch = children
                .get(session.name.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            for (index, child) in branch.iter().enumerate() {
                if visited.contains(&child.name) {
                    continue;
                }
                let last = index + 1 == branch.len();
                walk(
                    child,
                    &format!("{subtree_prefix}{}", if last { "└── " } else { "├── " }),
                    &format!("{subtree_prefix}{}", if last { "    " } else { "│   " }),
                    children,
                    current,
                    visited,
                    lines,
                );
            }
        }

        let mut lines = Vec::new();
        let mut visited = Vec::new();
        for root in roots {
            walk(root, "", "", &children, current, &mut visited, &mut lines);
        }
        // A parent cycle would leave sessions unreachable from any root; keep
        // them visible rather than silently dropping them.
        let mut strays: Vec<&Session> = sessions
            .iter()
            .filter(|session| !visited.contains(&session.name))
            .collect();
        strays.sort_by_key(|session| session.created);
        for stray in strays {
            if visited.contains(&stray.name) {
                continue;
            }
            walk(stray, "", "", &children, current, &mut visited, &mut lines);
        }
        lines
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

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, created: u64, parent: Option<&str>) -> Session {
        let mut session = Session::fresh();
        session.name = name.into();
        session.created = created;
        session.summary = format!("summary of {name}");
        session.parent = parent.map(str::to_string);
        session
    }

    #[test]
    fn the_tree_indents_forks_under_their_parent_and_marks_the_current_one() {
        let sessions = vec![
            named("root", 1, None),
            named("child-b", 3, Some("root")),
            named("child-a", 2, Some("root")),
            named("lonely", 4, None),
            named("orphan", 5, Some("deleted-session")),
        ];
        assert_eq!(
            Session::tree(&sessions, "child-a"),
            vec![
                "root  summary of root".to_string(),
                "├── child-a  summary of child-a  (current)".to_string(),
                "└── child-b  summary of child-b".to_string(),
                "lonely  summary of lonely".to_string(),
                "orphan  summary of orphan".to_string(),
            ]
        );
    }

    #[test]
    fn a_parent_cycle_keeps_every_session_visible() {
        let sessions = vec![named("a", 1, Some("b")), named("b", 2, Some("a"))];
        let lines = Session::tree(&sessions, "a");
        assert_eq!(
            lines,
            vec![
                "a  summary of a  (current)".to_string(),
                "└── b  summary of b".to_string(),
            ]
        );
    }
}
