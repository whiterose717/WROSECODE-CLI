//! pi-mono-style packages: install skill packs from a git URL or a local
//! directory into the user skill directory, with the version recorded so a
//! package can be reinstalled or replaced later.
//!
//! The install is the sandbox: a package is copied in through hard ceilings
//! (file count, per-file size, total size, two directory levels — the span
//! `skills::discover` can actually see), symlinks are never followed or
//! copied, every `SKILL.md` must parse as valid UTF-8 frontmatter before the
//! package is accepted, and nothing from a package is ever executed — only
//! its markdown enters prompts, exactly like a hand-written skill.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Maximum files copied in from one package.
pub const MAX_FILES: usize = 64;
/// Maximum size of a single file inside a package.
pub const MAX_FILE_BYTES: u64 = 512 * 1024;
/// Maximum total bytes copied in from one package.
pub const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum depth of a copied file relative to the package root. `discover`
/// walks the skills directory to depth 3, which is exactly `<pkg>/`, one
/// directory level, and the file — deeper content would be dead weight.
const MAX_DEPTH: usize = 2;
const MANIFEST_FILE: &str = "packages.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledPkg {
    /// Where it came from: the git URL or path the user installed.
    pub source: String,
    /// Git short commit (`--short=12`) or an FNV-1a content hash for paths.
    pub version: String,
    /// Skill names the package provides (its `name:` frontmatter).
    pub skills: Vec<String>,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub packages: BTreeMap<String, InstalledPkg>,
}

/// The manifest lives beside the skills directory (`~/.wrosecode/`), where
/// discovery never looks for `SKILL.md`.
pub fn manifest_path(skills_dir: &Path) -> PathBuf {
    skills_dir
        .parent()
        .map(|parent| parent.join(MANIFEST_FILE))
        .unwrap_or_else(|| skills_dir.join(MANIFEST_FILE))
}

