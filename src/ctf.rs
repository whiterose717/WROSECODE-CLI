use anyhow::Result;
use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlagHit {
    pub flag: String,
    pub source: String,
    pub transformation: String,
    pub confidence: f32,
}

/// Accuracy gate for raw flag-pattern matches. Placeholders, markup soup,
/// test-data repeats, and truncation artifacts never become candidates —
/// for the TUI panel, the `.ctf/flags.log` history, clipboard, and the
/// autopilot's verification budget alike.
///
/// `strict` is true for the built-in patterns; custom `--flag-format`
/// patterns keep only the placeholder/empty rejection so exotic real
/// formats still match.
fn valid_flag(matched: &str, strict: bool) -> bool {
    let body = match (matched.find('{'), matched.rfind('}')) {
        (Some(open), Some(close)) if close > open => matched[open + 1..close].trim(),
        _ if !strict => matched.trim(),
        _ => return false,
    };
    if body.is_empty() {
        return false;
    }
    // Placeholder and redaction bodies, punctuation-insensitive:
    // `HTB{REDACTED}`, `flag{your-flag-here}`, `CTF{EXAMPLE}`…
    let flat: String = body
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    const PLACEHOLDERS: [&str; 14] = [
        "REDACTED",
        "EXAMPLE",
        "SAMPLE",
        "PLACEHOLDER",
        "YOURFLAGHERE",
        "FLAGHERE",
        "TESTFLAG",
        "TODO",
        "CHANGEME",
        "NULL",
        "NONE",
        "EMPTY",
        "UNKNOWN",
        "FAKE",
    ];
    if PLACEHOLDERS.contains(&flat.as_str()) {
        return false;
    }
    // Uniform bodies (`XXXX`, `aaaa`) and long repeat runs
    // (`t3st_fl4g_aaaaaaa…` test data): real flags are never shaped so.
    let chars: Vec<char> = body.chars().collect();
    if chars.len() >= 4 && chars.iter().all(|&ch| ch == chars[0]) {
        return false;
    }
    let mut run = 1;
    for pair in chars.windows(2) {
        if pair[0] == pair[1] {
            run += 1;
            if run >= 6 {
                return false;
            }
        } else {
            run = 1;
        }
    }
    if !strict {
        return true;
    }
    // Strict mode: real flag bodies carry no markup, whitespace, quotes, or
    // truncation artifacts (`HTB{&quot;…`, `HTB{abc…}`, trailing dots).
    if body
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '&'))
    {
        return false;
    }
    if body.contains("..") || body.contains('…') || body.ends_with('.') {
        return false;
    }
    true
}

#[derive(Clone)]
pub struct CtfEngine {
    root: PathBuf,
    seen: Arc<Mutex<HashSet<String>>>,
    // Evidence observed in this run, separate from persistent flag history.
    current_hits: Arc<Mutex<Vec<FlagHit>>>,
    stream_buffers: Arc<Mutex<HashMap<String, String>>>,
    patterns: Arc<Vec<Regex>>,
    pattern_signature: Arc<String>,
    artifact_cache: Arc<Mutex<Option<ArtifactCache>>>,
    last_submit: Arc<Mutex<Option<Instant>>>,
    auto_copy: bool,
    auto_submit: bool,
    alert_bell: bool,
    /// True while the engine runs the built-in patterns. Custom
    /// `--flag-format` patterns keep their full expressive power: only the
    /// placeholder/empty rejection applies to them, never the strict
    /// charset rules below.
    strict: bool,
    pub category: String,
    /// Once locked (CTF autopilot), `turn()` stops overwriting the category
    /// so a stuck-rotation hypothesis survives the next prompt.
    category_locked: bool,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ArtifactCache {
    version: u8,
    pattern_signature: String,
    files: HashMap<String, CachedArtifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedArtifact {
    size: u64,
    modified_ns: u128,
    hits: Vec<FlagHit>,
}

impl CtfEngine {
    /// The directory this detector is watching — the verifier needs it to
    /// replay `file:` hits against their source artifact.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn new(root: &Path) -> Self {
        let configured = std::fs::read_to_string(root.join("config.toml"))
            .ok()
            .and_then(|source| toml::from_str::<DetectorFile>(&source).ok())
            .map(|file| file.ctf)
            .unwrap_or_default();
        let auto_copy = configured.auto_copy.unwrap_or(true);
        let auto_submit = configured.auto_submit.unwrap_or(false);
        let configured = configured.flag_patterns;
        let strict = configured.is_empty();
        let raw_patterns = if strict {
            vec![r"(?:flag|CTF|picoCTF|HTB)\{[^}\r\n]{1,512}\}".to_string()]
        } else {
            configured
        };
        let pattern_signature = raw_patterns.join("\n");
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
            current_hits: Arc::new(Mutex::new(Vec::new())),
            stream_buffers: Arc::new(Mutex::new(HashMap::new())),
            patterns: Arc::new(
                raw_patterns
                    .iter()
                    .filter_map(|pattern| Regex::new(pattern).ok())
                    .collect(),
            ),
            pattern_signature: Arc::new(pattern_signature),
            strict,
            artifact_cache: Arc::new(Mutex::new(None)),
            last_submit: Arc::new(Mutex::new(None)),
            auto_copy,
            auto_submit,
            alert_bell: true,
            category: categorize(root, ""),
            category_locked: false,
        }
    }

