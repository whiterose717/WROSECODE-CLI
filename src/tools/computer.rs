//! open-interpreter parity: the opt-in `computer` tool for OS-level control.
//!
//! Nothing here runs unless the user turns it on — `[tools] computer = true`
//! in config.toml or `--computer` on the command line — and every call still
//! passes through `Tools::authorize`, so `ask` prompts, plan mode refuses,
//! and only `yolo` drives the desktop unattended. Backends are probed from
//! PATH (grim / gnome-screenshot / scrot / import / screencapture for
//! screenshots, xdotool for input) and spawned with a literal argv, never a
//! shell string, so typed text and coordinates stay data instead of code.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Actions the tool accepts; every rejection repeats this list so the model
/// can correct itself without a second round trip.
const ACTIONS: [&str; 5] = ["screenshot", "click", "type", "key", "scroll"];

/// Screenshot backends, best first: Wayland, GNOME, scrot, ImageMagick, macOS.
const SHOT_BACKENDS: [&str; 5] = [
    "grim",
    "gnome-screenshot",
    "scrot",
    "import",
    "screencapture",
];

/// The one input backend (X11 click / type / key / scroll).
const INPUT_BACKEND: &str = "xdotool";

/// Validate and return the requested action. Dispatch calls this before the
/// approval prompt so a typo is an error, not a permission dialog.
pub fn action_of(input: &Value) -> Result<&str> {
    let Some(action) = input.get("action").and_then(Value::as_str) else {
        bail!("computer needs an action ({})", ACTIONS.join(", "));
    };
    if !ACTIONS.contains(&action) {
        bail!(
            "unknown computer action {action:?} (expected {})",
            ACTIONS.join(", ")
        );
    }
    Ok(action)
}

pub async fn perform(root: &Path, input: &Value, action: &str, timeout: u64) -> Result<String> {
    perform_with_path(root, input, action, timeout, std::env::var_os("PATH")).await
}

async fn perform_with_path(
    root: &Path,
    input: &Value,
    action: &str,
    timeout: u64,
    path: Option<OsString>,
) -> Result<String> {
    match action {
        "screenshot" => screenshot(root, input, timeout, path).await,
        "click" => click(root, input, timeout, path).await,
        "type" => type_text(root, input, timeout, path).await,
        "key" => key(root, input, timeout, path).await,
        "scroll" => scroll(root, input, timeout, path).await,
        _ => bail!("unknown computer action {action:?}"),
    }
}

/// A non-empty string argument, with the NUL byte rejected up front so
/// `Command::arg` can never panic on it.
fn argv_text<'a>(text: &'a str, what: &str) -> Result<&'a str> {
    if text.is_empty() {
        bail!("computer needs {what}");
    }
    if text.contains('\0') {
        bail!("computer {what} cannot contain NUL bytes");
    }
    Ok(text)
}

fn number(input: &Value, key: &str) -> Result<i64> {
    input
        .get(key)
        .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|v| v as i64)))
        .ok_or_else(|| anyhow::anyhow!("computer needs {key} as a number"))
}

fn number_or(input: &Value, key: &str, fallback: i64) -> Result<i64> {
    match input.get(key) {
        None => Ok(fallback),
        Some(value) => value
            .as_i64()
            .or_else(|| value.as_f64().map(|v| v as i64))
            .ok_or_else(|| anyhow::anyhow!("computer {key} must be a number")),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn find_bin(names: &[&str], path: Option<OsString>) -> Option<PathBuf> {
    let path = path?;
    for name in names {
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn backend(names: &[&str], path: Option<OsString>) -> Result<PathBuf> {
    find_bin(names, path).ok_or_else(|| {
        anyhow::anyhow!(
            "computer needs one of {} on PATH (install {})",
            names.join(", "),
            names.join(", ")
        )
    })
}

/// Spawn the probed backend with a literal argv — no shell — under the same
/// timeout the `shell` tool uses, and report the outcome in its format.
async fn run_argv(program: &Path, args: &[String], cwd: &Path, timeout: u64) -> Result<String> {
    let mut process = tokio::process::Command::new(program);
    process
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout.max(1)),
        process.output(),
    )
    .await
    .with_context(|| format!("{} timed out after {timeout} seconds", program.display()))??;
    Ok(format!(
        "exit={}\n{}{}",
        result.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    ))
}

fn default_shot(root: &Path) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    crate::tools::fs::resolve(root, &format!(".wrosecode/computer/shot-{stamp}.png"))
}

async fn screenshot(
    root: &Path,
    input: &Value,
    timeout: u64,
    path: Option<OsString>,
) -> Result<String> {
    let target = match input.get("path").and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => {
            crate::tools::fs::resolve(root, argv_text(text, "path")?)?
        }
        _ => default_shot(root)?,
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).context("create the screenshot directory")?;
    }
    let program = backend(&SHOT_BACKENDS, path)?;
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let shot = target.display().to_string();
    let args: Vec<String> = match name.as_str() {
        "gnome-screenshot" => vec!["-f".into(), shot],
        "scrot" => vec![shot],
        "import" => vec!["-window".into(), "root".into(), shot],
        "screencapture" => vec!["-x".into(), shot],
        _ => vec![shot],
    };
    let text = run_argv(&program, &args, root, timeout).await?;
    if !text.starts_with("exit=0") {
        bail!("screenshot with {name} failed:\n{text}");
    }
    if !target.is_file() {
        bail!(
            "{name} reported success but wrote no file at {}:\n{text}",
            target.display()
        );
    }
    let bytes = std::fs::metadata(&target)
        .map(|meta| meta.len())
        .unwrap_or(0);
    Ok(format!(
        "saved {} ({bytes} bytes)\n{text}",
        target.display()
    ))
}

