//! Evidence-backed bug-bounty workflow.
//!
//! The point of this module is not to produce more findings; it is to stop
//! unverified ones from ever looking reportable. A finding moves through
//! `potential → investigating → reproduced → verified → reported`, and every
//! transition that claims reproduction requires request/response evidence
//! stored on disk with a SHA-256 hash. Reports are generated only from
//! verified findings, and generation re-hashes every evidence file first.
//!
//! The scope allowlist is the same [`crate::autopilot::Scope`] the CTF
//! autopilot uses, extended with bug-bounty wildcard hosts and exclusions, so
//! every `http`, `web_fetch`, `browser_capture`, and network shell call is
//! checked by the existing tool gate.

use crate::agent::Agent;
use crate::autopilot::{host_entry, Budget, Scope};
use anyhow::{bail, Context, Result};
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const WORKSPACE_DIR: &str = ".bugbounty";
pub const STATES: [&str; 5] = [
    "potential",
    "investigating",
    "reproduced",
    "verified",
    "reported",
];
pub const EVIDENCE_KINDS: [&str; 4] = ["request", "response", "screenshot", "note"];
pub const SEVERITIES: [&str; 5] = ["info", "low", "medium", "high", "critical"];
const MAX_EVIDENCE_BYTES: usize = 2 * 1024 * 1024;

/// How invasive a tool action is under bug-bounty doctrine. Passive actions
/// stay on the local workspace; LowImpact and Elevated reach a remote asset
/// and therefore require an explicitly armed scope; Prohibited actions are
/// outside the evidence-and-scope model entirely.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ActionClass {
    Passive,
    LowImpact,
    Elevated,
    Prohibited,
}

impl ActionClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passive => "passive",
            Self::LowImpact => "low_impact",
            Self::Elevated => "elevated",
            Self::Prohibited => "prohibited",
        }
    }

    /// Remote actions need a program scope to be armed first. Local state
    /// changes do not; an unknown/future tool is Elevated and therefore also
    /// needs the scope.
    pub fn requires_scope(self) -> bool {
        matches!(self, Self::LowImpact | Self::Elevated)
    }
}

fn http_method(input: &Value) -> String {
    input["method"]
        .as_str()
        .unwrap_or("GET")
        .trim()
        .to_ascii_uppercase()
}

