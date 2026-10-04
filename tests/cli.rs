//! Argument-surface smoke tests: the CLI stays parseable, documented, and loud
//! about bad input. None of these touch the network or a model provider.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_wrosecode"))
}

#[test]
fn version_prints_the_package_version() {
    let output = bin().arg("--version").output().expect("run --version");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wrosecode"), "stdout: {stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout: {stdout}"
    );
}

#[test]
fn help_documents_the_operating_surface() {
    let output = bin().arg("--help").output().expect("run --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--headless",
        "--permission",
        "--summary",
        "--max-wall-time",
        "--race",
        "--session",
        "--think",
    ] {
        assert!(stdout.contains(flag), "--help is missing {flag}");
    }
}

#[test]
fn unknown_flags_fail_instead_of_being_swallowed() {
    let output = bin()
        .arg("--definitely-not-a-real-flag")
        .output()
        .expect("run bad flag");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--definitely-not-a-real-flag"),
        "stderr: {stderr}"
    );
}

#[test]
fn ctfd_verify_documents_exit_codes_and_retries() {
    let output = bin()
        .args(["ctfd", "verify", "--help"])
        .output()
        .expect("run ctfd verify --help");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("VERIFIED"), "stdout: {stdout}");
    assert!(stdout.contains("REJECTED"), "stdout: {stdout}");
    assert!(stdout.contains("--retries"), "stdout: {stdout}");
    assert!(stdout.contains("--backoff-ms"), "stdout: {stdout}");
}

#[test]
fn ctfd_verify_without_configuration_fails_clearly() {
    let output = bin()
        .args(["ctfd", "verify", "1", "flag{nope}"])
        .env_remove("CTFD_URL")
        .env_remove("CTFD_TOKEN")
        .output()
        .expect("run ctfd verify");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("CTFD_URL"), "stderr: {stderr}");
}

#[test]
fn a_panic_restores_the_terminal_and_writes_a_redacted_crash_report() {
    let home = std::env::temp_dir().join(format!("wrose-crash-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("temp home");
    let output = bin()
        .env("HOME", &home)
        .env("API_KEY", "sk-should-never-appear-in-a-report")
        .env("WROSECODE_FORCE_PANIC", "1")
        .current_dir(&home)
        .output()
        .expect("run forced panic");
    assert!(!output.status.success(), "panic must not exit 0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("internal error"), "stderr: {stderr}");
    assert!(stderr.contains("Crash report:"), "stderr: {stderr}");

    let crash_dir = home.join(".wrosecode").join("crash");
    let mut reports = Vec::new();
    for entry in std::fs::read_dir(&crash_dir).expect("crash dir") {
        let entry = entry.expect("crash entry");
        if entry.path().extension().is_some_and(|ext| ext == "log") {
            reports.push(entry.path());
        }
    }
    assert_eq!(reports.len(), 1, "expected exactly one .log report");
    let body = std::fs::read_to_string(&reports[0]).expect("read report");
    assert!(body.contains("kind: panic"), "{body}");
    assert!(body.contains("forced panic"), "{body}");
    assert!(
        !body.contains("sk-should-never-appear-in-a-report"),
        "report leaked a secret: {body}"
    );
    assert!(body.contains("API_KEY=***"), "{body}");

    // The project error log next to the cwd stays redacted too.
    let errors = std::fs::read_to_string(home.join(".ctf").join("errors.log")).unwrap_or_default();
    assert!(
        !errors.contains("sk-should-never-appear-in-a-report"),
        "{errors}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
