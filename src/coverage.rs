//! Engagement coverage checklists (PentesterFlow's coverage tracking): a
//! persisted per-project list of what has actually been tested, kept in
//! `.wrosecode/coverage.json`. The model updates it through the `coverage`
//! tool as it works; humans use `/coverage` in the TUI. `/plan` still shows
//! the transient step list — this is the durable "what have we not tried
//! yet" record that survives sessions.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

impl Coverage {
    pub fn path(root: &Path) -> PathBuf {
        root.join(".wrosecode").join("coverage.json")
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            }
        };
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut raw = serde_json::to_string_pretty(self)?;
        raw.push('\n');
        std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Record a new item. Returns false when the text is already tracked.
    pub fn add(&mut self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() || self.items.iter().any(|item| item.text == text) {
            return false;
        }
        self.items.push(Item {
            text: text.to_string(),
            done: false,
        });
        true
    }

    /// Resolve a human/model-given label to an item: exact text first, then a
    /// unique case-insensitive substring, so `done XSS` works without typing
    /// the whole line. Ambiguous or missing labels error with the candidates.
    pub fn find(&self, text: &str) -> Result<usize> {
        if let Some(index) = self.items.iter().position(|item| item.text == text) {
            return Ok(index);
        }
        let needle = text.trim().to_ascii_lowercase();
        let hits: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.text.to_ascii_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect();
        match hits.as_slice() {
            [only] => Ok(*only),
            [] => bail!("no coverage item matches {text:?}"),
            many => {
                let list = many
                    .iter()
                    .map(|index| self.items[*index].text.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("{} items match {text:?}: {list}", many.len());
            }
        }
    }

    pub fn set_done(&mut self, index: usize, done: bool) -> &Item {
        self.items[index].done = done;
        &self.items[index]
    }

    pub fn toggle(&mut self, index: usize) -> &Item {
        let done = !self.items[index].done;
        self.set_done(index, done)
    }

    pub fn done_count(&self) -> usize {
        self.items.iter().filter(|item| item.done).count()
    }

    /// The checklist as it is shown to both the model and the TUI.
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return "No coverage items yet — add one to start the checklist.".to_string();
        }
        let mut out = format!(
            "coverage: {} of {} checked",
            self.done_count(),
            self.items.len()
        );
        for item in &self.items {
            out.push_str(&format!(
                "\n[{}] {}",
                if item.done { "x" } else { " " },
                item.text
            ));
        }
        out
    }

    /// One label per item for the TUI picker.
    pub fn labels(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|item| format!("[{}] {}", if item.done { "x" } else { " " }, item.text))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wrosecode-coverage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_checklist_starts_empty_with_a_hint() {
        let root = dir("missing");
        let coverage = Coverage::load(&root).expect("load");
        assert!(coverage.items.is_empty());
        assert!(coverage.render().contains("No coverage items yet"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn items_are_tracked_once_and_matched_exactly_then_by_unique_substring() {
        let mut coverage = Coverage::default();
        assert!(coverage.add("Reflected XSS in /search"));
        assert!(!coverage.add("Reflected XSS in /search"), "duplicate");
        assert!(!coverage.add("   "), "blank");
        assert_eq!(coverage.items.len(), 1);

        assert_eq!(
            coverage.find("Reflected XSS in /search").unwrap(),
            0,
            "exact text wins"
        );
        assert_eq!(coverage.find("/SEARCH").unwrap(), 0, "case-insensitive");
        assert_eq!(coverage.find("xss").unwrap(), 0, "unique substring");
        assert!(coverage.find("nothing-here").is_err(), "missing");
    }

    #[test]
    fn ambiguous_labels_error_with_the_candidates() {
        let mut coverage = Coverage::default();
        coverage.add("XSS in search");
        coverage.add("XSS in login");
        let error = coverage.find("xss").unwrap_err().to_string();
        assert!(error.contains("2 items match"), "{error}");
        assert!(error.contains("XSS in search"), "{error}");
        assert!(error.contains("XSS in login"), "{error}");
    }

    #[test]
    fn the_checklist_round_trips_through_json() {
        let root = dir("roundtrip");
        let mut coverage = Coverage::load(&root).expect("load");
        coverage.add("port scan of the /24");
        coverage.add("directory brute force");
        let index = coverage.find("brute").unwrap();
        coverage.set_done(index, true);
        coverage.save(&root).expect("save");

        let reloaded = Coverage::load(&root).expect("reload");
        assert_eq!(reloaded.items.len(), 2);
        assert_eq!(reloaded.done_count(), 1);
        assert_eq!(
            reloaded.render(),
            "coverage: 1 of 2 checked\n[ ] port scan of the /24\n[x] directory brute force"
        );
        let toggled = reloaded.items[1].done;
        assert!(toggled, "done flag must survive the round trip");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn toggling_flips_the_done_flag() {
        let mut coverage = Coverage::default();
        coverage.add("item");
        assert!(!coverage.items[0].done);
        assert!(coverage.toggle(0).done);
        assert!(!coverage.toggle(0).done);
    }
}