    /// Start a challenge without accepting stale tool output from prior runs.
    /// Detach shared buffers so cancelled workers cannot contaminate this run.
    pub fn begin_run(&mut self) {
        self.current_hits = Arc::new(Mutex::new(Vec::new()));
        self.stream_buffers = Arc::new(Mutex::new(HashMap::new()));
    }

    pub fn current_hits(&self) -> Vec<FlagHit> {
        self.current_hits
            .lock()
            .map(|hits| hits.clone())
            .unwrap_or_default()
    }

    /// Freeze the current category hypothesis against `turn()`'s re-guess.
    pub fn lock_category(&mut self) {
        self.category_locked = true;
    }

    pub fn unlock_category(&mut self) {
        self.category_locked = false;
    }

    pub fn category_locked(&self) -> bool {
        self.category_locked
    }

    /// Override `[ui] alert_bell` from the resolved runtime configuration.
    #[must_use]
    pub fn with_alert_bell(mut self, alert_bell: bool) -> Self {
        self.alert_bell = alert_bell;
        self
    }

    /// Replace the configured flag patterns with one user-supplied regex
    /// (`wrosecode ctf --flag-format '…'`).
    pub fn with_flag_format(mut self, pattern: &str) -> Result<Self> {
        let compiled = Regex::new(pattern)
            .map_err(|error| anyhow::anyhow!("invalid --flag-format regex: {error}"))?;
        self.patterns = Arc::new(vec![compiled]);
        self.pattern_signature = Arc::new(pattern.to_owned());
        self.strict = false;
        self.artifact_cache = Arc::new(Mutex::new(None));
        Ok(self)
    }