pub fn load_manifest(skills_dir: &Path) -> Manifest {
    std::fs::read_to_string(manifest_path(skills_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_manifest(skills_dir: &Path, manifest: &Manifest) -> Result<()> {
    let path = manifest_path(skills_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialised = serde_json::to_string_pretty(manifest)?;
    // Write beside the target and rename, so a crash cannot leave a
    // half-written manifest behind.
    let staging = path.with_extension("json.tmp");
    std::fs::write(&staging, serialised)?;
    std::fs::rename(&staging, &path)?;
    Ok(())
}

/// What an install source resolves to: a git checkout or a directory.
enum Source {
    Git {
        url: String,
        reference: Option<String>,
    },
    Directory(PathBuf),
}

/// `<git-url>[#ref]` — anything with a scheme, an scp-style `git@host`, or a
/// `.git` suffix — is a git source; everything else is a local directory.
/// The `#ref` (branch, tag, or commit) pins what gets installed.
fn parse_source(raw: &str) -> Result<(Source, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("empty install source");
    }
    let (base, reference) = match trimmed.rsplit_once('#') {
        Some((base, reference)) if !reference.is_empty() => (base, Some(reference.to_string())),
        _ => (trimmed, None),
    };
    let source = if let Some(url) = base.strip_prefix("git+") {
        Source::Git {
            url: url.to_string(),
            reference,
        }
    } else if base.contains("://") || base.starts_with("git@") || base.ends_with(".git") {
        Source::Git {
            url: base.to_string(),
            reference,
        }
    } else {
        if reference.is_some() {
            bail!("a #ref only applies to git sources, not a directory path");
        }
        Source::Directory(PathBuf::from(base))
    };
    Ok((source, trimmed.to_string()))
}

/// Install `raw` (`<git-url>[#ref]` or a directory) into `skills_dir`,
/// replacing any package of the same name. Returns the skill names the
/// package now provides.
pub async fn install(skills_dir: &Path, raw: &str) -> Result<Vec<String>> {
    let (source, source_display) = parse_source(raw)?;
    std::fs::create_dir_all(skills_dir)
        .with_context(|| format!("create {}", skills_dir.display()))?;

    // Clone into a scratch directory; the copy below is the only thing that
    // touches the skills directory, and only after every check passes.
    let (src_root, name, version, scratch) = match &source {
        Source::Directory(path) => {
            let path = path
                .canonicalize()
                .with_context(|| format!("{} is not a reachable directory", path.display()))?;
            if !path.is_dir() {
                bail!("{} is not a directory", path.display());
            }
            let skills_canonical = skills_dir.canonicalize().ok();
            if let Some(skills_canonical) = skills_canonical {
                if path.starts_with(&skills_canonical) {
                    bail!("{} is already inside the skills directory", path.display());
                }
            }
            let name = package_name(Some(&path), None)?;
            (path, name, String::new(), None)
        }
        Source::Git { url, reference } => {
            let (checkout, version) = clone_git(url, reference.as_deref()).await?;
            let name = package_name(None, Some(url))?;
            (checkout.clone(), name, version, Some(checkout))
        }
    };

    let dest = skills_dir.join(&name);
    let (files, bytes, names) = match stage(&src_root, &dest) {
        Ok(staged) => staged,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dest);
            if let Some(scratch) = &scratch {
                let _ = std::fs::remove_dir_all(scratch);
            }
            return Err(error);
        }
    };

    let version = match source {
        Source::Directory(_) => content_version(&dest),
        Source::Git { .. } => version,
    };

    let mut manifest = load_manifest(skills_dir);
    manifest.packages.insert(
        name,
        InstalledPkg {
            source: source_display,
            version,
            skills: names.clone(),
            files,
            bytes,
        },
    );
    save_manifest(skills_dir, &manifest)?;
    if let Some(scratch) = scratch {
        let _ = std::fs::remove_dir_all(scratch);
    }
    Ok(names)
}

/// Remove an installed package (directory + manifest entry). Idempotent on
/// an already-missing directory as long as the manifest knows the name.
pub fn uninstall(skills_dir: &Path, name: &str) -> Result<InstalledPkg> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        bail!("invalid package name: {name}");
    }
    let mut manifest = load_manifest(skills_dir);
    let entry = manifest
        .packages
        .remove(name)
        .with_context(|| format!("{name} is not installed"))?;
    let dest = skills_dir.join(name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    save_manifest(skills_dir, &manifest)?;
    Ok(entry)
}

/// Installed packages, sorted by name.
pub fn list(skills_dir: &Path) -> Vec<(String, InstalledPkg)> {
    load_manifest(skills_dir).packages.into_iter().collect()
}

/// `git clone` the source (shallow at `#ref` when possible, a full clone
/// plus checkout when the ref is a commit sha), then return the checkout
/// and the pinned commit.
async fn clone_git(url: &str, reference: Option<&str>) -> Result<(PathBuf, String)> {
    let scratch = std::env::temp_dir().join(format!(
        "wrose-pkg-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos())
            .unwrap_or_default()
    ));
    let attempt = |args: Vec<String>| async move {
        let output = tokio::process::Command::new("git")
            .args(&args)
            .output()
            .await
            .with_context(|| "run git".to_string())?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok::<_, anyhow::Error>(())
    };
    let mut shallow: Vec<String> = vec![
        "clone".into(),
        "--quiet".into(),
        "--depth".into(),
        "1".into(),
    ];
    if let Some(reference) = reference {
        shallow.push("--branch".into());
        shallow.push(reference.into());
    }
    shallow.push(url.into());
    shallow.push(scratch.to_string_lossy().into_owned());
    let shallow_result = attempt(shallow).await;
    if shallow_result.is_err() {
        // A raw commit sha is not a branch name: fall back to a full clone
        // and check the exact ref out.
        let _ = std::fs::remove_dir_all(&scratch);
        let full: Vec<String> = vec![
            "clone".into(),
            "--quiet".into(),
            url.into(),
            scratch.to_string_lossy().into_owned(),
        ];
        let result = attempt(full).await;
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(error.context(format!("git clone {url} failed")));
        }
        if let Some(reference) = reference {
            let checkout =
                attempt(vec!["checkout".into(), "--quiet".into(), reference.into()]).await;
            if let Err(error) = checkout {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(error.context(format!("checkout {reference} failed")));
            }
        }
    }
    let version = git_version(&scratch)
        .await
        .with_context(|| "read the cloned commit")?;
    Ok((scratch, version))
}

