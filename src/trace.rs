//! `--trace` timing log: per-step API, tool, and render timings as NDJSON
//! at `~/.wrosecode/trace.log`, so future speed regressions are measurable
//! instead of guessed at. Off unless `--trace` is passed; details go
//! through the credential redactor because prompts and commands land here.
//!
//! Every line carries a `session` id, a monotonic `seq`, and the `turn` it
//! belongs to, so a turn's critical path can be reconstructed with `jq`
//! instead of correlating free-form phase lines by timestamp. Turn ids are
//! allocated by [`TraceSink::begin_turn`]; child agents run inside their
//! parent turn and therefore inherit it.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Append-only NDJSON timing sink. Cheap to clone; a default sink logs
/// nowhere, so call sites never branch on the flag.
#[derive(Clone)]
pub struct TraceSink {
    file: Arc<Mutex<Option<std::fs::File>>>,
    session: Arc<String>,
    seq: Arc<AtomicU64>,
    turn: Arc<AtomicU64>,
}

impl Default for TraceSink {
    fn default() -> Self {
        Self {
            file: Arc::new(Mutex::new(None)),
            session: Arc::new(String::new()),
            seq: Arc::new(AtomicU64::new(0)),
            turn: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl TraceSink {
    /// Open `~/.wrosecode/trace.log` for appending when enabled.
    pub fn open(enabled: bool) -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let sink = Self::open_in(enabled, home.as_deref());
        if sink.enabled() {
            install_global(&sink);
        }
        sink
    }

    /// Same, under an explicit home directory (tests stay off the real one).
    pub fn open_in(enabled: bool, home: Option<&std::path::Path>) -> Self {
        if !enabled {
            return Self::default();
        }
        let file = home
            .map(|home| home.join(".wrosecode").join("trace.log"))
            .and_then(|path| {
                std::fs::create_dir_all(path.parent()?).ok()?;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
            });
        if file.is_none() {
            return Self::default();
        }
        let session = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or_default()
        );
        Self {
            file: Arc::new(Mutex::new(file)),
            session: Arc::new(session),
            seq: Arc::new(AtomicU64::new(0)),
            turn: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.file.lock().map(|file| file.is_some()).unwrap_or(false)
    }

    /// The turn currently being measured (0 before the first `begin_turn`).
    pub fn turn_id(&self) -> u64 {
        self.turn.load(Ordering::Relaxed)
    }

    /// One timing line: `{"ts":…, "session":…, "turn":…, "seq":…,
    /// "phase":…, "ms":…, "detail":…}`.
    pub fn log(&self, phase: &str, ms: u128, detail: &str) {
        self.log_for_turn(phase, ms, detail, self.turn_id());
    }

    fn log_for_turn(&self, phase: &str, ms: u128, detail: &str, turn: u64) {
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        let Some(file) = guard.as_mut() else {
            return;
        };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let line = serde_json::json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or_default(),
            "session": &*self.session,
            "turn": turn,
            "seq": seq,
            "phase": phase,
            "ms": ms,
            "detail": crate::crash::redact(detail),
        });
        let _ = writeln!(file, "{line}");
    }

    /// Instantaneous critical-path event (`ms` is zero). Use [`Self::guard`]
    /// for anything whose duration matters.
    pub fn event(&self, phase: &str, detail: &str) {
        self.log(phase, 0, detail);
    }

    /// Start a new user turn: allocates the turn id, records `turn_begin`
    /// (with a fresh zero `ms`), and logs `turn_end` with the elapsed time
    /// when the guard drops on every return path.
    pub fn begin_turn(&self, detail: &str) -> TurnGuard {
        let turn = self.turn.fetch_add(1, Ordering::Relaxed) + 1;
        self.log_for_turn("turn_begin", 0, detail, turn);
        TurnGuard {
            sink: self.clone(),
            turn,
            started: Instant::now(),
        }
    }

