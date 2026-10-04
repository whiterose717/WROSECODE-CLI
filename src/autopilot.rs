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
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the user supplied for this challenge: files (challenge artifacts and
/// the working directory) and hosts (`--remote`, or the target URL). Anything
/// outside it needs explicit approval before a tool may touch it.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    roots: Vec<PathBuf>,
    hosts: Vec<String>,
}

impl Scope {
    /// Build the allowlist from the challenge target: an existing file or
    /// directory becomes a root (alongside the session working directory),
    /// a URL contributes its host, and `--remote host:port` adds a host.
    pub fn for_target(target: &str, root: &Path, remote: Option<&str>) -> Self {
        let mut roots = vec![root.to_path_buf()];
        let path = Path::new(target);
        if path.exists() {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
        let mut hosts = BTreeSet::new();
        if let Some(host) = url_host(target) {
            hosts.insert(host);
        }
        if let Some(remote) = remote {
            let normalized = remote
                .trim()
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            if !normalized.is_empty() {
                hosts.insert(normalized.to_ascii_lowercase());
            }
        }
        Self {
            roots,
            hosts: hosts.into_iter().collect(),
        }
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
        format!("files {files} · hosts {hosts}")
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
    /// entry without a port matches any port on that host.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('/').to_ascii_lowercase();
        if host.is_empty() {
            return true;
        }
        let (name, port) = match host.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
                (name.to_string(), Some(port.to_string()))
            }
            _ => (host.clone(), None),
        };
        self.hosts.iter().any(|allowed| {
            if let Some((_, allowed_port)) = allowed.rsplit_once(':') {
                if allowed_port.chars().all(|c| c.is_ascii_digit()) {
                    return allowed == &host;
                }
            }
            allowed == &name || (allowed == &host && port.is_none())
        })
    }
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

/// How far the autopilot may push before it must stop and report:
/// model turns, total tokens, and wall time. `steps=…,tokens=…,seconds=…`
/// or a bare number for steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub steps: usize,
    pub tokens: u64,
    pub seconds: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            steps: 40,
            tokens: 300_000,
            seconds: 1_800,
        }
    }
}

impl Budget {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
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
    let mut lines = vec!["TRIAGE (one parallel batch)".to_string()];
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
    lines.push(run_triage(root, "ls -la", &inventory).await);
    if entries.is_empty() {
        lines.push(run_triage(root, "ls -la", &inventory).await);
    }
    for path in &entries {
        let name = shell_quote(&path.display().to_string());
        for command in [
            format!("file {name}"),
            format!("xxd {name} | head -n 8"),
            format!("strings -n 8 {name} | head -n 30"),
        ] {
            lines.push(run_triage(root, &command, &inventory).await);
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match extension.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "wav" | "mp3" => {
                lines.push(
                    run_triage(root, &format!("exiftool {name} | head -n 20"), &inventory).await,
                );
            }
            "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" => {
                lines.push(
                    run_triage(root, &format!("unzip -l {name} | head -n 20"), &inventory).await,
                );
            }
            _ => {}
        }
        if inventory.present.iter().any(|tool| tool == "binwalk") {
            lines.push(run_triage(root, &format!("binwalk {name}"), &inventory).await);
        }
        if inventory.present.iter().any(|tool| tool == "checksec") {
            lines.push(run_triage(root, &format!("checksec --file={name}"), &inventory).await);
        }
    }
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
/// * `tool:` hits whose transformation is a decode chain (`hex`, `base64`,
///   `rot13`, …) are accepted — the chain itself is the evidence recorded in
///   `.ctf/flags.log`.
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
        let derived = match hit.source.strip_prefix("file:") {
            Some(relative) => std::fs::read_to_string(root.join(relative)).is_ok_and(|text| {
                engine
                    .detect(&hit.source, &text)
                    .iter()
                    .any(|found| found.flag == hit.flag)
            }),
            None => hit.source.starts_with("tool:") && hit.transformation != "plain",
        };
        if derived {
            from_artifact.insert(hit.flag.as_str());
        }
    }
    for hit in hits {
        if from_artifact.contains(hit.flag.as_str()) {
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
        "CTF AUTOPILOT — keep working until a flag is verified or the budget stops you.\n\
         Target: `{}`\n\
         Category hypothesis: {category}\n\
         Flag format: {}\n\
         Scope (only these files and hosts; anything else must be approved): {}\n\
         Tools: {}\n\
         {playbook_block}\
         Rules:\n\
         1. No check-ins: work autonomously inside the scope. Ask only for out-of-scope network,\n\
            destructive host actions, or input only the user can supply.\n\
         2. Record every dead end in ctf-notes.md so nothing is retried; re-read it when stuck.\n\
         3. Any string matching the flag format is a candidate — verify it (re-derive the decode\n\
            chain, re-run the extraction, or test it against the remote service) before claiming success.\n\
         4. Fork independent hypotheses with `delegate_task` (clean context each) and use the\n\
            `decode` tool for encoding sweeps.\n\
         TRIAGE:\n```text\n{triage_report}\n```\n",
        options.target,
        options
            .flag_format
            .clone()
            .unwrap_or_else(|| "flag{{…}} / CTF{{…}} / HTB{{…}} / picoCTF{{…}}".into()),
        scope.describe(),
        inventory.describe(),
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
    agent.ctf.lock_category();
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
        let hits = agent.ctf.recorded_hits();
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
        assert_eq!((budget.steps, budget.tokens), (Budget::default().steps, 50));
        assert!(Budget::parse("steps=nan").is_err());
        assert!(Budget::parse("watts=1").is_err());
        assert!(Budget::parse("").is_ok());

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
}
