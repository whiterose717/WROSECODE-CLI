//! `wrosecode dashboard` — attach to a running WROSECODE session and paint
//! the same eight-panel grid the TUI renders (spec 3.2).
//!
//! Data source precedence, in order:
//! 1. fresh `~/.wrosecode/live.json` (≤10 s old) written by the running TUI,
//! 2. `GET http://{listen}/v1/status` when that session runs `--web`,
//! 3. offline best effort (config.toml, `.ctf/flags.log`, git, process
//!    registry) so `--once --offline` works with nothing running.
//!
//! Every frame comes from [`tui::compose_grid`], the exact renderer the TUI
//! uses, so the attach view cannot drift from the in-terminal dashboard.

use crate::tui::{self, DashStats, Row};
use anyhow::{Context, Result};
use clap::Parser;
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::{Print, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

/// Freshness window for `live.json`: the TUI rewrites it about once a second
/// while idle, so anything older than 10 s means the session is gone.
const LIVE_FRESH_MS: u64 = 10_000;

#[derive(Parser)]
#[command(
    name = "wrosecode-dashboard",
    about = "Attach to a running WROSECODE session's live dashboard"
)]
pub struct AttachCli {
    /// Where the running session's web server listens.
    #[arg(long, default_value = "127.0.0.1:7878")]
    pub listen: String,
    /// Print a single frame and exit (pipe-friendly).
    #[arg(long)]
    pub once: bool,
    /// Never touch the network (fresh live.json or offline sources only).
    #[arg(long)]
    pub offline: bool,
}

/// Resolve the best stats source and hand off to the requested render mode.
pub async fn run(cli: AttachCli) -> Result<()> {
    if cli.once {
        let stats = gather(&cli.listen, cli.offline).await;
        let width = terminal::size().map(|(w, _)| w).unwrap_or(100);
        let height = terminal::size().map(|(_, h)| h).unwrap_or(40);
        print_plain(&stats, width as usize, height as usize);
        return Ok(());
    }
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("dashboard needs a terminal; use --once to print a frame");
    }
    dashboard_loop(cli).await
}

/// live.json (≤10 s) → HTTP `/v1/status` (2 s) → offline fallback.
async fn gather(listen: &str, offline: bool) -> DashStats {
    if let Some(stats) = read_live() {
        return stats;
    }
    if !offline {
        if let Some(stats) = fetch_status(listen).await {
            return stats;
        }
    }
    offline_stats()
}

/// The TUI's periodic `~/.wrosecode/live.json` snapshot, if it is fresh.
fn read_live() -> Option<DashStats> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".wrosecode/live.json");
    let text = std::fs::read_to_string(path).ok()?;
    let stats: DashStats = serde_json::from_str(&text).ok()?;
    let age = tui::unix_ms().saturating_sub(stats.live_at_ms);
    (stats.live_at_ms > 0 && age <= LIVE_FRESH_MS).then_some(stats)
}

/// Ask the running `--web` session for its stats.
async fn fetch_status(listen: &str) -> Option<DashStats> {
    let client = crate::provider::shared_client(2, 2).ok()?;
    let response = client
        .get(format!("http://{listen}/v1/status"))
        .send()
        .await
        .ok()?;
    let value = response.json::<serde_json::Value>().await.ok()?;
    let stats: DashStats = serde_json::from_value(value).ok()?;
    (stats.live_at_ms > 0).then_some(stats)
}

