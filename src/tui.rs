use crate::agent::Agent;
use crate::harness::Harness;
use crate::provider::Progress;
use crate::provider::Usage;
use crate::splash::{self, LOGO};
use crate::think::ThinkLevel;
use crate::tools::PermissionRequest;
use crate::{
    commands, package, project, provider,
    session::Session,
    settings::{McpServerDef, ProviderProfile, Settings},
    skills,
};
use anyhow::Result;
use crossterm::cursor;
use crossterm::event::{
    self, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::style::{Color, Print, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, PartialEq)]
pub(crate) struct Row {
    pub(crate) text: String,
    pub(crate) fg: Color,
    pub(crate) bg: Color,
    /// Per-character colours for the wordmark gradient. `None` keeps the
    /// single-`fg` fast path every other row uses.
    pub(crate) colors: Option<Vec<Color>>,
}

impl Row {
    fn new(text: impl Into<String>, fg: Color) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: Color::Reset,
            colors: None,
        }
    }
}

struct Renderer {
    previous: Vec<Row>,
    size: (u16, u16),
    /// Redraw every cell on every frame. Terminals whose cell buffer drifts
    /// (Zellij, some multiplexers) show stale glyphs with diff-only updates.
    full_redraw: bool,
}

impl Renderer {
    fn new() -> Self {
        Self {
            previous: Vec::new(),
            size: (0, 0),
            full_redraw: false,
        }
    }

    fn draw(&mut self, rows: &[Row], input_cursor: (u16, u16), size: (u16, u16)) -> io::Result<()> {
        let mut out = io::stdout().lock();
        if self.size != size || self.full_redraw {
            queue!(out, Clear(ClearType::All))?;
            self.previous.clear();
            self.size = size;
        }
        for (index, row) in rows.iter().enumerate() {
            if self.previous.get(index) == Some(row) {
                continue;
            }
            queue!(
                out,
                cursor::MoveTo(0, index as u16),
                SetForegroundColor(row.fg),
                SetBackgroundColor(row.bg),
                Clear(ClearType::CurrentLine)
            )?;
            match &row.colors {
                // Gradient rows paint one colour per character; the row-level
                // `fg` above still lands first so the tail after the gradient
                // has a sane colour even if the colour list runs short.
                Some(palette) => {
                    let mut painted = 0usize;
                    for (character, color) in row.text.chars().zip(palette) {
                        queue!(out, SetForegroundColor(*color), Print(character))?;
                        painted += 1;
                    }
                    let rest: String = row.text.chars().skip(painted).collect();
                    if !rest.is_empty() {
                        queue!(out, SetForegroundColor(row.fg), Print(rest))?;
                    }
                }
                None => queue!(out, Print(&row.text))?,
            }
        }
        // A shrinking frame (resize, closing a picker) must not leave the old
        // tail on screen.
        if rows.len() < self.previous.len() {
            for index in rows.len()..self.previous.len() {
                queue!(
                    out,
                    cursor::MoveTo(0, index as u16),
                    Clear(ClearType::CurrentLine)
                )?;
            }
        }
        queue!(
            out,
            SetForegroundColor(Color::Reset),
            SetBackgroundColor(Color::Reset),
            cursor::MoveTo(input_cursor.0, input_cursor.1),
            cursor::Show
        )?;
        out.flush()?;
        self.previous = rows.to_vec();
        Ok(())
    }
}

/// How the current terminal wants to be driven. Each field can be forced off with
/// an environment variable so a broken terminal never needs a code change:
/// `WROSECODE_NO_ALT_SCREEN`, `WROSECODE_NO_MOUSE`, `WROSECODE_FULL_REDRAW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalProfile {
    pub alternate: bool,
    pub mouse: bool,
    pub full_redraw: bool,
    pub colors: ColorSupport,
}

/// The terminal facts [`TerminalProfile::decide`] reasons over. Collected from the
/// environment so the decision logic stays pure and unit-testable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEnv {
    pub term: String,
    pub program: String,
    pub vscode: bool,
    pub multiplexer: bool,
    pub no_alt_screen: bool,
    pub force_full_redraw: bool,
    /// `NO_COLOR` set to a non-empty value, or `--no-color` on the command line.
    pub force_no_color: bool,
    /// `COLORTERM` — `truecolor`/`24bit` advertises 24-bit colour.
    pub colorterm: String,
}

impl TerminalEnv {
    pub fn from_process() -> Self {
        let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
        Self {
            term: std::env::var("TERM").unwrap_or_default(),
            vscode: program.eq_ignore_ascii_case("vscode")
                || program.eq_ignore_ascii_case("vscode-insiders")
                || std::env::var_os("VSCODE_PID").is_some()
                || std::env::var_os("VSCODE_CWD").is_some(),
            program,
            multiplexer: std::env::var_os("ZELLIJ").is_some(),
            no_alt_screen: std::env::var_os("WROSECODE_NO_ALT_SCREEN").is_some(),
            force_full_redraw: std::env::var_os("WROSECODE_FULL_REDRAW").is_some(),
            force_no_color: std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
            colorterm: std::env::var("COLORTERM").unwrap_or_default(),
        }
    }
}

impl TerminalProfile {
    /// `configured_alternate` comes from `[ui] alternate_screen`; `mouse_capture`
    /// is `"auto"`, `"on"` or `"off"`. `no_color_flag` is `--no-color`.
    pub fn detect(
        configured_alternate: bool,
        mouse_capture: Option<&str>,
        no_color_flag: bool,
    ) -> Self {
        let mut env = TerminalEnv::from_process();
        env.force_no_color |= no_color_flag;
        Self::decide(configured_alternate, mouse_capture, &env)
    }

    /// Pure decision function. `mouse_capture` of `None` means `"auto"`.
    pub fn decide(
        configured_alternate: bool,
        mouse_capture: Option<&str>,
        env: &TerminalEnv,
    ) -> Self {
        let dumb = env.term.is_empty() || env.term == "dumb";
        // VS Code reserves the wheel for its own scrollback, so mouse capture
        // fights the user; keep the terminal's native selection instead.
        let mouse = match mouse_capture
            .unwrap_or("auto")
            .to_ascii_lowercase()
            .as_str()
        {
            "on" | "true" | "yes" | "1" => !dumb,
            "off" | "false" | "no" | "0" => false,
            _ => !dumb && !env.vscode,
        };
        let alternate = configured_alternate && !dumb && !env.no_alt_screen;
        // Multiplexers that composite their own cell grid keep stale glyphs unless
        // the whole frame is repainted every tick.
        let full_redraw = env.multiplexer || env.force_full_redraw;
        // `NO_COLOR` (and `--no-color`) turn colour off entirely; otherwise pick
        // the richest channel the terminal advertises so the wordmark gradient
        // can degrade from truecolor to the 256-colour cube to plain ANSI.
        let colors = if env.force_no_color || dumb {
            ColorSupport::None
        } else if env.colorterm.contains("truecolor") || env.colorterm.contains("24bit") {
            ColorSupport::TrueColor
        } else if env.term.contains("256color") {
            ColorSupport::Ansi256
        } else {
            ColorSupport::Ansi16
        };
        Self {
            alternate,
            mouse,
            full_redraw,
            colors,
        }
    }
}

/// How much colour the current terminal can show. Drives the wordmark
/// gradient (truecolor → 256 → plain) and everything that opts out of
/// painting via `NO_COLOR` / `--no-color`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorSupport {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

impl ColorSupport {
    /// Whether per-character gradient painting is worth doing at this level.
    pub fn gradient(self) -> bool {
        matches!(self, Self::Ansi256 | Self::TrueColor)
    }
}

/// Sample points for the wordmark gradient: sky → violet → pink.
const GRADIENT_STOPS: [(u8, u8, u8); 3] = [(90, 200, 250), (167, 139, 250), (244, 114, 182)];

fn lerp8(from: u8, to: u8, t: f32) -> u8 {
    (from as f32 + (to as f32 - from as f32) * t).round() as u8
}

/// Sample the gradient at `t` in `0.0..=1.0`, piecewise-linear across the stops.
fn sample_gradient(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let segments = GRADIENT_STOPS.len() - 1;
    let scaled = t * segments as f32;
    let index = (scaled.floor() as usize).min(segments - 1);
    let local = scaled - index as f32;
    let (r1, g1, b1) = GRADIENT_STOPS[index];
    let (r2, g2, b2) = GRADIENT_STOPS[index + 1];
    (
        lerp8(r1, r2, local),
        lerp8(g1, g2, local),
        lerp8(b1, b2, local),
    )
}

/// Map 24-bit colour onto the xterm 6×6×6 colour cube for 256-colour terminals.
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let component = |value: u8| -> u8 {
        if value < 48 {
            0
        } else {
            (((value as u16 - 35) / 40) as u8).min(5)
        }
    };
    16 + 36 * component(r) + 6 * component(g) + component(b)
}

/// One colour per character of `text`, sampled from the wordmark gradient.
/// `None` when the terminal cannot show the gradient (`Ansi16` falls back to
/// the theme accent at the row level instead, `None` paints nothing).
pub(crate) fn gradient_colors(text: &str, support: ColorSupport) -> Option<Vec<Color>> {
    if !support.gradient() {
        return None;
    }
    let chars = text.chars().count();
    if chars == 0 {
        return Some(Vec::new());
    }
    Some(
        (0..chars)
            .map(|index| {
                let t = if chars == 1 {
                    0.5
                } else {
                    index as f32 / (chars - 1) as f32
                };
                let (r, g, b) = sample_gradient(t);
                match support {
                    ColorSupport::TrueColor => Color::Rgb { r, g, b },
                    _ => Color::AnsiValue(rgb_to_ansi256(r, g, b)),
                }
            })
            .collect(),
    )
}

/// The splash's pre-TUI equivalent of [`gradient_colors`]: returns `text`
/// either plain (no colour), wrapped in one ANSI-16 accent, or carrying
/// embedded SGR sequences per character. The escape codes live in the string
/// because the splash writes bytes before the row renderer exists.
pub(crate) fn gradient_paint(text: &str, support: ColorSupport) -> String {
    let Some(palette) = gradient_colors(text, support) else {
        return match support {
            // The 16-colour fallback: the whole wordmark in the accent cyan.
            ColorSupport::Ansi16 => format!("\x1b[36m{text}\x1b[0m"),
            _ => text.to_string(),
        };
    };
    let mut painted = String::new();
    for (character, color) in text.chars().zip(&palette) {
        match *color {
            Color::Rgb { r, g, b } => painted.push_str(&format!("\x1b[38;2;{r};{g};{b}m")),
            Color::AnsiValue(value) => painted.push_str(&format!("\x1b[38;5;{value}m")),
            _ => {}
        }
        painted.push(character);
    }
    painted.push_str("\x1b[0m");
    painted
}

/// The splash's rotating tips row (spec: Phase 2).
const TIPS: [&str; 5] = [
    "/ commands",
    "@ files",
    "ctrl+t thinking level",
    "ctrl+d dashboard",
    "ctrl+o expand output",
];

/// The wordmark / header when colour is off: every field `Reset` so a
/// `NO_COLOR` session paints no foreground at all.
const PLAIN_THEME: Theme = Theme {
    name: "plain",
    text: Color::Reset,
    muted: Color::Reset,
    accent: Color::Reset,
    status: Color::Reset,
    background: Color::Reset,
};

/// The project root's git branch, with a trailing `*` when tracked files are
/// dirty. `None` outside a repository (or when git is not installed).
fn git_state(root: &std::path::Path) -> Option<String> {
    let branch = git_stdout(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .or_else(|| git_stdout(root, &["symbolic-ref", "--short", "HEAD"]))?;
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }
    // `diff-index` skips untracked files, which keeps this fast on big trees.
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(false);
    Some(if dirty {
        format!("{branch}*")
    } else {
        branch.to_string()
    })
}

fn git_stdout(root: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One-line provider health for the splash: `3 connected · 2 need setup`.
fn provider_health_line() -> String {
    let Ok(settings) = Settings::load() else {
        return "providers unavailable".into();
    };
    if settings.providers.is_empty() {
        return "no providers yet · /connect".into();
    }
    let mut connected = 0usize;
    let mut setup = 0usize;
    for profile in &settings.providers {
        if settings.key(profile).is_some() || settings.no_key_needed(profile) {
            connected += 1;
        } else {
            setup += 1;
        }
    }
    format!("{connected} connected · {setup} need setup")
}

/// The last three saved sessions (newest first) for the splash's resume rows.
fn recent_sessions() -> Vec<(String, String)> {
    Session::dir()
        .ok()
        .and_then(|dir| Session::list(&dir).ok())
        .map(|sessions| {
            sessions
                .into_iter()
                .take(3)
                .map(|session| (session.name, session.summary))
                .collect()
        })
        .unwrap_or_default()
}

/// Owns the raw-mode / alternate-screen / mouse capture state for the whole
/// session. `main` enters it early (so the splash can paint) and hands it to
/// [`run`], which keeps it alive until the session ends.
pub struct TerminalGuard {
    profile: TerminalProfile,
}

impl TerminalGuard {
    pub fn enter(profile: TerminalProfile) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let entered = match (profile.alternate, profile.mouse) {
            (true, true) => execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture,
                cursor::Hide
            ),
            (true, false) => execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                cursor::Hide
            ),
            (false, true) => execute!(
                io::stdout(),
                EnableBracketedPaste,
                EnableMouseCapture,
                cursor::Hide
            ),
            (false, false) => execute!(io::stdout(), EnableBracketedPaste, cursor::Hide),
        };
        if let Err(error) = entered {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self { profile })
    }

    pub fn profile(&self) -> TerminalProfile {
        self.profile
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // One teardown path shared with the panic hook and the signal
        // handlers (src/crash.rs) so every exit leaves a usable shell.
        crate::crash::restore();
    }
}

#[derive(Clone, Copy)]
enum Speaker {
    User,
    Agent,
    Tool,
    System,
}

struct Entry {
    speaker: Speaker,
    text: String,
}

struct Ui {
    renderer: Renderer,
    entries: Vec<Entry>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    scroll: usize,
    status: String,
    draft: String,
    busy: bool,
    spinner: usize,
    root: String,
    /// User-defined slash commands from `.wrosecode/commands/*.md`, loaded
    /// once at startup for the palette, `/help`, and dispatch.
    user_commands: Vec<crate::markdown::UserCommand>,
    provider: String,
    model: String,
    harness: String,
    mode: String,
    permission: String,
    palette: bool,
    selected: usize,
    picker: Option<(String, Vec<String>)>,
    /// Unfiltered source for the `Ctrl+F` / `Ctrl+R` pickers; `None` means the
    /// picker owns its list and typing should not refilter it here.
    picker_all: Option<Vec<String>>,
    /// Live fuzzy query typed while a filterable picker is open.
    picker_query: String,
    mask_input: bool,
    tool_rows: HashMap<String, usize>,
    verbosity: String,
    tool_calls: usize,
    tool_failures: usize,
    session_started: std::time::Instant,
    session_id: String,
    category: String,
    thinking_level: u8,
    /// The configured thinking mode: cycled by Ctrl+T / `[` `]`, set by
    /// `/think`, seeded from `--think` → profile → config.
    think: ThinkLevel,
    /// A provider rejected our thinking control this session; the status bar
    /// shows `think:… (ignored)` until the level is changed again.
    think_ignored: bool,
    /// Reasoning tokens and model time of the previous turn (dashboard's
    /// per-turn reasoning line).
    last_turn_reasoning: u64,
    last_turn_model_ms: u64,
    usage: Usage,
    flags_found: usize,
    navigation_mode: bool,
    theme: usize,
    pane_percent: usize,
    dragging_separator: bool,
    tool_timeline: Vec<String>,
    metrics: crate::metrics::MetricsSnapshot,
    timeout_seconds: u64,
    search_query: String,
    search_matches: Vec<usize>,
    search_position: usize,
    budget_usd: f64,
    last_flag: String,
    turn_tokens: Vec<u64>,
    turn_usage: Usage,
    turn_started: Option<std::time::Instant>,
    last_turn_ms: u64,
    error_count: usize,
    last_error_kind: &'static str,
    completion: Option<CompletionState>,
    /// Wrapped transcript rows, rebuilt only when the transcript, the pane
    /// width, or the verbosity changes. Scrolling just reslices this, which is
    /// what keeps a long session from re-wrapping every line on every key.
    transcript_lines: Vec<String>,
    transcript_key: Option<(usize, usize, usize, bool)>,
    transcript_rebuilds: u64,
    /// Rows the transcript viewport could show at the last render; PageUp and
    /// friends page by this instead of a hardcoded five lines.
    page_size: usize,
    /// Largest scroll offset the last render allowed (`0` means "at bottom").
    max_scroll: usize,
    /// Messages typed while the agent was thinking, sent after the turn ends.
    pending: Vec<String>,
    /// Lines that arrived while the viewport was detached from the bottom,
    /// shown as `↓ N new lines  (End to jump)` until End re-engages follow.
    new_since_detach: usize,
    /// A large bracketed paste waiting to be inserted: the text and its
    /// line count, so one big paste never hammers the key handlers.
    paste_chip: Option<(String, usize)>,
    /// Colour depth for the wordmark gradient (from the terminal profile).
    colors: ColorSupport,
    /// `NO_COLOR` / `--no-color`: paint every row with the plain theme.
    no_color: bool,
    /// Current git branch of the project root, with a `*` when tracked files
    /// are dirty. `None` outside a git repository.
    git: Option<String>,
    /// Connected MCP servers.
    mcp_count: usize,
    /// Discovered skills.
    skill_count: usize,
    /// Sandbox engine name (`none`, `docker`, …).
    sandbox: String,
    /// One-line provider health, e.g. `3 connected · 2 need setup`.
    provider_health: String,
    /// The last three saved sessions, newest first: (id, summary).
    recent: Vec<(String, String)>,
    /// Index into [`TIPS`] for the rotating splash tip row.
    tip_index: usize,
    /// Tool calls in the current turn — the status bar's `step n`.
    turn_steps: usize,
    // ---- Phase 3: transcript cells ----
    /// Finished and in-flight tool cells, keyed by tool-call id.
    tool_cells: HashMap<String, ToolCell>,
    /// Entry rows rendered expanded (full output instead of a preview).
    expanded: HashSet<usize>,
    /// Nested delegate children keyed by their full id path (`parent/child`),
    /// rendered under the root cell of the chain.
    child_cells: HashMap<String, ToolCell>,
    /// Read-only call ids in the active group: id -> (group row, file read).
    ro_ids: HashMap<String, (usize, bool)>,
    /// Group counters keyed by the group entry's row.
    groups: HashMap<usize, GroupCell>,
    /// The group new read-only calls may join; `None` while any other kind
    /// of entry owns the transcript tail.
    active_group: Option<usize>,
    /// Cumulative coverage for the CTF panel: files read, searches, shells.
    files_total: usize,
    searches_total: usize,
    shell_calls: usize,
    /// This turn's reasoning cell: its row, and whether it has been
    /// finalized into a static `Thought  n.ns` line.
    think_row: Option<usize>,
    think_finished: bool,
    /// Transcript geometry from the last compose (click-to-expand).
    view: TranscriptView,
    /// Entry at the top of the viewport — the `Ctrl+O` / Enter target.
    top_entry: Option<usize>,
    /// Entry index per transcript line, rebuilt with the wrapped lines.
    line_origin: Vec<usize>,
    // ---- Phase 3: dashboard ----
    dashboard: bool,
    /// Focused panel, 0..8 (see [`DASH_PANELS`]).
    dash_focus: usize,
    /// Selection within the focused panel (a process or file row).
    dash_sel: usize,
    /// Open detail view: (title, body) for a process tail or a file diff.
    dash_detail: Option<(String, String)>,
    dash_procs: Vec<crate::tools::shell::ProcSnapshot>,
    dash_files: Vec<(String, i64, i64)>,
    dash_data_at: Option<std::time::Instant>,
    dash_files_at: Option<std::time::Instant>,
    /// Last `~/.wrosecode/live.json` write, rate-limited to one per second.
    live_written_at: Option<std::time::Instant>,
    /// The live checklist the model maintains through `update_plan`.
    plan: Vec<(String, bool)>,
    /// Recent flag hits: (flag, source), capped.
    flag_log: Vec<(String, String)>,
    /// Files edited or written this session (first seen order).
    changed_files: Vec<String>,
    /// A `Checker VERIFIED` line landed during the current task.
    verified: bool,
    /// The `/ctf` auto-offer hint has already been shown this session.
    ctf_offered: bool,
    // ---- Phase 3: result block accounting ----
    tool_ms_total: u128,
    model_ms_total: u128,
    turns_total: usize,
    max_parallel: usize,
    inflight_tools: usize,
    turn_tool_ms: u128,
    task_started: Option<std::time::Instant>,
    task_base: Option<TaskBase>,
}

/// State for cycling through Tab completions with the same token.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletionState {
    token_start: usize,
    matches: Vec<String>,
    index: usize,
}

/// A tool call's transcript cell, kept by id so the row can re-render when
/// output lands, when the row expands, or when a child tool reports in.
#[derive(Clone, Debug)]
struct ToolCell {
    title: String,
    running: bool,
    ok: bool,
    /// Full tool output (capped): expansion renders this, not the entry.
    output: String,
    elapsed_ms: u128,
    /// `exit=N` parsed from the result, when present.
    exit_code: Option<i32>,
    row: usize,
}

/// State behind a `▸ Explored …` group: completed counts plus what is still
/// in flight, so the cell grows live and finalizes when the last call ends.
#[derive(Clone, Debug, Default)]
struct GroupCell {
    files: usize,
    searches: usize,
    inflight: usize,
}

/// Where the transcript viewport sat at the last compose, so a click can map
/// a terminal row back to a transcript line (and then to its entry).
#[derive(Clone, Copy, Debug, Default)]
struct TranscriptView {
    body_start: usize,
    transcript_start: usize,
    height: usize,
}

/// Session counters captured when a task (a prompt plus anything queued
/// behind it) starts, so the result block reports deltas, not totals.
#[derive(Clone, Copy, Debug, Default)]
struct TaskBase {
    tool_calls: usize,
    tokens_in: u64,
    tokens_out: u64,
    tokens_reason: u64,
    cache_read: u64,
    cost: f64,
    has_cost: bool,
    flags: usize,
    errors: usize,
    tool_ms: u128,
    model_ms: u128,
    files: usize,
}

/// The eight dashboard panels, in grid order (spec 3.2).
const DASH_PANELS: [&str; 8] = [
    "PROCESSES",
    "THINKING",
    "TIMELINE",
    "PLAN",
    "TOKENS & COST",
    "FILES",
    "CTF",
    "BUDGET",
];

/// One tracked shell process as it appears in the dashboard and in
/// `~/.wrosecode/live.json` (a plain serializable view of the registry).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ProcStat {
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub age_s: u64,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cpu_pct: Option<f64>,
    pub rss_kb: Option<u64>,
    pub tail: String,
}

impl From<&crate::tools::shell::ProcSnapshot> for ProcStat {
    fn from(snapshot: &crate::tools::shell::ProcSnapshot) -> Self {
        Self {
            pid: snapshot.pid,
            command: snapshot.command.clone(),
            cwd: snapshot.cwd.clone(),
            age_s: snapshot.started.elapsed().as_secs(),
            running: snapshot.running,
            exit_code: snapshot.exit_code,
            timed_out: snapshot.timed_out,
            cpu_pct: snapshot.cpu_pct,
            rss_kb: snapshot.rss_kb,
            tail: snapshot.tail.clone(),
        }
    }
}

/// Everything a dashboard render, `/stats`, `wrosecode dashboard`, and
/// `exec --json` need, as one serializable snapshot (spec 3.2). Field-level
/// defaults let the attach client parse both `live.json` and the smaller
/// `/v1/status` payload.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct DashStats {
    pub schema: String,
    pub live_at_ms: u64,
    pub session_id: String,
    pub root: String,
    pub provider: String,
    pub model: String,
    pub mode: String,
    pub category: String,
    pub thinking_level: u8,
    /// Thinking label for the current level (`high`, `auto→medium`, …).
    pub think: String,
    pub status: String,
    pub elapsed_s: u64,
    pub budget_usd: f64,
    pub cost_usd: Option<f64>,
    pub usage: Usage,
    pub cache_hit_pct: u64,
    pub tool_calls: usize,
    pub tool_failures: usize,
    pub errors: usize,
    pub turns: usize,
    pub model_ms: u64,
    /// Reasoning tokens and model time of the previous (or live) turn —
    /// the THINKING panel's per-turn line (spec phase 4).
    pub turn_reasoning_tok: u64,
    pub turn_model_ms: u64,
    pub tool_ms: u64,
    pub wait_ms: u64,
    pub max_parallel: usize,
    pub inflight: usize,
    pub plan: Vec<(String, bool)>,
    pub flags: Vec<(String, String)>,
    pub flags_total: usize,
    pub files_read: usize,
    pub searches: usize,
    pub shells: usize,
    pub changed_files: Vec<(String, i64, i64)>,
    pub procs: Vec<ProcStat>,
    pub turn_tokens: Vec<u64>,
    pub verified: bool,
    pub last_error: String,
}

/// The handful of agent facts the shell paints. Split from `Ui::new` so tests
/// can build a `Ui` without an `Agent` (constructing one opens the session
/// store under `$HOME`, which would make tests environment-dependent).
struct UiMeta {
    root: String,
    provider: String,
    model: String,
    harness: String,
    permission: String,
    verbosity: String,
    category: String,
    thinking_level: u8,
    think: ThinkLevel,
    theme: usize,
    timeout_seconds: u64,
    budget_usd: f64,
    git: Option<String>,
    mcp_count: usize,
    skill_count: usize,
    sandbox: String,
    provider_health: String,
    recent: Vec<(String, String)>,
}

impl From<&Agent> for UiMeta {
    fn from(agent: &Agent) -> Self {
        Self {
            root: agent.config.root.display().to_string(),
            provider: agent.config.provider.clone(),
            model: agent.config.model.clone(),
            harness: agent.harness.name().into(),
            permission: format!("{:?}", agent.config.permission).to_ascii_lowercase(),
            verbosity: agent.config.verbosity.clone(),
            category: agent.ctf.category.clone(),
            thinking_level: agent.thinking_level,
            think: agent.think,
            theme: theme_index(&agent.config.ui_theme),
            timeout_seconds: agent.config.shell_timeout_seconds,
            budget_usd: agent.config.budget_usd,
            git: git_state(std::path::Path::new(&agent.config.root)),
            mcp_count: agent.tools.mcps.len(),
            skill_count: agent.skills.len(),
            sandbox: agent.config.sandbox.engine.clone(),
            provider_health: provider_health_line(),
            recent: recent_sessions(),
        }
    }
}

#[cfg(test)]
impl UiMeta {
    fn for_test() -> Self {
        Self {
            root: "/tmp/wrose-test".into(),
            provider: "stub".into(),
            model: "stub-model".into(),
            harness: "Claude".into(),
            permission: "ask".into(),
            verbosity: "normal".into(),
            category: "misc".into(),
            thinking_level: 5,
            think: ThinkLevel::Medium,
            theme: 0,
            timeout_seconds: 30,
            budget_usd: 0.0,
            git: None,
            mcp_count: 0,
            skill_count: 0,
            sandbox: "none".into(),
            provider_health: "0 connected · 0 need setup".into(),
            recent: Vec::new(),
        }
    }
}

impl Ui {
    fn new(agent: &Agent, session_id: String) -> Self {
        Self::from_meta(UiMeta::from(agent), session_id)
    }

    fn from_meta(meta: UiMeta, session_id: String) -> Self {
        let user_commands = crate::markdown::discover_commands(std::path::Path::new(&meta.root));
        Self {
            renderer: Renderer::new(),
            entries: vec![Entry {
                speaker: Speaker::System,
                text: format!(
                    "WROSECODE v{} · session {}\n\
                     Type a request · /help commands · Ctrl+P palette\n\
                     Tab build/plan · [ ] thinking · PgUp/PgDn scroll · /clear",
                    env!("CARGO_PKG_VERSION"),
                    session_id
                ),
            }],
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            completion: None,
            scroll: 0,
            status: "Ready".into(),
            draft: String::new(),
            busy: false,
            spinner: 0,
            root: meta.root,
            user_commands,
            provider: meta.provider,
            model: meta.model,
            harness: meta.harness,
            mode: "BUILD".into(),
            permission: meta.permission,
            palette: false,
            selected: 0,
            picker: None,
            picker_all: None,
            picker_query: String::new(),
            mask_input: false,
            tool_rows: HashMap::new(),
            verbosity: meta.verbosity,
            tool_calls: 0,
            tool_failures: 0,
            session_started: std::time::Instant::now(),
            session_id,
            category: meta.category,
            thinking_level: meta.thinking_level,
            think: meta.think,
            think_ignored: false,
            last_turn_reasoning: 0,
            last_turn_model_ms: 0,
            usage: Usage::default(),
            flags_found: 0,
            navigation_mode: false,
            theme: meta.theme,
            pane_percent: 68,
            dragging_separator: false,
            tool_timeline: Vec::new(),
            metrics: crate::metrics::MetricsSnapshot::default(),
            timeout_seconds: meta.timeout_seconds,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_position: 0,
            budget_usd: meta.budget_usd,
            last_flag: String::new(),
            turn_tokens: Vec::new(),
            turn_usage: Usage::default(),
            turn_started: None,
            last_turn_ms: 0,
            error_count: 0,
            last_error_kind: "",
            transcript_lines: Vec::new(),
            transcript_key: None,
            transcript_rebuilds: 0,
            page_size: 10,
            max_scroll: 0,
            pending: Vec::new(),
            new_since_detach: 0,
            paste_chip: None,
            colors: ColorSupport::None,
            no_color: false,
            git: meta.git,
            mcp_count: meta.mcp_count,
            skill_count: meta.skill_count,
            sandbox: meta.sandbox,
            provider_health: meta.provider_health,
            recent: meta.recent,
            tip_index: 0,
            turn_steps: 0,
            tool_cells: HashMap::new(),
            expanded: HashSet::new(),
            child_cells: HashMap::new(),
            ro_ids: HashMap::new(),
            groups: HashMap::new(),
            active_group: None,
            files_total: 0,
            searches_total: 0,
            shell_calls: 0,
            think_row: None,
            think_finished: true,
            view: TranscriptView::default(),
            top_entry: None,
            line_origin: Vec::new(),
            dashboard: false,
            dash_focus: 0,
            dash_sel: 0,
            dash_detail: None,
            dash_procs: Vec::new(),
            dash_files: Vec::new(),
            dash_data_at: None,
            dash_files_at: None,
            live_written_at: None,
            plan: Vec::new(),
            flag_log: Vec::new(),
            changed_files: Vec::new(),
            verified: false,
            ctf_offered: false,
            tool_ms_total: 0,
            model_ms_total: 0,
            turns_total: 0,
            max_parallel: 0,
            inflight_tools: 0,
            turn_tool_ms: 0,
            task_started: None,
            task_base: None,
        }
    }