async fn click(root: &Path, input: &Value, timeout: u64, path: Option<OsString>) -> Result<String> {
    let x = number(input, "x")?;
    let y = number(input, "y")?;
    if !(0..=10_000).contains(&x) || !(0..=10_000).contains(&y) {
        bail!("computer click coordinates ({x}, {y}) are off screen (expected 0..=10000)");
    }
    let button = match input.get("button") {
        None | Some(Value::Null) => "1".to_string(),
        Some(Value::String(text)) => match text.as_str() {
            "left" | "1" => "1".to_string(),
            "middle" | "2" => "2".to_string(),
            "right" | "3" => "3".to_string(),
            other => bail!("unknown mouse button {other:?} (use left, middle, right, or 1..=5)"),
        },
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value @ 1..=5) => value.to_string(),
            _ => bail!("unknown mouse button {number} (use left, middle, right, or 1..=5)"),
        },
        Some(_) => bail!("computer button must be left, middle, right, or 1..=5"),
    };
    let args = [
        "mousemove".to_string(),
        "--sync".to_string(),
        x.to_string(),
        y.to_string(),
        "click".to_string(),
        button,
    ];
    run_argv(&backend(&[INPUT_BACKEND], path)?, &args, root, timeout).await
}

async fn type_text(
    root: &Path,
    input: &Value,
    timeout: u64,
    path: Option<OsString>,
) -> Result<String> {
    let text = argv_text(
        input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "text",
    )?;
    let args = [
        "type".to_string(),
        "--clearmodifiers".to_string(),
        "--delay".to_string(),
        "12".to_string(),
        text.to_string(),
    ];
    run_argv(&backend(&[INPUT_BACKEND], path)?, &args, root, timeout).await
}

async fn key(root: &Path, input: &Value, timeout: u64, path: Option<OsString>) -> Result<String> {
    let keys = argv_text(
        input
            .get("keys")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "keys",
    )?;
    let args = ["key".to_string(), keys.to_string()];
    run_argv(&backend(&[INPUT_BACKEND], path)?, &args, root, timeout).await
}

