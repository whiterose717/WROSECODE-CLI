//! CTF autopilot (spec PHASE 5): the scope guard, run budget, triage batch,
//! installed-tool inventory, stuck handling, flag verification, and the
//! challenge brief that keeps the agent working until a flag is verified.
//!
//! The headless entry (`wrosecode ctf <target>`) drives the loop from
//! [`run`]; the TUI's `/ctf` command reuses every piece here and runs the
//! turns itself so the transcript stays visible.

use crate::agent::Agent;
use crate::ctf::{self, CtfEngine, FlagHit};
use crate::skills;
use crate::tools::shell;
use anyhow::Result;
use futures::stream::{self, StreamExt};
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the user supplied for this challenge: files (challenge artifacts and
/// the working directory) and hosts (`--remote`, or the target URL). Anything
/// outside it needs explicit approval before a tool may touch it.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    roots: Vec<PathBuf>,
    hosts: Vec<String>,
    excluded_hosts: Vec<String>,
}

impl Scope {
    /// Build the allowlist from the challenge target: an existing file or
    /// directory becomes a root (alongside the session working directory),
    /// a URL contributes its host, and `--remote host:port` adds a host.
    pub fn for_target(target: &str, root: &Path, remote: Option<&str>) -> Self {
        let mut scope =
            Self::from_parts(root, std::iter::empty::<&str>(), std::iter::empty::<&str>());
        if let Some(host) = host_entry(target) {
            scope.hosts.push(host);
        }
        if let Some(remote) = remote.and_then(host_entry) {
            scope.hosts.push(remote);
        }
        scope.add_local_root(target);
        scope.normalize();
        scope
    }

    /// Build a bug-bounty allowlist using explicit included hosts only.
    /// Target URL must not implicitly widen program scope. Local target
    /// directories are still permitted as workspace roots.
    /// Excluded hosts always win over includes.
    pub fn for_bounty<S, E, X, Y>(root: &Path, target: &str, includes: S, excludes: E) -> Self
    where
        S: IntoIterator<Item = X>,
        E: IntoIterator<Item = Y>,
        X: AsRef<str>,
        Y: AsRef<str>,
    {
        let mut scope = Self::from_parts(root, includes, excludes);
        scope.add_local_root(target);
        scope.normalize();
        scope
    }

    fn from_parts<S, E, X, Y>(root: &Path, includes: S, excludes: E) -> Self
    where
        S: IntoIterator<Item = X>,
        E: IntoIterator<Item = Y>,
        X: AsRef<str>,
        Y: AsRef<str>,
    {
        Self {
            roots: vec![root.to_path_buf()],
            hosts: includes
                .into_iter()
                .filter_map(|entry| host_entry(entry.as_ref()))
                .collect(),
            excluded_hosts: excludes
                .into_iter()
                .filter_map(|entry| host_entry(entry.as_ref()))
                .collect(),
        }
    }

    fn add_local_root(&mut self, target: &str) {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return;
        }
        let path = Path::new(trimmed);
        if path.exists() {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if !self.roots.contains(&canonical) {
                self.roots.push(canonical);
            }
        }
    }

    fn normalize(&mut self) {
        self.hosts.sort();
        self.hosts.dedup();
        self.excluded_hosts.sort();
        self.excluded_hosts.dedup();
    }

    pub fn allowed_hosts(&self) -> &[String] {
        &self.hosts
    }

    pub fn excluded_hosts(&self) -> &[String] {
        &self.excluded_hosts
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The allowlist line shown when the autopilot starts.
    pub fn describe(&self) -> String {
        let files = self
            .roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let hosts = if self.hosts.is_empty() {
            "none (network calls need approval)".to_string()
        } else {
            self.hosts.join(", ")
        };
        if self.excluded_hosts.is_empty() {
            format!("files {files} · hosts {hosts}")
        } else {
            format!(
                "files {files} · hosts {hosts} · excluded {}",
                self.excluded_hosts.join(", ")
            )
        }
    }

    /// May a write land here? `/tmp` is standard scratch space and the
    /// session root is always the user's own directory.
    pub fn allows_path(&self, root: &Path, path: &Path) -> bool {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        if resolved.starts_with("/tmp") {
            return true;
        }
        let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
        self.roots.iter().any(|allowed| {
            canonical.starts_with(allowed)
                || std::fs::canonicalize(allowed)
                    .is_ok_and(|allowed| canonical.starts_with(allowed))
        })
    }

    /// Is `host` (bare, `host:port`, or from a URL) on the allowlist? An
    /// entry without a port matches any port on that host. `*.example.com`
    /// matches subdomains but not the apex; exclusions are checked first.
    pub fn allows_host(&self, host: &str) -> bool {
        let Some((name, port)) = split_host(host) else {
            return true;
        };
        if self
            .excluded_hosts
            .iter()
            .any(|entry| host_entry_matches(entry, &name, port.as_deref()))
        {
            return false;
        }
        self.hosts
            .iter()
            .any(|entry| host_entry_matches(entry, &name, port.as_deref()))
    }
}

/// Strip a URL down to its authority, or accept a bare host entry. Returns
/// `None` for paths, descriptions, and empty input.
pub fn host_entry(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.contains("://") {
        return url_host(value);
    }
    if value.starts_with('/') || value.contains(char::is_whitespace) {
        return None;
    }
    let authority = value
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if authority.is_empty() {
        return None;
    }
    Some(authority)
}

/// Split `host`, `host:port`, or a URL into a lowercased name and optional
/// port. IPv6 literals in brackets are kept intact.
pub fn split_host(value: &str) -> Option<(String, Option<String>)> {
    let authority = host_entry(value)?;
    if let Some(rest) = authority.strip_prefix('[') {
        let (name, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':').filter(|port| !port.is_empty());
        return Some((name.to_ascii_lowercase(), port.map(str::to_string)));
    }
    match authority.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            Some((name.to_string(), Some(port.to_string())))
        }
        _ => Some((authority, None)),
    }
}

fn host_entry_matches(entry: &str, name: &str, port: Option<&str>) -> bool {
    let Some((pattern, entry_port)) = split_host(entry) else {
        return false;
    };
    if let (Some(entry_port), port) = (entry_port.as_deref(), port) {
        if port != Some(entry_port) {
            return false;
        }
    } else if entry_port.is_some() {
        // A port-qualified entry never authorizes the same host on another
        // port or an unspecified one.
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return name.len() > suffix.len()
            && name.ends_with(suffix)
            && name.as_bytes()[name.len() - suffix.len() - 1] == b'.';
    }
    pattern == name
}

