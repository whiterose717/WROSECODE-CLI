//! Real-terminal coverage for the interactive shell: scrollback keys, input
//! editing, repaint, and the `/clear` command, driven through a PTY.
//!
//! The suite needs `script` from util-linux (Linux CI); it is skipped
//! elsewhere because BSD `script` uses different flags. Everything runs in a
//! throwaway `HOME`, and no provider key is required: none of these commands
//! call the model.
#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};
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
}

impl Pty {
    fn start(binary: &str, home: &std::path::Path) -> Self {
        // A PTY allocated by `script` inherits no window size from a piped
        // stdout, which would leave the shell rendering a 0x0 frame.
        let command = format!("stty cols 100 rows 30 2>/dev/null; exec {binary}");
        let mut child = Command::new("script")
            .args(["-q", "-e", "-c", &command, "/dev/null"])
            .env("HOME", home)
            .env("TERM", "xterm-256color")
            .env("WROSECODE_NO_MOUSE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn script(1) to own a PTY");
        let stdin = child.stdin.take().expect("piped stdin");
        Self { child, stdin }
    }

    fn send(&mut self, bytes: &[u8], settle_ms: u64) {
        self.stdin.write_all(bytes).expect("write to the PTY");
        self.stdin.flush().expect("flush the PTY");
        std::thread::sleep(Duration::from_millis(settle_ms));
    }

    fn send_text(&mut self, text: &str, settle_ms: u64) {
        self.send(text.as_bytes(), settle_ms);
    }

    fn finish(self) -> (bool, String) {
        drop(self.stdin);
        let output = self
            .child
            .wait_with_output()
            .expect("wait for the TUI to exit");
        let mut transcript = String::from_utf8_lossy(&output.stdout).into_owned();
        transcript.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), transcript)
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
    pty.send(b"\x04", 250); // Ctrl+D → page down while the input is empty

    // Input editing: type, delete the word, clear the line, repaint.
    pty.send_text("half-typed", 150);
    pty.send(b"\x17", 150); // Ctrl+W
    pty.send(b"\x15", 150); // Ctrl+U
    pty.send(b"\x0c", 250); // Ctrl+L → repaint, must not wipe the transcript

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

    std::fs::remove_dir_all(&home).ok();
}
