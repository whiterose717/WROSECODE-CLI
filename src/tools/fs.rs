use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

pub fn resolve(root: &Path, path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        bail!("parent traversal is not allowed");
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let parent = joined.parent().context("invalid path")?;
    let ancestor = parent
        .ancestors()
        .find(|candidate| candidate.exists())
        .context("no existing ancestor")?;
    let canonical_parent = ancestor
        .canonicalize()?
        .join(parent.strip_prefix(ancestor)?);
    if !canonical_parent.starts_with(root.canonicalize()?) {
        bail!("path outside project");
    }
    let target = canonical_parent.join(joined.file_name().context("invalid file name")?);
    if target.exists() && !target.canonicalize()?.starts_with(root.canonicalize()?) {
        bail!("path outside project");
    }
    Ok(target)
}

pub async fn read(root: &Path, path: &str) -> Result<String> {
    Ok(tokio::fs::read_to_string(resolve(root, path)?).await?)
}

pub async fn write(root: &Path, path: &str, content: &str) -> Result<String> {
    let target = resolve(root, path)?;
    tokio::fs::create_dir_all(target.parent().context("invalid parent")?).await?;
    tokio::fs::write(&target, content).await?;
    Ok(format!(
        "wrote {} bytes to {}",
        content.len(),
        target.display()
    ))
}

pub async fn edit(root: &Path, path: &str, old: &str, new: &str) -> Result<String> {
    if old.is_empty() {
        bail!("old text must be nonempty");
    }
    let target = resolve(root, path)?;
    let text = tokio::fs::read_to_string(&target).await?;
    let count = text
        .char_indices()
        .filter(|(index, _)| text[*index..].starts_with(old))
        .count();
    if count != 1 {
        bail!("expected exactly one match, found {count}");
    }
    tokio::fs::write(&target, text.replacen(old, new, 1)).await?;
    Ok(format!("edited {}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn edit_rejects_ambiguous_match_without_changing_file() {
        let dir = std::env::temp_dir().join(format!("wrosecode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "sample.txt", "aa aa").await.unwrap();
        assert!(edit(&dir, "sample.txt", "aa", "bb").await.is_err());
        assert_eq!(read(&dir, "sample.txt").await.unwrap(), "aa aa");
        write(&dir, "sample.txt", "aaa").await.unwrap();
        assert!(edit(&dir, "sample.txt", "aa", "bb").await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