async fn scroll(
    root: &Path,
    input: &Value,
    timeout: u64,
    path: Option<OsString>,
) -> Result<String> {
    let amount = number_or(input, "amount", 1)?;
    if amount == 0 {
        bail!("computer scroll amount cannot be zero (positive scrolls up, negative down)");
    }
    let clicks = amount.unsigned_abs().min(50);
    let button = if amount > 0 { "4" } else { "5" };
    let args = [
        "click".to_string(),
        "--repeat".to_string(),
        clicks.to_string(),
        button.to_string(),
    ];
    run_argv(&backend(&[INPUT_BACKEND], path)?, &args, root, timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wrose-computer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An executable that records its argv (one argument per line) and exits 0.
    #[cfg(unix)]
    fn recording_backend(dir: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > argv.txt\necho ok\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_action_gate_accepts_the_five_actions_and_rejects_everything_else() {
        for action in ACTIONS {
            assert_eq!(action_of(&json!({"action": action})).unwrap(), action);
        }
        let missing = action_of(&json!({})).unwrap_err().to_string();
        assert!(missing.contains("needs an action"), "{missing}");
        let unknown = action_of(&json!({"action": "shutdown"}))
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("screenshot"), "{unknown}");
    }

    #[cfg(unix)]
    #[test]
    fn backends_must_be_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workspace("exec");
        let tool = dir.join("xdotool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        assert!(
            find_bin(&["xdotool"], Some(dir.as_os_str().to_owned())).is_none(),
            "a non-executable file is not a backend"
        );
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            find_bin(&["xdotool"], Some(dir.as_os_str().to_owned())).is_some(),
            "an executable on PATH is"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn input_actions_reach_the_backend_as_literal_arguments() {
        let root = workspace("argv");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        recording_backend(&bin, "xdotool");
        let path = Some(bin.as_os_str().to_owned());

        let out = perform_with_path(
            &root,
            &json!({"action":"click","x":7,"y":9}),
            "click",
            5,
            path.clone(),
        )
        .await
        .unwrap();
        assert!(out.starts_with("exit=0"), "{out}");
        let argv = std::fs::read_to_string(root.join("argv.txt")).unwrap();
        assert_eq!(argv, "mousemove\n--sync\n7\n9\nclick\n1\n");

        // Shell metacharacters in typed text stay one argument, not a command.
        let out = perform_with_path(
            &root,
            &json!({"action":"type","text":"hello; rm -rf /"}),
            "type",
            5,
            path.clone(),
        )
        .await
        .unwrap();
        assert!(out.starts_with("exit=0"), "{out}");
        let argv = std::fs::read_to_string(root.join("argv.txt")).unwrap();
        assert!(
            argv.starts_with("type\n--clearmodifiers\n--delay\n12\n"),
            "{argv}"
        );
        assert!(argv.ends_with("hello; rm -rf /\n"), "{argv}");

        let out = perform_with_path(
            &root,
            &json!({"action":"scroll","amount":-3}),
            "scroll",
            5,
            path,
        )
        .await
        .unwrap();
        assert!(out.starts_with("exit=0"), "{out}");
        let argv = std::fs::read_to_string(root.join("argv.txt")).unwrap();
        assert_eq!(argv, "click\n--repeat\n3\n5\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn screenshots_land_where_the_model_asked() {
        use std::os::unix::fs::PermissionsExt;
        let root = workspace("shot");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let grim = bin.join("grim");
        std::fs::write(&grim, "#!/bin/sh\nprintf 'PNG' > \"$1\"\n").unwrap();
        std::fs::set_permissions(&grim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = Some(bin.as_os_str().to_owned());

        let out = perform_with_path(
            &root,
            &json!({"action":"screenshot","path":"shots/view.png"}),
            "screenshot",
            5,
            path.clone(),
        )
        .await
        .unwrap();
        assert!(out.contains("saved"), "{out}");
        assert!(out.contains("shots/view.png"), "{out}");
        assert_eq!(
            std::fs::read_to_string(root.join("shots/view.png")).unwrap(),
            "PNG"
        );

        // Without a path the shot goes under .wrosecode/computer/.
        let out = perform_with_path(
            &root,
            &json!({"action":"screenshot"}),
            "screenshot",
            5,
            path,
        )
        .await
        .unwrap();
        assert!(out.contains(".wrosecode/computer/shot-"), "{out}");
        let shots = std::fs::read_dir(root.join(".wrosecode/computer")).unwrap();
        assert_eq!(shots.count(), 1, "the default shot was written");
    }

    #[tokio::test]
    async fn a_missing_backend_is_an_actionable_error() {
        let root = workspace("nobackend");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let path = Some(empty.as_os_str().to_owned());
        let err = perform_with_path(
            &root,
            &json!({"action":"click","x":1,"y":1}),
            "click",
            5,
            path.clone(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("xdotool"), "{err}");
        let err = perform_with_path(
            &root,
            &json!({"action":"screenshot"}),
            "screenshot",
            5,
            path,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("on PATH"), "{err}");
    }

    #[tokio::test]
    async fn input_is_validated_before_any_backend_runs() {
        let root = workspace("validate");
        let err = perform_with_path(
            &root,
            &json!({"action":"click","x":99999,"y":1}),
            "click",
            5,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("off screen"), "{err}");
        let err = perform_with_path(
            &root,
            &json!({"action":"click","x":1,"y":2,"button":"left-button"}),
            "click",
            5,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown mouse button"), "{err}");
        let err = perform_with_path(
            &root,
            &json!({"action":"scroll","amount":0}),
            "scroll",
            5,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("zero"), "{err}");
        let err = perform_with_path(&root, &json!({"action":"type"}), "type", 5, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs text"), "{err}");
        let err = perform_with_path(
            &root,
            &json!({"action":"key","keys":"a\u{0}b"}),
            "key",
            5,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("NUL"), "{err}");
    }
}
