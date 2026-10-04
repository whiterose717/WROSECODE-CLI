//! Terminal restoration and crash reporting.
//!
//! The TUI runs the terminal in raw mode on the alternate screen. A panic, a
//! `kill -INT`, or a `kill -TERM` in that state otherwise leaves the shell
//! unusable (no echo, no cursor, mouse tracking still on), so every fatal path
//! funnels through [`restore`] and every panic writes a redacted report under
//! `~/.wrosecode/crash/`.

use crossterm::{
    cursor,
    event::{DisableBracketedPaste, DisableMouseCapture},
    execute,
    style::{ResetColor, SetForegroundColor},
    terminal::{self, LeaveAlternateScreen},
};
use regex::Regex;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Put the terminal back into cooked mode on the primary screen.
///
/// Every sequence is emitted unconditionally: disabling a mode that was never
/// enabled is a no-op in every terminal emulator, while *forgetting* to disable
/// one that was is a permanent glitch.
pub fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        SetForegroundColor(crossterm::style::Color::Reset),
        ResetColor,
        cursor::Show,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = std::io::stdout().flush();
}

/// Directory crash reports are written to: `~/.wrosecode/crash`.
pub fn report_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".wrosecode").join("crash"))
}

/// Install the process-wide panic hook: restore the terminal, append to the
/// project error log, and write a redacted crash report to the home directory.
pub fn install_panic_hook(root: Option<PathBuf>) {
    std::panic::set_hook(Box::new(move |info| {
        restore();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let detail = redact(&info.to_string());
        if let Some(root) = &root {
            let dir = root.join(".ctf");
            let _ = std::fs::create_dir_all(&dir);
            let line = format!("{}\tpanic\t{detail}\n{backtrace}\n", timestamp_millis());
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("errors.log"))
            {
                let _ = file.write_all(redact(&line).as_bytes());
            }
        }
        let report = write_report(
            &report_dir().unwrap_or_else(|| PathBuf::from(".wrosecode-crash")),
            "panic",
            &detail,
            &backtrace,
            root.as_ref(),
        );
        match report {
            Some(path) => eprintln!(
                "WROSECODE encountered an internal error: {detail}\nCrash report: {}",
                path.display()
            ),
            None => eprintln!("WROSECODE encountered an internal error: {detail}"),
        }
    }));
}

/// Register SIGINT / SIGTERM handlers that restore the terminal before the
/// process dies. Returns `false` when the platform has no signal support.
pub fn install_signal_handlers() -> bool {
    #[cfg(unix)]
    {
        tokio::spawn(async {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(term) => term,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    restore();
                    std::process::exit(130);
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    restore();
                    std::process::exit(130);
                }
                _ = term.recv() => {
                    restore();
                    std::process::exit(143);
                }
            }
        });
        true
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Write one crash report file. Exposed for tests; production callers go
/// through [`report_dir`].
pub fn write_report(
    dir: &Path,
    kind: &str,
    detail: &str,
    backtrace: &str,
    root: Option<&PathBuf>,
) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let stamp = timestamp_millis();
    let path = dir.join(format!("{stamp}.log"));
    let mut body = String::new();
    body.push_str("WROSECODE crash report\n");
    body.push_str(&format!("kind: {kind}\n"));
    body.push_str(&format!("timestamp: {stamp}\n"));
    body.push_str(&format!("version: {}\n", env!("CARGO_PKG_VERSION")));
    body.push_str(&format!(
        "os: {}-{}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    if let Some(root) = root {
        body.push_str(&format!(
            "cwd: {}\n",
            root.display().to_string().replace('\n', " ")
        ));
    }
    body.push_str(&format!(
        "argv: {}\n",
        redact(&std::env::args().collect::<Vec<_>>().join(" "))
    ));
    body.push_str("\n[environment]\n");
    for (key, value) in std::env::vars() {
        let name = key.to_ascii_uppercase();
        let secret = secret_name(&name);
        body.push_str(&format!(
            "{}={}\n",
            key,
            if secret {
                "***".to_string()
            } else {
                redact(&value)
            }
        ));
    }
    body.push_str("\n[detail]\n");
    body.push_str(&redact(detail));
    body.push_str("\n\n[backtrace]\n");
    body.push_str(&redact(backtrace));
    if !body.ends_with('\n') {
        body.push('\n');
    }
    std::fs::write(&path, body).ok()?;
    Some(path)
}

/// True when an environment variable name looks like it holds a credential.
pub fn secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
        || upper.starts_with("WROSECODE_") && upper.ends_with("API_KEY")
}

/// Exact credential values already seen by this process (provider API keys
/// and other configured secrets). Patterns catch credential shapes; this
/// registry catches a bare key that carries no `api_key=`-style label.
static KNOWN_SECRETS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn known_secrets() -> &'static Mutex<Vec<String>> {
    KNOWN_SECRETS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Remember one configured secret for every later redaction pass. Short
