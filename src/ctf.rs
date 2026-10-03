use anyhow::Result;
use base64::Engine;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq)]
pub struct FlagHit {
    pub flag: String,
    pub source: String,
    pub transformation: String,
    pub confidence: f32,
}

#[derive(Clone)]
pub struct CtfEngine {
    root: PathBuf,
    seen: Arc<Mutex<HashSet<String>>>,
    patterns: Arc<Vec<Regex>>,
    last_submit: Arc<Mutex<Option<Instant>>>,
    auto_copy: bool,
    auto_submit: bool,
    alert_bell: bool,
    pub category: String,
}

#[derive(Default, Deserialize)]
struct DetectorFile {
    #[serde(default)]
    ctf: DetectorConfig,
}

#[derive(Default, Deserialize)]
struct DetectorConfig {
    #[serde(default)]
    flag_patterns: Vec<String>,
    #[serde(default)]
    auto_copy: Option<bool>,
    #[serde(default)]
    auto_submit: Option<bool>,
}

impl CtfEngine {
    pub fn new(root: &Path) -> Self {
        let configured = std::fs::read_to_string(root.join("config.toml"))
            .ok()
            .and_then(|source| toml::from_str::<DetectorFile>(&source).ok())
            .map(|file| file.ctf)
            .unwrap_or_default();
        let auto_copy = configured.auto_copy.unwrap_or(true);
        let auto_submit = configured.auto_submit.unwrap_or(false);
        let configured = configured.flag_patterns;
        let raw_patterns = if configured.is_empty() {
            vec![r"(?:flag|CTF|picoCTF|HTB)\{[^}\r\n]{1,512}\}".to_string()]
        } else {
            configured
        };
        let seen = std::fs::read_to_string(root.join(".ctf/flags.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                fields
                    .get(3)
                    .or_else(|| fields.get(2))
                    .map(|value| (*value).to_string())
            })
            .collect();
        Self {
            root: root.to_path_buf(),
            seen: Arc::new(Mutex::new(seen)),
            patterns: Arc::new(
                raw_patterns
                    .iter()
                    .filter_map(|pattern| Regex::new(pattern).ok())
                    .collect(),
            ),
            last_submit: Arc::new(Mutex::new(None)),
            auto_copy,
            auto_submit,
            alert_bell: true,
            category: categorize(root, ""),
        }
    }

    /// Override `[ui] alert_bell` from the resolved runtime configuration.
    #[must_use]
    pub fn with_alert_bell(mut self, alert_bell: bool) -> Self {
        self.alert_bell = alert_bell;
        self
    }

