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
}