/// The host inside a URL, lowercased, keeping an explicit port.
pub fn url_host(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https" | "ftp" | "ws" | "wss") {
        return None;
    }
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return None;
    }
    Some(authority.to_ascii_lowercase())
}

/// Optional safety ceilings for the autopilot: model turns, total tokens,
/// and wall time. Zero means unlimited. The interactive `/ctf` mode defaults
/// to unlimited so it keeps searching until a flag is verified or the user
/// cancels; explicit `--budget` values still provide bounded runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub steps: usize,
    pub tokens: u64,
    pub seconds: u64,
}

impl Budget {
    pub fn describe(&self) -> String {
        if self.steps == 0 && self.tokens == 0 && self.seconds == 0 {
            "unlimited".into()
        } else {
            format!(
                "{} steps / {} tokens / {} seconds",
                self.steps, self.tokens, self.seconds
            )
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self::default());
        }
        if text.eq_ignore_ascii_case("unlimited") || text.eq_ignore_ascii_case("none") {
            return Ok(Self::default());
        }
        if !text.contains('=') {
            let steps: usize = text
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid --budget `{text}` (want steps or `steps=40,tokens=300000,seconds=1800`))"))?;
            return Ok(Self {
                steps,
                ..Self::default()
            });
        }
        let mut budget = Self::default();
        let mut seen = 0;
        for part in text.split(',').filter(|part| !part.trim().is_empty()) {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("invalid --budget segment `{part}`"))?;
            match key.trim() {
                "steps" => {
                    budget.steps = value.trim().parse()?;
                    seen += 1;
                }
                "tokens" => {
                    budget.tokens = value.trim().parse()?;
                    seen += 1;
                }
                "seconds" => {
                    budget.seconds = value.trim().parse()?;
                    seen += 1;
                }
                other => anyhow::bail!("unknown --budget key `{other}`"),
            }
        }
        if seen == 0 {
            anyhow::bail!("--budget needs at least one of steps, tokens, seconds");
        }
        Ok(budget)
    }

    /// The first exhausted dimension, if any: `steps`, `tokens`, or
    /// `seconds`.
    pub fn exhausted(&self, steps: usize, tokens: u64, elapsed: Duration) -> Option<&'static str> {
        if self.steps > 0 && steps >= self.steps {
            return Some("steps");
        }
        if self.tokens > 0 && tokens >= self.tokens {
            return Some("tokens");
        }
        if self.seconds > 0 && elapsed.as_secs() >= self.seconds {
            return Some("seconds");
        }
        None
    }
}

/// The challenge-specific commands worth trying, split into what this
/// machine actually has. The agent is told both lists so a missing tool
/// becomes a suggested install instead of a silent failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolInventory {
    pub present: Vec<String>,
    pub missing: Vec<String>,
}

/// Every tool the Phase 5 spec asks about, looked up on `PATH`.
pub fn tool_inventory() -> ToolInventory {
    tool_inventory_from(&std::env::var("PATH").unwrap_or_default())
}

pub fn tool_inventory_from(path: &str) -> ToolInventory {
    let candidates: [(&str, &str); 17] = [
        ("gdb", "gdb"),
        ("r2", "r2"),
        ("objdump", "objdump"),
        ("ghidra-headless", "ghidra-headless"),
        ("python3", "python3"),
        ("binwalk", "binwalk"),
        ("steghide", "steghide"),
        ("zsteg", "zsteg"),
        ("exiftool", "exiftool"),
        ("john", "john"),
        ("hashcat", "hashcat"),
        ("openssl", "openssl"),
        ("sage", "sage"),
        ("tshark", "tshark"),
        ("foremost", "foremost"),
        ("volatility", "volatility"),
        ("checksec", "checksec"),
    ];
    let mut inventory = ToolInventory::default();
    for (name, binary) in candidates {
        if has_binary(path, binary) {
            inventory.present.push(name.to_string());
        } else {
            inventory.missing.push(name.to_string());
        }
    }
    inventory
}

fn has_binary(path: &str, binary: &str) -> bool {
    path.split(':').any(|dir| {
        !dir.is_empty()
            && std::fs::metadata(Path::new(dir).join(binary)).is_ok_and(|meta| meta.is_file())
    })
}

impl ToolInventory {
    /// One line for the brief: what exists, and what to install instead.
    pub fn describe(&self) -> String {
        let present = if self.present.is_empty() {
            "none".to_string()
        } else {
            self.present.join(", ")
        };
        let missing = if self.missing.is_empty() {
            "none".to_string()
        } else {
            self.missing.join(", ") + " (suggest `apt install <tool>` instead of failing silently)"
        };
        format!("Installed: {present}. Missing: {missing}")
    }
}