    fn begin_turn(&mut self) {
        self.turn_usage = Usage::default();
        self.turn_started = Some(std::time::Instant::now());
        self.turn_steps = 0;
        self.turn_tool_ms = 0;
        // A dimmed reasoning cell opens each turn; the first model activity
        // freezes it into `Thought  n.ns` (spec 3.1).
        self.think_finished = false;
        self.active_group = None;
        let row = self.entries.len();
        self.push_scrolled(
            Entry {
                speaker: Speaker::System,
                text: format!("{} Thinking…", SPINNER[0]),
            },
            1,
        );
        self.think_row = Some(row);
    }

    /// Freeze the live reasoning cell with its elapsed time. Safe to call
    /// repeatedly: only the first call rewrites the row.
    fn finalize_think(&mut self) {
        if self.think_finished {
            return;
        }
        self.think_finished = true;
        let Some(row) = self.think_row else { return };
        let secs = self
            .turn_started
            .map(|started| started.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        self.set_entry_text(row, format!("  Thought  {secs:.1}s"));
    }

    /// Tick the live `Thinking… 2.1s` cell from the render ticker.
    fn tick_thinking(&mut self) {
        if self.think_finished {
            return;
        }
        let Some(started) = self.turn_started else {
            return;
        };
        let secs = started.elapsed().as_secs_f64();
        let Some(row) = self.think_row else { return };
        let next = format!(
            "{} Thinking… {secs:.1}s",
            SPINNER[self.spinner % SPINNER.len()]
        );
        if self.entries[row].text != next {
            self.set_entry_text(row, next);
        }
    }

    /// Rewrite one entry's text, keeping a scrolled-up viewport anchored to
    /// the same content as the line count changes.
    fn set_entry_text(&mut self, row: usize, text: String) {
        let old_lines = self.entries[row].text.lines().count().max(1);
        self.entries[row].text = text;
        let new_lines = self.entries[row].text.lines().count().max(1);
        if self.scroll > 0 {
            self.scroll = self
                .scroll
                .saturating_add(new_lines.saturating_sub(old_lines));
        }
    }

    /// Mark the start of a task: everything the result block (spec 3.3)
    /// reports is measured against these baselines.
    fn task_begin(&mut self) {
        self.task_started = Some(std::time::Instant::now());
        self.verified = false;
        self.task_base = Some(TaskBase {
            tool_calls: self.tool_calls,
            tokens_in: self.usage.input,
            tokens_out: self.usage.output,
            tokens_reason: self.usage.reasoning,
            cache_read: self.usage.cache_read,
            cost: self.metrics.cost_usd.unwrap_or(0.0),
            has_cost: self.metrics.cost_usd.is_some(),
            flags: self.flags_found,
            errors: self.error_count,
            tool_ms: self.tool_ms_total,
            model_ms: self.model_ms_total,
            files: self.changed_files.len(),
        });
    }

    /// Advance the rotating splash tip (called from the spinner tick).
    fn advance_tip(&mut self) {
        self.tip_index = (self.tip_index + 1) % TIPS.len();
    }

    fn end_turn(&mut self) {
        // Freeze the reasoning cell before the clock is taken, so a turn that
        // produced no activity still reports its full thinking time.
        self.finalize_think();
        if let Some(started) = self.turn_started.take() {
            let wall_ms = started.elapsed().as_millis() as u64;
            self.last_turn_ms = wall_ms;
            self.turn_tokens.push(self.turn_usage.output.max(1));
            if self.turn_tokens.len() > 64 {
                self.turn_tokens.remove(0);
            }
            // Wall time that no tool owned is model time (spec 3.3 split).
            let model_ms = wall_ms as u128 - self.turn_tool_ms.min(wall_ms as u128);
            self.model_ms_total += model_ms;
            self.last_turn_model_ms = model_ms as u64;
        }
        self.last_turn_reasoning = self.turn_usage.reasoning;
        self.turns_total += 1;
        self.turn_usage = Usage::default();
    }

    /// The thinking label for the status bar, splash, and dashboard: the
    /// configured mode, the effective level while auto is driving it, and an
    /// `ignored` marker when the last provider rejected the control.
    fn think_label(&self) -> String {
        let mut label = think_label(self.think, self.thinking_level);
        if self.think_ignored {
            label.push_str(" (ignored)");
        }
        label
    }

    /// Apply a thinking level (Ctrl+T cycle, `[`/`]`, `/think`) to both the
    /// agent and the mirrored UI state.
    fn apply_think(&mut self, agent: &mut Agent, level: ThinkLevel) {
        agent.set_think(level);
        self.think = level;
        self.thinking_level = agent.thinking_level;
        self.think_ignored = false;
        self.status = format!("Think: {}", level.name());
    }

    fn record_error(&mut self, kind: &'static str) {
        self.error_count += 1;
        self.last_error_kind = kind;
    }

    /// The end-of-task summary block (spec 3.3): verification status,
    /// answer, proof, time split, steps, tokens, cache rate, cost, savings.
    fn push_result_block(&mut self) {
        let base = self.task_base.take().unwrap_or_default();
        let wall = self
            .task_started
            .take()
            .map(|started| started.elapsed().as_millis())
            .unwrap_or(0);
        let model = self.model_ms_total.saturating_sub(base.model_ms);
        let tools = self.tool_ms_total.saturating_sub(base.tool_ms);
        let wait = wall.saturating_sub(model + tools);
        let flags_delta = self.flags_found.saturating_sub(base.flags);
        let errors_delta = self.error_count.saturating_sub(base.errors);
        let status = if self.verified {
            "✔ verified"
        } else if errors_delta > 0 {
            "✗ failed"
        } else {
            "⚠ unverified"
        };
        let answer = self
            .entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.speaker, Speaker::Agent))
            .and_then(|entry| entry.text.lines().next())
            .map(|line| clip(line, 70))
            .unwrap_or_else(|| "—".to_string());
        let proof = self
            .flag_log
            .last()
            .map(|(flag, source)| format!("{} ({})", clip(flag, 40), clip(source, 18)))
            .unwrap_or_else(|| "none".into());
        let tokens_in = self.usage.input.saturating_sub(base.tokens_in);
        let tokens_out = self.usage.output.saturating_sub(base.tokens_out);
        let tokens_reason = self.usage.reasoning.saturating_sub(base.tokens_reason);
        let cache_read = self.usage.cache_read.saturating_sub(base.cache_read);
        let hit = hit_percent(&Usage {
            input: tokens_in,
            cache_read,
            ..Usage::default()
        });
        let steps = self.tool_calls.saturating_sub(base.tool_calls);
        let cost = if base.has_cost || self.metrics.cost_usd.is_some() {
            Some(self.metrics.cost_usd.unwrap_or(0.0) - base.cost)
        } else {
            None
        };
        let mut text = format!(
            " {status}\n answer: {answer}\n proof: {proof}\n time {} (model {} · tools {} · wait {})\n steps {steps} · tokens {} ({hit}% cache) · cost {} · saved ~{} tok",
            secs_text(wall),
            secs_text(model),
            secs_text(tools),
            secs_text(wait),
            compact_number(tokens_in + tokens_out + tokens_reason),
            cost.map(|cost| format!("${cost:.5}")).unwrap_or_else(|| "n/a".into()),
            compact_number(cache_read),
        );
        let files: Vec<&str> = self
            .changed_files
            .iter()
            .skip(base.files)
            .map(String::as_str)
            .collect();
        if !files.is_empty() {
            text.push_str(&format!("\n files {} {}", files.len(), files.join(", ")));
        }
        if flags_delta > 0 {
            text.push_str(&format!("\n flags found {flags_delta}"));
        }
        let header = format!("── RESULT ─ {status} ──────────────");
        self.push(Speaker::System, format!("{header}\n{text}"));
    }

    fn transcript_key(&self, left_width: usize, verbose: bool) -> (usize, usize, usize, bool) {
        (
            self.entries.len(),
            self.entries.iter().map(|entry| entry.text.len()).sum(),
            left_width,
            verbose,
        )
    }

    /// Rebuild the wrapped transcript only when something it depends on
    /// changed. Returns true when a rebuild happened (asserted by tests).
    fn ensure_transcript(&mut self, left_width: usize, verbose: bool) -> bool {
        let key = self.transcript_key(left_width, verbose);
        if self.transcript_key == Some(key) {
            return false;
        }
        let mut lines = Vec::new();
        let mut origin = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let wrapped = entry_lines(entry, left_width, verbose);
            let count = wrapped.len();
            lines.extend(wrapped);
            origin.extend(std::iter::repeat_n(index, count));
            lines.push(String::new());
            origin.push(index);
        }
        self.transcript_lines = lines;
        self.line_origin = origin;
        self.transcript_key = Some(key);
        self.transcript_rebuilds += 1;
        true
    }

    /// Scroll by `lines` toward the top, clamped to what the last render
    /// could actually show.
    fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines).min(self.max_scroll);
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    fn scroll_page(&mut self, down: bool) {
        let page = self.page_size.max(1);
        if down {
            self.scroll_down(page);
        } else {
            self.scroll_up(page);
        }
    }

    /// Ctrl+U / Ctrl+D scroll half a screen (spec 1.1), half of what PageUp
    /// and PageDown move.
    fn scroll_half_page(&mut self, down: bool) {
        let half = self.page_size.max(2).div_ceil(2);
        if down {
            self.scroll_down(half);
        } else {
            self.scroll_up(half);
        }
    }

    fn scroll_home(&mut self) {
        self.scroll = self.max_scroll;
    }

    fn scroll_end(&mut self) {
        self.scroll = 0;
        self.new_since_detach = 0;
    }

    fn is_following(&self) -> bool {
        self.scroll == 0
    }

    fn clear_input(&mut self) {
        self.set_input(String::new());
        self.completion = None;
    }

    /// Readline-style Ctrl+W: drop the word before the cursor.
    fn delete_word_before_cursor(&mut self) {
        let head = &self.input[..self.cursor];
        // Trim the word itself, leaving the separator that preceded it, so
        // "hello world" becomes "hello " instead of vanishing entirely.
        let word_start = head.trim_end_matches(|ch: char| !ch.is_whitespace()).len();
        if word_start == head.len() {
            // Only whitespace (or nothing) before the cursor: drop one char.
            self.backspace();
            return;
        }
        self.input.replace_range(word_start..self.cursor, "");
        self.cursor = word_start;
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.input.len() {
            let end = self.input[self.cursor..]
                .chars()
                .next()
                .map(|ch| self.cursor + ch.len_utf8())
                .unwrap_or(self.cursor);
            self.input.replace_range(self.cursor..end, "");
        }
    }

    /// Alt+← / Alt+B: jump to the start of the previous word.
    fn move_word_left(&mut self) {
        let head = &self.input[..self.cursor];
        let chars: Vec<(usize, char)> = head.char_indices().collect();
        let mut index = chars.len();
        while index > 0 && chars[index - 1].1.is_whitespace() {
            index -= 1;
        }
        while index > 0 && !chars[index - 1].1.is_whitespace() {
            index -= 1;
        }
        self.cursor = chars.get(index).map(|(offset, _)| *offset).unwrap_or(0);
    }

    /// Alt+→ / Alt+F: jump to the start of the next word (or the end of the
    /// line when only whitespace remains).
    fn move_word_right(&mut self) {
        let base = self.cursor;
        let chars: Vec<(usize, char)> = self.input[base..].char_indices().collect();
        let mut index = 0;
        while index < chars.len() && !chars[index].1.is_whitespace() {
            index += 1;
        }
        while index < chars.len() && chars[index].1.is_whitespace() {
            index += 1;
        }
        self.cursor = match chars.get(index) {
            Some((offset, _)) => base + offset,
            None => self.input.len(),
        };
    }

    /// Insert whole text (a mention, an expanded paste) at the cursor.
    fn insert_str(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Append to prompt history: consecutive duplicates are dropped and the
    /// list is capped so a long session cannot grow it forever (spec 1.2).
    fn push_history(&mut self, line: String) {
        if self.history.last().map(String::as_str) == Some(line.as_str()) {
            self.history_index = None;
            return;
        }
        self.history.push(line);
        if self.history.len() > 500 {
            self.history.remove(0);
        }
        self.history_index = None;
    }

    /// Clear the visible transcript without touching the session or the
    /// conversation context (spec 1.3).
    fn clear_transcript(&mut self) {
        self.entries.clear();
        self.tool_rows.clear();
        self.tool_cells.clear();
        self.child_cells.clear();
        self.expanded.clear();
        self.groups.clear();
        self.ro_ids.clear();
        self.active_group = None;
        self.think_row = None;
        self.top_entry = None;
        self.line_origin.clear();
        self.tool_timeline.clear();
        self.last_flag.clear();
        self.search_query.clear();
        self.search_matches.clear();
        self.search_position = 0;
        self.transcript_key = None;
        self.renderer.previous.clear();
        self.renderer.full_redraw = true;
        self.scroll_end();
    }

    /// Rows the prompt box occupies: one for a single line, growing with the
    /// draft up to four, then scrolling like the transcript (spec 1.2).
    fn input_rows(&self) -> usize {
        self.input.split('\n').count().clamp(1, 4)
    }

    /// Bracketed paste (spec 1.2): a short paste types through, a long one
    /// becomes an expandable chip so large pastes never replay per key.
    fn accept_paste(&mut self, text: &str) {
        let normalized: String = text.chars().filter(|ch| *ch != '\r').collect();
        let lines = normalized.lines().count().max(1);
        if lines >= 4 || normalized.len() >= 200 {
            self.paste_chip = Some((normalized, lines));
            self.status = format!("[Pasted {lines} lines]  ⏎ insert  ·  Esc discard");
        } else {
            for ch in normalized.chars() {
                self.insert(ch);
            }
            self.palette = self.input.starts_with('/') && !self.input.contains(' ');
            self.selected = 0;
        }
    }

    /// ` · ↓ N new lines  (End to jump)` when the viewport is detached from
    /// the bottom, so the user can tell follow mode is off and press End to
    /// re-engage it (spec 1.1).
    fn follow_marker(&self) -> String {
        if self.is_following() || self.max_scroll == 0 {
            String::new()
        } else {
            format!(" · ↓ {} new lines  (End to jump)", self.new_since_detach)
        }
    }

    /// Insert a transcript row while keeping a scrolled-up viewport anchored.
    fn push_scrolled(&mut self, entry: Entry, added_lines: usize) {
        if self.scroll > 0 {
            self.scroll = self.scroll.saturating_add(added_lines);
            self.new_since_detach += added_lines;
        }
        self.entries.push(entry);
    }

    fn push(&mut self, speaker: Speaker, text: impl Into<String>) {
        let text = text.into();
        // Any plain entry takes over the transcript tail: the next read-only
        // call starts a fresh exploration group instead of joining a stale one.
        self.active_group = None;
        if text.starts_with("Checker VERIFIED") {
            self.verified = true;
        }
        if self.scroll > 0 {
            let added = text.lines().count() + 1;
            self.scroll = self.scroll.saturating_add(added);
            self.new_since_detach += added;
        }
        self.entries.push(Entry { speaker, text });
    }

    fn apply_progress(&mut self, progress: Progress) {
        match progress {
            Progress::ToolBegin { id, title } => {
                self.tool_calls += 1;
                self.turn_steps += 1;
                self.inflight_tools += 1;
                self.max_parallel = self.max_parallel.max(self.inflight_tools);
                self.finalize_think();
                if title.starts_with("Ran shell") {
                    self.shell_calls += 1;
                }
                // A nested delegate child never gets its own row: it renders
                // under the root cell of its delegate chain (spec 3.1).
                if id.contains('/') {
                    let cell = ToolCell {
                        title: title.clone(),
                        running: true,
                        ok: false,
                        output: String::new(),
                        elapsed_ms: 0,
                        exit_code: None,
                        row: usize::MAX,
                    };
                    self.child_cells.insert(id.clone(), cell);
                    let root = id.split('/').next().unwrap_or(&id).to_string();
                    self.refresh_cell(&root);
                    return;
                }
                // Read-only calls collect into one `▸ Explored …` group cell
                // instead of one row per file or search (spec 3.1).
                if title.starts_with("Read ") || title.starts_with("Searched ") {
                    let is_read = title.starts_with("Read ");
                    let row = match self.active_group {
                        Some(row) => row,
                        None => {
                            let row = self.entries.len();
                            let group = GroupCell {
                                inflight: 1,
                                ..GroupCell::default()
                            };
                            let text = group_text(&group);
                            self.push_scrolled(
                                Entry {
                                    speaker: Speaker::Tool,
                                    text,
                                },
                                1,
                            );
                            self.groups.insert(row, group);
                            self.active_group = Some(row);
                            row
                        }
                    };
                    if let Some(group) = self.groups.get_mut(&row) {
                        group.inflight += 1;
                    }
                    self.ro_ids.insert(id.clone(), (row, is_read));
                    // The cell tracks the title for a failure; it has no row
                    // of its own unless the call fails.
                    self.tool_cells.insert(
                        id,
                        ToolCell {
                            title,
                            running: true,
                            ok: false,
                            output: String::new(),
                            elapsed_ms: 0,
                            exit_code: None,
                            row: usize::MAX,
                        },
                    );
                    return;
                }
                self.active_group = None;
                let row = self.entries.len();
                self.push_scrolled(
                    Entry {
                        speaker: Speaker::Tool,
                        text: format!(" ⠋ {title}  …"),
                    },
                    1,
                );
                self.tool_rows.insert(id.clone(), row);
                self.tool_cells.insert(
                    id.clone(),
                    ToolCell {
                        title,
                        running: true,
                        ok: false,
                        output: String::new(),
                        elapsed_ms: 0,
                        exit_code: None,
                        row,
                    },
                );
            }
            Progress::ToolEnd {
                id,
                ok,
                output,
                elapsed_ms,
            } => {
                if !ok {
                    self.tool_failures += 1;
                }
                self.inflight_tools = self.inflight_tools.saturating_sub(1);
                self.turn_tool_ms += elapsed_ms;
                self.tool_ms_total += elapsed_ms;
                // Nested child: update its row under the root cell.
                if id.contains('/') {
                    if let Some(cell) = self.child_cells.get_mut(&id) {
                        cell.running = false;
                        cell.ok = ok;
                        cell.elapsed_ms = elapsed_ms;
                    }
                    let root = id.split('/').next().unwrap_or(&id).to_string();
                    self.refresh_cell(&root);
                    return;
                }
                let exit_code = parse_exit_code(&output);
                let output = cap_output(&output);
                // A grouped read-only call: bump the group's counts; a
                // failure surfaces as its own expanded cell after the group.
                if let Some((group_row, is_read)) = self.ro_ids.remove(&id) {
                    let mut finished = false;
                    let mut text = None;
                    if let Some(group) = self.groups.get_mut(&group_row) {
                        group.inflight = group.inflight.saturating_sub(1);
                        if ok {
                            if is_read {
                                group.files += 1;
                                self.files_total += 1;
                            } else {
                                group.searches += 1;
                                self.searches_total += 1;
                            }
                        }
                        finished = group.inflight == 0;
                        text = Some(group_text(group));
                    }
                    if let Some(text) = text {
                        self.set_entry_text(group_row, text);
                    }
                    if finished && self.active_group == Some(group_row) {
                        self.active_group = None;
                    }
                    let cell = self.tool_cells.remove(&id);
                    if !ok {
                        let title = cell
                            .map(|cell| cell.title)
                            .unwrap_or_else(|| "failed call".into());
                        let row = self.entries.len();
                        let cell = ToolCell {
                            title: title.clone(),
                            running: false,
                            ok: false,
                            output,
                            elapsed_ms,
                            exit_code,
                            row,
                        };
                        // Failures auto-expand (spec 3.1).
                        self.expanded.insert(row);
                        let text = self.cell_text(&id, &cell);
                        self.tool_rows.insert(id.clone(), row);
                        self.tool_cells.insert(id.clone(), cell);
                        let added = text.lines().count().max(1);
                        self.push_scrolled(
                            Entry {
                                speaker: Speaker::Tool,
                                text,
                            },
                            added,
                        );
                        self.push_timeline(&title, false, elapsed_ms);
                    }
                    return;
                }
                if let Some(row) = self.tool_rows.get(&id).copied() {
                    let title = self
                        .tool_cells
                        .get(&id)
                        .map(|cell| cell.title.clone())
                        .unwrap_or_else(|| "tool".into());
                    if title.starts_with("Edited ") {
                        // "Edited <name> <path> …" — keep the path for the
                        // result block's changed-files line.
                        if let Some(path) = title.split_whitespace().nth(2) {
                            if !self.changed_files.iter().any(|file| file == path) {
                                self.changed_files.push(path.to_string());
                            }
                        }
                    }
                    let mut cell = ToolCell {
                        title: title.clone(),
                        running: false,
                        ok,
                        output,
                        elapsed_ms,
                        exit_code,
                        row,
                    };
                    cell.running = false;
                    if !ok {
                        // Failures auto-expand (spec 3.1).
                        self.expanded.insert(row);
                    }
                    let text = self.cell_text(&id, &cell);
                    self.tool_cells.insert(id.clone(), cell);
                    self.set_entry_text(row, text);
                    self.push_timeline(&title, ok, elapsed_ms);
                }
            }
            Progress::Plan(items) => self.plan = items,
            Progress::Tool(message) => {
                self.finalize_think();
                self.push(Speaker::Tool, message);
            }
            Progress::ResetText => {
                self.finalize_think();
                if !self.draft.is_empty() {
                    let completed = std::mem::take(&mut self.draft);
                    self.push(Speaker::Agent, completed);
                }
            }
            Progress::TextDelta(piece) => {
                self.finalize_think();
                self.draft.push_str(&piece);
            }
            Progress::Usage(usage) => {
                self.turn_usage.add(usage);
                self.usage.add(usage);
            }
            Progress::Metrics(metrics) => self.metrics = metrics,
            Progress::FlagFound { flag, source } => {
                self.finalize_think();
                self.flags_found += 1;
                self.last_flag = flag.clone();
                self.flag_log.push((flag.clone(), source.clone()));
                if self.flag_log.len() > 32 {
                    self.flag_log.remove(0);
                }
                self.push(
                    Speaker::System,
                    format!(
                        "🚩 FLAG FOUND  {flag}\n   Clipboard copy attempted · source: {source} · logged in .ctf/flags.log"
                    ),
                );
            }
            Progress::Think {
                from,
                to,
                to_level,
                reason,
            } => {
                self.finalize_think();
                self.thinking_level = to_level;
                self.think_ignored = false;
                self.push(Speaker::System, format!("think: {from} → {to} ({reason})"));
            }
            Progress::ThinkIgnored(note) => {
                self.finalize_think();
                self.think_ignored = true;
                self.push(Speaker::System, format!("think control ignored: {note}"));
            }
        }
    }

    /// Re-render a tool cell's row from its stored cell (child changes, etc).
    fn refresh_cell(&mut self, id: &str) {
        let Some(cell) = self.tool_cells.get(id).cloned() else {
            return;
        };
        if cell.row == usize::MAX {
            return;
        }
        let text = self.cell_text(id, &cell);
        self.set_entry_text(cell.row, text);
    }

    /// One timeline line per finished tool, capped like the session itself.
    fn push_timeline(&mut self, title: &str, ok: bool, elapsed_ms: u128) {
        let icon = if ok { "✓" } else { "✗" };
        self.tool_timeline.push(format!(
            "{icon} {title} · {:.1}s",
            elapsed_ms as f64 / 1000.0
        ));
        if self.tool_timeline.len() > 200 {
            self.tool_timeline.remove(0);
        }
    }

    /// Full text for a tool cell: header, nested child tools, and either the
    /// collapsed preview or the expanded output.
    fn cell_text(&self, id: &str, cell: &ToolCell) -> String {
        let mut out = if cell.running {
            format!(" ⠋ {}  …", cell.title)
        } else {
            let icon = if cell.ok { "✓" } else { "✗" };
            format!(
                " {icon} {}  {:.1}s",
                cell.title,
                cell.elapsed_ms as f64 / 1000.0
            )
        };
        if !id.contains('/') {
            out.push_str(&self.child_lines(id, 1));
        }
        if cell.running {
            return out;
        }
        let expanded = self.expanded.contains(&cell.row);
        if !cell.ok || expanded {
            out.push_str(&expanded_block(&cell.output, cell.exit_code));
        } else {
            out.push_str(&preview_block(&cell.output, 3, 2));
        }
        out
    }

    /// Direct children of `parent` (recursive, depth-capped), one line each.
    fn child_lines(&self, parent: &str, depth: usize) -> String {
        if depth > 4 {
            return String::new();
        }
        let prefix = format!("{parent}/");
        let mut children: Vec<&String> = self
            .child_cells
            .keys()
            .filter_map(|id| {
                id.strip_prefix(&prefix)
                    .and_then(|rest| (!rest.contains('/')).then_some(id))
            })
            .collect();
        if children.is_empty() {
            return String::new();
        }
        children.sort();
        let mut out = String::new();
        let last = children.len() - 1;
        let indent = "   ".repeat(depth);
        for (index, child_id) in children.iter().enumerate() {
            let Some(child) = self.child_cells.get(*child_id) else {
                continue;
            };
            let icon = if child.running {
                "⠋"
            } else if child.ok {
                "✓"
            } else {
                "✗"
            };
            let tail = if child.running {
                "  …".to_string()
            } else {
                format!("  {:.1}s", child.elapsed_ms as f64 / 1000.0)
            };
            let connector = if index == last { "└─" } else { "├─" };
            out.push_str(&format!(
                "\n{indent}{connector} {icon} {}{tail}",
                child.title
            ));
            out.push_str(&self.child_lines(child_id, depth + 1));
        }
        out
    }

    /// Toggle the expanded state of the entry at `row` (Ctrl+O / Enter).
    fn toggle_entry(&mut self, row: usize) {
        let id = self
            .tool_cells
            .iter()
            .find(|(_, cell)| cell.row == row && !cell.running)
            .map(|(id, _)| id.clone());
        let Some(id) = id else {
            return;
        };
        if self.expanded.contains(&row) {
            self.expanded.remove(&row);
        } else {
            self.expanded.insert(row);
        }
        let Some(cell) = self.tool_cells.get(&id).cloned() else {
            return;
        };
        let text = self.cell_text(&id, &cell);
        self.set_entry_text(row, text);
    }

    /// Expand/collapse the entry at the top of the viewport.
    fn toggle_top_entry(&mut self) {
        if let Some(row) = self.top_entry {
            self.toggle_entry(row);
        }
    }

    /// Refresh dashboard data on its own clocks: processes at 10 Hz, git
    /// files at ~1.5 Hz so a large diff cannot stall the frame (spec 3.2).
    fn refresh_dash(&mut self) {
        let now = std::time::Instant::now();
        if self
            .dash_data_at
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(100))
        {
            self.dash_procs = crate::tools::shell::proc_snapshots();
            self.dash_data_at = Some(now);
        }
        if self
            .dash_files_at
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(1500))
        {
            self.dash_files = git_changed_files(&self.root);
            self.dash_files_at = Some(now);
        }
    }

    /// Write `~/.wrosecode/live.json` at most once per second: the attach
    /// dashboard (`wrosecode dashboard`) reads this file, and the write runs
    /// from both the idle loop and the busy-turn ticker so a live session
    /// always looks fresh (spec 3.2).
    fn maybe_write_live(&mut self) {
        if self
            .live_written_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.live_written_at = Some(std::time::Instant::now());
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let path = PathBuf::from(home).join(".wrosecode").join("live.json");
        let stats = self.dash_stats();
        if let Ok(json) = serde_json::to_string_pretty(&stats) {
            let _ = std::fs::write(path, json);
        }
    }

    /// The serializable dashboard snapshot (panels, live.json, /v1/status).
    fn dash_stats(&mut self) -> DashStats {
        self.refresh_dash();
        let elapsed = self.session_started.elapsed();
        let wall_ms = elapsed.as_millis() as u64;
        let model = self.model_ms_total as u64;
        let tools = self.tool_ms_total as u64;
        DashStats {
            schema: "wrosecode/live-v1".into(),
            live_at_ms: unix_ms(),
            session_id: self.session_id.clone(),
            root: self.root.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            mode: self.mode.clone(),
            category: self.category.clone(),
            thinking_level: self.thinking_level,
            think: self.think_label(),
            status: if self.busy { "busy" } else { "ready" }.into(),
            elapsed_s: elapsed.as_secs(),
            budget_usd: self.budget_usd,
            cost_usd: self.metrics.cost_usd,
            usage: self.usage,
            cache_hit_pct: hit_percent(&self.usage),
            tool_calls: self.tool_calls,
            tool_failures: self.tool_failures,
            errors: self.error_count,
            turns: self.turns_total,
            model_ms: model,
            turn_reasoning_tok: if self.turn_started.is_some() {
                self.turn_usage.reasoning
            } else {
                self.last_turn_reasoning
            },
            turn_model_ms: match self.turn_started {
                Some(started) => {
                    (started.elapsed().as_millis() as u64).saturating_sub(self.turn_tool_ms as u64)
                }
                None => self.last_turn_model_ms,
            },
            tool_ms: tools,
            wait_ms: wall_ms.saturating_sub(model + tools),
            max_parallel: self.max_parallel,
            inflight: self.inflight_tools,
            plan: self.plan.clone(),
            flags: self.flag_log.clone(),
            flags_total: self.flags_found,
            files_read: self.files_total,
            searches: self.searches_total,
            shells: self.shell_calls,
            changed_files: self.dash_files.clone(),
            // Newest first: the panel and the selection both index this order.
            procs: self.dash_procs.iter().rev().map(ProcStat::from).collect(),
            turn_tokens: self.turn_tokens.clone(),
            verified: self.verified,
            last_error: self.last_error_kind.to_string(),
        }
    }

    /// Move focus between the eight panels (←/→).
    fn dash_panel(&mut self, delta: i32) {
        self.dash_focus = (self.dash_focus as i32 + delta).rem_euclid(8) as usize;
        self.dash_sel = 0;
        self.dash_detail = None;
    }

    /// Move the selection inside a selectable panel (↑/↓ or j/k).
    fn dash_move(&mut self, delta: i32) {
        if self.dash_detail.is_some() {
            return;
        }
        let count = match self.dash_focus {
            0 => self.dash_procs.len(),
            5 => self.dash_files.len(),
            _ => return,
        };
        if count == 0 {
            return;
        }
        let selected = self.dash_sel as i32 + delta;
        self.dash_sel = selected.clamp(0, count as i32 - 1) as usize;
    }

    /// Open the selected process (full tail) or file (git diff).
    fn dash_open(&mut self) {
        self.refresh_dash();
        if self.dash_detail.is_some() {
            return;
        }
        match self.dash_focus {
            0 => {
                let selected = self.dash_procs.iter().rev().nth(self.dash_sel).cloned();
                if let Some(proc) = selected {
                    let mut body = format!(
                        "pid {}\ncommand {}\ncwd {}\nage {}s\nstatus {}\ncpu {} · rss {}\n\nOUTPUT (live tail)\n",
                        proc.pid,
                        proc.command,
                        proc.cwd,
                        proc.started.elapsed().as_secs(),
                        if proc.running {
                            "running".to_string()
                        } else {
                            format!(
                                "exit {:?}{}",
                                proc.exit_code,
                                if proc.timed_out { " (timed out)" } else { "" }
                            )
                        },
                        proc.cpu_pct
                            .map(|cpu| format!("{cpu:.1}%"))
                            .unwrap_or_else(|| "n/a".into()),
                        proc.rss_kb
                            .map(|kb| format!("{}M", kb / 1024))
                            .unwrap_or_else(|| "n/a".into()),
                    );
                    let tail = if proc.tail.trim().is_empty() {
                        "(no output yet)".to_string()
                    } else {
                        proc.tail.clone()
                    };
                    body.push_str(&tail);
                    self.dash_detail = Some((format!("PROCESS {pid}", pid = proc.pid), body));
                }
            }
            5 => {
                let Some((path, _, _)) = self.dash_files.get(self.dash_sel).cloned() else {
                    return;
                };
                let diff = std::process::Command::new("git")
                    .args(["-C", &self.root, "diff", "HEAD", "--", &path])
                    .output()
                    .map(|output| {
                        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                        if text.trim().is_empty() {
                            text = "(untracked or staged-only file — no unstaged diff)".into();
                        }
                        text
                    })
                    .unwrap_or_else(|error| format!("git diff failed: {error}"));
                self.dash_detail = Some((format!("DIFF {path}"), diff));
            }
            // Any other focused panel opens as plain rows, the same body the
            // attach client shows (spec 3.2 "Enter detail").
            _ => {
                let width = terminal::size().map(|(width, _)| width).unwrap_or(100);
                let stats = self.dash_stats();
                let panel = dashboard_panels(&stats, width.saturating_sub(1) as usize)
                    .into_iter()
                    .nth(self.dash_focus);
                if let Some((title, rows)) = panel {
                    self.dash_detail = Some((title.to_string(), rows.join("\n")));
                }
            }
        }
    }

    /// SIGINT (or SIGTERM) the selected process (Ctrl+C / Ctrl+K).
    fn dash_signal(&mut self, kill: bool) {
        if self.dash_focus != 0 {
            return;
        }
        let selected = self
            .dash_procs
            .iter()
            .rev()
            .nth(self.dash_sel)
            .map(|proc| proc.pid);
        let Some(pid) = selected else {
            self.status = "No process selected".into();
            return;
        };
        match crate::tools::shell::signal(pid, kill) {
            Ok(message) => {
                self.status = message;
                self.dash_procs = crate::tools::shell::proc_snapshots();
                self.dash_data_at = Some(std::time::Instant::now());
            }
            Err(error) => self.status = format!("signal failed: {error}"),
        }
    }

    /// Route one key while the dashboard is open. Always consumed.
    fn dashboard_key(&mut self, key: &KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                if self.dash_detail.is_some() {
                    self.dash_detail = None;
                } else {
                    self.dashboard = false;
                    self.status = "Ready".into();
                }
            }
            KeyCode::Char('d') if control => {
                self.dashboard = false;
                self.status = "Ready".into();
            }
            KeyCode::Char('c') if control => self.dash_signal(false),
            KeyCode::Char('k') if control => self.dash_signal(true),
            KeyCode::Left => self.dash_panel(-1),
            KeyCode::Right => self.dash_panel(1),
            KeyCode::Up => self.dash_move(-1),
            KeyCode::Down => self.dash_move(1),
            KeyCode::Char('j') if !control => self.dash_move(1),
            KeyCode::Char('k') if !control => self.dash_move(-1),
            KeyCode::Enter => self.dash_open(),
            _ => {}
        }
    }

    fn render(&mut self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let (frame, cursor) = self.compose(width, height);
        self.renderer.draw(&frame, cursor, (width, height))
    }

    /// Build one frame for a `width` x `height` terminal. Layout only, no IO,
    /// so tests can assert exact row text at awkward sizes.
    fn compose(&mut self, width: u16, height: u16) -> (Vec<Row>, (u16, u16)) {
        let width_usize = width as usize;
        let height_usize = height as usize;
        let mut frame = vec![Row::new("", Color::Reset); height_usize];
        if height_usize == 0 {
            return (frame, (0, 0));
        }
        let colors = if self.no_color {
            &PLAIN_THEME
        } else {
            theme(self.theme)
        };
        if self.dashboard {
            return self.compose_dashboard(width, height, colors);
        }
        let show_logo = self.entries.len() <= 1 && self.input.is_empty() && !self.busy;
        let show_alert = !self.last_flag.is_empty();
        let mut body_start = 0usize;
        if show_logo {
            let spin = SPINNER[self.spinner % SPINNER.len()];
            let paint_note = match splash::first_paint_ms() {
                Some(ms) => format!(" · paint {ms}ms"),
                None => String::new(),
            };
            // Narrow terminals get a one-line wordmark instead of the block art.
            let (lines, wide) = if width_usize < 80 {
                (
                    vec![format!("WROSECODE v{}", env!("CARGO_PKG_VERSION"))],
                    false,
                )
            } else {
                (LOGO.iter().map(|line| (*line).to_string()).collect(), true)
            };
            for (index, line) in lines.iter().enumerate() {
                if body_start >= height_usize {
                    break;
                }
                let tail = match (wide, index) {
                    (false, 0) => format!(
                        "  {spin}  session {} · [{}] · {}{paint_note}",
                        self.session_id,
                        self.category,
                        speed_tier(self.thinking_level)
                    ),
                    (true, 0) => format!(
                        "  {spin}  session {} · [{}]{paint_note}",
                        self.session_id, self.category
                    ),
                    (true, 2) => format!(
                        "  WROSECODE v{} · {}",
                        env!("CARGO_PKG_VERSION"),
                        speed_tier(self.thinking_level)
                    ),
                    _ => String::new(),
                };
                let text = clip(&format!("{line}{tail}"), width_usize);
                let mut row = Row::new(text, colors.accent);
                row.colors = gradient_colors(&row.text, self.colors);
                frame[body_start] = row;
                body_start += 1;
            }
            // Compact info panel beneath the wordmark: provider/model ·
            // thinking · sandbox · approval · cwd, then mode/harness and the
            // git, MCP, and skill counts.
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        &format!(
                            " {} / {} · think {} · {} · sandbox {} · {} · {}",
                            self.provider,
                            self.model,
                            self.think_label(),
                            level_resources(self.thinking_level),
                            self.sandbox,
                            self.permission,
                            self.root
                        ),
                        width_usize,
                    ),
                    colors.muted,
                );
                body_start += 1;
            }
            if body_start < height_usize {
                let git = match &self.git {
                    Some(branch) => format!(" · git {branch}"),
                    None => String::new(),
                };
                frame[body_start] = Row::new(
                    clip(
                        &format!(
                            " {} · {} · [{}]{} · {} mcp · {} skills",
                            self.mode,
                            self.harness,
                            self.category,
                            git,
                            self.mcp_count,
                            self.skill_count
                        ),
                        width_usize,
                    ),
                    colors.muted,
                );
                body_start += 1;
            }
            // Provider health with the hint to open /providers.
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        &format!(" {} · /providers", self.provider_health),
                        width_usize,
                    ),
                    colors.status,
                );
                body_start += 1;
            }
            // Rotating tips row.
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        &format!(" tip · {}", TIPS[self.tip_index % TIPS.len()]),
                        width_usize,
                    ),
                    colors.accent,
                );
                body_start += 1;
            }
            // Recent sessions with the resume shortcut, then the CTF hint.
            for (name, summary) in &self.recent {
                if body_start >= height_usize {
                    break;
                }
                frame[body_start] = Row::new(
                    clip(&format!("  resume {name}  ·  {summary}"), width_usize),
                    colors.muted,
                );
                body_start += 1;
            }
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        "  ctf  wrosecode ctf <file|dir|url>  ·  or /ctf here",
                        width_usize,
                    ),
                    colors.status,
                );
                body_start += 1;
            }
        } else if body_start < height_usize {
            // The splash collapses into this slim header once the first
            // message is sent: identity plus the facts that still matter.
            frame[body_start] = Row::new(
                clip(
                    &format!(
                        " WROSECODE v{} • {} • {} / {} • {} • {} • [{}] • think {} · {}",
                        env!("CARGO_PKG_VERSION"),
                        self.mode,
                        self.provider,
                        self.model,
                        self.harness,
                        self.permission,
                        self.category,
                        self.think_label(),
                        level_resources(self.thinking_level)
                    ),
                    width_usize,
                ),
                colors.muted,
            );
            body_start += 1;
        }
        if body_start < height_usize {
            frame[body_start] = Row::new("─".repeat(width_usize), colors.muted);
            body_start += 1;
        }
        if show_alert && body_start < height_usize {
            frame[body_start] = Row::new(
                clip(
                    &format!(
                        " 🚩 FLAG ALERT  {}   ·   {} captured   ·   auto-copied, logged to .ctf/flags.log",
                        self.last_flag, self.flags_found
                    ),
                    width_usize,
                ),
                Color::Green,
            );
            body_start += 1;
        }
        // The prompt box grows with the draft (up to four rows) and a large
        // paste parks a chip above it; the body shrinks by exactly that much
        // so the status line, separator and prompt stay glued to the bottom.
        let input_rows = self.input_rows();
        let chip_rows = usize::from(self.paste_chip.is_some());
        let body_end = height_usize.saturating_sub(2 + input_rows + chip_rows);
        let body_height = body_end.saturating_sub(body_start);
        // The clamp keeps a usable left pane but must never exceed the
        // terminal itself: on a 23-column screen the old bounds produced a
        // 24-column pane and pushed rows two cells past the edge.
        let left_width = width_usize
            .saturating_mul(self.pane_percent)
            .checked_div(100)
            .unwrap_or(0)
            .clamp(24, width_usize.saturating_sub(22).max(24))
            .min(width_usize.saturating_sub(1));
        let right_width = width_usize.saturating_sub(left_width + 1);
        let top_height = body_height.saturating_mul(2).checked_div(3).unwrap_or(0);
        let bottom_height = body_height.saturating_sub(top_height);
        // Wrapped rows come from the cache; scrolling reslices instead of
        // re-wrapping, and only the visible window is ever read below.
        let verbose = self.verbosity == "verbose";
        self.ensure_transcript(left_width, verbose);
        let base = self.transcript_lines.len();
        // The live draft only joins the view when the viewport is pinned to
        // the bottom; otherwise its growth would shift a scrolled-up view.
        let draft_lines: Vec<String> = if self.scroll == 0 && !self.draft.is_empty() {
            entry_lines(
                &Entry {
                    speaker: Speaker::Agent,
                    text: self.draft.clone(),
                },
                left_width,
                verbose,
            )
        } else {
            Vec::new()
        };
        let transcript_height = top_height.saturating_sub(1);
        let total_lines = base + draft_lines.len();
        self.max_scroll = total_lines.saturating_sub(transcript_height);
        self.scroll = self.scroll.min(self.max_scroll);
        self.page_size = transcript_height.max(1);
        let transcript_end = total_lines.saturating_sub(self.scroll);
        let transcript_start = transcript_end.saturating_sub(transcript_height);
        let mut visible: Vec<&str> = Vec::with_capacity(transcript_height);
        for index in transcript_start..transcript_end {
            if let Some(line) = self.transcript_lines.get(index) {
                visible.push(line.as_str());
            } else if let Some(line) = draft_lines.get(index - base) {
                visible.push(line.as_str());
            }
        }
        // Remember where the transcript sits so a click (or Ctrl+O) can map a
        // terminal row back to its entry: line `transcript_start` paints on
        // frame row `body_start + 1`.
        self.view = TranscriptView {
            body_start,
            transcript_start,
            height: transcript_height,
        };
        self.top_entry = self.line_origin.get(transcript_start).copied();

        let usage_max = [
            self.usage.input,
            self.usage.output,
            self.usage.reasoning,
            self.usage.cache_read,
            self.usage.cache_write,
        ]
        .into_iter()
        .max()
        .unwrap_or(1)
        .max(1);
        let turn_note = if self.last_turn_ms > 0 {
            format!(" · {}", format_duration_ms(self.last_turn_ms))
        } else {
            String::new()
        };
        let mut dashboard: Vec<String> = vec![
            " TOKEN DASHBOARD".to_string(),
            metric_bar("input", self.usage.input, usage_max, right_width),
            metric_bar("output", self.usage.output, usage_max, right_width),
            metric_bar("reason", self.usage.reasoning, usage_max, right_width),
            metric_bar("cache R", self.usage.cache_read, usage_max, right_width),
            metric_bar("cache W", self.usage.cache_write, usage_max, right_width),
            format!(" model  {}", self.model),
            format!(
                " cost   {}",
                self.metrics
                    .cost_usd
                    .map(|cost| format!("${cost:.5}"))
                    .unwrap_or_else(|| "n/a".into())
            ),
            if self.budget_usd > 0.0 {
                let percent = self.metrics.cost_usd.unwrap_or(0.0) * 100.0 / self.budget_usd;
                if percent >= 80.0 {
                    format!(" WARN budget {percent:.0}% of ${:.2}", self.budget_usd)
                } else {
                    format!(" budget {percent:.0}% of ${:.2}", self.budget_usd)
                }
            } else {
                " budget unlimited".into()
            },
            format!(
                " p50/p95/p99 {}/{}/{}ms",
                self.metrics.latency_p50_ms,
                self.metrics.latency_p95_ms,
                self.metrics.latency_p99_ms
            ),
            format!(
                " lat  {}",
                sparkline(
                    &self
                        .metrics
                        .latency_histogram
                        .iter()
                        .map(|bucket| bucket.count)
                        .collect::<Vec<u64>>(),
                    right_width.saturating_sub(6).max(4)
                )
            ),
        ];
        if self.metrics.rate_limits > 0 {
            dashboard.push(format!(" WARN 429s {}", self.metrics.rate_limits));
        }
        if self.metrics.by_model.len() > 1 {
            for (name, model) in self.metrics.by_model.iter().take(4) {
                dashboard.push(format!(
                    "  {} {} tok ${}",
                    clip(name, right_width.saturating_sub(14).max(6)),
                    compact_number(model.usage.input + model.usage.output),
                    model
                        .cost_usd
                        .map(|cost| format!("{cost:.5}"))
                        .unwrap_or_else(|| "?".into())
                ));
            }
        }
        dashboard.extend_from_slice(&[
            format!(" cache  {}%", cache_percent(self.usage)),
            format!(" flags  {}", self.flags_found),
            format!(
                " turn   {}{}",
                sparkline(
                    &self.turn_tokens,
                    right_width.saturating_sub(8 + turn_note.len()).max(4)
                ),
                turn_note
            ),
            format!(
                " errors {}{}",
                self.error_count,
                if self.error_count > 0 {
                    format!(" · last {}", self.last_error_kind)
                } else {
                    String::new()
                }
            ),
            format!(
                " tier   {} · {} workers",
                speed_tier(self.thinking_level),
                level_workers(self.thinking_level)
            ),
        ]);
        for row in 0..top_height {
            let left = if row == 0 {
                format!(
                    " TRANSCRIPT  {} {}{}",
                    self.entries.len(),
                    if self.entries.len() == 1 {
                        "entry"
                    } else {
                        "entries"
                    },
                    self.follow_marker()
                )
            } else {
                visible
                    .get(row - 1)
                    .map(|line| (*line).to_string())
                    .unwrap_or_default()
            };
            let right = dashboard.get(row).cloned().unwrap_or_default();
            frame[body_start + row] = Row::new(
                join_panes(&left, &right, left_width, right_width),
                colors.text,
            );
        }
        let active: Vec<String> = self
            .tool_rows
            .values()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| {
                let filled = self.spinner % 6 + 1;
                format!(
                    " ├─ [{}{}] {}",
                    "█".repeat(filled),
                    "░".repeat(7 - filled),
                    entry.text.trim()
                )
            })
            .collect();
        for row in 0..bottom_height {
            let left = if row == 0 {
                format!(" SUBAGENT TREE  {} active", active.len())
            } else if row == 1 {
                format!(
                    " root [{}] think {} · {}",
                    self.category,
                    self.think_label(),
                    level_resources(self.thinking_level)
                )
            } else {
                active.get(row - 2).cloned().unwrap_or_default()
            };
            let right = if row == 0 {
                format!(" TOOL TIMELINE  {} calls", self.tool_calls)
            } else {
                let start = self
                    .tool_timeline
                    .len()
                    .saturating_sub(bottom_height.saturating_sub(1));
                self.tool_timeline
                    .get(start + row - 1)
                    .cloned()
                    .unwrap_or_default()
            };
            let target = body_start + top_height + row;
            if target < body_end {
                frame[target] = Row::new(
                    join_panes(&left, &right, left_width, right_width),
                    colors.muted,
                );
            }
        }
        let choices: Vec<String> = if self.picker.is_some() {
            picker_view(self)
        } else if self.palette {
            commands::filtered(&self.input, &self.user_commands)
                .iter()
                .map(|command| format!("{}  {}", command.name, command.description))
                .collect()
        } else {
            Vec::new()
        };
        if self.palette || self.picker.is_some() {
            let title = self
                .picker
                .as_ref()
                .map(|(title, _)| title.as_str())
                .unwrap_or("Commands");
            let visible = choices.len().min(body_height.saturating_sub(1));
            let top = body_end.saturating_sub(visible + 1);
            let heading = if self.picker_all.is_some() && !self.picker_query.is_empty() {
                format!(
                    " {title}  ({})  filter: {}",
                    choices.len(),
                    self.picker_query
                )
            } else {
                format!(" {title}  ({})", choices.len())
            };
            if top < body_end {
                frame[top] = Row {
                    text: clip(&heading, width_usize),
                    fg: colors.accent,
                    bg: Color::Black,
                    colors: None,
                };
            }
            let offset = self.selected.saturating_sub(visible.saturating_sub(1));
            for (index, choice) in choices.iter().skip(offset).take(visible).enumerate() {
                let selected = offset + index == self.selected;
                frame[top + 1 + index] = Row {
                    text: clip(
                        &format!(" {:width$}", choice, width = width_usize.saturating_sub(1)),
                        width_usize,
                    ),
                    fg: if selected { Color::Black } else { Color::Grey },
                    bg: if selected {
                        colors.accent
                    } else {
                        Color::Black
                    },
                    colors: None,
                };
            }
        }
        let status_row = body_end;
        let spinner = SPINNER;
        let queue_note = if self.pending.is_empty() {
            String::new()
        } else {
            format!(" · queued {}", self.pending.len())
        };
        let status_text = if self.busy {
            format!(
                " {} {}{} · {} · think:{} · tok {}/{} · cache {}% · {}s · step {} · {}/{} · {} · {}w · cost {} · timeout {}s · tools {}:{}",
                spinner[self.spinner % spinner.len()],
                self.status,
                queue_note,
                self.model,
                self.think_label(),
                self.usage.input,
                self.usage.output,
                cache_percent(self.usage),
                self.session_started.elapsed().as_secs(),
                self.turn_steps,
                self.sandbox,
                self.permission,
                speed_tier(self.thinking_level),
                level_workers(self.thinking_level),
                self.metrics
                    .cost_usd
                    .map(|cost| format!("${cost:.4}"))
                    .unwrap_or_else(|| "n/a".into()),
                self.timeout_seconds,
                self.tool_calls,
                self.tool_rows.len(),
            )
        } else {
            format!(
                " {}{} · {} · think:{} · tok {}/{} · cache {}% · {}s · step {} · {}/{} · {} · {}w · cost {} · tools {}:{} · errors {} · flags {}",
                self.status,
                queue_note,
                self.model,
                self.think_label(),
                self.usage.input,
                self.usage.output,
                cache_percent(self.usage),
                self.session_started.elapsed().as_secs(),
                self.turn_steps,
                self.sandbox,
                self.permission,
                speed_tier(self.thinking_level),
                level_workers(self.thinking_level),
                self.metrics.cost_usd.map(|cost| format!("${cost:.4}")).unwrap_or_else(|| "n/a".into()),
                self.tool_calls,
                self.tool_rows.len(),
                self.tool_failures,
                self.flags_found,
            )
        };
        if status_row < height_usize {
            frame[status_row] = Row::new(clip(&status_text, width_usize), colors.status);
        }
        let separator_row = status_row.saturating_add(1);
        if separator_row < height_usize {
            frame[separator_row] = Row::new("─".repeat(width_usize), colors.muted);
        }
        if let Some((_, pasted_lines)) = &self.paste_chip {
            let chip_row = separator_row.saturating_add(1);
            if chip_row < height_usize {
                frame[chip_row] = Row::new(
                    clip(
                        &format!(" [Pasted {pasted_lines} lines]  ⏎ insert  ·  Esc discard"),
                        width_usize,
                    ),
                    colors.accent,
                );
            }
        }
        // The prompt grows with the draft: up to `input_rows` rows are shown,
        // the window follows the caret so a long draft scrolls inside its box,
        // and only the first line of the draft keeps the "> " marker.
        let input_top = height_usize.saturating_sub(input_rows);
        let input_width = width_usize.saturating_sub(3);
        let input_color = input_syntax_color(&self.input, colors);
        let draft_lines: Vec<&str> = self.input.split('\n').collect();
        let caret_line = self.input[..self.cursor].matches('\n').count();
        let skip = caret_line.saturating_sub(input_rows.saturating_sub(1));
        let cursor_in_line = self.input[..self.cursor]
            .rsplit('\n')
            .next()
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0);
        let mut caret_x = 2usize;
        for (offset, line) in draft_lines.iter().enumerate().skip(skip).take(input_rows) {
            let row_index = input_top + (offset - skip);
            if row_index >= height_usize {
                break;
            }
            let prefix = if offset == skip {
                if skip == 0 {
                    "> "
                } else {
                    "↳ "
                }
            } else {
                "  "
            };
            let line_chars: Vec<char> = line.chars().collect();
            let visible: String = if offset == caret_line {
                let start = cursor_in_line.saturating_sub(input_width.saturating_sub(1));
                caret_x = 2 + cursor_in_line.saturating_sub(start);
                if self.mask_input {
                    line_chars
                        .iter()
                        .skip(start)
                        .take(input_width)
                        .map(|_| '•')
                        .collect()
                } else {
                    line_chars.iter().skip(start).take(input_width).collect()
                }
            } else if self.mask_input {
                line_chars.iter().take(input_width).map(|_| '•').collect()
            } else {
                line_chars.iter().take(input_width).collect()
            };
            frame[row_index] = Row::new(
                clip(&format!("{prefix}{visible}"), width_usize),
                input_color,
            );
        }
        for row in &mut frame {
            if row.bg == Color::Reset {
                row.bg = colors.background;
            }
        }
        let x = caret_x.min(width_usize.saturating_sub(1)) as u16;
        let y = input_top
            .saturating_add(caret_line.saturating_sub(skip))
            .min(height_usize.saturating_sub(1)) as u16;
        (frame, (x, y))
    }

    /// The eight-panel grid (spec 3.2): header, two columns of panels, and a
    /// key-hint footer; a detail view takes the whole body when one is open.
    fn compose_dashboard(
        &mut self,
        width: u16,
        height: u16,
        colors: &'static Theme,
    ) -> (Vec<Row>, (u16, u16)) {
        let stats = self.dash_stats();
        compose_grid(
            &stats,
            width,
            height,
            colors,
            self.dash_focus,
            self.dash_sel,
            self.dash_detail.clone(),
        )
    }

    fn insert(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }
    fn backspace(&mut self) {
        if let Some((index, _)) = self.input[..self.cursor].char_indices().last() {
            self.input.remove(index);
            self.cursor = index;
        }
    }
    fn move_left(&mut self) {
        if let Some((index, _)) = self.input[..self.cursor].char_indices().last() {
            self.cursor = index;
        }
    }
    fn move_right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor += self.input[self.cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0);
        }
    }
    fn set_input(&mut self, text: String) {
        self.input = text;
        self.cursor = self.input.len();
    }
    fn take_input(&mut self) -> String {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_index = None;
        line.trim().to_string()
    }
}

fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// One transcript entry as rendered rows: label on the first line, indented
/// continuation lines after it, clipped to the transcript pane.
fn entry_lines(entry: &Entry, left_width: usize, verbose: bool) -> Vec<String> {
    let label = match entry.speaker {
        Speaker::User => "YOU",
        Speaker::Agent => "WROSE",
        Speaker::Tool => "TOOL",
        Speaker::System => "INFO",
    };
    let available = left_width.saturating_sub(9).max(1);
    let mut lines = Vec::new();
    for (part_index, line) in entry.text.lines().enumerate() {
        for (wrap_index, part) in if verbose {
            wrap(line, available)
        } else {
            vec![clip(line, available)]
        }
        .into_iter()
        .enumerate()
        {
            let prefix = if part_index == 0 && wrap_index == 0 {
                format!(" {label:<5} ")
            } else {
                "       ".into()
            };
            lines.push(clip(&format!("{prefix}{part}"), left_width));
        }
    }
    lines
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    name: &'static str,
    text: Color,
    muted: Color,
    accent: Color,
    status: Color,
    background: Color,
}

/// Every selectable palette. `/theme` (picker or `<name>`) and `[ui] theme` in
/// config.toml all index this one list, so a theme cannot exist in one place
/// and be missing from another.
const THEMES: &[Theme] = &[
    Theme {
        name: "dark",
        text: Color::White,
        muted: Color::DarkGrey,
        accent: Color::Cyan,
        status: Color::Yellow,
        background: Color::Black,
    },
    Theme {
        name: "light",
        text: Color::Black,
        muted: Color::DarkGrey,
        accent: Color::Blue,
        status: Color::DarkYellow,
        background: Color::White,
    },
    Theme {
        name: "solarized",
        text: Color::Grey,
        muted: Color::DarkCyan,
        accent: Color::Cyan,
        status: Color::DarkYellow,
        background: Color::Rgb { r: 0, g: 43, b: 54 },
    },
    Theme {
        name: "dracula",
        text: Color::White,
        muted: Color::DarkMagenta,
        accent: Color::Magenta,
        status: Color::Cyan,
        background: Color::Rgb {
            r: 40,
            g: 42,
            b: 54,
        },
    },
    Theme {
        name: "nord",
        text: Color::Grey,
        muted: Color::DarkBlue,
        accent: Color::Blue,
        status: Color::Cyan,
        background: Color::Rgb {
            r: 46,
            g: 52,
            b: 64,
        },
    },
    // The NO_COLOR-friendly palette: named colours only, no RGB, so the whole
    // frame stays inside the 16-colour baseline.
    Theme {
        name: "mono",
        text: Color::White,
        muted: Color::DarkGrey,
        accent: Color::Grey,
        status: Color::White,
        background: Color::Black,
    },
];

/// User palettes from `~/.wrosecode/themes/*.toml`, loaded once at startup.
/// `&'static Theme` because `theme()` hands out borrowed palettes on the hot
/// render path.
static USER_THEMES: std::sync::RwLock<Vec<&'static Theme>> = std::sync::RwLock::new(Vec::new());

/// Built-in plus user palette count — what `/theme <name>` indices wrap over.
fn theme_total() -> usize {
    THEMES.len() + USER_THEMES.read().map(|extra| extra.len()).unwrap_or(0)
}

fn theme(index: usize) -> &'static Theme {
    // The index is a live cursor (`/theme` moves it), so it wraps rather than
    // panicking when a palette is removed.
    let index = index % theme_total();
    if index < THEMES.len() {
        return &THEMES[index];
    }
    USER_THEMES
        .read()
        .ok()
        .and_then(|extra| extra.get(index - THEMES.len()).copied())
        .unwrap_or(&THEMES[0])
}

/// Config spelling to palette index. `"wrose-dark"` is the historical name of
/// the default; unknown names fall back to it instead of erroring.
fn theme_index(name: &str) -> usize {
    theme_position(name).unwrap_or(0)
}

/// Like [`theme_index`] but reports whether the name is real, so `/theme
/// bogus` can say so instead of silently going dark.
fn theme_position(name: &str) -> Option<usize> {
    let name = name.trim().to_ascii_lowercase();
    if let Some(index) = THEMES
        .iter()
        .position(|theme| theme.name == name || (theme.name == "dark" && name == "wrose-dark"))
    {
        return Some(index);
    }
    let extra = USER_THEMES.read().ok()?;
    extra
        .iter()
        .position(|theme| theme.name == name)
        .map(|index| THEMES.len() + index)
}

/// Startup timing for `/debug`: where the first frame landed against the
/// splash budget, or why there is no number to show.
fn first_paint_line() -> String {
    match splash::first_paint_ms() {
        Some(ms) => format!(
            "{ms}ms (budget {}ms, {})",
            splash::FIRST_PAINT_BUDGET_MS,
            if splash::within_budget() {
                "ok"
            } else {
                "over"
            }
        ),
        None => "not painted (headless)".into(),
    }
}

fn theme_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = THEMES.iter().map(|theme| theme.name).collect();
    if let Ok(extra) = USER_THEMES.read() {
        names.extend(extra.iter().map(|theme| theme.name));
    }
    names
}

/// Flat theme file shape: five hex colours plus an optional name override.
/// Anything missing or unparsable skips the file — a broken theme must never
/// block startup.
#[derive(serde::Deserialize)]
struct ThemeFile {
    #[serde(default)]
    name: Option<String>,
    text: String,
    #[serde(default)]
    muted: Option<String>,
    accent: String,
    status: String,
    #[serde(default)]
    background: Option<String>,
}

/// `#rrggbb` (or `rrggbb`) to a truecolor entry.
fn hex_colour(value: &str) -> Option<Color> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(Color::Rgb {
        r: u8::from_str_radix(&hex[0..2], 16).ok()?,
        g: u8::from_str_radix(&hex[2..4], 16).ok()?,
        b: u8::from_str_radix(&hex[4..6], 16).ok()?,
    })
}

/// Parse one theme file. `fallback_name` is the file stem, used when the file
/// carries no `name` key. Names are lowercased and may not shadow a
/// built-in (that palette would be unreachable in `theme_position`).
fn parse_theme(fallback_name: &str, source: &str) -> Option<Theme> {
    let file: ThemeFile = toml::from_str(source).ok()?;
    let text = hex_colour(&file.text)?;
    let accent = hex_colour(&file.accent)?;
    let status = hex_colour(&file.status)?;
    let muted = match &file.muted {
        Some(value) => hex_colour(value)?,
        None => text,
    };
    let background = match &file.background {
        Some(value) => hex_colour(value)?,
        None => Color::Black,
    };
    let name = file
        .name
        .map(|name| name.trim().to_ascii_lowercase())
        .unwrap_or_else(|| fallback_name.trim().to_ascii_lowercase());
    if name.is_empty() || THEMES.iter().any(|theme| theme.name == name) {
        return None;
    }
    let name: &'static str = Box::leak(name.into_boxed_str());
    Some(Theme {
        name,
        text,
        muted,
        accent,
        status,
        background,
    })
}

/// Read every `*.toml` in `dir` into leaked palettes. Missing directory or
/// unreadable files yield nothing.
fn load_theme_files(dir: &std::path::Path) -> Vec<&'static Theme> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut loaded = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(theme) = parse_theme(stem, &source) {
            loaded.push(&*Box::leak(Box::new(theme)));
        }
    }
    loaded
}

/// Load `~/.wrosecode/themes/*.toml` into the shared registry. Called once
/// from `tui::run` before `Ui::new` so `config.toml` can already name a user
/// theme. A broken themes directory is silent, not fatal.
fn register_user_themes() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = PathBuf::from(home).join(".wrosecode").join("themes");
    let loaded = load_theme_files(&dir);
    if let Ok(mut extra) = USER_THEMES.write() {
        extra.clear();
        extra.extend(loaded);
    }
}

