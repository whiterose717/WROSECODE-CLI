use anyhow::Result;
use std::path::Path;

pub fn destructive(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    [
        "rm -rf",
        "rm -fr",
        "mkfs",
        "dd if=",
        ":(){",
        "chmod -r 777 /",
        "> /dev/sd",
        ">/dev/sd",
        "> /dev/nvme",
        ">/dev/nvme",
    ]
    .iter()
    .any(|needle| c.contains(needle))
}

pub fn read_only(command: &str) -> bool {
    let trimmed = command.trim();
    let first = trimmed.split_whitespace().next().unwrap_or("");
    let simple = ![";", "&&", "||", "|", ">", "<", "`", "$(", "\n"]
        .iter()
        .any(|token| trimmed.contains(token));
    simple
        && matches!(
            first,
            "pwd"
                | "ls"
                | "cat"
                | "rg"
                | "grep"
                | "find"
                | "head"
                | "tail"
                | "wc"
                | "git"
                | "file"
                | "strings"
        )
        && !(first == "git"
            && !trimmed.starts_with("git status")
            && !trimmed.starts_with("git diff")
            && !trimmed.starts_with("git log")
            && !trimmed.starts_with("git show"))
}

pub async fn run(command: &str, cwd: &Path) -> Result<String> {
    run_with_timeout(command, cwd, 30).await
}

pub async fn run_with_timeout(command: &str, cwd: &Path, timeout_seconds: u64) -> Result<String> {
    let mut process = tokio::process::Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .kill_on_drop(true);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_seconds),
        process.output(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("command timed out after {timeout_seconds} seconds and was terminated")
    })??;
    Ok(format!(
        "exit={}\n{}{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_commands_always_match() {
        assert!(destructive("rm -rf /tmp/data"));
        assert!(destructive("dd if=/dev/zero of=/dev/sda"));
        assert!(!read_only("git status && rm -rf /tmp/data"));
    }
}
