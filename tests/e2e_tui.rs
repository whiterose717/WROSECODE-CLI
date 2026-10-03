//! Real-terminal coverage for the interactive shell: scrollback keys, input
//! editing, repaint, and the `/clear` command, driven through a PTY.
//!
//! The suite needs `script` from util-linux (Linux CI); it is skipped
//! elsewhere because BSD `script` uses different flags. Everything runs in a
//! throwaway `HOME`, and no provider key is required: none of these commands
//! call the model.
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// util-linux `script` only; BSD/macOS `script` has a different CLI.
fn pty_helper() -> bool {
    Command::new("script")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

struct Pty {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    captured: Arc<Mutex<Vec<u8>>>,
    reader: std::thread::JoinHandle<()>,
}

impl Pty {
    fn start(binary: &str, home: &std::path::Path) -> Self {
        Self::start_with(binary, home, &[])
    }

    /// `extra` is appended to the environment (used by the 5000-line flood
    /// test to seed the transcript without a model).
    fn start_with(binary: &str, home: &std::path::Path, extra: &[(&str, &str)]) -> Self {
        // A PTY allocated by `script` inherits no window size from a piped
        // stdout, which would leave the shell rendering a 0x0 frame.
        let command = format!("stty cols 100 rows 30 2>/dev/null; exec {binary}");
        let mut launcher = Command::new("script");
        launcher
            .args(["-q", "-e", "-c", &command, "/dev/null"])
            .env("HOME", home)
            .env("TERM", "xterm-256color")
            .env("WROSECODE_NO_MOUSE", "1");
        for (key, value) in extra {
            launcher.env(key, value);
        }
        let mut child = launcher
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn script(1) to own a PTY");
        let stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        // Drain stdout while the run is in flight. Without this the pipe
        // fills (frames are large), `script(1)` blocks, the inner PTY stops
        // being read, and keystrokes pile up in script's stdin until they
        // arrive coalesced — a lone Esc then merges with the next byte and
        // parses as something other than Esc.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => sink
                        .lock()
                        .expect("capture lock")
                        .extend_from_slice(&buffer[..read]),
                }
            }
        });
        Self {
            child,
            stdin,
            captured,
            reader,
        }
    }

    fn send(&mut self, bytes: &[u8], settle_ms: u64) {
        self.stdin.write_all(bytes).expect("write to the PTY");
        self.stdin.flush().expect("flush the PTY");
        std::thread::sleep(Duration::from_millis(settle_ms));
    }

    fn send_text(&mut self, text: &str, settle_ms: u64) {
        self.send(text.as_bytes(), settle_ms);
    }

    fn finish(mut self) -> (bool, String) {
        drop(self.stdin);
        let status = self.child.wait().expect("wait for the TUI to exit");
        self.reader
            .join()
            .expect("stdout capture thread joins at EOF");
        let stdout = self.captured.lock().expect("capture lock");
        let mut transcript = String::from_utf8_lossy(&stdout).into_owned();
        drop(stdout);
        let mut stderr = Vec::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_end(&mut stderr);
        }
        transcript.push_str(&String::from_utf8_lossy(&stderr));
        (status.success(), transcript)
    }
}

/// Sequence keys an operator would actually press while the shell is up.
fn drive(binary: &str, home: &std::path::Path) -> (bool, String) {
    let mut pty = Pty::start(binary, home);
    // Let the shell take the terminal (raw mode, alternate screen) before any
    // keys arrive, otherwise the line discipline consumes them instead.
    std::thread::sleep(Duration::from_millis(900));
    pty.send_text("/help", 400);
    pty.send(b"\r", 600); // run /help → a transcript longer than the viewport

    // Scroll: page up, jump to the top, jump back to the bottom, page down.
    pty.send(b"\x1b[5~", 250); // PageUp
    pty.send(b"\x1b[H", 250); // Home with an empty input
    pty.send(b"\x1b[F", 250); // End with an empty input
    pty.send(b"\x1b[6~", 250); // PageDown
    pty.send(b"\x04", 250); // Ctrl+D → half a page down while input is empty

    // Input editing: type, delete the word, clear the line, clear the screen.
    pty.send_text("half-typed", 150);
    pty.send(b"\x17", 150); // Ctrl+W
    pty.send(b"\x15", 150); // Ctrl+U
    pty.send(b"\x0c", 250); // Ctrl+L → clears the visible transcript (spec 1.3)

    // A multi-line bracketed paste collapses into a chip the draft ignores
    // until Enter; Esc discards it.
    pty.send(b"\x1b[200~", 100);
    pty.send_text(
        "pasted one\npasted two\npasted three\npasted four\npasted five\npasted six",
        300,
    );
    pty.send(b"\x1b[201~", 300); // end of paste → chip, not keystrokes
    pty.send(b"\x1b", 250); // Esc → drop the chip

    // `@` opens the file-mention picker; Esc closes it again, leaving the
    // `@` behind for the draft to keep — Ctrl+U wipes it for the next step.
    pty.send_text("@", 300);
    pty.send(b"\x1b", 250);
    pty.send(b"\x15", 150); // Ctrl+U

    // Themes: a real name applies, an unknown one is refused with the list.
    pty.send_text("/theme nord", 250);
    pty.send(b"\r", 400);
    pty.send_text("/theme bogus", 250);
    pty.send(b"\r", 400);

    // Clear the transcript, then exit cleanly.
    pty.send_text("/clear", 250);
    pty.send(b"\r", 400);
    pty.send_text("/quit", 250);
    pty.send(b"\r", 600);
    pty.finish()
}