/// Classify a tool call for bug-bounty enforcement. The default is Elevated,
/// not Passive: a newly added tool cannot silently gain unattended network
/// access until this table is updated. Local file edits remain Passive so the
/// workspace and report flow keep working with no target armed.
pub fn classify_action(name: &str, input: &Value) -> ActionClass {
    match name {
        "read_file" | "grep" | "glob" | "decode" | "bugbounty" | "update_plan" | "coverage"
        | "write_file" | "edit_file" | "search_replace" | "apply_patch" | "archive_writeup" => {
            ActionClass::Passive
        }
        "shell" => match input["command"].as_str() {
            Some(command) if crate::tools::shell::destructive(command) => ActionClass::Prohibited,
            Some(command) if crate::tools::shell::read_only(command) => ActionClass::Passive,
            Some(_) => ActionClass::Elevated,
            None => ActionClass::Elevated,
        },
        "http" | "web_fetch" | "burp_export" | "web_search" | "search_writeups" => {
            match http_method(input).as_str() {
                // `web_search`/`search_writeups` use GET semantics against a
                // public index, not the target; still low-impact and scope-gated
                // so a bug-bounty run never quietly browses.
                "GET" | "HEAD" | "OPTIONS" => ActionClass::LowImpact,
                _ => ActionClass::Elevated,
            }
        }
        // Desktop automation and arbitrary MCP servers can act outside any
        // request/response evidence and outside the armed host scope.
        // Playwright screenshot CLI can follow cross-host redirects and load
        // third-party resources without a per-request scope interceptor.
        // Do not give the autonomous bounty agent that bypass by default.
        "computer" | "mcp" | "browser_capture" => ActionClass::Prohibited,
        "delegate_task" => ActionClass::Elevated,
        _ => ActionClass::Elevated,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub target: String,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub scope_file: Option<PathBuf>,
    pub program: String,
    pub budget: Budget,
    pub parallel: usize,
}

impl Options {
    pub fn with_default_budget() -> Self {
        Self {
            budget: Budget {
                steps: 20,
                tokens: 400_000,
                seconds: 1_800,
            },
            parallel: 2,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScopeRecord {
    #[serde(default)]
    pub includes: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub roots: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub kind: String,
    /// Workspace-relative path, so a moved project keeps working.
    pub file: String,
    pub sha256: String,
    #[serde(default)]
    pub note: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub asset: String,
    pub category: String,
    pub severity: String,
    #[serde(default)]
    pub severity_reason: String,
    #[serde(default)]
    pub cwe: String,
    #[serde(default)]
    pub wstg_id: String,
    #[serde(default)]
    pub asvs_id: String,
    #[serde(default)]
    pub cvss_vector: String,
    pub state: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub observed: String,
    #[serde(default)]
    pub expected: String,
    #[serde(default)]
    pub impact: String,
    #[serde(default)]
    pub remediation: String,
    #[serde(default)]
    pub scope_confirmed: bool,
    #[serde(default)]
    pub scope_note: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub reproduced_at: Option<u64>,
    #[serde(default)]
    pub verified_at: Option<u64>,
    #[serde(default)]
    pub reported_at: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub program: String,
    pub target: String,
    pub scope: ScopeRecord,
    pub findings: Vec<Finding>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ReportOutcome {
    pub report: Vec<PathBuf>,
    pub verified: usize,
    pub reproduced: usize,
    pub potential: usize,
    pub steps: usize,
    pub tokens: u64,
    pub stop_reason: String,
}

impl Workspace {
    pub fn dir(root: &Path) -> PathBuf {
        root.join(WORKSPACE_DIR)
    }

    pub fn path(root: &Path) -> PathBuf {
        Self::dir(root).join("findings.json")
    }

    pub fn evidence_dir(root: &Path, id: &str) -> PathBuf {
        Self::dir(root).join("evidence").join(id)
    }

    pub fn reports_dir(root: &Path) -> PathBuf {
        Self::dir(root).join("reports")
    }

    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = Self::path(root);
        if !path.exists() {
            return Ok(None);
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let workspace: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        workspace.validate()?;
        Ok(Some(workspace))
    }

    pub fn load_or_init(root: &Path, program: &str, target: &str, scope: &Scope) -> Result<Self> {
        let mut workspace = Self::load(root)?.unwrap_or_else(|| {
            let now = now();
            Self {
                program: program.to_string(),
                target: target.to_string(),
                scope: ScopeRecord::from_scope(scope),
                findings: Vec::new(),
                created_at: now,
                updated_at: now,
            }
        });
        if !program.trim().is_empty() {
            workspace.program = program.trim().to_string();
        } else if workspace.program.trim().is_empty() {
            workspace.program = "authorized-engagement".into();
        }
        if !target.trim().is_empty() {
            workspace.target = target.trim().to_string();
        }
        // Re-issuing `/bugbounty` with a narrower scope must narrow the stored
        // authorization, never inherit a previously wider one.
        workspace.scope = ScopeRecord::from_scope(scope);
        // Invalidated prior proof when the engagement scope narrows.
        for finding in &mut workspace.findings {
            if check_asset(scope, &finding.asset).is_err() {
                finding.scope_confirmed = false;
                finding.scope_note = "outside the currently armed program scope".into();
                if finding.state == "verified" {
                    finding.state = "reproduced".into();
                    finding.verified_at = None;
                }
            }
        }
        workspace.touch();
        fs::create_dir_all(Self::reports_dir(root))
            .with_context(|| format!("creating {}", Self::reports_dir(root).display()))?;
        secure_dir(&Self::dir(root))?;
        workspace.save(root)?;
        Ok(workspace)
    }

    pub fn save(&mut self, root: &Path) -> Result<()> {
        self.validate()?;
        self.touch();
        let text = serde_json::to_string_pretty(self)?;
        write_private(&Self::path(root), text.as_bytes())?;
        Ok(())
    }

    fn touch(&mut self) {
        self.updated_at = now();
    }

    fn validate(&self) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        for finding in &self.findings {
            if !ids.insert(finding.id.clone()) {
                bail!("duplicate finding id {}", finding.id);
            }
            if !STATES.contains(&finding.state.as_str()) {
                bail!("finding {} has invalid state {}", finding.id, finding.state);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn find(&self, id: &str) -> Option<&Finding> {
        self.findings
            .iter()
            .find(|finding| finding.id.eq_ignore_ascii_case(id.trim()))
    }

    pub fn find_mut(&mut self, id: &str) -> Result<&mut Finding> {
        let id = id.trim().to_string();
        self.findings
            .iter_mut()
            .find(|finding| finding.id.eq_ignore_ascii_case(&id))
            .with_context(|| format!("unknown finding `{id}`"))
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |state: &str| {
            self.findings
                .iter()
                .filter(|finding| finding.state == state)
                .count()
        };
        (
            count("verified") + count("reported"),
            count("reproduced"),
            count("potential") + count("investigating"),
        )
    }

    pub fn status(&self, root: &Path) -> String {
        let (verified, reproduced, open) = self.counts();
        let mut out = format!(
            "Bug bounty: {}\nTarget: {}\nScope: {}\nWorkspace: {}\nFindings: {} open · {} reproduced · {} verified/reported\n",
            if self.program.is_empty() {
                "authorized-engagement"
            } else {
                &self.program
            },
            if self.target.is_empty() {
                "(not set)"
            } else {
                &self.target
            },
            describe_scope(&self.scope),
            Self::path(root).display(),
            open,
            reproduced,
            verified,
        );
        if self.findings.is_empty() {
            out.push_str("No findings recorded yet. Use the `bugbounty` tool (action=add) only after observing a concrete issue.\n");
        } else {
            for finding in &self.findings {
                out.push_str(&format!(
                    "  {} [{}] {} — {} ({})\n",
                    finding.id,
                    finding.state,
                    finding.severity,
                    crate::crash::redact(&finding.title),
                    crate::crash::redact(&finding.asset)
                ));
            }
        }
        out
    }

    pub fn next_id(&self) -> String {
        let highest = self
            .findings
            .iter()
            .filter_map(|finding| finding.id.strip_prefix("BB-"))
            .filter_map(|number| number.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        format!("BB-{:04}", highest + 1)
    }

    fn add(&mut self, input: &Value, scope: Option<&Scope>) -> Result<String> {
        let title = required(input, "title")?;
        let asset = required(input, "asset")?;
        if let Some(scope) = scope {
            check_asset(scope, asset)?;
        }
        let severity = input
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or("medium")
            .to_ascii_lowercase();
        check_severity(&severity)?;
        let now = now();
        let id = self.next_id();
        let finding = Finding {
            id: id.clone(),
            title: title.to_string(),
            asset: asset.to_string(),
            category: input
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("unclassified")
                .to_string(),
            severity,
            severity_reason: input["severity_reason"].as_str().unwrap_or_default().into(),
            cwe: input["cwe"].as_str().unwrap_or_default().into(),
            wstg_id: input["wstg_id"].as_str().unwrap_or_default().into(),
            asvs_id: input["asvs_id"].as_str().unwrap_or_default().into(),
            cvss_vector: input["cvss_vector"].as_str().unwrap_or_default().into(),
            state: "potential".into(),
            summary: input
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            steps: Vec::new(),
            observed: String::new(),
            expected: String::new(),
            impact: String::new(),
            remediation: String::new(),
            scope_confirmed: false,
            scope_note: String::new(),
            evidence: Vec::new(),
            created_at: now,
            updated_at: now,
            reproduced_at: None,
            verified_at: None,
            reported_at: None,
        };
        self.findings.push(finding);
        Ok(format!(
            "Recorded {id} as potential (not verified). Next: collect request/response evidence, then reproduce."
        ))
    }

    fn update(&mut self, input: &Value, scope: Option<&Scope>) -> Result<String> {
        let id = required(input, "finding_id")?.to_string();
        let finding = self.find_mut(&id)?;
        if finding.state == "reported" {
            bail!("{id} is already reported; create a new finding for a different issue");
        }
        if let Some(new_asset) = input.get("asset").and_then(Value::as_str) {
            if new_asset != finding.asset {
                if !finding.evidence.is_empty()
                    || !matches!(finding.state.as_str(), "potential" | "investigating")
                {
                    bail!("cannot change a finding asset after evidence or reproduction; create a new finding to preserve proof provenance");
                }
                let scope = scope.context("changing a finding asset requires an armed scope")?;
                check_asset(scope, new_asset)?;
                finding.asset = new_asset.to_owned();
                finding.scope_confirmed = false;
                finding.scope_note.clear();
            }
        }
        for (key, field) in [
            ("title", &mut finding.title),
            ("category", &mut finding.category),
            ("summary", &mut finding.summary),
            ("impact", &mut finding.impact),
            ("remediation", &mut finding.remediation),
            ("severity_reason", &mut finding.severity_reason),
            ("cwe", &mut finding.cwe),
            ("wstg_id", &mut finding.wstg_id),
            ("asvs_id", &mut finding.asvs_id),
            ("cvss_vector", &mut finding.cvss_vector),
        ] {
            if let Some(value) = input.get(key).and_then(Value::as_str) {
                *field = value.to_string();
            }
        }
        if let Some(severity) = input.get("severity").and_then(Value::as_str) {
            let severity = severity.to_ascii_lowercase();
            check_severity(&severity)?;
            finding.severity = severity;
        }
        finding.updated_at = now();
        Ok(format!("Updated {id}."))
    }

    fn investigate(&mut self, id: &str) -> Result<String> {
        let finding = self.find_mut(id)?;
        match finding.state.as_str() {
            "potential" | "investigating" => {
                finding.state = "investigating".into();
                finding.updated_at = now();
                Ok(format!("{id}: investigating."))
            }
            state => bail!("{id} is {state}; investigating only moves potential → investigating"),
        }
    }

    fn evidence(&mut self, root: &Path, input: &Value) -> Result<String> {
        let id = required(input, "finding_id")?.to_string();
        let kind = required(input, "kind")?.to_ascii_lowercase();
        if !EVIDENCE_KINDS.contains(&kind.as_str()) {
            bail!("kind must be one of: {}", EVIDENCE_KINDS.join(", "));
        }
        let content = required(input, "content")?;
        if content.len() > MAX_EVIDENCE_BYTES {
            bail!(
                "evidence is {} bytes; the cap is {MAX_EVIDENCE_BYTES} bytes. Save the full artifact outside the report and record its path in note.",
                content.len()
            );
        }
        let finding = self.find_mut(&id)?;
        if finding.state == "reported" {
            bail!("{id} is already reported; evidence is frozen");
        }
        let dir = Workspace::evidence_dir(root, &finding.id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        secure_dir(&dir)?;
        let supplied = input
            .get("filename")
            .and_then(Value::as_str)
            .map(sanitize_filename)
            .filter(|name| !name.is_empty());
        let sequence = finding.evidence.len() + 1;
        let filename = supplied.unwrap_or_else(|| {
            let extension = match kind.as_str() {
                "screenshot" => "png",
                _ => "txt",
            };
            format!("{sequence:02}-{kind}.{extension}")
        });
        let mut path = dir.join(&filename);
        if path.exists() {
            path = dir.join(format!("{sequence:02}-{filename}"));
        }
        write_private(&path, content.as_bytes())?;
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let evidence = Evidence {
            id: format!("{}-E{:02}", finding.id, sequence),
            kind,
            file: relative,
            sha256: sha256_hex(content.as_bytes()),
            note: input
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            created_at: now(),
        };
        let evidence_id = evidence.id.clone();
        let hash = evidence.sha256.clone();
        finding.evidence.push(evidence);
        if finding.state == "potential" {
            finding.state = "investigating".into();
        }
        finding.updated_at = now();
        Ok(format!(
            "Evidence {evidence_id} recorded at {} (sha256 {hash}).",
            path.display()
        ))
    }

    fn reproduce(&mut self, input: &Value) -> Result<String> {
        let id = required(input, "finding_id")?.to_string();
        let steps = parse_steps(input.get("steps"))?;
        let observed = required(input, "observed")?.trim().to_string();
        let expected = required(input, "expected")?.trim().to_string();
        if observed.is_empty() || expected.is_empty() {
            bail!("reproduce needs non-empty observed and expected results");
        }
        let finding = self.find_mut(&id)?;
        if !matches!(finding.state.as_str(), "potential" | "investigating") {
            bail!(
                "{} is {}; reproduce only moves potential/investigating → reproduced",
                finding.id,
                finding.state
            );
        }
        let problems = evidence_problems(finding, None);
        if !problems.is_empty() {
            bail!("cannot mark reproduced: {}", problems.join("; "));
        }
        finding.steps = steps;
        finding.observed = observed;
        finding.expected = expected;
        finding.state = "reproduced".into();
        finding.reproduced_at = Some(now());
        finding.updated_at = now();
        Ok(format!(
            "{id}: reproduced. Next: confirm scope and run action=verify (verification re-hashes evidence)."
        ))
    }

    fn confirm_scope(&mut self, input: &Value, scope: Option<&Scope>) -> Result<String> {
        let id = required(input, "finding_id")?.to_string();
        let in_scope = input
            .get("in_scope")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow::anyhow!("scope action needs in_scope: true|false"))?;
        let finding = self.find_mut(&id)?;
        if !in_scope {
            finding.scope_confirmed = false;
            finding.scope_note = input
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or("marked out of scope")
                .to_string();
            finding.updated_at = now();
            return Ok(format!("{id}: scope NOT confirmed; it cannot be verified."));
        }
        let effective = scope.ok_or_else(|| {
            anyhow::anyhow!("no program scope is armed; rerun /bugbounty with --scope/--scope-file")
        })?;
        check_asset(effective, &finding.asset)?;
        finding.scope_confirmed = true;
        finding.scope_note = input
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("asset matched the armed program scope")
            .to_string();
        finding.updated_at = now();
        Ok(format!(
            "{id}: asset confirmed in scope. Verification is now possible."
        ))
    }

    fn verify(&mut self, root: &Path, id: &str) -> Result<String> {
        let active_scope = Scope::for_bounty(root, "", &self.scope.includes, &self.scope.excludes);
        let finding = self.find_mut(id)?;
        check_asset(&active_scope, &finding.asset)?;
        if finding.state != "reproduced" {
            bail!(
                "{} is {}; only reproduced findings can be verified",
                finding.id,
                finding.state
            );
        }
        if !finding.scope_confirmed {
            bail!("{} has not had its asset confirmed in scope", finding.id);
        }
        if finding.steps.is_empty() || finding.observed.trim().is_empty() {
            bail!(
                "{} has no reproduction steps or observed result",
                finding.id
            );
        }
        if finding.expected.trim().is_empty() || finding.impact.trim().is_empty() {
            bail!(
                "{} needs both expected behavior and security impact before verification",
                finding.id
            );
        }
        let problems = evidence_problems(finding, Some(root));
        if !problems.is_empty() {
            bail!("verification failed: {}", problems.join("; "));
        }
        finding.state = "verified".into();
        finding.verified_at = Some(now());
        finding.updated_at = now();
        Ok(format!(
            "{id}: VERIFIED with {} evidence file(s). Report generation will re-hash them.",
            finding.evidence.len()
        ))
    }

    fn report(&mut self, root: &Path) -> Result<Vec<PathBuf>> {
        let active_scope = Scope::for_bounty(root, "", &self.scope.includes, &self.scope.excludes);
        let candidates: Vec<usize> = self
            .findings
            .iter()
            .enumerate()
            .filter(|(_, finding)| matches!(finding.state.as_str(), "verified" | "reported"))
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            bail!("no verified findings; refusing to generate a speculative report");
        }
        for index in &candidates {
            let finding = &self.findings[*index];
            if !finding.scope_confirmed {
                bail!(
                    "{} cannot be reported: current program scope is not confirmed",
                    finding.id
                );
            }
            check_asset(&active_scope, &finding.asset)?;
            let problems = evidence_problems(finding, Some(root));
            if !problems.is_empty() {
                bail!(
                    "{} cannot be reported: {}",
                    self.findings[*index].id,
                    problems.join("; ")
                );
            }
        }
        let dir = Workspace::reports_dir(root);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        secure_dir(&dir)?;
        let mut paths = Vec::new();
        for index in &candidates {
            let finding = &self.findings[*index];
            let path = dir.join(format!("{}-{}.md", finding.id, slug(&finding.title, 48)));
            write_private(
                &path,
                finding_markdown(finding, root, &self.program)?.as_bytes(),
            )?;
            paths.push(path);
        }
        let index = dir.join("BUG_BOUNTY_REPORT.md");
        write_private(
            &index,
            workspace_markdown(self, root, &candidates)?.as_bytes(),
        )?;
        paths.insert(0, index);
        let stamp = now();
        for index in candidates {
            let finding = &mut self.findings[index];
            if finding.state == "verified" {
                finding.state = "reported".into();
                finding.reported_at = Some(stamp);
                finding.updated_at = stamp;
            }
        }
        Ok(paths)
    }
}

impl ScopeRecord {
    fn from_scope(scope: &Scope) -> Self {
        Self {
            includes: scope.allowed_hosts().to_vec(),
            excludes: scope.excluded_hosts().to_vec(),
            roots: scope
                .roots()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        }
    }
}

/// Parse `/bugbounty <target> [--scope HOST[,HOST]] [--scope-file PATH]
/// [--exclude HOST] [--program NAME] [--budget …] [--parallel N]`.
pub fn parse_command_args(args: &str) -> Result<Options> {
    let tokens = tokenize(args)?;
    let mut options = Options::with_default_budget();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        let mut value = |name: &str| -> Result<String> {
            index += 1;
            tokens
                .get(index)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match token.as_str() {
            "--scope" | "--in-scope" => {
                let value = value(token)?;
                for entry in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if host_entry(entry).is_none() {
                        bail!("`{entry}` is not a host, URL, or * wildcard");
                    }
                    options.includes.push(entry.to_string());
                }
            }
            "--exclude" | "--exclude-scope" => {
                let value = value(token)?;
                for entry in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if host_entry(entry).is_none() {
                        bail!("`{entry}` is not a host, URL, or * wildcard");
                    }
                    options.excludes.push(entry.to_string());
                }
            }
            "--scope-file" => options.scope_file = Some(PathBuf::from(value(token)?)),
            "--program" => options.program = value(token)?,
            "--budget" => {
                options.budget = Budget::parse(&value(token)?)?;
            }
            "--parallel" => {
                let parsed: usize = value(token)?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--parallel needs a number"))?;
                options.parallel = parsed.clamp(1, 8);
            }
            "--help" | "-h" => {
                bail!(
                    "usage: /bugbounty [target] [--scope HOST[,HOST]] [--scope-file PATH] \
                     [--exclude HOST] [--program NAME] [--budget STEPS|KEY=VALUE,…] [--parallel N]"
                );
            }
            other if other.starts_with("--") => bail!("unknown /bugbounty option `{other}`"),
            other => {
                if !options.target.is_empty() {
                    bail!("unexpected extra target `{other}` (quote a description or use --scope)");
                }
                options.target = other.to_string();
            }
        }
        index += 1;
    }
    finalize(options)
}

/// Shared post-processing for `/bugbounty …` and `wrosecode bugbounty …`:
/// split comma-separated entries, validate every host, merge the scope file,
/// and refuse a live run with nothing in scope.
pub fn finalize(mut options: Options) -> Result<Options> {
    options.target = options.target.trim().to_string();
    options.includes = split_entries(std::mem::take(&mut options.includes))?;
    options.excludes = split_entries(std::mem::take(&mut options.excludes))?;
    options.program = options.program.trim().to_string();
    options.parallel = options.parallel.clamp(1, 8);
    if let Some(path) = options.scope_file.clone() {
        let (includes, excludes) = read_scope_file(&path)?;
        options.includes.extend(includes);
        options.excludes.extend(excludes);
    }
    options.includes.sort();
    options.includes.dedup();
    options.excludes.sort();
    options.excludes.dedup();
    if options.target.is_empty() && options.includes.is_empty() {
        bail!("bug bounty needs an in-scope host (--scope), --scope-file, or a local path target");
    }
    if options.program.is_empty() {
        options.program = "authorized-engagement".into();
    }
    Ok(options)
}

/// URL parsers normalize default ports away (https:443, http:80). In
/// security scope entries this is unsafe: explicitly scoped port 443 must
/// not become an unrestricted host entry permitting every port.
fn explicit_authority_port(input: &str) -> Option<u16> {
    let without_scheme = input
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(input);
    let authority = without_scheme.split(['/', '?', '#']).next()?;
    let (_, candidate) = authority.rsplit_once(':')?;
    candidate.parse::<u16>().ok()
}

/// Explicit scope entries must not silently authorize all paths of a host
/// when the program only specified a path. Require host-only HTTP(S).
fn checked_scope_host(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value == "*" {
        bail!("scope requires explicit host(s); unrestricted wildcard is unsafe");
    }
    let wildcard = value.starts_with("*.");
    let parsed = if wildcard { &value[2..] } else { value };
    if parsed.contains(char::is_whitespace) {
        bail!("scope entry contains whitespace");
    }
    let url = if parsed.contains("://") {
        url::Url::parse(parsed)?
    } else {
        url::Url::parse(&format!("https://{parsed}"))?
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("scope entry must be a host-only HTTP(S) URL, host, or host:port");
    }
    let host = url.host_str().unwrap_or_default();
    if host.contains('*') {
        bail!("invalid scope wildcard");
    }
    let authority = match explicit_authority_port(parsed).or_else(|| url.port()) {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    if wildcard {
        Ok(format!("*.{authority}"))
    } else {
        Ok(authority)
    }
}

fn checked_asset_host(asset: &str) -> Result<String> {
    let asset = asset.trim();
    if asset == "*" || asset.starts_with("*.") {
        bail!("finding asset must be a concrete host");
    }
    let url = if asset.contains("://") {
        url::Url::parse(asset)?
    } else {
        url::Url::parse(&format!("https://{asset}"))?
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("finding asset requires an HTTP(S) host without credentials");
    }
    let host = url.host_str().unwrap_or_default();
    if host.contains('*') {
        bail!("finding asset must be a concrete host");
    }
    Ok(
        match explicit_authority_port(asset).or_else(|| url.port()) {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        },
    )
}

fn split_entries(entries: Vec<String>) -> Result<Vec<String>> {
    let mut split = Vec::new();
    for entry in entries {
        for part in entry.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            split.push(checked_scope_host(part)?);
        }
    }
    Ok(split)
}

/// One include/exclude per line. `include host` / `exclude host` prefixes are
/// supported; bare lines are includes; `#` starts a comment.
pub fn read_scope_file(path: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading scope file {}", path.display()))?;
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let (bucket, entry) = if let Some(rest) = line
            .strip_prefix("include ")
            .or_else(|| line.strip_prefix("in-scope "))
            .or_else(|| line.strip_prefix("scope "))
        {
            (&mut includes, rest.trim())
        } else if let Some(rest) = line.strip_prefix("exclude ") {
            (&mut excludes, rest.trim())
        } else {
            (&mut includes, line)
        };
        let parsed = checked_scope_host(entry)
            .with_context(|| format!("scope file {}:{}", path.display(), number + 1))?;
        bucket.push(parsed);
    }
    if includes.is_empty() && excludes.is_empty() {
        bail!("scope file {} has no entries", path.display());
    }
    if includes.is_empty() {
        bail!(
            "scope file {} only contains exclusions; an allowlist is still required",
            path.display()
        );
    }
    Ok((includes, excludes))
}

/// Build the program scope and refuse a live run with no allowlisted hosts.
pub fn build_scope(root: &Path, options: &Options) -> Result<Scope> {
    let local_target = !options.target.trim().is_empty() && Path::new(&options.target).exists();
    // A local challenge directory is its own authorization. A live target is
    // not: the operator must name the program hosts explicitly (directly or
    // through --scope-file) so nothing outside the program is ever probed.
    if !local_target && options.includes.is_empty() {
        bail!(
            "no in-scope hosts. Pass `--scope app.example.com` (or `--scope-file`) so every \
             request is bounded by the program's allowlist"
        );
    }
    let scope = Scope::for_bounty(
        root,
        &options.target,
        options.includes.iter().map(String::as_str),
        options.excludes.iter().map(String::as_str),
    );
    if scope.allowed_hosts().is_empty() && !local_target {
        bail!(
            "no in-scope hosts. Pass `--scope app.example.com` (or `--scope-file`) so every \
             request is bounded by the program's allowlist"
        );
    }
    if !local_target && !options.target.is_empty() {
        check_asset(&scope, &options.target)?;
    }
    Ok(scope)
}

/// The system-prompt addendum for bug-bounty mode.
pub fn doctrine() -> &'static str {
    "BUG BOUNTY MODE: operate only inside the armed program scope and use non-destructive validation. \
     A suspicion is not a finding and a finding is not verified until the `bugbounty` tool records it. \
     Collect request/response evidence, reproduce with the actual observed result, confirm the asset is in scope, \
     then verify. Never edit .bugbounty/findings.json directly, never invent responses, and never describe a \
     finding as platform-accepted unless acceptance was observed. Reports are generated only from verified findings.\n"
}

/// The first-turn brief. It is deliberately concrete about the state machine
/// and the refuse-to-report rule so weak models cannot skip the gates.
pub fn brief(options: &Options, scope: &Scope, workspace: &Workspace, root: &Path) -> String {
    let (verified, reproduced, open) = workspace.counts();
    format!(
        "BUG BOUNTY AUTOPILOT\n\
         Program: {program}\n\
         Target: {target}\n\
         Scope: {scope}\n\
         Workspace: {workspace}\n\
         Budget: {budget} · parallel hypotheses: {parallel}\n\
         Findings now: {open} open · {reproduced} reproduced · {verified} verified/reported\n\n\
         RULES\n\
         - Every network action must stay inside the scope above. The tool gate denies anything else.\n\
         - Prefer GET/HEAD and read-only probes. Do not modify production data, brute force, run DoS, or evade rate limits.\n\
         - Use only accounts and credentials the user supplied. Never create accounts or test third-party users.\n\
         - Rate-limit yourself to at most 2 requests/second per host. Stop on 429 and back off.\n\
         - A finding is only `verified` after `bugbounty(action=\"verify\")` succeeds. Model prose is not evidence.\n\
         - Redact cookies, Authorization values, tokens, and personal data from notes and reports.\n\n\
         WORKFLOW\n\
         1. Call bugbounty(action=assessment_plan) once to create the offline WSTG/ASVS test matrix; map only in-scope assets.\n\
         2. Form ranked hypotheses (access control, auth/session, injection, SSRF, data exposure, business logic).\n\
         3. For each hypothesis, run the smallest non-destructive test that distinguishes true from false.\n\
         4. On a real issue: `bugbounty(add)` → `bugbounty(evidence, kind=\"request\")` → `(kind=\"response\")` →\n\
            `reproduce` → `scope(in_scope=true)` → `verify` → `report`.\n\
         5. Record dead ends in .bugbounty/notes.md so a later turn does not repeat them.\n\
         6. When the scope is exhausted or the budget is close, call `bugbounty(action=\"report\")` and end with\n\
            the exact line `BOUNTY COMPLETE` plus a short verified/potential summary.\n\n\
         If no vulnerability is verified, say so plainly and list the strongest unverified hypotheses; do not inflate them.\n\
         Workspace path: {workspace_dir}",
        program = if options.program.is_empty() {
            "authorized-engagement"
        } else {
            &options.program
        },
        target = if options.target.is_empty() {
            "(not set)"
        } else {
            &options.target
        },
        scope = scope.describe(),
        workspace = Workspace::path(root).display(),
        budget = options.budget.describe(),
        parallel = options.parallel,
        workspace_dir = Workspace::dir(root).display(),
    )
}

pub fn continuation_brief(workspace: &Workspace, root: &Path, step: usize, tokens: u64) -> String {
    let (verified, reproduced, open) = workspace.counts();
    format!(
        "Continue the bug-bounty run. Step {step}; {tokens} tokens spent. Findings: {open} open · \
         {reproduced} reproduced · {verified} verified/reported.\n\
         Current state:\n{}\n\
         Next: verify the most promising hypothesis with a non-destructive test, or record a dead end. \
         Finish only with `bugbounty(action=\"report\")` (if anything is verified) and `BOUNTY COMPLETE`.\n\
         Workspace: {}",
        workspace.status(root),
        Workspace::path(root).display()
    )
}

/// Everything a caller needs to know after a run.
pub fn finish(
    root: &Path,
    workspace: &mut Workspace,
    steps: usize,
    tokens: u64,
    stop_reason: &str,
) -> ReportOutcome {
    let (verified, reproduced, potential) = workspace.counts();
    let mut outcome = ReportOutcome {
        report: Vec::new(),
        verified,
        reproduced,
        potential,
        steps,
        tokens,
        stop_reason: stop_reason.to_string(),
    };
    if verified > 0 {
        match workspace.report(root) {
            Ok(paths) => {
                let _ = workspace.save(root);
                outcome.report = paths;
            }
            Err(error) => {
                outcome.stop_reason = format!("report generation failed: {error:#}");
            }
        }
    }
    outcome
}

pub fn stop_report(outcome: &ReportOutcome) -> String {
    let mut text = format!(
        "Bug-bounty run stopped: {}\nSteps: {} · tokens: {} · verified: {} · reproduced: {} · open: {}\n",
        outcome.stop_reason,
        outcome.steps,
        outcome.tokens,
        outcome.verified,
        outcome.reproduced,
        outcome.potential
    );
    if outcome.report.is_empty() {
        text.push_str("No report generated: no finding reached verified with intact evidence.");
    } else {
        text.push_str("Reports:\n");
        for path in &outcome.report {
            text.push_str(&format!("  {}\n", path.display()));
        }
    }
    text
}

/// Headless and TUI-shared run: arm the scope, persist the workspace, then
/// turn after turn until the model declares the run complete, repeats
/// itself, or the budget stops it. Findings only leave the run through the
/// evidence gates in [`Workspace`].
pub async fn run(agent: &mut Agent, options: &Options) -> Result<ReportOutcome> {
    let root = agent.config.root.clone();
    let scope = build_scope(&root, options)?;
    agent.tools.set_scope(Some(scope.clone()));
    let mut workspace = Workspace::load_or_init(&root, &options.program, &options.target, &scope)?;
    println!("SCOPE   {}", scope.describe());
    println!("PROGRAM {}", workspace.program);
    println!("WORKSPACE {}", Workspace::path(&root).display());
    let mut brief = brief(options, &scope, &workspace, &root);
    let started = std::time::Instant::now();
    let mut steps = 0_usize;
    let mut last_reply: Option<String> = None;
    let mut repeats = 0_usize;
    let stop_reason: String;
    loop {
        let reply = match agent.turn(&brief).await {
            Ok(reply) => reply,
            Err(error) => {
                return Err(error.context("bug-bounty turn failed"));
            }
        };
        steps += 1;
        let head: String = reply.trim().chars().take(160).collect();
        println!("[{steps}] {}", head.replace('\n', " "));
        workspace = Workspace::load(&root)?.unwrap_or(workspace);
        let tokens = agent.usage.input + agent.usage.output;
        if reply.contains("BOUNTY COMPLETE") {
            stop_reason = "the agent declared the run complete".to_string();
            break;
        }
        if last_reply.as_deref() == Some(reply.trim()) {
            repeats += 1;
            if repeats >= 2 {
                stop_reason = "the agent repeated itself twice; stopping instead of looping".into();
                break;
            }
        } else {
            repeats = 0;
            last_reply = Some(reply.trim().to_string());
        }
        if let Some(reason) = options.budget.exhausted(steps, tokens, started.elapsed()) {
            stop_reason = format!("budget exhausted ({reason})");
            break;
        }
        brief = continuation_brief(&workspace, &root, steps, tokens);
    }
    let tokens = agent.usage.input + agent.usage.output;
    let outcome = finish(&root, &mut workspace, steps, tokens, &stop_reason);
    println!("{}", stop_report(&outcome));
    Ok(outcome)
}

/// One interactive turn for the TUI `/bugbounty` loop. Reloads the workspace
/// after the turn so the next prompt sees whatever the model verified.
pub fn after_turn(root: &Path, workspace: &Workspace) -> Result<Workspace> {
    Ok(Workspace::load(root)?.unwrap_or_else(|| workspace.clone()))
}

/// Create an offline engagement test plan. This prepares manual checks and
/// owner-controlled test matrices; it does not send any network requests.
fn create_vapt_plan(root: &Path, workspace: &Workspace) -> Result<String> {
    let directory = Workspace::dir(root);
    fs::create_dir_all(&directory)?;
    secure_dir(&directory)?;
    let path = directory.join("VAPT_ASSESSMENT.md");
    if path.exists() {
        return Ok(format!(
            "Existing VAPT assessment preserved: {}",
            path.display()
        ));
    }
    let mut text = format!(
        "# Authorized VAPT / Bug Bounty Assessment\n\n         Program: {}\nTarget: {}\nApproved scope: {}\n\n         ## Engagement rules and evidence\n\n         - [ ] Confirm written authorization and program rules before active requests.\n         - [ ] Record allowed assets, methods, exclusions and per-host rate ceilings.\n         - [ ] Use owned dummy accounts/objects; never enumerate third-party records.\n         - [ ] Avoid DoS, brute force, destructive writes and sensitive data extraction.\n         - [ ] Record timestamps, request and response pairs, expected vs actual, and hash.\n         - [ ] Do not call a scanner finding verified without a benign reproducer.\n         - [ ] Confirm the issue is not a duplicate before reporting.\n\n",
        redact(&workspace.program),
        redact(&workspace.target),
        redact(&describe_scope(&workspace.scope))
    );
    text.push_str(
        "## Coverage checklist (OWASP WSTG / ASVS 5.0.0)\n\n         - [ ] Asset inventory, technology and exposed paths (only approved hosts).\n         - [ ] TLS, security headers, cookies and session expiry.\n         - [ ] Authentication: registration, login, recovery, MFA and logout.\n         - [ ] Authorization: BOLA/IDOR, BFLA, cross-tenant and horizontal/vertical roles.\n         - [ ] API: REST/GraphQL authorization, validation and safe rate limits.\n         - [ ] Input/output validation: injection, XSS and encoding contexts.\n         - [ ] File upload, downloads, object storage and sensitive information handling.\n         - [ ] Business logic: state transitions, replay, quotas and workflow integrity.\n         - [ ] Configuration and dependency review where permitted.\n         - [ ] Error handling, audit trails and safe failure behavior.\n         - [ ] Remediation recommendations and scoped retest after approval.\n\n         ## Authorization test matrix (use owned dummy objects only)\n\n         | Actor | Owned object | Other owned test object | Expected | Actual/evidence |\n         |---|---|---|---|---|\n         | Anonymous | none | none | deny private data | pending |\n         | User A | A-owned object | B-owned object | A allow; B deny | pending |\n         | User B | B-owned object | A-owned object | B allow; A deny | pending |\n         | Admin (only if approved) | approved | approved | policy-defined | pending |\n\n         Change one role, method or dummy object identifier at a time. A 200 response\n         without proof of unauthorized content is not a verified IDOR.\n\n         ## Findings and reporting quality gate\n\n         - [ ] Exact affected host, endpoint, parameter, preconditions and role.\n         - [ ] Expected vs actual response and reproducible low-impact PoC.\n         - [ ] Request AND response evidence stored and SHA-256 verified.\n         - [ ] Demonstrated security impact with no exposed live credentials/PII.\n         - [ ] Severity reviewed against current Bugcrowd VRT and program policy.\n         - [ ] Clear remediation and program-specific submission requirements.\n         - [ ] Only verified findings move to final report.\n\n         References: https://wstg.owasp.org/ ; https://asvs.owasp.org/ ;\n         https://bugcrowd.com/vulnerability-rating-taxonomy\n\n         This document is a plan, not a scanner result or proof of vulnerabilities.\n"
    );
    write_private(&path, text.as_bytes())?;
    Ok(format!(
        "Offline VAPT assessment created: {}",
        path.display()
    ))
}

/// Tool entry point. `scope` is `None` outside bug-bounty mode, in which case
/// `add` still records local findings but `scope` confirmation is refused.
pub fn tool_action(
    root: &Path,
    scope: Option<&Scope>,
    action: &str,
    input: &Value,
) -> Result<String> {
    let mut workspace = Workspace::load(root)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no bug-bounty workspace at {}; run /bugbounty [target] --scope … first",
            Workspace::path(root).display()
        )
    })?;
    let output = match action {
        "status" => workspace.status(root),
        "assessment_plan" => create_vapt_plan(root, &workspace)?,
        "list" => {
            if workspace.findings.is_empty() {
                "No findings recorded.".into()
            } else {
                workspace
                    .findings
                    .iter()
                    .map(|finding| {
                        format!(
                            "{} [{}] {} — {}",
                            finding.id, finding.state, finding.severity, finding.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "add" => workspace.add(input, scope)?,
        "update" => workspace.update(input, scope)?,
        "investigate" => workspace.investigate(required(input, "finding_id")?)?,
        "evidence" => workspace.evidence(root, input)?,
        "reproduce" => workspace.reproduce(input)?,
        "scope" => workspace.confirm_scope(input, scope)?,
        "verify" => workspace.verify(root, required(input, "finding_id")?)?,
        "report" => {
            let paths = workspace.report(root)?;
            let listing = paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            workspace.save(root)?;
            return Ok(format!("Reports written:\n{listing}"));
        }
        other => bail!(
            "unknown bugbounty action `{other}` (use status, list, add, update, investigate, \
             evidence, reproduce, scope, verify, assessment_plan, report)"
        ),
    };
    workspace.save(root)?;
    Ok(output)
}

/// JSON schema for the `bugbounty` tool.
pub fn schema() -> Value {
    json!({
        "name": "bugbounty",
        "description": "Evidence-backed bug-bounty and VAPT workflow. assessment_plan creates a local OWASP WSTG/ASVS checklist and owned-object authorization matrix without network calls. Findings move potential → investigating → reproduced → verified → reported. Verify requires scope confirmation, observed impact and SHA-256-checked request/response evidence. Report refuses unverified findings.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "assessment_plan", "list", "add", "update", "investigate", "evidence", "reproduce", "scope", "verify", "report"]
                },
                "finding_id": {"type": "string", "description": "BB-0001"},
                "title": {"type": "string"},
                "asset": {"type": "string", "description": "in-scope host or URL"},
                "category": {"type": "string", "description": "idor, auth, xss, ssrf, info-disclosure, business-logic, …"},
                "severity": {"type": "string", "enum": SEVERITIES},
                "summary": {"type": "string"},
                "severity_reason": {"type": "string"},
                "cwe": {"type": "string", "description": "analyst-verified CWE identifier; never invent"},
                "wstg_id": {"type": "string", "description": "analyst-selected OWASP WSTG reference"},
                "asvs_id": {"type": "string", "description": "analyst-selected ASVS 5.0 requirement"},
                "cvss_vector": {"type": "string", "description": "analyst-calculated CVSS vector, not automatically inferred"},
                "impact": {"type": "string"},
                "remediation": {"type": "string"},
                "kind": {"type": "string", "enum": EVIDENCE_KINDS},
                "content": {"type": "string", "description": "raw request/response/note text (max 2 MiB)"},
                "filename": {"type": "string", "description": "optional safe filename"},
                "note": {"type": "string"},
                "steps": {
                    "anyOf": [
                        {"type": "string", "description": "one step per line"},
                        {"type": "array", "items": {"type": "string"}}
                    ]
                },
                "observed": {"type": "string", "description": "what the test actually returned"},
                "expected": {"type": "string", "description": "what a secure implementation should have done"},
                "in_scope": {"type": "boolean", "description": "scope action: asset belongs to the program"}
            },
            "required": ["action"],
            "additionalProperties": false
        }
    })
}

/// Evidence references are data loaded from disk. Recheck their canonical
/// location before reading: a crafted findings.json path or symlink must not
/// make report generation read files outside this finding's evidence folder.
fn checked_evidence_path(root: &Path, finding: &Finding, item: &Evidence) -> Result<PathBuf> {
    let relative = Path::new(&item.file);
    if relative.is_absolute() {
        bail!("absolute evidence paths are not allowed");
    }
    let dir = fs::canonicalize(Workspace::evidence_dir(root, &finding.id))
        .context("evidence directory missing or inaccessible")?;
    let path =
        fs::canonicalize(root.join(relative)).context("evidence file missing or inaccessible")?;
    if !path.starts_with(&dir) || !path.is_file() {
        bail!("evidence reference leaves the finding evidence directory");
    }
    Ok(path)
}

fn evidence_problems(finding: &Finding, root: Option<&Path>) -> Vec<String> {
    let mut problems = Vec::new();
    let has = |kind: &str| finding.evidence.iter().any(|item| item.kind == kind);
    if !has("request") {
        problems.push("missing request evidence".to_string());
    }
    if !has("response") {
        problems.push("missing response evidence".to_string());
    }
    if let Some(root) = root {
        for item in &finding.evidence {
            match checked_evidence_path(root, finding, item)
                .and_then(|path| fs::read(path).map_err(Into::into))
            {
                Ok(bytes) => {
                    let actual = sha256_hex(&bytes);
                    if actual != item.sha256 {
                        problems.push(format!(
                            "{} hash mismatch (recorded {}, actual {})",
                            item.id, item.sha256, actual
                        ));
                    }
                }
                Err(error) => problems.push(format!("{} unreadable: {error}", item.file)),
            }
        }
    }
    problems
}

fn check_asset(scope: &Scope, asset: &str) -> Result<()> {
    let host = checked_asset_host(asset)?;
    if scope.allows_host(&host) {
        Ok(())
    } else {
        bail!(
            "asset is outside the armed program scope ({})",
            scope.describe()
        )
    }
}

/// Fail-closed URL check for remote bug-bounty tools. A malformed URL must
/// never bypass the generic scope checker by returning no parsed hostname.
pub(crate) fn check_network_url(scope: &Scope, url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).context("invalid HTTP(S) target URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("bug-bounty network tools require an HTTP(S) URL without credentials");
    }
    let host = parsed.host_str().unwrap_or_default();
    let authority = match explicit_authority_port(url).or_else(|| parsed.port()) {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    if !scope.allows_host(&authority) {
        bail!("network to {authority} is outside the armed program scope");
    }
    Ok(())
}

fn parse_steps(value: Option<&Value>) -> Result<Vec<String>> {
    let steps: Vec<String> = match value {
        Some(Value::String(text)) => text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if steps.is_empty() {
        bail!("reproduce needs at least one step");
    }
    if steps.len() > 40 {
        bail!("too many reproduction steps (max 40)");
    }
    Ok(steps)
}

fn check_severity(severity: &str) -> Result<()> {
    if SEVERITIES.contains(&severity) {
        Ok(())
    } else {
        bail!("severity must be one of: {}", SEVERITIES.join(", "))
    }
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("bugbounty action needs `{key}`"))
}

fn describe_scope(scope: &ScopeRecord) -> String {
    let hosts = if scope.includes.is_empty() {
        "none".to_string()
    } else {
        scope.includes.join(", ")
    };
    if scope.excludes.is_empty() {
        hosts
    } else {
        format!("{hosts} (excluding {})", scope.excludes.join(", "))
    }
}

fn workspace_markdown(workspace: &Workspace, root: &Path, candidates: &[usize]) -> Result<String> {
    let mut out = String::new();
    out.push_str("# Bug Bounty Report\n\n");
    out.push_str(&format!(
        "- Program: {}\n- Target: {}\n- Generated: {}\n- Scope: {}\n",
        redact(&workspace.program),
        redact(&workspace.target),
        timestamp(),
        redact(&describe_scope(&workspace.scope))
    ));
    out.push_str(&format!(
        "- Workspace: `{}`\n- Verified/reported findings: {}\n\n",
        Workspace::path(root).display(),
        candidates.len()
    ));
    out.push_str("Every finding below was reproduced with recorded request/response evidence. Full evidence stays in `.bugbounty/evidence/<id>/`; the SHA-256 values below were re-checked when this report was generated.\n\n");
    out.push_str("| ID | Severity | Title | Asset | State |\n|---|---|---|---|---|\n");
    for index in candidates {
        let finding = &workspace.findings[*index];
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            finding.id,
            finding.severity,
            escape_table(&redact(&finding.title)),
            escape_table(&redact(&finding.asset)),
            finding.state
        ));
    }
    out.push('\n');
    for index in candidates {
        out.push_str(&finding_markdown(
            &workspace.findings[*index],
            root,
            &workspace.program,
        )?);
        out.push('\n');
    }
    out.push_str("## Unverified leads\n\n");
    let unverified: Vec<&Finding> = workspace
        .findings
        .iter()
        .filter(|finding| !matches!(finding.state.as_str(), "verified" | "reported"))
        .collect();
    if unverified.is_empty() {
        out.push_str("None recorded.\n");
    } else {
        out.push_str("These are not vulnerabilities and must not be submitted as such.\n\n");
        for finding in unverified {
            out.push_str(&format!(
                "- {} [{}] {} — {}\n",
                finding.id,
                finding.state,
                finding.severity,
                redact(&finding.title)
            ));
        }
    }
    Ok(out)
}

