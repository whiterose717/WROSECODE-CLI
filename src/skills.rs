use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub fork: bool,
    /// Keywords that trigger the skill on their own (OpenHands-style
    /// microagents): a short snippet with `keywords:` fires whenever one of
    /// those words is mentioned, even if the name never appears.
    pub keywords: Vec<String>,
    pub body: String,
    /// External skill bodies are loaded only when selected/matched. Startup
    /// indexes frontmatter only so thousands of skills do not require tens of
    /// megabytes of synchronous reads before the first prompt.
    source: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    fork: Option<bool>,
    keywords: Option<serde_yaml::Value>,
}

const SKILL_INDEX_VERSION: u8 = 1;
const DEFAULT_SKILL_INDEX_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Serialize, Deserialize)]
struct SkillIndex {
    version: u8,
    generated_at: u64,
    roots: Vec<PathBuf>,
    skills: Vec<IndexedSkill>,
}

#[derive(Serialize, Deserialize)]
struct IndexedSkill {
    name: String,
    description: String,
    fork: bool,
    keywords: Vec<String>,
    source: PathBuf,
}

/// `~/.wrosecode/skills` — where hand-placed skills and installed packages
/// both live, and what `crate::package` writes into.
pub fn default_skills_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".wrosecode/skills")
}

pub fn discover(root: &Path, extra: &[PathBuf]) -> Result<HashMap<String, Skill>> {
    let dirs = skill_dirs(root, extra);
    let mut found = builtin_skills()?;

    if let Some(cached) = load_skill_index(root, &dirs) {
        found.extend(cached.into_iter().map(|skill| (skill.name.clone(), skill)));
        return Ok(found);
    }

    // Walking is cheap; opening hundreds or thousands of metadata files is
    // not. Collect paths in precedence order, then parse frontmatter with a
    // small bounded I/O pool before replaying that order into the map.
    let mut candidates = Vec::new();
    for dir in &dirs {
        if !dir.exists() {
            continue;
        }
        for entry in WalkDir::new(dir)
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_name() != "SKILL.md" {
                continue;
            }
            let fallback = entry
                .path()
                .parent()
                .unwrap_or(entry.path())
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            candidates.push((candidates.len(), entry.path().to_path_buf(), fallback));
        }
    }
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2)
        .clamp(1, 8)
        .min(candidates.len().max(1));
    let chunk_size = candidates.len().div_ceil(workers).max(1);
    let mut handles = Vec::new();
    for chunk in candidates.chunks(chunk_size) {
        let owned = chunk.to_vec();
        handles.push(std::thread::spawn(move || {
            owned
                .into_iter()
                .map(|(order, path, fallback)| {
                    parse_skill_file(&path, &fallback).map(|skill| (order, skill))
                })
                .collect::<Vec<_>>()
        }));
    }
    let mut parsed = Vec::with_capacity(candidates.len());
    for handle in handles {
        for item in handle
            .join()
            .map_err(|_| anyhow::anyhow!("skill metadata worker panicked"))?
        {
            parsed.push(item?);
        }
    }
    parsed.sort_by_key(|(order, _)| *order);
    let mut external = HashMap::new();
    for (_, skill) in parsed {
        external.insert(skill.name.clone(), skill);
    }
    save_skill_index(root, &dirs, external.values());
    found.extend(external);
    Ok(found)
}

fn skill_dirs(root: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = vec![
        root.join("skills"),
        root.join(".ctf/skills"),
        default_skills_dir(),
    ];
    dirs.extend_from_slice(extra);
    dirs
}

