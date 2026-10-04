use crate::session::Session;
use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Verdict {
    Verified,
    Rejected,
    Inconclusive,
}

pub fn check_flag(flag: &str, evidence: &str) -> Verdict {
    let valid = Regex::new(r"^(?:flag|CTF|picoCTF|HTB)\{[^}\r\n]{1,512}\}$")
        .is_ok_and(|pattern| pattern.is_match(flag));
    if !valid {
        Verdict::Rejected
    } else if evidence.contains(flag) {
        Verdict::Verified
    } else {
        Verdict::Inconclusive
    }
}

pub fn writeup(root: &Path, session: &Session, flags: &[String]) -> Result<PathBuf> {
    let dir = root.join(".ctf/reports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", session.name));
    let mut output = format!(
        "# CTF Writeup: {}\n\n## Overview\n\n- Session: `{}`\n- Provider: `{}`\n- Model: `{}`\n- Started: `{}`\n\n## Flags\n\n",
        session.summary, session.name, session.provider_name, session.model, session.created
    );
    if flags.is_empty() {
        output.push_str("No flag was recorded.\n");
    } else {
        for flag in flags {
            output.push_str(&format!("- `{flag}`\n"));
        }
    }
    output.push_str("\n## Evidence Timeline\n\n");
    for (speaker, text) in &session.transcript {
        output.push_str(&format!(
            "### {speaker}\n\n```text\n{}\n```\n\n",
            sanitize(text)
        ));
    }
    output.push_str("## Reproduction\n\nRepeat the commands in the evidence timeline against the original challenge artifact.\n");
    std::fs::write(&path, output)?;
    Ok(path)
}

fn sanitize(value: &str) -> String {
    value.replace("```", "` ` `")
}

/// Slug for the per-challenge writeup path: `writeups/<challenge>.md`
/// (spec 5.8).
pub fn writeup_slug(target: &str) -> String {
    let cleaned: String = target
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = cleaned
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "challenge".into()
    } else {
        slug.chars().take(80).collect()
    }
}

/// The autopilot's writeup (spec 5.8): `writeups/<challenge>.md`, built from
/// the session transcript, recorded flags, and the `ctf-notes.md` scratch pad.
pub fn writeup_challenge(
    root: &Path,
    session: &Session,
    flags: &[String],
    notes: Option<&str>,
) -> Result<PathBuf> {
    let dir = root.join("writeups");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", writeup_slug(&session.summary)));
    let mut output = format!(
        "# Writeup: {}\n\n## Overview\n\n- Session: `{}`\n- Provider: `{}`\n- Model: `{}`\n- Started: `{}`\n\n## Flags\n\n",
        session.summary, session.name, session.provider_name, session.model, session.created
    );
    if flags.is_empty() {
        output.push_str("No flag was recorded.\n");
    } else {
        for flag in flags {
            output.push_str(&format!("- `{flag}`\n"));
        }
    }
    output.push_str("\n## Approach\n\n");
    if let Some(notes) = notes.filter(|notes| !notes.trim().is_empty()) {
        output.push_str(&notes.replace("# CTF notes —", "## Challenge notes —"));
        output.push('\n');
    } else {
        output.push_str("No notes were recorded.\n");
    }
    output.push_str("\n## Evidence Timeline\n\n");
    for (speaker, text) in &session.transcript {
        output.push_str(&format!(
            "### {speaker}\n\n```text\n{}\n```\n\n",
            sanitize(text)
        ));
    }
    output.push_str(
        "## Reproduction\n\nRepeat the commands in the evidence timeline against the original challenge artifact.\n",
    );
    std::fs::write(&path, output)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checker_requires_format_and_evidence() {
        let flag = format!("flag{}", "{proof}");
        assert!(matches!(check_flag(&flag, &flag), Verdict::Verified));
        assert!(matches!(check_flag("bad", "bad"), Verdict::Rejected));
        assert!(matches!(check_flag(&flag, "none"), Verdict::Inconclusive));
    }

    #[test]
    fn writeup_slug_is_stable_and_bounded() {
        assert_eq!(writeup_slug("Rev/Box #3"), "rev-box-3");
        assert_eq!(writeup_slug("///"), "challenge");
        assert_eq!(writeup_slug(&"a".repeat(200)).len(), 80);
    }

    #[test]
    fn writeup_challenge_writes_notes_flags_and_transcript() {
        let root = std::env::temp_dir().join(format!("wrose-writeup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut session = Session::fresh();
        session.summary = "ctf rev/box".into();
        session.transcript = vec![("Agent".into(), "decoded `flag{demo}`".into())];
        let notes = "# CTF notes — rev/box\n\n## Dead ends\n\n- xor brute force\n";
        let path =
            writeup_challenge(&root, &session, &["flag{demo}".to_string()], Some(notes)).unwrap();
        assert_eq!(path.file_name().unwrap(), "ctf-rev-box.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("flag{demo}"));
        assert!(body.contains("## Challenge notes — rev/box"));
        assert!(body.contains("decoded `flag{demo}`"));
        assert!(body.contains("## Evidence Timeline"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writeup_challenge_without_notes_still_writes() {
        let root = std::env::temp_dir().join(format!("wrose-writeup-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let session = Session::fresh();
        let path = writeup_challenge(&root, &session, &[], None).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("No flag was recorded."));
        assert!(body.contains("No notes were recorded."));
        let _ = std::fs::remove_dir_all(&root);
    }
}