fn finding_markdown(finding: &Finding, root: &Path, program: &str) -> Result<String> {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} — {}\n\n",
        finding.id,
        redact(&finding.title)
    ));
    out.push_str(&format!(
        "- Program: {}\n- Asset: {}\n- Category: {}\n- Severity: {}\n- State: {}\n- Scope confirmed: {} ({})\n- Recorded: {}\n\n",
        redact(program),
        redact(&finding.asset),
        redact(&finding.category),
        finding.severity,
        finding.state,
        if finding.scope_confirmed { "yes" } else { "no" },
        redact(&finding.scope_note),
        timestamp()
    ));
    if !finding.severity_reason.trim().is_empty() {
        out.push_str("## Severity rationale\n\n");
        out.push_str(&redact(&finding.severity_reason));
        out.push_str("\n\n");
    }
    for (label, reference) in [
        ("CWE", &finding.cwe),
        ("OWASP WSTG", &finding.wstg_id),
        ("OWASP ASVS", &finding.asvs_id),
        ("CVSS vector (analyst provided)", &finding.cvss_vector),
    ] {
        if !reference.trim().is_empty() {
            out.push_str(&format!("- {}: {}\n", label, redact(reference)));
        }
    }
    out.push('\n');
    if !finding.summary.is_empty() {
        out.push_str("## Summary\n\n");
        out.push_str(&redact(&finding.summary));
        out.push_str("\n\n");
    }
    out.push_str("## Reproduction steps\n\n");
    for (index, step) in finding.steps.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", index + 1, redact(step)));
    }
    out.push_str("\n## Observed result\n\n");
    out.push_str(&fenced(&redact(&finding.observed)));
    out.push_str("\n\n## Expected secure behavior\n\n");
    out.push_str(&redact(&finding.expected));
    out.push_str("\n\n## Security impact\n\n");
    out.push_str(&redact(&finding.impact));
    out.push_str("\n\n## Remediation\n\n");
    if finding.remediation.is_empty() {
        out.push_str("_Not recorded yet._\n");
    } else {
        out.push_str(&redact(&finding.remediation));
        out.push('\n');
    }
    out.push_str("\n## Evidence\n\n");
    for item in &finding.evidence {
        out.push_str(&format!(
            "- `{}` ({}) — `{}` — SHA-256 `{}`{}\n",
            redact(&item.file),
            item.kind,
            redact(&item.note),
            item.sha256,
            if item.kind == "note" {
                ""
            } else {
                " (required proof)"
            }
        ));
        if item.kind == "request" || item.kind == "response" {
            if let Ok(bytes) = checked_evidence_path(root, finding, item)
                .and_then(|path| fs::read(path).map_err(Into::into))
            {
                let text = String::from_utf8_lossy(&bytes);
                let mut snippet: String = text.chars().take(2_000).collect();
                if text.chars().count() > 2_000 {
                    snippet.push_str("\n… (truncated; full file above)");
                }
                out.push_str(&format!(
                    "\n<details><summary>{} excerpt</summary>\n\n{}\n\n</details>\n",
                    item.kind,
                    fenced(&redact(&snippet))
                ));
            }
        }
    }
    Ok(out)
}

