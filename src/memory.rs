use anyhow::Result;
use regex::Regex;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Memory {
    pub project: PathBuf,
    pub personal: PathBuf,
}

impl Memory {
    pub fn new(root: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.to_path_buf());
        Self {
            project: root.join(".wrosecode/memory"),
            personal: home.join(".wrosecode/memory"),
        }
    }
    pub fn redact(text: &str) -> String {
        let mut out = text.to_string();
        for pattern in [
            r"(?i)(api[_-]?key|token|password|secret)\s*[:=]\s*\S+",
            r"sk-[A-Za-z0-9_-]{16,}",
            r"ghp_[A-Za-z0-9]{20,}",
            r"AKIA[A-Z0-9]{16}",
        ] {
            if let Ok(re) = Regex::new(pattern) {
                out = re.replace_all(&out, "[REDACTED]").to_string();
            }
        }
        out
    }
    pub fn save(&self, text: &str, personal: bool) -> Result<String> {
        let fact = Self::redact(text.trim());
        if fact.is_empty() {
            anyhow::bail!("empty fact");
        }
        let dir = if personal {
            &self.personal
        } else {
            &self.project
        };
        std::fs::create_dir_all(dir)?;
        let existing = self.list_dir(dir)?;
        if existing.iter().any(|f| f.eq_ignore_ascii_case(&fact)) {
            return Ok("fact already saved".into());
        }
        let mut number = existing.len();
        while dir.join(format!("fact-{number}.md")).exists() {
            number += 1;
        }
        std::fs::write(
            dir.join(format!("fact-{number}.md")),
            format!(
                "---\nscope: {}\n---\n{}\n",
                if personal { "personal" } else { "project" },
                fact
            ),
        )?;
        Ok("fact saved".into())
    }
    fn list_dir(&self, dir: &Path) -> Result<Vec<String>> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        Ok(std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .map(|s| s.split("---\n").nth(2).unwrap_or(&s).trim().to_string())
            .collect())
    }
    pub fn list(&self) -> Result<Vec<String>> {
        let mut all = self.list_dir(&self.project)?;
        all.extend(self.list_dir(&self.personal)?);
        Ok(all)
    }
    pub fn recall(&self, query: &str) -> Result<String> {
        let words: Vec<String> = query
            .split_whitespace()
            .filter(|w| w.len() > 3)
            .map(|w| w.to_ascii_lowercase())
            .collect();
        let mut scored: Vec<(usize, String)> = self
            .list()?
            .into_iter()
            .map(|f| {
                let lower = f.to_ascii_lowercase();
                (
                    words.iter().filter(|w| lower.contains(w.as_str())).count(),
                    f,
                )
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by_key(|item| std::cmp::Reverse(item.0));
        Ok(scored
            .into_iter()
            .take(8)
            .map(|(_, f)| f)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::Memory;

    #[test]
    fn redacts_common_secret_assignments() {
        assert_eq!(
            Memory::redact("api_key=abc123 password: hunter2"),
            "[REDACTED] [REDACTED]"
        );
    }
}