/// values are ignored so ordinary words are never treated as credentials.
pub fn register_secret(value: &str) {
    let value = value.trim();
    if value.len() < 8 {
        return;
    }
    let mut known = known_secrets()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !known.iter().any(|known| known == value) {
        known.push(value.to_string());
        known.sort_by_key(|known| std::cmp::Reverse(known.len()));
    }
}

fn redactor() -> &'static Regex {
    static REDACTOR: OnceLock<Regex> = OnceLock::new();
    REDACTOR.get_or_init(|| {
        Regex::new(
            r#"(?xi)
            (?P<prefix>\b(?:api[_-]?key|apikey|token|secret|passwd|password|authorization)\b\s*[=:]\s*)(?P<value>[^\s"',;]+)
            | (?P<directive>\b(?:bearer|basic|token)\s+)(?P<bearer>[A-Za-z0-9._~+/=-]{8,})
            | (?P<aws>AKIA[0-9A-Z]{16})
            | (?P<github>(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})
            | (?P<slack>xox[baprs]-[A-Za-z0-9-]{10,})
            | (?P<openai>sk-[A-Za-z0-9_-]{16,})
            | (?P<jwt>eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})
            "#,
        )
        .expect("static redaction pattern")
    })
}

/// Replace credential-shaped substrings with `***` so crash reports, logs, and
/// the error log never carry a usable secret. Exact values previously passed
/// to [`register_secret`] are removed too, even when they appear without a
/// credential-shaped label.
pub fn redact(text: &str) -> String {
    let pattern = redactor();
    let mut out = pattern
        .replace_all(text, |captures: &regex::Captures| {
            // Keyed groups keep the `name=` so reports stay readable; everything
            // that *is* a credential disappears entirely.
            for name in ["prefix", "directive"] {
                if let Some(matched) = captures.name(name) {
                    return format!("{}***", matched.as_str());
                }
            }
            "***".to_string()
        })
        .into_owned();
    let known = known_secrets()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for secret in known.iter() {
        if !secret.is_empty() {
            out = out.replace(secret, "***");
        }
    }
    out
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_safe_to_call_without_a_tui() {
        // Emitting the teardown sequences on a non-raw terminal is a no-op;
        // the point is that it never panics or hangs.
        restore();
        restore();
    }

    #[test]
    fn redaction_covers_common_credential_shapes() {
        let samples = [
            "ANTHROPIC_API_KEY=sk-ant-abcdefghijklmnop",
            "api_key: super-secret-value",
            "Authorization: Bearer eyJhbGciOi.eyJzdWIi.SflKxwRJ",
            "aws_key AKIAIOSFODNN7EXAMPLE",
            "token ghp_abcdefghijklmnopqrstuv",
            "slack xoxb-1234567890-abcdef",
            "password=hunter2",
        ];
        for sample in samples {
            let redacted = redact(sample);
            assert_ne!(redacted, sample, "not redacted: {sample}");
            assert!(
                !redacted.contains("hunter2")
                    && !redacted.contains("sk-ant-abcdefghijklmnop")
                    && !redacted.contains("AKIAIOSFODNN7EXAMPLE")
                    && !redacted.contains("ghp_abcdefghijklmnopqrstuv")
                    && !redacted.contains("xoxb-1234567890-abcdef"),
                "leaked: {redacted}"
            );
        }
    }

    #[test]
    fn ordinary_text_survives_redaction() {
        let text = "read src/main.rs then run cargo test --release (12345)";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn secret_names_are_recognised() {
        assert!(secret_name("ANTHROPIC_API_KEY"));
        assert!(secret_name("MY_TOKEN"));
        assert!(secret_name("DB_PASSWORD"));
        assert!(!secret_name("PATH"));
        assert!(!secret_name("WROSECODE_THEME"));
    }

    #[test]
    fn registered_secret_values_are_removed_without_a_label() {
        let secret = "registered-bare-test-secret-value";
        register_secret(secret);
        let redacted = redact(&format!("the token is {secret} in plain text"));
        assert!(!redacted.contains(secret), "leaked: {redacted}");
        assert!(redacted.contains("***"), "{redacted}");
        // Short values are not treated as secrets.
        register_secret("short");
        assert_eq!(redact("a short word"), "a short word");
    }

    #[test]
    fn reports_are_written_and_redacted() {
        let dir = std::env::temp_dir().join(format!("wrose-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = std::env::temp_dir();
        let path = write_report(
            &dir,
            "panic",
            "boom API_KEY=sk-abcdefghijklmnopqrst",
            "backtrace goes here",
            Some(&root),
        )
        .expect("report written");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("kind: panic"));
        assert!(body.contains("backtrace goes here"));
        assert!(!body.contains("sk-abcdefghijklmnopqrst"), "{body}");
        assert!(body.contains("***"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
