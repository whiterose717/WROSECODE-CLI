use anyhow::Result;
use globset::Glob;
use regex::Regex;
use std::path::Path;
use walkdir::WalkDir;

fn files(root: &Path) -> impl Iterator<Item = walkdir::DirEntry> + '_ {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | "target" | "node_modules" | ".wrosecode")
            )
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
}

pub fn glob(root: &Path, pattern: &str) -> Result<String> {
    let matcher = Glob::new(pattern)?.compile_matcher();
    Ok(files(root)
        .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .filter(|p| matcher.is_match(p))
        .take(500)
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn grep(root: &Path, pattern: &str) -> Result<String> {
    let re = Regex::new(pattern)?;
    let mut hits = Vec::new();
    for e in files(root) {
        if e.metadata()?.len() > 1_000_000 {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(e.path()) {
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{}:{}:{}", e.path().display(), i + 1, line));
                    if hits.len() >= 500 {
                        return Ok(hits.join("\n"));
                    }
                }
            }
        }
    }
    Ok(hits.join("\n"))
}
