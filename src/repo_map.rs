use anyhow::Result;
use regex::Regex;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

/// Token budget for the repo map inside the system prompt (~4 chars/token),
/// so a large repository can never crowd out the conversation.
const BUDGET_TOKENS: usize = 1024;
const BUDGET_CHARS: usize = BUDGET_TOKENS * 4;
/// Identifier tokens cached per file to build the reference graph.
const TOKENS_PER_FILE: usize = 400;
/// PageRank power-iteration rounds: the graph is small and converges fast.
const PAGERANK_ROUNDS: usize = 15;
const DAMPING: f64 = 0.85;

/// Per file: mtime, defined symbols, and the lowercase identifier tokens the
/// file mentions (the edges of the reference graph).
type Entry = (SystemTime, Vec<String>, Vec<String>);

#[derive(Default)]
pub struct RepoMap {
    entries: HashMap<PathBuf, Entry>,
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
                .is_some_and(|(time, _, _)| *time == modified)
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
            let tokens = identifier_tokens(&text);
            self.entries.insert(relative, (modified, symbols, tokens));
        }
        self.entries.retain(|path, _| seen.contains(path));
        Ok(())
    }
    /// The system-prompt map: files ranked by query relevance *and* by how
    /// central they are in the symbol-reference graph (PageRank), emitted
    /// until the token budget runs out.
    pub fn render(&self, query: &str) -> String {
        let words: Vec<String> = query
            .split_whitespace()
            .map(|word| word.to_ascii_lowercase())
            .collect();
        let mut paths: Vec<&PathBuf> = self.entries.keys().collect();
        paths.sort();
        if paths.is_empty() {
            return String::new();
        }
        let relevance: Vec<f64> = paths
            .iter()
            .map(|path| {
                let (_, symbols, _) = &self.entries[*path];
                let combined =
                    format!("{} {}", path.display(), symbols.join(" ")).to_ascii_lowercase();
                words
                    .iter()
                    .filter(|word| word.len() > 2 && combined.contains(word.as_str()))
                    .count() as f64
            })
            .collect();
        let ranks = self.pagerank(&paths);
        let peak = ranks.iter().cloned().fold(0.0_f64, f64::max).max(1e-9);
        let mut ranked: Vec<(f64, &PathBuf, &[String])> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let score = relevance[index] * 2.0 + ranks[index] / peak;
                (score, *path, self.entries[*path].1.as_slice())
            })
            .collect();
        ranked.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.1.cmp(right.1))
        });
        let mut output = String::new();
        for (_, path, symbols) in ranked {
            let line = format!("{}: {}", path.display(), symbols.join(", "));
            if !output.is_empty() && output.len() + line.len() + 1 > BUDGET_CHARS {
                break;
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&line);
        }
        output
    }

    /// PageRank over "file A mentions symbols defined in file B" edges, so a
    /// file many modules depend on ranks above an orphan with the same name.
    fn pagerank(&self, paths: &[&PathBuf]) -> Vec<f64> {
        let size = paths.len();
        if size == 0 {
            return Vec::new();
        }
        let mut owners: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, path) in paths.iter().enumerate() {
            for symbol in &self.entries[*path].1 {
                owners
                    .entry(symbol.to_ascii_lowercase())
                    .or_default()
                    .push(index);
            }
        }
        let mut outgoing: Vec<Vec<(usize, f64)>> = vec![Vec::new(); size];
        for (from, path) in paths.iter().enumerate() {
            let mut edges: HashMap<usize, f64> = HashMap::new();
            for token in &self.entries[*path].2 {
                if let Some(to) = owners.get(token) {
                    for target in to {
                        if *target != from {
                            *edges.entry(*target).or_default() += 1.0;
                        }
                    }
                }
            }
            let total: f64 = edges.values().sum();
            if total > 0.0 {
                outgoing[from] = edges
                    .into_iter()
                    .map(|(to, weight)| (to, weight / total))
                    .collect();
            }
        }
        let mut rank = vec![1.0 / size as f64; size];
        for _ in 0..PAGERANK_ROUNDS {
            let mut next = vec![0.0_f64; size];
            let mut dangling = 0.0;
            for (from, edges) in outgoing.iter().enumerate() {
                if edges.is_empty() {
                    dangling += rank[from];
                    continue;
                }
                for (to, weight) in edges {
                    next[*to] += rank[from] * weight;
                }
            }
            let base = (1.0 - DAMPING) / size as f64 + DAMPING * dangling / size as f64;
            for value in next.iter_mut() {
                *value = base + DAMPING * *value;
            }
            rank = next;
        }
        rank
    }
}

/// Lowercase identifiers (length ≥ 4) from one file, deduplicated and capped
/// — enough to notice "that file uses `snapshot::capture`" without keeping
/// whole sources in memory.
fn identifier_tokens(text: &str) -> Vec<String> {
    let Ok(regex) = Regex::new(r"[A-Za-z_][A-Za-z_0-9_]{3,}") else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut tokens: Vec<String> = regex
        .find_iter(text)
        .map(|found| found.as_str().to_ascii_lowercase())
        .filter(|token| seen.insert(token.clone()))
        .collect();
    tokens.truncate(TOKENS_PER_FILE);
    tokens.sort();
    tokens
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

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("wrosecode-repomap-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_file_every_module_references_outranks_a_same_named_orphan() {
        let root = workspace("rank");
        std::fs::write(
            root.join("handle_request.rs"),
            "pub fn handle_request() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("handle_notes.rs"), "pub fn handle_notes() {}\n").unwrap();
        for index in 0..3 {
            std::fs::write(
                root.join(format!("member{index}.rs")),
                format!("pub fn member{index}() {{ handle_request(); }}\n"),
            )
            .unwrap();
        }
        let mut map = RepoMap::default();
        map.update(&root).expect("update");
        let render = map.render("handle");
        let first = render.lines().next().expect("at least one file");
        assert!(
            first.starts_with("handle_request.rs"),
            "the referenced file must win over the orphan: {render}"
        );
        assert!(render.contains("handle_notes.rs"), "{render}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_repo_map_never_exceeds_its_token_budget() {
        let root = workspace("budget");
        for file in 0..40 {
            let body: String = (0..30)
                .map(|index| format!("pub fn symbol_{file}_{index}() {{}}\n"))
                .collect();
            std::fs::write(root.join(format!("file{file}.rs")), body).unwrap();
        }
        let mut map = RepoMap::default();
        map.update(&root).expect("update");
        let render = map.render("");
        assert!(
            render.len() <= BUDGET_CHARS,
            "render is {} chars, budget is {BUDGET_CHARS}",
            render.len()
        );
        assert!(
            render.lines().count() < 40,
            "the budget must cut the list short: {} lines",
            render.lines().count()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