fn join_panes(left: &str, right: &str, left_width: usize, right_width: usize) -> String {
    format!(
        "{:<left_width$}│{:<right_width$}",
        clip(left, left_width),
        clip(right, right_width)
    )
}

fn metric_bar(label: &str, value: u64, maximum: u64, width: usize) -> String {
    let bar_width = width.saturating_sub(19).min(18);
    let filled = value
        .saturating_mul(bar_width as u64)
        .checked_div(maximum)
        .unwrap_or(0) as usize;
    format!(
        " {label:<7} {:>7} {}{}",
        compact_number(value),
        "█".repeat(filled),
        "░".repeat(bar_width.saturating_sub(filled))
    )
}

/// `8400` -> `8.4s`, `320` -> `320ms`, `125_000` -> `2m5s`.
fn format_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        let seconds = ms / 1_000;
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn compact_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn input_syntax_color(input: &str, colors: &Theme) -> Color {
    let lower = input.trim_start().to_ascii_lowercase();
    if lower.starts_with("python") || lower.contains("```python") {
        Color::Green
    } else if lower.contains(" asm") || lower.contains("objdump") || lower.contains("gdb") {
        Color::Magenta
    } else if lower.starts_with('!') || lower.starts_with("$ ") || lower.contains("cargo ") {
        Color::Yellow
    } else {
        colors.text
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn level_resources(level: u8) -> &'static str {
    match level {
        0..=3 => "1 agent / fast",
        4..=7 => "up to 3 agents",
        8..=12 => "up to 5 + checker",
        _ => "up to 20 / race",
    }
}

fn level_workers(level: u8) -> u8 {
    match level {
        0..=3 => 1,
        4..=7 => 3,
        8..=12 => 5,
        _ => 10,
    }
}

fn speed_tier(level: u8) -> &'static str {
    match level {
        0..=3 => "TURBO",
        4..=7 => "FAST",
        8..=12 => "SMART",
        _ => "DEEP",
    }
}

fn sparkline(values: &[u64], width: usize) -> String {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() || width == 0 {
        return "·".to_string();
    }
    let visible = values.len().min(width);
    let start = values.len() - visible;
    let max = values[start..].iter().copied().max().unwrap_or(1).max(1);
    values[start..]
        .iter()
        .map(|value| GLYPHS[(*value as usize * 7 / max as usize).min(7)])
        .collect()
}

fn classify_error(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("429")
        || lower.contains("quota")
    {
        "rate_limit"
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timeout"
    } else if lower.contains("permission") || lower.contains("denied") {
        "permission"
    } else if lower.contains("json") || lower.contains("parse") || lower.contains("decode") {
        "parse"
    } else if lower.contains("connect") || lower.contains("dns") || lower.contains("tls") {
        "network"
    } else if lower.contains("memory") || lower.contains("allocation") {
        "oom"
    } else {
        "runtime"
    }
}

fn error_suggestion(kind: &str) -> &'static str {
    match kind {
        "rate_limit" => "Back off and retry; lower the thinking level with `[` or switch provider with /models.",
        "timeout" => "Raise [agent] shell_timeout_seconds in config.toml, or split the command into shorter steps.",
        "permission" => "Run with `--permission yolo` for full auto-approval, or allow this path in config.toml.",
        "parse" => "The provider returned malformed JSON; retry, or switch model with /models.",
        "network" => "Check connectivity and API keys with /providers, then retry the turn.",
        "oom" => "Run /clear to drop the transcript and lower the thinking level with `[`.",
        _ => "Run /debug for a stack trace, then /providers to re-test the active provider.",
    }
}

fn jump_to_search(ui: &mut Ui) {
    if let Some(index) = ui.search_matches.get(ui.search_position).copied() {
        ui.scroll = ui.entries.len().saturating_sub(index + 1);
        ui.status = format!(
            "Search '{}' · {}/{}",
            ui.search_query,
            ui.search_position + 1,
            ui.search_matches.len()
        );
    } else {
        ui.status = format!("No matches for '{}'", ui.search_query);
    }
}

/// Every candidate for the token at the cursor: slash commands, tool names and
/// filesystem paths. Order matters — an explicit `/command` never becomes a path
/// when a real command matches.
fn completion_matches(agent: &Agent, token: &str) -> Vec<String> {
    completion_matches_at(&agent.config.root, &agent.tools.schemas(), token)
}

/// Core completion logic split from [`Agent`] so it can be unit tested against a
/// temporary directory and a hand-written tool schema list.
pub fn completion_matches_at(
    root: &std::path::Path,
    schemas: &[serde_json::Value],
    token: &str,
) -> Vec<String> {
    if token.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    if let Some(command) = token.strip_prefix('/') {
        if !command.contains('/') {
            let user = crate::markdown::discover_commands(root);
            for spec in commands::filtered(command, &user) {
                matches.push(spec.name);
            }
            if !matches.is_empty() {
                return matches;
            }
        }
    }
    if !token.contains('/') {
        let needle = token.trim_start_matches('/');
        let mut tools: Vec<String> = schemas
            .iter()
            .filter_map(|schema| schema["name"].as_str().map(str::to_string))
            .filter(|name| name.starts_with(needle))
            .collect();
        tools.sort();
        matches.extend(tools);
    }
    let token_path = std::path::Path::new(token);
    let parent = token_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let prefix = token_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let directory = root.join(parent);
    if let Ok(entries) = std::fs::read_dir(directory) {
        let mut paths: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.starts_with(prefix).then(|| {
                    let mut value = parent.join(name).to_string_lossy().to_string();
                    if entry.path().is_dir() {
                        value.push('/');
                    }
                    value
                })
            })
            .collect();
        paths.sort();
        matches.extend(paths);
    }
    matches.dedup();
    matches.truncate(200);
    matches
}

/// Tab completion. Repeated Tab cycles through the candidates for the same token
/// instead of jumping to the single first match.
fn complete_input(ui: &mut Ui, agent: &Agent) {
    let start = ui.input[..ui.cursor]
        .rfind(char::is_whitespace)
        .map(|index| index + 1)
        .unwrap_or(0);
    let token = ui.input[start..ui.cursor].to_string();

    // Stay inside the previous candidate list while the token still equals the
    // entry we just inserted, so Tab cycles instead of recomputing from scratch.
    let cycled = ui.completion.as_ref().and_then(|state| {
        (state.token_start == start
            && state.matches.get(state.index).map(String::as_str) == Some(token.as_str()))
        .then(|| {
            (
                state.matches.clone(),
                (state.index + 1) % state.matches.len(),
            )
        })
    });

    let (matches, index) = match cycled {
        Some((matches, index)) => (matches, index),
        None => {
            let matches = completion_matches(agent, &token);
            if matches.is_empty() {
                ui.completion = None;
                ui.status = "No completion for that token".into();
                return;
            }
            (matches, 0)
        }
    };
    let completion = matches[index].clone();
    ui.input.replace_range(start..ui.cursor, &completion);
    ui.cursor = start + completion.len();
    ui.status = if matches.len() == 1 {
        format!("Completed {}", completion)
    } else {
        format!("{}/{} completions · Tab cycles", index + 1, matches.len())
    };
    ui.completion = Some(CompletionState {
        token_start: start,
        matches,
        index,
    });
}

fn cache_percent(usage: Usage) -> u64 {
    usage
        .cache_read
        .saturating_mul(100)
        .checked_div(usage.input)
        .unwrap_or(0)
}

/// Output lines for a cell preview: ANSI stripped, blank lines dropped.
fn clean_output_lines(output: &str) -> Vec<String> {
    // Stripping only the ESC byte keeps the scan byte-wise and cheap; the
    // remaining CSI bytes never start a line, so previews stay readable.
    output
        .replace('\u{1b}', "")
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `exit=N` from a shell result's first line, when present.
fn parse_exit_code(output: &str) -> Option<i32> {
    output
        .lines()
        .next()?
        .strip_prefix("exit=")?
        .trim()
        .parse::<i32>()
        .ok()
}

/// Keep a single tool output bounded so a session cannot grow without limit.
fn cap_output(output: &str) -> String {
    const CAP: usize = 256 * 1024;
    if output.len() <= CAP {
        return output.to_string();
    }
    let mut cut = CAP;
    while cut > 0 && !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… [output truncated at {} KB]",
        &output[..cut],
        CAP / 1024
    )
}

/// Collapsed cell body: first `head` lines, a middle marker, last `tail`.
fn preview_block(output: &str, head: usize, tail: usize) -> String {
    let lines = clean_output_lines(output);
    let mut block = String::new();
    if lines.is_empty() {
        return block;
    }
    if lines.len() <= head + tail {
        let last = lines.len() - 1;
        for (index, line) in lines.iter().enumerate() {
            let connector = if index == last { '└' } else { '├' };
            block.push_str(&format!("\n   {connector} {line}"));
        }
        return block;
    }
    for line in lines.iter().take(head) {
        block.push_str(&format!("\n   ├ {line}"));
    }
    block.push_str(&format!("\n   ⋮ {} more lines", lines.len() - head - tail));
    let start = lines.len() - tail;
    for (index, line) in lines[start..].iter().enumerate() {
        let connector = if index + 1 == tail { '└' } else { '├' };
        block.push_str(&format!("\n   {connector} {line}"));
    }
    block
}

/// Expanded cell body: exit code up front, then a bounded head/tail slice so
/// one huge result cannot flood the transcript.
fn expanded_block(output: &str, exit_code: Option<i32>) -> String {
    const MAX: usize = 120;
    const HEAD: usize = 80;
    let mut block = String::new();
    if let Some(code) = exit_code.filter(|code| *code != 0) {
        block.push_str(&format!("\n   └ exit {code}"));
    }
    let lines = clean_output_lines(output);
    if lines.is_empty() {
        return block;
    }
    if lines.len() <= MAX {
        let last = lines.len() - 1;
        for (index, line) in lines.iter().enumerate() {
            let connector = if index == last { '└' } else { '├' };
            block.push_str(&format!("\n   {connector} {line}"));
        }
        return block;
    }
    let tail = MAX - HEAD;
    for line in lines[..HEAD].iter() {
        block.push_str(&format!("\n   ├ {line}"));
    }
    block.push_str(&format!("\n   ⋮ {} more lines", lines.len() - MAX));
    for (index, line) in lines[lines.len() - tail..].iter().enumerate() {
        let connector = if index + 1 == tail { '└' } else { '├' };
        block.push_str(&format!("\n   {connector} {line}"));
    }
    block
}

/// The `▸ Explored …` group line: counts grow as calls land, the spinner
/// spins while any call is still in flight.
fn group_text(cell: &GroupCell) -> String {
    let counts = format!(
        "{} file{} · {} search{}",
        cell.files,
        if cell.files == 1 { "" } else { "s" },
        cell.searches,
        if cell.searches == 1 { "" } else { "es" }
    );
    if cell.inflight > 0 {
        format!("⠋ ▸ Explored  {counts}  …")
    } else {
        format!("▸ Explored  {counts}")
    }
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Cache hit rate: reads served from cache over everything read.
fn hit_percent(usage: &Usage) -> u64 {
    let total = usage.cache_read + usage.input;
    usage
        .cache_read
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
}

fn secs_text(ms: u128) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// A `▓▓▓░░░` bar for the budget gauge.
fn gauge(ratio: f64, width: usize) -> String {
    let width = width.max(1);
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{}", "▓".repeat(filled), "░".repeat(width - filled))
}

/// Split `height` across fractional bands, keeping the sum exact.
fn bands(height: usize, fractions: &[f32]) -> Vec<usize> {
    let total: f32 = fractions.iter().sum::<f32>().max(1.0);
    let mut out: Vec<usize> = fractions
        .iter()
        .map(|fraction| (height as f32 * fraction / total).floor() as usize)
        .collect();
    let sum: usize = out.iter().sum();
    if let Some(last) = out.last_mut().filter(|_| sum < height) {
        *last += height - sum;
    }
    out
}

/// Working-tree churn for the FILES panel: numstat against HEAD plus
/// untracked files (listed with `-1` added so they still show up).
pub(crate) fn git_changed_files(root: &str) -> Vec<(String, i64, i64)> {
    let mut out = Vec::new();
    if let Ok(output) = std::process::Command::new("git")
        .args(["-C", root, "diff", "--numstat", "HEAD"])
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.split('\t');
            let (Some(added), Some(removed), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            out.push((
                path.to_string(),
                added.parse::<i64>().unwrap_or(0),
                removed.parse::<i64>().unwrap_or(0),
            ));
        }
    }
    if let Ok(output) = std::process::Command::new("git")
        .args(["-C", root, "status", "--porcelain"])
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some(path) = line.strip_prefix("?? ") else {
                continue;
            };
            if !out.iter().any(|(known, _, _)| known == path) {
                out.push((path.to_string(), -1, 0));
            }
        }
    }
    out.sort_by_key(|row| std::cmp::Reverse(row.1.abs() + row.2.abs()));
    out.truncate(50);
    out
}

/// The eight panels as plain text rows, shared by the grid dashboard,
/// `/stats`, and the `wrosecode dashboard` / `exec` CLI (spec 3.2).
pub(crate) fn dashboard_panels(
    stats: &DashStats,
    width: usize,
) -> Vec<(&'static str, Vec<String>)> {
    // 0 PROCESSES — newest first, bounded by the registry cap.
    let mut procs = Vec::new();
    if stats.procs.is_empty() {
        procs.push(" no tracked processes".to_string());
    }
    for proc in stats.procs.iter() {
        let state = if proc.running {
            format!("▶ {}s", proc.age_s)
        } else {
            let code = proc
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "?".into());
            if proc.timed_out {
                format!("✗{code} TO")
            } else if proc.exit_code == Some(0) {
                format!("✓{code}")
            } else {
                format!("✗{code}")
            }
        };
        let cpu = proc
            .cpu_pct
            .map(|cpu| format!("{cpu:.0}%"))
            .unwrap_or_else(|| "n/a".into());
        let rss = proc
            .rss_kb
            .map(|kb| format!("{}M", kb / 1024))
            .unwrap_or_else(|| "n/a".into());
        let budget = width.saturating_sub(34).max(8);
        procs.push(format!(
            " {pid:>6} {state:<8} {cpu:>5} {rss:>6}  {cmd}",
            pid = proc.pid,
            cmd = clip(&proc.command, budget),
        ));
    }

    // 1 THINKING — level and mode, speed tier, the model/tool/wait split,
    // and the previous turn's model time and reasoning tokens (phase 4).
    let thinking = vec![
        format!(
            " level {}/20 {} · {} · {} · {}",
            stats.thinking_level,
            stats.think,
            speed_tier(stats.thinking_level),
            stats.status,
            if stats.inflight > 0 {
                format!("{} tools live", stats.inflight)
            } else {
                "idle".to_string()
            }
        ),
        format!(
            " thinking {} · acting {} · waiting {}",
            secs_text(stats.model_ms as u128),
            secs_text(stats.tool_ms as u128),
            secs_text(stats.wait_ms as u128),
        ),
        format!(
            " turns {} · last turn {}",
            stats.turns,
            sparkline(&stats.turn_tokens, width.saturating_sub(14).max(4)),
        ),
        format!(
            " last turn {} · reasoning {} tok",
            secs_text(stats.turn_model_ms as u128),
            stats.turn_reasoning_tok,
        ),
    ];

    // 2 TIMELINE — durations, parallelism, per-turn token spark.
    let timeline = vec![
        format!(
            " model {} · tools {} · wait {} · wall {}",
            secs_text(stats.model_ms as u128),
            secs_text(stats.tool_ms as u128),
            secs_text(stats.wait_ms as u128),
            format!("{}s", stats.elapsed_s),
        ),
        format!(
            " parallel ×{} · steps {} · fails {}",
            stats.max_parallel, stats.tool_calls, stats.tool_failures,
        ),
        format!(
            " tok {}",
            sparkline(&stats.turn_tokens, width.saturating_sub(6).max(4))
        ),
        format!(
            " recent: {}",
            if stats.tool_failures > 0 {
                stats.last_error.clone()
            } else {
                "—".to_string()
            }
        ),
    ];

    // 3 PLAN — the `update_plan` checklist (spec 3.1 / 3.2).
    let mut plan = Vec::new();
    if stats.plan.is_empty() {
        plan.push(" no plan yet (update_plan)".to_string());
    }
    for (text, done) in stats.plan.iter().take(10) {
        plan.push(format!(
            " {} {}",
            if *done { "☑" } else { "☐" },
            clip(text, width.saturating_sub(4).max(8)),
        ));
    }
    if stats.plan.len() > 10 {
        plan.push(format!(" +{} more", stats.plan.len() - 10));
    }

    // 4 TOKENS & COST — turn totals, cache rate, spark, cost and savings.
    let tokens = vec![
        format!(
            " in {} · out {} · reason {}",
            compact_number(stats.usage.input),
            compact_number(stats.usage.output),
            compact_number(stats.usage.reasoning),
        ),
        format!(
            " cache {}% · read {} · write {}",
            stats.cache_hit_pct,
            compact_number(stats.usage.cache_read),
            compact_number(stats.usage.cache_write),
        ),
        format!(
            " tok {}",
            sparkline(&stats.turn_tokens, width.saturating_sub(6).max(4))
        ),
        format!(
            " cost {} · saved ~{} tok",
            stats
                .cost_usd
                .map(|cost| format!("${cost:.5}"))
                .unwrap_or_else(|| "n/a".into()),
            compact_number(stats.usage.cache_read),
        ),
    ];

    // 5 FILES — git churn, newest/heaviest first.
    let mut files = Vec::new();
    let total_add: i64 = stats
        .changed_files
        .iter()
        .map(|(_, added, _)| added.max(&0))
        .sum();
    let total_del: i64 = stats
        .changed_files
        .iter()
        .map(|(_, _, removed)| removed)
        .sum();
    files.push(format!(
        " +{total_add} -{total_del} · {} changed",
        stats.changed_files.len()
    ));
    for (path, added, removed) in stats.changed_files.iter().take(8) {
        let mark = if *added < 0 {
            "+new".to_string()
        } else {
            format!("+{added}")
        };
        files.push(format!(" {mark} -{removed}  {path}",));
    }
    if stats.changed_files.len() > 8 {
        files.push(format!(" +{} more", stats.changed_files.len() - 8));
    }

    // 6 CTF — coverage, flags, dead ends (spec 3.2).
    let mut ctf = Vec::new();
    ctf.push(format!(
        " [{}] · tools {} ({} failed)",
        stats.category, stats.tool_calls, stats.tool_failures,
    ));
    ctf.push(format!(
        " coverage: {} files · {} searches · {} shells",
        stats.files_read, stats.searches, stats.shells,
    ));
    if stats.flags.is_empty() {
        ctf.push(format!(" flags {}", stats.flags_total));
    } else {
        for (flag, source) in stats.flags.iter().rev().take(3) {
            ctf.push(format!(" 🚩 {} ({})", clip(flag, 32), clip(source, 18)));
        }
    }
    if stats.tool_failures > 0 {
        ctf.push(format!(
            " dead ends: {} failed · last {}",
            stats.tool_failures, stats.last_error
        ));
    }
    ctf.push(format!(
        " checker: {}",
        if stats.verified {
            "VERIFIED".to_string()
        } else {
            "unverified".to_string()
        }
    ));

    // 7 BUDGET — cost against budget, steps, wall clock.
    let mut budget = Vec::new();
    let cost = stats.cost_usd;
    if stats.budget_usd > 0.0 {
        let used = cost.unwrap_or(0.0) / stats.budget_usd;
        budget.push(format!(
            " {} ${:.4} / ${:.2} ({:.0}%)",
            gauge(used, 10),
            cost.unwrap_or(0.0),
            stats.budget_usd,
            used * 100.0,
        ));
    } else {
        budget.push(format!(
            " {} ${} / n/a (unlimited)",
            gauge(0.0, 10),
            cost.map(|cost| format!("{cost:.4}"))
                .unwrap_or_else(|| "?".into()),
        ));
    }
    budget.push(format!(" steps {} (no cap)", stats.tool_calls));
    budget.push(format!(
        " wall {}s · flags {}",
        stats.elapsed_s, stats.flags_total
    ));
    budget.push(format!(
        " errors {} · status {}",
        stats.errors, stats.status
    ));

    vec![
        (DASH_PANELS[0], procs),
        (DASH_PANELS[1], thinking),
        (DASH_PANELS[2], timeline),
        (DASH_PANELS[3], plan),
        (DASH_PANELS[4], tokens),
        (DASH_PANELS[5], files),
        (DASH_PANELS[6], ctf),
        (DASH_PANELS[7], budget),
    ]
}

/// Paint one full dashboard frame: header row, footer key-hints, the
/// eight-panel grid (or a full-body detail view), and the background fill.
/// Shared by the TUI (`Ui::compose_dashboard`) and the attach client so both
/// render byte-identical grids from the same `DashStats`.
pub(crate) fn compose_grid(
    stats: &DashStats,
    width: u16,
    height: u16,
    colors: &'static Theme,
    focus: usize,
    selected: usize,
    detail: Option<(String, String)>,
) -> (Vec<Row>, (u16, u16)) {
    let w = width as usize;
    let h = height as usize;
    let mut frame = vec![Row::new("", Color::Reset); h];
    if h == 0 || w == 0 {
        return (frame, (0, 0));
    }
    frame[0] = Row::new(
        clip(
            &format!(
                " DASHBOARD · {}/{}/{}/{} · {}",
                stats.mode, stats.provider, stats.model, stats.category, stats.status
            ),
            w,
        ),
        colors.accent,
    );
    if h >= 2 {
        frame[h - 1] = Row::new(
            clip(
                " ←→ panel · ↑↓/jk select · Enter detail · Ctrl+C/K signal · Esc/Ctrl+D close",
                w,
            ),
            colors.muted,
        );
    }
    if h <= 2 {
        return (frame, (0, 0));
    }
    if let Some((title, body)) = detail {
        frame[1] = Row::new(clip(&format!("── {title} "), w), colors.accent);
        let capacity = h.saturating_sub(4);
        for (index, line) in body.lines().enumerate().take(capacity) {
            frame[2 + index] = Row::new(clip(line, w), colors.text);
        }
        let total = body.lines().count();
        if total > capacity && h >= 4 {
            frame[2 + capacity] = Row::new(
                clip(&format!(" ⋯ +{} more lines", total - capacity), w),
                colors.muted,
            );
        }
        if h >= 3 {
            frame[h - 2] = Row::new(clip(" Esc back", w), colors.muted);
        }
        return (frame, (0, 0));
    }
    let panels = dashboard_panels(stats, w.saturating_sub(1));
    let body_h = h - 2;
    let left_w = w / 2;
    let right_w = w.saturating_sub(left_w + 1);
    let left = dash_screen(
        &panels,
        [0, 1, 2, 3],
        &bands(body_h, &[4.0, 1.5, 1.5, 3.0]),
        left_w,
        focus,
        selected,
    );
    let right = dash_screen(
        &panels,
        [4, 5, 6, 7],
        &bands(body_h, &[3.0, 3.0, 2.5, 1.5]),
        right_w,
        focus,
        selected,
    );
    for row in 0..body_h {
        let (ltext, laccent) = left
            .get(row)
            .cloned()
            .unwrap_or_else(|| (String::new(), false));
        let (rtext, raccent) = right
            .get(row)
            .cloned()
            .unwrap_or_else(|| (String::new(), false));
        let mut text = pad_row(&ltext, left_w);
        text.push('│');
        text.push_str(&pad_row(&rtext, right_w));
        let color = if laccent || raccent {
            colors.accent
        } else {
            colors.text
        };
        frame[1 + row] = Row::new(clip(&text, w), color);
    }
    for row in &mut frame {
        if row.bg == Color::Reset {
            row.bg = colors.background;
        }
    }
    (frame, (0, 0))
}

/// The theme the attach client paints with (`NO_COLOR` → plain).
pub(crate) fn dashboard_theme(no_color: bool) -> &'static Theme {
    if no_color {
        &PLAIN_THEME
    } else {
        theme(0)
    }
}