fn builtin_skills() -> Result<HashMap<String, Skill>> {
    let mut found = HashMap::new();
    for (fallback, raw) in [
        ("coding", include_str!("../skills/coding/SKILL.md")),
        (
            "binary-analysis",
            include_str!("../skills/binary-analysis/SKILL.md"),
        ),
        ("ctf-pwn", include_str!("../skills/ctf-pwn/SKILL.md")),
        ("ctf-crypto", include_str!("../skills/ctf-crypto/SKILL.md")),
        ("webvuln", include_str!("../skills/webvuln/SKILL.md")),
        ("ctf-web", include_str!("../skills/ctf-web/SKILL.md")),
        ("recon", include_str!("../skills/recon/SKILL.md")),
        ("ctf-rev", include_str!("../skills/ctf-rev/SKILL.md")),
        (
            "ctf-forensics",
            include_str!("../skills/ctf-forensics/SKILL.md"),
        ),
        ("ctf-stego", include_str!("../skills/ctf-stego/SKILL.md")),
        ("ctf-osint", include_str!("../skills/ctf-osint/SKILL.md")),
        (
            "ctf-network",
            include_str!("../skills/ctf-network/SKILL.md"),
        ),
    ] {
        let skill = parse_skill(raw, fallback)?;
        found.insert(skill.name.clone(), skill);
    }
    Ok(found)
}

fn skill_index_ttl_secs() -> u64 {
    std::env::var("WROSECODE_SKILL_INDEX_TTL_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SKILL_INDEX_TTL_SECS)
}

fn skill_index_path(root: &Path, dirs: &[PathBuf]) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    dirs.hash(&mut hasher);
    root.join(".wrosecode/cache")
        .join(format!("skills-{:016x}.json", hasher.finish()))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn load_skill_index(root: &Path, dirs: &[PathBuf]) -> Option<Vec<Skill>> {
    let raw = std::fs::read_to_string(skill_index_path(root, dirs)).ok()?;
    let index: SkillIndex = serde_json::from_str(&raw).ok()?;
    if index.version != SKILL_INDEX_VERSION || index.roots != dirs {
        return None;
    }
    if now_secs().saturating_sub(index.generated_at) > skill_index_ttl_secs() {
        return None;
    }
    Some(
        index
            .skills
            .into_iter()
            .map(|skill| Skill {
                name: skill.name,
                description: skill.description,
                fork: skill.fork,
                keywords: skill.keywords,
                body: String::new(),
                source: Some(skill.source),
            })
            .collect(),
    )
}

fn save_skill_index<'a>(root: &Path, dirs: &[PathBuf], skills: impl Iterator<Item = &'a Skill>) {
    let path = skill_index_path(root, dirs);
    let entries = skills
        .filter_map(|skill| {
            Some(IndexedSkill {
                name: skill.name.clone(),
                description: skill.description.clone(),
                fork: skill.fork,
                keywords: skill.keywords.clone(),
                source: skill.source.clone()?,
            })
        })
        .collect();
    let index = SkillIndex {
        version: SKILL_INDEX_VERSION,
        generated_at: now_secs(),
        roots: dirs.to_vec(),
        skills: entries,
    };
    let Ok(raw) = serde_json::to_vec(&index) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, raw).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

/// Force the next discovery to rescan metadata. Package installation/removal
/// calls this automatically; manual skill edits can use `/skills reload`.
pub fn invalidate_cache(root: &Path, extra: &[PathBuf]) {
    let dirs = skill_dirs(root, extra);
    let _ = std::fs::remove_file(skill_index_path(root, &dirs));
}

pub(crate) fn parse_skill(raw: &str, fallback: &str) -> Result<Skill> {
    let (meta, body) = if let Some(rest) = raw.strip_prefix("---\n") {
        if let Some((yaml, body)) = rest.split_once("\n---\n") {
            (serde_yaml::from_str::<Frontmatter>(yaml)?, body.to_string())
        } else {
            (Frontmatter::default(), raw.to_string())
        }
    } else {
        (Frontmatter::default(), raw.to_string())
    };
    Ok(Skill {
        name: meta.name.unwrap_or_else(|| fallback.into()),
        description: meta.description.unwrap_or_default(),
        fork: meta.fork.unwrap_or(false),
        keywords: normalize_keywords(meta.keywords),
        body,
        source: None,
    })
}

