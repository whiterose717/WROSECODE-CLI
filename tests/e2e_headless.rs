//! End-to-end headless runs against a local OpenAI-compatible mock provider:
//! no network, no API key, deterministic model output.

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
        let unique = format!("wrose-e2e-{}-{}", label, std::process::id());
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

fn headless(sandbox: &Sandbox, prompt: &str, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wrosecode"));
    command
        .current_dir(&sandbox.root)
        .env("HOME", &sandbox.home)
        .env("WROSECODE_MOCK_API_KEY", "test-key")
        .env_remove("WROSECODE_OPENAI_API_KEY")
        .args([
            "--headless",
            "--quiet",
            "--summary",
            "json",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--max-wall-time",
            "60",
        ])
        .args(extra)
        .arg(prompt);
    command.output().expect("run wrosecode")
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

fn assert_task_complete(output: &Output, needle: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("\"type\":\"TaskComplete\""),
        "stdout: {stdout}"
    );
    assert!(stdout.contains(needle), "stdout: {stdout}");
    assert!(stdout.contains("\"status\""), "stdout: {stdout}");
}

#[test]
fn single_turn_answer_runs_end_to_end() {
    let prompt = "what is the answer?";
    let server = spawn(vec![completion("MOCK-ANSWER-42")]);
    let sandbox = Sandbox::new("single", &server.url(""));

    let output = headless(&sandbox, prompt, &[]);
    assert_task_complete(&output, "MOCK-ANSWER-42");

    assert!(server.count() >= 1, "provider was never contacted");
    let requests = server.requests();
    let first = &requests[0];
    assert_eq!(first.method, "POST");
    assert!(
        first.path.contains("/chat/completions"),
        "path: {}",
        first.path
    );
    assert!(
        first.body.contains(prompt),
        "prompt never reached the model"
    );
    assert!(first.body.contains("mock-model"), "body: {}", first.body);
    assert!(
        first.body.contains("WROSECODE") || first.body.contains("system"),
        "no system prompt"
    );
}

#[test]
fn tool_calls_are_executed_and_returned_to_the_model() {
    let server = spawn(vec![
        tool_call("read_file", "{\"path\":\"challenge.txt\"}"),
        completion("MOCK-ANSWER-FROM-TOOL"),
    ]);
    let sandbox = Sandbox::new("tool", &server.url(""));
    sandbox.file("challenge.txt", "MOCK-CONTENT-77");

    let output = headless(&sandbox, "read challenge.txt", &[]);
    assert_task_complete(&output, "MOCK-ANSWER-FROM-TOOL");

    let requests = server.requests();
    assert!(
        requests.len() >= 2,
        "expected a tool round trip, saw {} requests",
        requests.len()
    );
    let last = requests.last().expect("at least one request");
    assert!(
        last.body.contains("MOCK-CONTENT-77"),
        "tool output never made it back to the model: {}",
        last.body
    );
    assert!(
        last.body.contains("\"role\":\"tool\""),
        "expected a tool message: {}",
        last.body
    );
}

#[test]
fn until_flag_verifies_the_final_answer() {
    let server = spawn(vec![completion("the answer is 42")]);
    let sandbox = Sandbox::new("until", &server.url(""));

    let output = headless(&sandbox, "solve it", &["--until", "answer is 42"]);
    assert_task_complete(&output, "the answer is 42");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"status\":\"verified\""),
        "stdout: {stdout}"
    );
}

#[test]
fn unmet_until_flag_reports_unverified() {
    let server = spawn(vec![completion("no idea")]);
    let sandbox = Sandbox::new("unverified", &server.url(""));

    let output = headless(&sandbox, "solve it", &["--until", "flag\\{expected\\}"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // An unmet --until contract exits 2 so shell pipelines can branch on it.
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("\"status\":\"unverified\""),
        "stdout: {stdout}"
    );
}

#[test]
fn json_summary_reports_usage_from_the_provider() {
    let server = spawn(vec![completion("done")]);
    let sandbox = Sandbox::new("usage", &server.url(""));

    let output = headless(&sandbox, "hi", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let summary_line = stdout
        .lines()
        .find(|line| line.contains("\"type\":\"TaskComplete\""))
        .expect("summary line");
    let value: serde_json::Value = serde_json::from_str(summary_line).expect("valid json summary");
    assert_eq!(value["usage"]["input"], 11, "{summary_line}");
    assert_eq!(value["usage"]["output"], 3, "{summary_line}");
    assert_eq!(value["model_turns"], 1, "{summary_line}");
    let _ = Path::new(&sandbox.root);
}

#[test]
fn exec_json_emits_the_dashboard_snapshot() {
    let server = spawn(vec![completion("ok")]);
    let sandbox = Sandbox::new("exec-json", &server.url(""));

    // `exec` must be argv[1]: main rewrites it to --headless + --json.
    let output = Command::new(env!("CARGO_BIN_EXE_wrosecode"))
        .current_dir(&sandbox.root)
        .env("HOME", &sandbox.home)
        .env("WROSECODE_MOCK_API_KEY", "test-key")
        .env_remove("WROSECODE_OPENAI_API_KEY")
        .args([
            "exec",
            "--json",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--max-wall-time",
            "60",
            "say ok",
        ])
        .output()
        .expect("run wrosecode exec --json");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("exec --json prints one JSON document");
    assert_eq!(value["schema"], "wrosecode/live-v1", "{stdout}");
    assert_eq!(value["answer"], "ok", "{stdout}");
    assert!(value["verified"].as_bool().unwrap_or(false), "{stdout}");
    assert!(value["mode"].is_string(), "{stdout}");
    assert_eq!(value["provider"], "mock", "{stdout}");
    assert_eq!(value["model"], "mock-model", "{stdout}");
}

#[test]
fn agents_md_chain_reaches_the_system_prompt() {
    let server = spawn(vec![completion("CHAIN-OK")]);
    let sandbox = Sandbox::new("instructions", &server.url(""));
    sandbox.file("AGENTS.md", "always answer with the marker ALFA-91");
    sandbox.file("WROSECODE.md", "project guide marker BRAVO-42");

    let output = headless(&sandbox, "hi", &[]);
    assert_task_complete(&output, "CHAIN-OK");

    let requests = server.requests();
    let first = &requests[0];
    assert!(
        first.body.contains("ALFA-91"),
        "AGENTS.md never reached the system prompt: {}",
        first.body
    );
    assert!(
        first.body.contains("BRAVO-42"),
        "WROSECODE.md never reached the system prompt: {}",
        first.body
    );
    assert!(
        first.body.contains("Project instructions"),
        "chain header missing: {}",
        first.body
    );
}

#[test]
fn apply_patch_adds_a_file_and_the_model_sees_the_result() {
    let server = spawn(vec![
        tool_call(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** Add File: notes/hello.txt\n+from patch\n*** End Patch"}"#,
        ),
        completion("PATCH-OK"),
    ]);
    let sandbox = Sandbox::new("patch", &server.url(""));

    let output = headless(&sandbox, "add a note", &["--permission", "auto-safe"]);
    assert_task_complete(&output, "PATCH-OK");

    assert_eq!(
        std::fs::read_to_string(sandbox.root.join("notes/hello.txt")).unwrap(),
        "from patch\n"
    );
    let requests = server.requests();
    assert!(requests.len() >= 2, "no tool round trip");
    assert!(
        requests.last().unwrap().body.contains("applied patch"),
        "model never saw the patch result: {}",
        requests.last().unwrap().body
    );
}
