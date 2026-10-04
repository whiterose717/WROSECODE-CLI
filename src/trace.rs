//! `--trace` timing log: per-step API, tool, and render timings as NDJSON
//! at `~/.wrosecode/trace.log`, so future speed regressions are measurable
//! instead of guessed at. Off unless `--trace` is passed; details go
//! through the credential redactor because prompts and commands land here.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Append-only NDJSON timing sink. Cheap to clone; a default sink logs
/// nowhere, so call sites never branch on the flag.
#[derive(Clone, Default)]
pub struct TraceSink {
    file: Arc<Mutex<Option<std::fs::File>>>,
}

impl TraceSink {
    /// Open `~/.wrosecode/trace.log` for appending when enabled.
    pub fn open(enabled: bool) -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        Self::open_in(enabled, home.as_deref())
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
        Self {
            file: Arc::new(Mutex::new(file)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.file.lock().map(|file| file.is_some()).unwrap_or(false)
    }

    /// One timing line: `{"ts":…, "phase":…, "ms":…, "detail":…}`.
    pub fn log(&self, phase: &str, ms: u128, detail: &str) {
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        let Some(file) = guard.as_mut() else {
            return;
        };
        let line = serde_json::json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or_default(),
            "phase": phase,
            "ms": ms,
            "detail": crate::crash::redact(detail),
        });
        let _ = writeln!(file, "{line}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_sink_logs_nowhere() {
        let sink = TraceSink::open(false);
        assert!(!sink.enabled());
        sink.log("turn", 12, "api_key=should-never-appear");
        let _guard = sink.guard("turn", "done".into());
    }

    #[test]
    fn enabled_sink_appends_redacted_ndjson() {
        let home = std::env::temp_dir().join(format!("wrose-trace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let sink = TraceSink::open_in(true, Some(&home));
        assert!(sink.enabled());
        sink.log("provider", 41, "api_key=top-secret-value");
        drop(sink);
        let body = std::fs::read_to_string(home.join(".wrosecode/trace.log")).unwrap();
        assert!(body.contains("\"phase\":\"provider\""), "{body}");
        assert!(body.contains("\"ms\":41"), "{body}");
        assert!(!body.contains("top-secret-value"), "{body}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