/// The first triage batch: cheap evidence, one pass, before any reasoning.
/// Every command degrades to a "not installed" note instead of an error.
pub async fn triage(root: &Path) -> String {
    let inventory = tool_inventory();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && !matches!(
                            path.file_name().and_then(|name| name.to_str()),
                            Some(".gitignore" | "Cargo.lock" | "package-lock.json")
                        )
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    entries.truncate(8);

    // Triage is deliberately read-only, so these independent probes can run
    // concurrently. Keep the concurrency bounded: a challenge directory may
    // contain expensive binaries, and unbounded process spawning makes the
    // agent less responsive rather than faster.
    let mut commands = vec![(0usize, "ls -la".to_string())];
    if entries.is_empty() {
        commands.push((1, "ls -la".to_string()));
    }
    for (entry_index, path) in entries.iter().enumerate() {
        let name = shell_quote(&path.display().to_string());
        let probes = [
            format!("file {name}"),
            format!("xxd {name} | head -n 8"),
            format!("strings -n 8 {name} | head -n 30"),
        ];
        for (probe_index, command) in probes.into_iter().enumerate() {
            commands.push((1 + entry_index * 10 + probe_index, command));
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match extension.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "wav" | "mp3" => {
                commands.push((
                    1 + entry_index * 10 + 3,
                    format!("exiftool {name} | head -n 20"),
                ));
            }
            "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" => {
                commands.push((
                    1 + entry_index * 10 + 3,
                    format!("unzip -l {name} | head -n 20"),
                ));
            }
            _ => {}
        }
        if inventory.present.iter().any(|tool| tool == "binwalk") {
            commands.push((1 + entry_index * 10 + 4, format!("binwalk {name}")));
        }
        if inventory.present.iter().any(|tool| tool == "checksec") {
            commands.push((1 + entry_index * 10 + 5, format!("checksec --file={name}")));
        }
    }

    let root = root.to_path_buf();
    let results = stream::iter(commands.into_iter().map(|(order, command)| {
        let root = root.clone();
        let inventory = inventory.clone();
        async move { (order, run_triage(&root, &command, &inventory).await) }
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;

    let mut results = results;
    results.sort_by_key(|(order, _)| *order);
    let mut lines = vec!["TRIAGE (4 bounded read-only workers)".to_string()];
    lines.extend(results.into_iter().map(|(_, output)| output));
    lines.join("\n")
}

async fn run_triage(root: &Path, command: &str, inventory: &ToolInventory) -> String {
    let binary = command.split_whitespace().next().unwrap_or_default();
    let known_missing = inventory.missing.iter().any(|tool| tool == binary);
    if known_missing {
        return format!("$ {command}\n  skipped · {binary} is not installed");
    }
    match shell::run_with_timeout(command, root, 5).await {
        Ok(output) => {
            let output = output.trim();
            let output: String = output
                .chars()
                .take(1_200)
                .collect::<Vec<_>>()
                .into_iter()
                .collect();
            if output.is_empty() {
                format!("$ {command}\n  (no output)")
            } else {
                format!("$ {command}\n{}", indent(&output))
            }
        }
        Err(error) => format!("$ {command}\n  failed: {error:#}"),
    }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "/._-".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// The category hypotheses the stuck handler rotates through.
pub const CATEGORIES: [&str; 8] = [
    "crypto",
    "web",
    "pwn",
    "rev",
    "forensics",
    "stego",
    "misc",
    "osint-offline",
];

/// Low-progress tracking: after three failing steps the autopilot raises the
/// thinking level, rotates the category hypothesis, asks for a fresh-context
/// subagent, and re-reads the challenge notes.
#[derive(Clone, Debug, Default)]
pub struct Stuck {
    pub low_progress: usize,
    pub hypothesis: usize,
}

impl Stuck {
    pub const THRESHOLD: usize = 3;

    /// Record a step's outcome; returns true when a stuck nudge should fire
    /// (and resets the streak so nudges do not repeat every step).
    pub fn step(&mut self, failures: usize) -> bool {
        if failures > 0 {
            self.low_progress += failures;
        } else {
            self.low_progress = 0;
        }
        if self.low_progress >= Self::THRESHOLD {
            self.low_progress = 0;
            self.hypothesis = (self.hypothesis + 1) % CATEGORIES.len();
            return true;
        }
        false
    }

    /// The transcript line and next-turn nudge for a stuck run.
    pub fn nudge(&self, current: &str) -> String {
        format!(
            "think: medium → high (no progress ×{})\n\
             Stuck handling: hypothesis switched `{current}` → `{}`.\n\
             Re-read the challenge text and hints for missed clues, fork a \
             fresh-context subagent with `delegate_task` for one independent \
             attempt at `{}`, and re-check the assumptions list in ctf-notes.md \
             before repeating anything already recorded there as a dead end.",
            Stuck::THRESHOLD,
            CATEGORIES[self.hypothesis],
            CATEGORIES[self.hypothesis],
        )
    }
}

/// The autopilot's verdict on a recorded flag: verified means the flag was
/// re-derived from a challenge artifact (or confirmed by a checker), not just
/// asserted by the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub flag: String,
    pub evidence: String,
}

/// Decide whether any recorded hit is a verified flag.
///
/// * `file:` hits are re-derived: the artifact is re-read and the same flag
///   must appear again through the recorded transformation.
/// * `tool:` hits are trusted execution evidence. Plain output (for example a
///   remote service printing `HTB{...}`) and decoded output both verify; model
///   text never uses a `tool:` source.
/// * `model response` claims are candidates until the same flag also shows
///   up from a file or a decode chain.
///
/// Replay every hit against its source and return the first flag that
/// re-derives. `engine` carries the run's flag patterns, so a
/// `--flag-format` run verifies with the same regex it detected with.
pub fn verify(engine: &CtfEngine, hits: &[FlagHit]) -> Option<Verified> {
    let root = engine.root();
    let mut from_artifact: BTreeSet<&str> = BTreeSet::new();
    for hit in hits {
        // Same lossy view as the project scan: binary artifacts replay
        // exactly what the scan saw, so forensics evidence verifies.
        let derived = match hit.source.strip_prefix("file:") {
            Some(relative) => std::fs::read(root.join(relative))
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .is_some_and(|text| {
                    engine
                        .detect(&hit.source, &text)
                        .iter()
                        .any(|found| found.flag == hit.flag)
                }),
            None => hit.source.starts_with("tool:"),
        };
        if derived {
            from_artifact.insert(hit.flag.as_str());
        }
    }
    for hit in hits {
        if from_artifact.contains(hit.flag.as_str())
            && (hit.source.starts_with("file:") || hit.source.starts_with("tool:"))
        {
            let evidence = if let Some(path) = hit.source.strip_prefix("file:") {
                format!("re-derived from {path} via {} decode", hit.transformation)
            } else if hit.transformation != "plain" {
                format!("decode chain `{}` from {}", hit.transformation, hit.source)
            } else {
                format!("extracted from {}", hit.source)
            };
            return Some(Verified {
                flag: hit.flag.clone(),
                evidence,
            });
        }
    }
    None
}

/// Fast local-artifact path: scan a bounded, overlapping window rather than
/// loading a whole PCAP into memory. A candidate is independently re-read
/// from its original byte range before it can be reported as verified.
pub(crate) fn preflight_local_flag(
    engine: &CtfEngine,
    root: &Path,
    target: &str,
) -> Option<Verified> {
    const WINDOW_BYTES: usize = 128 * 1024;
    const OVERLAP_BYTES: usize = 4096;
    const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
    const PREFLIGHT_BUDGET: Duration = Duration::from_secs(15);
    let candidate = Path::new(target);
    let path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ARTIFACT_BYTES {
        return None;
    }
    let relative = path.strip_prefix(root).unwrap_or(path.as_path());
    let source = format!("file:{}", relative.display());
    let mut file = std::fs::File::open(&path).ok()?;
    let mut block = vec![0_u8; WINDOW_BYTES];
    let mut overlap = Vec::<u8>::new();
    let mut consumed = 0_u64;
    let started = Instant::now();
    let mut unique: Option<(FlagHit, u64, Vec<u8>)> = None;
    loop {
        // A deliberately short local preflight must not become a TUI stall on
        // a gigabyte-long capture; ambiguous/large inputs fall back to triage.
        if started.elapsed() > PREFLIGHT_BUDGET {
            return None;
        }
        let size = file.read(&mut block).ok()?;
        if size == 0 {
            break;
        }
        let start = consumed.saturating_sub(overlap.len() as u64);
        let mut window = Vec::with_capacity(overlap.len() + size);
        window.extend_from_slice(&overlap);
        window.extend_from_slice(&block[..size]);
        let hits = engine.detect(&source, &String::from_utf8_lossy(&window));
        for hit in hits {
            if let Some((existing, _, _)) = &unique {
                // Multiple distinct plausible flags require further evidence.
                // Never auto-submit an arbitrary first match.
                if existing.flag != hit.flag {
                    return None;
                }
            } else {
                unique = Some((hit, start, window.clone()));
            }
        }
        overlap.clear();
        overlap.extend_from_slice(&window[window.len().saturating_sub(OVERLAP_BYTES)..]);
        consumed += size as u64;
    }
    let (candidate, offset, original) = unique?;
    let mut replay = vec![0_u8; original.len()];
    let mut evidence_file = std::fs::File::open(&path).ok()?;
    evidence_file.seek(SeekFrom::Start(offset)).ok()?;
    evidence_file.read_exact(&mut replay).ok()?;
    if replay != original {
        return None;
    }
    let text = String::from_utf8_lossy(&replay);
    if !engine
        .detect(&source, &text)
        .iter()
        .any(|hit| hit.flag == candidate.flag && hit.transformation == candidate.transformation)
    {
        return None;
    }
    let _ = engine.scan(&source, &text);
    Some(Verified {
        flag: candidate.flag,
        evidence: format!(
            "re-derived from {} at byte offset {} via {} decode",
            relative.display(),
            offset,
            candidate.transformation
        ),
    })
}

/// Keep binary/regex work off Tokio's I/O executor so provider streams,
/// signal handling, and MCP heartbeats are not starved by a large capture.
pub(crate) async fn preflight_local_flag_async(
    engine: CtfEngine,
    root: PathBuf,
    target: String,
) -> Option<Verified> {
    tokio::task::spawn_blocking(move || preflight_local_flag(&engine, &root, &target))
        .await
        .ok()
        .flatten()
}

/// A candidate the autopilot saw but could not verify, with what is missing.
pub fn candidate_note(engine: &CtfEngine, hits: &[FlagHit]) -> Option<String> {
    let verified = verify(engine, hits).map(|verified| verified.flag);
    hits.iter()
        .filter(|hit| !verified.as_deref().is_some_and(|flag| flag == hit.flag))
        .max_by(|a, b| a.confidence.total_cmp(&b.confidence))
        .map(|hit| {
            format!(
                "candidate, unverified: `{}` from {} ({}) — missing: re-derive it \
                 from the artifact or confirm it against the remote service",
                hit.flag, hit.source, hit.transformation
            )
        })
}

/// The CTF autopilot options carried from the CLI or `/ctf`.
#[derive(Clone, Debug)]
pub struct Options {
    pub target: String,
    pub flag_format: Option<String>,
    pub category: Option<String>,
    pub remote: Option<String>,
    pub budget: Budget,
    pub parallel: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            target: ".".into(),
            flag_format: None,
            category: None,
            remote: None,
            budget: Budget::default(),
            parallel: 3,
        }
    }
}

/// Parse the interactive `/ctf` arguments. Targets may be paths, URLs, or a
/// natural-language description containing spaces; option values are kept
/// separate so a description cannot accidentally become the last word only.
pub fn parse_command_args(args: &str) -> Result<Options> {
    let mut options = Options::default();
    let mut target = Vec::new();
    let mut words = args.split_whitespace().peekable();
    while let Some(word) = words.next() {
        match word {
            "--flag-format" => {
                options.flag_format = Some(
                    words
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--flag-format needs a value"))?
                        .to_string(),
                );
            }
            "--category" => {
                options.category = Some(
                    words
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--category needs a value"))?
                        .to_string(),
                );
            }
            "--remote" => {
                options.remote = Some(
                    words
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--remote needs a value"))?
                        .to_string(),
                );
            }
            "--budget" => {
                let value = words
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--budget needs a value"))?;
                options.budget = Budget::parse(value)?;
            }
            "--parallel" => {
                options.parallel = words
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--parallel needs a number"))?
                    .parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("--parallel needs a number"))?;
            }
            other if other.starts_with("--") => {
                anyhow::bail!("unknown flag {other}");
            }
            other => target.push(other.trim_matches(|ch| ch == '\"' || ch == '\'')),
        }
    }
    if !target.is_empty() {
        options.target = target.join(" ");
    }
    Ok(options)
}

