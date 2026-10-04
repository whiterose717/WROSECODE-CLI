//! End-to-end coverage for the CTF autopilot entry point
//! (`wrosecode ctf <target>`): scope arming, flag verification, candidate
//! handling, budget stop reports, and writeups — all against the local
//! OpenAI-compatible mock provider, no network, no API key.

mod common;

use common::{spawn, Response};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    home: PathBuf,
    root: PathBuf,
}

impl Sandbox {
    fn new(label: &str, base_url: &str) -> Self {
        let unique = format!("wrose-ctf-{}-{}", label, std::process::id());
        let home = std::env::temp_dir().join(format!("{unique}-home"));
        let root = std::env::temp_dir().join(format!("{unique}-root"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(home.join(".wrosecode")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            home.join(".wrosecode/providers.toml"),
            format!(
                r#"
[[provider]]
name = "mock"
kind = "openai_compat"
base_url = "{base_url}/v1"
model = "mock-model"
key_ref = "env:WROSECODE_MOCK_API_KEY"
"#
            ),
        )
        .unwrap();
        Self { home, root }
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ctf(sandbox: &Sandbox, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wrosecode"));
    command
        .current_dir(&sandbox.root)
        .env("HOME", &sandbox.home)
        .env("WROSECODE_MOCK_API_KEY", "test-key")
        .env_remove("WROSECODE_OPENAI_API_KEY")
        .args(["ctf"])
        .args(args)
        .args(["--provider", "mock", "--model", "mock-model"]);
    command.output().expect("run wrosecode ctf")
}

fn completion(content: &str) -> Response {
    Response::json(
        serde_json::json!({
            "id": "cmpl-1",
            "object": "chat.completion",
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 3},
        })
        .to_string(),
    )
}

fn tool_call(name: &str, arguments: &str) -> Response {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).expect("tool arguments must be json");
    Response::json(
        serde_json::json!({
            "id": "cmpl-1",
            "object": "chat.completion",
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": serde_json::Value::Null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": name, "arguments": parsed.to_string()},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 3},
        })
        .to_string(),
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// A flag sitting in a challenge artifact is re-derived by the verifier, so
/// the run exits 0 with `FLAG` and leaves a writeup behind.
#[test]
fn flag_in_a_challenge_file_verifies_and_writes_a_writeup() {
    let server = spawn(vec![completion("no idea")]);
    let sandbox = Sandbox::new("verified", &server.url(""));
    sandbox.file("challenge.txt", "the password is flag{artifact_derived} ok");

    let output = ctf(&sandbox, &["challenge.txt", "--budget", "5"]);
    let stdout = stdout(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        stderr(&output)
    );
    assert!(stdout.contains("SCOPE"), "stdout: {stdout}");
    assert!(stdout.contains("✔ flag verified"), "stdout: {stdout}");
    assert!(
        stdout.contains("FLAG    flag{artifact_derived}"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("EVIDENCE"), "stdout: {stdout}");
    assert!(
        stdout.contains("WRITEUP"),
        "every run writes a writeup: {stdout}"
    );
    let writeup = sandbox.root.join("writeups/ctf-challenge-txt.md");
    assert!(writeup.exists(), "missing {}", writeup.display());
    let text = std::fs::read_to_string(&writeup).unwrap();
    assert!(text.contains("flag{artifact_derived}"), "{text}");
}

/// A custom `--flag-format` replaces the built-in detector.
#[test]
fn flag_format_regex_drives_detection() {
    let server = spawn(vec![completion("keep looking")]);
    let sandbox = Sandbox::new("format", &server.url(""));
    sandbox.file("challenge.txt", "wrap: SECRET{rotated_ok}");

    let output = ctf(
        &sandbox,
        &[
            "challenge.txt",
            "--budget",
            "5",
            "--flag-format",
            r"SECRET\{[^}]+\}",
        ],
    );
    let stdout = stdout(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        stderr(&output)
    );
    assert!(
        stdout.contains("FLAG    SECRET{rotated_ok}"),
        "stdout: {stdout}"
    );
}

/// A model claim that no artifact backs stays a candidate and the run ends
/// with the full Tried/Learned/Next report instead of a silent exit.
#[test]
fn model_claim_is_a_candidate_and_the_stop_report_is_not_silent() {
    let server = spawn(vec![completion("answer is flag{claimed_only}")]);
    let sandbox = Sandbox::new("candidate", &server.url(""));
    sandbox.file("challenge.txt", "no flag in here");

    let output = ctf(&sandbox, &["challenge.txt", "--budget", "1"]);
    let stdout = stdout(&output);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {stdout}\nstderr: {}",
        stderr(&output)
    );
    assert!(
        stdout.contains("⚠ candidate, unverified"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("flag{claimed_only}"), "stdout: {stdout}");
    assert!(stdout.contains("BUDGET EXHAUSTED"), "stdout: {stdout}");
    assert!(stdout.contains("Tried:"), "stdout: {stdout}");
    assert!(stdout.contains("Learned:"), "stdout: {stdout}");
    assert!(stdout.contains("Next:"), "stdout: {stdout}");
    // No artifact backs the claim, so it must never read as verified.
    assert!(!stdout.contains("✔ flag verified"), "stdout: {stdout}");
    assert!(stdout.contains("WRITEUP"), "stdout: {stdout}");
    assert!(
        sandbox.root.join("writeups/ctf-challenge-txt.md").exists(),
        "partial run still writes a writeup"
    );
}

/// The allowlist is shown at start, `--remote` widens it, and an
/// out-of-scope write is denied in headless mode with the reason handed
/// back to the model instead of running.
#[test]
fn out_of_scope_write_is_denied_and_remote_widens_the_scope() {
    let server = spawn(vec![
        tool_call(
            "write_file",
            "{\"path\":\"/etc/wrose-pwned\",\"content\":\"x\"}",
        ),
        completion("understood"),
    ]);
    let sandbox = Sandbox::new("scope", &server.url(""));
    sandbox.file("challenge.txt", "just a challenge");

    let output = ctf(
        &sandbox,
        &[
            "challenge.txt",
            "--budget",
            "5",
            "--remote",
            "demo.example:8443",
        ],
    );
    let stdout = stdout(&output);
    assert!(
        stdout.contains("SCOPE"),
        "the allowlist must be shown at start: {stdout}"
    );
    assert!(
        stdout.contains("demo.example:8443"),
        "--remote must appear in the scope line: {stdout}"
    );
    assert!(
        !Path::new("/etc/wrose-pwned").exists(),
        "an out-of-scope write must not land"
    );

    let requests = server.requests();
    assert!(
        requests.len() >= 2,
        "expected a tool round trip, saw {} requests",
        requests.len()
    );
    let last = requests.last().expect("at least one request");
    assert!(
        last.body.contains("out-of-scope"),
        "the denial must reach the model: {}",
        last.body
    );

    // The budget stops the run loudly rather than silently.
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {stdout}\nstderr: {}",
        stderr(&output)
    );
    assert!(stdout.contains("BUDGET EXHAUSTED"), "stdout: {stdout}");
}