    /// Span guard: logs `phase` with the elapsed time when dropped, so every
    /// return path (including `?`) is measured.
    pub fn guard(&self, phase: &'static str, detail: String) -> TraceGuard {
        TraceGuard {
            sink: self.clone(),
            phase,
            detail,
            started: Instant::now(),
        }
    }
}

/// See [`TraceSink::begin_turn`].
pub struct TurnGuard {
    sink: TraceSink,
    turn: u64,
    started: Instant,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.sink.log_for_turn(
            "turn_end",
            self.started.elapsed().as_millis(),
            "turn complete",
            self.turn,
        );
    }
}

/// See [`TraceSink::guard`].
pub struct TraceGuard {
    sink: TraceSink,
    phase: &'static str,
    detail: String,
    started: Instant,
}

impl Drop for TraceGuard {
    fn drop(&mut self) {
        self.sink
            .log(self.phase, self.started.elapsed().as_millis(), &self.detail);
    }
}

/// Process-wide sink used by provider adapters, which do not own an `Agent`.
/// Only installed when `--trace` opened a real file, so the hot path costs one
/// `OnceLock` read and a `Mutex` lock at the first streamed text delta.
static GLOBAL: OnceLock<Mutex<Option<TraceSink>>> = OnceLock::new();

fn global_slot() -> &'static Mutex<Option<TraceSink>> {
    GLOBAL.get_or_init(|| Mutex::new(None))
}

fn install_global(sink: &TraceSink) {
    if let Ok(mut slot) = global_slot().lock() {
        *slot = Some(sink.clone());
    }
}

/// Record a process-wide trace event when `--trace` is active.
pub fn global_event(phase: &str, detail: &str) {
    let sink = global_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
    if let Some(sink) = sink {
        sink.event(phase, detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn home(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wrose-trace-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn lines(home: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(home.join(".wrosecode/trace.log"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn disabled_sink_logs_nowhere() {
        let sink = TraceSink::open(false);
        assert!(!sink.enabled());
        assert_eq!(sink.turn_id(), 0);
        sink.log("turn", 12, "api_key=should-never-appear");
        let _guard = sink.guard("turn", "done".into());
    }

    #[test]
    fn enabled_sink_appends_redacted_ndjson() {
        let home = home("redact");
        let sink = TraceSink::open_in(true, Some(&home));
        assert!(sink.enabled());
        let guard = sink.begin_turn("api_key=top-secret-value");
        sink.event("prompt_submitted", "redact me");
        drop(guard);
        let body = std::fs::read_to_string(home.join(".wrosecode/trace.log")).unwrap();
        assert!(body.contains("\"phase\":\"prompt_submitted\""), "{body}");
        assert!(body.contains("\"ms\":0"), "{body}");
        assert!(!body.contains("top-secret-value"), "{body}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn turns_are_scoped_and_sequence_is_monotonic() {
        let home = home("turns");
        let sink = TraceSink::open_in(true, Some(&home));
        let first = sink.begin_turn("first");
        assert_eq!(sink.turn_id(), 1);
        sink.event("context_build_started", "t1");
        drop(first);
        let second = sink.begin_turn("second");
        assert_eq!(sink.turn_id(), 2);
        sink.event("context_build_started", "t2");
        drop(second);

        let lines = lines(&home);
        assert!(lines.iter().all(|line| line["session"].is_string()));
        let phases: Vec<&str> = lines
            .iter()
            .map(|line| line["phase"].as_str().unwrap())
            .collect();
        assert_eq!(
            phases,
            [
                "turn_begin",
                "context_build_started",
                "turn_end",
                "turn_begin",
                "context_build_started",
                "turn_end"
            ]
        );
        let turns: Vec<u64> = lines
            .iter()
            .map(|line| line["turn"].as_u64().unwrap())
            .collect();
        assert_eq!(turns, [1, 1, 1, 2, 2, 2]);
        let seqs: Vec<u64> = lines
            .iter()
            .map(|line| line["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5, 6]);
        // The second turn_end reports the second turn's own elapsed time.
        assert!(lines[5]["ms"].as_u64().is_some());
        let _ = std::fs::remove_dir_all(&home);
    }
}