/// Index only the YAML frontmatter of an external skill. Reading stops at the
/// closing `---`, so a 500 KB reference-heavy skill costs only a few hundred
/// bytes at startup. Files without frontmatter remain usable by fallback name.
fn parse_skill_file(path: &Path, fallback: &str) -> Result<Skill> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let meta = if first.trim_end() == "---" {
        let mut yaml = String::new();
        let mut line = String::new();
        let mut closed = false;
        while yaml.len() < 32 * 1024 {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if line.trim_end() == "---" {
                closed = true;
                break;
            }
            yaml.push_str(&line);
        }
        if closed {
            serde_yaml::from_str::<Frontmatter>(&yaml)?
        } else {
            Frontmatter::default()
        }
    } else {
        Frontmatter::default()
    };
    Ok(Skill {
        name: meta.name.unwrap_or_else(|| fallback.into()),
        description: meta.description.unwrap_or_default(),
        fork: meta.fork.unwrap_or(false),
        keywords: normalize_keywords(meta.keywords),
        body: String::new(),
        source: Some(path.to_path_buf()),
    })
}

impl Skill {
    /// Return the current body. Built-ins are already resident; external skill
    /// bodies are read on demand and never contribute to cold-start cost.
    pub fn body_text(&self) -> Result<String> {
        let Some(path) = &self.source else {
            return Ok(self.body.clone());
        };
        let raw = std::fs::read_to_string(path)?;
        if let Some(rest) = raw.strip_prefix("---\n") {
            if let Some((_, body)) = rest.split_once("\n---\n") {
                return Ok(body.to_string());
            }
        }
        Ok(raw)
    }
}

pub fn match_skill<'a>(skills: &'a HashMap<String, Skill>, text: &str) -> Option<&'a Skill> {
    let lower = text.to_ascii_lowercase();
    skills
        .values()
        .filter_map(|skill| {
            let score = if lower.contains(&skill.name) {
                100
            } else {
                let name_hits = skill
                    .name
                    .split('-')
                    .filter(|word| word.len() >= 4 && lower.contains(word))
                    .count();
                // A declared keyword beats fuzzy name matching but never the
                // skill's own name, and ties break on the name for stability.
                if skill.keywords.iter().any(|word| mentions(&lower, word)) {
                    60 + name_hits
                } else {
                    name_hits
                }
            };
            (score > 0).then_some((score, skill))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)))
        .map(|(_, skill)| skill)
}

/// `keywords:` frontmatter — a YAML list, or one string with commas/spaces.
/// Every entry is lowercased once at parse time.
fn normalize_keywords(value: Option<serde_yaml::Value>) -> Vec<String> {
    let raw = match value {
        Some(serde_yaml::Value::Sequence(items)) => items
            .into_iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .join(","),
        Some(serde_yaml::Value::String(text)) => text,
        _ => String::new(),
    };
    let mut words: Vec<String> = raw
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_ascii_lowercase())
        .collect();
    words.sort();
    words.dedup();
    words
}

