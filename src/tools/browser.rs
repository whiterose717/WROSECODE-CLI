use anyhow::{Context, Result};
use std::path::Path;

pub async fn capture(root: &Path, url: &str, output: &str, timeout_seconds: u64) -> Result<String> {
    if !matches!(url.split(':').next(), Some("http" | "https")) {
        anyhow::bail!("browser capture URL must use http or https");
    }
    let output = super::fs::resolve(root, output)?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut command = tokio::process::Command::new("npx");
    command
        .args([
            "--no-install",
            "playwright",
            "screenshot",
            "--wait-for-timeout",
            "1000",
        ])
        .arg(url)
        .arg(&output)
        .current_dir(root)
        .kill_on_drop(true);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_seconds),
        command.output(),
    )
    .await
    .context("Playwright capture timed out")??;
    if !result.status.success() {
        anyhow::bail!(
            "Playwright capture failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(format!("captured {}", output.display()))
}
