//! Codex-style `apply_patch`: one patch document, many files, multi-hunk
//! updates plus add/delete/move. Every hunk must match exactly once, so a
//! stale patch fails loudly instead of silently editing the wrong place.

use crate::tools::fs;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Verb {
    Add,
    Delete,
    Update,
}

struct Section {
    verb: Verb,
    path: String,
    move_to: Option<String>,
    body: Vec<String>,
}

/// Every path a patch touches (including `*** Move to:` destinations), for
/// scope checks and read-before-edit tracking. Unparsable input yields no
/// paths; the tool itself will report the parse error.
pub fn paths(patch: &str) -> Vec<String> {
    match parse(patch) {
        Ok(sections) => sections
            .iter()
            .flat_map(|section| {
                let mut all = vec![section.path.clone()];
                if let Some(destination) = &section.move_to {
                    all.push(destination.clone());
                }
                all
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn parse(patch: &str) -> Result<Vec<Section>> {
    let mut sections: Vec<Section> = Vec::new();
    let mut started = false;
    let mut ended = false;
    for line in patch.lines() {
        let line = line.strip_prefix('\r').unwrap_or(line);
        if line.trim() == "*** Begin Patch" {
            if started {
                bail!("nested *** Begin Patch");
            }
            started = true;
            continue;
        }
        if line.trim() == "*** End Patch" {
            if !started {
                bail!("*** End Patch without *** Begin Patch");
            }
            ended = true;
            break;
        }
        if !started {
            if line.trim().is_empty() {
                continue;
            }
            bail!("content before *** Begin Patch: {line}");
        }
        if let Some(rest) = line.strip_prefix("*** Update File:") {
            sections.push(Section {
                verb: Verb::Update,
                path: rest.trim().to_string(),
                move_to: None,
                body: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("*** Add File:") {
            sections.push(Section {
                verb: Verb::Add,
                path: rest.trim().to_string(),
                move_to: None,
                body: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("*** Delete File:") {
            sections.push(Section {
                verb: Verb::Delete,
                path: rest.trim().to_string(),
                move_to: None,
                body: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("*** Move to:") {
            let section = sections
                .last_mut()
                .context("*** Move to without a file section")?;
            if section.verb != Verb::Update {
                bail!("*** Move to only applies to an *** Update File section");
            }
            section.move_to = Some(rest.trim().to_string());
        } else {
            let section = sections
                .last_mut()
                .context("patch line outside a file section")?;
            section.body.push(line.to_string());
        }
    }
    if !started {
        bail!("patch must start with *** Begin Patch");
    }
    if !ended {
        bail!("patch is missing *** End Patch");
    }
    if sections.is_empty() {
        bail!("patch contains no file sections");
    }
    Ok(sections)
}

/// Split the update body into hunks and apply them one at a time. Each hunk's
/// removed-plus-context block must appear exactly once in the file.
fn apply_hunks(original: &str, body: &[String], path: &str) -> Result<String> {
    let trailing_newline = original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let mut hunks: Vec<Vec<&String>> = Vec::new();
    for line in body {
        if line.starts_with("@@") {
            hunks.push(Vec::new());
        } else {
            let hunk = hunks
                .last_mut()
                .with_context(|| format!("{path}: line outside a hunk: {line}"))?;
            hunk.push(line);
        }
    }
    if hunks.is_empty() {
        bail!("{path}: update section needs a @@ hunk header");
    }
    let mut applied = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let label = index + 1;
        let mut old: Vec<String> = Vec::new();
        let mut new: Vec<String> = Vec::new();
        for line in hunk {
            if line.is_empty() {
                old.push(String::new());
                new.push(String::new());
                continue;
            }
            let (tag, rest) = line.split_at(1);
            match tag {
                " " => {
                    old.push(rest.to_string());
                    new.push(rest.to_string());
                }
                "-" => old.push(rest.to_string()),
                "+" => new.push(rest.to_string()),
                other => bail!(
                    "{path} hunk {label}: line must start with ' ', '-' or '+' (got {other:?}): {line}"
                ),
            }
        }
        if old.is_empty() {
            bail!("{path} hunk {label}: nothing to anchor on — add a context line");
        }
        let hits: Vec<usize> = lines
            .windows(old.len())
            .enumerate()
            .filter(|(_, window)| window.iter().zip(&old).all(|(have, want)| have == want))
            .map(|(offset, _)| offset)
            .collect();
        if hits.is_empty() {
            bail!(
                "{path} hunk {label}: no match in the current file. Expected to find:\n{}",
                old.join("\n")
            );
        }
        if hits.len() > 1 {
            bail!(
                "{path} hunk {label}: {} matches — add context lines to disambiguate",
                hits.len()
            );
        }
        let at = hits[0];
        lines.splice(at..at + old.len(), new.iter().cloned());
        applied += 1;
    }
    let mut out = lines.join("\n");
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    let _ = applied;
    Ok(out)
}

/// A fully validated change. Every section is parsed, matched, and checked
/// before anything touches the disk, so a stale patch in section three cannot
/// leave section one half-applied.
enum Action {
    Write {
        target: PathBuf,
        content: String,
        fresh: bool,
    },
    Delete {
        target: PathBuf,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
        content: String,
    },
}

struct Step {
    summary: String,
    action: Action,
}

fn plan(root: &std::path::Path, section: &Section) -> Result<Step> {
    match section.verb {
        Verb::Add => {
            let target = fs::resolve(root, &section.path)?;
            if target.exists() {
                bail!("{} already exists — use *** Update File", section.path);
            }
            let mut content = String::new();
            for line in &section.body {
                match line.strip_prefix('+') {
                    Some(rest) => {
                        content.push_str(rest);
                        content.push('\n');
                    }
                    None if line.trim().is_empty() => {}
                    None => bail!(
                        "{}: add-file line must start with '+': {line}",
                        section.path
                    ),
                }
            }
            Ok(Step {
                summary: format!("added {}", section.path),
                action: Action::Write {
                    target,
                    content,
                    fresh: true,
                },
            })
        }
        Verb::Delete => {
            if section.body.iter().any(|line| !line.trim().is_empty()) {
                bail!("*** Delete File {} must not have a body", section.path);
            }
            let target = fs::resolve(root, &section.path)?;
            if !target.exists() {
                bail!("{} does not exist", section.path);
            }
            Ok(Step {
                summary: format!("deleted {}", section.path),
                action: Action::Delete { target },
            })
        }
        Verb::Update => {
            let target = fs::resolve(root, &section.path)?;
            let original = std::fs::read_to_string(&target)
                .with_context(|| format!("read {}", section.path))?;
            let hunks = section
                .body
                .iter()
                .filter(|line| line.starts_with("@@"))
                .count();
            let content = apply_hunks(&original, &section.body, &section.path)?;
            match &section.move_to {
                Some(destination) => {
                    let to = fs::resolve(root, destination)?;
                    if to.exists() {
                        bail!("{destination} already exists");
                    }
                    Ok(Step {
                        summary: format!(
                            "updated {} ({hunks} hunks) and moved it to {destination}",
                            section.path
                        ),
                        action: Action::Move {
                            from: target,
                            to,
                            content,
                        },
                    })
                }
                None => Ok(Step {
                    summary: format!("updated {} ({hunks} hunks)", section.path),
                    action: Action::Write {
                        target,
                        content,
                        fresh: false,
                    },
                }),
            }
        }
    }
}

/// Apply a whole patch document. Returns the human summary and the paths this
/// patch created, so the caller can track them like `write_file` does.
pub async fn apply(root: &std::path::Path, patch: &str) -> Result<(String, Vec<PathBuf>)> {
    let sections = parse(patch)?;
    let mut steps = Vec::with_capacity(sections.len());
    for section in &sections {
        steps.push(plan(root, section)?);
    }
    let created: Vec<PathBuf> = steps
        .iter()
        .filter_map(|step| match &step.action {
            Action::Write {
                target,
                fresh: true,
                ..
            } => Some(target.clone()),
            Action::Move { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect();
    let mut applied: Vec<String> = Vec::with_capacity(steps.len());
    for step in steps {
        match step.action {
            Action::Write {
                target, content, ..
            } => {
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&target, content).await?;
            }
            Action::Delete { target } => {
                tokio::fs::remove_file(&target).await?;
            }
            Action::Move { from, to, content } => {
                if let Some(parent) = to.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&to, content).await?;
                tokio::fs::remove_file(&from).await?;
            }
        }
        applied.push(step.summary);
    }
    Ok((format!("applied patch: {}", applied.join(", ")), created))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wrosecode-patch-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn multi_hunk_update_applies_in_order() {
        let dir = scratch("multi");
        std::fs::write(dir.join("main.rs"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: main.rs\n@@\n alpha\n-beta\n+bravo\n@@\n gamma\n-delta\n+delta2\n*** End Patch";
        let (summary, created) = apply(&dir, patch).await.unwrap();
        assert!(summary.contains("updated main.rs (2 hunks)"), "{summary}");
        assert!(created.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join("main.rs")).unwrap(),
            "alpha\nbravo\ngamma\ndelta2\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn ambiguous_hunk_is_rejected_and_leaves_the_file_alone() {
        let dir = scratch("ambiguous");
        let original = "one\nsame\ntwo\nsame\nthree\nsame\n";
        std::fs::write(dir.join("dup.txt"), original).unwrap();
        let patch = "*** Begin Patch\n*** Update File: dup.txt\n@@\n-same\n+twice\n*** End Patch";
        let error = apply(&dir, patch).await.unwrap_err().to_string();
        assert!(error.contains("3 matches"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("dup.txt")).unwrap(),
            original
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn stale_hunk_reports_what_it_expected() {
        let dir = scratch("stale");
        std::fs::write(dir.join("lib.rs"), "one\ntwo\n").unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: lib.rs\n@@\n one\n-nine\n+nine\n*** End Patch";
        let error = apply(&dir, patch).await.unwrap_err().to_string();
        assert!(error.contains("no match"), "{error}");
        assert!(error.contains("nine"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("lib.rs")).unwrap(),
            "one\ntwo\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn add_update_move_and_delete_apply_together() {
        let dir = scratch("mix");
        std::fs::write(dir.join("old.txt"), "keep me\n").unwrap();
        std::fs::write(dir.join("obsolete.txt"), "throw away\n").unwrap();
        let patch = "*** Begin Patch\n*** Add File: notes/new.txt\n+hello\n*** Update File: old.txt\n*** Move to: renamed.txt\n@@\n keep me\n*** Delete File: obsolete.txt\n*** End Patch";
        let (summary, created) = apply(&dir, patch).await.unwrap();
        assert!(summary.contains("added notes/new.txt"), "{summary}");
        assert!(summary.contains("moved it to renamed.txt"), "{summary}");
        assert!(summary.contains("deleted obsolete.txt"), "{summary}");
        assert_eq!(created.len(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("renamed.txt")).unwrap(),
            "keep me\n"
        );
        assert!(!dir.join("old.txt").exists());
        assert!(!dir.join("obsolete.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("notes/new.txt")).unwrap(),
            "hello\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_stale_section_writes_nothing_at_all() {
        let dir = scratch("atomic");
        std::fs::write(dir.join("live.txt"), "one\n").unwrap();
        std::fs::write(dir.join("stale.txt"), "one\n").unwrap();
        let patch = "*** Begin Patch\n*** Add File: fresh.txt\n+new\n*** Update File: stale.txt\n@@\n missing\n*** End Patch";
        let error = apply(&dir, patch).await.unwrap_err().to_string();
        assert!(error.contains("no match"), "{error}");
        assert!(
            !dir.join("fresh.txt").exists(),
            "the whole document must be rejected before any write"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("stale.txt")).unwrap(),
            "one\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn delete_rejects_a_body_and_a_missing_file() {
        let dir = scratch("delete");
        std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
        let with_body = "*** Begin Patch\n*** Delete File: gone.txt\n+oops\n*** End Patch";
        assert!(apply(&dir, with_body)
            .await
            .unwrap_err()
            .to_string()
            .contains("must not have a body"));
        let missing = "*** Begin Patch\n*** Delete File: absent.txt\n*** End Patch";
        assert!(apply(&dir, missing)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not exist"));
        assert!(dir.join("gone.txt").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn malformed_documents_are_rejected() {
        let dir = scratch("malformed");
        for patch in [
            "not a patch",
            "*** End Patch",
            "*** Begin Patch\n*** Update File: x\n@@\n a\n",
            "*** Begin Patch\n@@\n a\n*** End Patch",
        ] {
            let error = apply(&dir, patch).await.unwrap_err().to_string();
            assert!(!error.is_empty());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn paths_reports_every_touched_file_for_scope_checks() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** Update File: b.txt\n*** Move to: c.txt\n@@\n x\n*** Delete File: d.txt\n*** End Patch";
        assert_eq!(paths(patch), vec!["a.txt", "b.txt", "c.txt", "d.txt"]);
        assert!(paths("garbage").is_empty());
    }
}
