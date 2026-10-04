//! Turn-scoped file snapshots backing `/undo` and `/redo`.
//!
//! Every batch that touches files through `write_file`, `edit_file`, or
//! `apply_patch` first captures the pre-edit content of exactly those paths
//! under `.wrosecode/snapshots/undo/<id>/`. `/undo` puts the last snapshot
//! back and moves it onto the redo stack; `/redo` reverses that. Nothing
//! touches git, so snapshots work in a directory that was never initialised
//! and never scribble on the developer's own history.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn stack(root: &Path, kind: &str) -> PathBuf {
    root.join(".wrosecode").join("snapshots").join(kind)
}

fn list(root: &Path, kind: &str) -> Result<Vec<u64>> {
    let dir = stack(root, kind);
    let mut ids = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
        Err(error) => return Err(error).context("read snapshot stack"),
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if let Ok(id) = name.parse::<u64>() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn highest(root: &Path, kind: &str) -> Option<u64> {
    list(root, kind).ok().and_then(|ids| ids.last().copied())
}

fn dir_of(root: &Path, kind: &str, id: u64) -> PathBuf {
    stack(root, kind).join(format!("{id:06}"))
}

/// Relative path marker: `+path` stores the file bytes, `!path` records that
/// the file did not exist yet.
enum Entry {
    Content(PathBuf),
    Absent(PathBuf),
}

fn read_manifest(dir: &Path) -> Result<Vec<Entry>> {
    let manifest = std::fs::read_to_string(dir.join("manifest.txt"))
        .with_context(|| format!("read {}", dir.join("manifest.txt").display()))?;
    let mut entries = Vec::new();
    for line in manifest.lines() {
        let (marker, path) = line.split_at(1);
        let path = PathBuf::from(path);
        match marker {
            "+" => entries.push(Entry::Content(path)),
            "!" => entries.push(Entry::Absent(path)),
            other => bail!("corrupt snapshot manifest marker {other:?}"),
        }
    }
    Ok(entries)
}

fn write_manifest(dir: &Path, entries: &[Entry]) -> Result<()> {
    let mut manifest = String::new();
    for entry in entries {
        match entry {
            Entry::Content(path) => {
                manifest.push('+');
                manifest.push_str(&path.to_string_lossy());
            }
            Entry::Absent(path) => {
                manifest.push('!');
                manifest.push_str(&path.to_string_lossy());
            }
        }
        manifest.push('\n');
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("manifest.txt"), manifest)?;
    Ok(())
}

/// Record the current content of `paths` into `<stack>/<kind>/<id>/`.
/// Overwrites any stale entry that already sits at that id: an entry there is
/// by definition unreachable.
fn capture_into(root: &Path, kind: &str, id: u64, paths: &[PathBuf]) -> Result<()> {
    let dir = dir_of(root, kind, id);
    let _ = std::fs::remove_dir_all(&dir);
    let mut entries: Vec<Entry> = Vec::new();
    for path in paths {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        if entries.iter().any(|entry| match entry {
            Entry::Content(stored) | Entry::Absent(stored) => stored == relative,
        }) {
            continue;
        }
        if path.is_file() {
            let bytes = std::fs::read(path)?;
            let stored = dir.join("files").join(relative);
            if let Some(parent) = stored.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&stored, &bytes)?;
            entries.push(Entry::Content(relative.to_path_buf()));
        } else {
            entries.push(Entry::Absent(relative.to_path_buf()));
        }
    }
    if entries.is_empty() {
        bail!("nothing to capture");
    }
    write_manifest(&dir, &entries)?;
    Ok(())
}

