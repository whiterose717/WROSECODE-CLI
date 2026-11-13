//! End-to-end headless runs against a local OpenAI-compatible mock provider:
//! no network, no API key, deterministic model output.

mod common;

use common::{spawn, Response};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    home: PathBuf,
    root: PathBuf,
    envs: Vec<(String, String)>,
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
        Self {
            home,
            root,
            envs: Vec::new(),
        }
    }

    /// Extra environment for the run, e.g. `WROSECODE_COMPACT_CHARS`.
    fn env(&mut self, key: &str, value: &str) -> &mut Self {
        self.envs.push((key.into(), value.into()));
        self
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
    for (key, value) in &sandbox.envs {
        command.env(key, value);
    }
    command.output().expect("run wrosecode")
}

fn bugbounty_headless(sandbox: &Sandbox, target: &str, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wrosecode"));
    command
        .current_dir(&sandbox.root)
        .env("HOME", &sandbox.home)
        .env("WROSECODE_MOCK_API_KEY", "test-key")
        .env_remove("WROSECODE_OPENAI_API_KEY")
        .args([
            "bugbounty",
            target,
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
        .args(extra);
    for (key, value) in &sandbox.envs {
        command.env(key, value);
    }
    command.output().expect("run wrosecode bugbounty")
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
    raw_tool_call(name, &parsed.to_string())
}

/// A tool call whose `arguments` field is exactly `arguments`, valid JSON or
/// not — the shape a weak model emits when it truncates or wraps its output.
fn raw_tool_call(name: &str, arguments: &str) -> Response {
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
                        "function": {"name": name, "arguments": arguments},
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
fn truncated_tool_arguments_fail_the_call_without_executing_it() {
    // A stream that cuts a write off mid-string must never write the partial
    // content, and must not abort the turn: the model sees a real error and
    // gets a chance to correct it.
    let server = spawn(vec![
        raw_tool_call(
            "write_file",
            r#"{"path":"partial.txt","content":"cut off mid-sentence"#,
        ),
        completion("RECOVERED-AFTER-BAD-JSON"),
    ]);
    let sandbox = Sandbox::new("badjson", &server.url(""));

    let output = headless(&sandbox, "write partial.txt", &[]);
    assert_task_complete(&output, "RECOVERED-AFTER-BAD-JSON");
    assert!(
        !sandbox.root.join("partial.txt").exists(),
        "a truncated write must not run"
    );
    let requests = server.requests();
    assert!(
        requests.len() >= 2,
        "the turn must survive the bad call, saw {} requests",
        requests.len()
    );
    let last = requests.last().expect("at least one request");
    assert!(
        last.body.contains("was not executed"),
        "the model must be told the call did not run: {}",
        last.body
    );
}

#[test]
fn syntax_only_tool_argument_repairs_still_execute() {
    // A trailing comma or fence around otherwise complete arguments is a
    // formatting defect, not missing data: the call should run.
    let server = spawn(vec![
        raw_tool_call(
            "read_file",
            r#"```json
{"path":"challenge.txt",}
```"#,
        ),
        completion("REPAIRED-CALL-SAW-THE-FILE"),
    ]);
    let sandbox = Sandbox::new("repair", &server.url(""));
    sandbox.file("challenge.txt", "REPAIRED-CONTENT-91");

    let output = headless(&sandbox, "read challenge.txt", &[]);
    assert_task_complete(&output, "REPAIRED-CALL-SAW-THE-FILE");
    let requests = server.requests();
    let last = requests.last().expect("at least one request");
    assert!(
        last.body.contains("REPAIRED-CONTENT-91"),
        "the repaired read must return the file: {}",
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
    // A model response with no --goal/--until/--until-cmd does not prove
    // a task was externally verified.
    assert_eq!(value["verified"], false, "{stdout}");
    assert_eq!(value["status"], "unverified", "{stdout}");
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

#[test]
fn editing_turn_captures_an_undo_snapshot_of_the_pre_edit_file() {
    let server = spawn(vec![
        tool_call("read_file", r#"{"path":"app.txt"}"#),
        tool_call(
            "edit_file",
            r#"{"path":"app.txt","old":"before","new":"after"}"#,
        ),
        completion("EDITED"),
    ]);
    let sandbox = Sandbox::new("snapshot", &server.url(""));
    sandbox.file("app.txt", "before\n");

    let output = headless(
        &sandbox,
        "change before to after",
        &["--permission", "auto-safe"],
    );
    assert_task_complete(&output, "EDITED");
    assert_eq!(
        std::fs::read_to_string(sandbox.root.join("app.txt")).unwrap(),
        "after\n"
    );

    let manifest = std::fs::read_to_string(
        sandbox
            .root
            .join(".wrosecode/snapshots/undo/000001/manifest.txt"),
    )
    .expect("undo snapshot was not captured");
    assert!(manifest.contains("+app.txt"), "{manifest}");
    let stored = std::fs::read_to_string(
        sandbox
            .root
            .join(".wrosecode/snapshots/undo/000001/files/app.txt"),
    )
    .unwrap();
    assert_eq!(stored, "before\n");
}

#[test]
fn a_turn_that_changes_nothing_leaves_no_snapshot() {
    let server = spawn(vec![completion("NO-EDITS")]);
    let sandbox = Sandbox::new("nosnapshot", &server.url(""));

    let output = headless(&sandbox, "just answer", &["--permission", "auto-safe"]);
    assert_task_complete(&output, "NO-EDITS");

    assert!(
        !sandbox
            .root
            .join(".wrosecode/snapshots/undo/000001")
            .exists(),
        "a read-only turn must not create an undo snapshot"
    );
}

#[test]
fn a_large_resumed_history_is_compacted_before_the_turn() {
    let server = spawn(vec![
        completion("SUMMARY-MARKER-OK"),
        completion("ANSWER-AFTER-COMPACT"),
    ]);
    let mut sandbox = Sandbox::new("compact", &server.url(""));
    sandbox.env("WROSECODE_COMPACT_CHARS", "64");
    let messages: Vec<serde_json::Value> = (0..6)
        .map(|index| {
            serde_json::json!({
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "content": [{
                    "Text": if index == 0 {
                        "SEED-MARKER-ALFA: rewrite the parser".to_string()
                    } else {
                        format!("turn {index} of an old conversation")
                    }
                }]
            })
        })
        .collect();
    let session = serde_json::json!({
        "name": "big",
        "created": 1,
        "summary": "seeded history",
        "provider_name": "mock",
        "model": "mock-model",
        "messages": messages,
        "transcript": [],
    });
    sandbox.file("big.json", &session.to_string());

    let imported = headless(&sandbox, "import", &["--import-session", "big.json"]);
    assert!(imported.status.success(), "session import failed");

    let output = headless(&sandbox, "continue the task", &["--session", "big"]);
    assert_task_complete(&output, "ANSWER-AFTER-COMPACT");

    let requests = server.requests();
    assert!(
        requests.len() >= 2,
        "expected a compaction call then the turn, got {}",
        requests.len()
    );
    let summarizer = &requests[0].body;
    assert!(
        summarizer.contains("compaction engine"),
        "no compaction request: {summarizer}"
    );
    assert!(
        summarizer.contains("SEED-MARKER-ALFA"),
        "history never reached the summarizer: {summarizer}"
    );

    let turn = &requests[1].body;
    assert!(
        turn.contains("SUMMARY-MARKER-OK"),
        "summary missing from the turn: {turn}"
    );
    assert!(
        !turn.contains("SEED-MARKER-ALFA"),
        "history was not replaced by the summary: {turn}"
    );
}

#[test]
fn events_flag_writes_a_framed_submission_stream() {
    let server = spawn(vec![
        tool_call("read_file", r#"{"path":"notes.txt"}"#),
        completion("STREAMED-ANSWER"),
    ]);
    let sandbox = Sandbox::new("events", &server.url(""));
    sandbox.file("notes.txt", "hello\n");

    let output = headless(&sandbox, "read the notes", &["--events", "events.jsonl"]);
    assert_task_complete(&output, "STREAMED-ANSWER");

    let text = std::fs::read_to_string(sandbox.root.join("events.jsonl"))
        .expect("event stream was not written");
    let frames: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("frame must be one JSON object per line"))
        .collect();
    assert!(!frames.is_empty());
    assert_eq!(frames[0]["type"], "start");
    assert_eq!(frames[0]["prompt"], "read the notes");
    assert!(
        frames.iter().any(|frame| {
            frame["type"] == "step" && frame["phase"] == "end" && frame["ok"] == true
        }),
        "no tool-end frame in {frames:?}"
    );
    let last = frames.last().expect("result frame");
    assert_eq!(last["type"], "result");
    assert_eq!(last["answer"], "STREAMED-ANSWER");
    // No explicit goal validator was supplied: events must not falsely claim
    // that tool completion proves the final answer's correctness.
    assert_eq!(last["verified"], false);
}

#[test]
fn a_user_agent_markdown_reaches_the_system_prompt() {
    let server = spawn(vec![completion("AGENT-OK")]);
    let sandbox = Sandbox::new("useragent", &server.url(""));
    std::fs::create_dir_all(sandbox.root.join(".wrosecode/agents")).unwrap();
    sandbox.file(
        ".wrosecode/agents/reviewer.md",
        "---\ndescription: Reviews diffs\nmode: plan\nthinking: deep\n---\nAlways quote the evidence marker MARK-77.\n",
    );

    let output = headless(&sandbox, "review this", &["--agent", "reviewer"]);
    assert_task_complete(&output, "AGENT-OK");

    let requests = server.requests();
    let first = &requests[0];
    assert!(
        first.body.contains("MARK-77"),
        "agent body never reached the system prompt: {}",
        first.body
    );
    assert!(
        first.body.contains("Active user agent (reviewer)"),
        "agent header missing: {}",
        first.body
    );
    assert!(
        first.body.contains("Mode: plan"),
        "the agent's mode: frontmatter was not applied: {}",
        first.body
    );
}

#[test]
fn a_pinned_file_rides_along_in_the_system_prompt() {
    let server = spawn(vec![completion("PIN-OK")]);
    let sandbox = Sandbox::new("pinned", &server.url(""));
    sandbox.file(
        "notes.txt",
        "PIN-MARKER-31: the live target listens on port 4444 and the flag format is FLAG{...}.",
    );

    let output = headless(&sandbox, "which port is open?", &["--add", "notes.txt"]);
    assert_task_complete(&output, "PIN-OK");

    let requests = server.requests();
    let body = &requests[0].body;
    assert!(
        body.contains("PIN-MARKER-31"),
        "pinned contents never reached the prompt: {body}"
    );
    assert!(
        body.contains("Pinned files"),
        "the pin header is missing from the prompt: {body}"
    );
}

#[test]
fn a_user_command_prompt_expands_before_the_turn() {
    let server = spawn(vec![completion("SHIP-OK")]);
    let sandbox = Sandbox::new("usercmd", &server.url(""));
    std::fs::create_dir_all(sandbox.root.join(".wrosecode/commands")).unwrap();
    sandbox.file(
        ".wrosecode/commands/ship.md",
        "---\ndescription: Ship the branch\n---\nCommit $ARGUMENTS and push.\n",
    );

    let output = headless(&sandbox, "/ship the fix", &[]);
    assert_task_complete(&output, "SHIP-OK");

    let body = &server.requests()[0].body;
    assert!(
        body.contains("Commit the fix and push."),
        "command body was not expanded: {body}"
    );
    assert!(!body.contains("$ARGUMENTS"), "placeholder leaked: {body}");
}

/// The `recipe` subcommand goes through the same argv rewrite as `exec`.
fn recipe_argv(sandbox: &Sandbox, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wrosecode"));
    command
        .current_dir(&sandbox.root)
        .env("HOME", &sandbox.home)
        .env("WROSECODE_MOCK_API_KEY", "test-key")
        .env_remove("WROSECODE_OPENAI_API_KEY")
        .arg("recipe")
        .args(args);
    for (key, value) in &sandbox.envs {
        command.env(key, value);
    }
    command.output().expect("run wrosecode recipe")
}

fn audit_recipe() -> String {
    r#"
name: audit
description: Two-step audit
version: "1"
params:
  target:
    description: host to audit
steps:
  - prompt: "Recon {{target}} and list open ports."
  - command: "echo ports-open-for-{{target}}"
  - prompt: "Given {{prev}}, write the one-line verdict."
"#
    .to_string()
}

#[test]
fn a_recipe_runs_each_step_in_order_with_parameters() {
    let server = spawn(vec![completion("FIRST-OK"), completion("LAST-OK")]);
    let sandbox = Sandbox::new("recipe", &server.url(""));
    std::fs::create_dir_all(sandbox.root.join(".wrosecode/recipes")).unwrap();
    sandbox.file(".wrosecode/recipes/audit.yaml", &audit_recipe());

    let output = recipe_argv(
        &sandbox,
        &[
            "run",
            "audit",
            "--set",
            "target=attackme.local",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--summary",
            "json",
            "--max-wall-time",
            "60",
        ],
    );
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
    assert!(stdout.contains("LAST-OK"), "stdout: {stdout}");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "each prompt step should call the model once: {}",
        requests.len()
    );
    assert!(
        requests[0].body.contains("Recon attackme.local"),
        "the --set parameter never reached step 1: {}",
        requests[0].body
    );
    assert!(
        !requests[0].body.contains("{{target}}"),
        "placeholder leaked into step 1: {}",
        requests[0].body
    );
    assert!(
        requests[1].body.contains("ports-open-for-attackme.local"),
        "the command step's output never reached step 3 via {{prev}}: {}",
        requests[1].body
    );
    assert!(
        requests[1].body.contains("exit=0"),
        "the command step's exit status is missing from {{prev}}: {}",
        requests[1].body
    );
}

#[test]
fn recipe_list_prints_discoverable_recipes() {
    let server = spawn(Vec::new());
    let sandbox = Sandbox::new("recipe-list", &server.url(""));
    std::fs::create_dir_all(sandbox.root.join(".wrosecode/recipes")).unwrap();
    sandbox.file(".wrosecode/recipes/audit.yaml", &audit_recipe());

    let output = recipe_argv(
        &sandbox,
        &["list", "--provider", "mock", "--model", "mock-model"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("audit"), "stdout: {stdout}");
    assert!(stdout.contains("Two-step audit"), "stdout: {stdout}");
    assert!(server.count() == 0, "recipe list should not call the model");
}

#[test]
fn a_forked_run_records_the_session_it_forked_from() {
    let server = spawn(vec![completion("FORK-OK")]);
    let sandbox = Sandbox::new("fork", &server.url(""));
    let seed = serde_json::json!({
        "name": "seed",
        "created": 1,
        "summary": "seeded history",
        "provider_name": "mock",
        "model": "mock-model",
        "messages": [{
            "role": "user",
            "content": [{ "Text": "SEED-FROM: the original session" }]
        }],
        "transcript": [],
    });
    sandbox.file("seed.json", &seed.to_string());
    let imported = headless(&sandbox, "import", &["--import-session", "seed.json"]);
    assert!(imported.status.success(), "session import failed");

    let output = headless(&sandbox, "keep going", &["--session", "seed", "--fork"]);
    assert_task_complete(&output, "FORK-OK");

    let dir = sandbox.home.join(".wrosecode/sessions");
    let mut forked_parent = None;
    for entry in std::fs::read_dir(&dir).expect("sessions dir") {
        let path = entry.expect("session entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read session")).unwrap();
        if stored["parent"].is_string() {
            forked_parent = stored["parent"].as_str().map(str::to_string);
        }
    }
    assert_eq!(
        forked_parent.as_deref(),
        Some("seed"),
        "the fork never recorded its parent session"
    );
}

#[test]
fn fork_at_a_message_keeps_only_the_prefix_of_the_context() {
    let server = spawn(vec![completion("TRUNC-OK")]);
    let sandbox = Sandbox::new("fork-at", &server.url(""));
    let seed = serde_json::json!({
        "name": "seed",
        "created": 1,
        "summary": "seeded history",
        "provider_name": "mock",
        "model": "mock-model",
        "messages": [
            {"role": "user", "content": [{"Text": "K1"}]},
            {"role": "assistant", "content": [{"Text": "A1"}]},
            {"role": "user", "content": [{"Text": "K2"}]}
        ],
        "transcript": [],
    });
    sandbox.file("seed.json", &seed.to_string());
    let imported = headless(&sandbox, "import", &["--import-session", "seed.json"]);
    assert!(imported.status.success(), "session import failed");

    // Keep only the first message: A1 and K2 must not reach the branch.
    let output = headless(
        &sandbox,
        "keep going",
        &["--session", "seed", "--fork-at", "1"],
    );
    assert_task_complete(&output, "TRUNC-OK");

    let dir = sandbox.home.join(".wrosecode/sessions");
    let mut forked: Option<serde_json::Value> = None;
    for entry in std::fs::read_dir(&dir).expect("sessions dir") {
        let path = entry.expect("session entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read session")).unwrap();
        if stored["parent"].as_str() == Some("seed") {
            forked = Some(stored);
        }
    }
    let forked = forked.expect("no forked session was written");
    let messages = forked["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        3,
        "kept prefix + prompt + response: {forked}"
    );
    assert_eq!(messages[0]["content"][0]["Text"], "K1");
    assert!(
        !forked["messages"].to_string().contains("A1")
            && !forked["messages"].to_string().contains("K2"),
        "a truncated-away message leaked into the fork: {forked}"
    );
}

#[test]
fn computer_tools_stay_hidden_until_opted_in() {
    let server = spawn(vec![
        tool_call("computer", "{\"action\":\"click\",\"x\":10,\"y\":20}"),
        completion("DONE-NO-COMPUTER"),
    ]);
    let sandbox = Sandbox::new("computer-off", &server.url(""));

    let output = headless(&sandbox, "click somewhere", &[]);
    assert_task_complete(&output, "DONE-NO-COMPUTER");

    let requests = server.requests();
    let first = &requests[0];
    assert!(
        !first.body.contains("Control this machine's desktop"),
        "the computer schema must not be advertised without the opt-in: {}",
        first.body
    );
    let last = requests.last().expect("at least one request");
    assert!(
        last.body.contains("computer tools are off"),
        "the refusal must reach the model: {}",
        last.body
    );
}

#[cfg(unix)]
#[test]
fn computer_tools_run_once_opted_in() {
    use std::os::unix::fs::PermissionsExt;

    let server = spawn(vec![
        tool_call("computer", "{\"action\":\"click\",\"x\":7,\"y\":9}"),
        completion("DONE-COMPUTER"),
    ]);
    let mut sandbox = Sandbox::new("computer-on", &server.url(""));

    // A stand-in xdotool that records the argv it was given, one per line.
    let bin = sandbox.root.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let backend = bin.join("xdotool");
    std::fs::write(
        &backend,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > argv.txt\necho clicked\n",
    )
    .unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    sandbox.env("PATH", &path);

    let output = headless(
        &sandbox,
        "click the thing",
        &["--computer", "--permission", "auto-safe"],
    );
    assert_task_complete(&output, "DONE-COMPUTER");

    let argv = std::fs::read_to_string(sandbox.root.join("argv.txt"))
        .expect("the recorded argv file was written");
    assert_eq!(argv, "mousemove\n--sync\n7\n9\nclick\n1\n");

    let requests = server.requests();
    let last = requests.last().expect("at least one request");
    assert!(
        last.body.contains("clicked"),
        "the tool output must round-trip to the model: {}",
        last.body
    );
}

#[test]
fn desktop_listing_runs_a_real_tool_without_indexing_other_projects() {
    let server = spawn(vec![
        tool_call("shell", r#"{"command":"ls -1 Desktop"}"#),
        completion("DIRECTORY-LISTING-COMPLETE"),
    ]);
    let sandbox = Sandbox::new("desktop-listing", &server.url(""));
    std::fs::create_dir_all(sandbox.root.join("Desktop")).unwrap();
    std::fs::write(
        sandbox.root.join("Desktop/ACTUAL-DESKTOP-ENTRY.txt"),
        "test",
    )
    .unwrap();
    sandbox.file("AGENTS.md", "KEEP-DIRECTORY-INSTRUCTIONS");
    for i in 0..350 {
        sandbox.file(
            &format!("unrelated_{i}.py"),
            "def irrelevant(): return something\n",
        );
    }
    let output = headless(
        &sandbox,
        "list dir in desktop",
        &["--permission", "yolo", "--trace"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\\nstderr: {stderr}"
    );
    assert!(stdout.contains("\"type\":\"TaskComplete\""), "{stdout}");
    assert!(
        stdout.contains("ACTUAL-DESKTOP-ENTRY.txt"),
        "the listing must use the actual filesystem, not a synthetic answer: {stdout}"
    );
    assert!(
        server.requests().is_empty(),
        "simple directory listings should not call the model"
    );
    let trace = std::fs::read_to_string(sandbox.home.join(".wrosecode/trace.log")).unwrap();
    assert!(
        !trace.contains("\"phase\":\"repo_map\""),
        "listing must not index unrelated source files"
    );
}

#[test]
fn bugbounty_requires_an_explicit_scope_before_any_model_call() {
    let server = spawn(vec![completion("this answer must never be requested")]);
    let sandbox = Sandbox::new("bounty-noscope", &server.url(""));

    let output = bugbounty_headless(&sandbox, "https://app.example.test", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a live target without a scope must fail: stdout {stdout} stderr {stderr}"
    );
    assert!(
        stderr.contains("no in-scope hosts"),
        "the refusal must name the missing scope: {stderr}"
    );
    assert!(
        server.requests().is_empty(),
        "scope validation must happen before the first model call"
    );
}

#[test]
fn bugbounty_records_evidence_and_writes_a_verified_report_end_to_end() {
    let add = serde_json::json!({
        "action": "add",
        "title": "IDOR in invoice endpoint",
        "asset": "https://app.example.test/api/invoices/42",
        "category": "idor",
        "severity": "high",
        "summary": "Another tenant's invoice is readable"
    })
    .to_string();
    let request = serde_json::json!({
        "action": "evidence",
        "finding_id": "BB-0001",
        "kind": "request",
        "content": "GET /api/invoices/42 HTTP/1.1\nHost: app.example.test\nAuthorization: Bearer SECRET-TOKEN-123\n"
    })
    .to_string();
    let response = serde_json::json!({
        "action": "evidence",
        "finding_id": "BB-0001",
        "kind": "response",
        "content": "HTTP/1.1 200 OK\n\n{\"tenant\":\"other\",\"total\":999,\"email\":\"victim@example.test\"}"
    })
    .to_string();
    let reproduce = serde_json::json!({
        "action": "reproduce",
        "finding_id": "BB-0001",
        "steps": [
            "Sign in as tenant A",
            "Request tenant B invoice 42",
            "Observe HTTP 200 with tenant B data"
        ],
        "observed": "HTTP 200 returned another tenant's invoice",
        "expected": "HTTP 403 or 404 for another tenant's object"
    })
    .to_string();
    let scope = serde_json::json!({
        "action": "scope",
        "finding_id": "BB-0001",
        "in_scope": true,
        "note": "asset matched the armed wildcard"
    })
    .to_string();
    let update = serde_json::json!({
        "action": "update",
        "finding_id": "BB-0001",
        "impact": "Cross-tenant invoice disclosure",
        "remediation": "Authorize object ownership before serialization"
    })
    .to_string();
    let verify = serde_json::json!({
        "action": "verify",
        "finding_id": "BB-0001"
    })
    .to_string();
    let report = serde_json::json!({"action": "report"}).to_string();

    let server = spawn(vec![
        tool_call("bugbounty", &add),
        tool_call("bugbounty", &request),
        tool_call("bugbounty", &response),
        tool_call("bugbounty", &reproduce),
        tool_call("bugbounty", &scope),
        tool_call("bugbounty", &update),
        tool_call("bugbounty", &verify),
        tool_call("bugbounty", &report),
        completion("BB-0001 is verified and reported. BOUNTY COMPLETE"),
    ]);
    let sandbox = Sandbox::new("bounty-flow", &server.url(""));

    let output = bugbounty_headless(
        &sandbox,
        "https://app.example.test",
        &[
            "--scope",
            "*.example.test",
            "--exclude",
            "admin.example.test",
            "--program",
            "Example Program",
            "--permission",
            "yolo",
            "--budget",
            "40",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("VERIFIED 1"), "{stdout}");
    assert!(stdout.contains("Example Program"), "{stdout}");

    let workspace_path = sandbox.root.join(".bugbounty/findings.json");
    let workspace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&workspace_path).unwrap()).unwrap();
    assert_eq!(workspace["program"], "Example Program");
    assert_eq!(workspace["findings"][0]["id"], "BB-0001");
    assert_eq!(workspace["findings"][0]["state"], "reported");
    assert_eq!(
        workspace["findings"][0]["evidence"]
            .as_array()
            .expect("evidence array")
            .len(),
        2
    );
    assert!(
        workspace["scope"]["includes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "*.example.test"),
        "{workspace}"
    );
    assert!(
        workspace["scope"]["excludes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "admin.example.test"),
        "{workspace}"
    );

    let report =
        std::fs::read_to_string(sandbox.root.join(".bugbounty/reports/BUG_BOUNTY_REPORT.md"))
            .expect("verified report exists");
    assert!(report.contains("IDOR in invoice endpoint"), "{report}");
    assert!(
        report.contains("Cross-tenant invoice disclosure"),
        "{report}"
    );
    assert!(report.contains("SHA-256"), "{report}");
    assert!(
        !report.contains("SECRET-TOKEN-123"),
        "the report leaked a bearer token: {report}"
    );

    let requests = server.requests();
    assert!(
        requests.len() >= 9,
        "expected one round trip per gate, saw {}",
        requests.len()
    );
    assert!(
        requests[0].body.contains("BUG BOUNTY AUTOPILOT"),
        "the first prompt never described the workflow: {}",
        requests[0].body
    );
    assert!(
        requests[0].body.contains("admin.example.test"),
        "exclusions never reached the model: {}",
        requests[0].body
    );
    assert!(
        requests.last().unwrap().body.contains("Reports written"),
        "the report tool result never reached the model: {}",
        requests.last().unwrap().body
    );
}

#[test]
fn bugbounty_scoped_run_without_findings_exits_two_and_creates_no_report() {
    let server = spawn(vec![completion(
        "No vulnerability was verified. BOUNTY COMPLETE",
    )]);
    let sandbox = Sandbox::new("bounty-empty", &server.url(""));

    let output = bugbounty_headless(
        &sandbox,
        "https://app.example.test",
        &["--scope", "app.example.test"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "no verified finding must exit 2: stdout {stdout} stderr {stderr}"
    );
    assert!(stdout.contains("VERIFIED 0"), "{stdout}");
    assert!(
        sandbox.root.join(".bugbounty/findings.json").is_file(),
        "the scoped workspace must still be created"
    );
    assert!(
        !sandbox
            .root
            .join(".bugbounty/reports/BUG_BOUNTY_REPORT.md")
            .exists(),
        "an empty run must not write a report"
    );
    assert!(
        stdout.contains("no finding reached verified"),
        "the stop report must say why there is no report: {stdout}"
    );
}