    /// Every match in `text`, without deduplicating, logging, copying, or
    /// notifying. The autopilot uses this to *re-derive* a candidate flag
    /// from its recorded source — the replay that separates a verified flag
    /// from a model claim.
    ///
    /// Raw regex hits pass through [`valid_flag`] first, so markup soup
    /// (`HTB{&quot;…`), placeholders (`HTB{REDACTED}`), test-data repeats,
    /// and truncated `…` artifacts never become candidates.
    pub fn detect(&self, source: &str, text: &str) -> Vec<FlagHit> {
        let mut hits = Vec::new();
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
                    if !valid_flag(&flag, self.strict) {
                        continue;
                    }
                    hits.push(FlagHit {
                        flag,
                        source: source.to_string(),
                        transformation: transformation.clone(),
                        confidence,
                    });
                }
            }
        }
        hits
    }

    pub fn scan(&self, source: &str, text: &str) -> Result<Vec<FlagHit>> {
        self.record_hits(self.detect(source, text))
    }

    fn record_hits(&self, detected: Vec<FlagHit>) -> Result<Vec<FlagHit>> {
        // Record provenance before notification deduplication. A model may
        // mention a candidate before a real tool independently derives it.
        {
            let mut current = self
                .current_hits
                .lock()
                .map_err(|_| anyhow::anyhow!("flag evidence lock poisoned"))?;
            for hit in &detected {
                if !current.iter().any(|old| {
                    old.flag == hit.flag
                        && old.source == hit.source
                        && old.transformation == hit.transformation
                }) {
                    current.push(hit.clone());
                }
            }
        }
        let mut seen = self
            .seen
            .lock()
            .map_err(|_| anyhow::anyhow!("flag detector lock poisoned"))?;
        let mut hits = Vec::new();
        for hit in detected {
            if !seen.insert(hit.flag.clone()) {
                continue;
            }
            self.log_flag(&hit)?;
            if self.auto_copy {
                let _ = copy_to_clipboard(&hit.flag);
            }
            hits.push(hit);
        }
        Ok(hits)
    }

    /// Scan incrementally delivered output without losing a flag split across
    /// chunks. The rolling buffer is intentionally bounded because providers
    /// can stream indefinitely; `scan` still owns deduplication/persistence.
    pub fn scan_stream(&self, source: &str, chunk: &str) -> Result<Vec<FlagHit>> {
        let buffer = {
            let mut buffers = self
                .stream_buffers
                .lock()
                .map_err(|_| anyhow::anyhow!("flag stream buffer lock poisoned"))?;
            let value = buffers.entry(source.to_string()).or_default();
            value.push_str(chunk);
            // Wide enough that a flag split across a large streamed
            // artifact still matches; bounded against endless streams.
            if value.chars().count() > 16384 {
                *value = value.chars().skip(value.chars().count() - 16384).collect();
            }
            value.clone()
        };
        self.scan(source, &buffer)
    }

    #[cfg(test)]
    pub fn scan_project(&self) -> Vec<FlagHit> {
        let cache_path = self.root.join(".ctf/artifact-index.json");
        let cache = self.load_artifact_cache(&cache_path);
        let previous_files = cache.files;
        let mut current_files = HashMap::new();
        let mut detected = Vec::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                // Skip our own scratch files: `ctf-notes.md` carries the
                // `flag{…}` format placeholder and `writeups/` repeats flags
                // we already recorded — scanning either would "verify" a
                // flag from the autopilot's own notes.
                !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "node_modules" | ".ctf" | "ctf-notes.md" | "writeups")
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
            let source = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .display()
                .to_string();
            let modified_ns = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            let source_key = format!("file:{source}");

            if let Some(cached) = previous_files.get(&source) {
                if cached.size == metadata.len() && cached.modified_ns == modified_ns {
                    detected.extend(cached.hits.iter().cloned());
                    current_files.insert(source, cached.clone());
                    continue;
                }
            }

            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            // Lossy text view: binary artifacts (pcaps, images, dumps) still
            // carry ASCII flag bodies, and skipping them made forensics
            // challenges unverifiable through no fault of the evidence.
            let text = String::from_utf8_lossy(&bytes);
            let hits = self.detect(&source_key, &text);
            detected.extend(hits.iter().cloned());
            current_files.insert(
                source,
                CachedArtifact {
                    size: metadata.len(),
                    modified_ns,
                    hits,
                },
            );
        }
        let cache = ArtifactCache {
            version: 1,
            pattern_signature: (*self.pattern_signature).clone(),
            files: current_files,
        };
        if let Ok(mut guard) = self.artifact_cache.lock() {
            *guard = Some(cache.clone());
        }
        self.save_artifact_cache(&cache_path, &cache);
        self.record_hits(detected).unwrap_or_default()
    }

    #[cfg(test)]
    fn load_artifact_cache(&self, path: &Path) -> ArtifactCache {
        if let Ok(guard) = self.artifact_cache.lock() {
            if let Some(cache) = guard.as_ref() {
                return cache.clone();
            }
        }
        let cache = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ArtifactCache>(&bytes).ok())
            .filter(|cache| {
                cache.version == 1 && cache.pattern_signature == *self.pattern_signature
            })
            .unwrap_or_else(|| ArtifactCache {
                version: 1,
                pattern_signature: (*self.pattern_signature).clone(),
                files: HashMap::new(),
            });
        if let Ok(mut guard) = self.artifact_cache.lock() {
            *guard = Some(cache.clone());
        }
        cache
    }

    #[cfg(test)]
    fn save_artifact_cache(&self, path: &Path, cache: &ArtifactCache) {
        let Ok(bytes) = serde_json::to_vec(cache) else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let temporary = parent.join(format!("artifact-index.{}.tmp", std::process::id()));
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(temporary, path);
        }
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
            // Each attempt carries its own bound: submissions share the
            // unbounded streaming client, and a hung challenge server must
            // surface as an error, not a stuck turn.
            let attempt_future = client
                .post(format!(
                    "{}/api/v1/challenges/attempt",
                    base.trim_end_matches('/')
                ))
                .header("Authorization", format!("Token {token}"))
                .json(&serde_json::json!({"challenge_id": challenge_id, "submission": flag}))
                .send();
            match tokio::time::timeout(Duration::from_secs(30), attempt_future).await {
                Ok(Ok(result)) if !result.status().is_server_error() => {
                    response = Some(result);
                    break;
                }
                Ok(Ok(result)) => response = Some(result),
                Ok(Err(error)) if attempt == 2 => return Err(error.into()),
                Ok(Err(_)) => {}
                Err(_) if attempt == 2 => {
                    return Err(anyhow::anyhow!("CTFd submission timed out"));
                }
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

    /// Every flag ever recorded for this challenge, parsed back out of
    /// `.ctf/flags.log` as [`FlagHit`]s so the autopilot can verify them
    /// against their sources.
    pub fn recorded_hits(&self) -> Vec<FlagHit> {
        std::fs::read_to_string(self.root.join(".ctf/flags.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split('\t').collect();
                let [_timestamp, source, transformation, flag, confidence] = fields[..] else {
                    return None;
                };
                Some(FlagHit {
                    flag: flag.to_string(),
                    source: source.to_string(),
                    transformation: transformation.to_string(),
                    confidence: confidence.parse().unwrap_or(1.0),
                })
            })
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
            {
                if let Some(hit) = magic_category(&magic) {
                    return hit.into();
                }
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
                "pcap" | "pcapng" | "raw" | "mem" | "jpg" | "jpeg" | "png" | "wav" | "ogg"
                | "flac" | "mp3" | "mp4" | "avi" | "mkv" => "forensics",
                "so" | "elf" | "exe" => "pwn",
                "py" if sample.contains("crypto")
                    || sample.contains("rsa")
                    || sample.contains("cipher") =>
                {
                    "crypto"
                }
                "wasm" | "class" | "dex" | "apk" | "ipa" => "rev",
                _ => continue,
            };
            break;
        }
    }
    category.into()
}

