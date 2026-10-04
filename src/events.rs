//! Framed submission stream for embedding clients (Codex submission/event
//! protocol).
//!
//! `--events PATH` writes one JSON object per line — `start` before the turn,
//! `step`/`text` frames while it runs, `result` once it finishes — so a host
//! application can drive a session without parsing the TUI transcript or the
//! `--summary json` tail.

use crate::provider::{Progress, Usage};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{Stdout, Write};
use std::path::Path;

enum Sink {
    File(std::fs::File),
    Stdout(Stdout),
}

/// The NDJSON sink: a file, or stdout when the path is `-`.
pub struct EventWriter {
    sink: Sink,
}

impl EventWriter {
    pub fn open(path: &Path) -> Result<Self> {
        let sink = if path.as_os_str() == "-" {
            Sink::Stdout(std::io::stdout())
        } else {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            Sink::File(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open event stream {}", path.display()))?,
            )
        };
        Ok(Self { sink })
    }

    /// Append one frame as a single line and flush, so a reader sees events
    /// as they happen.
    pub fn write(&mut self, frame: &Value) -> Result<()> {
        let mut line = serde_json::to_string(frame)?;
        line.push('\n');
        match &mut self.sink {
            Sink::File(file) => {
                file.write_all(line.as_bytes())?;
                file.flush()?;
            }
            Sink::Stdout(stdout) => {
                stdout.write_all(line.as_bytes())?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

/// The session opening a turn. The user prompt is redacted before it reaches
/// the NDJSON stream: prompts may paste credentials, and the events file can
/// be consumed by another application.
pub fn start_frame(prompt: &str, session: &str) -> Value {
    json!({
        "type": "start",
        "session": session,
        "prompt": crate::crash::redact(prompt),
        "ts": now_ms(),
    })
}

/// The session closing a turn. Answers can quote tool output, so they use
/// the same credential filter as every other outward-facing frame.
pub fn result_frame(answer: &str, verified: bool, usage: &Usage, model_turns: usize) -> Value {
    json!({
        "type": "result",
        "answer": crate::crash::redact(answer),
        "verified": verified,
        "usage": {
            "input": usage.input,
            "output": usage.output,
            "reasoning": usage.reasoning,
            "cache_read": usage.cache_read,
            "cache_write": usage.cache_write,
            "estimated": usage.estimated,
        },
        "model_turns": model_turns,
        "ts": now_ms(),
    })
}

/// One live event. Returns `None` for events that are internal (resets,
/// metrics refreshes, render hints) and would only add noise to the stream.
pub fn frame(event: &Progress) -> Option<Value> {
    let value = match event {
        Progress::ToolBegin { id, title } => {
            json!({"type": "step", "phase": "begin", "id": id, "title": crate::crash::redact(title), "ts": now_ms()})
        }
        Progress::ToolEnd {
            id,
            ok,
            output,
            elapsed_ms,
        } => json!({
            "type": "step",
            "phase": "end",
            "id": id,
            "ok": ok,
            "elapsed_ms": elapsed_ms,
            "output": preview(&crate::crash::redact(output)),
            "ts": now_ms(),
        }),
        Progress::Tool(title) => {
            json!({"type": "step", "phase": "tool", "title": crate::crash::redact(title)})
        }
        Progress::TextDelta(delta) => json!({"type": "text", "delta": crate::crash::redact(delta)}),
        Progress::Think {
            from,
            to,
            to_level,
            reason,
        } => json!({
            "type": "step",
            "phase": "think",
            "from": from,
            "to": to,
            "to_level": to_level,
            "reason": reason,
        }),
        Progress::ThinkIgnored(reason) => {
            json!({"type": "step", "phase": "think-ignored", "reason": reason})
        }
        Progress::FlagFound { flag, source } => {
            json!({"type": "step", "phase": "flag", "flag": flag, "source": source})
        }
        Progress::Plan(items) => json!({
            "type": "step",
            "phase": "plan",
            "items": items
                .iter()
                .map(|(text, done)| json!({"text": text, "done": done}))
                .collect::<Vec<_>>(),
        }),
        Progress::Usage(usage) => json!({
            "type": "step",
            "phase": "usage",
            "input": usage.input,
            "output": usage.output,
            "reasoning": usage.reasoning,
            "estimated": usage.estimated,
        }),
        Progress::ResetText | Progress::Metrics(_) => return None,
    };
    Some(value)
}

fn preview(output: &str) -> String {
    const CAP: usize = 500;
    if output.chars().count() <= CAP {
        return output.to_string();
    }
    let head: String = output.chars().take(CAP).collect();
    format!("{head}…")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ToolCall, Usage};

    #[test]
    fn start_and_result_frames_carry_the_submission() {
        let start = start_frame("solve it", "session-1");
        assert_eq!(start["type"], "start");
        assert_eq!(start["prompt"], "solve it");
        assert_eq!(start["session"], "session-1");

        let result = result_frame(
            "FLAG{ok}",
            true,
            &Usage {
                input: 10,
                output: 20,
                ..Usage::default()
            },
            3,
        );
        assert_eq!(result["type"], "result");
        assert_eq!(result["answer"], "FLAG{ok}");
        assert_eq!(result["verified"], true);
        assert_eq!(result["usage"]["input"], 10);
        assert_eq!(result["model_turns"], 3);
    }

    #[test]
    fn tool_events_become_step_frames() {
        let begin = frame(&Progress::ToolBegin {
            id: "call_1".into(),
            title: "Read main.rs".into(),
        })
        .expect("begin frame");
        assert_eq!(begin["type"], "step");
        assert_eq!(begin["phase"], "begin");
        assert_eq!(begin["id"], "call_1");

        let end = frame(&Progress::ToolEnd {
            id: "call_1".into(),
            ok: true,
            output: "fn main() {}".into(),
            elapsed_ms: 12,
        })
        .expect("end frame");
        assert_eq!(end["phase"], "end");
        assert_eq!(end["ok"], true);
        assert_eq!(end["elapsed_ms"], 12);
        assert_eq!(end["output"], "fn main() {}");
    }

    #[test]
    fn long_tool_output_is_truncated_but_frames_stay_single_line() {
        let long = "x".repeat(5_000);
        let frame = frame(&Progress::ToolEnd {
            id: "c".into(),
            ok: false,
            output: long,
            elapsed_ms: 1,
        })
        .expect("frame");
        let line = serde_json::to_string(&frame).expect("serialise");
        assert!(!line.contains('\n'));
        assert!(frame["output"].as_str().expect("output").len() < 700);
    }

    #[test]
    fn render_only_events_are_skipped() {
        assert!(frame(&Progress::ResetText).is_none());
        assert!(frame(&Progress::Metrics(
            crate::metrics::MetricsSnapshot::default()
        ))
        .is_none());
    }

    #[test]
    fn frames_redact_secrets_but_keep_flags() {
        let secret = "events-sink-test-secret-value";
        crate::crash::register_secret(secret);
        let begin = frame(&Progress::ToolBegin {
            id: "call_1".into(),
            title: format!("Ran shell echo {secret}"),
        })
        .expect("begin frame");
        assert!(!begin["title"].as_str().unwrap_or_default().contains(secret));

        let end = frame(&Progress::ToolEnd {
            id: "call_1".into(),
            ok: true,
            output: format!("api_key={secret}\nflag{{events_ok}}"),
            elapsed_ms: 3,
        })
        .expect("end frame");
        let output = end["output"].as_str().expect("output");
        assert!(!output.contains(secret), "{output}");
        assert!(output.contains("flag{events_ok}"), "{output}");

        let delta = frame(&Progress::TextDelta(format!("password={secret}"))).expect("delta frame");
        assert!(!delta["delta"].as_str().unwrap_or_default().contains(secret));

        let start = start_frame(&format!("use {secret}"), "session-1");
        assert!(!start["prompt"]
            .as_str()
            .unwrap_or_default()
            .contains(secret));
        let result = result_frame(&format!("used {secret}"), true, &Usage::default(), 1);
        assert!(!result["answer"]
            .as_str()
            .unwrap_or_default()
            .contains(secret));
    }

    #[test]
    fn the_writer_appends_ndjson_lines() {
        let path = std::env::temp_dir().join(format!(
            "wrosecode-events-test-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut writer = EventWriter::open(&path).expect("open");
        writer.write(&start_frame("a", "s")).expect("write");
        writer
            .write(&result_frame("done", false, &Usage::default(), 1))
            .expect("write");
        drop(writer);

        let text = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap()["type"],
            "start"
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap()["type"],
            "result"
        );

        let mut writer = EventWriter::open(&path).expect("reopen");
        writer.write(&start_frame("b", "s")).expect("write");
        drop(writer);
        let text = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(text.lines().count(), 3, "reopening appends");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tool_call_frames_round_trip_through_json() {
        let _call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            input: json!({"path": "a.txt"}),
        };
        let frame = frame(&Progress::Usage(Usage::default())).expect("usage frame");
        assert_eq!(frame["phase"], "usage");
    }
}