fn fenced(text: &str) -> String {
    let text = text.replace("```", "` ` `");
    format!("```text\n{}\n```", text)
}

fn escape_table(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn redact(text: &str) -> String {
    crate::crash::redact(text)
}

fn slug(text: &str, max: usize) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= max {
            break;
        }
    }
    slug.trim_matches('-').to_string()
}

fn sanitize_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches('.');
    let mut safe: String = base
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        .collect();
    safe.truncate(80);
    if safe.is_empty() {
        safe = "evidence.txt".into();
    }
    safe
}

fn tokenize(text: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(opening) = quote {
            if character == opening {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        if character == '\'' || character == '"' {
            quote = Some(character);
            continue;
        }
        if character.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(character);
    }
    if quote.is_some() {
        bail!("unterminated quote in /bugbounty arguments");
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        secure_dir(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_nanos()));
    let mut file = fs::File::create(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn secure_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = digest(&SHA256, bytes);
    let mut out = String::with_capacity(64);
    for byte in digest.as_ref() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn timestamp() -> String {
    format!("{} (unix)", now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "wrose-bounty-{}-{}-{}",
            label,
            std::process::id(),
            now_nanos()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn setup(label: &str) -> (PathBuf, Scope, Workspace) {
        let root = scratch(label);
        let scope = Scope::for_bounty(
            &root,
            "https://app.example.test",
            ["*.example.test"],
            ["admin.example.test"],
        );
        let workspace =
            Workspace::load_or_init(&root, "Example", "https://app.example.test", &scope).unwrap();
        (root, scope, workspace)
    }

    fn add(workspace: &mut Workspace, scope: &Scope) -> String {
        workspace
            .add(
                &json!({
                    "title": "IDOR in invoice endpoint",
                    "asset": "https://app.example.test/api/invoices/42",
                    "category": "idor",
                    "severity": "high",
                    "summary": "Another tenant's invoice is readable"
                }),
                Some(scope),
            )
            .unwrap();
        "BB-0001".into()
    }

    fn prove(workspace: &mut Workspace, root: &Path, id: &str) {
        workspace
            .evidence(
                root,
                &json!({
                    "finding_id": id,
                    "kind": "request",
                    "content": "GET /api/invoices/42 HTTP/1.1\nHost: app.example.test\nAuthorization: Bearer SECRET\n"
                }),
            )
            .unwrap();
        workspace
            .evidence(
                root,
                &json!({
                    "finding_id": id,
                    "kind": "response",
                    "content": "HTTP/1.1 200 OK\n\n{\"tenant\":\"other\",\"total\":999,\"email\":\"victim@example.test\"}"
                }),
            )
            .unwrap();
        workspace
            .reproduce(
                &json!({
                    "finding_id": id,
                    "steps": ["Log in as tenant A", "Request tenant B invoice 42", "Observe 200 with tenant B data"],
                    "observed": "HTTP 200 returned another tenant's invoice",
                    "expected": "HTTP 403 or 404 for another tenant's object"
                }),
            )
            .unwrap();
        workspace
            .confirm_scope(
                &json!({"finding_id": id, "in_scope": true, "note": "listed wildcard"}),
                Some(&Scope::for_bounty(
                    root,
                    "https://app.example.test",
                    ["*.example.test"],
                    ["admin.example.test"],
                )),
            )
            .unwrap();
    }

    #[test]
    fn scope_wildcards_and_exclusions_are_enforced() {
        let root = scratch("scope");
        let scope = Scope::for_bounty(
            &root,
            "https://app.example.test",
            ["*.example.test"],
            ["admin.example.test"],
        );
        assert!(scope.allows_host("app.example.test"));
        assert!(scope.allows_host("deep.app.example.test:8443"));
        assert!(
            !scope.allows_host("example.test"),
            "`*.example.test` covers subdomains but never the apex"
        );
        assert!(!scope.allows_host("admin.example.test"));
        assert!(!scope.allows_host("evil.test"));
        assert!(!scope.allows_host("example.test.evil.test"));
        assert!(scope.describe().contains("excluded admin.example.test"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn state_machine_refuses_verification_without_proof() {
        let (root, scope, mut workspace) = setup("gate");
        let id = add(&mut workspace, &scope);
        let error = workspace.verify(&root, &id).unwrap_err().to_string();
        assert!(error.contains("only reproduced"), "{error}");
        let error = workspace
            .reproduce(&json!({
                "finding_id": id,
                "steps": ["step"],
                "observed": "observed",
                "expected": "expected"
            }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("request"), "{error}");
        let error = workspace.report(&root).unwrap_err().to_string();
        assert!(error.contains("no verified findings"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verification_requires_scope_and_report_marks_reported() {
        let (root, scope, mut workspace) = setup("verify");
        let id = add(&mut workspace, &scope);
        prove(&mut workspace, &root, &id);
        workspace
            .update(
                &json!({
                    "finding_id": id,
                    "impact": "Cross-tenant invoice disclosure",
                    "remediation": "Authorize object ownership before serialization"
                }),
                None,
            )
            .unwrap();
        let message = workspace.verify(&root, &id).unwrap();
        assert!(message.contains("VERIFIED"));
        assert_eq!(workspace.find(&id).unwrap().state, "verified");
        let paths = workspace.report(&root).unwrap();
        assert!(!paths.is_empty());
        let report = fs::read_to_string(&paths[0]).unwrap();
        assert!(report.contains("IDOR in invoice endpoint"));
        assert!(report.contains("Cross-tenant invoice disclosure"));
        assert!(!report.contains("SECRET"), "report leaked a bearer token");
        assert!(report.contains("SHA-256"));
        assert_eq!(workspace.find(&id).unwrap().state, "reported");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn evidence_blocks_asset_swap_and_narrowed_scope_revokes_verification() {
        let (root, scope, mut workspace) = setup("scope-revocation");
        let id = add(&mut workspace, &scope);
        prove(&mut workspace, &root, &id);
        let error = workspace
            .update(
                &json!({"finding_id": id, "asset": "https://app.example.test/other"}),
                Some(&scope),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot change a finding asset"), "{error}");
        let error = workspace
            .update(
                &json!({"finding_id": id, "asset": "https://outside.test/"}),
                Some(&scope),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot change a finding asset"), "{error}");
        workspace
            .update(
                &json!({"finding_id": id, "impact": "Controlled cross-tenant read"}),
                None,
            )
            .unwrap();
        workspace.verify(&root, &id).unwrap();
        workspace.save(&root).unwrap();
        let narrower = Scope::for_bounty(
            &root,
            "",
            ["other.example.test"],
            std::iter::empty::<&str>(),
        );
        let mut updated = Workspace::load_or_init(&root, "Example", "", &narrower).unwrap();
        let finding = updated.find(&id).unwrap();
        assert!(!finding.scope_confirmed);
        assert_eq!(finding.state, "reproduced");
        assert!(updated.verify(&root, &id).is_err());
        assert!(updated.report(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn vapt_reporting_keeps_existing_json_compatible_and_preserves_rationale() {
        let (root, scope, mut workspace) = setup("vapt-reports");
        let id = add(&mut workspace, &scope);
        prove(&mut workspace, &root, &id);
        workspace.update(
            &json!({
                "finding_id": id,
                "impact": "Controlled cross-tenant object disclosure",
                "severity_reason": "Owner-only authorization bypass verified with test accounts",
                "cwe": "CWE-639",
                "wstg_id": "WSTG-ATHZ-04",
                "asvs_id": "ASVS 5.0 (analyst mapping)",
                "cvss_vector": "CVSS:4.0/AV:N/AC:L/AT:N/PR:L/UI:N/VC:H/VI:N/VA:N/SC:N/SI:N/SA:N"
            }), None
        ).unwrap();
        workspace.verify(&root, &id).unwrap();
        let report = workspace.report(&root).unwrap();
        let body = fs::read_to_string(&report[1]).unwrap();
        for word in [
            "Severity rationale",
            "CWE-639",
            "WSTG-ATHZ-04",
            "ASVS 5.0",
            "CVSS:4.0/",
        ] {
            assert!(body.contains(word), "missing report field {word}");
        }
        let mut old = serde_json::to_value(workspace.find(&id).unwrap()).unwrap();
        for field in [
            "severity_reason",
            "cwe",
            "wstg_id",
            "asvs_id",
            "cvss_vector",
        ] {
            old.as_object_mut().unwrap().remove(field);
        }
        let legacy: Finding = serde_json::from_value(old).unwrap();
        assert!(legacy.severity_reason.is_empty());
        assert!(legacy.cvss_vector.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn forged_evidence_path_cannot_escape_its_finding_directory() {
        let (root, scope, mut workspace) = setup("no-evidence-traversal");
        let id = add(&mut workspace, &scope);
        prove(&mut workspace, &root, &id);
        workspace
            .update(
                &json!({"finding_id": id, "impact": "Controlled test-owned data"}),
                None,
            )
            .unwrap();
        let original = workspace.find(&id).unwrap().evidence[0].file.clone();
        workspace.find_mut(&id).unwrap().evidence[0].file = "../outside-secret.txt".into();
        let error = workspace.verify(&root, &id).unwrap_err().to_string();
        assert!(error.contains("unreadable"), "{error}");
        workspace.find_mut(&id).unwrap().evidence[0].file = original;
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = root.join("outside-secret.txt");
            fs::write(&outside, "synthetic private fixture").unwrap();
            let dir = Workspace::evidence_dir(&root, &id);
            symlink(&outside, dir.join("symlink-response.txt")).unwrap();
            workspace.find_mut(&id).unwrap().evidence[0].file =
                format!(".bugbounty/evidence/{id}/symlink-response.txt");
            let error = workspace.verify(&root, &id).unwrap_err().to_string();
            assert!(error.contains("evidence reference leaves"), "{error}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampered_evidence_blocks_verification_and_reporting() {
        let (root, scope, mut workspace) = setup("tamper");
        let id = add(&mut workspace, &scope);
        prove(&mut workspace, &root, &id);
        workspace
            .update(&json!({"finding_id": id, "impact": "impact"}), None)
            .unwrap();
        let response = workspace
            .find(&id)
            .unwrap()
            .evidence
            .iter()
            .find(|item| item.kind == "response")
            .unwrap()
            .file
            .clone();
        fs::write(root.join(response), "HTTP/1.1 403 Forbidden").unwrap();
        let error = workspace.verify(&root, &id).unwrap_err().to_string();
        assert!(error.contains("hash mismatch"), "{error}");
        assert_eq!(workspace.find(&id).unwrap().state, "reproduced");
        // A persisted state can claim `verified` even when the evidence on
        // disk was altered afterwards; report generation must re-hash and
        // refuse it instead of trusting the label.
        workspace.find_mut(&id).unwrap().state = "verified".into();
        let error = workspace.report(&root).unwrap_err().to_string();
        assert!(error.contains("cannot be reported"), "{error}");
        assert!(error.contains("hash mismatch"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn out_of_scope_asset_is_refused_at_add_and_verify() {
        let (root, scope, mut workspace) = setup("asset");
        let error = workspace
            .add(
                &json!({"title": "x", "asset": "https://evil.test/x"}),
                Some(&scope),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the armed program scope"), "{error}");
        let id = add(&mut workspace, &scope);
        let error = workspace
            .confirm_scope(
                &json!({"finding_id": id, "in_scope": true}),
                Some(&Scope::for_bounty(
                    &root,
                    "https://other.example.test",
                    ["other.example.test"],
                    std::iter::empty::<&str>(),
                )),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the armed program scope"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unlisted_target_must_not_extend_the_program_allowlist() {
        let root = scratch("bounty-scope-target");
        let options = Options {
            target: "https://outside.example.test".into(),
            includes: vec!["app.example.test".into()],
            ..Options::with_default_budget()
        };
        let error = build_scope(&root, &options).unwrap_err().to_string();
        assert!(error.contains("outside the armed program scope"), "{error}");
        let scope = Scope::for_bounty(
            &root,
            "https://outside.example.test",
            ["app.example.test"],
            std::iter::empty::<&str>(),
        );
        assert!(!scope.allows_host("outside.example.test"));
        assert!(scope.allows_host("app.example.test"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scope_hosts_validate_ports_paths_userinfo_and_exclusions() {
        assert_eq!(
            checked_scope_host("*.Example.test:8443").unwrap(),
            "*.example.test:8443"
        );
        assert_eq!(
            checked_scope_host("https://app.example.test:443").unwrap(),
            "app.example.test:443"
        );
        let default_port_scope = Scope::for_bounty(
            std::path::Path::new("/tmp"),
            "",
            ["app.example.test:443"],
            std::iter::empty::<&str>(),
        );
        assert!(!default_port_scope.allows_host("app.example.test:1234"));
        check_network_url(&default_port_scope, "https://app.example.test:443/path").unwrap();
        assert!(
            check_network_url(&default_port_scope, "https://app.example.test:8443/path").is_err()
        );
        for bad in [
            "*",
            "ftp://app.example.test",
            "https://app.example.test/private",
            "https://app.example.test/?x=1",
            "https://evil@app.example.test",
            "http://",
            "example.test:badport",
            "*.example.test.evil@test",
        ] {
            assert!(checked_scope_host(bad).is_err(), "scope accepted {bad}");
        }
        let root = scratch("bounty-port");
        let scope = Scope::for_bounty(&root, "", ["app.example.test:8443"], ["deny.example.test"]);
        check_asset(&scope, "https://app.example.test:8443/api").unwrap();
        assert!(check_asset(&scope, "https://app.example.test/api").is_err());
        assert!(check_asset(&scope, "https://bad@app.example.test:8443/").is_err());
        assert!(check_asset(&scope, "ftp://app.example.test:8443/").is_err());
        check_network_url(&scope, "https://app.example.test:8443/safe").unwrap();
        for bad in [
            "https://app.example.test/",
            "https://deny.example.test/",
            "file:///etc/passwd",
            "not a URL",
            "https://evil@app.example.test:8443/",
        ] {
            assert!(
                check_network_url(&scope, bad).is_err(),
                "network allowed {bad}"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_assessment_plan_includes_owned_object_matrix_without_overwriting() {
        let (root, _scope, workspace) = setup("offline-vapt-plan");
        let path = Workspace::dir(&root).join("VAPT_ASSESSMENT.md");
        assert!(!path.exists());
        let result = create_vapt_plan(&root, &workspace).unwrap();
        assert!(
            result.contains("Offline VAPT assessment created"),
            "{result}"
        );
        let body = fs::read_to_string(&path).unwrap();
        for label in [
            "WSTG",
            "ASVS 5.0.0",
            "BOLA",
            "User A",
            "User B",
            "expected",
            "rate",
            "SHA-256",
            "VRT",
            "own",
        ] {
            assert!(
                body.to_ascii_lowercase()
                    .contains(&label.to_ascii_lowercase()),
                "missing {label}"
            );
        }
        fs::write(&path, "owner edited checklist").unwrap();
        assert!(create_vapt_plan(&root, &workspace)
            .unwrap()
            .contains("preserved"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "owner edited checklist");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scope_file_parses_includes_excludes_and_comments() {
        let root = scratch("scopefile");
        let path = root.join("scope.txt");
        fs::write(
            &path,
            "# program scope\napp.example.test\ninclude *.example.test\nexclude admin.example.test # internal\n",
        )
        .unwrap();
        let (includes, excludes) = read_scope_file(&path).unwrap();
        assert_eq!(includes.len(), 2);
        assert_eq!(excludes, vec!["admin.example.test"]);
        let options =
            parse_command_args(&format!("--scope-file {} --program Acme", path.display())).unwrap();
        assert_eq!(options.program, "Acme");
        assert_eq!(options.includes.len(), 2);
        assert_eq!(options.budget.steps, 20);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parser_accepts_quotes_and_rejects_unknown_flags() {
        let options = parse_command_args(
            "\"https://app.example.test/a b\" --scope app.example.test,*.example.test --parallel 99",
        );
        assert_eq!(options.unwrap().parallel, 8);
        assert!(parse_command_args("app.example.test --wat").is_err());
        assert!(parse_command_args("app.example.test --scope 'unterminated").is_err());
    }

    #[test]
    fn live_scope_requires_at_least_one_host() {
        let root = scratch("nohost");
        let options = Options {
            target: "not a host".into(),
            ..Options::with_default_budget()
        };
        let error = build_scope(&root, &options).unwrap_err().to_string();
        assert!(error.contains("no in-scope hosts"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn filenames_cannot_escape_the_evidence_directory() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\x"), "x");
        assert_eq!(sanitize_filename("a/b"), "b");
        assert!(sanitize_filename("<>").ends_with("evidence.txt"));
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn tool_actions_round_trip_through_disk() {
        let (root, scope, mut workspace) = setup("tool");
        let id = add(&mut workspace, &scope);
        workspace.save(&root).unwrap();
        let output = tool_action(
            &root,
            Some(&scope),
            "evidence",
            &json!({
                "finding_id": id,
                "kind": "request",
                "content": "GET / HTTP/1.1"
            }),
        )
        .unwrap();
        assert!(output.contains("sha256"));
        let output = tool_action(&root, Some(&scope), "status", &json!({})).unwrap();
        assert!(output.contains("BB-0001"));
        assert!(tool_action(&root, Some(&scope), "nope", &json!({})).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_directory_targets_need_no_network_allowlist() {
        let root = scratch("local");
        let target = root.join("challenge");
        fs::create_dir_all(&target).unwrap();
        let options = Options {
            target: target.display().to_string(),
            ..Options::with_default_budget()
        };
        let scope = build_scope(&root, &options).unwrap();
        assert!(
            scope.allowed_hosts().is_empty(),
            "a local target must not invent network scope"
        );
        let canonical = fs::canonicalize(&target).unwrap();
        assert!(
            scope
                .roots()
                .iter()
                .any(|candidate| candidate == &canonical),
            "the target directory must become a readable root: {:?}",
            scope.roots()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn report_contains_only_verified_findings_and_names_unverified_leads() {
        let (root, scope, mut workspace) = setup("mixed");
        let verified = add(&mut workspace, &scope);
        prove(&mut workspace, &root, &verified);
        workspace
            .update(
                &json!({
                    "finding_id": verified,
                    "impact": "Cross-tenant invoice disclosure",
                    "remediation": "Authorize object ownership before serialization"
                }),
                None,
            )
            .unwrap();
        workspace.verify(&root, &verified).unwrap();

        workspace
            .add(
                &json!({
                    "title": "Suspicious cache header",
                    "asset": "https://app.example.test/api",
                    "severity": "low"
                }),
                Some(&scope),
            )
            .unwrap();

        let paths = workspace.report(&root).unwrap();
        let index = fs::read_to_string(&paths[0]).unwrap();
        assert!(index.contains("IDOR in invoice endpoint"), "{index}");
        assert!(index.contains("Unverified leads"), "{index}");
        assert!(index.contains("Suspicious cache header"), "{index}");
        assert!(index.contains("must not be submitted as such"), "{index}");
        assert_eq!(workspace.find(&verified).unwrap().state, "reported");
        let unverified = workspace
            .findings
            .iter()
            .find(|finding| finding.title == "Suspicious cache header")
            .unwrap();
        assert_eq!(unverified.state, "potential");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deleted_evidence_turns_finish_into_an_explicit_failure() {
        let (root, scope, mut workspace) = setup("deleted");
        let id = add(&mut workspace, &scope);
        prove(&mut workspace, &root, &id);
        workspace
            .update(&json!({"finding_id": id, "impact": "impact"}), None)
            .unwrap();
        workspace.verify(&root, &id).unwrap();
        let response = workspace
            .find(&id)
            .unwrap()
            .evidence
            .iter()
            .find(|item| item.kind == "response")
            .unwrap()
            .file
            .clone();
        fs::remove_file(root.join(response)).unwrap();

        let outcome = finish(&root, &mut workspace, 3, 99, "done");
        assert!(outcome.report.is_empty(), "no report may be written");
        assert!(
            outcome.stop_reason.contains("report generation failed"),
            "{}",
            outcome.stop_reason
        );
        let rendered = stop_report(&outcome);
        assert!(rendered.contains("No report generated"), "{rendered}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn action_classes_separate_local_remote_and_prohibited_work() {
        use super::ActionClass;
        let classify = super::classify_action;

        // Local, evidence-gated work stays available with no network scope.
        for (name, input) in [
            ("read_file", json!({"path": "a.txt"})),
            ("grep", json!({"pattern": "token"})),
            ("write_file", json!({"path": "notes.md", "content": "x"})),
            ("bugbounty", json!({"action": "list"})),
            ("shell", json!({"command": "git status"})),
        ] {
            assert_eq!(
                classify(name, &input),
                ActionClass::Passive,
                "{name} must be passive"
            );
            assert!(!classify(name, &input).requires_scope(), "{name}");
        }

        // Remote reads and writes are scope-gated but not prohibited.
        assert_eq!(
            classify("http", &json!({"url": "https://app.example.test"})),
            ActionClass::LowImpact
        );
        assert_eq!(
            classify(
                "http",
                &json!({"method": "POST", "url": "https://app.example.test/api"})
            ),
            ActionClass::Elevated
        );
        assert_eq!(
            classify("web_fetch", &json!({"url": "https://app.example.test"})),
            ActionClass::LowImpact
        );
        assert_eq!(
            classify(
                "shell",
                &json!({"command": "curl -X POST https://app.example.test"})
            ),
            ActionClass::Elevated
        );

        // Destructive shell and side-effect surfaces outside the evidence
        // model are prohibited.
        assert_eq!(
            classify("shell", &json!({"command": "rm -rf /tmp/x"})),
            ActionClass::Prohibited
        );
        assert_eq!(
            classify("computer", &json!({"action": "click"})),
            ActionClass::Prohibited
        );
        assert_eq!(
            classify("mcp", &json!({"server": "x", "tool": "y"})),
            ActionClass::Prohibited
        );

        // Unknown tools default to Elevated, never Passive.
        assert_eq!(classify("future_tool", &json!({})), ActionClass::Elevated);
        assert!(ActionClass::LowImpact.requires_scope());
        assert!(ActionClass::Elevated.requires_scope());
        assert!(!ActionClass::Prohibited.requires_scope());
    }
}
