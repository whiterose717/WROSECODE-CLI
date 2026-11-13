//! Repeatable CTF pipeline benchmarks: scripted mock-model solves over
//! local fixtures, measuring verified solve rate, wall time, and model
//! request count per challenge.
//!
//! These benchmarks measure WROSECODE's own pipeline overhead (triage,
//! turns, tool dispatch, verification) — not model intelligence, which the
//! scripted mock holds fixed. Every run must verify; the table printed at
//! the end (`--nocapture`) is the honest before/after record.

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
        let unique = format!("wrose-bench-{}-{}", label, std::process::id());
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

    fn file(&self, name: &str, contents: &[u8]) {
        std::fs::write(self.root.join(name), contents).unwrap();
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

struct Run {
    verified: bool,
    wall: Duration,
    requests: usize,
}

/// One scripted solve: run the binary, assert nothing here, return facts.
fn solve(bench: &Bench, server: &common::MockServer, target: &str, extra_args: &[&str]) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wrosecode"));
    command
        .current_dir(&bench.root)
        .env("HOME", &bench.home)
        .env("WROSECODE_MOCK_API_KEY", "test-key")
        .env_remove("WROSECODE_OPENAI_API_KEY")
        .args(["ctf"])
        .args([target, "--budget", "8"])
        .args(extra_args)
        .args(["--provider", "mock", "--model", "mock-model"]);
    let started = Instant::now();
    let output: Output = command.output().expect("run wrosecode ctf");
    let wall = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Run {
        verified: output.status.code() == Some(0) && stdout.contains("✔ flag verified"),
        wall,
        requests: server.count(),
    }
}

/// Fixture 1 — plaintext flag in a file: triage, one read, verify.
#[test]
fn bench_file_flag_solves_repeatedly() {
    let mut walls = Vec::new();
    let mut requests = Vec::new();
    for run in 0..3 {
        let server = spawn(vec![
            tool_call("read_file", r#"{"path":"challenge.txt"}"#),
            completion("solved: flag{bench_file_ok}"),
        ]);
        let bench = Bench::new(&format!("file{run}"), &server.url(""));
        bench.file("challenge.txt", b"the password is flag{bench_file_ok} ok");
        let outcome = solve(&bench, &server, "challenge.txt", &[]);
        assert!(outcome.verified, "run {run} must verify");
        assert!(
            outcome.requests <= 10,
            "run {run}: {} model requests is too many for a 2-turn solve",
            outcome.requests
        );
        walls.push(outcome.wall);
        requests.push(outcome.requests);
    }
    eprintln!("BENCH file: walls={walls:?} requests={requests:?}");
}

/// Fixture 2 — base64-wrapped flag: the decode chain is the evidence, and
/// the verifier must accept the variant-derived hit.
#[test]
fn bench_base64_flag_solves_repeatedly() {
    let mut walls = Vec::new();
    let mut requests = Vec::new();
    for run in 0..3 {
        let server = spawn(vec![
            tool_call("read_file", r#"{"path":"b64.txt"}"#),
            completion("decoded: flag{bench_b64_ok}"),
        ]);
        let bench = Bench::new(&format!("b64-{run}"), &server.url(""));
        bench.file("b64.txt", b"ZmxhZ3tiZW5jaF9iNjRfb2t9\n");
        let outcome = solve(&bench, &server, "b64.txt", &[]);
        assert!(outcome.verified, "run {run} must verify via decode chain");
        walls.push(outcome.wall);
        requests.push(outcome.requests);
    }
    eprintln!("BENCH b64: walls={walls:?} requests={requests:?}");
}

/// Fixture 3 — extensionless pcap magic: deterministic routing must guess
/// forensics before any model reasoning, then solve like fixture 1.
#[test]
fn bench_pcap_magic_routes_and_solves() {
    let mut blob = b"\xd4\xc3\xb2\xa1\x02\x00\x04\x00".to_vec();
    blob.extend_from_slice(&[0u8; 64]);
    blob.extend_from_slice(b"flag{bench_pcap_ok}\n");
    for run in 0..2 {
        let server = spawn(vec![
            tool_call("read_file", r#"{"path":"capture"}"#),
            completion("found flag{bench_pcap_ok} in the dump"),
        ]);
        let bench = Bench::new(&format!("pcap-{run}"), &server.url(""));
        bench.file("capture", &blob);
        let mut command = Command::new(env!("CARGO_BIN_EXE_wrosecode"));
        command
            .current_dir(&bench.root)
            .env("HOME", &bench.home)
            .env("WROSECODE_MOCK_API_KEY", "test-key")
            .env_remove("WROSECODE_OPENAI_API_KEY")
            .args([
                "ctf",
                "capture",
                "--budget",
                "8",
                "--provider",
                "mock",
                "--model",
                "mock-model",
            ]);
        let started = Instant::now();
        let output: Output = command.output().expect("run wrosecode ctf");
        let wall = started.elapsed();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            stdout.contains("GUESS   forensics"),
            "magic routes before reasoning:\n{stdout}"
        );
        assert!(
            output.status.code() == Some(0) && stdout.contains("✔ flag verified"),
            "run {run} must verify:\n{stdout}"
        );
        eprintln!(
            "BENCH pcap run {run}: wall={wall:?} requests={}",
            server.count()
        );
    }
}