/// `ctf-notes.md` — the shared scratch pad: challenge, scope, triage,
/// assumptions, dead ends. The agent is told to append dead ends here so
/// nothing is retried, and the stuck handler re-reads it.
pub fn notes_path(root: &Path) -> PathBuf {
    root.join("ctf-notes.md")
}

pub fn init_notes(
    root: &Path,
    options: &Options,
    scope: &Scope,
    triage_report: &str,
) -> Result<PathBuf> {
    let path = notes_path(root);
    let body = format!(
        "# CTF notes — {}\n\n\
         ## Challenge\n\n- target: `{}`\n- category hypothesis: {}\n- flag format: {}\n\n\
         ## Scope (allowlist)\n\n{}\n\n\
         ## Triage\n\n```text\n{triage_report}\n```\n\n\
         ## Assumptions\n\n- (record every assumption here before acting on it)\n\n\
         ## Dead ends\n\n- (append what was tried and failed; never retry a dead end)\n",
        options.target,
        options.target,
        options
            .category
            .clone()
            .unwrap_or_else(|| "unclassified".into()),
        options
            .flag_format
            .clone()
            .unwrap_or_else(|| "flag{…} / CTF{…} / HTB{…} / picoCTF{…}".into()),
        scope.describe(),
    );
    std::fs::create_dir_all(root)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

pub fn append_note(root: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(notes_path(root))
    {
        let _ = writeln!(file, "{line}");
    }
}

/// The category guess after triage: the target's filename/keywords first,
/// then the artifact sniffing in [`ctf::categorize`].
pub fn guess_category(root: &Path, target: &str, triage_report: &str) -> String {
    let hint = format!("{target}\n{triage_report}");
    let category = ctf::categorize(root, &hint);
    CATEGORIES
        .iter()
        .find(|known| **known == category)
        .map(|known| (*known).to_string())
        .unwrap_or_else(|| "misc".into())
}

/// The opening brief: scope, tools, playbook, triage, and the rules that
/// keep the run unattended (no check-ins, verify before claiming, record
/// dead ends).
pub fn brief(
    options: &Options,
    scope: &Scope,
    inventory: &ToolInventory,
    category: &str,
    triage_report: &str,
    playbook: Option<&skills::Skill>,
) -> String {
    let playbook_block = match playbook {
        Some(skill) => format!(
            "Follow the escalation ladder in the `{}` skill (cheap → expensive);\n{}\n",
            skill.name,
            skill
                .body
                .chars()
                .take(3_000)
                .collect::<String>()
        ),
        None => "No dedicated playbook matched; work generic escalation: enumerate, classify, decode, exploit.\n"
            .to_string(),
    };
    format!(
        "CTF AUTOPILOT — keep working until a flag is verified, or until the user cancels.\n\
         Target: `{}`\n\
         Category hypothesis: {category}\n\
         Flag format: {}\n\
         Scope: {}\n\
         Tools: {}\n\
         Budget: {}\n\
         {playbook_block}\
         Rules:\n\
         1. No check-ins: work autonomously. YOLO skips ordinary approval prompts; the scope allowlist\n\
            and the destructive-command gate always apply. Widen scope with --remote instead of guessing.\n\
         2. Record every dead end in ctf-notes.md so nothing is retried; re-read it when stuck.\n\
         3. Any string matching the flag format is a candidate — verify it (re-derive the decode\n\
            chain, re-run the extraction, or test it against the remote service) before claiming success.\n\
         4. Fork independent hypotheses with `delegate_task`; give each child one falsifiable\n\
            hypothesis and demand commands/evidence, not speculation. Use `decode` for sweeps.\n\
         5. Prefer deterministic evidence first: metadata/magic/strings/protocol structure, then\n\
            exploit/decrypt. If an approach fails, record the exact reason and pivot once.\n\
         6. A plain flag printed by a real tool or remote response is evidence; model-authored\n\
            flag text is never evidence. Stop immediately after a verified flag.\n\
         TRIAGE:\n```text\n{triage_report}\n```\n",
        options.target,
        options
            .flag_format
            .clone()
            .unwrap_or_else(|| "flag{{…}} / CTF{{…}} / HTB{{…}} / picoCTF{{…}}".into()),
        scope.describe(),
        inventory.describe(),
        options.budget.describe(),
    )
}

/// The stop report on budget exhaustion: what was tried, what was learned,
/// what to try next. Never leave silently.
pub fn stop_report(
    root: &Path,
    options: &Options,
    steps: usize,
    tokens: u64,
    reason: &str,
) -> String {
    let notes = std::fs::read_to_string(notes_path(root)).unwrap_or_default();
    let dead_ends = notes
        .lines()
        .skip_while(|line| !line.starts_with("## Dead ends"))
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .filter(|line| !line.trim().is_empty() && line.trim() != "-")
        .collect::<Vec<_>>();
    let flags = CtfEngine::new(root).history("");
    let mut report = format!(
        "BUDGET EXHAUSTED ({reason}) — not silently ending.\n\
         Tried: {steps} steps · {tokens} tokens · target `{}`\n",
        options.target
    );
    report.push_str("Learned:\n");
    if dead_ends.is_empty() && flags.is_empty() {
        report.push_str("- no verified findings yet; see ctf-notes.md\n");
    } else {
        for line in dead_ends.iter().take(8) {
            report.push_str(&format!("- {}\n", line.trim_start_matches("- ")));
        }
        for flag in flags.iter().take(3) {
            report.push_str(&format!("- flag recorded: {flag}\n"));
        }
    }
    report.push_str(
        "Next: pick an untried category hypothesis, fork a fresh-context subagent, \
         and re-read the challenge text for missed clues.",
    );
    report
}

/// What a headless `wrosecode ctf` run ended as.
pub enum Outcome {
    Verified {
        flag: String,
        evidence: String,
    },
    Unsolved {
        report: String,
        candidate: Option<String>,
    },
}

/// A tool call that would leave the allowlist, with the human-readable reason
/// the autopilot shows in the approval prompt (headless runs are denied
/// outright — there is nobody there to approve).
pub fn violation(
    scope: &Scope,
    root: &Path,
    name: &str,
    input: &serde_json::Value,
) -> Option<String> {
    let path = input.get("path").and_then(|value| value.as_str());
    match name {
        "write_file" | "edit_file" => {
            let path = path?;
            if scope.allows_path(root, Path::new(path)) {
                None
            } else {
                Some(format!("write {path} is outside the allowed files"))
            }
        }
        "apply_patch" => crate::tools::patch::paths(input.get("patch")?.as_str()?)
            .into_iter()
            .find(|path| !scope.allows_path(root, Path::new(path)))
            .map(|path| format!("write {path} is outside the allowed files")),
        "shell" => shell_violation(scope, root, input.get("command")?.as_str()?),
        "http" | "web_fetch" | "browser_capture" => {
            let url = input.get("url").and_then(|value| value.as_str()).or(path)?;
            let host = url_host(url)?;
            if scope.allows_host(&host) {
                None
            } else {
                Some(format!(
                    "network to {host} is out of scope (approve it, or add it with --remote {host})"
                ))
            }
        }
        _ => None,
    }
}

/// What in a shell command leaves the allowlist: network verbs aimed at a
/// host, and writes aimed at an absolute path outside the roots. Ambiguous
/// commands stay out of this path and fall through to the normal permission
/// prompt, erring toward asking.
pub fn shell_violation(scope: &Scope, root: &Path, command: &str) -> Option<String> {
    const NET_VERBS: [&str; 13] = [
        "curl", "wget", "nc", "ncat", "ssh", "scp", "ftp", "telnet", "ping", "dig", "host", "nmap",
        "socat",
    ];
    const WRITE_VERBS: [&str; 10] = [
        "cp", "mv", "rm", "touch", "mkdir", "chmod", "chown", "dd", "tee", "install",
    ];
    let mut words = command.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let word = word.trim_start_matches(['|', ';', '&']);
        if NET_VERBS.contains(&word) {
            for arg in words.by_ref() {
                if arg.starts_with('-') {
                    continue;
                }
                let candidate = arg
                    .trim_start_matches(['\'', '"'])
                    .trim_end_matches(['\'', '"']);
                if let Some(host) = url_host(candidate) {
                    if !scope.allows_host(&host) {
                        return Some(format!(
                            "network to {host} is out of scope (approve it, or add it with --remote {host})"
                        ));
                    }
                } else if candidate.contains(':')
                    && !candidate.starts_with('/')
                    && candidate
                        .split_once(':')
                        .is_some_and(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
                    && !scope.allows_host(candidate)
                {
                    return Some(format!(
                        "network to {candidate} is out of scope (approve it, or add it with --remote {candidate})"
                    ));
                }
                break;
            }
        }
        if WRITE_VERBS.contains(&word) {
            if let Some(arg) = words.next() {
                let candidate = arg.trim_start_matches(['\'', '"']);
                if candidate.starts_with('/') && !scope.allows_path(root, Path::new(candidate)) {
                    return Some(format!("write {candidate} is outside the allowed files"));
                }
            }
        }
    }
    for (index, part) in command.split('>').enumerate() {
        if index == 0 {
            continue;
        }
        let target = part.split_whitespace().next().unwrap_or_default();
        if target.starts_with('/') && !scope.allows_path(root, Path::new(target)) {
            return Some(format!("write {target} is outside the allowed files"));
        }
    }
    None
}

/// The headless autopilot: arm the scope, triage, then turn after turn until
/// a flag verifies or the budget stops the run.
pub async fn run(agent: &mut Agent, options: &Options) -> Result<Outcome> {
    let root = agent.config.root.clone();
    let scope = Scope::for_target(&options.target, &root, options.remote.as_deref());
    agent.tools.set_scope(Some(scope.clone()));
    if let Some(pattern) = &options.flag_format {
        agent.ctf = agent.ctf.clone().with_flag_format(pattern)?;
    }
    if let Some(category) = &options.category {
        agent.ctf.category = category.clone();
    }
    agent.ctf.begin_run();
    agent.ctf.lock_category();
    if let Some(verified) =
        preflight_local_flag_async(agent.ctf.clone(), root.clone(), options.target.clone()).await
    {
        println!("SCOPE   {}", scope.describe());
        println!("TOOLS   local artifact verifier (model probes skipped)");
        let category = options
            .category
            .clone()
            .unwrap_or_else(|| crate::ctf::categorize(&root, &options.target));
        agent.ctf.category = category.clone();
        println!("GUESS   {category}");
        println!("✔ flag verified: {} ({})", verified.flag, verified.evidence);
        return Ok(Outcome::Verified {
            flag: verified.flag,
            evidence: verified.evidence,
        });
    }
    let inventory = tool_inventory();
    println!("SCOPE   {}", scope.describe());
    println!("TOOLS   {}", inventory.describe());
    let triage_report = triage(&root).await;
    let category = options
        .category
        .clone()
        .unwrap_or_else(|| guess_category(&root, &options.target, &triage_report));
    agent.ctf.category = category.clone();
    println!("GUESS   {category}");
    init_notes(&root, options, &scope, &triage_report)?;
    let playbook = skills::match_skill(
        &agent.skills,
        &format!("{} {}", options.target, triage_report),
    )
    .cloned();
    let mut brief = brief(
        options,
        &scope,
        &inventory,
        &category,
        &triage_report,
        playbook.as_ref(),
    );
    // Parallel hypotheses (spec 5.4): cap concurrent delegate_task workers.
    let parallel = options.parallel.max(1);
    brief = format!(
        "Parallelism: run at most {parallel} hypothesis subagent(s) at a time via delegate_task (hypotheses: {category}).\n{brief}"
    );
    let mut stuck = Stuck::default();
    let started = Instant::now();
    let mut steps = 0_usize;
    let mut last_reply: Option<String> = None;
    loop {
        let reply = match agent.turn(&brief).await {
            Ok(reply) => reply,
            Err(error) => {
                crate::ctf::log_error(&root, "ctf autopilot", &format!("{error:#}"));
                return Err(error);
            }
        };
        steps += 1;
        let head: String = reply.trim().chars().take(160).collect();
        println!(
            "[{steps}/{}] {}",
            options.budget.steps,
            head.replace('\n', " ")
        );
        let tokens = agent.usage.input + agent.usage.output;
        let hits = agent.ctf.current_hits();
        if let Some(verified) = verify(&agent.ctf, &hits) {
            println!("✔ flag verified: {} ({})", verified.flag, verified.evidence);
            return Ok(Outcome::Verified {
                flag: verified.flag,
                evidence: verified.evidence,
            });
        }
        // Low progress: an empty reply, or the agent repeating itself.
        let repeats = last_reply.as_deref() == Some(reply.trim());
        let failures = usize::from(reply.trim().is_empty()) + usize::from(repeats);
        last_reply = Some(reply.trim().to_string());
        let nudge_now = stuck.step(failures);
        if nudge_now {
            // Stuck handling (spec 5.2): raise thinking, rotate the category
            // hypothesis, and force a fresh-context attempt.
            agent.thinking_level = agent.thinking_level.max(10);
            agent.ctf.category = CATEGORIES[stuck.hypothesis].to_string();
            let nudge = stuck.nudge(&agent.ctf.category);
            println!("{}", nudge.lines().next().unwrap_or_default());
            append_note(
                &root,
                &format!(
                    "- stuck nudge @ step {steps}: hypothesis {}",
                    CATEGORIES[stuck.hypothesis]
                ),
            );
            brief = format!("{nudge}\n\n{brief}");
        }
        if let Some(reason) = options.budget.exhausted(steps, tokens, started.elapsed()) {
            let report = stop_report(&root, options, steps, tokens, reason);
            // The caller prints the report and any candidate — printing here
            // too would duplicate them on stdout.
            let candidate = candidate_note(&agent.ctf, &hits);
            return Ok(Outcome::Unsolved { report, candidate });
        }
        brief = format!(
            "Continue. {} step(s) used, {} tokens spent, budget {} steps / {} tokens / {} seconds.\n{brief}",
            steps,
            tokens,
            options.budget.steps,
            options.budget.tokens,
            options.budget.seconds,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("wrose-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn budget_parses_forms_and_reports_exhaustion() {
        assert_eq!(Budget::parse("25").unwrap().steps, 25);
        let budget = Budget::parse("steps=4,tokens=100,seconds=60").unwrap();
        assert_eq!((budget.steps, budget.tokens, budget.seconds), (4, 100, 60));
        let budget = Budget::parse("tokens=50").unwrap();
        assert_eq!((budget.steps, budget.tokens), (0, 50));
        assert!(Budget::parse("steps=nan").is_err());
        assert!(Budget::parse("watts=1").is_err());
        assert!(Budget::parse("").is_ok());
        assert_eq!(Budget::parse("unlimited").unwrap(), Budget::default());
        assert_eq!(Budget::default().describe(), "unlimited");
        assert_eq!(
            Budget::default().exhausted(usize::MAX, u64::MAX, Duration::MAX),
            None
        );

        let budget = Budget {
            steps: 2,
            tokens: 10,
            seconds: 1,
        };
        assert_eq!(budget.exhausted(3, 5, Duration::ZERO), Some("steps"));
        assert_eq!(budget.exhausted(1, 99, Duration::ZERO), Some("tokens"));
        assert_eq!(
            budget.exhausted(1, 5, Duration::from_secs(3)),
            Some("seconds")
        );
        assert_eq!(budget.exhausted(1, 5, Duration::ZERO), None);
    }

    #[test]
    fn local_file_preflight_verifies_plain_and_encoded_evidence() {
        let root = scratch("preflight");
        let engine = CtfEngine::new(&root);

        std::fs::write(root.join("plain.txt"), "HTB{fast_local_ok}").unwrap();
        let verified = preflight_local_flag(&engine, &root, "plain.txt").unwrap();
        assert_eq!(verified.flag, "HTB{fast_local_ok}");
        assert!(verified.evidence.contains("plain.txt"));

        std::fs::write(root.join("encoded.txt"), "ZmxhZ3tiZW5jaF9iNjRfb2t9").unwrap();
        let verified = preflight_local_flag(&engine, &root, "encoded.txt").unwrap();
        assert_eq!(verified.flag, "flag{bench_b64_ok}");

        // Realistic packet captures exceed the old 2 MiB fast-path ceiling.
        let mut capture = vec![0_u8; 3 * 1024 * 1024];
        capture.extend_from_slice(b"CTF{large_capture_verified}");
        std::fs::write(root.join("capture.pcapng"), capture).unwrap();
        let verified = preflight_local_flag(&engine, &root, "capture.pcapng").unwrap();
        assert_eq!(verified.flag, "CTF{large_capture_verified}");

        // A flag spanning two read windows is still detected, and the former
        // 16 MiB ceiling no longer excludes otherwise ordinary captures.
        let mut long_capture = vec![0_u8; 17 * 1024 * 1024];
        let offset = 128 * 1024 - 5;
        long_capture[offset..offset + 23].copy_from_slice(b"CTF{window_boundary_ok}");
        std::fs::write(root.join("large.pcapng"), &long_capture).unwrap();
        let verified = preflight_local_flag(&engine, &root, "large.pcapng").unwrap();
        assert_eq!(verified.flag, "CTF{window_boundary_ok}");

        // Decoy/ambiguous artifacts must be handed to the solver instead of
        // claiming that whichever flag appeared first is the true answer.
        std::fs::write(
            root.join("ambiguous.txt"),
            "CTF{first_candidate_ok}\nCTF{second_candidate_ok}",
        )
        .unwrap();
        assert!(preflight_local_flag(&engine, &root, "ambiguous.txt").is_none());

        assert!(preflight_local_flag(&engine, &root, "missing.txt").is_none());
        assert!(preflight_local_flag(&engine, &root, "https://example.org").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn triage_uses_bounded_workers_and_keeps_probe_order() {
        let root = scratch("triage-batch");
        std::fs::write(root.join("challenge.txt"), "not a flag\n").unwrap();

        let report = triage(&root).await;

        assert!(report.starts_with("TRIAGE (4 bounded read-only workers)"));
        let file = report.find("$ file ").expect("file probe");
        let xxd = report.find("$ xxd ").expect("hex probe");
        let strings = report.find("$ strings ").expect("strings probe");
        assert!(file < xxd && xxd < strings, "probe order changed: {report}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scope_covers_roots_and_hosts() {
        let root = scratch("scope");
        let challenge = root.join("chal");
        std::fs::create_dir_all(&challenge).unwrap();
        // An existing file/dir target adds itself to the roots.
        let scope = Scope::for_target(&challenge.display().to_string(), &root, None);
        assert_eq!(scope.roots.len(), 2);
        // A URL target contributes its host instead of a path.
        let scope = Scope::for_target("https://demo.example/chal.zip", &root, None);
        assert_eq!(scope.roots.len(), 1);
        assert!(scope.allows_path(&root, &root.join("notes.md")));
        assert!(scope.allows_path(&root, Path::new("/tmp/scratch.bin")));
        assert!(!scope.allows_path(&root, Path::new("/etc/passwd")));

        assert!(scope.allows_host("demo.example"));
        assert!(scope.allows_host("demo.example:8443"));

        let scope = Scope::for_target(&root.display().to_string(), &root, Some("10.0.0.5:1337"));
        assert!(scope.allows_host("10.0.0.5:1337"));
        assert!(!scope.allows_host("10.0.0.6:1337"));
        assert!(!scope.allows_host("other.example"));
        assert!(scope.describe().contains("10.0.0.5:1337"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shell_scope_flags_only_what_leaves() {
        let root = scratch("shellscope");
        let scope = Scope::for_target(&root.display().to_string(), &root, Some("demo.example"));
        assert_eq!(
            shell_violation(&scope, &root, "curl http://demo.example/api"),
            None
        );
        assert!(shell_violation(&scope, &root, "curl http://evil.example/x").is_some());
        assert!(shell_violation(&scope, &root, "cat notes.md > /etc/hosts").is_some());
        assert_eq!(shell_violation(&scope, &root, "cat notes.md"), None);
        assert_eq!(shell_violation(&scope, &root, "python3 solve.py"), None);
        assert_eq!(shell_violation(&scope, &root, "cp a.txt ./b.txt"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn inventory_reports_missing_tools() {
        let inventory = tool_inventory_from("/definitely/not/a/path");
        assert!(inventory.present.is_empty());
        assert_eq!(inventory.missing.len(), 17);
        assert!(inventory.describe().contains("apt install"));

        let fake = scratch("bin");
        std::fs::write(fake.join("gdb"), b"#!/bin/sh\n").unwrap();
        let inventory = tool_inventory_from(&fake.display().to_string());
        assert_eq!(inventory.present, vec!["gdb".to_string()]);
        assert!(inventory.missing.contains(&"binwalk".to_string()));
        let _ = std::fs::remove_dir_all(&fake);
    }

    #[test]
    fn stuck_rotates_categories_and_nudges() {
        let mut stuck = Stuck::default();
        assert!(!stuck.step(1));
        assert!(!stuck.step(1));
        assert!(stuck.step(1));
        assert_eq!(CATEGORIES[stuck.hypothesis], "web");
        let nudge = stuck.nudge("crypto");
        assert!(nudge.contains("crypto"));
        assert!(nudge.contains("web"));
        assert!(nudge.contains("delegate_task"));
        // A clean step resets the streak.
        assert!(!stuck.step(0));
        assert_eq!(stuck.low_progress, 0);
    }

    #[test]
    fn verify_rederives_file_hits_and_rejects_model_claims() {
        let root = scratch("verify");
        std::fs::write(root.join("chal.txt"), "hello flag{derived_ok} bye").unwrap();
        let hits = vec![FlagHit {
            flag: "flag{derived_ok}".into(),
            source: "file:chal.txt".into(),
            transformation: "plain".into(),
            confidence: 0.9,
        }];
        let engine = CtfEngine::new(&root);
        let verified = verify(&engine, &hits).expect("file hit must verify");
        assert_eq!(verified.flag, "flag{derived_ok}");
        assert!(verified.evidence.contains("chal.txt"));

        let model_only = vec![FlagHit {
            flag: "flag{maybe_fake}".into(),
            source: "model response".into(),
            transformation: "plain".into(),
            confidence: 1.0,
        }];
        assert!(verify(&engine, &model_only).is_none());
        assert!(candidate_note(&engine, &model_only)
            .expect("model claim becomes a candidate")
            .contains("unverified"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `--flag-format` runs must verify with the same regex they detected
    /// with; the default detector would miss `SECRET{…}` entirely.
    #[test]
    fn verify_uses_the_runs_flag_format() {
        let root = scratch("verify-format");
        std::fs::write(root.join("chal.txt"), "wrap SECRET{rotated_ok} end").unwrap();
        let engine = CtfEngine::new(&root)
            .with_flag_format(r"SECRET\{[^}]+\}")
            .expect("valid regex");
        let hits = engine.detect("file:chal.txt", "wrap SECRET{rotated_ok} end");
        assert_eq!(hits.len(), 1, "{hits:?}");
        let verified = verify(&engine, &hits).expect("custom format re-derives");
        assert_eq!(verified.flag, "SECRET{rotated_ok}");

        let default_engine = CtfEngine::new(&root);
        assert!(
            verify(&default_engine, &hits).is_none(),
            "the default pattern cannot read SECRET{{…}}, so it must not verify it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plain_tool_output_is_verified_but_model_text_is_not() {
        let root = scratch("toolverify");
        let hits = vec![FlagHit {
            flag: "HTB{remote_service_evidence}".into(),
            source: "tool:shell-42".into(),
            transformation: "plain".into(),
            confidence: 1.0,
        }];
        let verified = verify(&CtfEngine::new(&root), &hits).expect("real tool output verifies");
        assert_eq!(verified.flag, "HTB{remote_service_evidence}");
        assert!(verified.evidence.contains("tool:shell-42"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decode_chain_hit_verifies_without_the_artifact() {
        let root = scratch("chainverify");
        let hits = vec![FlagHit {
            flag: "flag{hex_chain}".into(),
            source: "tool:decode".into(),
            transformation: "hex".into(),
            confidence: 1.0,
        }];
        let verified = verify(&CtfEngine::new(&root), &hits).expect("decode chain verifies");
        assert_eq!(verified.flag, "flag{hex_chain}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn notes_round_trip_and_category_guess() {
        let root = scratch("notes");
        let options = Options {
            target: "web".into(),
            category: Some("web".into()),
            ..Options::default()
        };
        let scope = Scope::for_target(".", &root, None);
        let path = init_notes(&root, &options, &scope, "ls -la").unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("Dead ends"));
        append_note(&root, "- tried dirbuster, nothing");
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("tried dirbuster"));
        let guess = guess_category(&root, "challenge.pcap", "tcpdump: HTTP GET /admin");
        assert_eq!(guess, "forensics");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn interactive_ctf_parser_keeps_description_and_options() {
        let options = parse_command_args(
            "\"an ELF challenge with a hidden flag\" --category pwn --parallel 4 --budget steps=8,tokens=9000",
        )
        .unwrap();
        assert_eq!(options.target, "an ELF challenge with a hidden flag");
        assert_eq!(options.category.as_deref(), Some("pwn"));
        assert_eq!(options.parallel, 4);
        assert_eq!(options.budget.steps, 8);
        assert_eq!(options.budget.tokens, 9000);
    }

    #[test]
    fn interactive_ctf_parser_rejects_unknown_options() {
        let error = parse_command_args("challenge --wat")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown flag --wat"), "{error}");
    }
}
