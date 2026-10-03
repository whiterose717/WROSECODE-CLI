use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
pub struct CtfdCli {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
    List,
    Scoreboard,
    Submit {
        challenge_id: u64,
        flag: String,
    },
    Download {
        challenge_id: u64,
        #[arg(long, default_value = ".ctf/artifacts")]
        output: PathBuf,
    },
    /// Submit a flag and print a machine-readable verdict.
    ///
    /// Exit codes: 0 = VERIFIED, 2 = REJECTED, 1 = INCONCLUSIVE / transport error.
    /// Retries `--retries` times on HTTP 429 and transient network failures.
    Verify {
        challenge_id: u64,
        flag: String,
        #[arg(long, default_value_t = 3)]
        retries: u8,
        #[arg(long, default_value_t = 500)]
        backoff_ms: u64,
    },
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    fn from_env() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::new(),
            base: std::env::var("CTFD_URL").context("CTFD_URL is not set")?,
            token: std::env::var("CTFD_TOKEN").context("CTFD_TOKEN is not set")?,
        })
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}{}", self.base.trim_end_matches('/'), path))
            .header("Authorization", format!("Token {}", self.token))
            .send()
            .await?;
        if !response.status().is_success() {
            anyhow::bail!("CTFd returned {}", response.status());
        }
        Ok(response.json().await?)
    }

    /// POST one attempt and return the HTTP status plus parsed body. Errors here are
    /// transport failures; throttling surfaces as `429`.
    async fn attempt(&self, challenge_id: u64, flag: &str) -> Result<(u16, Value)> {
        let response = self
            .http
            .post(format!(
                "{}/api/v1/challenges/attempt",
                self.base.trim_end_matches('/')
            ))
            .header("Authorization", format!("Token {}", self.token))
            .json(&serde_json::json!({"challenge_id": challenge_id, "submission": flag}))
            .send()
            .await?;
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        Ok((status, body))
    }

    /// Submit with exponential backoff on 429 / transient transport errors so an
    /// auto-submit loop never hammers a rate-limited platform.
    async fn attempt_with_retry(
        &self,
        challenge_id: u64,
        flag: &str,
        retries: u8,
        backoff_ms: u64,
    ) -> Result<(u16, Value, u8)> {
        let mut attempt = 0_u8;
        loop {
            match self.attempt(challenge_id, flag).await {
                Ok((status, _body)) if status == 429 && attempt < retries => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        backoff_ms.saturating_mul(1 << attempt),
                    ))
                    .await;
                }
                Ok((status, body)) => return Ok((status, body, attempt)),
                Err(error) if attempt < retries && is_transient(&error) => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        backoff_ms.saturating_mul(1 << attempt),
                    ))
                    .await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn is_transient(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("timeout")
        || text.contains("timed out")
        || text.contains("connection")
        || text.contains("reset")
        || text.contains("429")
        || text.contains("temporarily")
}

/// Classify a CTFd attempt response. `None` means the platform gave no verdict.
pub fn verdict(status: u16, body: &Value) -> Option<&'static str> {
    if !(200..300).contains(&status) {
        return None;
    }
    let message = body["data"]["message"].as_str().unwrap_or_default();
    let state = body["data"]["status"].as_str().unwrap_or_default();
    let haystack = format!("{message} {state}").to_ascii_lowercase();
    // Rejections come first: "incorrect" contains "correct", so ordering is
    // what makes the classification correct at all.
    if haystack.contains("incorrect")
        || haystack.contains("not correct")
        || haystack.contains("wrong")
    {
        return Some("REJECTED");
    }
    if haystack.contains("correct") || haystack.contains("already solved") {
        return Some("VERIFIED");
    }
    None
}