/// Whole-word-ish keyword lookup: "go" must not fire inside "cargo".
fn mentions(lower_text: &str, keyword: &str) -> bool {
    if keyword.is_empty() {
        return false;
    }
    let bytes = lower_text.as_bytes();
    let mut at = match lower_text.find(keyword) {
        Some(index) => index,
        None => return false,
    };
    loop {
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let end = at + keyword.len();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        match lower_text[at + 1..].find(keyword) {
            Some(relative) => at += 1 + relative,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(frontmatter: &str, body: &str) -> Skill {
        parse_skill(&format!("---\n{frontmatter}---\n{body}"), "fallback").expect("parse")
    }

    fn single(skill: Skill) -> HashMap<String, Skill> {
        let mut map = HashMap::new();
        map.insert(skill.name.clone(), skill);
        map
    }

    #[test]
    fn discovery_cache_avoids_rescanning_until_explicit_reload() {
        let root = std::env::temp_dir().join(format!("wrose-skill-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("skills/cached");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: cached-skill\ndescription: first\n---\nFIRST-BODY",
        )
        .unwrap();
        let first = discover(&root, &[]).unwrap();
        assert_eq!(first["cached-skill"].description, "first");
        let cache_dir = root.join(".wrosecode/cache");
        assert!(cache_dir.exists());

        std::fs::write(
            &path,
            "---\nname: cached-skill\ndescription: second\n---\nSECOND-BODY",
        )
        .unwrap();
        let cached = discover(&root, &[]).unwrap();
        assert_eq!(cached["cached-skill"].description, "first");
        // Body stays lazy and reads the current file even while metadata is cached.
        assert!(cached["cached-skill"]
            .body_text()
            .unwrap()
            .contains("SECOND-BODY"));

        invalidate_cache(&root, &[]);
        let refreshed = discover(&root, &[]).unwrap();
        assert_eq!(refreshed["cached-skill"].description, "second");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn external_skill_bodies_are_loaded_lazily() {
        let root = std::env::temp_dir().join(format!("wrose-skill-lazy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("fast-skill")).unwrap();
        let path = root.join("fast-skill/SKILL.md");
        let body = format!(
            "---\nname: fast-skill\ndescription: lazy body test\nkeywords: [fastword]\n---\nLAZY-BODY-MARKER\n{}",
            "x".repeat(256 * 1024)
        );
        std::fs::write(&path, body).unwrap();

        let skill = parse_skill_file(&path, "fallback").unwrap();
        assert_eq!(skill.name, "fast-skill");
        assert_eq!(skill.description, "lazy body test");
        assert!(skill.keywords.contains(&"fastword".to_string()));
        assert!(
            skill.body.is_empty(),
            "startup must not retain the large body"
        );
        let loaded = skill.body_text().unwrap();
        assert!(loaded.starts_with("LAZY-BODY-MARKER"));
        assert!(loaded.len() > 256 * 1024);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keywords_trigger_a_microagent_without_its_name_being_mentioned() {
        let skill = snippet(
            "description: k8s reminders\nkeywords: [kubernetes, kubectl]\n",
            "KNOW-MARKER-12: check the pod disruption budget first.",
        );
        assert_eq!(skill.keywords, vec!["kubectl", "kubernetes"]);
        let skills = single(skill);

        let hit = match_skill(&skills, "deploy the service to Kubernetes please").expect("hit");
        assert!(hit.body.contains("KNOW-MARKER-12"), "{}", hit.body);
        assert!(
            match_skill(&skills, "plain talk about cargo packaging").is_none(),
            "an unrelated message must not trigger the microagent"
        );
    }

    #[test]
    fn keyword_matching_respects_word_boundaries() {
        let skills = single(snippet("keywords: [go]\n", "body"));
        assert!(match_skill(&skills, "rewrite it in Go please").is_some());
        assert!(
            match_skill(&skills, "cargo build fails").is_none(),
            "the 'go' inside 'cargo' must not count"
        );
    }

    #[test]
    fn keywords_accept_a_comma_separated_string() {
        let skill = snippet("keywords: Kubernetes, kubectl,   pod\n", "body");
        assert_eq!(skill.keywords, vec!["kubectl", "kubernetes", "pod"]);
    }

    #[test]
    fn a_skill_name_still_beats_a_keyword_hit() {
        let named = snippet("name: webvuln\nkeywords: [notes]\n", "NAME-MARKER");
        let keyed = snippet("name: other-snippet\nkeywords: [webvuln]\n", "KEY-MARKER");
        let skills = HashMap::from([(named.name.clone(), named), (keyed.name.clone(), keyed)]);
        let hit = match_skill(&skills, "look at webvuln and also notes").expect("hit");
        assert!(hit.body.contains("NAME-MARKER"), "{}", hit.body);
    }
}
