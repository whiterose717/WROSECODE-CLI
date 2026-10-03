use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn diff(root: &Path) -> Result<String> {
    let mut text = if git(root, &["rev-parse", "--verify", "HEAD"]).is_ok() {
        git(root, &["diff", "--no-ext-diff", "HEAD", "--"])?
    } else {
        format!(
            "{}{}",
            git(root, &["diff", "--no-ext-diff", "--cached", "--"])?,
            git(root, &["diff", "--no-ext-diff", "--"])?
        )
    };
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for name in untracked.split('\0').filter(|name| !name.is_empty()) {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        let output = Command::new("git")
            .args(["diff", "--no-index", "--", "/dev/null", name])
            .current_dir(root)
            .output()?;
        if output.status.code() == Some(1) || output.status.success() {
            text.push_str(&String::from_utf8_lossy(&output.stdout));
        }
    }
    Ok(text)
}

pub fn changed_files(root: &Path) -> Result<Vec<String>> {
    Ok(git(root, &["status", "--porcelain=v1", "-z"])?
        .split('\0')
        .filter_map(|line| line.get(3..))
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn suggested_commit(root: &Path) -> Result<String> {
    let files = changed_files(root)?;
    if files.is_empty() {
        bail!("nothing to commit");
    }
    let names: Vec<_> = files
        .iter()
        .take(3)
        .map(|name| {
            Path::new(name)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    Ok(format!("Update {}", names.join(", ")))
}

pub fn commit(root: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        bail!("commit message is empty");
    }
    git(root, &["add", "--all", "--"])?;
    git(root, &["commit", "-m", message.trim()])
}

pub fn init(root: &Path) -> Result<String> {
    let dir = root.join(".wrosecode");
    std::fs::create_dir_all(&dir)?;
    let checks = if root.join("Cargo.toml").exists() {
        "cargo test && cargo clippy"
    } else if root.join("package.json").exists() {
        "npm test && npm run lint"
    } else if root.join("pyproject.toml").exists() {
        "pytest && ruff check ."
    } else if root.join("go.mod").exists() {
        "go test ./... && go vet ./..."
    } else {
        "Add this project's test and lint commands"
    };
    let config = dir.join("project.toml");
    if !config.exists() {
        std::fs::write(&config, format!("check_command = {:?}\n", checks))?;
    }
    let guide = root.join("WROSECODE.md");
    if !guide.exists() {
        let name = root.file_name().unwrap_or_default().to_string_lossy();
        std::fs::write(&guide, format!("# {name}\n\nProject root: {}\n\n## Validation\n\nRun `{checks}` after changes.\n\n## Repository notes\n\nDescribe architecture and conventions here.\n", root.display()))?;
    }
    Ok(format!(
        "Project config: {}\nGuidance: {}\nValidation: {checks}",
        config.display(),
        guide.display()
    ))
}

fn remote(root: &Path) -> Result<(String, String, String)> {
    let url = git(root, &["remote", "get-url", "origin"])?
        .trim()
        .to_string();
    let without_git = url.trim_end_matches(".git");
    let (host, path) = if let Some(rest) = without_git.strip_prefix("git@") {
        rest.split_once(':').context("unsupported SSH remote")?
    } else if let Some(rest) = without_git.strip_prefix("https://") {
        rest.split_once('/').context("unsupported HTTPS remote")?
    } else {
        bail!("unsupported remote URL: {url}");
    };
    Ok((host.into(), path.into(), url))
}

pub async fn issues(root: &Path, client: &reqwest::Client) -> Result<Vec<String>> {
    let (host, path, _) = remote(root)?;
    let (url, token) = if host == "github.com" {
        (
            format!("https://api.github.com/repos/{path}/issues?state=open&per_page=30"),
            std::env::var("GITHUB_TOKEN").ok(),
        )
    } else if host.contains("gitlab") {
        let encoded = path.replace('/', "%2F");
        (
            format!("https://{host}/api/v4/projects/{encoded}/issues?state=opened&per_page=30"),
            std::env::var("GITLAB_TOKEN").ok(),
        )
    } else {
        bail!("issues support GitHub and GitLab remotes");
    };
    let mut request = client
        .get(url)
        .header("User-Agent", "wrosecode")
        .timeout(std::time::Duration::from_secs(15));
    if let Some(token) = token {
        request = if host == "github.com" {
            request.bearer_auth(token)
        } else {
            request.header("PRIVATE-TOKEN", token)
        };
    }
    let response = request.send().await?.error_for_status()?;
    let items: Vec<serde_json::Value> = response.json().await?;
    Ok(items
        .into_iter()
        .filter(|item| item.get("pull_request").is_none())
        .map(|item| {
            let number = item["number"]
                .as_u64()
                .or_else(|| item["iid"].as_u64())
                .unwrap_or_default();
            format!(
                "#{number} {} — {}",
                item["title"].as_str().unwrap_or("(untitled)"),
                item["html_url"]
                    .as_str()
                    .or_else(|| item["web_url"].as_str())
                    .unwrap_or("")
            )
        })
        .collect())
}

pub fn slop_candidates(
    root: &Path,
    created: &std::collections::HashSet<PathBuf>,
    referenced: &std::collections::HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let tracked: std::collections::HashSet<_> = git(root, &["ls-files", "-z"])?
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| root.join(s))
        .collect();
    Ok(created
        .iter()
        .filter(|path| {
            path.starts_with(root) && !tracked.contains(*path) && !referenced.contains(*path)
        })
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if path.is_dir() {
                return std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
            }
            path.is_file()
                && (name.ends_with(".bak")
                    || name.ends_with('~')
                    || name.starts_with("scratch")
                    || name.starts_with("tmp"))
        })
        .cloned()
        .collect())
}
