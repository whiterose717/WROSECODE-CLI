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
    pub body: String,
}

#[derive(Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    fork: Option<bool>,
}

pub fn discover(root: &Path, extra: &[PathBuf]) -> Result<HashMap<String, Skill>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut dirs = vec![
        root.join("skills"),
        root.join(".ctf/skills"),
        home.join(".wrosecode/skills"),
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

fn parse_skill(raw: &str, fallback: &str) -> Result<Skill> {
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
                skill
                    .name
                    .split('-')
                    .filter(|word| word.len() >= 4 && lower.contains(word))
                    .count()
            };
            (score > 0).then_some((score, skill))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)))
        .map(|(_, skill)| skill)
}
