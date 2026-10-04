use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
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
}

#[derive(Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    fork: Option<bool>,
    keywords: Option<serde_yaml::Value>,
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
    let mut dirs = vec![
        root.join("skills"),
        root.join(".ctf/skills"),
        default_skills_dir(),
    ];
    dirs.extend_from_slice(extra);
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
    for dir in dirs {
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
            let raw = std::fs::read_to_string(entry.path())?;
            let fallback = entry
                .path()
                .parent()
                .unwrap_or(entry.path())
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let skill = parse_skill(&raw, &fallback)?;
            found.insert(skill.name.clone(), skill);
        }
    }
    Ok(found)
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
    })
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