    pub fn scan(&self, source: &str, text: &str) -> Result<Vec<FlagHit>> {
        let mut hits = Vec::new();
        let mut seen = self
            .seen
            .lock()
            .map_err(|_| anyhow::anyhow!("flag detector lock poisoned"))?;
        let mut variants = vec![("plain".to_string(), text.to_string(), 1.0_f32)];
        variants.push(("rot13".into(), rot13(text), 0.85));
        for token in text.split(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '`')) {
            if !(8..=4096).contains(&token.len()) {
                continue;
            }
            if let Some(decoded) = decode_hex(token) {
                variants.push(("hex".into(), decoded, 0.95));
            }
            if shannon_entropy(token.as_bytes()) < 3.0 {
                continue;
            }
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(token) {
                if let Ok(decoded) = String::from_utf8(bytes) {
                    variants.push(("base64".into(), decoded, 0.95));
                }
            }
        }
        for (transformation, candidate, confidence) in variants {
            for pattern in self.patterns.iter() {
                for found in pattern.find_iter(&candidate) {
                    let flag = found.as_str().to_string();
                    if !seen.insert(flag.clone()) {
                        continue;
                    }
                    let hit = FlagHit {
                        flag: flag.clone(),
                        source: source.to_string(),
                        transformation: transformation.clone(),
                        confidence,
                    };
                    self.log_flag(&hit)?;
                    if self.auto_copy {
                        let _ = copy_to_clipboard(&flag);
                    }
                    notify_flag(&flag, self.alert_bell);
                    hits.push(hit);
                }
            }
        }
        Ok(hits)
    }

    pub fn scan_project(&self) -> Vec<FlagHit> {
        let mut hits = Vec::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "node_modules" | ".ctf")
                )
            })
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .take(2_000)
        {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > 2 * 1024 * 1024 {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let source = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .display()
                .to_string();
            if let Ok(mut found) = self.scan(&format!("file:{source}"), &text) {
                hits.append(&mut found);
            }
        }
        hits
    }

    fn log_flag(&self, hit: &FlagHit) -> Result<()> {
        let dir = self.root.join(".ctf");
        std::fs::create_dir_all(&dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("flags.log"))?;
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{:.2}",
            unix_timestamp(),
            hit.source,
            hit.transformation,
            hit.flag,
            hit.confidence
        )?;
        Ok(())
    }

    pub async fn auto_submit(
        &self,
        client: &reqwest::Client,
        flag: &str,
    ) -> Result<Option<String>> {
        let enabled = std::env::var("WROSECODE_CTFD_AUTO_SUBMIT")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(self.auto_submit);
        if !enabled {
            return Ok(None);
        }
        let base = std::env::var("CTFD_URL")?;
        let token = std::env::var("CTFD_TOKEN")?;
        let challenge_id: u64 = std::env::var("CTFD_CHALLENGE_ID")?.parse()?;
        let delay = self
            .last_submit
            .lock()
            .ok()
            .and_then(|last| *last)
            .map(|last| Duration::from_secs(2).saturating_sub(last.elapsed()))
            .unwrap_or_default();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let mut response = None;
        for attempt in 0..3 {
            match client
                .post(format!(
                    "{}/api/v1/challenges/attempt",
                    base.trim_end_matches('/')
                ))
                .header("Authorization", format!("Token {token}"))
                .json(&serde_json::json!({"challenge_id": challenge_id, "submission": flag}))
                .send()
                .await
            {
                Ok(result) if !result.status().is_server_error() => {
                    response = Some(result);
                    break;
                }
                Ok(result) => response = Some(result),
                Err(error) if attempt == 2 => return Err(error.into()),
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
        }
        if let Ok(mut last) = self.last_submit.lock() {
            *last = Some(Instant::now());
        }
        let response = response.ok_or_else(|| anyhow::anyhow!("CTFd submission failed"))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let message = body["data"]["message"]
            .as_str()
            .or_else(|| body["message"].as_str())
            .unwrap_or(if status.is_success() {
                "submitted"
            } else {
                "submission failed"
            });
        Ok(Some(format!("CTFd {status}: {message}")))
    }

    pub fn history(&self, query: &str) -> Vec<String> {
        std::fs::read_to_string(self.root.join(".ctf/flags.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                query.is_empty()
                    || line
                        .to_ascii_lowercase()
                        .contains(&query.to_ascii_lowercase())
            })
            .map(str::to_string)
            .collect()
    }
}

pub fn categorize(root: &Path, prompt: &str) -> String {
    let lower = prompt.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.contains(" url ") {
        return "web".into();
    }
    for (category, words) in [
        ("pwn", &["buffer overflow", "rop", "heap", "segfault"][..]),
        ("crypto", &["cipher", "rsa", "aes", "nonce", "modulus"][..]),
        (
            "rev",
            &["reverse engineering", "decompile", "disassemble"][..],
        ),
        (
            "forensics",
            &["pcap", "memory dump", "disk image", "steganography"][..],
        ),
        ("web", &["http/1.", "server:", "set-cookie:", "burp"][..]),
    ] {
        if words.iter().any(|word| lower.contains(word)) {
            return category.into();
        }
    }
    let mut category = "misc";
    if let Ok(entries) = std::fs::read_dir(root) {
        for path in entries.flatten().map(|entry| entry.path()) {
            let mut magic = [0_u8; 4];
            if std::fs::File::open(&path)
                .and_then(|mut file| file.read_exact(&mut magic))
                .is_ok()
                && &magic == b"\x7fELF"
            {
                return "pwn".into();
            }
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut sample_bytes = Vec::new();
            if let Ok(file) = std::fs::File::open(&path) {
                let _ = file.take(16_384).read_to_end(&mut sample_bytes);
            }
            let sample = String::from_utf8_lossy(&sample_bytes).to_ascii_lowercase();
            category = match extension.as_str() {
                "pcap" | "pcapng" | "raw" | "mem" | "jpg" | "jpeg" | "png" | "wav" => "forensics",
                "so" | "elf" | "exe" => "pwn",
                "py" if sample.contains("crypto")
                    || sample.contains("rsa")
                    || sample.contains("cipher") =>
                {
                    "crypto"
                }
                "wasm" | "class" | "dex" => "rev",
                _ => continue,
            };
            break;
        }
    }
    category.into()
}

