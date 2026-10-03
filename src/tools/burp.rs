use anyhow::Result;
use serde_json::json;
use std::path::Path;

pub fn import(root: &Path, path: &str) -> Result<String> {
    let resolved = super::fs::resolve(root, path)?;
    let raw = std::fs::read_to_string(resolved)?;
    let normalized = raw.replace("\r\n", "\n");
    let (head, body) = normalized.split_once("\n\n").unwrap_or((&normalized, ""));
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let headers: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| json!({"name":name.trim(),"value":value.trim()}))
        .collect();
    Ok(serde_json::to_string_pretty(&json!({
        "method": method,
        "target": target,
        "headers": headers,
        "body": body,
    }))?)
}

pub fn export(root: &Path, path: &str, request: &str) -> Result<String> {
    let resolved = super::fs::resolve(root, path)?;
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let normalized = request.replace("\r\n", "\n").replace('\n', "\r\n");
    std::fs::write(&resolved, normalized)?;
    Ok(format!("exported Burp request to {}", resolved.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_raw_request() {
        let root = std::env::temp_dir().join(format!("wrose-burp-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("request.txt"),
            "GET /flag HTTP/1.1\r\nHost: example.test\r\n\r\n",
        )
        .unwrap();
        let parsed = import(&root, "request.txt").unwrap();
        assert!(parsed.contains("example.test"));
        let _ = std::fs::remove_dir_all(root);
    }
}