/// Render one dashboard column: four banded panels, each a titled box with
/// its rows, marking the focused panel and the selected row (`▌`).
fn dash_screen(
    panels: &[(&'static str, Vec<String>)],
    indices: [usize; 4],
    band_heights: &[usize],
    width: usize,
    focus: usize,
    selected: usize,
) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for (slot, &index) in indices.iter().enumerate() {
        let start = out.len();
        let height = band_heights.get(slot).copied().unwrap_or(0);
        let Some((title, rows)) = panels.get(index) else {
            out.resize(start + height, (String::new(), false));
            continue;
        };
        let mark = if focus == index { "▌" } else { " " };
        let mut header = format!(" {mark}{title} ");
        while header.chars().count() < width.max(1) {
            header.push('─');
        }
        out.push((clip(&header, width), focus == index));
        for (row_index, text) in rows.iter().enumerate() {
            if out.len() - start >= height {
                break;
            }
            let is_sel = focus == index && selected == row_index;
            let prefix = if is_sel { "▌" } else { " " };
            out.push((clip(&format!("{prefix}{text}"), width), is_sel));
        }
        while out.len() - start < height {
            out.push((String::new(), false));
        }
        if out.len() - start > height {
            out.truncate(start + height);
        }
    }
    out
}

/// Pad (or truncate) a string to an exact display width.
fn pad_row(text: &str, width: usize) -> String {
    let mut out = clip(text, width);
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

/// The process registry as dashboard stats, newest first (shared with the
/// web `/v1/status` endpoint and the attach client).
pub(crate) fn proc_stats() -> Vec<ProcStat> {
    crate::tools::shell::proc_snapshots()
        .iter()
        .rev()
        .map(ProcStat::from)
        .collect()
}

/// The thinking label for a configured mode plus its effective level:
/// `high` for a concrete mode, `auto→medium` while auto drives the level.
/// Shared by the live UI and the offline [`stats_from_agent`] snapshot.
pub(crate) fn think_label(mode: ThinkLevel, level: u8) -> String {
    let name = ThinkLevel::from_level(level).name();
    if mode.is_auto() {
        format!("auto→{name}")
    } else {
        name.to_string()
    }
}

/// A [`DashStats`] snapshot straight from an [`Agent`]: what the web
/// `/v1/status` endpoint serves and what `wrosecode exec --json` prints.
/// Turn-split wall/tool timings belong to the TUI's event stream, so those
/// fields stay zero here; the per-turn reasoning and last-call latency come
/// from the agent's own counters.
pub(crate) fn stats_from_agent(agent: &Agent, session_id: &str, status: &str) -> DashStats {
    let metrics = agent.metrics.snapshot();
    let usage = agent.usage;
    let plan = agent
        .tools
        .checklist
        .lock()
        .map(|checklist| checklist.clone())
        .unwrap_or_default();
    let history = agent.ctf.history("");
    let flags_total = history.len();
    let root = agent.config.root.display().to_string();
    DashStats {
        schema: "wrosecode/live-v1".into(),
        live_at_ms: unix_ms(),
        session_id: session_id.into(),
        root,
        provider: agent.config.provider.clone(),
        model: agent.config.model.clone(),
        mode: agent.mode.clone(),
        category: agent.ctf.category.clone(),
        thinking_level: agent.thinking_level,
        think: think_label(agent.think, agent.thinking_level),
        status: status.into(),
        elapsed_s: agent.started_at.elapsed().as_secs(),
        budget_usd: agent.config.budget_usd,
        cost_usd: metrics.cost_usd,
        usage,
        cache_hit_pct: hit_percent(&usage),
        tool_calls: metrics.tool_calls as usize,
        tool_failures: metrics.tool_failures as usize,
        errors: metrics.rate_limits as usize,
        turns: agent.model_turns,
        plan,
        flags: flag_rows(&history),
        flags_total,
        changed_files: git_changed_files(&agent.config.root.display().to_string()),
        procs: proc_stats(),
        turn_reasoning_tok: agent.last_turn_reasoning,
        turn_model_ms: agent
            .last_api_latency
            .map(|latency| latency.as_millis() as u64)
            .unwrap_or(0),
        ..DashStats::default()
    }
}

/// Parse `.ctf/flags.log` lines into `(flag, source)` pairs, newest first
/// (shared with the offline attach fallback). The log format is
/// `unix_ts \t source \t transformation \t flag \t confidence`.
pub(crate) fn flag_rows(history: &[String]) -> Vec<(String, String)> {
    history
        .iter()
        .rev()
        .take(8)
        .map(|line| {
            let source = line.split('\t').nth(1).unwrap_or("").to_string();
            let flag = line.split('\t').nth(3).unwrap_or("").to_string();
            let flag = if flag.is_empty() {
                line.split_whitespace().next().unwrap_or(line).to_string()
            } else {
                flag
            };
            (
                flag,
                if source.is_empty() {
                    "checker".into()
                } else {
                    source
                },
            )
        })
        .collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    text.chars()
        .collect::<Vec<_>>()
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

pub async fn run(
    mut agent: Agent,
    initial_session: Option<Session>,
    store: crate::store::Store,
    // Entered (and the splash painted) back in main, before the expensive
    // startup work; holding it here is what keeps the terminal ours.
    guard: TerminalGuard,
) -> Result<()> {
    let profile = guard.profile();
    let _guard = guard;
    let (event_tx, mut events) = mpsc::unbounded_channel();
    let (permission_tx, mut permissions) = mpsc::unbounded_channel();
    agent.event_tx = Some(event_tx);
    agent.tools.permission_tx = Some(permission_tx);
    let mut settings = Settings::load()?;
    // User palettes must exist before Ui::new: `ui_theme` in config.toml may
    // name one.
    register_user_themes();
    let session_dir = Session::dir()?;
    let mut session = initial_session.unwrap_or_else(Session::fresh);
    let mut ui = Ui::new(&agent, session.name.clone());
    ui.history = load_history();
    ui.renderer.full_redraw = profile.full_redraw;
    ui.colors = profile.colors;
    ui.no_color = profile.colors == ColorSupport::None;
    // Test hook: the PTY suite floods the transcript with
    // `WROSECODE_E2E_LINES` entries at startup so it can prove bottom-pinned
    // auto-follow and the detached-scroll indicator without a model. Unknown
    // or unparsable values are ignored.
    if let Ok(raw) = std::env::var("WROSECODE_E2E_LINES") {
        if let Ok(count) = raw.trim().parse::<usize>() {
            for index in 0..count {
                ui.push(Speaker::System, format!("flood line {index}"));
            }
        }
    }
    if !session.transcript.is_empty() {
        ui.entries.clear();
        for (speaker, text) in &session.transcript {
            ui.entries.push(Entry {
                speaker: match speaker.to_ascii_lowercase().as_str() {
                    "you" | "user" => Speaker::User,
                    "wrose" | "agent" => Speaker::Agent,
                    "tool" => Speaker::Tool,
                    _ => Speaker::System,
                },
                text: text.clone(),
            });
        }
    }
    let mut last_checkpoint = std::time::Instant::now();
    let mut last_spin = std::time::Instant::now();
    let mut last_tip = std::time::Instant::now();
    loop {
        ui.render()?;
        if last_checkpoint.elapsed() >= Duration::from_secs(10) {
            sync_session(&mut session, &agent, &ui);
            store.save_session(&session)?;
            last_checkpoint = std::time::Instant::now();
        }
        if !event::poll(Duration::from_millis(16))? {
            if last_spin.elapsed() >= Duration::from_millis(90) {
                ui.spinner = ui.spinner.wrapping_add(1);
                last_spin = std::time::Instant::now();
            }
            if last_tip.elapsed() >= Duration::from_secs(4) {
                ui.advance_tip();
                last_tip = std::time::Instant::now();
            }
            // Keep `live.json` current for `wrosecode dashboard` attaches.
            ui.maybe_write_live();
            continue;
        }
        let next = event::read()?;
        if let Event::Paste(text) = &next {
            ui.accept_paste(text);
            continue;
        }
        if let Event::Mouse(mouse) = &next {
            let (terminal_width, _) = terminal::size()?;
            let separator = terminal_width as usize * ui.pane_percent / 100;
            if ui.dashboard {
                // The dashboard has no scrollback: the wheel moves the panel
                // selection instead (spec 3.2).
                match mouse.kind {
                    MouseEventKind::ScrollUp => ui.dash_move(-1),
                    MouseEventKind::ScrollDown => ui.dash_move(1),
                    MouseEventKind::Down(MouseButton::Left) => ui.dash_open(),
                    _ => {}
                }
                continue;
            }
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if (mouse.column as usize).abs_diff(separator) <= 1 =>
                {
                    ui.dragging_separator = true;
                }
                MouseEventKind::Drag(MouseButton::Left) if ui.dragging_separator => {
                    ui.pane_percent = (mouse.column as usize * 100
                        / terminal_width.max(1) as usize)
                        .clamp(40, 80);
                }
                MouseEventKind::Up(MouseButton::Left) => ui.dragging_separator = false,
                // A click on a transcript row expands or collapses its cell
                // (spec 3.1): map the terminal row back through line_origin.
                MouseEventKind::Down(MouseButton::Left) => {
                    let row = mouse.row as usize;
                    let first = ui.view.body_start + 1;
                    if row >= first && row < first + ui.view.height {
                        let index = ui.view.transcript_start + (row - first);
                        if let Some(entry) = ui.line_origin.get(index).copied() {
                            ui.toggle_entry(entry);
                        }
                    }
                }
                MouseEventKind::ScrollUp => ui.scroll_up(agent.config.smooth_scroll_lines),
                MouseEventKind::ScrollDown => ui.scroll_down(agent.config.smooth_scroll_lines),
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = next else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        // The dashboard owns every key while it is open (spec 3.2): panel
        // focus, selection, detail, signals, and close.
        if ui.dashboard {
            ui.dashboard_key(&key);
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('c')
        {
            // One press clears what you were typing; a second press from an
            // empty prompt exits.
            if ui.input.is_empty() {
                break;
            }
            ui.clear_input();
            ui.completion = None;
            ui.status = "Input cleared · Ctrl+C again to exit".into();
            continue;
        }
        match key.code {
            KeyCode::Esc if ui.paste_chip.is_some() => {
                ui.paste_chip = None;
                ui.status = "Paste discarded".into();
            }
            KeyCode::Esc if ui.picker.is_some() => {
                close_picker(&mut ui);
            }
            KeyCode::Esc if ui.palette => {
                ui.palette = false;
                ui.selected = 0;
            }
            KeyCode::Esc => {
                ui.navigation_mode = !ui.navigation_mode;
                ui.status = if ui.navigation_mode {
                    "Navigation mode · j/k scroll · Esc return".into()
                } else {
                    "Ready".into()
                };
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.backspace();
                ui.selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Readline kill-line when there is text; half page up when
                // not, so the scroll keys never depend on a full input box.
                if ui.input.is_empty() {
                    ui.scroll_half_page(false);
                } else {
                    ui.clear_input();
                }
                ui.palette = false;
                ui.selected = 0;
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if ui.input.is_empty() {
                    // Spec 3.2: Ctrl+D with an empty prompt opens the
                    // live dashboard; Ctrl+D closes it from inside.
                    ui.dashboard = true;
                    ui.dash_detail = None;
                    ui.dash_data_at = None;
                    ui.dash_files_at = None;
                    ui.status = "Dashboard · Ctrl+D close".into();
                } else {
                    ui.delete_forward();
                }
                ui.completion = None;
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Expand or collapse the entry at the top of the viewport
                // without reaching for the mouse (spec 3.1).
                ui.toggle_top_entry();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.cursor = 0;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.cursor = ui.input.len();
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.delete_word_before_cursor();
                ui.completion = None;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.set_input(String::new());
                ui.palette = false;
                ui.selected = 0;
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+L clears the visible transcript and forces a full
                // redraw; the session and the conversation context survive
                // (spec 1.3), and /clear --context is what drops those.
                ui.clear_transcript();
                ui.push(
                    Speaker::System,
                    "Transcript cleared. Session context and history are kept.",
                );
                ui.palette = false;
                ui.selected = 0;
                ui.status = "Transcript cleared".into();
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.set_input("/".into());
                ui.palette = true;
                ui.selected = 0;
                ui.status = "Command palette".into();
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Cycle the thinking level: off → low → … → max → auto → off.
                let level = agent.think.next();
                ui.apply_think(&mut agent, level);
            }
            KeyCode::Char('e')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                match agent
                    .metrics
                    .export(&agent.config.root.join(".ctf/reports"))
                {
                    Ok((json, csv)) => ui.push(
                        Speaker::System,
                        format!("Metrics exported:\n{}\n{}", json.display(), csv.display()),
                    ),
                    Err(error) => ui.push(Speaker::System, format!("Export failed: {error}")),
                }
            }
            KeyCode::Char('e' | 'E')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                let errors = std::fs::read_to_string(agent.config.root.join(".ctf/errors.log"))
                    .unwrap_or_else(|_| "No recorded errors.".into());
                ui.push(Speaker::System, format!("ERROR LOG\n{errors}"));
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let flags = agent.ctf.history("");
                ui.picker = Some(("Flag history".into(), flags.clone()));
                ui.picker_all = Some(flags);
                ui.picker_query.clear();
                ui.selected = 0;
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let items: Vec<String> = ui.history.iter().rev().cloned().collect();
                ui.picker = Some(("Prompt history".into(), items.clone()));
                ui.picker_all = Some(items);
                ui.picker_query.clear();
                ui.selected = 0;
            }
            KeyCode::Char('c' | 'C')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Some(entry) = ui.entries.last() {
                    let _ = crate::ctf::copy_to_clipboard(&entry.text);
                    ui.status = "Copied latest output".into();
                }
            }
            KeyCode::Char('[') if ui.input.is_empty() => {
                let level = agent.think.prev();
                ui.apply_think(&mut agent, level);
            }
            KeyCode::Char(']') if ui.input.is_empty() => {
                let level = agent.think.next();
                ui.apply_think(&mut agent, level);
            }
            KeyCode::Char('k') if ui.navigation_mode => ui.scroll_up(1),
            KeyCode::Char('j') if ui.navigation_mode => ui.scroll_down(1),
            KeyCode::Char(' ') if ui.navigation_mode => ui.scroll_page(true),
            KeyCode::Char('b') if ui.navigation_mode => ui.scroll_page(false),
            KeyCode::Char('/') if ui.navigation_mode => {
                if let Some(query) = ask_line(&mut ui, "Search transcript", "")? {
                    ui.search_query = query.to_ascii_lowercase();
                    ui.search_matches = ui
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| {
                            entry.text.to_ascii_lowercase().contains(&ui.search_query)
                        })
                        .map(|(index, _)| index)
                        .collect();
                    ui.search_position = 0;
                    jump_to_search(&mut ui);
                }
            }
            KeyCode::Char('n') if ui.navigation_mode && !ui.search_matches.is_empty() => {
                ui.search_position = (ui.search_position + 1) % ui.search_matches.len();
                jump_to_search(&mut ui);
            }
            KeyCode::Char('N') if ui.navigation_mode && !ui.search_matches.is_empty() => {
                ui.search_position =
                    (ui.search_position + ui.search_matches.len() - 1) % ui.search_matches.len();
                jump_to_search(&mut ui);
            }
            KeyCode::Char(ch)
                if ui.picker_all.is_some()
                    && !ch.is_control()
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                ui.picker_query.push(ch);
                ui.selected = 0;
            }
            KeyCode::Backspace if ui.picker_all.is_some() => {
                ui.picker_query.pop();
                ui.selected = 0;
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                ui.navigation_mode = false;
                let token_start = ui.input.is_empty()
                    || ui.input[..ui.cursor]
                        .chars()
                        .last()
                        .is_some_and(char::is_whitespace);
                if ch == '@' && token_start && !ui.palette && ui.picker_all.is_none() {
                    let files = list_files_for_mention(&ui.root);
                    ui.picker = Some(("Mention files".into(), files.clone()));
                    ui.picker_all = Some(files);
                    ui.picker_query.clear();
                    ui.selected = 0;
                    ui.status = "@ mention · type to filter · Enter insert · Esc cancel".into();
                    continue;
                }
                let opening = ch == '/' && ui.input.is_empty();
                ui.insert(ch);
                if opening {
                    ui.palette = true;
                    ui.selected = 0;
                } else if ui.palette {
                    ui.palette = !ui.input.contains(' ');
                    ui.selected = 0;
                }
            }
            KeyCode::Backspace => {
                ui.backspace();
                if ui.palette {
                    ui.selected = 0;
                    if ui.input.is_empty() {
                        ui.palette = false;
                    }
                }
            }
            KeyCode::Delete if ui.cursor == ui.input.len() => ui.backspace(),
            KeyCode::Delete if ui.cursor < ui.input.len() => {
                ui.input.remove(ui.cursor);
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => ui.move_word_left(),
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => ui.move_word_right(),
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => ui.move_word_left(),
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => ui.move_word_right(),
            KeyCode::Left => ui.move_left(),
            KeyCode::Right => ui.move_right(),
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => ui.scroll_home(),
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => ui.scroll_end(),
            KeyCode::Home if ui.input.is_empty() => ui.scroll_home(),
            KeyCode::End if ui.input.is_empty() => ui.scroll_end(),
            KeyCode::Home => ui.cursor = 0,
            KeyCode::End => ui.cursor = ui.input.len(),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => ui.scroll_up(5),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => ui.scroll_down(5),
            KeyCode::Up if ui.palette => {
                let len = commands::filtered(&ui.input, &ui.user_commands).len();
                if len > 0 {
                    ui.selected = (ui.selected + len - 1) % len;
                }
            }
            KeyCode::Up if ui.picker.is_some() => {
                ui.selected = ui.selected.saturating_sub(1);
            }
            KeyCode::Down if ui.palette => {
                let len = commands::filtered(&ui.input, &ui.user_commands).len();
                if len > 0 {
                    ui.selected = (ui.selected + 1) % len;
                }
            }
            KeyCode::Down if ui.picker.is_some() => {
                let len = picker_view(&ui).len();
                ui.selected = (ui.selected + 1).min(len.saturating_sub(1));
            }
            // Scroll first whenever the viewport is away from the bottom, so the
            // transcript never gets "stuck" behind prompt-history navigation.
            KeyCode::Up if ui.input.is_empty() && ui.scroll > 0 => ui.scroll_up(1),
            KeyCode::Down if ui.input.is_empty() && ui.scroll > 0 => ui.scroll_down(1),
            KeyCode::Up if ui.input.is_empty() && ui.history_index.is_none() => ui.scroll_up(1),
            KeyCode::Down if ui.input.is_empty() && ui.history_index.is_none() => ui.scroll_down(1),
            KeyCode::Up => {
                if !ui.history.is_empty() {
                    let index = ui
                        .history_index
                        .unwrap_or(ui.history.len())
                        .saturating_sub(1);
                    ui.history_index = Some(index);
                    ui.set_input(ui.history[index].clone());
                }
            }
            KeyCode::Down => {
                if let Some(index) = ui.history_index {
                    if index + 1 < ui.history.len() {
                        ui.history_index = Some(index + 1);
                        ui.set_input(ui.history[index + 1].clone());
                    } else {
                        ui.history_index = None;
                        ui.set_input(String::new());
                    }
                }
            }
            KeyCode::PageUp => ui.scroll_page(false),
            KeyCode::PageDown => ui.scroll_page(true),
            KeyCode::Tab if !ui.input.is_empty() => complete_input(&mut ui, &agent),
            KeyCode::Tab => {
                let next = if agent.mode == "build" {
                    "plan"
                } else {
                    "build"
                };
                agent.set_mode(next)?;
                ui.mode = next.to_ascii_uppercase();
                ui.status = if next == "plan" && !agent.config.planner_model.is_empty() {
                    format!("PLAN mode — planner {}", agent.config.planner_model)
                } else {
                    format!("{} mode", ui.mode)
                };
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                ui.insert('\n');
                ui.palette = false;
            }
            KeyCode::Enter => {
                if let Some((pasted, _)) = ui.paste_chip.take() {
                    ui.insert_str(&pasted);
                    ui.status = "Pasted into the input".into();
                    continue;
                }
                if ui.picker.is_some() {
                    let title = ui
                        .picker
                        .as_ref()
                        .map(|(title, _)| title.clone())
                        .unwrap_or_default();
                    let choices = picker_view(&ui);
                    let selected = ui.selected;
                    close_picker(&mut ui);
                    if let Some(choice) = choices.get(selected) {
                        if title == "Prompt history" {
                            ui.set_input(choice.clone());
                        } else if title == "Mention files" {
                            ui.insert_str(&format!("@{choice} "));
                        } else {
                            let _ = crate::ctf::copy_to_clipboard(choice);
                            ui.status = "Flag history entry copied".into();
                        }
                    }
                    continue;
                }
                if ui.palette {
                    let choices = commands::filtered(&ui.input, &ui.user_commands);
                    if let Some(command) = choices.get(ui.selected) {
                        ui.set_input(command.name.clone());
                    }
                    ui.palette = false;
                    ui.selected = 0;
                }
                let line = ui.take_input();
                if line.is_empty() {
                    // Enter on an empty prompt expands the top entry instead
                    // of doing nothing (spec 3.1).
                    ui.toggle_top_entry();
                    continue;
                }
                ui.push_history(line.clone());
                save_history(&ui.history);
                ui.push(Speaker::User, line.clone());
                if line == "/quit" || line == "/exit" {
                    break;
                }
                if line.starts_with('/') {
                    if let Err(error) = run_command(
                        &line,
                        &mut agent,
                        &mut ui,
                        &mut settings,
                        &mut session,
                        &mut events,
                        &mut permissions,
                    )
                    .await
                    {
                        ui.push(Speaker::System, format!("Error: {error:#}"));
                    }
                } else if let Some(fact) = line.strip_prefix("#!fact ") {
                    ui.push(Speaker::System, agent.memory.save(fact, true)?);
                } else if let Some(fact) = line.strip_prefix("#fact ") {
                    ui.push(Speaker::System, agent.memory.save(fact, false)?);
                } else {
                    if !ui.ctf_offered && mentions_ctf(&line) {
                        ui.ctf_offered = true;
                        ui.push(
                            Speaker::System,
                            "Tip: /ctf <file|dir|url|description> runs the scope-guarded CTF autopilot — parallel hypotheses, verified flags, budget enforced, writeup written.",
                        );
                    }
                    if !run_turn_queue(&mut agent, &mut ui, &line, &mut events, &mut permissions)
                        .await?
                    {
                        break;
                    }
                    if session.summary == "New session" {
                        session.summary = line.chars().take(80).collect();
                    }
                    save_session(&mut session, &agent, &ui, &session_dir)?;
                }
            }
            _ => {}
        }
    }
    save_session(&mut session, &agent, &ui, &session_dir)?;
    store.save_session(&session)?;
    Ok(())
}

/// What ended a turn: the model answered, or the user interrupted it.
enum TurnOutcome {
    Done(Result<String>),
    Cancelled,
}

async fn run_prompt(
    agent: &mut Agent,
    ui: &mut Ui,
    prompt: &str,
    events: &mut mpsc::UnboundedReceiver<Progress>,
    permissions: &mut mpsc::UnboundedReceiver<PermissionRequest>,
    scroll_step: usize,
) -> Result<bool> {
    ui.category = crate::ctf::categorize(&agent.config.root, prompt);
    ui.busy = true;
    ui.status = "Thinking".into();
    ui.begin_turn();
    ui.render()?;
    let project_root = agent.config.root.clone();
    let session_store = agent.store.clone();
    let metrics = agent.metrics.clone();
    let messages_before = agent.messages.len();
    // The turn future borrows `agent`, so it is confined to this block. Once
    // the block ends the borrow is released and a cancelled turn can be
    // rolled back instead of leaving half a conversation behind.
    let outcome = {
        let turn = agent.turn(prompt);
        tokio::pin!(turn);
        let mut ticker = tokio::time::interval(Duration::from_millis(16));
        loop {
            tokio::select! {
                result = &mut turn => break TurnOutcome::Done(result),
                Some(message) = events.recv() => {
                    ui.apply_progress(message);
                    ui.status = "Working".into();
                    ui.render()?;
                }
                Some(request) = permissions.recv() => {
                    if !ask_permission(ui, request).await? { return Ok(false); }
                    ui.status = "Working".into();
                }
                _ = ticker.tick() => {
                    ui.spinner = ui.spinner.wrapping_add(1);
                    // Advance the live `Thinking… n.ns` cell (spec 3.1).
                    ui.tick_thinking();
                    // Keep `live.json` fresh while a turn runs (spec 3.2).
                    ui.maybe_write_live();
                    // Keep the terminal usable while the model thinks: keys
                    // land in the input box instead of being thrown away.
                    if drain_turn_keys(ui, scroll_step)? {
                        break TurnOutcome::Cancelled;
                    }
                    ui.render()?;
                }
            }
        }
    };
    while let Ok(message) = events.try_recv() {
        ui.apply_progress(message);
    }
    ui.draft.clear();
    ui.end_turn();
    ui.busy = false;
    ui.status = "Ready".into();
    match outcome {
        TurnOutcome::Cancelled => {
            agent.messages.truncate(messages_before);
            ui.push(
                Speaker::System,
                "Turn cancelled. Your input and any queued messages are kept.",
            );
        }
        TurnOutcome::Done(Ok(reply)) => ui.push(
            Speaker::Agent,
            if reply.is_empty() {
                "(no response)".into()
            } else {
                reply
            },
        ),
        TurnOutcome::Done(Err(error)) => {
            crate::ctf::log_error(&project_root, "agent turn", &format!("{error:#}"));
            let kind = classify_error(&error.to_string());
            if kind == "rate_limit" {
                metrics.record_rate_limit();
            }
            let _ =
                session_store.record_error(&ui.session_id, kind, &error.to_string(), "agent turn");
            ui.record_error(kind);
            ui.push(
                Speaker::System,
                format!(
                    "Error [{kind}]: {error:#}\n   Fix: {}",
                    error_suggestion(kind)
                ),
            );
        }
    }
    ui.render()?;
    Ok(true)
}

/// Consume terminal input while a turn is running. Typing goes to the input
/// box, Enter queues the message for after the turn, scroll keys move the
/// viewport, and Ctrl+C cancels the turn (the TUI stays up).
fn drain_turn_keys(ui: &mut Ui, scroll_step: usize) -> Result<bool> {
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            Event::Paste(text) => {
                ui.accept_paste(&text);
                ui.completion = None;
            }
            Event::Mouse(mouse) => match mouse.kind {
                // The dashboard steals the wheel for selection (spec 3.2).
                _ if ui.dashboard => match mouse.kind {
                    MouseEventKind::ScrollUp => ui.dash_move(-1),
                    MouseEventKind::ScrollDown => ui.dash_move(1),
                    _ => {}
                },
                MouseEventKind::ScrollUp => ui.scroll_up(scroll_step),
                MouseEventKind::ScrollDown => ui.scroll_down(scroll_step),
                _ => {}
            },
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                // Dashboard keys are consumed before anything else, so a busy
                // turn can still be watched and its processes signalled.
                if ui.dashboard {
                    ui.dashboard_key(&key);
                    continue;
                }
                if control && key.code == KeyCode::Char('c') {
                    return Ok(true);
                }
                match key.code {
                    KeyCode::Esc if ui.paste_chip.is_some() => {
                        ui.paste_chip = None;
                    }
                    KeyCode::Char(ch) if !control && !alt => {
                        ui.navigation_mode = false;
                        ui.insert(ch);
                    }
                    KeyCode::Backspace if !control => ui.backspace(),
                    KeyCode::Delete => ui.delete_forward(),
                    KeyCode::Left if alt => ui.move_word_left(),
                    KeyCode::Right if alt => ui.move_word_right(),
                    KeyCode::Left if !control => ui.move_left(),
                    KeyCode::Right if !control => ui.move_right(),
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        ui.insert('\n');
                    }
                    KeyCode::Enter if ui.input.is_empty() => ui.toggle_top_entry(),
                    KeyCode::Enter if ui.paste_chip.is_some() => {
                        let (pasted, _) = ui.paste_chip.take().expect("chip is present");
                        ui.insert_str(&pasted);
                    }
                    KeyCode::Enter => {
                        let queued = ui.take_input();
                        ui.completion = None;
                        ui.push(
                            Speaker::System,
                            format!("Queued while thinking: {}", clip(&queued, 72)),
                        );
                        ui.status = format!("Thinking · {} queued", ui.pending.len() + 1);
                        ui.push_history(queued.clone());
                        ui.pending.push(queued);
                    }
                    KeyCode::Home if ui.input.is_empty() => ui.scroll_home(),
                    KeyCode::End if ui.input.is_empty() => ui.scroll_end(),
                    KeyCode::Home => ui.cursor = 0,
                    KeyCode::End => ui.cursor = ui.input.len(),
                    KeyCode::Up => ui.scroll_up(1),
                    KeyCode::Down => ui.scroll_down(1),
                    KeyCode::PageUp => ui.scroll_page(false),
                    KeyCode::PageDown => ui.scroll_page(true),
                    KeyCode::Char('u') if control => {
                        if ui.input.is_empty() {
                            ui.scroll_half_page(false);
                        } else {
                            ui.clear_input();
                        }
                    }
                    KeyCode::Char('d') if control => {
                        if ui.input.is_empty() {
                            // Open the dashboard mid-turn too (spec 3.2).
                            ui.dashboard = true;
                            ui.dash_detail = None;
                            ui.dash_data_at = None;
                            ui.dash_files_at = None;
                        } else {
                            ui.delete_forward();
                        }
                    }
                    KeyCode::Char('o') if control => ui.toggle_top_entry(),
                    KeyCode::Char('w') if control => ui.delete_word_before_cursor(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// Run one prompt, then everything that was queued while it was running.
/// Returns `false` when the caller should exit the TUI.
async fn run_turn_queue(
    agent: &mut Agent,
    ui: &mut Ui,
    first: &str,
    events: &mut mpsc::UnboundedReceiver<Progress>,
    permissions: &mut mpsc::UnboundedReceiver<PermissionRequest>,
) -> Result<bool> {
    let scroll_step = agent.config.smooth_scroll_lines;
    // One result block covers the prompt plus anything queued behind it.
    ui.task_begin();
    let mut line = first.to_string();
    loop {
        if !run_prompt(agent, ui, &line, events, permissions, scroll_step).await? {
            return Ok(false);
        }
        let Some(next) = ui.pending.first().cloned() else {
            ui.push_result_block();
            return Ok(true);
        };
        ui.pending.remove(0);
        ui.push(Speaker::User, next.clone());
        line = next;
        ui.status = format!("Queued · {} left", ui.pending.len());
        ui.render()?;
    }
}

async fn ask_permission(ui: &mut Ui, request: PermissionRequest) -> Result<bool> {
    ui.status = format!("Allow {}?  y / N", request.action);
    ui.render()?;
    loop {
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(KeyEvent {
                    code: KeyCode::Char('y' | 'Y'),
                    ..
                }) => {
                    let _ = request.response.send(true);
                    ui.push(Speaker::System, format!("Allowed {}", request.action));
                    return Ok(true);
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Enter,
                    ..
                }) => {
                    let _ = request.response.send(false);
                    ui.push(Speaker::System, format!("Denied {}", request.action));
                    return Ok(true);
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::CONTROL) => {
                    // Deny this one call but keep the session alive; quitting
                    // the whole TUI from a permission prompt is a trap.
                    let _ = request.response.send(false);
                    ui.push(Speaker::System, format!("Denied {}", request.action));
                    return Ok(true);
                }
                Event::Resize(_, _) => ui.render()?,
                _ => {}
            }
        }
    }
}

fn save_session(
    session: &mut Session,
    agent: &Agent,
    ui: &Ui,
    dir: &std::path::Path,
) -> Result<()> {
    sync_session(session, agent, ui);
    session.save(dir)?;
    crate::store::Store::open_default()?.save_session(session)?;
    Ok(())
}

fn sync_session(session: &mut Session, agent: &Agent, ui: &Ui) {
    session.messages = agent.messages.clone();
    session.pinned = agent.pinned.clone();
    session.provider_name = agent.config.provider.clone();
    session.model = agent.config.model.clone();
    session.transcript = ui
        .entries
        .iter()
        .map(|entry| {
            (
                match entry.speaker {
                    Speaker::User => "user",
                    Speaker::Agent => "agent",
                    Speaker::Tool => "tool",
                    Speaker::System => "system",
                }
                .into(),
                entry.text.clone(),
            )
        })
        .collect();
}

fn restore_session(
    session: &Session,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &Settings,
) -> Result<()> {
    if !session.provider_name.is_empty() && !session.model.is_empty() {
        switch_provider(agent, ui, settings, &session.provider_name, &session.model)?;
    }
    agent.messages = session.messages.clone();
    agent.pinned = session.pinned.clone();
    ui.entries = session
        .transcript
        .iter()
        .map(|(speaker, text)| Entry {
            speaker: match speaker.as_str() {
                "user" => Speaker::User,
                "agent" => Speaker::Agent,
                "tool" => Speaker::Tool,
                _ => Speaker::System,
            },
            text: text.clone(),
        })
        .collect();
    ui.scroll = 0;
    Ok(())
}

fn ask_line(ui: &mut Ui, label: &str, default: &str) -> Result<Option<String>> {
    ask_input(ui, label, default, false)
}

fn ask_secret(ui: &mut Ui, label: &str) -> Result<Option<String>> {
    ask_input(ui, label, "", true)
}

/// Fuzzy-filter picker entries. Substring matches win first through
/// [`commands::fuzzy_score`], so typing `flg` still finds `flag{...}`.
fn filter_picker_items(items: &[String], query: &str) -> Vec<String> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return items.to_vec();
    }
    items
        .iter()
        .filter(|item| commands::fuzzy_score(item, &needle).is_some())
        .cloned()
        .collect()
}

/// `~/.wrosecode/history` — prompt history survives restarts (spec 1.2).
fn history_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".wrosecode").join("history"))
}

/// Load persisted prompt history, dropping consecutive duplicates and
/// anything past the 500-entry cap `push_history` enforces.
fn load_history() -> Vec<String> {
    let Some(path) = history_file() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    lines.dedup();
    while lines.len() > 500 {
        lines.remove(0);
    }
    lines
}

/// Best-effort save: a read-only home directory must not fail the shell.
fn save_history(history: &[String]) {
    let Some(path) = history_file() else {
        return;
    };
    let mut text = history.join("\n");
    text.push('\n');
    let _ = std::fs::write(path, text);
}

/// Files offered by the `@` mention picker: a fuzzy-filterable slice of the
/// project tree, skipping VCS/build noise and capped so a huge repository
/// cannot stall a keystroke (spec 1.2).
fn list_files_for_mention(root: &str) -> Vec<String> {
    let skip_dirs = [".git", "target", "node_modules", ".wrosecode"];
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(entry.file_type().is_dir() && skip_dirs.contains(&name.as_ref()))
        });
    for entry in walker.flatten() {
        if files.len() >= 400 {
            break;
        }
        if entry.file_type().is_file() {
            if let Ok(relative) = entry.path().strip_prefix(root) {
                files.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    files.sort();
    files
}

/// What an open picker should display: the query-filtered list when the picker
/// was opened with `Ctrl+F` / `Ctrl+R`, otherwise the picker's own list.
fn picker_view(ui: &Ui) -> Vec<String> {
    let Some((_, fallback)) = &ui.picker else {
        return Vec::new();
    };
    match &ui.picker_all {
        Some(all) => filter_picker_items(all, &ui.picker_query),
        None => fallback.clone(),
    }
}

fn close_picker(ui: &mut Ui) {
    ui.picker = None;
    ui.picker_all = None;
    ui.picker_query.clear();
    ui.selected = 0;
}

fn ask_input(ui: &mut Ui, label: &str, default: &str, masked: bool) -> Result<Option<String>> {
    ui.palette = false;
    close_picker(ui);
    ui.status = format!("{label}  •  Enter accept  •  Esc cancel");
    ui.mask_input = masked;
    ui.set_input(default.into());
    loop {
        ui.render()?;
        let event = event::read()?;
        if let Event::Paste(text) = &event {
            for ch in text.chars() {
                if ch != '\n' && ch != '\r' {
                    ui.insert(ch);
                }
            }
            continue;
        }
        if let Event::Key(key) = event {
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Esc => {
                    ui.set_input(String::new());
                    ui.status = "Ready".into();
                    ui.mask_input = false;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    let value = ui.take_input();
                    ui.status = "Ready".into();
                    ui.mask_input = false;
                    return Ok(Some(value));
                }
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    ui.backspace()
                }
                KeyCode::Char(ch) => ui.insert(ch),
                KeyCode::Backspace => ui.backspace(),
                KeyCode::Delete if ui.cursor == ui.input.len() => ui.backspace(),
                KeyCode::Delete if ui.cursor < ui.input.len() => {
                    ui.input.remove(ui.cursor);
                }
                KeyCode::Left => ui.move_left(),
                KeyCode::Right => ui.move_right(),
                _ => {}
            }
        }
    }
}

fn picker(ui: &mut Ui, title: &str, items: Vec<String>) -> Result<Option<usize>> {
    if items.is_empty() {
        ui.push(Speaker::System, format!("No {title} available"));
        return Ok(None);
    }
    ui.set_input(String::new());
    ui.selected = 0;
    // These pickers filter through `ui.input`; a stale Ctrl+F query would
    // otherwise keep narrowing their list.
    ui.picker_all = None;
    ui.picker_query.clear();
    loop {
        let matches: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.to_ascii_lowercase()
                    .contains(&ui.input.to_ascii_lowercase())
            })
            .map(|(index, _)| index)
            .collect();
        ui.selected = ui.selected.min(matches.len().saturating_sub(1));
        ui.picker = Some((
            format!("{title} • type to filter"),
            matches.iter().map(|index| items[*index].clone()).collect(),
        ));
        ui.render()?;
        let event = event::read()?;
        if let Event::Paste(text) = &event {
            for ch in text.chars() {
                if ch != '\n' && ch != '\r' {
                    ui.insert(ch);
                }
            }
            ui.selected = 0;
            continue;
        }
        if let Event::Key(key) = event {
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Esc => {
                    ui.picker = None;
                    ui.set_input(String::new());
                    return Ok(None);
                }
                KeyCode::Enter => {
                    let result = matches.get(ui.selected).copied();
                    ui.picker = None;
                    ui.set_input(String::new());
                    return Ok(result);
                }
                KeyCode::Up if !matches.is_empty() => {
                    ui.selected = (ui.selected + matches.len() - 1) % matches.len()
                }
                KeyCode::Down if !matches.is_empty() => {
                    ui.selected = (ui.selected + 1) % matches.len()
                }
                KeyCode::PageUp => ui.selected = ui.selected.saturating_sub(5),
                KeyCode::PageDown => {
                    ui.selected = (ui.selected + 5).min(matches.len().saturating_sub(1))
                }
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    ui.backspace();
                    ui.selected = 0;
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    ui.backspace();
                    ui.selected = 0;
                }
                KeyCode::Char(ch) => {
                    ui.insert(ch);
                    ui.selected = 0;
                }
                _ => {}
            }
        }
    }
}