async fn git_version(dir: &Path) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(dir)
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Copy the package in under the sandbox ceilings, then require every
/// `SKILL.md` to parse. On any failure the destination is left untouched
/// (the caller removes it).
fn stage(src_root: &Path, dest: &Path) -> Result<(usize, u64, Vec<String>)> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).with_context(|| format!("replace {}", dest.display()))?;
    }
    std::fs::create_dir_all(dest)?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut found_skill = false;
    let result: Result<()> = (|| {
        for entry in WalkDir::new(src_root)
            .max_depth(MAX_DEPTH)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let file_type = entry.file_type();
            // Symlinks are never followed and never copied: a package cannot
            // smuggle in `/etc/passwd` or reach back out of the skills dir.
            if file_type.is_symlink() {
                continue;
            }
            let relative = match entry.path().strip_prefix(src_root) {
                Ok(relative) if !relative.as_os_str().is_empty() => relative,
                _ => continue,
            };
            let target = dest.join(relative);
            if file_type.is_dir() {
                // A directory at the depth cap could only hold files the
                // cap excludes — don't leave dead empty dirs behind.
                if entry.depth() < MAX_DEPTH {
                    std::fs::create_dir_all(&target)?;
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let length = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if length > MAX_FILE_BYTES {
                bail!(
                    "{} is {} bytes; the cap is {}",
                    relative.display(),
                    length,
                    MAX_FILE_BYTES
                );
            }
            if files + 1 > MAX_FILES {
                bail!("more than {MAX_FILES} files");
            }
            if bytes + length > MAX_TOTAL_BYTES {
                bail!("more than {MAX_TOTAL_BYTES} bytes total");
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
            files += 1;
            bytes += length;
            if entry.file_name() == "SKILL.md" {
                found_skill = true;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(dest);
        return Err(error);
    }
    if !found_skill {
        let _ = std::fs::remove_dir_all(dest);
        bail!("no SKILL.md in the package");
    }
    // Every SKILL.md must read as UTF-8 and parse, or discovery would fail
    // for the whole project the first time it walked the directory.
    let names = skill_names(dest)?;
    Ok((files, bytes, names))
}

/// The `name:` frontmatter of every `SKILL.md` in a staged package.
fn skill_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in WalkDir::new(dir)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_name() != "SKILL.md" || !entry.file_type().is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(entry.path())
            .with_context(|| format!("{} is not valid UTF-8", entry.path().display()))?;
        let fallback = entry
            .path()
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let skill = crate::skills::parse_skill(&raw, &fallback)
            .with_context(|| format!("{} has broken frontmatter", entry.path().display()))?;
        names.push(skill.name);
    }
    names.sort();
    names.dedup();
    if names.is_empty() {
        bail!("no SKILL.md in the package");
    }
    Ok(names)
}

/// Package directory name: the source directory's basename, or the git
/// URL's final segment with `.git` stripped — sanitised so it can never
/// climb out of the skills directory.
fn package_name(dir: Option<&Path>, url: Option<&str>) -> Result<String> {
    let raw = match (dir, url) {
        (Some(dir), _) => dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        (_, Some(url)) => url.trim_end_matches('/').rsplit('/').next().map(|segment| {
            segment
                .rsplit_once(':')
                .map(|(_, tail)| tail)
                .unwrap_or(segment)
                .trim_end_matches(".git")
                .to_string()
        }),
        _ => None,
    };
    let cleaned: String = raw
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let cleaned = cleaned.trim_matches(|character| character == '-' || character == '.');
    if cleaned.is_empty() {
        bail!("cannot derive a package name from the source");
    }
    Ok(cleaned.to_string())
}

/// FNV-1a over every file's relative path and contents, in path order —
/// the version of a directory-source package (no git, so no commit).
fn content_version(dir: &Path) -> String {
    let mut entries: Vec<(String, Vec<u8>)> = WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(dir).ok()?.to_path_buf();
            let contents = std::fs::read(entry.path()).ok()?;
            Some((relative.to_string_lossy().into_owned(), contents))
        })
        .collect();
    entries.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (path, contents) in entries {
        for byte in path.as_bytes().iter().chain(contents.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_md(name: &str) -> String {
        format!("---\nname: {name}\ndescription: a test skill\n---\nbody of {name}\n")
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wrose-pkgtest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    async fn git(args: &[&str], dir: &Path) -> String {
        let output = tokio::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn installs_a_directory_package_with_a_content_version() {
        let root = scratch("dir");
        let skills_dir = root.join("skills");
        let source = root.join("pack");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("SKILL.md"), skill_md("pack-root")).unwrap();
        std::fs::write(source.join("nested/SKILL.md"), skill_md("pack-nested")).unwrap();

        let names = install(&skills_dir, source.to_str().unwrap())
            .await
            .expect("install");
        assert_eq!(
            names,
            vec!["pack-nested".to_string(), "pack-root".to_string()]
        );
        assert!(skills_dir.join("pack/nested/SKILL.md").exists());

        let manifest = load_manifest(&skills_dir);
        let entry = manifest.packages.get("pack").expect("manifest entry");
        assert_eq!(entry.source, source.to_str().unwrap());
        assert_eq!(entry.version.len(), 16, "content hash");
        assert_eq!(entry.skills, names);
        assert_eq!(entry.files, 2);

        // A content change moves the version; an identical copy does not.
        std::fs::write(source.join("SKILL.md"), skill_md("pack-root-v2")).unwrap();
        let changed = install(&skills_dir, source.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            changed,
            vec!["pack-nested".to_string(), "pack-root-v2".to_string()]
        );
        let after = load_manifest(&skills_dir).packages["pack"].version.clone();
        assert_ne!(after, entry.version, "changed content, changed version");

        // Discovery sees the staged layout through the extra-dirs path.
        let found =
            crate::skills::discover(&root, std::slice::from_ref(&skills_dir)).expect("discover");
        // The second install overwrote the root skill's frontmatter name.
        assert!(found.contains_key("pack-root-v2"), "missing pack-root-v2");
        assert!(found.contains_key("pack-nested"), "missing pack-nested");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn installs_a_git_package_pinned_to_a_ref() {
        let root = scratch("git");
        let skills_dir = root.join("skills");
        let repo = root.join("remote");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("SKILL.md"), skill_md("from-git")).unwrap();
        git(&["init", "-q"], &repo).await;
        git(&["config", "user.email", "wrose@local"], &repo).await;
        git(&["config", "user.name", "wrose test"], &repo).await;
        git(&["add", "."], &repo).await;
        git(&["commit", "-q", "-m", "v1"], &repo).await;
        git(&["branch", "release"], &repo).await;
        let head = git(&["rev-parse", "--short=12", "HEAD"], &repo).await;

        let source = format!("file://{}#release", repo.display());
        let names = install(&skills_dir, &source).await.expect("install");
        assert_eq!(names, vec!["from-git".to_string()]);
        let manifest = load_manifest(&skills_dir);
        // The name comes from the URL's last segment — the repo dir here.
        let entry = &manifest.packages["remote"];
        assert_eq!(entry.version, head, "version is the pinned commit");
        assert_eq!(entry.source, source);
        assert!(skills_dir.join("remote/SKILL.md").exists());
        // The scratch clone must not linger.
        let stray: Vec<_> = std::env::temp_dir()
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("wrose-pkg-")
            })
            .collect();
        assert!(stray.is_empty(), "scratch clones left behind: {stray:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_package_without_a_parsable_skill_is_rejected() {
        let root = scratch("reject");
        let skills_dir = root.join("skills");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("README.md"), "no skills here").unwrap();
        let error = install(&skills_dir, empty.to_str().unwrap())
            .await
            .expect_err("must reject");
        assert!(error.to_string().contains("no SKILL.md"), "{error}");
        assert!(
            !skills_dir.join("empty").exists(),
            "partial copy left behind"
        );

        let broken = root.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(
            broken.join("SKILL.md"),
            "---\nname: [unterminated\n---\nbody",
        )
        .unwrap();
        let error = install(&skills_dir, broken.to_str().unwrap())
            .await
            .expect_err("must reject");
        assert!(format!("{error:#}").contains("frontmatter"), "{error}");
        assert!(!skills_dir.join("broken").exists());
        assert!(load_manifest(&skills_dir).packages.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn the_sandbox_stops_symlinks_oversize_files_and_deep_files() {
        let root = scratch("sandbox");
        let skills_dir = root.join("skills");
        let source = root.join("pack");
        std::fs::create_dir_all(source.join("a/b")).unwrap();
        std::fs::write(source.join("SKILL.md"), skill_md("pack")).unwrap();
        std::fs::write(source.join("a/SKILL.md"), skill_md("pack-deep")).unwrap();
        std::fs::write(source.join("a/b/SKILL.md"), skill_md("too-deep")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", source.join("link.md")).unwrap();

        let names = install(&skills_dir, source.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(names, vec!["pack".to_string(), "pack-deep".to_string()]);
        assert!(skills_dir.join("pack/SKILL.md").exists());
        assert!(skills_dir.join("pack/a/SKILL.md").exists());
        assert!(
            !skills_dir.join("pack/a/b").exists(),
            "files past the depth cap must not be copied"
        );
        assert!(!skills_dir.join("pack/link.md").exists(), "symlink copied");

        // An oversize file rejects the whole package.
        let big = root.join("big");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(big.join("SKILL.md"), skill_md("big")).unwrap();
        std::fs::write(
            big.join("blob.bin"),
            vec![0u8; (MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        let error = install(&skills_dir, big.to_str().unwrap())
            .await
            .expect_err("must reject");
        assert!(error.to_string().contains("cap"), "{error}");
        assert!(!skills_dir.join("big").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn reinstall_replaces_the_copy_and_uninstall_removes_it() {
        let root = scratch("lifecycle");
        let skills_dir = root.join("skills");
        let source = root.join("pack");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), skill_md("pack")).unwrap();
        install(&skills_dir, source.to_str().unwrap())
            .await
            .unwrap();
        // A stray file inside the installed copy must not survive a reinstall.
        std::fs::write(skills_dir.join("pack/stray.txt"), "junk").unwrap();

        install(&skills_dir, source.to_str().unwrap())
            .await
            .unwrap();
        assert!(!skills_dir.join("pack/stray.txt").exists(), "replaced");
        assert!(load_manifest(&skills_dir).packages.contains_key("pack"));

        let removed = uninstall(&skills_dir, "pack").expect("uninstall");
        assert_eq!(removed.skills, vec!["pack".to_string()]);
        assert!(!skills_dir.join("pack").exists());
        assert!(load_manifest(&skills_dir).packages.is_empty());
        uninstall(&skills_dir, "pack").expect_err("already gone");
        uninstall(&skills_dir, "../escape").expect_err("traversal");
        let _ = std::fs::remove_dir_all(&root);
    }
}
