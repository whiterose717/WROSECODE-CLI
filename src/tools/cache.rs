use anyhow::{Context, Result};
use std::hash::{Hash, Hasher};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub fn redis_address(url: &str) -> Option<String> {
    let value = url.strip_prefix("redis://")?.trim_end_matches('/');
    if value.contains('@') || value.contains('/') {
        return None;
    }
    Some(if value.contains(':') {
        value.to_string()
    } else {
        format!("{value}:6379")
    })
}

pub async fn redis_get(address: &str, key: &str) -> Result<Option<String>> {
    let key = cache_key(key);
    let response = command(address, &["GET", &key]).await?;
    if response.starts_with(b"$-1") {
        return Ok(None);
    }
    let split = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .context("invalid Redis response")?;
    if response.first() != Some(&b'$') {
        anyhow::bail!("Redis GET failed");
    }
    let body = &response[split + 2..];
    Ok(Some(
        String::from_utf8_lossy(body.strip_suffix(b"\r\n").unwrap_or(body)).into_owned(),
    ))
}

pub async fn redis_set(address: &str, key: &str, value: &str) -> Result<()> {
    let key = cache_key(key);
    let response = command(address, &["SET", &key, value, "EX", "86400"]).await?;
    if !response.starts_with(b"+OK") {
        anyhow::bail!("Redis SET failed");
    }
    Ok(())
}

async fn command(address: &str, parts: &[&str]) -> Result<Vec<u8>> {
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .context("Redis connection timed out")??;
    let mut request = format!("*{}\r\n", parts.len());
    for part in parts {
        request.push_str(&format!("${}\r\n{}\r\n", part.len(), part));
    }
    stream.write_all(request.as_bytes()).await?;
    let mut buffer = vec![0_u8; 65_536];
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        stream.read(&mut buffer),
    )
    .await
    .context("Redis response timed out")??;
    buffer.truncate(read);
    Ok(buffer)
}

fn cache_key(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("wrose:{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_redis_urls() {
        assert_eq!(
            redis_address("redis://localhost/"),
            Some("localhost:6379".into())
        );
        assert!(redis_address("https://localhost").is_none());
    }
}
