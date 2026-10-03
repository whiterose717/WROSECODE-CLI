//! CTFd `verify` end-to-end: exit codes, retry/backoff behaviour, and the exact
//! request the CLI sends to the platform.

mod common;

use common::{spawn, Response};
use std::process::Command;

fn verify(url: &str, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wrosecode"));
    command
        .env("CTFD_URL", url)
        .env("CTFD_TOKEN", "test-token")
        .args(["ctfd", "verify"])
        .args(args);
    command.output().expect("run ctfd verify")
}

fn correct() -> Response {
    Response::json(r#"{"success":true,"data":{"status":"correct","message":"Correct flag"}}"#)
}

fn rejected() -> Response {
    Response::json(r#"{"success":true,"data":{"status":"incorrect","message":"Wrong flag"}}"#)
}

#[test]
fn correct_flag_exits_zero_and_sends_the_expected_payload() {
    let server = spawn(vec![correct()]);
    let output = verify(&server.url(""), &["7", "flag{ok}"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("VERIFIED"), "stdout: {stdout}");
    assert!(stdout.contains("challenge=7"), "stdout: {stdout}");
    assert!(stdout.contains("retries=0"), "stdout: {stdout}");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/v1/challenges/attempt");
    let payload: serde_json::Value = serde_json::from_str(&request.body).expect("json payload");
    assert_eq!(payload["challenge_id"], 7);
    assert_eq!(payload["submission"], "flag{ok}");
}

#[test]
fn wrong_flag_exits_two() {
    let server = spawn(vec![rejected()]);
    let output = verify(&server.url(""), &["7", "flag{bad}"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("REJECTED"), "stdout: {stdout}");
}

#[test]
fn rate_limited_attempts_are_retried() {
    let server = spawn(vec![
        Response::with_status(429, r#"{"success":false,"info":"slow down"}"#),
        Response::with_status(429, r#"{"success":false,"info":"slow down"}"#),
        correct(),
    ]);
    let output = verify(
        &server.url(""),
        &["7", "flag{ok}", "--retries", "3", "--backoff-ms", "10"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("VERIFIED"), "stdout: {stdout}");
    assert!(stdout.contains("retries=2"), "stdout: {stdout}");
    assert_eq!(server.count(), 3, "every attempt should hit the platform");
}

#[test]
fn retries_are_bounded_by_the_flag() {
    let server = spawn(vec![Response::with_status(
        429,
        r#"{"success":false,"info":"slow down"}"#,
    )]);
    let output = verify(
        &server.url(""),
        &["7", "flag{ok}", "--retries", "1", "--backoff-ms", "10"],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected an inconclusive exit"
    );
    assert_eq!(server.count(), 2, "one attempt plus one retry");
}

#[test]
fn verdictless_response_is_inconclusive() {
    let server = spawn(vec![Response::json(r#"{"success":true,"data":{}}"#)]);
    let output = verify(&server.url(""), &["7", "flag{ok}", "--retries", "0"]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("INCONCLUSIVE"), "stdout: {stdout}");
}
