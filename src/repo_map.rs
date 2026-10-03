use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

#[derive(Default)]
pub struct RepoMap {
    entries: HashMap<PathBuf, (SystemTime, Vec<String>)>,
}

impl RepoMap {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn update(&mut self, root: &Path) -> Result<()> {
        let symbol = Regex::new(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait|class|def|function|interface)\s+([A-Za-z_][A-Za-z_0-9]*)",
        )?;
        let mut seen = std::collections::HashSet::new();
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| {
                !matches!(
                    e.file_name().to_str(),
                    Some(".git" | "target" | "node_modules" | ".wrosecode")
                )
            })
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "cpp" | "h")
            ) {
                continue;
            }
            if entry.metadata()?.len() > 1_000_000 {
                continue;
            }
            let relative = path.strip_prefix(root)?.to_path_buf();
            seen.insert(relative.clone());
            let modified = entry.metadata()?.modified()?;
            if self
                .entries
                .get(&relative)
                .is_some_and(|(time, _)| *time == modified)
            {
                continue;
            }
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let symbols = if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                rust_symbols(&text).unwrap_or_else(|| {
                    text.lines()
                        .filter_map(|line| symbol.captures(line).map(|c| c[1].to_string()))
                        .take(60)
                        .collect()
                })
            } else {
                text.lines()
                    .filter_map(|line| symbol.captures(line).map(|c| c[1].to_string()))
                    .take(60)
                    .collect()
            };
            self.entries.insert(relative, (modified, symbols));
        }
        self.entries.retain(|path, _| seen.contains(path));
        Ok(())
    }
    pub fn render(&self, query: &str) -> String {
        let words: Vec<String> = query
            .split_whitespace()
            .map(|w| w.to_ascii_lowercase())
            .collect();
        let mut ranked: Vec<_> = self
            .entries
            .iter()
            .map(|(path, (_, symbols))| {
                let combined =
                    format!("{} {}", path.display(), symbols.join(" ")).to_ascii_lowercase();
                (
                    words
                        .iter()
                        .filter(|w| w.len() > 2 && combined.contains(w.as_str()))
                        .count(),
                    path,
                    symbols,
                )
            })
            .collect();
        ranked.sort_by_key(|item| std::cmp::Reverse(item.0));
        ranked
            .into_iter()
            .take(80)
            .map(|(_, path, symbols)| format!("{}: {}", path.display(), symbols.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn rust_symbols(text: &str) -> Option<Vec<String>> {
    let ast = syn::parse_file(text).ok()?;
    let mut names = Vec::new();
    for item in ast.items {
        let name = match item {
            syn::Item::Fn(v) => Some(v.sig.ident.to_string()),
            syn::Item::Struct(v) => Some(v.ident.to_string()),
            syn::Item::Enum(v) => Some(v.ident.to_string()),
            syn::Item::Trait(v) => Some(v.ident.to_string()),
            syn::Item::Mod(v) => Some(v.ident.to_string()),
            syn::Item::Type(v) => Some(v.ident.to_string()),
            _ => None,
        };
        if let Some(name) = name {
            names.push(name);
        }
        if names.len() >= 60 {
            break;
        }
    }
    Some(names)
}