/// Paths a stored snapshot refers to, resolved back against the project root.
fn stored_paths(root: &Path, kind: &str, id: u64) -> Vec<PathBuf> {
    read_manifest(&dir_of(root, kind, id))
        .map(|entries| {
            entries
                .iter()
                .map(|entry| match entry {
                    Entry::Content(path) | Entry::Absent(path) => root.join(path),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Capture the current content of `paths` as a new undo snapshot. Returns the
/// snapshot id, or `None` when there was nothing to record.
pub fn capture(root: &Path, paths: &[PathBuf]) -> Option<u64> {
    let id = highest(root, "undo").map_or(1, |last| last + 1);
    capture_into(root, "undo", id, paths).ok()?;
    Some(id)
}

/// Restore a snapshot directory back onto the working tree.
fn restore(root: &Path, dir: &Path) -> Result<Vec<String>> {
    let mut restored = Vec::new();
    for entry in read_manifest(dir)? {
        match entry {
            Entry::Content(relative) => {
                let source = dir.join("files").join(&relative);
                let target = root.join(&relative);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&source, &target)
                    .with_context(|| format!("restore {}", target.display()))?;
                restored.push(relative.display().to_string());
            }
            Entry::Absent(relative) => {
                let target = root.join(&relative);
                if target.exists() {
                    std::fs::remove_file(&target)?;
                }
                restored.push(relative.display().to_string());
            }
        }
    }
    Ok(restored)
}

fn preview(root: &Path, kind: &str, id: u64) -> String {
    let files: Vec<String> = stored_paths(root, kind, id)
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .collect();
    join_preview(&files)
}

/// Restore the newest snapshot, keeping the tree it replaces on the redo
/// stack so `/redo` can put it back.
pub fn undo(root: &Path) -> Result<String> {
    let Some(id) = highest(root, "undo") else {
        bail!("nothing to undo — no file edits have been captured yet");
    };
    let dir = dir_of(root, "undo", id);
    let files = preview(root, "undo", id);
    let current = stored_paths(root, "undo", id);
    capture_into(root, "redo", id, &current)
        .context("could not keep the current tree for /redo")?;
    let restored = restore(root, &dir)?;
    std::fs::remove_dir_all(&dir).context("drop the applied snapshot")?;
    Ok(format!(
        "restored {} from snapshot {:06}: {}",
        plural(restored.len()),
        id,
        files
    ))
}

/// Re-apply the newest undone snapshot, keeping the tree it replaces on the
/// undo stack.
pub fn redo(root: &Path) -> Result<String> {
    let Some(id) = highest(root, "redo") else {
        bail!("nothing to redo");
    };
    let dir = dir_of(root, "redo", id);
    let files = preview(root, "redo", id);
    let current = stored_paths(root, "redo", id);
    capture_into(root, "undo", id, &current)
        .context("could not keep the current tree for /undo")?;
    let restored = restore(root, &dir)?;
    std::fs::remove_dir_all(&dir).context("drop the reapplied snapshot")?;
    Ok(format!(
        "reapplied {} in snapshot {:06}: {}",
        plural(restored.len()),
        id,
        files
    ))
}

/// Drop the snapshot just taken because the turn ended up changing nothing:
/// an undo that restores identical bytes is noise on the stack.
pub fn discard_latest(root: &Path) {
    if let Some(id) = highest(root, "undo") {
        let _ = std::fs::remove_dir_all(dir_of(root, "undo", id));
    }
}

/// A successful edit invalidates anything waiting on the redo stack: those
/// snapshots describe a tree that no longer exists.
pub fn commit(root: &Path) {
    if let Ok(ids) = list(root, "redo") {
        for id in ids {
            let _ = std::fs::remove_dir_all(dir_of(root, "redo", id));
        }
    }
}

/// How many snapshots sit on each stack, for `/help`-style status lines.
pub fn counts(root: &Path) -> (usize, usize) {
    (
        list(root, "undo").map(|ids| ids.len()).unwrap_or(0),
        list(root, "redo").map(|ids| ids.len()).unwrap_or(0),
    )
}

fn plural(count: usize) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
}

fn join_preview(files: &[String]) -> String {
    const MAX: usize = 6;
    if files.is_empty() {
        return "no paths".to_string();
    }
    let shown: Vec<&str> = files.iter().take(MAX).map(String::as_str).collect();
    if files.len() > MAX {
        format!("{} (+{} more)", shown.join(", "), files.len() - MAX)
    } else {
        shown.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wrosecode-snapshot-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn undo_restores_content_and_redo_puts_the_edit_back() {
        let root = scratch("roundtrip");
        let file = root.join("main.rs");
        std::fs::write(&file, "before\n").unwrap();

        let id = capture(&root, std::slice::from_ref(&file)).expect("snapshot");
        assert_eq!(id, 1);
        std::fs::write(&file, "after\n").unwrap();

        let message = undo(&root).unwrap();
        assert!(message.contains("snapshot 000001"), "{message}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before\n");
        assert_eq!(counts(&root), (0, 1));

        let message = redo(&root).unwrap();
        assert!(message.contains("reapplied"), "{message}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after\n");
        assert_eq!(counts(&root), (1, 0));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let root = scratch("invalidate");
        let file = root.join("notes.txt");
        std::fs::write(&file, "v1\n").unwrap();
        capture(&root, std::slice::from_ref(&file)).unwrap();
        std::fs::write(&file, "v2\n").unwrap();
        undo(&root).unwrap();
        assert_eq!(counts(&root), (0, 1));

        capture(&root, std::slice::from_ref(&file)).unwrap();
        commit(&root);
        assert_eq!(counts(&root), (1, 0));
        assert!(redo(&root).is_err(), "redo must be empty after a new edit");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_created_by_the_edit_disappear_on_undo() {
        let root = scratch("created");
        let fresh = root.join("notes/new.txt");
        let id = capture(&root, std::slice::from_ref(&fresh)).expect("absent snapshot");
        assert_eq!(id, 1);
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(&fresh, "created later\n").unwrap();

        undo(&root).unwrap();
        assert!(!fresh.exists(), "undo must remove files that did not exist");
        redo(&root).unwrap();
        assert!(fresh.exists(), "redo must bring the new file back");
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "created later\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshots_stack_in_order_and_discard_drops_the_last() {
        let root = scratch("stack");
        let file = root.join("a.txt");
        std::fs::write(&file, "1\n").unwrap();
        capture(&root, std::slice::from_ref(&file)).unwrap();
        std::fs::write(&file, "2\n").unwrap();
        capture(&root, std::slice::from_ref(&file)).unwrap();
        std::fs::write(&file, "3\n").unwrap();
        assert_eq!(counts(&root).0, 2);

        discard_latest(&root);
        assert_eq!(counts(&root).0, 1);
        undo(&root).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "1\n");
        assert!(undo(&root).is_err(), "stack is empty now");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nothing_captured_means_nothing_to_undo() {
        let root = scratch("empty");
        assert!(capture(&root, &[]).is_none());
        assert!(undo(&root)
            .unwrap_err()
            .to_string()
            .contains("nothing to undo"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binary_files_survive_the_round_trip() {
        let root = scratch("binary");
        let file = root.join("blob.bin");
        let bytes: Vec<u8> = vec![0, 159, 146, 150, 255, 10];
        std::fs::write(&file, &bytes).unwrap();
        capture(&root, std::slice::from_ref(&file)).unwrap();
        std::fs::write(&file, b"changed").unwrap();
        undo(&root).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        std::fs::remove_dir_all(root).unwrap();
    }
}
