use anyhow::Result;
use regex::Regex;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
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
// Repo context is optional. Never index an entire Desktop/home directory or
// allow the reference graph to grow quadratically without a bound.
const MAX_MAP_FILES: usize = 256;
const MAX_WALK_ENTRIES: usize = 4096;
const MAP_SCAN_BUDGET: Duration = Duration::from_millis(150);

/// Per file: mtime, defined symbols, and the lowercase identifier tokens the
/// file mentions (the edges of the reference graph).
type Entry = (SystemTime, Vec<String>, Vec<String>);

#[derive(Default)]
pub struct RepoMap {
    entries: HashMap<PathBuf, Entry>,
    /// Bumped on every mutation; lets `render` cache per (query, revision)
    /// so repeated turns over unchanged trees skip PageRank entirely.
    revision: u64,
    /// Last rendered query and the revision it was built from.
    render_cache: Option<(String, u64, String)>,
    /// Graph ranks depend only on the indexed files, not on the user's query.
    /// Reuse them across turns until an update changes the map revision.
    rank_cache: Option<(u64, Vec<f64>)>,
}

/// The symbol-line pattern, compiled once per process instead of on every
/// `update()` call.
fn symbol_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait|class|def|function|interface)\s+([A-Za-z_][A-Za-z_0-9]*)",
        )
        .expect("symbol pattern is static and valid")
    })
}

impl RepoMap {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn update(&mut self, root: &Path) -> Result<()> {
        let started = Instant::now();
        let symbol = symbol_pattern();
        if self.entries.len() > MAX_MAP_FILES {
            self.entries.clear();
            self.revision = self.revision.wrapping_add(1);
            self.render_cache = None;
            self.rank_cache = None;
        }
        let before = self.entries.len();
        let mut inserted = false;
        let mut seen = std::collections::HashSet::new();
        let mut complete_scan = true;
        for (visited, entry) in WalkDir::new(root)
            .max_depth(8)
            .into_iter()
            .filter_entry(|e| {
                !matches!(
                    e.file_name().to_str(),
                    Some(
                        ".git"
                            | "target"
                            | "node_modules"
                            | ".wrosecode"
                            | ".venv"
                            | "venv"
                            | "__pycache__"
                            | ".ctf"
                            | "dist"
                            | "vendor"
                    )
                )
            })
            .filter_map(Result::ok)
            .enumerate()
        {
            if visited >= MAX_WALK_ENTRIES
                || started.elapsed() >= MAP_SCAN_BUDGET
                || seen.len() >= MAX_MAP_FILES
            {
                complete_scan = false;
                break;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "cpp" | "h")
            ) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.len() > 128 * 1024 {
                continue;
            }
            let relative = path.strip_prefix(root)?.to_path_buf();
            seen.insert(relative.clone());
            let modified = metadata.modified()?;
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
            if !self.entries.contains_key(&relative) && self.entries.len() >= MAX_MAP_FILES {
                complete_scan = false;
                break;
            }
            self.entries.insert(relative, (modified, symbols, tokens));
            inserted = true;
        }
        if complete_scan {
            self.entries.retain(|path, _| seen.contains(path));
        }
        if inserted || self.entries.len() != before {
            // Any mutation (or deletion) invalidates the cached render.
            self.revision = self.revision.wrapping_add(1);
            self.render_cache = None;
            self.rank_cache = None;
        }
        Ok(())
    }
    /// The system-prompt map: files ranked by query relevance *and* by how
    /// central they are in the symbol-reference graph (PageRank), emitted
    /// until the token budget runs out.
    pub fn render(&mut self, query: &str) -> String {
        if let Some((cached_query, cached_revision, cached)) = &self.render_cache {
            if *cached_revision == self.revision && cached_query == query {
                return cached.clone();
            }
        }
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
        if self
            .rank_cache
            .as_ref()
            .is_none_or(|(revision, _)| *revision != self.revision)
        {
            self.rank_cache = Some((self.revision, self.pagerank(&paths)));
        }
        let ranks = &self.rank_cache.as_ref().expect("ranks populated").1;
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
        self.render_cache = Some((query.to_string(), self.revision, output.clone()));
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
    static IDENTIFIERS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = IDENTIFIERS.get_or_init(|| {
        Regex::new(r"[A-Za-z_][A-Za-z_0-9_]{3,}").expect("static identifier pattern")
    });
    let mut seen = HashSet::new();
    let mut tokens: Vec<String> = regex
        .find_iter(text)
        .map(|found| found.as_str().to_ascii_lowercase())
        .filter(|token| seen.insert(token.clone()))
        .take(TOKENS_PER_FILE)
        .collect();
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

    #[test]
    fn render_caches_per_query_and_revision() {
        let root = workspace("cache");
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        let mut map = RepoMap::default();
        map.update(&root).expect("update");
        let revision = map.revision;
        assert!(revision > 0, "cold update bumps the revision");
        let first = map.render("alpha");
        assert!(map.render_cache.is_some(), "render populates the cache");
        let second = map.render("alpha");
        assert_eq!(first, second, "same query and revision reuses the cache");
        // Touching a file invalidates: new content, new revision, no cache.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
        map.update(&root).expect("re-update");
        assert!(
            map.revision > revision,
            "changed content bumps the revision"
        );
        assert!(
            map.render_cache.is_none(),
            "mutation drops the cached render"
        );
        let third = map.render("alpha");
        assert!(
            third.contains("beta"),
            "re-render picks up the new symbol: {third}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn graph_ranks_survive_query_changes_and_refresh_after_edits_and_deletes() {
        let root = workspace("rank-reuse");
        std::fs::write(root.join("alpha.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(root.join("beta.rs"), "pub fn beta() { alpha(); }\n").unwrap();
        let mut map = RepoMap::default();
        map.update(&root).unwrap();
        let first = map.render("alpha");
        assert!(first.starts_with("alpha.rs:"));
        let (revision, ranks) = map.rank_cache.clone().unwrap();
        let second = map.render("beta");
        assert!(second.starts_with("beta.rs:"), "{second}");
        assert_eq!(map.rank_cache.as_ref().unwrap(), &(revision, ranks));
        // A stable update must retain the query-independent graph cache.
        map.update(&root).unwrap();
        assert_eq!(map.rank_cache.as_ref().unwrap().0, revision);

        std::fs::write(root.join("gamma.rs"), "pub fn gamma() { beta(); }\n").unwrap();
        map.update(&root).unwrap();
        assert!(map.rank_cache.is_none());
        assert!(map.render("gamma").starts_with("gamma.rs:"));
        assert_eq!(map.rank_cache.as_ref().unwrap().1.len(), 3);

        std::fs::remove_file(root.join("alpha.rs")).unwrap();
        map.update(&root).unwrap();
        assert!(map.rank_cache.is_none());
        assert!(!map.render("alpha").contains("alpha.rs:"));
        assert_eq!(map.rank_cache.as_ref().unwrap().1.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

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
    #[test]
    fn indexing_a_large_workspace_has_a_hard_file_limit() {
        let root = workspace("bounded-desktop");
        for i in 0..(MAX_MAP_FILES + 80) {
            std::fs::write(
                root.join(format!("file_{i}.py")),
                format!("def function_{i}(): return shared_value\n"),
            )
            .unwrap();
        }
        let mut map = RepoMap::default();
        map.update(&root).unwrap();
        assert!(map.len() > 0);
        assert!(
            map.len() <= MAX_MAP_FILES,
            "optional context must be bounded"
        );
        assert!(map.render("function").chars().count() <= BUDGET_CHARS);
        std::fs::remove_dir_all(root).unwrap();
    }
}