pub fn log_error(root: &Path, context: &str, error: &str) {
    let dir = root.join(".ctf");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("errors.log"))
    {
        let _ = writeln!(
            file,
            "{}\t{}\t{}",
            unix_timestamp(),
            context,
            error.replace('\n', " ")
        );
    }
}

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    for (program, args) in [
        ("wl-copy", Vec::<&str>::new()),
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
    ] {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        if child.wait()?.success() {
            return Ok(());
        }
    }
    anyhow::bail!("no clipboard helper found")
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn decode_hex(value: &str) -> Option<String> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

fn rot13(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            'a'..='z' => (b'a' + (character as u8 - b'a' + 13) % 26) as char,
            'A'..='Z' => (b'A' + (character as u8 - b'A' + 13) % 26) as char,
            _ => character,
        })
        .collect()
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0_u32; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let probability = *count as f64 / bytes.len() as f64;
            -probability * probability.log2()
        })
        .sum()
}

fn notify_flag(flag: &str, alert_bell: bool) {
    // The BEL byte reaches the terminal even when no notification daemon exists.
    if alert_bell {
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\x07");
        let _ = stdout.flush();
    }
    let _ = Command::new("notify-send")
        .arg("WROSECODE flag found")
        .arg(flag)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let _ = Command::new("canberra-gtk-play")
        .args(["--id", "complete"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_deduplicates_flags() {
        let root = std::env::temp_dir().join(format!("wrose-ctf-{}", std::process::id()));
        let engine = CtfEngine::new(&root);
        let first = format!("noise picoCTF{} flag{}", "{proof_123}", "{second}");
        let hits = engine.scan("test", &first).unwrap();
        assert_eq!(hits.len(), 2);
        let repeated = format!("picoCTF{}", "{proof_123}");
        assert!(engine.scan("again", &repeated).unwrap().is_empty());
        let log = std::fs::read_to_string(root.join(".ctf/flags.log")).unwrap();
        assert!(log.contains(&repeated));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn decodes_hex_and_rot13_before_matching() {
        let root = std::env::temp_dir().join(format!("wrose-decode-{}", std::process::id()));
        let engine = CtfEngine::new(&root);
        let encoded = "666c61677b6865785f77696e7d";
        assert!(engine
            .scan("hex", encoded)
            .unwrap()
            .iter()
            .any(|hit| hit.flag == "flag{hex_win}"));
        assert!(engine
            .scan("rot", "synt{ebg13_jva}")
            .unwrap()
            .iter()
            .any(|hit| hit.flag == "flag{rot13_win}"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corpus_detection_accuracy_is_complete() {
        let root = std::env::temp_dir().join(format!("wrose-corpus-{}", std::process::id()));
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("../tests/flag_corpus.json")).unwrap();
        let mut correct = 0;
        for (index, item) in corpus.as_array().unwrap().iter().enumerate() {
            let engine = CtfEngine::new(&root.join(index.to_string()));
            let detected = !engine
                .scan("corpus", item["input"].as_str().unwrap())
                .unwrap()
                .is_empty();
            correct += usize::from(detected == item["expected"].as_bool().unwrap());
        }
        assert_eq!(correct, corpus.as_array().unwrap().len());
        let _ = std::fs::remove_dir_all(root);
    }
}
