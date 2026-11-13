//! Repeatable workflow benchmarks for the deterministic parts of general
//! coding, CTF and bug-bounty work: scripted mock-model tool calls over local
//! fixtures. They measure the harness (turn loop, tool dispatch, evidence
//! plumbing, verification) with model reasoning held fixed. Every run must
//! pass; the `BENCH` lines are the honest record.

mod common;

use common::{spawn, Response};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Bench {
    home: PathBuf,
    root: PathBuf,
}

impl Bench {
    fn new(label: &str, base_url: &str) -> Self {
        let unique = format!("wrose-flow-{}-{}", label, std::process::id());
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

    fn file(&self, name: &str, contents: &str) {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap()
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.root);
    }
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

struct Outcome {
    output: Output,
    wall: Duration,
    requests: Vec<common::Recorded>,
}

fn run(bench: &Bench, server: &common::MockServer, prompt: &str) -> Outcome {
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_wrosecode"))
        .current_dir(&bench.root)
        .env("HOME", &bench.home)
        .env("WROSECODE_MOCK_API_KEY", "test-key")
        .env_remove("WROSECODE_OPENAI_API_KEY")
        .args([
            "--headless",
            "--quiet",
            "--summary",
            "json",
            "--permission",
            "yolo",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--max-wall-time",
            "60",
            prompt,
        ])
        .output()
        .expect("run wrosecode");
    let wall = started.elapsed();
    let requests = server.requests();
    Outcome {
        output,
        wall,
        requests,
    }
}

fn assert_passed(outcome: &Outcome, needle: &str) {
    let stdout = String::from_utf8_lossy(&outcome.output.stdout);
    let stderr = String::from_utf8_lossy(&outcome.output.stderr);
    assert!(
        outcome.output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("\"type\":\"TaskComplete\""),
        "stdout: {stdout}"
    );
    assert!(stdout.contains(needle), "stdout: {stdout}");
}

fn last_request(outcome: &Outcome) -> &str {
    &outcome
        .requests
        .last()
        .expect("at least one model request")
        .body
}

/// General coding: read a file, make an exact edit, prove the edit with a
/// shell command, and let the model see that evidence on the next turn.
#[test]
fn bench_general_read_edit_verify_workflow() {
    for run_index in 0..3 {
        let server = spawn(vec![
            tool_call("read_file", r#"{"path":"src.txt"}"#),
            tool_call(
                "edit_file",
                r#"{"path":"src.txt","old":"before","new":"after"}"#,
            ),
            tool_call("shell", r#"{"command":"grep -n after src.txt"}"#),
            completion("WORKFLOW-GENERAL-OK"),
        ]);
        let bench = Bench::new(&format!("general-{run_index}"), &server.url(""));
        bench.file("src.txt", "alpha\nbefore\nomega\n");

        let outcome = run(&bench, &server, "change before to after and verify it");
        assert_passed(&outcome, "WORKFLOW-GENERAL-OK");
        assert_eq!(bench.read("src.txt"), "alpha\nafter\nomega\n");
        let last = last_request(&outcome);
        assert!(
            last.contains("after"),
            "the shell evidence never reached the model: {last}"
        );
        eprintln!(
            "BENCH general run {run_index}: wall={:?} requests={}",
            outcome.wall,
            outcome.requests.len()
        );
    }
}

/// Bug-bounty static recon: grep a JavaScript bundle for an API route, read
/// the bundle, and confirm the endpoint and the hardcoded key both reached
/// the model's context.
#[test]
fn bench_bugbounty_static_js_recon() {
    let bundle = "\
const API_KEY = \"sk_live_bench_51H9xQ2\";\n\
fetch(\"/api/v1/internal/users\");\n\
function main() { return API_KEY; }\n";
    for run_index in 0..3 {
        let server = spawn(vec![
            tool_call("grep", r#"{"pattern":"api/v1"}"#),
            tool_call("read_file", r#"{"path":"static/app.js"}"#),
            completion("BUGREPORT: /api/v1/internal/users with a live key"),
        ]);
        let bench = Bench::new(&format!("bb-static-{run_index}"), &server.url(""));
        bench.file("static/app.js", bundle);
        bench.file("robots.txt", "User-agent: *\nDisallow: /internal/\n");

        let outcome = run(&bench, &server, "recon the local web app");
        assert_passed(&outcome, "BUGREPORT:");
        let last = last_request(&outcome);
        assert!(
            last.contains("/api/v1/internal/users"),
            "route never reached the model: {last}"
        );
        assert!(
            last.contains("sk_live_bench_51H9xQ2"),
            "hardcoded key never reached the model: {last}"
        );
        eprintln!(
            "BENCH bugbounty-static run {run_index}: wall={:?} requests={}",
            outcome.wall,
            outcome.requests.len()
        );
    }
}

/// Bug-bounty live recon: curl a local HTTP fixture whose robots.txt exposes
/// a hidden path, and confirm the path was fed back to the model.
#[test]
fn bench_bugbounty_live_http_recon() {
    for run_index in 0..3 {
        let server = spawn(vec![Response::json(
            "User-agent: *\nDisallow: /internal/\nDisallow: /backup-2026/\n",
        )]);
        let target = server.url("/robots.txt");
        let responses = vec![
            tool_call("shell", &format!(r#"{{"command":"curl -s {target}"}}"#)),
            completion("BUGREPORT: /internal/ and /backup-2026/ are disallowed"),
        ];
        // The provider fixture is a second server: the first one serves the
        // target under test, this one serves scripted model replies.
        let model = spawn(responses);
        let bench = Bench::new(&format!("bb-live-{run_index}"), &model.url(""));

        let outcome = run(&bench, &model, "check the live fixture's robots.txt");
        assert_passed(&outcome, "BUGREPORT:");
        let last = last_request(&outcome);
        assert!(
            last.contains("/backup-2026/"),
            "robots.txt evidence never reached the model: {last}"
        );
        eprintln!(
            "BENCH bugbounty-live run {run_index}: wall={:?} requests={}",
            outcome.wall,
            outcome.requests.len()
        );
    }
}