/// File-magic triage for extensionless artifacts: packet captures and
/// still images route to forensics, ELF binaries to pwn, and non-ELF
/// executables (PE, Mach-O) to reversing, where disassembly tooling
/// applies rather than memory-corruption playbooks.
fn magic_category(magic: &[u8]) -> Option<&'static str> {
    if magic.starts_with(b"\x7fELF") {
        return Some("pwn");
    }
    // pcap classic in both byte orders, plus the pcapng section header.
    if magic.starts_with(b"\xd4\xc3\xb2\xa1")
        || magic.starts_with(b"\xa1\xb2\xc3\xd4")
        || magic.starts_with(b"\x0a\x0d\x0d\x0a")
    {
        return Some("forensics");
    }
    // Still images: PNG and JPEG — where steganography lives.
    if magic.starts_with(b"\x89PNG") || magic.starts_with(b"\xff\xd8\xff") {
        return Some("forensics");
    }
    // Mach-O (32/64-bit, both endiannesses) and MZ (PE).
    if magic.starts_with(b"MZ")
        || magic.starts_with(b"\xcf\xfa\xed\xfe")
        || magic.starts_with(b"\xce\xfa\xed\xfe")
        || magic.starts_with(b"\xfe\xed\xfa\xcf")
        || magic.starts_with(b"\xfe\xed\xfa\xce")
    {
        return Some("rev");
    }
    None
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
    fn junk_matches_never_become_candidates() {
        let root = std::env::temp_dir().join(format!("wrose-ctf-junk-{}", std::process::id()));
        let engine = CtfEngine::new(&root);
        // Every one of these matched the old bare regex and polluted the
        // panel, the log, and the clipboard. None may survive now.
        for junk in [
            "noise HTB{} trailing",
            "HTB{REDACTED} in output",
            "see flag{your-flag-here} docs",
            "CTF{EXAMPLE} shown",
            "HTB{&quot;</span> in flag </span> junk}",
            "HTB{t3st_fl4g_aaaaaaaaaaaaaaaaaaaa} test data",
            "result HTB{4n0th3R_d4Y...} truncated",
            "HTB{with space} inside",
            "HTB{XXXX} uniform",
        ] {
            assert!(
                engine.detect("test", junk).is_empty(),
                "junk rejected: {junk}"
            );
        }
        // …while genuine flags still match exactly once.
        for real in [
            "HTB{4n0th3R_d4Y7h3r3_0k}",
            "flag{proof_123}",
            "picoCTF{abc-def_9}",
            "CTF{x}",
        ] {
            assert_eq!(engine.detect("test", real).len(), 1, "real kept: {real}");
        }
        // Custom --flag-format keeps exotic shapes, still drops junk.
        let custom = CtfEngine::new(&root)
            .with_flag_format(r"ZZZ\{[^}]+\}")
            .unwrap();
        assert_eq!(custom.detect("t", "ZZZ{with space ok}").len(), 1);
        assert!(custom.detect("t", "ZZZ{REDACTED}").is_empty());
        assert!(custom.detect("t", "ZZZ{}").is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn magic_and_extension_route_extensionless_artifacts() {
        let root = std::env::temp_dir().join(format!("wrose-ctf-magic-{}", std::process::id()));
        let case = |name: &str, bytes: &[u8], want: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("artifact"), bytes).unwrap();
            assert_eq!(categorize(&dir, ""), want, "case {name}");
        };
        case("pcap", b"\xd4\xc3\xb2\xa1\x02\x00\x04\x00", "forensics");
        case("pcapng", b"\x0a\x0d\x0d\x0a\x1c\x00\x00\x00", "forensics");
        case("png", b"\x89PNG\r\n\x1a\n rest of file", "forensics");
        case("jpeg", b"\xff\xd8\xff\xe0\x00\x10JFIF", "forensics");
        case("elf", b"\x7fELF\x02\x01\x01\x00", "pwn");
        case("pe", b"MZ\x90\x00\x03\x00\x00\x00", "rev");
        case("macho", b"\xcf\xfa\xed\xfe\x07\x00\x00\x01", "rev");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn media_and_mobile_extensions_route_without_magic() {
        let root = std::env::temp_dir().join(format!("wrose-ctf-ext-{}", std::process::id()));
        let case = |name: &str, file: &str, want: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), b"not magic at all....").unwrap();
            assert_eq!(categorize(&dir, ""), want, "case {name}");
        };
        case("mp3", "audio.mp3", "forensics");
        case("mp4", "clip.mp4", "forensics");
        case("apk", "app.apk", "rev");
        case("txt", "notes.txt", "misc");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stream_scan_detects_flags_split_across_chunks() {
        let root = std::env::temp_dir().join(format!("wrose-ctf-stream-{}", std::process::id()));
        let engine = CtfEngine::new(&root);
        assert!(engine
            .scan_stream("tool:stdout", "prefix flag{split_")
            .unwrap()
            .is_empty());
        let hits = engine.scan_stream("tool:stdout", "across_chunks}").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].flag, "flag{split_across_chunks}");
        assert!(engine.scan_stream("tool:stdout", "}").unwrap().is_empty());
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

    #[test]
    fn project_scan_caches_hits_and_invalidates_changed_artifacts() {
        let root = std::env::temp_dir().join(format!(
            "wrose-ctf-artifact-cache-{}-{}",
            std::process::id(),
            unix_timestamp()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let artifact = root.join("evidence.txt");
        std::fs::write(&artifact, "flag{cached_one}").unwrap();

        let first = CtfEngine::new(&root).scan_project();
        assert_eq!(
            first
                .iter()
                .map(|hit| hit.flag.as_str())
                .collect::<Vec<_>>(),
            ["flag{cached_one}"]
        );
        assert!(root.join(".ctf/artifact-index.json").is_file());

        let second = CtfEngine::new(&root).scan_project();
        assert!(
            second.is_empty(),
            "flags remain deduplicated across restarts"
        );

        std::fs::write(&artifact, "flag{cached_two_with_new_size}").unwrap();
        let changed = CtfEngine::new(&root).scan_project();
        assert_eq!(
            changed
                .iter()
                .map(|hit| hit.flag.as_str())
                .collect::<Vec<_>>(),
            ["flag{cached_two_with_new_size}"]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn binary_artifacts_scan_and_replay_lossy() {
        let root = std::env::temp_dir().join(format!("wrose-ctf-binary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // A pcap that is not valid UTF-8 as a whole but carries an ASCII
        // flag body: the scan must still surface it, or forensics evidence
        // can never verify.
        let mut blob = b"\xd4\xc3\xb2\xa1\x02\x00\x04\x00".to_vec();
        blob.extend_from_slice(&[0u8; 64]);
        blob.extend_from_slice(b"flag{binary_pcap_ok}\n");
        std::fs::write(root.join("capture"), &blob).unwrap();
        let engine = CtfEngine::new(&root);
        let hits = engine.scan_project();
        assert!(
            hits.iter()
                .any(|hit| hit.flag == "flag{binary_pcap_ok}" && hit.source == "file:capture"),
            "binary artifact yields a file: hit: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn current_evidence_keeps_tool_provenance_after_model_candidate() {
        let root = std::env::temp_dir().join(format!("wrose-evidence-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut engine = CtfEngine::new(&root);
        engine
            .scan("model response", "HTB{provenance_kept}")
            .unwrap();
        engine.scan("tool:shell", "HTB{provenance_kept}").unwrap();
        let evidence = engine.current_hits();
        let verified = crate::autopilot::verify(&engine, &evidence).unwrap();
        assert!(verified.evidence.contains("tool:shell"));
        // Restart and new challenge must retain history, but cannot use stale
        // terminal output as proof that the new challenge is already solved.
        let restarted = CtfEngine::new(&root);
        assert!(!restarted.recorded_hits().is_empty());
        assert!(restarted.current_hits().is_empty());
        engine.begin_run();
        assert!(crate::autopilot::verify(&engine, &engine.current_hits()).is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