fn switch_provider(
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &Settings,
    name: &str,
    model: &str,
) -> Result<()> {
    let profile = settings
        .profile(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    let provider = provider::create_profile(
        profile,
        settings.key(profile),
        model,
        agent.tools.client.clone(),
    )?;
    let mut config = (*agent.config).clone();
    config.provider = name.into();
    config.model = model.into();
    agent.config = std::sync::Arc::new(config);
    agent.tools.config = agent.config.clone();
    agent.provider = provider;
    ui.provider = name.into();
    ui.model = model.into();
    ui.push(Speaker::System, format!("Provider: {name} / {model}"));
    Ok(())
}

async fn available_models(agent: &Agent, settings: &Settings) -> Vec<(String, String)> {
    use futures::future::join_all;
    let profiles: Vec<_> = settings
        .providers
        .iter()
        .filter(|p| {
            !p.base_url.is_empty() && (settings.key(p).is_some() || settings.no_key_needed(p))
        })
        .cloned()
        .collect();
    let client = agent.tools.client.clone();
    let results = join_all(profiles.into_iter().map(|profile| {
        let client = client.clone();
        let key = settings.key(&profile);
        async move {
            let mut models = if let Ok(provider) =
                provider::create_profile(&profile, key, &profile.model, client)
            {
                tokio::time::timeout(Duration::from_secs(5), provider.list_models())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if !profile.model.is_empty() {
                models.push(profile.model.clone());
            }
            models.sort();
            models.dedup();
            models
                .into_iter()
                .map(|model| (profile.name.clone(), model))
                .collect::<Vec<_>>()
        }
    }))
    .await;
    results.into_iter().flatten().collect()
}

/// True when a prompt looks like it wants the CTF autopilot (flag, challenge,
/// or CTF mentioned) so the `/ctf` offer shows once per session.
fn mentions_ctf(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("flag{") || lower.contains("ctf") || lower.contains("challenge")
}

async fn run_command(
    line: &str,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
    session: &mut Session,
    events: &mut mpsc::UnboundedReceiver<Progress>,
    permissions: &mut mpsc::UnboundedReceiver<PermissionRequest>,
) -> Result<()> {
    let session_dir = Session::dir()?;
    let (name, args) = line
        .split_once(' ')
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((line, ""));
    if commands::lookup(line).is_none() {
        // A user-defined command from `.wrosecode/commands/<name>.md` expands
        // its body (`$ARGUMENTS` replaced) and runs it as a normal turn.
        if let Some(command) = ui.user_commands.iter().find(|command| command.name == name) {
            let prompt = command.expand(args);
            let summary: String = prompt.chars().take(80).collect();
            if !run_turn_queue(agent, ui, &prompt, events, permissions).await? {
                return Ok(());
            }
            if session.summary == "New session" {
                session.summary = summary;
            }
            save_session(&mut *session, agent, ui, &session_dir)?;
            return Ok(());
        }
        ui.push(
            Speaker::System,
            format!("Unknown command: {name}. Type /help."),
        );
        return Ok(());
    }
    match name {
        "/help" => ui.push(Speaker::System, commands::help(&ui.user_commands)),
        "/clear" => {
            let drop_context = args.split_whitespace().any(|flag| flag == "--context");
            ui.clear_transcript();
            if drop_context {
                agent.messages.clear();
                ui.push(
                    Speaker::System,
                    "Transcript and context cleared. The next turn starts fresh.",
                );
            } else {
                ui.push(
                    Speaker::System,
                    "Transcript cleared. Session context and history are kept (Ctrl+L does the same · /clear --context drops context).",
                );
            }
        }
        "/build" | "/plan" => {
            let mode = name.trim_start_matches('/');
            agent.set_mode(mode)?;
            ui.mode = mode.to_ascii_uppercase();
            if mode == "plan" && !agent.config.planner_model.is_empty() {
                ui.push(
                    Speaker::System,
                    format!(
                        "plan agent active — planner {}/{}",
                        agent.config.planner_provider, agent.config.planner_model
                    ),
                );
            } else {
                ui.push(Speaker::System, format!("{mode} agent active"));
            }
        }
        "/agents" => {
            let modes = ["build", "plan", "general"];
            let user = crate::markdown::discover_agents(&agent.config.root);
            let mut labels: Vec<String> = modes
                .iter()
                .map(|mode| format!("{} {mode}", if agent.mode == *mode { "●" } else { " " }))
                .collect();
            let active = agent.agent_name.clone();
            labels.extend(user.iter().map(|selected_agent| {
                format!(
                    "{} {} ({})",
                    if selected_agent.name == active {
                        "●"
                    } else {
                        " "
                    },
                    selected_agent.name,
                    selected_agent.description
                )
            }));
            if let Some(index) = picker(ui, "Agents", labels)? {
                let selected = user
                    .get(index.saturating_sub(modes.len()))
                    .filter(|_| index >= modes.len());
                if let Some(selected) = selected {
                    agent.set_user_agent(selected)?;
                    ui.mode = agent.mode.to_ascii_uppercase();
                    ui.think = agent.think;
                    ui.thinking_level = agent.thinking_level;
                    ui.push(
                        Speaker::System,
                        format!("{} agent active ({})", selected.name, selected.description),
                    );
                } else if let Some(mode) = modes.get(index) {
                    agent.set_mode(mode)?;
                    ui.mode = mode.to_ascii_uppercase();
                    ui.push(Speaker::System, format!("{mode} agent active"));
                }
            }
        }
        "/harness" => {
            let chosen = if args.is_empty() {
                ask_line(ui, "Harness (minimal/swe/claude)", agent.harness.name())?
            } else {
                Some(args.into())
            };
            if let Some(chosen) = chosen {
                if let Some(harness) = Harness::parse(&chosen) {
                    agent.harness = harness;
                    ui.harness = harness.name().into();
                    ui.push(Speaker::System, format!("Harness: {}", harness.name()));
                } else {
                    ui.push(Speaker::System, "Unknown harness");
                }
            }
        }
        "/memory" => ui.push(Speaker::System, agent.memory.list()?.join("\n")),
        "/greet" => ui.push(
            Speaker::System,
            format!(
                "WROSECODE  /  {}\n{} • {} / {} • {} • {}",
                ui.root, ui.mode, ui.provider, ui.model, ui.harness, ui.permission
            ),
        ),
        "/debug" => {
            let map_size = agent
                .repo_map
                .lock()
                .map(|map| map.len())
                .unwrap_or_default();
            let latency = agent
                .last_api_latency
                .map(|d| format!("{} ms", d.as_millis()))
                .unwrap_or_else(|| "none".into());
            ui.push(Speaker::System, format!("Provider/model: {} / {}\nPermission: {}\nAgent: {}\nSkills: {}\nMemory facts: {}\nRepo map files: {}\nLast API latency: {}\nFirst paint: {}\nVersion: {} ({})", agent.config.provider, agent.config.model, ui.permission, agent.mode, agent.skills.len(), agent.memory.list()?.len(), map_size, latency, first_paint_line(), env!("CARGO_PKG_VERSION"), option_env!("WROSECODE_GIT_COMMIT").unwrap_or("unknown")));
        }
        "/theme" => {
            if args.is_empty() {
                let labels = theme_names()
                    .into_iter()
                    .map(|name| format!("{name}  (palette)"))
                    .collect();
                if let Some(index) = picker(ui, "Themes", labels)? {
                    ui.theme = index;
                    ui.status = format!("Theme: {}", theme(ui.theme).name);
                    ui.push(
                        Speaker::System,
                        format!("Theme set to {}", theme(ui.theme).name),
                    );
                }
            } else if let Some(index) = theme_position(args) {
                ui.theme = index;
                ui.status = format!("Theme: {}", theme(ui.theme).name);
                ui.push(
                    Speaker::System,
                    format!("Theme set to {}", theme(ui.theme).name),
                );
            } else {
                // Two lines, not one: compact mode clips a transcript entry at
                // the pane edge, and a single long line loses the tail of the
                // theme list on a narrow terminal.
                ui.push(
                    Speaker::System,
                    format!(
                        "Unknown theme `{args}`.\nAvailable: {}",
                        theme_names().join(", ")
                    ),
                );
            }
        }
        "/think" => {
            let levels = ThinkLevel::ALL.map(|level| level.name()).join(", ");
            if args.is_empty() {
                ui.push(
                    Speaker::System,
                    format!(
                        "Thinking level: {}\nAvailable: {levels}\n\
                         Ctrl+T cycles · --think sets the startup default · auto escalates without progress",
                        ui.think_label()
                    ),
                );
            } else if let Some(level) = ThinkLevel::parse(args) {
                ui.apply_think(agent, level);
                ui.push(Speaker::System, format!("Think: {}", level.name()));
            } else {
                ui.push(
                    Speaker::System,
                    format!("Unknown thinking level `{args}`.\nAvailable: {levels}"),
                );
            }
        }
        "/verbosity" => {
            let current = ui.verbosity.clone();
            let mode = if args.is_empty() {
                ask_line(ui, "Verbosity (compact/normal/verbose)", &current)?
            } else {
                Some(args.into())
            };
            if let Some(mode) = mode {
                if matches!(mode.as_str(), "compact" | "normal" | "verbose") {
                    ui.verbosity = mode.clone();
                    ui.push(Speaker::System, format!("Verbosity: {mode}"));
                } else {
                    ui.push(
                        Speaker::System,
                        "Verbosity must be compact, normal, or verbose",
                    );
                }
            }
        }
        "/stats" => {
            // The same eight panels as the dashboard, printed as text
            // (spec 3.2: `/stats` must be readable without the grid).
            let width = terminal::size().map(|(w, _)| w as usize).unwrap_or(100);
            let stats = ui.dash_stats();
            let mut text = format!(
                "Session {} · {}/{} · [{}] · {}\n{}\n",
                stats.session_id,
                stats.provider,
                stats.model,
                stats.category,
                stats.status,
                agent.stats_line()
            );
            for (title, rows) in dashboard_panels(&stats, width.saturating_sub(4)) {
                text.push_str(&format!("[{title}]\n"));
                for row in rows {
                    text.push_str(&row);
                    text.push('\n');
                }
            }
            ui.push(Speaker::System, text);
        }
        "/dashboard" => {
            ui.dashboard = true;
            ui.dash_detail = None;
            ui.dash_data_at = None;
            ui.dash_files_at = None;
            ui.status = "Dashboard · Ctrl+D close".into();
        }
        "/export" => {
            let (json, csv) = agent
                .metrics
                .export(&agent.config.root.join(".ctf/reports"))?;
            ui.push(
                Speaker::System,
                format!("Exported:\n{}\n{}", json.display(), csv.display()),
            );
        }
        "/flags" => {
            let flags = agent.ctf.history(args);
            ui.push(
                Speaker::System,
                if flags.is_empty() {
                    "No matching flags".into()
                } else {
                    flags.join("\n")
                },
            );
        }
        "/sandbox" => {
            let sandbox = agent.tools.sandbox.clone();
            let engine = if sandbox.enabled() { "docker" } else { "host" };
            let image = agent.config.sandbox.image.clone();
            let status = format!(
                "Sandbox: {}\nEngine: {engine}\nImage: {image}\n\n/sandbox up   · warm the container so the first command has no cold start\n/sandbox down · stop and remove the container",
                sandbox.describe()
            );
            match args.split_whitespace().next() {
                None => ui.push(Speaker::System, status),
                Some("down" | "reset") => ui.push(Speaker::System, sandbox.reset().await?),
                Some("up" | "warm") => ui.push(Speaker::System, sandbox.warm().await?),
                Some(other) => ui.push(
                    Speaker::System,
                    format!("Unknown action `{other}`\n\n{status}"),
                ),
            }
        }
        "/writeup" => {
            sync_session(session, agent, ui);
            let flags: Vec<String> = agent
                .ctf
                .history("")
                .into_iter()
                .filter_map(|line| line.split('\t').nth(3).map(str::to_string))
                .collect();
            let path = crate::report::writeup(&agent.config.root, session, &flags)?;
            if let Some(url) = &agent.config.qdrant_url {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let _ = crate::tools::vector::archive(
                        &agent.tools.client,
                        url,
                        &session.summary,
                        &text,
                    )
                    .await;
                }
            }
            ui.push(
                Speaker::System,
                format!("Writeup generated: {}", path.display()),
            );
        }
        "/ctf" => {
            let mut options = crate::autopilot::Options::default();
            let mut words = args.split_whitespace().peekable();
            while let Some(word) = words.next() {
                match word {
                    "--flag-format" => options.flag_format = words.next().map(str::to_string),
                    "--category" => options.category = words.next().map(str::to_string),
                    "--remote" => options.remote = words.next().map(str::to_string),
                    "--budget" => match words.next() {
                        Some(value) => options.budget = crate::autopilot::Budget::parse(value)?,
                        None => {
                            ui.push(
                                Speaker::System,
                                "--budget needs a value (steps, or steps=…,tokens=…,seconds=…)",
                            );
                            return Ok(());
                        }
                    },
                    "--parallel" => {
                        match words.next().and_then(|value| value.parse::<usize>().ok()) {
                            Some(value) => options.parallel = value,
                            None => {
                                ui.push(Speaker::System, "--parallel needs a number");
                                return Ok(());
                            }
                        }
                    }
                    other if other.starts_with("--") => {
                        ui.push(
                            Speaker::System,
                            format!("Unknown flag {other}. Try /ctf [target] [--flag-format F] [--category C] [--remote host:port] [--budget B] [--parallel N]"),
                        );
                        return Ok(());
                    }
                    other => options.target = other.to_string(),
                }
            }
            let root = agent.config.root.clone();
            let scope = crate::autopilot::Scope::for_target(
                &options.target,
                &root,
                options.remote.as_deref(),
            );
            agent.tools.set_scope(Some(scope.clone()));
            if let Some(pattern) = &options.flag_format {
                agent.ctf = agent.ctf.clone().with_flag_format(pattern)?;
            }
            agent.ctf.lock_category();
            let inventory = crate::autopilot::tool_inventory();
            ui.push(Speaker::System, format!("SCOPE   {}", scope.describe()));
            ui.push(Speaker::System, format!("TOOLS   {}", inventory.describe()));
            let triage_report = crate::autopilot::triage(&root).await;
            let category = options.category.clone().unwrap_or_else(|| {
                crate::autopilot::guess_category(&root, &options.target, &triage_report)
            });
            agent.ctf.category = category.clone();
            ui.category = category.clone();
            ui.push(Speaker::System, format!("GUESS   {category}"));
            crate::autopilot::init_notes(&root, &options, &scope, &triage_report)?;
            let playbook = crate::skills::match_skill(
                &agent.skills,
                &format!("{} {}", options.target, triage_report),
            )
            .cloned();
            let mut brief = crate::autopilot::brief(
                &options,
                &scope,
                &inventory,
                &category,
                &triage_report,
                playbook.as_ref(),
            );
            let parallel = options.parallel.max(1);
            brief = format!(
                "Parallelism: run at most {parallel} hypothesis subagent(s) at a time via delegate_task (hypotheses: {category}).\n{brief}"
            );
            let mut stuck = crate::autopilot::Stuck::default();
            let started = std::time::Instant::now();
            let mut steps = 0_usize;
            let mut last_reply: Option<String> = None;
            let mut last_candidate: Option<String> = None;
            let mut stop_reason: Option<String> = None;
            loop {
                ui.status = format!("CTF autopilot · step {}", steps + 1);
                let entries_before = ui.entries.len();
                let messages_before = agent.messages.len();
                if !run_turn_queue(agent, ui, &brief, events, permissions).await? {
                    stop_reason = Some("permission denied".into());
                    break;
                }
                let fresh = &ui.entries[entries_before..];
                let reply = fresh
                    .iter()
                    .rev()
                    .find(|entry| matches!(entry.speaker, Speaker::Agent))
                    .map(|entry| entry.text.clone());
                let cancelled = fresh.iter().any(|entry| {
                    matches!(entry.speaker, Speaker::System)
                        && entry.text.starts_with("Turn cancelled")
                });
                let Some(reply) = reply else {
                    stop_reason = Some(if cancelled || agent.messages.len() <= messages_before {
                        "stopped by the user".into()
                    } else {
                        "the last turn produced no reply".into()
                    });
                    break;
                };
                steps += 1;
                let head: String = reply.trim().chars().take(160).collect();
                ui.push(
                    Speaker::System,
                    format!(
                        "[{steps}/{}] {}",
                        options.budget.steps,
                        head.replace('\n', " ")
                    ),
                );
                let tokens = agent.usage.input + agent.usage.output;
                let hits = agent.ctf.recorded_hits();
                if let Some(verified) = crate::autopilot::verify(&agent.ctf, &hits) {
                    ui.push(
                        Speaker::System,
                        format!("✔ flag verified: {} ({})", verified.flag, verified.evidence),
                    );
                    ui.verified = true;
                    break;
                }
                if let Some(candidate) = crate::autopilot::candidate_note(&agent.ctf, &hits) {
                    if last_candidate.as_deref() != Some(candidate.as_str()) {
                        last_candidate = Some(candidate.clone());
                        ui.push(Speaker::System, format!("⚠ {candidate}"));
                    }
                }
                let repeats = last_reply.as_deref() == Some(reply.trim());
                let failures = usize::from(reply.trim().is_empty()) + usize::from(repeats);
                last_reply = Some(reply.trim().to_string());
                if stuck.step(failures) {
                    agent.thinking_level = agent.thinking_level.max(10);
                    ui.thinking_level = agent.thinking_level;
                    agent.ctf.category = crate::autopilot::CATEGORIES[stuck.hypothesis].to_string();
                    let nudge = stuck.nudge(&agent.ctf.category);
                    ui.push(
                        Speaker::System,
                        nudge.lines().next().unwrap_or_default().to_string(),
                    );
                    crate::autopilot::append_note(
                        &root,
                        &format!(
                            "- stuck nudge @ step {steps}: hypothesis {}",
                            crate::autopilot::CATEGORIES[stuck.hypothesis]
                        ),
                    );
                    brief = format!("{nudge}\n\n{brief}");
                }
                if let Some(reason) = options.budget.exhausted(steps, tokens, started.elapsed()) {
                    stop_reason = Some(format!("budget exhausted ({reason})"));
                    break;
                }
                brief = format!(
                    "Continue. {} step(s) used, {} tokens spent, budget {} steps / {} tokens / {} seconds.\n{brief}",
                    steps,
                    tokens,
                    options.budget.steps,
                    options.budget.tokens,
                    options.budget.seconds,
                );
            }
            let tokens = agent.usage.input + agent.usage.output;
            let hits = agent.ctf.recorded_hits();
            if let Some(reason) = stop_reason {
                if let Some(candidate) = crate::autopilot::candidate_note(&agent.ctf, &hits) {
                    if last_candidate.as_deref() != Some(candidate.as_str()) {
                        ui.push(Speaker::System, format!("⚠ {candidate}"));
                    }
                }
                let report = crate::autopilot::stop_report(&root, &options, steps, tokens, &reason);
                ui.push(Speaker::System, report);
            }
            sync_session(session, agent, ui);
            if session.summary == "New session" {
                session.summary = format!("ctf {}", options.target);
            }
            let flags: Vec<String> = hits.iter().map(|hit| hit.flag.clone()).collect();
            let notes = std::fs::read_to_string(crate::autopilot::notes_path(&root)).ok();
            match crate::report::writeup_challenge(&root, session, &flags, notes.as_deref()) {
                Ok(path) => ui.push(Speaker::System, format!("Writeup: {}", path.display())),
                Err(error) => ui.push(Speaker::System, format!("Writeup failed: {error:#}")),
            }
        }
        "/compact" => {
            ui.status = "Compacting context".into();
            ui.render()?;
            let outcome = agent.compact("/compact").await;
            ui.status = "Ready".into();
            match outcome {
                Ok(message) => ui.push(Speaker::System, message),
                Err(error) => ui.push(Speaker::System, format!("compaction failed: {error:#}")),
            }
        }
        "/diff" => {
            let diff = project::diff(&agent.config.root)?;
            ui.push(
                Speaker::System,
                if diff.is_empty() {
                    "No uncommitted changes".into()
                } else {
                    diff
                },
            );
        }
        "/review" => {
            let diff = project::diff(&agent.config.root)?;
            if diff.is_empty() {
                ui.push(Speaker::System, "No uncommitted changes to review");
            } else {
                ui.status = "Reviewing diff".into();
                ui.render()?;
                let result = agent.review(&diff).await?;
                ui.status = "Ready".into();
                ui.push(Speaker::Agent, result);
            }
        }
        "/commit" => {
            let diff = project::diff(&agent.config.root)?;
            if diff.is_empty() {
                ui.push(Speaker::System, "Nothing to commit");
            } else {
                ui.status = "Writing commit message".into();
                ui.render()?;
                let suggestion = agent
                    .commit_message(&diff)
                    .await
                    .unwrap_or(project::suggested_commit(&agent.config.root)?);
                ui.status = "Ready".into();
                ui.push(
                    Speaker::System,
                    format!(
                        "Commit summary:\n{}\nSuggested message: {suggestion}",
                        diff.lines().take(25).collect::<Vec<_>>().join("\n")
                    ),
                );
                if let Some(message) =
                    ask_line(ui, "Commit message (edit or Esc to cancel)", &suggestion)?
                {
                    if !message.is_empty() {
                        let confirmation = ask_line(ui, "Type YES to stage and commit", "")?;
                        if confirmation.as_deref() == Some("YES") {
                            ui.push(
                                Speaker::System,
                                project::commit(&agent.config.root, &message)?,
                            );
                        }
                    }
                }
            }
        }
        "/init" => ui.push(Speaker::System, project::init(&agent.config.root)?),
        "/issues" => {
            let issues = project::issues(&agent.config.root, &agent.tools.client).await?;
            ui.push(
                Speaker::System,
                if issues.is_empty() {
                    "No open issues".into()
                } else {
                    issues.join("\n")
                },
            );
        }
        "/rmslop" => {
            let created = agent
                .tools
                .created_files
                .lock()
                .map_err(|_| anyhow::anyhow!("file tracking lock poisoned"))?
                .clone();
            let referenced = agent
                .tools
                .referenced_files
                .lock()
                .map_err(|_| anyhow::anyhow!("reference tracking lock poisoned"))?
                .clone();
            let candidates = project::slop_candidates(&agent.config.root, &created, &referenced)?;
            if candidates.is_empty() {
                ui.push(Speaker::System, "No agent-created scratch files found");
            } else {
                ui.push(
                    Speaker::System,
                    format!(
                        "Candidates for deletion:\n{}",
                        candidates
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                );
                if ask_line(ui, "Type DELETE to remove listed paths", "")?.as_deref()
                    == Some("DELETE")
                {
                    for path in &candidates {
                        if path.is_dir() {
                            std::fs::remove_dir(path)?;
                        } else {
                            std::fs::remove_file(path)?;
                        }
                    }
                    ui.push(
                        Speaker::System,
                        format!("Removed {} paths", candidates.len()),
                    );
                }
            }
        }
        "/undo" => match crate::snapshot::undo(&agent.config.root) {
            Ok(message) => {
                agent.tools.clear_reads()?;
                ui.push(Speaker::System, message);
            }
            Err(error) => {
                let (undo, redo) = crate::snapshot::counts(&agent.config.root);
                ui.push(
                    Speaker::System,
                    format!("{error} ({undo} undo / {redo} redo on the stack)"),
                );
            }
        },
        "/redo" => match crate::snapshot::redo(&agent.config.root) {
            Ok(message) => {
                agent.tools.clear_reads()?;
                ui.push(Speaker::System, message);
            }
            Err(error) => {
                let (undo, redo) = crate::snapshot::counts(&agent.config.root);
                ui.push(
                    Speaker::System,
                    format!("{error} ({undo} undo / {redo} redo on the stack)"),
                );
            }
        },
        "/add" => {
            if args.is_empty() {
                let list = if agent.pinned.is_empty() {
                    "No pinned files. Use /add <path> to pin one.".to_string()
                } else {
                    format!("Pinned files:\n{}", agent.pinned.join("\n"))
                };
                ui.push(Speaker::System, list);
            } else {
                for path in args.split_whitespace() {
                    match agent.pin(path) {
                        Ok(message) => ui.push(Speaker::System, message),
                        Err(error) => ui.push(Speaker::System, format!("{path}: {error:#}")),
                    }
                }
            }
        }
        "/drop" => {
            if args.is_empty() {
                ui.push(
                    Speaker::System,
                    "Usage: /drop <path> or /drop all".to_string(),
                );
            } else {
                match agent.unpin(args) {
                    Ok(message) => ui.push(Speaker::System, message),
                    Err(error) => ui.push(Speaker::System, format!("{error:#}")),
                }
            }
        }
        "/recipe" => {
            let mut recipes = crate::recipe::discover(&agent.config.root)?;
            if recipes.is_empty() {
                ui.push(
                    Speaker::System,
                    "No recipes found (create .wrosecode/recipes/<name>.yaml or .md)".to_string(),
                );
            } else {
                let picked = if args.is_empty() {
                    let labels = recipes
                        .iter()
                        .map(|plan| format!("{}  {}", plan.name, plan.description))
                        .collect::<Vec<_>>();
                    picker(ui, "Recipes", labels)?.map(|index| recipes.remove(index))
                } else {
                    Some(crate::recipe::find(&agent.config.root, args)?)
                };
                let Some(plan) = picked else {
                    return Ok(());
                };
                // Ask for the parameters that have no default, then let
                // `resolve` fill the rest in.
                let mut values = std::collections::BTreeMap::new();
                for name in plan.required_params() {
                    let label = match plan.params.iter().find(|(key, _)| *key == name) {
                        Some((_, def)) if !def.description.is_empty() => {
                            format!("{name} ({})", def.description)
                        }
                        _ => name.clone(),
                    };
                    match ask_line(ui, &label, "")? {
                        Some(answer) if !answer.trim().is_empty() => {
                            values.insert(name, answer);
                        }
                        _ => {
                            ui.push(Speaker::System, "Recipe cancelled".to_string());
                            return Ok(());
                        }
                    }
                }
                let values = crate::recipe::resolve(&plan, &values)?;
                ui.push(
                    Speaker::System,
                    format!("Running recipe {} ({})", plan.name, plan.origin.display()),
                );
                let mut prev = String::new();
                let mut steps_context = String::new();
                for (position, step) in plan.steps.iter().enumerate() {
                    let mut vars = values.clone();
                    vars.insert("prev".to_string(), prev.clone());
                    vars.insert("steps".to_string(), steps_context.clone());
                    match step {
                        crate::recipe::Step::Prompt { prompt } => {
                            let text = crate::recipe::substitute(prompt, &vars);
                            if !run_turn_queue(agent, ui, &text, events, permissions).await? {
                                ui.push(Speaker::System, "Recipe stopped".to_string());
                                return Ok(());
                            }
                            prev = ui
                                .entries
                                .iter()
                                .rev()
                                .find(|entry| matches!(entry.speaker, Speaker::Agent))
                                .map(|entry| entry.text.clone())
                                .unwrap_or_default();
                            crate::recipe::push_step_context(
                                &mut steps_context,
                                "prompt",
                                position + 1,
                                &prev,
                            );
                        }
                        crate::recipe::Step::Command { command } => {
                            let text = crate::recipe::substitute(command, &vars);
                            ui.push(Speaker::Tool, format!("$ {text}"));
                            let output = match crate::tools::shell::run_with_timeout(
                                &text,
                                &agent.config.root,
                                agent.config.shell_timeout_seconds,
                            )
                            .await
                            {
                                Ok(output) => output,
                                Err(error) => format!("{error:#}"),
                            };
                            ui.push(Speaker::Tool, output.clone());
                            prev = output.clone();
                            crate::recipe::push_step_context(
                                &mut steps_context,
                                "command",
                                position + 1,
                                &output,
                            );
                        }
                    }
                }
                save_session(session, agent, ui, &session_dir)?;
                ui.push(Speaker::System, format!("Recipe {} finished", plan.name));
            }
        }
        "/new" => {
            save_session(session, agent, ui, &session_dir)?;
            *session = Session::fresh();
            agent.messages.clear();
            agent.pinned.clear();
            ui.entries.clear();
            ui.push(Speaker::System, "New session started");
        }
        "/move" => {
            if let Some(new_name) = ask_line(ui, "New session name", args)? {
                save_session(session, agent, ui, &session_dir)?;
                session.rename(&session_dir, &new_name)?;
                ui.push(
                    Speaker::System,
                    format!("Session renamed to {}", session.name),
                );
            }
        }
        "/sessions" | "/resume" => {
            save_session(session, agent, ui, &session_dir)?;
            let sessions = Session::list(&session_dir)?;
            let labels = sessions
                .iter()
                .map(|s| format!("{}  {}  {}", s.created, s.name, s.summary))
                .collect();
            if let Some(index) = picker(ui, "Sessions", labels)? {
                *session = sessions[index].clone();
                restore_session(session, agent, ui, settings)?;
                ui.push(Speaker::System, format!("Resumed {}", session.name));
            }
        }
        "/fork" => {
            // Keep the current session on disk, then continue in a copy of
            // it: both sides share the history up to this point and diverge
            // from here.
            save_session(session, agent, ui, &session_dir)?;
            let parent = session.name.clone();
            let mut forked = Session::fresh();
            forked.parent = Some(parent.clone());
            forked.summary = format!("Fork of {parent}");
            forked.messages = session.messages.clone();
            forked.transcript = session.transcript.clone();
            forked.pinned = session.pinned.clone();
            forked.provider_name = session.provider_name.clone();
            forked.model = session.model.clone();
            if !args.is_empty() {
                if let Err(error) = forked.rename(&session_dir, args) {
                    ui.push(Speaker::System, format!("{error:#}"));
                    return Ok(());
                }
            }
            *session = forked;
            save_session(session, agent, ui, &session_dir)?;
            ui.push(
                Speaker::System,
                format!("Forked {parent} into {}", session.name),
            );
        }
        "/tree" => {
            save_session(session, agent, ui, &session_dir)?;
            let sessions = Session::list(&session_dir)?;
            let lines = Session::tree(&sessions, &session.name);
            if lines.is_empty() {
                ui.push(Speaker::System, "No saved sessions yet".to_string());
            } else {
                ui.push(Speaker::System, lines.join("\n"));
            }
        }
        "/coverage" => {
            let root = agent.config.root.clone();
            let mut coverage = crate::coverage::Coverage::load(&root)?;
            if let Some(text) = args.strip_prefix("add ") {
                if coverage.add(text) {
                    coverage.save(&root)?;
                    ui.push(Speaker::System, format!("Added. {}", coverage.render()));
                } else {
                    ui.push(
                        Speaker::System,
                        format!("Already tracked. {}", coverage.render()),
                    );
                }
            } else if let Some(text) = args.strip_prefix("done ") {
                let index = coverage.find(text)?;
                let item = coverage.set_done(index, true).clone();
                coverage.save(&root)?;
                ui.push(Speaker::System, format!("Checked off: {}", item.text));
            } else if let Some(text) = args.strip_prefix("undone ") {
                let index = coverage.find(text)?;
                let item = coverage.set_done(index, false).clone();
                coverage.save(&root)?;
                ui.push(Speaker::System, format!("Reopened: {}", item.text));
            } else if coverage.items.is_empty() {
                ui.push(Speaker::System, coverage.render());
                ui.push(
                    Speaker::System,
                    "Add items with /coverage add <item>".to_string(),
                );
            } else {
                // Toggle items until the picker is dismissed; state lands in
                // .wrosecode/coverage.json either way.
                loop {
                    ui.push(Speaker::System, coverage.render());
                    let Some(index) = picker(
                        ui,
                        "Coverage (enter toggles, esc closes)",
                        coverage.labels(),
                    )?
                    else {
                        break;
                    };
                    coverage.toggle(index);
                    coverage.save(&root)?;
                }
            }
        }
        "/skills" if args == "list" || args.starts_with("list ") => {
            let packages = package::list(&skills::default_skills_dir());
            if packages.is_empty() {
                ui.push(Speaker::System, "No skill packages installed.");
            } else {
                let rows = packages
                    .iter()
                    .map(|(name, pkg)| {
                        let files = if pkg.files == 1 {
                            "1 file".to_string()
                        } else {
                            format!("{} files", pkg.files)
                        };
                        format!(
                            "{}  {}  {}  {} skill(s)\n      from {}",
                            name,
                            pkg.version,
                            files,
                            pkg.skills.len(),
                            pkg.source
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.push(Speaker::System, format!("Installed packages:\n{rows}"));
            }
        }
        "/skills" if args.starts_with("install ") => {
            let source = args["install ".len()..].trim();
            if source.is_empty() {
                ui.push(
                    Speaker::System,
                    "Usage: /skills install <git-url[#ref] | path>",
                );
            } else {
                match package::install(&skills::default_skills_dir(), source).await {
                    Ok(names) => {
                        agent.skills =
                            skills::discover(&agent.config.root, &agent.config.skill_dirs)?;
                        ui.push(
                            Speaker::System,
                            format!("Installed {source}: {}", names.join(", ")),
                        );
                    }
                    Err(error) => ui.push(Speaker::System, format!("Install failed: {error:#}")),
                }
            }
        }
        "/skills" if args.starts_with("uninstall ") => {
            let name = args["uninstall ".len()..].trim();
            if name.is_empty() {
                ui.push(Speaker::System, "Usage: /skills uninstall <package>");
            } else {
                match package::uninstall(&skills::default_skills_dir(), name) {
                    Ok(entry) => {
                        agent.skills =
                            skills::discover(&agent.config.root, &agent.config.skill_dirs)?;
                        ui.push(
                            Speaker::System,
                            format!("Uninstalled {name} ({})", entry.skills.join(", ")),
                        );
                    }
                    Err(error) => ui.push(Speaker::System, format!("{error:#}")),
                }
            }
        }
        "/skills" => {
            let mut skills: Vec<_> = agent.skills.values().cloned().collect();
            skills.sort_by(|a, b| a.name.cmp(&b.name));
            let labels = skills
                .iter()
                .map(|s| {
                    format!(
                        "{}  {}  {}",
                        s.name,
                        if s.fork { "fork" } else { "inline" },
                        s.description
                    )
                })
                .collect();
            if let Some(index) = picker(ui, "Skills", labels)? {
                agent.active_skill = Some(skills[index].name.clone());
                ui.push(
                    Speaker::System,
                    format!("Skill active: {}", skills[index].name),
                );
            }
        }
        "/providers" if args == "add" => {
            connect(agent, ui, settings).await?;
        }
        "/connect" => {
            connect(agent, ui, settings).await?;
        }
        "/providers" => {
            provider_screen(agent, ui, settings).await?;
        }
        "/model" => {
            let model = if args.is_empty() {
                ask_line(ui, "Model", &agent.config.model)?
            } else {
                Some(args.into())
            };
            if let Some(model) = model {
                switch_provider(agent, ui, settings, &agent.config.provider.clone(), &model)?;
            }
        }
        "/models" => {
            if !agent.config.planner_model.is_empty() {
                ui.push(
                    Speaker::System,
                    format!(
                        "planner (plan mode): {}/{} — worker: {}/{}",
                        agent.config.planner_provider,
                        agent.config.planner_model,
                        agent.config.provider,
                        agent.config.model
                    ),
                );
            }
            let models = available_models(agent, settings).await;
            let labels = models
                .iter()
                .map(|(name, model)| format!("{name}  /  {model}"))
                .collect();
            if let Some(index) = picker(ui, "Models", labels)? {
                switch_provider(agent, ui, settings, &models[index].0, &models[index].1)?;
            }
        }
        "/mcps" | "/mcp" => {
            mcp_command(args, agent, ui, settings).await?;
        }
        "/editor" => {
            let path =
                std::env::temp_dir().join(format!("wrosecode-prompt-{}.txt", std::process::id()));
            std::fs::write(&path, "")?;
            execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
            terminal::disable_raw_mode()?;
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let parts: Vec<_> = editor.split_whitespace().collect();
            let status = std::process::Command::new(parts[0])
                .args(&parts[1..])
                .arg(&path)
                .status();
            terminal::enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
            ui.renderer.size = (0, 0);
            status?
                .success()
                .then_some(())
                .ok_or_else(|| anyhow::anyhow!("editor exited unsuccessfully"))?;
            let prompt = std::fs::read_to_string(&path)?;
            let _ = std::fs::remove_file(path);
            if !prompt.trim().is_empty() {
                ui.scroll_end();
                ui.push(Speaker::User, prompt.clone());
                run_prompt(
                    agent,
                    ui,
                    &prompt,
                    events,
                    permissions,
                    agent.config.smooth_scroll_lines,
                )
                .await?;
                save_session(session, agent, ui, &session_dir)?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn connect(agent: &mut Agent, ui: &mut Ui, settings: &mut Settings) -> Result<()> {
    provider_form(None, agent, ui, settings).await
}

fn ask_provider_name(ui: &mut Ui, settings: &Settings) -> Result<Option<String>> {
    ui.palette = false;
    close_picker(ui);
    ui.set_input(String::new());
    loop {
        let valid =
            Settings::validate_name(&ui.input).is_ok() && settings.profile(&ui.input).is_none();
        ui.status = if valid {
            "Provider name valid • Enter accept • Esc cancel".into()
        } else {
            "Name must be unique and use a-z, 0-9, - or _".into()
        };
        ui.render()?;
        let event = event::read()?;
        if let Event::Paste(text) = &event {
            for ch in text.chars() {
                if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_' {
                    ui.insert(ch);
                }
            }
            continue;
        }
        if let Event::Key(key) = event {
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Esc => {
                    ui.set_input(String::new());
                    ui.status = "Ready".into();
                    return Ok(None);
                }
                KeyCode::Enter if valid => {
                    ui.status = "Ready".into();
                    return Ok(Some(ui.take_input()));
                }
                KeyCode::Backspace | KeyCode::Delete => ui.backspace(),
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    ui.backspace()
                }
                KeyCode::Char(ch)
                    if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_' =>
                {
                    ui.insert(ch)
                }
                _ => {}
            }
        }
    }
}

async fn provider_form(
    existing: Option<ProviderProfile>,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
) -> Result<()> {
    loop {
        let Some(name) = (if let Some(profile) = &existing {
            Some(profile.name.clone())
        } else {
            ask_provider_name(ui, settings)?
        }) else {
            return Ok(());
        };
        Settings::validate_name(&name)?;
        if existing.is_none() && settings.profile(&name).is_some() {
            anyhow::bail!("provider {name} already exists");
        }
        let previous = existing.clone();
        let default_url = previous.as_ref().map(|p| p.base_url.as_str()).unwrap_or("");
        let Some(base_url) = ask_line(ui, "Base URL", default_url)? else {
            return Ok(());
        };
        let base_url = Settings::normalize_url(&base_url)?;
        if base_url.is_empty() {
            anyhow::bail!("base URL is required");
        }
        if Settings::insecure_http(&base_url) {
            ui.push(
                Speaker::System,
                "Warning: HTTP sends API keys without TLS to a non-local host",
            );
        }
        let current_key = previous.as_ref().and_then(|profile| settings.key(profile));
        ui.status = format!(
            "Key: {}  •  Enter to keep",
            current_key
                .as_ref()
                .map(|key| crate::settings::redact(key))
                .unwrap_or_else(|| "missing".into())
        );
        ui.render()?;
        let Some(entered_key) =
            ask_secret(ui, "API key (empty keeps current; local gateways may omit)")?
        else {
            return Ok(());
        };
        let key = if entered_key.is_empty() {
            current_key
        } else if let Some(env) = entered_key.strip_prefix("env:") {
            std::env::var(env).ok().filter(|value| !value.is_empty())
        } else {
            Some(entered_key.clone())
        };
        let kind = if let Some(profile) = &previous {
            profile.kind.clone()
        } else {
            let Some(kind) = ask_line(ui, "Kind (openai_compat or anthropic)", "openai_compat")?
            else {
                return Ok(());
            };
            kind
        };
        if !matches!(kind.as_str(), "openai_compat" | "anthropic") {
            anyhow::bail!("kind must be openai_compat or anthropic");
        }
        let mut profile = ProviderProfile {
            name: name.clone(),
            kind,
            base_url,
            model: previous
                .as_ref()
                .map(|p| p.model.clone())
                .unwrap_or_default(),
            key_ref: previous
                .as_ref()
                .map(|p| p.key_ref.clone())
                .unwrap_or_default(),
            headers: previous
                .as_ref()
                .map(|p| p.headers.clone())
                .unwrap_or_default(),
            think: previous.as_ref().and_then(|p| p.think),
            think_map: previous.as_ref().and_then(|p| p.think_map.clone()),
            builtin: Settings::builtin(&name),
        };
        if key.is_none() && !settings.no_key_needed(&profile) && !entered_key.starts_with("env:") {
            anyhow::bail!("API key is required for {name}");
        }
        let suggested = if profile.model.is_empty() {
            if let Ok(provider) =
                provider::create_profile(&profile, key.clone(), "", agent.tools.client.clone())
            {
                let found = tokio::time::timeout(Duration::from_secs(5), provider.list_models())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                if found.is_empty() {
                    String::new()
                } else if let Some(index) = picker(ui, "Available models", found.clone())? {
                    found[index].clone()
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            profile.model.clone()
        };
        let Some(model) = ask_line(ui, "Default model (optional)", &suggested)? else {
            return Ok(());
        };
        profile.model = model;
        if previous.is_none() {
            loop {
                let Some(header) = ask_line(ui, "Extra header K=V (blank to finish)", "")? else {
                    return Ok(());
                };
                if header.is_empty() {
                    break;
                }
                let (name, value) = header
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("header must be K=V"))?;
                if name.is_empty()
                    || matches!(
                        name.to_ascii_lowercase().as_str(),
                        "authorization" | "x-api-key" | "proxy-authorization"
                    )
                {
                    anyhow::bail!("use the API key field for auth headers");
                }
                profile.headers.insert(name.into(), value.into());
            }
        }
        let result = if let Ok(provider) = provider::create_profile(
            &profile,
            key.clone(),
            &profile.model,
            agent.tools.client.clone(),
        ) {
            tokio::time::timeout(Duration::from_secs(15), provider.test())
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("connection timed out")))
        } else {
            Err(anyhow::anyhow!("provider setup is invalid"))
        };
        if let Err(error) = &result {
            ui.push(Speaker::System, format!("Connection test failed: {error}"));
            let choice = ask_line(
                ui,
                "Type SAVE to save anyway, EDIT to retry, Esc to cancel",
                "",
            )?;
            match choice.as_deref() {
                Some("EDIT") => continue,
                Some("SAVE") => {}
                _ => return Ok(()),
            }
        }
        settings.upsert_provider(profile.clone())?;
        if !entered_key.is_empty() {
            settings.set_key(&name, &entered_key)?;
        }
        settings.last_tests.insert(
            name.clone(),
            result.as_ref().map(|d| *d).map_err(|e| e.to_string()),
        );
        if let Ok(latency) = result {
            ui.push(
                Speaker::System,
                format!("{name}: connected ({} ms)", latency.as_millis()),
            );
        } else {
            ui.push(
                Speaker::System,
                format!("Saved {name}; connection needs attention"),
            );
        }
        if !profile.model.is_empty() {
            switch_provider(agent, ui, settings, &name, &profile.model)?;
        }
        ui.status = format!(
            "{name} key {}",
            settings
                .profile(&name)
                .and_then(|profile| settings.key(profile))
                .as_deref()
                .map(crate::settings::redact)
                .unwrap_or_else(|| "none".into())
        );
        return Ok(());
    }
}

async fn provider_screen(agent: &mut Agent, ui: &mut Ui, settings: &mut Settings) -> Result<()> {
    ui.set_input(String::new());
    ui.selected = 0;
    loop {
        let matches: Vec<_> = settings
            .providers
            .iter()
            .filter(|profile| profile.name.contains(&ui.input.to_ascii_lowercase()))
            .cloned()
            .collect();
        let mut rows = vec!["+ Add custom provider".to_string()];
        rows.extend(matches.iter().map(|profile| {
            format!(
                "{} {:<18} {:<14} {:<42} {}",
                if agent.config.provider == profile.name {
                    "●"
                } else {
                    "○"
                },
                profile.name,
                profile.kind,
                profile.base_url,
                settings.status(profile)
            )
        }));
        ui.selected = ui.selected.min(rows.len().saturating_sub(1));
        ui.picker = Some(("Providers".into(), rows));
        ui.picker_all = None;
        ui.picker_query.clear();
        ui.status = "Enter select · a add · e edit · d delete · t test · Esc back".into();
        ui.render()?;
        let event = event::read()?;
        if let Event::Paste(text) = &event {
            for ch in text.chars() {
                if ch != '\n' && ch != '\r' {
                    ui.insert(ch);
                }
            }
            ui.selected = 0;
            continue;
        }
        if let Event::Key(key) = event {
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            let selected = ui
                .selected
                .checked_sub(1)
                .and_then(|index| matches.get(index))
                .cloned();
            match key.code {
                KeyCode::Esc => {
                    ui.picker = None;
                    ui.set_input(String::new());
                    ui.status = "Ready".into();
                    return Ok(());
                }
                KeyCode::Up => ui.selected = ui.selected.saturating_sub(1),
                KeyCode::Down => ui.selected = (ui.selected + 1).min(matches.len()),
                KeyCode::PageUp => ui.selected = ui.selected.saturating_sub(5),
                KeyCode::PageDown => ui.selected = (ui.selected + 5).min(matches.len()),
                KeyCode::Backspace | KeyCode::Delete => {
                    ui.backspace();
                    ui.selected = 0;
                }
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    ui.backspace();
                    ui.selected = 0;
                }
                KeyCode::Char('a') if ui.input.is_empty() => {
                    ui.picker = None;
                    provider_form(None, agent, ui, settings).await?;
                }
                KeyCode::Char('e') if ui.input.is_empty() => {
                    if let Some(profile) = selected {
                        ui.picker = None;
                        provider_form(Some(profile), agent, ui, settings).await?;
                    }
                }
                KeyCode::Char('t') if ui.input.is_empty() => {
                    if let Some(profile) = selected {
                        ui.picker = None;
                        match crate::provider_cli::test_one(
                            settings,
                            &profile.name,
                            &agent.tools.client,
                        )
                        .await
                        {
                            Ok(latency) => ui.push(
                                Speaker::System,
                                format!("{} connected in {} ms", profile.name, latency.as_millis()),
                            ),
                            Err(error) => {
                                ui.push(Speaker::System, format!("{}: {error}", profile.name))
                            }
                        }
                    }
                }
                KeyCode::Char('d') if ui.input.is_empty() => {
                    if let Some(profile) = selected {
                        if profile.builtin {
                            ui.push(Speaker::System, "Built-in providers cannot be deleted");
                        } else {
                            let fallback = settings
                                .providers
                                .iter()
                                .find(|other| {
                                    other.name != profile.name
                                        && !other.model.is_empty()
                                        && !other.base_url.is_empty()
                                        && (settings.key(other).is_some()
                                            || settings.no_key_needed(other))
                                })
                                .cloned();
                            if agent.config.provider == profile.name && fallback.is_none() {
                                ui.push(
                                    Speaker::System,
                                    "Connect another provider before deleting the active one",
                                );
                                continue;
                            }
                            ui.picker = None;
                            if ask_line(ui, &format!("Type DELETE to remove {}", profile.name), "")?
                                .as_deref()
                                == Some("DELETE")
                            {
                                settings.remove_provider(&profile.name)?;
                                if agent.config.provider == profile.name {
                                    let fallback = fallback.expect("checked above");
                                    switch_provider(
                                        agent,
                                        ui,
                                        settings,
                                        &fallback.name,
                                        &fallback.model,
                                    )?;
                                }
                                ui.push(Speaker::System, format!("Removed {}", profile.name));
                            }
                        }
                    }
                }
                KeyCode::Enter if ui.selected == 0 => {
                    ui.picker = None;
                    provider_form(None, agent, ui, settings).await?;
                }
                KeyCode::Enter => {
                    if let Some(profile) = selected {
                        ui.picker = None;
                        provider_form(Some(profile), agent, ui, settings).await?;
                    }
                }
                KeyCode::Char(ch) => {
                    ui.insert(ch);
                    ui.selected = 0;
                }
                _ => {}
            }
        }
    }
}

async fn mcp_command(
    args: &str,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
) -> Result<()> {
    if let Some(name) = args.strip_prefix("remove ") {
        settings.remove_mcp(name.trim())?;
        agent.tools.mcps.retain(|(server, _)| server != name.trim());
        ui.push(
            Speaker::System,
            format!("Removed MCP server {}", name.trim()),
        );
        return Ok(());
    }
    if args == "add" {
        let Some(name) = ask_line(ui, "MCP server name", "")? else {
            return Ok(());
        };
        let Some(url) = ask_line(ui, "MCP endpoint URL (blank = local executable)", "")? else {
            return Ok(());
        };
        let def = if url.trim().is_empty() {
            let Some(bin) = ask_line(ui, "MCP executable", "")? else {
                return Ok(());
            };
            let Some(arguments) = ask_line(ui, "MCP arguments (space separated)", "")? else {
                return Ok(());
            };
            McpServerDef {
                name: name.clone(),
                bin,
                args: arguments.split_whitespace().map(str::to_owned).collect(),
                url: None,
                headers: std::collections::BTreeMap::new(),
            }
        } else {
            McpServerDef {
                name: name.clone(),
                bin: String::new(),
                args: Vec::new(),
                url: Some(url.trim().to_string()),
                headers: std::collections::BTreeMap::new(),
            }
        };
        let mcp = crate::tools::mcp::Mcp::connect_def(&def).await?;
        settings.upsert_mcp(def)?;
        agent.tools.mcps.retain(|(server, _)| server != &name);
        agent.tools.mcps.push((name.clone(), mcp));
        ui.push(Speaker::System, format!("Connected MCP server {name}"));
        return Ok(());
    }
    let mut lines = Vec::new();
    for def in &settings.mcps {
        if let Some((_, mcp)) = agent.tools.mcps.iter().find(|(name, _)| name == &def.name) {
            lines.push(format!(
                "{}: connected ({}) — {}",
                def.name,
                def.endpoint(),
                mcp.schemas
                    .iter()
                    .filter_map(|s| s["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            lines.push(format!("{}: disconnected", def.name));
        }
    }
    for (name, mcp) in &agent.tools.mcps {
        if !settings.mcps.iter().any(|def| &def.name == name) {
            lines.push(format!(
                "{name}: connected — {}",
                mcp.schemas
                    .iter()
                    .filter_map(|s| s["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    ui.push(
        Speaker::System,
        if lines.is_empty() {
            "No MCP servers. Use /mcps add.".into()
        } else {
            lines.join("\n")
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formatting_stays_short() {
        assert_eq!(format_duration_ms(320), "320ms");
        assert_eq!(format_duration_ms(8_400), "8.4s");
        assert_eq!(format_duration_ms(125_000), "2m5s");
    }

    #[test]
    fn thinking_tiers_map_to_worker_counts() {
        assert_eq!(speed_tier(0), "TURBO");
        assert_eq!(speed_tier(5), "FAST");
        assert_eq!(speed_tier(9), "SMART");
        assert_eq!(speed_tier(15), "DEEP");
        assert_eq!(level_workers(0), 1);
        assert_eq!(level_workers(5), 3);
        assert_eq!(level_workers(9), 5);
        assert_eq!(level_workers(15), 10);
    }

    #[test]
    fn unknown_theme_falls_back_to_dark() {
        assert_eq!(theme_index("wrose-dark"), 0);
        assert_eq!(theme_index("dark"), 0);
        assert_eq!(theme_index("dracula"), 3);
        assert_eq!(theme_index("something-new"), 0);
    }

    #[test]
    fn theme_registry_is_stable_and_round_trips() {
        assert_eq!(
            theme_names(),
            ["dark", "light", "solarized", "dracula", "nord", "mono"]
        );
        for (position, name) in theme_names().iter().enumerate() {
            assert_eq!(theme_position(name), Some(position), "{name}");
            assert_eq!(theme_position(name).expect("registered"), position);
            assert_eq!(theme(position).name, *name);
        }
        assert_eq!(theme_position("nope"), None);
        assert_eq!(theme(THEMES.len()).name, "dark", "the cycle wraps");
    }

    #[test]
    fn mono_stays_inside_the_16_colour_baseline() {
        let mono = theme(theme_position("mono").expect("mono registered"));
        for (label, colour) in [
            ("text", mono.text),
            ("muted", mono.muted),
            ("accent", mono.accent),
            ("status", mono.status),
            ("background", mono.background),
        ] {
            assert!(
                !matches!(colour, Color::Rgb { .. } | Color::AnsiValue(_)),
                "mono {label} must be a named colour, got {colour:?}"
            );
        }
    }

    #[test]
    fn no_color_profile_disables_every_channel() {
        let mut forced = env("xterm-256color");
        forced.force_no_color = true;
        assert_eq!(
            TerminalProfile::decide(true, None, &forced).colors,
            ColorSupport::None
        );
        let mut truecolor = env("xterm-256color");
        truecolor.colorterm = "truecolor".into();
        assert_eq!(
            TerminalProfile::decide(true, None, &truecolor).colors,
            ColorSupport::TrueColor
        );
        assert_eq!(
            TerminalProfile::decide(true, None, &env("xterm-256color")).colors,
            ColorSupport::Ansi256
        );
        assert_eq!(
            TerminalProfile::decide(true, None, &env("xterm")).colors,
            ColorSupport::Ansi16
        );
    }

    #[test]
    fn gradient_paint_honours_colour_support() {
        let plain = gradient_paint("WROSECODE", ColorSupport::None);
        assert_eq!(plain, "WROSECODE", "NO_COLOR means no escapes");
        let ansi16 = gradient_paint("WROSECODE", ColorSupport::Ansi16);
        assert!(
            ansi16.starts_with("\x1b[36m") && ansi16.ends_with("\x1b[0m"),
            "the 16-colour fallback wraps the whole line: {ansi16:?}"
        );
        assert!(
            gradient_paint("WROSECODE", ColorSupport::Ansi256).contains("38;5;"),
            "the 256-colour cube speaks its own dialect"
        );
        assert!(
            gradient_paint("WROSECODE", ColorSupport::TrueColor).contains("38;2;"),
            "truecolor gets per-channel codes"
        );
    }

    #[test]
    fn wordmark_gradient_tracks_the_profile() {
        let mut ui = shell();
        ui.colors = ColorSupport::TrueColor;
        let (frame, _) = ui.compose(100, 30);
        let colours = frame[0]
            .colors
            .as_ref()
            .expect("truecolor splashes the wordmark");
        assert_eq!(colours.len(), frame[0].text.chars().count());
        ui.no_color = true;
        ui.colors = ColorSupport::None;
        let (frame, _) = ui.compose(100, 30);
        for (index, row) in frame.iter().take(splash::LOGO.len()).enumerate() {
            assert!(
                row.colors.is_none(),
                "row {index} stays uncoloured under NO_COLOR"
            );
        }
    }

    #[test]
    fn narrow_splash_collapses_the_wordmark() {
        let wide = splash::paint_lines(100, ColorSupport::Ansi256, None);
        assert!(
            wide.len() > splash::LOGO.len(),
            "the wide splash keeps art plus tails: {} rows",
            wide.len()
        );
        assert!(wide[0].contains("38;5;"), "the art is gradient-painted");
        let narrow = splash::paint_lines(79, ColorSupport::Ansi256, None);
        assert_eq!(narrow.len(), 1, "one line below 80 columns");
        assert!(
            strip_sgr(&narrow[0]).contains("WROSECODE v"),
            "the collapsed wordmark keeps the version: {:?}",
            narrow[0]
        );
        let muted = splash::paint_lines(79, ColorSupport::None, Some(3));
        assert!(
            !muted[0].contains('\x1b'),
            "NO_COLOR splash is plain text: {:?}",
            muted[0]
        );
        assert!(
            muted.iter().any(|row| row.contains("first paint 3ms")),
            "the timing line survives: {muted:?}"
        );
    }

    #[test]
    fn info_panel_reports_health_tips_and_ctf() {
        let mut ui = shell();
        let (frame, _) = ui.compose(120, 40);
        let joined = frame
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "/providers",
            "tip ·",
            "ctf",
            "sandbox",
            "think ",
            "mcp",
            "skills",
        ] {
            assert!(
                joined.contains(needle),
                "info panel shows {needle:?}:\n{joined}"
            );
        }
    }

    #[test]
    fn status_bar_leads_with_the_run_state_and_key_fields() {
        let mut ui = shell();
        fill_transcript(&mut ui, 5);
        let (frame, _) = ui.compose(200, 40);
        let status = frame
            .iter()
            .map(|row| &row.text)
            .find(|text| text.contains("tok ") && text.contains("think:"))
            .expect("a status bar row");
        for needle in ["think:", "tok ", "step ", "/"] {
            assert!(status.contains(needle), "status shows {needle:?}: {status}");
        }
        let model = status
            .find(&ui.model)
            .expect("the model is named in the status bar");
        let tokens = status.find("tok ").expect("token counts");
        assert!(model < tokens, "model leads the line: {status}");
        assert!(
            status.starts_with(&format!(" {}", ui.status)),
            "the state word opens the line: {status}"
        );
    }

    #[test]
    fn think_label_reports_mode_level_and_provider_rejection() {
        let ui = shell();
        assert_eq!(ui.think_label(), "medium", "the shipped default level");
        assert_eq!(think_label(ThinkLevel::Auto, 10), "auto→high");
        assert_eq!(think_label(ThinkLevel::Off, 0), "off");
        assert_eq!(think_label(ThinkLevel::Max, 20), "max");
        let mut ui = ui;
        ui.think_ignored = true;
        assert_eq!(ui.think_label(), "medium (ignored)");
        ui.think = ThinkLevel::Auto;
        ui.thinking_level = 3;
        ui.think_ignored = false;
        assert_eq!(ui.think_label(), "auto→low");
    }

    #[test]
    fn think_transitions_land_in_the_transcript_and_status() {
        let mut ui = shell();
        ui.apply_progress(Progress::Think {
            from: "medium".into(),
            to: "high".into(),
            to_level: 10,
            reason: "no progress ×3".into(),
        });
        assert_eq!(
            ui.thinking_level, 10,
            "the live level follows the escalation"
        );
        let line = &ui.entries.last().expect("the transition is recorded").text;
        assert_eq!(line, "think: medium → high (no progress ×3)");
        ui.think = ThinkLevel::Auto;
        assert_eq!(ui.think_label(), "auto→high");

        ui.apply_progress(Progress::ThinkIgnored(
            "provider rejected reasoning_effort".into(),
        ));
        assert!(ui.think_ignored, "a rejected control marks the status bar");
        let line = &ui.entries.last().expect("the rejection is recorded").text;
        assert!(line.contains("think control ignored"), "{line}");
        assert!(
            ui.think_label().ends_with("(ignored)"),
            "{}",
            ui.think_label()
        );
    }

    #[test]
    fn dashboard_thinking_panel_reports_the_label_and_reasoning_line() {
        let mut ui = shell();
        ui.think = ThinkLevel::Auto;
        ui.thinking_level = 10;
        ui.last_turn_reasoning = 1280;
        ui.last_turn_model_ms = 4200;
        let stats = ui.dash_stats();
        assert_eq!(stats.think, "auto→high");
        assert_eq!(stats.turn_reasoning_tok, 1280);
        assert_eq!(stats.turn_model_ms, 4200);

        let panels = dashboard_panels(&stats, 100);
        let (_, thinking) = panels
            .iter()
            .find(|(title, _)| *title == "THINKING")
            .expect("the THINKING panel");
        assert!(
            thinking[0].contains("level 10/20 auto→high"),
            "{:?}",
            thinking[0]
        );
        assert!(
            thinking
                .iter()
                .any(|line| line.contains("last turn 4.2s · reasoning 1280 tok")),
            "{thinking:?}"
        );
    }

    #[test]
    fn tips_rotate_and_wrap() {
        let mut ui = shell();
        for _ in 0..TIPS.len() {
            ui.advance_tip();
        }
        assert_eq!(ui.tip_index, 0, "the tip list wraps");
        ui.advance_tip();
        assert_eq!(ui.tip_index, 1);
    }

    #[test]
    fn user_theme_files_load_and_reject_bad_ones() {
        let dir = std::env::temp_dir().join(format!("wrose-themes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("copper.toml"),
            "text = \"#e0e0e0\"\naccent = \"#d78700\"\nstatus = \"#afaf00\"\n",
        )
        .unwrap();
        // Bad hex, a shadow of a built-in name, and a non-toml file: all skip.
        std::fs::write(
            dir.join("broken.toml"),
            "text = \"nope\"\naccent = \"#112233\"\nstatus = \"#112233\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("shadow.toml"),
            "text = \"#ffffff\"\naccent = \"#000000\"\nstatus = \"#000000\"\nname = \"dark\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let loaded = load_theme_files(&dir);
        assert_eq!(loaded.len(), 1, "only the valid palette loads: {loaded:?}");
        assert_eq!(loaded[0].name, "copper", "file stem names the palette");
        assert_eq!(
            loaded[0].background,
            Color::Black,
            "an omitted background defaults to black"
        );
        let with_name = parse_theme(
            "stem",
            "name = \"copper\"\ntext = \"#e0e0e0\"\naccent = \"#d78700\"\nstatus = \"#afaf00\"\n",
        )
        .expect("a named palette parses");
        assert_eq!(with_name.name, "copper", "the name key wins over the stem");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wordmark_stays_quiet_until_something_has_painted() {
        let mut ui = shell();
        let (frame, _) = ui.compose(100, 30);
        assert!(
            frame[0].text.contains("session"),
            "row 0 is the art plus the session tail: {:?}",
            frame[0].text
        );
        assert!(
            frame.iter().any(|row| row.text.contains("WROSECODE v")),
            "the version line renders"
        );
        assert!(
            !frame[0].text.contains("paint"),
            "no timing note before a real first paint: {:?}",
            frame[0].text
        );
        assert!(splash::first_paint_ms().is_none(), "no test paints");
    }

    #[test]
    fn sparkline_degrades_gracefully() {
        assert_eq!(sparkline(&[], 10), "·");
        assert!(!sparkline(&[1, 2, 3], 4).is_empty());
    }

    fn shell() -> Ui {
        Ui::from_meta(UiMeta::for_test(), "test".into())
    }

    fn fill_transcript(ui: &mut Ui, entries: usize) {
        for index in 0..entries {
            ui.push(
                Speaker::User,
                format!("entry {index} padded with plenty of characters to wrap in narrow panes"),
            );
        }
    }

    /// Drop SGR sequences so assertions can read gradient-painted text.
    fn strip_sgr(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for next in chars.by_ref() {
                    if next == 'm' {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn scroll_helpers_clamp_and_track_follow_mode() {
        let mut ui = shell();
        assert!(ui.is_following());
        fill_transcript(&mut ui, 60);
        let (frame, _) = ui.compose(100, 30);
        assert_eq!(frame.len(), 30);
        assert!(ui.max_scroll > 0, "a long transcript must be scrollable");

        ui.scroll_up(5);
        assert_eq!(ui.scroll, 5);
        assert!(!ui.is_following());
        assert_eq!(
            ui.follow_marker(),
            " · ↓ 0 new lines  (End to jump)",
            "detached state advertises the jump hint (spec 1.1)"
        );

        // Output that lands while detached is counted for the indicator.
        ui.push(Speaker::System, "late arrival");
        assert_eq!(ui.new_since_detach, 2, "entry plus its separator line");
        assert_eq!(
            ui.follow_marker(),
            " · ↓ 2 new lines  (End to jump)",
            "the counter feeds the indicator"
        );

        ui.scroll_up(10_000);
        assert_eq!(ui.scroll, ui.max_scroll, "scroll_up clamps at the top");

        ui.scroll_down(10_000);
        assert!(ui.is_following(), "scroll_down clamps back to the bottom");

        ui.scroll_page(false);
        assert_eq!(ui.scroll, ui.page_size.min(ui.max_scroll));
        assert!(!ui.is_following());
        ui.scroll_page(true);
        assert!(ui.is_following(), "one page down returns to follow mode");

        ui.scroll_home();
        assert_eq!(ui.scroll, ui.max_scroll);
        ui.scroll_end();
        assert_eq!(ui.scroll, 0);
        assert_eq!(ui.follow_marker(), String::new());
        assert_eq!(
            ui.new_since_detach, 0,
            "End clears the new-lines counter along with the offset"
        );
    }

    #[test]
    fn ctrl_u_ctrl_d_scroll_half_a_page() {
        let mut ui = shell();
        fill_transcript(&mut ui, 60);
        ui.compose(100, 30);
        let page = ui.page_size;
        assert!(page > 4, "a full page must be bigger than a half page");

        ui.scroll_half_page(false);
        assert_eq!(ui.scroll, page.div_ceil(2).min(ui.max_scroll));
        ui.scroll_half_page(true);
        assert!(ui.is_following(), "half a page down returns to the bottom");
    }

    #[test]
    fn transcript_cache_only_rebuilds_when_the_wrapped_lines_change() {
        let mut ui = shell();
        fill_transcript(&mut ui, 40);
        ui.compose(80, 24);
        let first = ui.transcript_rebuilds;
        assert_eq!(first, 1);

        for _ in 0..5 {
            ui.scroll_up(3);
            ui.compose(80, 24);
            ui.scroll_end();
            ui.compose(80, 24);
        }
        assert_eq!(
            ui.transcript_rebuilds, first,
            "scrolling must reslice the cache, not re-wrap the transcript"
        );

        ui.push(Speaker::Agent, "fresh entry");
        ui.compose(80, 24);
        let after_entry = ui.transcript_rebuilds;
        assert!(after_entry > first, "new content invalidates the cache");

        ui.pane_percent = 50;
        ui.compose(80, 24);
        let after_width = ui.transcript_rebuilds;
        assert!(after_width > after_entry, "a different pane width re-wraps");

        ui.verbosity = "verbose".into();
        ui.compose(80, 24);
        assert!(
            ui.transcript_rebuilds > after_width,
            "verbosity changes the wrapped output"
        );
    }

    #[test]
    fn compose_writes_exact_rows_at_awkward_sizes() {
        for (width, height) in [(10u16, 4u16), (23, 7), (80, 24)] {
            let mut ui = shell();
            fill_transcript(&mut ui, 30);
            let (frame, cursor) = ui.compose(width, height);
            let height = height as usize;
            let width = width as usize;

            assert_eq!(frame.len(), height, "one row per terminal line");
            for (index, row) in frame.iter().enumerate() {
                assert!(
                    row.text.chars().count() <= width,
                    "row {index} of {width}x{height} overflows: {:?}",
                    row.text
                );
            }
            assert_eq!(frame[height - 1].text, "> ", "the prompt owns the last row");
            assert_eq!(
                frame[height - 2].text,
                "─".repeat(width),
                "the divider owns the second-to-last row"
            );
            assert!(
                frame[0].text.starts_with(" WROSECODE"),
                "row 0 is the slim header: {:?}",
                frame[0].text
            );
            assert_eq!(
                cursor,
                (2, (height - 1) as u16),
                "the caret sits in the empty prompt"
            );

            if height >= 7 {
                assert!(
                    frame[1].text.chars().all(|ch| ch == '─'),
                    "row 1 is the full-width rule: {:?}",
                    frame[1].text
                );
                assert!(
                    frame[2].text.starts_with(" TRANSCRIPT  31"),
                    "row 2 is the transcript header: {:?}",
                    frame[2].text
                );
            } else {
                // A four-row terminal is too short for a transcript: the
                // status line takes the slot under the mode header.
                assert!(
                    frame[1].text.starts_with(" Ready"),
                    "row 1 is the status line: {:?}",
                    frame[1].text
                );
            }
            if width >= 80 {
                // Following the bottom of a 30-entry transcript shows the
                // newest lines, not the oldest ones.
                assert!(
                    frame.iter().any(|row| row.text.contains("entry 29 padded")),
                    "the newest scrollback content renders at full size"
                );
            }
        }
    }

    #[test]
    fn compose_pins_scrolled_history_and_releases_the_draft() {
        let mut ui = shell();
        fill_transcript(&mut ui, 40);
        ui.draft = "streaming reply in progress".into();

        let (frame, _) = ui.compose(80, 24);
        assert!(
            frame
                .iter()
                .any(|row| row.text.contains("streaming reply in progress")),
            "the live draft is visible while following"
        );

        ui.scroll_up(4);
        let (frame, _) = ui.compose(80, 24);
        assert!(
            !frame
                .iter()
                .any(|row| row.text.contains("streaming reply in progress")),
            "a scrolled-up view stays put while the draft grows"
        );
        assert!(
            frame[2].text.contains("↓ 0 new lines  (End to jump)"),
            "the header advertises the jump hint while detached: {:?}",
            frame[2].text
        );

        ui.scroll_end();
        let (frame, _) = ui.compose(80, 24);
        assert!(
            frame
                .iter()
                .any(|row| row.text.contains("streaming reply in progress")),
            "returning to the bottom restores the draft"
        );
    }

    #[test]
    fn queued_messages_are_visible_in_the_status_line() {
        let mut ui = shell();
        fill_transcript(&mut ui, 10);
        ui.busy = true;
        ui.status = "Thinking".into();
        ui.pending.push("second request".into());
        let (frame, _) = ui.compose(80, 24);
        let status = &frame[21].text;
        assert!(status.contains("queued 1"), "status: {status}");
    }

    #[test]
    fn input_editing_matches_readline_expectations() {
        let mut ui = shell();
        for ch in "hello world".chars() {
            ui.insert(ch);
        }
        ui.delete_word_before_cursor();
        assert_eq!(ui.input, "hello ", "Ctrl+W keeps the separator");

        ui.delete_word_before_cursor();
        assert_eq!(
            ui.input, "hello",
            "Ctrl+W on bare whitespace drops one char"
        );
        ui.delete_word_before_cursor();
        assert_eq!(ui.input, "");
        ui.delete_word_before_cursor();
        assert_eq!(ui.input, "", "Ctrl+W on an empty line is a no-op");

        for ch in "abc".chars() {
            ui.insert(ch);
        }
        ui.cursor = 1;
        ui.delete_forward();
        assert_eq!(ui.input, "ac", "Delete removes forward only");

        ui.cursor = 0;
        ui.backspace();
        assert_eq!(ui.input, "ac", "Backspace at the start does nothing");

        for ch in "more".chars() {
            ui.insert(ch);
        }
        ui.clear_input();
        assert_eq!(ui.input, "");
        assert_eq!(ui.cursor, 0);
    }

    #[test]
    fn word_jumps_follow_readline_semantics() {
        let mut ui = shell();
        ui.set_input("hello world".into());
        ui.cursor = ui.input.len();
        ui.move_word_left();
        assert_eq!(ui.cursor, 6, "Alt+Left lands on the current word start");
        ui.move_word_left();
        assert_eq!(ui.cursor, 0, "Alt+Left again crosses to the line start");
        ui.move_word_right();
        assert_eq!(ui.cursor, 6, "Alt+Right skips the word and the separator");
        ui.move_word_right();
        assert_eq!(
            ui.cursor, 11,
            "Alt+Right past the last word ends at the line end"
        );

        ui.set_input("  spaced  out  ".into());
        ui.cursor = 3;
        ui.move_word_left();
        assert_eq!(ui.cursor, 2, "Alt+Left inside a word jumps to its start");
        ui.move_word_right();
        assert_eq!(ui.cursor, 10, "Alt+Right jumps over word and separator");

        ui.cursor = 0;
        ui.move_word_left();
        assert_eq!(ui.cursor, 0, "Alt+Left at the start does nothing");
        ui.cursor = ui.input.len();
        ui.move_word_right();
        assert_eq!(
            ui.cursor,
            ui.input.len(),
            "Alt+Right at the end does nothing"
        );
    }

    #[test]
    fn prompt_history_drops_consecutive_duplicates_and_caps() {
        let mut ui = shell();
        ui.push_history("first".into());
        ui.push_history("first".into());
        ui.push_history("second".into());
        ui.push_history("first".into());
        assert_eq!(
            ui.history,
            vec!["first", "second", "first"],
            "only consecutive duplicates collapse"
        );
        ui.history.clear();
        for index in 0..520 {
            ui.push_history(format!("line {index}"));
        }
        assert_eq!(ui.history.len(), 500, "the list is capped at 500");
        assert_eq!(ui.history.first().map(String::as_str), Some("line 20"));
    }

    #[test]
    fn long_pastes_become_an_expandable_chip() {
        let mut ui = shell();
        ui.accept_paste("one line");
        assert!(
            ui.paste_chip.is_none(),
            "a short paste types straight through"
        );
        assert_eq!(ui.input, "one line");

        let big: String = (0..300).map(|index| format!("line {index}\n")).collect();
        ui.accept_paste(&big);
        let (text, lines) = ui
            .paste_chip
            .clone()
            .expect("a 300-line paste becomes a chip, not keystrokes");
        assert_eq!(lines, 300);
        assert_eq!(text, big);
        assert_eq!(
            ui.input, "one line",
            "the chip does not touch the draft until Enter"
        );

        // The chip owns a row between the separator and the prompt.
        let (frame, _) = ui.compose(100, 30);
        assert!(
            frame[28].text.contains("[Pasted 300 lines]"),
            "chip row: {:?}",
            frame[28].text
        );
        assert!(
            frame[26].text.contains("Ready") || frame[26].text.starts_with(' '),
            "the status line shifts up to make room: {:?}",
            frame[26].text
        );
    }

    #[test]
    fn multiline_input_grows_then_scrolls() {
        let mut ui = shell();
        ui.set_input("one".into());
        assert_eq!(ui.input_rows(), 1);

        ui.set_input("one\ntwo".into());
        assert_eq!(ui.input_rows(), 2, "the draft grows a row per line");
        let (frame, (_, y)) = ui.compose(100, 30);
        assert_eq!(y, 29, "the caret sits on the draft's last row");
        assert!(frame[28].text.starts_with("> one"), "{:?}", frame[28].text);
        assert!(frame[29].text.contains("two"), "{:?}", frame[29].text);

        // Ten lines are capped at a four-row box that follows the caret.
        let long = (0..10)
            .map(|index| format!("line{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_input(long);
        ui.cursor = ui.input.len();
        assert_eq!(ui.input_rows(), 4, "the box stops growing after four rows");
        let (frame, (_, y)) = ui.compose(100, 30);
        assert_eq!(y, 29, "the caret stays on the bottom row of the box");
        assert!(
            frame[29].text.contains("line9"),
            "the window follows the caret: {:?}",
            frame[29].text
        );
        assert!(
            frame[26].text.contains("line6"),
            "the box starts four rows above the caret: {:?}",
            frame[26].text
        );
        assert!(
            frame[26].text.starts_with("↳ "),
            "a scrolled window marks its first row: {:?}",
            frame[26].text
        );

        // The caret walks up: the window follows it back toward the top.
        ui.cursor = 0;
        let (frame, (_, y)) = ui.compose(100, 30);
        assert_eq!(y, 26, "caret on the first line of the visible window");
        assert!(
            frame[26].text.starts_with("> line0"),
            "{:?}",
            frame[26].text
        );
    }

    #[test]
    fn clearing_keeps_history_and_context_fields() {
        let mut ui = shell();
        fill_transcript(&mut ui, 10);
        ui.push_history("remembered".into());
        ui.tool_rows.insert("tool-1".into(), 3);
        ui.clear_transcript();
        assert!(ui.entries.is_empty(), "the visible transcript is cleared");
        assert!(ui.tool_rows.is_empty());
        assert_eq!(
            ui.history,
            vec!["remembered"],
            "history survives a clear (spec 1.3)"
        );
        assert_eq!(ui.transcript_key, None, "the wrap cache is invalidated");
    }

    #[test]
    fn mention_picker_skips_build_and_vcs_noise() {
        let root = std::env::temp_dir().join(format!("wrose-mention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("temp src dir");
        std::fs::create_dir_all(root.join("target/debug")).expect("temp target dir");
        std::fs::create_dir_all(root.join(".git")).expect("temp git dir");
        std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("temp source");
        std::fs::write(root.join("target/debug/wrosecode"), "binary").expect("temp binary");
        std::fs::write(root.join(".git/config"), "[core]").expect("temp git config");

        let files = list_files_for_mention(root.to_str().expect("utf-8 path"));
        assert_eq!(
            files,
            vec!["src/main.rs".to_string()],
            "only project files are offered to @"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pushes_while_scrolled_stay_anchored() {
        let mut ui = shell();
        fill_transcript(&mut ui, 60);
        ui.compose(80, 24);
        ui.scroll_up(6);
        let before = ui.scroll;
        ui.push(Speaker::Agent, "new message while scrolled up");
        ui.compose(80, 24);
        assert!(
            ui.scroll >= before,
            "the viewport follows the content instead of jumping"
        );
        assert!(ui.scroll <= ui.max_scroll);
    }

    #[test]
    fn every_error_kind_has_a_suggestion() {
        for kind in [
            "timeout",
            "permission",
            "parse",
            "network",
            "oom",
            "rate_limit",
            "runtime",
        ] {
            assert!(!error_suggestion(kind).is_empty());
        }
    }

    fn env(term: &str) -> TerminalEnv {
        TerminalEnv {
            term: term.into(),
            ..TerminalEnv::default()
        }
    }

    #[test]
    fn vscode_terminal_keeps_native_wheel() {
        let profile = TerminalProfile::decide(
            true,
            None,
            &TerminalEnv {
                term: "xterm-256color".into(),
                vscode: true,
                ..TerminalEnv::default()
            },
        );
        assert!(profile.alternate, "alt screen is fine in VS Code");
        assert!(!profile.mouse, "mouse capture would fight the scroll wheel");
    }

    #[test]
    fn dumb_terminal_gets_neither_alt_screen_nor_mouse() {
        let profile = TerminalProfile::decide(true, Some("on"), &env("dumb"));
        assert!(!profile.alternate);
        assert!(!profile.mouse);
        assert!(!TerminalProfile::decide(true, Some("on"), &env("")).alternate);
    }

    #[test]
    fn zellij_forces_a_full_repaint() {
        let profile = TerminalProfile::decide(
            true,
            None,
            &TerminalEnv {
                term: "xterm-kitty".into(),
                multiplexer: true,
                ..TerminalEnv::default()
            },
        );
        assert!(profile.full_redraw);
        assert!(!TerminalProfile::decide(true, None, &env("xterm")).full_redraw);
    }

    #[test]
    fn explicit_overrides_beat_detection() {
        let off = TerminalProfile::decide(true, Some("off"), &env("xterm-256color"));
        assert!(!off.mouse);
        let on = TerminalProfile::decide(false, Some("on"), &env("xterm-256color"));
        assert!(on.mouse && !on.alternate);
        let no_alt = TerminalProfile::decide(
            true,
            None,
            &TerminalEnv {
                term: "xterm".into(),
                no_alt_screen: true,
                ..TerminalEnv::default()
            },
        );
        assert!(!no_alt.alternate);
    }

    #[test]
    fn tab_completes_paths_tools_and_commands() {
        let root = std::env::temp_dir().join(format!("wrose-complete-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let schemas = vec![serde_json::json!({"name": "read_file"})];

        assert_eq!(
            completion_matches_at(&root, &schemas, "src/l"),
            vec!["src/lib.rs".to_string()]
        );
        assert!(completion_matches_at(&root, &schemas, "sr").contains(&"src/".to_string()));
        assert_eq!(
            completion_matches_at(&root, &schemas, "read"),
            vec!["read_file".to_string()]
        );
        assert!(completion_matches_at(&root, &schemas, "/mo")
            .iter()
            .any(|name| name.contains("model")));
        assert!(completion_matches_at(&root, &schemas, "zzz-nope").is_empty());
        assert!(completion_matches_at(&root, &schemas, "").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn flag_history_filtering_is_fuzzy() {
        let items: Vec<String> = [
            "flag{deadbeef}  crypto/rsa",
            "CTF{padding_oracle}",
            "picoCTF{entropy_check}",
        ]
        .iter()
        .map(|text| text.to_string())
        .collect();

        assert_eq!(filter_picker_items(&items, ""), items);
        assert_eq!(
            filter_picker_items(&items, "flag{"),
            vec!["flag{deadbeef}  crypto/rsa".to_string()]
        );
        assert_eq!(
            filter_picker_items(&items, "PICO"),
            vec!["picoCTF{entropy_check}".to_string()]
        );
        // Subsequence match: letters in order, gaps allowed.
        assert_eq!(
            filter_picker_items(&items, "pdng"),
            vec!["CTF{padding_oracle}".to_string()]
        );
        assert!(filter_picker_items(&items, "zzz").is_empty());
        assert_eq!(
            filter_picker_items(&items, "flag{deadbeef}  crypto/rsa").len(),
            1
        );
    }

    #[test]
    fn rate_limit_errors_classify_separately() {
        assert_eq!(classify_error("HTTP 429 Too Many Requests"), "rate_limit");
        assert_eq!(classify_error("rate limit exceeded"), "rate_limit");
        assert_eq!(classify_error("connection refused"), "network");
    }

    #[test]
    fn logo_is_three_lines() {
        assert_eq!(LOGO.len(), 3);
        assert!(LOGO[0].contains("W") || LOGO[0].contains('╦'));
    }

    #[test]
    fn dash_stats_round_trip_and_partial_json() {
        let mut ui = shell();
        ui.busy = true;
        ui.flags_found = 2;
        ui.flag_log.push(("flag{a}".into(), "checker".into()));
        let stats = ui.dash_stats();
        assert_eq!(stats.schema, "wrosecode/live-v1");
        assert_eq!(stats.session_id, "test");
        assert_eq!(stats.status, "busy");
        assert_eq!(stats.flags_total, 2);
        assert!(stats.live_at_ms > 0, "the freshness gate needs a timestamp");

        let json = serde_json::to_string(&stats).expect("DashStats serializes");
        let back: DashStats = serde_json::from_str(&json).expect("round trips");
        assert_eq!(back.schema, stats.schema);
        assert_eq!(back.session_id, stats.session_id);
        assert_eq!(back.flags, stats.flags);

        // live.json / /v1/status consumers must tolerate partial documents.
        let partial: DashStats =
            serde_json::from_str(r#"{"schema":"wrosecode/live-v1","session_id":"x"}"#)
                .expect("partial documents parse");
        assert_eq!(partial.session_id, "x");
        assert!(partial.status.is_empty());
        assert!(partial.procs.is_empty());
        let empty: DashStats = serde_json::from_str("{}").expect("defaults cover every field");
        assert!(empty.schema.is_empty());
        assert_eq!(empty.live_at_ms, 0);
    }

    #[test]
    fn flag_rows_parse_the_ctf_log_format() {
        let history = vec![
            "1700000001\tsrc-a\trot13\tflag{first}\t0.9".to_string(),
            "1700000002\tsrc-b\tplain\tflag{second}\t1.0".to_string(),
        ];
        let rows = flag_rows(&history);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("flag{second}".to_string(), "src-b".to_string()));
        assert_eq!(rows[1], ("flag{first}".to_string(), "src-a".to_string()));

        // A line that never got tab-separated still yields its first token
        // under the generic source.
        let loose = vec!["flag{loose} crypto/rsa".to_string()];
        assert_eq!(
            flag_rows(&loose),
            vec![("flag{loose}".to_string(), "checker".to_string())]
        );

        // Newest eight only: the panel and the result block agree on that cap.
        let many: Vec<String> = (0..10)
            .map(|index| format!("170000000{index}\tchecker\tt\tflag{{{index}}}\t1"))
            .collect();
        let rows = flag_rows(&many);
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].0, "flag{9}");
        assert_eq!(rows[7].0, "flag{2}");
    }

    #[test]
    fn compose_grid_paints_header_footer_panels_and_details() {
        let stats = DashStats {
            mode: "build".into(),
            provider: "openai".into(),
            model: "m".into(),
            category: "crypto".into(),
            status: "ready".into(),
            plan: vec![("step one".into(), false), ("step two".into(), true)],
            changed_files: vec![("src/main.rs".into(), 3, 1)],
            budget_usd: 1.0,
            cost_usd: Some(0.25),
            ..DashStats::default()
        };
        let colors = dashboard_theme(true);
        let (frame, _) = compose_grid(&stats, 100, 24, colors, 0, 0, None);
        assert_eq!(frame.len(), 24, "one row per terminal line");
        assert!(
            frame[0]
                .text
                .contains(" DASHBOARD · build/openai/m/crypto · ready"),
            "header: {:?}",
            frame[0].text
        );
        assert!(
            frame[23].text.contains("←→ panel · ↑↓/jk select"),
            "footer: {:?}",
            frame[23].text
        );
        let joined = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for title in [
            "PROCESSES",
            "THINKING",
            "TIMELINE",
            "PLAN",
            "TOKENS & COST",
            "FILES",
            "CTF",
            "BUDGET",
        ] {
            assert!(joined.contains(title), "{title} panel missing:\n{joined}");
        }
        assert!(joined.contains("step one"), "plan rows render:\n{joined}");
        assert!(
            joined.contains("src/main.rs"),
            "file rows render:\n{joined}"
        );
        for (index, row) in frame.iter().enumerate() {
            assert!(
                row.text.chars().count() <= 100,
                "row {index} overflows 100 columns: {:?}",
                row.text
            );
        }

        // Detail view: title line, body lines, and the back hint.
        let (frame, _) = compose_grid(
            &stats,
            80,
            12,
            colors,
            1,
            0,
            Some(("THINKING".into(), "line one\nline two".into())),
        );
        assert!(frame[1].text.contains("── THINKING"), "{:?}", frame[1].text);
        assert!(frame[10].text.contains("Esc back"), "{:?}", frame[10].text);
        let joined = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("line one"));
        assert!(joined.contains("line two"));
    }

    #[test]
    fn dashboard_panels_are_the_eight_named_bodies() {
        let panels = dashboard_panels(&DashStats::default(), 100);
        assert_eq!(panels.len(), 8);
        let titles: Vec<&str> = panels.iter().map(|(title, _)| *title).collect();
        assert_eq!(titles, DASH_PANELS);
        assert!(
            panels[0]
                .1
                .iter()
                .any(|row| row.contains("no tracked processes")),
            "empty process list: {:?}",
            panels[0].1
        );
        assert!(
            panels[6].1.iter().any(|row| row.contains("flags 0")),
            "empty CTF panel still reports the count: {:?}",
            panels[6].1
        );
        assert!(
            panels[7].1.iter().any(|row| row.contains("unlimited")),
            "no budget renders as unlimited: {:?}",
            panels[7].1
        );
    }

    #[test]
    fn reasoning_cell_opens_ticks_and_freezes() {
        let mut ui = shell();
        ui.begin_turn();
        let row = ui.entries.len() - 1;
        assert!(
            ui.entries[row].text.contains("Thinking…"),
            "{:?}",
            ui.entries[row].text
        );
        ui.tick_thinking();
        assert!(ui.entries[row].text.contains("Thinking…"));
        ui.finalize_think();
        let frozen = ui.entries[row].text.clone();
        assert!(
            frozen.contains("Thought") && frozen.contains('s'),
            "the frozen cell reports its elapsed time: {frozen:?}"
        );
        ui.tick_thinking();
        assert_eq!(ui.entries[row].text, frozen, "a frozen cell stops ticking");
        ui.finalize_think();
        assert_eq!(ui.entries[row].text, frozen, "freeze is idempotent");

        ui.begin_turn();
        let next = ui.entries.len() - 1;
        assert!(next > row, "each turn opens a new reasoning cell");
        assert!(ui.entries[next].text.contains("Thinking…"));
    }

    #[test]
    fn result_block_reports_verification_and_failures() {
        let mut ui = shell();
        ui.push(Speaker::Agent, "the flag is flag{ok}");
        ui.task_begin();
        ui.verified = true;
        ui.push_result_block();
        let text = &ui.entries.last().expect("a result block was pushed").text;
        assert!(text.contains("── RESULT ─ ✔ verified"), "{text}");
        assert!(text.contains("answer: the flag is flag{ok}"), "{text}");
        assert!(text.contains("proof: none"), "{text}");
        assert!(text.contains("steps "), "{text}");

        let mut ui = shell();
        ui.task_begin();
        ui.record_error("network");
        ui.push_result_block();
        let text = &ui.entries.last().expect("a result block was pushed").text;
        assert!(text.contains("── RESULT ─ ✗ failed"), "{text}");

        let mut ui = shell();
        ui.task_begin();
        ui.push_result_block();
        let text = &ui.entries.last().expect("a result block was pushed").text;
        assert!(text.contains("── RESULT ─ ⚠ unverified"), "{text}");
    }

    #[test]
    fn tool_cells_preview_expand_and_auto_expand_failures() {
        let mut ui = shell();
        ui.push(Speaker::System, "tool row");
        let row = ui.entries.len() - 1;
        let output = (1..=10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.tool_cells.insert(
            "t1".into(),
            ToolCell {
                title: "shell ls".into(),
                running: false,
                ok: true,
                output,
                elapsed_ms: 1500,
                exit_code: Some(0),
                row,
            },
        );

        // Collapsed cells preview first three + last two lines (spec 3.1).
        let cell = ui.tool_cells["t1"].clone();
        let text = ui.cell_text("t1", &cell);
        assert!(text.contains("✓ shell ls  1.5s"), "{text}");
        assert!(text.contains("⋮ 5 more lines"), "{text}");
        assert!(
            text.contains("line 1") && text.contains("line 10"),
            "{text}"
        );
        assert!(!text.contains("line 6"), "{text}");

        // Enter / Ctrl+O expands, and a second press collapses again.
        ui.toggle_entry(row);
        let cell = ui.tool_cells["t1"].clone();
        let text = ui.cell_text("t1", &cell);
        assert!(text.contains("line 6"), "expanded:\n{text}");
        ui.toggle_entry(row);
        let cell = ui.tool_cells["t1"].clone();
        assert!(!ui.cell_text("t1", &cell).contains("line 6"));

        // Failed cells auto-expand with their exit code, no toggle required.
        ui.tool_cells.insert(
            "t2".into(),
            ToolCell {
                title: "shell false".into(),
                running: false,
                ok: false,
                output: "boom".into(),
                elapsed_ms: 10,
                exit_code: Some(1),
                row: row + 1,
            },
        );
        let cell = ui.tool_cells["t2"].clone();
        let text = ui.cell_text("t2", &cell);
        assert!(text.contains("✗ shell false"), "{text}");
        assert!(text.contains("└ exit 1"), "{text}");
        assert!(text.contains("boom"), "{text}");

        // A running cell shows no output until it finishes.
        ui.tool_cells.insert(
            "t3".into(),
            ToolCell {
                title: "shell sleep".into(),
                running: true,
                ok: false,
                output: "hidden".into(),
                elapsed_ms: 0,
                exit_code: None,
                row: row + 2,
            },
        );
        let cell = ui.tool_cells["t3"].clone();
        let text = ui.cell_text("t3", &cell);
        assert!(text.contains("shell sleep  …"), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn child_tools_render_a_nested_tree_and_groups_count_work() {
        let mut ui = shell();
        ui.child_cells.insert(
            "g1/c1".into(),
            ToolCell {
                title: "read a.rs".into(),
                running: false,
                ok: true,
                output: String::new(),
                elapsed_ms: 100,
                exit_code: Some(0),
                row: usize::MAX,
            },
        );
        ui.child_cells.insert(
            "g1/c2".into(),
            ToolCell {
                title: "grep b".into(),
                running: true,
                ok: false,
                output: String::new(),
                elapsed_ms: 0,
                exit_code: None,
                row: usize::MAX,
            },
        );
        let tree = ui.child_lines("g1", 1);
        assert!(tree.contains("├─ ✓ read a.rs  0.1s"), "{tree}");
        assert!(tree.contains("└─ ⠋ grep b  …"), "{tree}");

        // The collapsed `▸ Explored` group counts finished work and still
        // marks in-flight children.
        let done = GroupCell {
            files: 2,
            searches: 1,
            inflight: 0,
        };
        assert_eq!(group_text(&done), "▸ Explored  2 files · 1 search");
        let live = GroupCell {
            files: 1,
            searches: 0,
            inflight: 1,
        };
        assert_eq!(group_text(&live), "⠋ ▸ Explored  1 file · 0 searches  …");
    }
}