/// Best effort when nothing is running: the same facts the TUI would show
/// minus turn timings (they only exist in the live event stream).
fn offline_stats() -> DashStats {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let runtime = crate::config::RuntimeConfig::load(&root);
    let history: Vec<String> = std::fs::read_to_string(root.join(".ctf/flags.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    DashStats {
        schema: "wrosecode/live-v1".into(),
        live_at_ms: tui::unix_ms(),
        session_id: "offline".into(),
        root: root.display().to_string(),
        provider: "offline".into(),
        model: "—".into(),
        mode: "chat".into(),
        category: String::new(),
        thinking_level: runtime.agent.thinking_level.min(20),
        think: tui::think_label(
            runtime.agent.think.unwrap_or_else(|| {
                crate::think::ThinkLevel::from_level(runtime.agent.thinking_level)
            }),
            runtime.agent.thinking_level.min(20),
        ),
        status: "offline".into(),
        budget_usd: runtime.agent.budget_usd,
        plan: Vec::new(),
        flags: tui::flag_rows(&history),
        flags_total: history.len(),
        changed_files: tui::git_changed_files(&root.display().to_string()),
        procs: tui::proc_stats(),
        ..DashStats::default()
    }
}

/// One interactive frame: raw mode, ≤10 Hz repaint, panel/selection keys,
/// detail view, and Ctrl+C / Ctrl+K signalling of the selected process.
async fn dashboard_loop(cli: AttachCli) -> Result<()> {
    terminal::enable_raw_mode().context("enable raw mode")?;
    let result = frame_loop(&cli).await;
    terminal::disable_raw_mode().ok();
    result
}

async fn frame_loop(cli: &AttachCli) -> Result<()> {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let colors = tui::dashboard_theme(no_color);
    let mut focus = 0usize;
    let mut selected = 0usize;
    let mut detail: Option<(String, String)> = None;
    let mut stdout = std::io::stdout();
    loop {
        let stats = gather(&cli.listen, cli.offline).await;
        let (width, height) = terminal::size().unwrap_or((100, 40));
        let (frame, _) = tui::compose_grid(
            &stats,
            width,
            height,
            colors,
            focus,
            selected,
            detail.clone(),
        );
        paint(&mut stdout, &frame)?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Repeat {
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if detail.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => detail = None,
                KeyCode::Char('q') => break,
                KeyCode::Char('c') if control => signal_selected(&stats, focus, selected, false),
                KeyCode::Char('k') if control => signal_selected(&stats, focus, selected, true),
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('d') if control => break,
            KeyCode::Left => {
                focus = (focus + 7) % 8;
                selected = 0;
            }
            KeyCode::Right => {
                focus = (focus + 1) % 8;
                selected = 0;
            }
            KeyCode::Up | KeyCode::Char('k') if !control => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if !control => {
                let count = sel_count(&stats, focus);
                selected = (selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Enter => {
                detail = open_detail(&stats, focus, selected, width as usize);
            }
            KeyCode::Char('c') if control => signal_selected(&stats, focus, selected, false),
            KeyCode::Char('k') if control => signal_selected(&stats, focus, selected, true),
            _ => {}
        }
    }
    Ok(())
}

/// Signal the selected process in the Processes panel (Ctrl+C → SIGINT,
/// Ctrl+K → SIGKILL), exactly like the TUI's dashboard keys — which only
/// fire while the Processes panel holds focus.
fn signal_selected(stats: &DashStats, focus: usize, selected: usize, kill: bool) {
    if focus != 0 {
        return;
    }
    if let Some(proc) = stats.procs.get(selected) {
        let _ = crate::tools::shell::signal(proc.pid, kill);
    }
}

/// Which panel rows can be selected (mirrors the TUI's dash_move targets).
fn sel_count(stats: &DashStats, focus: usize) -> usize {
    let count = match focus {
        0 => stats.procs.len(),
        5 => stats.changed_files.len(),
        _ => 1,
    };
    count.max(1)
}

/// Enter on a row: the selected process's full tail, the selected file's
/// git diff, else the panel's body.
fn open_detail(
    stats: &DashStats,
    focus: usize,
    selected: usize,
    width: usize,
) -> Option<(String, String)> {
    if focus == 0 {
        let proc = stats.procs.get(selected)?;
        let body = format!(
            "pid {}\ncommand {}\ncwd {}\nage {}s\nstatus {}\ncpu {} · rss {}\n\nOUTPUT (live tail)\n{}",
            proc.pid,
            proc.command,
            proc.cwd,
            proc.age_s,
            status_text(proc),
            proc.cpu_pct
                .map(|value| format!("{value:.1}%"))
                .unwrap_or_else(|| "n/a".into()),
            proc.rss_kb
                .map(|value| format!("{} KB", value / 1024))
                .unwrap_or_else(|| "n/a".into()),
            proc.tail
        );
        return Some((format!("PROCESS {}", proc.pid), body));
    }
    if focus == 5 {
        let (path, _, _) = stats.changed_files.get(selected)?;
        let diff = std::process::Command::new("git")
            .args(["-C", &stats.root, "diff", "HEAD", "--", path])
            .output()
            .map(|output| {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                if text.trim().is_empty() {
                    text = "(untracked or staged-only file — no unstaged diff)".into();
                }
                text
            })
            .unwrap_or_else(|error| format!("git diff failed: {error}"));
        return Some((format!("DIFF {path}"), diff));
    }
    let (title, rows) = tui::dashboard_panels(stats, width.saturating_sub(1))
        .into_iter()
        .nth(focus)?;
    Some((title.to_string(), rows.join("\n")))
}

fn status_text(proc: &tui::ProcStat) -> String {
    match (proc.running, proc.exit_code) {
        (true, _) => "running".into(),
        (false, Some(code)) => format!("exited {code}"),
        (false, None) => "done".into(),
    }
}

/// Paint one frame with the terminal's own colours (TUI-compatible diff-free
/// full repaint; the ≤10 Hz cap keeps flicker and CPU in check).
fn paint(stdout: &mut impl Write, frame: &[Row]) -> Result<()> {
    queue!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    let last = frame.len().saturating_sub(1);
    for (index, row) in frame.iter().enumerate() {
        queue!(
            stdout,
            SetForegroundColor(row.fg),
            SetBackgroundColor(row.bg),
            Print(&row.text)
        )?;
        if index < last {
            queue!(stdout, Print("\r\n"))?;
        }
    }
    stdout.flush()?;
    Ok(())
}

/// `--once`: one plain-text frame, no ANSI (pipe/grep friendly).
fn print_plain(stats: &DashStats, width: usize, height: usize) {
    let colors = tui::dashboard_theme(true);
    let (frame, _) = tui::compose_grid(stats, width as u16, height as u16, colors, 0, 0, None);
    let mut out = String::new();
    for row in &frame {
        out.push_str(&row.text);
        out.push('\n');
    }
    print!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> DashStats {
        DashStats {
            root: env!("CARGO_MANIFEST_DIR").into(),
            procs: vec![tui::ProcStat {
                pid: 42,
                command: "sleep 99".into(),
                cwd: "/tmp".into(),
                age_s: 3,
                running: true,
                exit_code: None,
                timed_out: false,
                cpu_pct: Some(1.5),
                rss_kb: Some(2048),
                tail: "tail line".into(),
            }],
            changed_files: vec![("src/main.rs".into(), 5, 0)],
            plan: vec![
                ("one".into(), false),
                ("two".into(), false),
                ("three".into(), false),
            ],
            flags: vec![
                ("flag{a}".into(), "checker".into()),
                ("flag{b}".into(), "checker".into()),
            ],
            ..DashStats::default()
        }
    }

    #[test]
    fn sel_count_follows_the_selectable_panels() {
        let stats = stats();
        // Grid order (spec 3.2): 0 PROCESSES, 5 FILES — the only panels the
        // TUI lets you move a selection inside.
        assert_eq!(sel_count(&stats, 0), 1);
        assert_eq!(sel_count(&stats, 5), 1);
        assert_eq!(sel_count(&stats, 3), 1, "plan rows are not selectable");
        assert_eq!(sel_count(&stats, 6), 1, "flags are not selectable");
        assert_eq!(sel_count(&DashStats::default(), 0), 1, "always ≥1");
        let many = DashStats {
            changed_files: vec![
                ("a.rs".into(), 1, 0),
                ("b.rs".into(), 1, 0),
                ("c.rs".into(), 1, 0),
            ],
            ..DashStats::default()
        };
        assert_eq!(sel_count(&many, 5), 3);
    }

    #[test]
    fn open_detail_builds_process_panel_and_diff_bodies() {
        let stats = stats();

        let (title, body) = open_detail(&stats, 0, 0, 100).expect("process detail");
        assert_eq!(title, "PROCESS 42");
        assert!(body.contains("sleep 99"), "{body}");
        assert!(body.contains("tail line"), "{body}");
        assert!(body.contains("running"), "{body}");
        assert!(open_detail(&stats, 0, 7, 100).is_none(), "no such pid");

        let (title, _) = open_detail(&stats, 5, 0, 100).expect("file diff");
        assert_eq!(title, "DIFF src/main.rs");

        let (title, body) = open_detail(&stats, 1, 0, 100).expect("panel body");
        assert_eq!(title, "THINKING");
        assert!(!body.is_empty());
    }

    #[test]
    fn offline_stats_produces_a_fresh_publishable_frame() {
        let stats = offline_stats();
        assert_eq!(stats.schema, "wrosecode/live-v1");
        assert_eq!(stats.status, "offline");
        assert!(stats.live_at_ms > 0, "offline frames still gate freshness");
        // It must render: one frame of rows, none wider than the width.
        let colors = tui::dashboard_theme(true);
        let (frame, _) = tui::compose_grid(&stats, 100, 30, colors, 0, 0, None);
        assert_eq!(frame.len(), 30);
        for (index, row) in frame.iter().enumerate() {
            assert!(
                row.text.chars().count() <= 100,
                "row {index} overflows: {:?}",
                row.text
            );
        }
    }
}