pub async fn run(action: Action) -> Result<()> {
    let client = Client::from_env()?;
    match action {
        Action::List => {
            let data = client.get("/api/v1/challenges").await?;
            for challenge in data["data"].as_array().into_iter().flatten() {
                println!(
                    "{}\t{}\t{}\t{}",
                    challenge["id"].as_u64().unwrap_or_default(),
                    challenge["category"].as_str().unwrap_or("unknown"),
                    challenge["name"].as_str().unwrap_or("unnamed"),
                    challenge["value"].as_u64().unwrap_or_default()
                );
            }
        }
        Action::Scoreboard => {
            let data = client.get("/api/v1/scoreboard").await?;
            println!("{}", serde_json::to_string_pretty(&data["data"])?);
        }
        Action::Submit { challenge_id, flag } => {
            let (status, body) = client.attempt(challenge_id, &flag).await?;
            if status == 429 {
                anyhow::bail!("CTFd rate limited the submission (HTTP 429)");
            }
            if !(200..300).contains(&status) {
                anyhow::bail!("CTFd returned HTTP {status}");
            }
            println!(
                "{}",
                body["data"]["message"]
                    .as_str()
                    .unwrap_or("unknown verdict")
            );
        }
        Action::Verify {
            challenge_id,
            flag,
            retries,
            backoff_ms,
        } => {
            let (status, body, used) = client
                .attempt_with_retry(challenge_id, &flag, retries, backoff_ms)
                .await?;
            let state = verdict(status, &body).unwrap_or("INCONCLUSIVE");
            println!(
                "{}\tchallenge={}\tretries={}\tmessage={}",
                state,
                challenge_id,
                used,
                body["data"]["message"].as_str().unwrap_or("no message")
            );
            match state {
                "VERIFIED" => {}
                "REJECTED" => std::process::exit(2),
                _ => std::process::exit(1),
            }
        }
        Action::Download {
            challenge_id,
            output,
        } => {
            std::fs::create_dir_all(&output)?;
            let data = client
                .get(&format!("/api/v1/challenges/{challenge_id}"))
                .await?;
            for raw in data["data"]["files"].as_array().into_iter().flatten() {
                let Some(file) = raw.as_str() else { continue };
                let url = if file.starts_with("http") {
                    file.to_string()
                } else {
                    format!("{}{}", client.base.trim_end_matches('/'), file)
                };
                let name = file
                    .split('?')
                    .next()
                    .and_then(|path| path.rsplit('/').next())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("challenge.bin");
                let bytes = client
                    .http
                    .get(url)
                    .header("Authorization", format!("Token {}", client.token))
                    .send()
                    .await?
                    .bytes()
                    .await?;
                let path = output.join(name);
                std::fs::write(&path, bytes)?;
                println!("{}", path.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verdicts_are_classified_without_substring_confusion() {
        assert_eq!(
            verdict(200, &json!({"data": {"status": "correct"}})),
            Some("VERIFIED")
        );
        assert_eq!(
            verdict(200, &json!({"data": {"message": "Correct flag"}})),
            Some("VERIFIED")
        );
        assert_eq!(
            verdict(200, &json!({"data": {"message": "Already solved"}})),
            Some("VERIFIED")
        );
        assert_eq!(
            verdict(200, &json!({"data": {"status": "incorrect"}})),
            Some("REJECTED")
        );
        assert_eq!(
            verdict(200, &json!({"data": {"message": "Wrong flag"}})),
            Some("REJECTED")
        );
        assert_eq!(
            verdict(200, &json!({"data": {"message": "Not correct"}})),
            Some("REJECTED")
        );
        assert_eq!(verdict(200, &json!({"data": {}})), None);
        assert_eq!(verdict(429, &json!({"data": {"status": "correct"}})), None);
        assert_eq!(
            verdict(500, &json!({"data": {"status": "incorrect"}})),
            None
        );
    }

    #[test]
    fn transient_transport_errors_are_detected() {
        assert!(is_transient(&anyhow::anyhow!(
            "error sending request: connection reset by peer"
        )));
        assert!(is_transient(&anyhow::anyhow!("operation timed out")));
        assert!(!is_transient(&anyhow::anyhow!("invalid api key")));
    }
}