/// Boot with a transcript far taller than the viewport, scroll to the top,
/// push more lines while detached, then jump back to the bottom.
fn drive_flood(binary: &str, home: &std::path::Path) -> (bool, String) {
    let mut pty = Pty::start_with(binary, home, &[("WROSECODE_E2E_LINES", "5000")]);
    std::thread::sleep(Duration::from_millis(1200));
    pty.send(b"\x1b[H", 350); // Home → top of the 5000-line transcript
    pty.send_text("/theme bogus", 250); // pushes lines while detached
    pty.send(b"\r", 500);
    pty.send(b"\x1b[F", 350); // End → re-engage auto-follow
    pty.send_text("/quit", 250);
    pty.send(b"\r", 600);
    pty.finish()
}

/// The splash banner stamps `first paint {n}ms` measured from `main`, which
/// is exactly the number the startup budget is about.
fn first_paint_ms(transcript: &str) -> Option<u128> {
    let after = transcript.split("first paint ").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[test]
fn tui_survives_scroll_editing_and_clearing_in_a_real_terminal() {
    if !pty_helper() {
        eprintln!("skipping: util-linux script(1) is not available");
        return;
    }
    let home = std::env::temp_dir().join(format!("wrosecode-tui-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("temp HOME");
    let binary = env!("CARGO_BIN_EXE_wrosecode").to_string();

    let (exited_cleanly, transcript) = drive(&binary, &home);

    assert!(
        exited_cleanly,
        "the TUI must leave through /quit, not a panic\n{transcript}"
    );
    assert!(
        !transcript.contains("panicked at"),
        "no panic may escape into the terminal\n{transcript}"
    );
    assert!(
        !transcript.contains("internal error"),
        "the crash banner must not appear\n{transcript}"
    );
    assert!(
        transcript.contains("WROSECODE v"),
        "the welcome banner renders\n{transcript}"
    );
    assert!(
        transcript.contains("TRANSCRIPT"),
        "the transcript pane header renders\n{transcript}"
    );
    assert!(
        !transcript.contains("Search transcript"),
        "none of these keys opens the transcript search prompt\n{transcript}"
    );
    assert!(
        transcript.contains("Transcript cleared."),
        "/clear runs from the palette\n{transcript}"
    );
    assert!(
        !transcript.contains("Unknown command"),
        "no key in the sequence is misread as a command\n{transcript}"
    );
    assert!(
        !transcript.contains("Denied"),
        "no permission prompt was raised by these commands\n{transcript}"
    );
    let painted = first_paint_ms(&transcript)
        .expect("the splash must stamp a first-paint measurement\n{transcript}");
    // The product budget is `splash::FIRST_PAINT_BUDGET_MS` (50ms), pinned by
    // a unit test: a debug build on a loaded CI box can miss it through no
    // fault of the ordering. What this PTY run proves instead is that the
    // stamp is written in the first bytes of output — the splash is painted
    // before anything else — with a wide ceiling to catch the real
    // regression, which is letting provider/MCP/skill init run first.
    assert!(
        transcript
            .find("first paint ")
            .is_some_and(|index| index < 4_096),
        "the splash is the first frame the shell paints\n{transcript}"
    );
    assert!(
        painted <= 250,
        "first paint {painted}ms is nowhere near the 50ms budget\n{transcript}"
    );
    assert!(
        transcript.contains("Theme set to nord"),
        "/theme <name> applies a palette\n{transcript}"
    );
    assert!(
        transcript.contains("Unknown theme `bogus`") && transcript.contains("dracula"),
        "/theme <unknown> lists what exists\n{transcript}"
    );
    assert!(
        transcript.contains("[Pasted 6 lines]"),
        "a multi-line paste collapses into a chip\n{transcript}"
    );
    assert!(
        transcript.contains("Mention files"),
        "`@` opens the file-mention picker\n{transcript}"
    );
    assert!(
        transcript.contains("Transcript cleared. Session context and history are kept."),
        "Ctrl+L clears the visible transcript like /clear (spec 1.3)\n{transcript}"
    );
    assert!(
        !transcript.contains("pasted five"),
        "a discarded paste never reaches the draft or the transcript\n{transcript}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Spec 1.1: a transcript taller than the viewport is pinned to the bottom on
/// boot, Home detaches with the new-lines indicator, output pushed while
/// detached is counted, and End re-engages auto-follow.
#[test]
fn pty_scrolls_up_counts_new_lines_and_reengages_follow() {
    if !pty_helper() {
        eprintln!("skipping: util-linux script(1) is not available");
        return;
    }
    let home = std::env::temp_dir().join(format!("wrosecode-flood-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("temp HOME");
    let binary = env!("CARGO_BIN_EXE_wrosecode").to_string();

    let (exited_cleanly, transcript) = drive_flood(&binary, &home);

    assert!(
        exited_cleanly,
        "the flood run must exit cleanly\n{transcript}"
    );
    assert!(
        !transcript.contains("panicked at"),
        "5000 lines must not blow the frame\n{transcript}"
    );
    let bottom = transcript
        .find("flood line 4999")
        .expect("the bottom of the flood renders");
    let top = transcript
        .find("flood line 0")
        .expect("scrolling to Home reaches the top");
    assert!(
        bottom < top,
        "boot is pinned to the bottom of the transcript; Home moves it up\n{transcript}"
    );
    assert!(
        transcript.contains("↓ 5 new lines  (End to jump)"),
        "output pushed while detached feeds the indicator\n{transcript}"
    );
    // The renderer rewrites a whole row when it changes, so the LAST header
    // we ever see is the one after End: it must carry no marker.
    let last_header = transcript
        .rfind("TRANSCRIPT")
        .expect("the header renders at least once");
    let after = &transcript[last_header..];
    let window = &after[..after.len().min(160)];
    assert!(
        !window.contains("End to jump"),
        "End re-engages auto-follow and drops the indicator: {window}\n{transcript}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// `NO_COLOR` (spec 2.4) must reach both the splash and the row renderer:
/// no 256-colour cube codes, no truecolor channels, in the whole session.
#[test]
fn no_color_session_emits_no_palette_codes() {
    if !pty_helper() {
        eprintln!("skipping: util-linux script(1) is not available");
        return;
    }
    let home = std::env::temp_dir().join(format!("wrosecode-nocolor-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("temp HOME");
    let binary = env!("CARGO_BIN_EXE_wrosecode").to_string();

    let mut pty = Pty::start_with(&binary, &home, &[("NO_COLOR", "1")]);
    // Splash first paint, then the theme picker (a full themed frame), then
    // a transcript entry — all three paint paths must stay monochrome.
    std::thread::sleep(Duration::from_millis(900));
    pty.send_text("/theme", 300);
    pty.send(b"\r", 500);
    pty.send_text("/help", 300);
    pty.send(b"\r", 600);
    pty.send_text("/quit", 300);
    pty.send(b"\r", 400);
    let (exited_cleanly, transcript) = pty.finish();

    assert!(
        exited_cleanly,
        "the TUI must leave through /quit, not a panic\n{transcript}"
    );
    assert!(
        !transcript.contains("38;5;"),
        "NO_COLOR drops the 256-colour cube codes\n{transcript}"
    );
    assert!(
        !transcript.contains("38;2;"),
        "NO_COLOR drops truecolor codes\n{transcript}"
    );
    assert!(
        transcript.contains("WROSECODE v"),
        "the splash still paints its wordmark\n{transcript}"
    );
    std::fs::remove_dir_all(&home).ok();
}
