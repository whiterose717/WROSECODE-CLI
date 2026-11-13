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
use anyhow::{Context, Result};
use crossterm::cursor;
use crossterm::event::{
    self, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::style::{Color, Print, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen};
use crossterm::{execute, queue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
    previous_cursor: (u16, u16),
    /// Redraw every cell on every frame. Terminals whose cell buffer drifts
    /// (Zellij, some multiplexers) show stale glyphs with diff-only updates.
    full_redraw: bool,
}

impl Renderer {
    fn new() -> Self {
        Self {
            previous: Vec::new(),
            size: (0, 0),
            previous_cursor: (u16::MAX, u16::MAX),
            full_redraw: false,
        }
    }

    fn draw(&mut self, rows: &[Row], input_cursor: (u16, u16), size: (u16, u16)) -> io::Result<()> {
        let mut out = io::stdout().lock();
        self.draw_to(&mut out, rows, input_cursor, size)
    }

    /// The frame writer, split out so tests can capture the bytes a resize
    /// or a diff actually emits without touching the real terminal.
    fn draw_to(
        &mut self,
        out: &mut impl Write,
        rows: &[Row],
        input_cursor: (u16, u16),
        size: (u16, u16),
    ) -> io::Result<()> {
        if self.size == size
            && !self.full_redraw
            && self.previous.len() == rows.len()
            && self.previous == rows
            && self.previous_cursor == input_cursor
        {
            return Ok(());
        }
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
        self.previous_cursor = input_cursor;
        Ok(())
    }
}

/// How the current terminal wants to be driven. Each field can be forced off with
/// an environment variable so a broken terminal never needs a code change:
/// `WROSECODE_NO_ALT_SCREEN`, `WROSECODE_MOUSE=1` (opt into mouse reporting),
/// `WROSECODE_FULL_REDRAW`.
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
    /// `WROSECODE_MOUSE=1` opts into mouse reporting when `mouse_capture`
    /// is `"auto"`.
    pub force_mouse: bool,
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
            force_mouse: std::env::var("WROSECODE_MOUSE").is_ok_and(|value| value == "1"),
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

    /// Pure decision function. `mouse_capture` of `None` means `"auto"`,
    /// which keeps native terminal text selection: mouse reporting stays
    /// off unless explicitly enabled, so drag-to-select and the terminal's
    /// own copy shortcut keep working. Opt in with `"on"` (or
    /// `WROSECODE_MOUSE=1`) for click-to-expand and wheel handling; every
    /// mouse action already has a keyboard equivalent.
    pub fn decide(
        configured_alternate: bool,
        mouse_capture: Option<&str>,
        env: &TerminalEnv,
    ) -> Self {
        let dumb = env.term.is_empty() || env.term == "dumb";
        let mouse = match mouse_capture
            .unwrap_or("auto")
            .to_ascii_lowercase()
            .as_str()
        {
            "on" | "true" | "yes" | "1" => !dumb,
            "off" | "false" | "no" | "0" => false,
            _ => env.force_mouse,
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
    panel: Color::Reset,
};

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

/// Owns the raw-mode / alternate-screen / mouse capture state for the whole
/// session. `main` enters it early (so the splash can paint) and hands it to
/// [`run`], which keeps it alive until the session ends.
pub struct TerminalGuard {
    profile: TerminalProfile,
}

impl TerminalGuard {
    /// Take the screen for the splash but leave the terminal in canonical
    /// mode, so `SIGINT` is still generated while startup work (OAuth, MCP
    /// handshakes, session restore) runs. The signal handler in
    /// [`crate::crash`] restores the terminal and exits; [`Self::start_input`]
    /// upgrades to raw mode once the TUI event loop is ready to read keys.
    pub fn enter_painting(profile: TerminalProfile) -> io::Result<Self> {
        Self::paint_owned(profile)
    }

    /// Enable raw input for the interactive TUI loop.
    pub fn start_input(&self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        crate::crash::arm_terminal(self.profile.alternate, self.profile.mouse);
        Ok(())
    }

    fn paint_owned(profile: TerminalProfile) -> io::Result<Self> {
        crate::crash::arm_terminal(profile.alternate, profile.mouse);
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
            crate::crash::restore();
            return Err(error);
        }
        crate::crash::arm_terminal(profile.alternate, profile.mouse);
        Ok(Self { profile })
    }

    pub fn profile(&self) -> TerminalProfile {
        self.profile
    }

    /// Temporarily return terminal ownership to a child process such as the
    /// user's editor, then restore the exact terminal profile afterwards.
    pub fn suspend(&self) {
        crate::crash::restore();
    }

    pub fn resume(&self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        crate::crash::arm_terminal(self.profile.alternate, self.profile.mouse);
        let entered = match (self.profile.alternate, self.profile.mouse) {
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
            crate::crash::restore();
            return Err(error);
        }
        crate::crash::arm_terminal(self.profile.alternate, self.profile.mouse);
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // One teardown path shared with the panic hook and the signal
        // handlers (src/crash.rs) so every exit leaves a usable shell.
        crate::crash::restore();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Speaker {
    User,
    Agent,
    Tool,
    System,
}

/// The single presentation state for the active task. Backend progress still
/// arrives through the existing channel, but the renderer no longer has to
/// guess a visual state from arbitrary status strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskState {
    Ready,
    Submitting,
    Thinking,
    Streaming,
    RunningTool,
    WaitingForModel,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    fn marker(self) -> &'static str {
        match self {
            Self::Ready => "◆ READY",
            Self::Submitting => "● SUBMIT",
            Self::Thinking => "◇ THINK",
            Self::Streaming => "◆ STREAM",
            Self::RunningTool => "● TOOL",
            Self::WaitingForModel => "● WAIT",
            Self::Completed => "✓ DONE",
            Self::Failed => "× ERROR",
            Self::Cancelled => "× CANCELLED",
        }
    }
}

struct Entry {
    speaker: Speaker,
    text: String,
}

/// Wrap-cache bookkeeping for one transcript entry: where its rows live in
/// the flattened transcript, how many rows it owns (including the blank
/// separator), and the revision that produced them.
#[derive(Clone, Copy, Debug)]
struct EntryRowsMeta {
    start: usize,
    count: usize,
    revision: u64,
}

/// Streaming state for the assistant body renderer. The full renderer and
/// the incremental draft cache both drive this machine, so the live draft is
/// byte-identical to rendering the finished text at every split point.
///
/// A fence opens on its marker line, emits code rows as they arrive, and
/// closes on the next marker (or at the end of the visible text). Nothing is
/// buffered, so a long code block streams instead of appearing only once its
/// closing fence arrives.
#[derive(Clone, Debug, Default)]
struct BodyStream {
    in_fence: bool,
    fence_lang: String,
}

impl BodyStream {
    /// Render one logical line into `out`.
    fn feed(
        &mut self,
        line: &str,
        width: usize,
        panel: Color,
        out: &mut Vec<(String, Option<Color>)>,
    ) {
        let width = width.max(12);
        let available = width.saturating_sub(6).max(1);
        let trimmed = line.trim_start();
        // Studio: a slim gutter, rather than double-boxing every paragraph.
        // Code fences keep the same streaming state as the final message.
        if studio_panel(panel) {
            if trimmed.as_bytes().starts_with(&[96, 96, 96]) {
                if self.in_fence {
                    self.in_fence = false;
                    self.fence_lang.clear();
                    out.push(("  ╰─".into(), Some(panel)));
                } else {
                    self.in_fence = true;
                    self.fence_lang = trimmed
                        .trim_start_matches(char::from(96))
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    out.push((
                        format!(
                            "  ╭─ {}",
                            if self.fence_lang.is_empty() {
                                "code"
                            } else {
                                &self.fence_lang
                            }
                        ),
                        Some(panel),
                    ));
                }
                return;
            }
            for part in wrap(line, available) {
                let prefix = if self.in_fence { "  │ " } else { "    " };
                out.push((
                    clip(&format!("{prefix}{part}"), width),
                    if self.in_fence { Some(panel) } else { None },
                ));
            }
            return;
        }
        if !self.in_fence && trimmed.starts_with("```") {
            self.in_fence = true;
            self.fence_lang = trimmed
                .strip_prefix("```")
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            let label = if self.fence_lang.is_empty() {
                "code"
            } else {
                &self.fence_lang
            };
            out.push((box_top_row(width, label), Some(panel)));
            return;
        }
        if self.in_fence && trimmed.starts_with("```") {
            self.in_fence = false;
            self.fence_lang.clear();
            out.push((box_bottom_row_text(width), Some(panel)));
            return;
        }
        // Prose and code both wrap; indentation is preserved verbatim. An
        // empty line still owns a row so paragraph breaks keep their shape.
        for part in wrap(line, available) {
            out.push((box_body_row(&part, width), Some(panel)));
        }
    }

    /// Close an unclosed fence at the end of the currently visible text, so
    /// a half-streamed block renders as a real container rather than leaking
    /// raw backticks or an unterminated box.
    fn finish(&mut self, width: usize, panel: Color, out: &mut Vec<(String, Option<Color>)>) {
        if self.in_fence {
            self.in_fence = false;
            self.fence_lang.clear();
            if studio_panel(panel) {
                out.push(("  ╰─".into(), Some(panel)));
            } else {
                out.push((box_bottom_row_text(width.max(12)), Some(panel)));
            }
        }
    }
}

/// Incremental renderer for the live assistant draft. Complete lines are
/// wrapped once, when they arrive; only the trailing partial line is
/// re-wrapped per frame. That keeps the per-frame cost independent of how
/// long the streamed answer has grown.
fn agent_header(width: usize, panel: Color) -> (String, Option<Color>) {
    if studio_panel(panel) {
        ("  ◈ WROSECODE".into(), None)
    } else {
        (box_top_row(width, "◈ WROSECODE"), Some(panel))
    }
}

fn agent_footer(width: usize, panel: Color) -> Option<(String, Option<Color>)> {
    if studio_panel(panel) {
        None
    } else {
        Some((box_bottom_row_text(width), Some(panel)))
    }
}

#[derive(Debug, Default)]
struct DraftCache {
    width: usize,
    verbose: bool,
    panel: Option<Color>,
    /// Bytes of the draft already converted into `rows` (ends at a newline).
    processed: usize,
    rows: Vec<(String, Option<Color>)>,
    body: BodyStream,
}

struct Ui {
    renderer: Renderer,
    entries: Vec<Entry>,
    input: String,
    cursor: usize,
    /// First wrapped input row shown in the editor viewport.
    input_scroll: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    scroll: usize,
    status: String,
    /// The last execution stage the provider announced (`Progress::ModelStage`).
    /// Kept apart from `status`: the status line shows transient UI state
    /// ("Streaming", "Tool active"), while this holds the stage the model is
    /// actually in ("Preparing request context…", "Sending prompt to model").
    stage: String,
    task_state: TaskState,
    draft: String,
    /// Incremental wrapping state for `draft` (see [`DraftCache`]).
    draft_cache: DraftCache,
    busy: bool,
    spinner: usize,
    root: String,
    /// User-defined slash commands from `.wrosecode/commands/*.md`, loaded
    /// once at startup for the palette, `/help`, and dispatch.
    user_commands: Vec<crate::markdown::UserCommand>,
    provider: String,
    model: String,
    /// Cached provider/profile/model rows used by `/models`. The cache is
    /// presentation-only; the active provider/model remains authoritative in
    /// `Agent::config` and the selected provider profile.
    model_catalog: Vec<(String, String)>,
    model_catalog_errors: Vec<String>,
    model_catalog_fetched: Option<std::time::Instant>,
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
    activity_rail: bool,
    plan_row: Option<usize>,
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
    /// Live MCP servers: (name, tool count). Synced from the agent; empty
    /// means no MCP is connected — never invented. Surfaced through the
    /// Ctrl+B session snapshot now that the rail is gone.
    mcp_live: Vec<(String, usize)>,
    /// Live LSP servers: (server key, ready). Empty when LSP is disabled or
    /// no server has been spawned yet.
    lsp_live: Vec<(String, bool)>,
    /// LSP server keys that failed to start and will not be retried (e.g. a
    /// missing binary). Surfaced in Ctrl+B instead of raw stderr.
    lsp_dead: Vec<String>,
    /// Whether `[lsp]` is enabled in the active config.
    lsp_enabled: bool,
    /// Cached git state (refreshed at most every 5s, never per frame).
    git_branch: String,
    git_files: usize,
    git_added: i64,
    git_removed: i64,
    git_at: Option<std::time::Instant>,
    /// Last backend sync; rate-limits the git refresh.
    chrome_at: Option<std::time::Instant>,
    theme: usize,
    /// Finished delegate subagents this session, collapsed to one summary
    /// line in the tree pane instead of growing it without bound.
    finished_children: usize,
    finished_ms: u128,
    /// Last Ctrl+C at an idle prompt (or cancelling a turn): a second press
    /// within the exit window quits instead of just clearing input.
    last_interrupt: Option<std::time::Instant>,
    /// `--trace` timing sink, shared with the agent: slow frames land in
    /// `~/.wrosecode/trace.log` next to the API/tool timings.
    pub trace: crate::trace::TraceSink,
    metrics: crate::metrics::MetricsSnapshot,
    search_query: String,
    search_matches: Vec<usize>,
    search_position: usize,
    budget_usd: f64,
    last_flag: String,
    turn_tokens: Vec<u64>,
    turn_usage: Usage,
    turn_started: Option<std::time::Instant>,
    error_count: usize,
    last_error_kind: &'static str,
    completion: Option<CompletionState>,
    /// Wrapped transcript rows, rebuilt only when the transcript, the pane
    /// width, or the verbosity changes. Scrolling just reslices this, which is
    /// what keeps a long session from re-wrapping every line on every key.
    transcript_lines: Vec<String>,
    /// Per-row background override parallel to `transcript_lines`: `Some`
    /// paints charcoal containers (code fences, tool output, snapshot),
    /// `None` keeps the entry's style background.
    transcript_bg: Vec<Option<Color>>,
    transcript_key: Option<(usize, bool, Color)>,
    /// Earliest entry mutated since the last wrap pass. Avoid an O(history)
    /// revision scan on every 30 FPS frame of an hours-long conversation.
    transcript_dirty_from: Option<usize>,
    /// Per-entry revision, parallel to `entries`. Mutating one entry bumps
    /// only its revision, so the wrap cache re-wraps that entry instead of
    /// the whole transcript. Entries are append-only between clears.
    entry_revisions: Vec<u64>,
    /// Next revision handed out to a new or rewritten entry.
    next_entry_revision: u64,
    /// Where each entry starts in `transcript_lines` plus the revision and
    /// row count recorded when it was last wrapped. Aligned with `entries`.
    entry_rows_meta: Vec<EntryRowsMeta>,
    transcript_rebuilds: u64,
    /// Entries re-wrapped across all rebuild passes (cache misses only).
    entries_rewrapped: u64,
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
    /// One-line provider health, e.g. `3 connected · 2 need setup`.
    provider_health: String,
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
    /// Bounded operation history, plus monotonic start times for live clocks.
    dash_activity: VecDeque<(ActivityStat, std::time::Instant)>,
    dash_files: Vec<(String, i64, i64)>,
    dash_data_at: Option<std::time::Instant>,
    dash_files_at: Option<std::time::Instant>,
    /// Last `~/.wrosecode/live.json` write, rate-limited to one per second.
    live_written_at: Option<std::time::Instant>,
    /// The live checklist the model maintains through `update_plan`.
    plan: Vec<(String, bool)>,
    /// Recent flag hits: (flag, source), capped.
    flag_log: Vec<(String, String)>,
    /// Detector used by the UI event reducer for flags split across streamed
    /// model/tool chunks. Persistence and auto-submit remain agent-owned.
    flag_detector: crate::ctf::CtfEngine,
    flag_stream_buffers: HashMap<String, String>,
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
    /// Real request milestones for the task receipt and live diagnostics.
    first_token_ms: Option<u64>,
    first_tool_ms: Option<u64>,
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
    "ACTIVITY / PROCESSES",
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
            command: crate::crash::redact(&snapshot.command),
            cwd: snapshot.cwd.clone(),
            age_s: snapshot.started.elapsed().as_secs(),
            running: snapshot.running,
            exit_code: snapshot.exit_code,
            timed_out: snapshot.timed_out,
            cpu_pct: snapshot.cpu_pct,
            rss_kb: snapshot.rss_kb,
            tail: crate::crash::redact(&snapshot.tail),
        }
    }
}

/// A model-requested operation, including non-shell tools such as reads,
/// searches, edits, MCP calls and delegated agent jobs.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ActivityStat {
    pub id: String,
    pub title: String,
    pub running: bool,
    pub ok: Option<bool>,
    pub elapsed_ms: u64,
}

/// Everything the dashboard and attach clients need. Field defaults let
/// older snapshots be read without losing compatibility.
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
    /// Newest-first model tool calls, not just shell subprocesses.
    pub activities: Vec<ActivityStat>,
    /// Current transport/execution state, distinct from internal reasoning.
    pub stage: String,
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
    budget_usd: f64,
    provider_health: String,
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
            budget_usd: agent.config.budget_usd,
            provider_health: provider_health_line(),
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
            budget_usd: 0.0,
            provider_health: "0 connected · 0 need setup".into(),
        }
    }
}

impl Ui {
    fn new(agent: &Agent, session_id: String) -> Self {
        let mut ui = Self::from_meta(UiMeta::from(agent), session_id);
        ui.trace = agent.trace.clone();
        ui.flag_detector = agent.ctf.clone();
        ui
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
            input_scroll: 0,
            history: Vec::new(),
            history_index: None,
            completion: None,
            scroll: 0,
            status: "Ready".into(),
            stage: String::new(),
            task_state: TaskState::Ready,
            draft: String::new(),
            draft_cache: DraftCache::default(),
            busy: false,
            spinner: 0,
            root: meta.root,
            user_commands,
            provider: meta.provider,
            model: meta.model,
            model_catalog: Vec::new(),
            model_catalog_errors: Vec::new(),
            model_catalog_fetched: None,
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
            activity_rail: false,
            plan_row: None,
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
            mcp_live: Vec::new(),
            lsp_live: Vec::new(),
            lsp_dead: Vec::new(),
            lsp_enabled: true,
            git_branch: String::new(),
            git_files: 0,
            git_added: 0,
            git_removed: 0,
            git_at: None,
            chrome_at: None,
            theme: meta.theme,
            finished_children: 0,
            finished_ms: 0,
            last_interrupt: None,
            metrics: crate::metrics::MetricsSnapshot::default(),
            trace: crate::trace::TraceSink::default(),
            search_query: String::new(),
            search_matches: Vec::new(),
            search_position: 0,
            budget_usd: meta.budget_usd,
            last_flag: String::new(),
            turn_tokens: Vec::new(),
            turn_usage: Usage::default(),
            turn_started: None,
            error_count: 0,
            last_error_kind: "",
            transcript_lines: Vec::new(),
            transcript_bg: Vec::new(),
            transcript_key: None,
            transcript_dirty_from: Some(0),
            entry_revisions: vec![0],
            next_entry_revision: 1,
            entry_rows_meta: Vec::new(),
            transcript_rebuilds: 0,
            entries_rewrapped: 0,
            page_size: 10,
            max_scroll: 0,
            pending: Vec::new(),
            new_since_detach: 0,
            paste_chip: None,
            colors: ColorSupport::None,
            no_color: false,
            provider_health: meta.provider_health,
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
            dash_activity: VecDeque::new(),
            dash_files: Vec::new(),
            dash_data_at: None,
            dash_files_at: None,
            live_written_at: None,
            plan: Vec::new(),
            flag_log: Vec::new(),
            flag_detector: crate::ctf::CtfEngine::new(std::path::Path::new(".")),
            flag_stream_buffers: HashMap::new(),
            changed_files: Vec::new(),
            verified: false,
            ctf_offered: false,
            tool_ms_total: 0,
            model_ms_total: 0,
            turns_total: 0,
            max_parallel: 0,
            inflight_tools: 0,
            turn_tool_ms: 0,
            first_token_ms: None,
            first_tool_ms: None,
            task_started: None,
            task_base: None,
        }
    }

    fn begin_turn(&mut self) {
        self.set_task_state(TaskState::WaitingForModel);
        self.turn_usage = Usage::default();
        self.turn_started = Some(std::time::Instant::now());
        self.turn_steps = 0;
        self.turn_tool_ms = 0;
        self.first_token_ms = None;
        self.flag_stream_buffers.clear();
        self.first_tool_ms = None;
        // A quiet reasoning cell opens each turn and freezes into its
        // elapsed time on first model activity.
        self.think_finished = false;
        self.active_group = None;
        self.plan_row = None;
        self.stage.clear();
        let row = self.entries.len();
        self.push_scrolled(
            Entry {
                speaker: Speaker::System,
                text: "◌ Thinking…".into(),
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
        self.set_entry_text(row, format!("Thought {secs:.1}s"));
    }

    /// Tick the live `Thinking… 2.1s` cell from the render ticker.
    fn tick_thinking(&mut self) {
        if self.inflight_tools > 0
            && self.shell_calls > 0
            && self
                .dash_data_at
                .is_none_or(|at| at.elapsed() >= Duration::from_millis(250))
        {
            self.dash_procs = crate::tools::shell::proc_snapshots();
            self.dash_data_at = Some(std::time::Instant::now());
        }
        // Every running agent tool has a visible, live elapsed timer.
        let running: Vec<String> = self
            .tool_cells
            .iter()
            .filter(|(_, cell)| cell.running && cell.row != usize::MAX)
            .map(|(id, _)| id.clone())
            .collect();
        for id in running {
            self.refresh_cell(&id);
        }
        if self.think_finished {
            return;
        }
        let Some(started) = self.turn_started else {
            return;
        };
        if self.first_token_ms.is_none() && self.inflight_tools == 0 {
            self.set_task_state(TaskState::Thinking);
        }
        let secs = started.elapsed().as_secs_f64();
        let Some(row) = self.think_row else { return };
        if row >= self.entries.len() {
            self.think_row = None;
            self.think_finished = true;
            return;
        }
        let next = format!("◌ Thinking… {secs:.1}s");
        if self.entries[row].text != next {
            self.set_entry_text(row, next);
        }
    }

    /// Rewrite one entry's text, keeping a scrolled-up viewport anchored to
    /// the same content as the line count changes.
    fn set_entry_text(&mut self, row: usize, text: String) {
        // A session switch, /new or Ctrl+L can replace all rows. Tool
        // completions arriving from the old turn must never index new rows.
        let Some(entry) = self.entries.get(row) else {
            return;
        };
        if entry.text == text {
            return;
        }
        self.transcript_dirty_from = Some(
            self.transcript_dirty_from
                .map_or(row, |start| start.min(row)),
        );
        let old_lines = self.entries[row].text.lines().count().max(1);
        self.entries[row].text = text;
        if let Some(revision) = self.entry_revisions.get_mut(row) {
            *revision = self.next_entry_revision;
            self.next_entry_revision = self.next_entry_revision.wrapping_add(1);
        }
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

    /// True while anything on screen can change on its own: a running turn
    /// or the dashboard's live panels. When false the frame is a pure
    /// function of input state, so the loop sleeps instead of repainting —
    /// zero periodic draws while idle.
    fn wants_animation(&self) -> bool {
        self.busy || self.dashboard
    }

    /// Wrose Pulse: a small semantic state marker, driven only by real UI
    /// state transitions. It is intentionally calmer than a decorative
    /// spinner and lets users identify the current phase at a glance.
    /// Surfaced in the Ctrl+B session snapshot.
    fn pulse_marker(&self) -> &'static str {
        self.task_state.marker()
    }

    /// Cheap identity of everything the 1 Hz chrome sync can change. The
    /// idle loop repaints only when this moves (or a turn is running), so a
    /// quiet session costs no frames.
    fn chrome_fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
            self.provider,
            self.model,
            self.mode,
            self.category,
            self.usage.input,
            self.usage.output,
            self.usage.reasoning,
            self.usage.cache_read,
            self.tool_calls,
            self.tool_failures,
            self.inflight_tools,
            self.mcp_live.len(),
            self.lsp_live.len(),
            self.git_branch,
            self.flags_found,
            self.metrics.cost_usd,
        )
    }

    /// The slim think status bar owns one row directly above the composer
    /// box whenever a thinking level is active (`/think`, Ctrl+T, `[`/`]`).
    /// Turn it off and the bar — and its row — disappear with it.
    fn think_bar_visible(&self) -> bool {
        self.think != ThinkLevel::Off
    }

    fn set_task_state(&mut self, state: TaskState) {
        self.task_state = state;
    }

    /// Mirror live backend facts into the chrome (header, composer info line,
    /// Ctrl+B snapshot). Cheap fields sync every call; git is refreshed at
    /// most every 5s so rendering never pays for `git` per frame. Never
    /// invents state: missing integrations stay empty and the snapshot omits
    /// those sections.
    fn sync_chrome(&mut self, agent: &Agent) {
        self.provider = agent.config.provider.clone();
        self.model = agent.config.model.clone();
        self.mode = agent.mode.to_ascii_uppercase();
        self.category = agent.ctf.category.clone();
        self.usage = agent.usage;
        self.metrics = agent.metrics.snapshot();
        self.mcp_live = agent
            .tools
            .mcps
            .iter()
            .map(|(name, mcp)| (name.clone(), mcp.schemas.len()))
            .collect();
        self.mcp_live.sort_by(|a, b| a.0.cmp(&b.0));
        self.lsp_enabled = agent.config.lsp.enabled;
        // Live LSP servers need the async lock; the render path must never
        // block, so best-effort only — a locked registry keeps its last view.
        if let Ok(lsp) = agent.tools.lsp.try_lock() {
            self.lsp_live = lsp.live_servers();
            self.lsp_dead = lsp.dead_servers();
        }
        let now = std::time::Instant::now();
        // Three git child processes cost ~50ms; refresh them every 5s while
        // working but only every 30s while idle.
        let stale = git_stale(self.git_at, now, self.busy);
        if stale {
            let (branch, files, added, removed) = git_sidebar_state(&self.root);
            self.git_branch = branch;
            self.git_files = files;
            self.git_added = added;
            self.git_removed = removed;
            self.git_at = Some(now);
        }
        self.chrome_at = Some(now);
    }

    /// One-shot session snapshot for Ctrl+B: every live backend fact in one
    /// muted block, now that the rail is gone. Sections with no real data are
    /// omitted, never invented. Mirrors `/stats` and `/debug`, which remain
    /// the canonical deep views.
    fn session_snapshot(&self) -> String {
        let mut out = String::from("SESSION\n");
        out.push_str(&format!(
            " project  {}\n session  {} · {} · {}\n backend  {} / {}\n",
            short_path(&self.root, 48),
            clip(&self.session_id, 18),
            self.mode,
            self.pulse_marker(),
            self.provider,
            clip(&self.model, 40),
        ));
        out.push_str(&format!(
            " context  {} in · {} out · {}% cache",
            compact_number(self.usage.input),
            compact_number(self.usage.output),
            cache_percent(self.usage),
        ));
        match self.metrics.cost_usd {
            Some(cost) => out.push_str(&format!(" · ${cost:.4}\n")),
            None => out.push_str(" · cost n/a\n"),
        }
        if self.mcp_live.is_empty() {
            out.push_str(" mcp      no servers connected\n");
        } else {
            out.push_str(&format!(" mcp      {} connected\n", self.mcp_live.len()));
            for (name, tools) in self.mcp_live.iter().take(6) {
                out.push_str(&format!("   ● {} · {} tools\n", clip(name, 28), tools));
            }
        }
        if self.lsp_enabled {
            if self.lsp_live.is_empty() {
                out.push_str(" lsp      enabled · no live servers\n");
            } else {
                out.push_str(" lsp\n");
                for (name, ready) in self.lsp_live.iter().take(6) {
                    out.push_str(&format!(
                        "   {} {}\n",
                        if *ready { "●" } else { "◌" },
                        clip(name, 40)
                    ));
                }
            }
            for name in self.lsp_dead.iter().take(6) {
                out.push_str(&format!("   ✗ {} · unavailable\n", clip(name, 36)));
            }
        }
        if self.inflight_tools > 0 || self.tool_calls > 0 {
            out.push_str(&format!(
                " tasks    {} done · {} running · {} failed\n",
                self.tool_calls, self.inflight_tools, self.tool_failures,
            ));
        }
        if !self.git_branch.is_empty() {
            out.push_str(&format!(
                " git      {} · ~{} +{} -{}\n",
                clip(&self.git_branch, 28),
                self.git_files,
                self.git_added,
                self.git_removed,
            ));
        }
        if self.mode.eq_ignore_ascii_case("CTF") {
            out.push_str(&format!(
                " ctf      {} flags · {} · {}\n",
                self.flags_found,
                self.category,
                if self.verified {
                    "verified"
                } else {
                    "unverified"
                },
            ));
        }
        out.push_str(" details  /stats · /debug · /models");
        out
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
    /// answer, proof, time split, numbered step list, tokens, cache rate,
    /// cost, savings.
    fn push_result_block(&mut self, steps: &[String]) {
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
            Some("✔ verified")
        } else if errors_delta > 0 {
            Some("✗ failed")
        } else {
            None
        };
        let step_count = self.tool_calls.saturating_sub(base.tool_calls);
        if self.verbosity != "verbose" {
            let marker = match self.task_state {
                TaskState::Failed => "× Failed",
                TaskState::Cancelled => "× Cancelled",
                _ if self.verified => "✓ Verified",
                _ => "✓ Done",
            };
            let tools_label = format!(
                "{} tool{}",
                step_count,
                if step_count == 1 { "" } else { "s" }
            );
            let context = if self.usage.input > 0 {
                format!(" · {} ctx", compact_number(self.usage.input))
            } else {
                String::new()
            };
            self.push(
                Speaker::System,
                format!(
                    "{marker} in {} · {tools_label}{context}",
                    format_duration_ms(wall as u64)
                ),
            );
            return;
        }
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
        let mut step_lines = String::new();
        // The closing receipt names what the turn did, one line per step;
        // the list is capped so a long run cannot flood the transcript.
        for (index, step) in steps.iter().enumerate().take(8) {
            step_lines.push_str(&format!("\n {}. {step}", index + 1));
        }
        if steps.len() > 8 {
            step_lines.push_str(&format!("\n … +{} more", steps.len() - 8));
        }
        let cost = if base.has_cost || self.metrics.cost_usd.is_some() {
            Some(self.metrics.cost_usd.unwrap_or(0.0) - base.cost)
        } else {
            None
        };
        let status_line = status
            .map(|status| format!(" {status}\n"))
            .unwrap_or_default();
        let first_token = self
            .first_token_ms
            .map(format_duration_ms)
            .unwrap_or_else(|| "n/a".into());
        let first_tool = self
            .first_tool_ms
            .map(format_duration_ms)
            .unwrap_or_else(|| "n/a".into());
        let mut text = format!(
            "{status_line} answer: {answer}\n proof: {proof}\n time {} (model {} · tools {} · wait {})\n first token {first_token} · first tool {first_tool}\n steps {step_count}{step_lines} · tokens {} ({hit}% cache) · cost {} · saved ~{} tok",
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
        let header = match status {
            Some(status) => format!("── RESULT ─ {status} ──────────────"),
            None => "── RESULT ─────────────────────────".into(),
        };
        self.push(Speaker::System, format!("{header}\n{text}"));
    }

    fn transcript_key(
        &self,
        left_width: usize,
        verbose: bool,
        panel: Color,
    ) -> (usize, bool, Color) {
        (left_width, verbose, panel)
    }

    /// Rebuild the wrapped transcript only where it actually changed.
    /// Returns true when at least one entry was re-wrapped (asserted by
    /// tests). Entries are append-only, so a streaming turn re-wraps only
    /// the entry that is growing; a width/verbosity/theme change re-wraps
    /// everything.
    fn ensure_transcript(&mut self, left_width: usize, verbose: bool, panel: Color) -> bool {
        let key = self.transcript_key(left_width, verbose, panel);
        let geometry_changed = self.transcript_key != Some(key);
        if geometry_changed {
            self.transcript_key = Some(key);
            self.entry_rows_meta.clear();
            self.transcript_lines.clear();
            self.transcript_bg.clear();
            self.line_origin.clear();
        }
        // All normal mutations mark the first changed entry. A render that
        // only animates a spinner or updates draft text never walks the full
        // history to compare thousands of unchanged revision numbers.
        let mut first_stale = if geometry_changed {
            0
        } else {
            self.transcript_dirty_from
                .unwrap_or(self.entries.len())
                .min(self.entry_rows_meta.len())
        };
        self.transcript_dirty_from = None;
        // A direct session replacement must also invalidate the cache even
        // when its new transcript happens to have the same number of rows.
        if !geometry_changed
            && first_stale == self.entries.len()
            && self.entry_rows_meta.len() != self.entries.len()
        {
            first_stale = self.entry_rows_meta.len().min(self.entries.len());
        }
        if self.entry_rows_meta.len() > self.entries.len() {
            // A clear happened without the meta being cleared alongside it.
            let keep = self.entries.len();
            let start = self
                .entry_rows_meta
                .get(keep)
                .map(|meta| meta.start)
                .unwrap_or(self.transcript_lines.len());
            self.transcript_lines.truncate(start);
            self.transcript_bg.truncate(start);
            self.line_origin.truncate(start);
            self.entry_rows_meta.truncate(keep);
        }
        if first_stale >= self.entries.len() {
            return geometry_changed;
        }
        let start = self
            .entry_rows_meta
            .get(first_stale)
            .map(|meta| meta.start)
            .unwrap_or(self.transcript_lines.len());
        self.transcript_lines.truncate(start);
        self.transcript_bg.truncate(start);
        self.line_origin.truncate(start);
        self.entry_rows_meta.truncate(first_stale);
        for (index, entry) in self.entries.iter().enumerate().skip(first_stale) {
            let revision = self.entry_revisions.get(index).copied().unwrap_or_default();
            let begin = self.transcript_lines.len();
            let wrapped = entry_rows(entry, left_width, verbose, panel);
            let count = wrapped.len();
            self.transcript_lines
                .extend(wrapped.iter().map(|(text, _)| text.clone()));
            self.transcript_bg.extend(wrapped.iter().map(|(_, bg)| *bg));
            self.line_origin.extend(std::iter::repeat_n(index, count));
            self.transcript_lines.push(String::new());
            self.transcript_bg.push(None);
            self.line_origin.push(index);
            self.entry_rows_meta.push(EntryRowsMeta {
                start: begin,
                count: count + 1,
                revision,
            });
        }
        self.transcript_rebuilds += 1;
        self.entries_rewrapped += self.entries.len().saturating_sub(first_stale) as u64;
        true
    }

    /// Feed every newly completed line of the draft through the incremental
    /// wrap cache. Only text appended since the previous frame is processed,
    /// so per-frame work is proportional to the new bytes, not the whole
    /// response.
    fn refresh_draft_rows(&mut self, width: usize, verbose: bool, panel: Color) {
        let width = width.max(12);
        let reset = self.draft_cache.width != width
            || self.draft_cache.verbose != verbose
            || self.draft_cache.panel != Some(panel)
            || self.draft_cache.processed > self.draft.len();
        if reset {
            self.draft_cache = DraftCache {
                width,
                verbose,
                panel: Some(panel),
                ..DraftCache::default()
            };
            self.draft_cache.rows.push(agent_header(width, panel));
        }
        let start = self.draft_cache.processed;
        let Some(offset) = self.draft[start..].rfind('\n') else {
            return;
        };
        let end = start + offset;
        // Disjoint field borrows: the cache is mutated while the draft is
        // only read, which the borrow checker accepts for direct fields.
        let cache = &mut self.draft_cache;
        for line in self.draft[start..end].split('\n') {
            cache.body.feed(line, width, panel, &mut cache.rows);
        }
        cache.processed = end + 1;
    }

    /// Rows for the not-yet-newline-terminated tail plus the closing border.
    /// Returns the number of already-cached draft rows alongside the tail,
    /// so the caller can index the combined sequence without copying the
    /// cached rows on every frame.
    #[allow(clippy::type_complexity)]
    fn prepare_draft(
        &mut self,
        width: usize,
        verbose: bool,
        panel: Color,
    ) -> (usize, Vec<(String, Option<Color>)>) {
        self.refresh_draft_rows(width, verbose, panel);
        let width = width.max(12);
        let mut tail = Vec::new();
        // Replay the partial line on a copy of the streaming state: the
        // preview must match a full render of the same bytes without
        // committing anything the cache will see again next frame.
        let mut preview = self.draft_cache.body.clone();
        let processed = self.draft_cache.processed;
        for line in self.draft[processed..].split('\n') {
            preview.feed(line, width, panel, &mut tail);
        }
        preview.finish(width, panel, &mut tail);
        if let Some(footer) = agent_footer(width, panel) {
            tail.push(footer);
        }
        (self.draft_cache.rows.len(), tail)
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
            self.history_index = None;
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
        self.history_index = None;
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
        self.entry_revisions.clear();
        self.entry_rows_meta.clear();
        self.draft.clear();
        self.draft_cache = DraftCache::default();
        self.tool_rows.clear();
        self.tool_cells.clear();
        self.child_cells.clear();
        self.expanded.clear();
        self.groups.clear();
        self.ro_ids.clear();
        self.active_group = None;
        self.think_row = None;
        self.plan_row = None;
        self.dash_detail = None;
        self.dash_sel = 0;
        self.top_entry = None;
        self.line_origin.clear();
        self.transcript_lines.clear();
        self.transcript_bg.clear();
        self.finished_children = 0;
        self.finished_ms = 0;
        self.last_flag.clear();
        self.search_query.clear();
        self.search_matches.clear();
        self.search_position = 0;
        self.transcript_key = None;
        self.transcript_dirty_from = Some(0);
        self.renderer.previous.clear();
        self.renderer.full_redraw = true;
        self.scroll_end();
    }

    /// Restore only the flags belonging to the active saved session. The
    /// detector's project-wide `.ctf/flags.log` remains available to the CTF
    /// verifier, but it must not leak into the interactive session panel.
    fn restore_flags(&mut self, flags: Vec<(String, String)>) {
        self.flag_log.clear();
        self.flags_found = 0;
        for (flag, source) in flags {
            self.record_flag(flag, source);
        }
    }

    fn clear_session_flags(&mut self) {
        self.flag_log.clear();
        self.flags_found = 0;
        self.last_flag.clear();
        self.flag_stream_buffers.clear();
    }

    /// Bracketed paste (spec 1.2): a short paste types through, a long one
    /// becomes an expandable chip so large pastes never replay per key.
    fn accept_paste(&mut self, text: &str) {
        let normalized: String = text.chars().filter(|ch| *ch != '\r').collect();
        let lines = normalized.lines().count().max(1);
        if lines >= 4 || normalized.len() >= 200 {
            self.paste_chip = Some((normalized, lines));
            self.status =
                format!("[Pasted {lines} lines]  Enter insert · Enter again send · Esc discard");
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

    /// Two-stage idle Ctrl+C: the first press clears the draft (or, with an
    /// empty prompt, arms a ~2s exit window with a hint); a second press
    /// inside the window quits. Returns true when the event loop should exit.
    fn ctrl_c_idle(&mut self) -> bool {
        if self.input.is_empty() {
            let recent = self
                .last_interrupt
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(2));
            self.last_interrupt = Some(std::time::Instant::now());
            if recent {
                return true;
            }
            self.status = "Ready · (press Ctrl+C again to exit)".into();
            return false;
        }
        self.last_interrupt = Some(std::time::Instant::now());
        self.clear_input();
        self.completion = None;
        self.status = "Input cleared · Ctrl+C again to exit".into();
        false
    }

    /// Insert a transcript row while keeping a scrolled-up viewport anchored.
    fn push_scrolled(&mut self, entry: Entry, added_lines: usize) {
        if self.scroll > 0 {
            self.scroll = self.scroll.saturating_add(added_lines);
            self.new_since_detach += added_lines;
        }
        let row = self.entries.len();
        self.transcript_dirty_from = Some(
            self.transcript_dirty_from
                .map_or(row, |start| start.min(row)),
        );
        self.entries.push(entry);
        self.entry_revisions.push(self.next_entry_revision);
        self.next_entry_revision = self.next_entry_revision.wrapping_add(1);
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
        let row = self.entries.len();
        self.transcript_dirty_from = Some(
            self.transcript_dirty_from
                .map_or(row, |start| start.min(row)),
        );
        self.entries.push(Entry { speaker, text });
        self.entry_revisions.push(self.next_entry_revision);
        self.next_entry_revision = self.next_entry_revision.wrapping_add(1);
    }

    fn record_flag(&mut self, flag: String, source: String) {
        if flag.trim().is_empty() || self.flag_log.iter().any(|(known, _)| known == &flag) {
            return;
        }
        self.flags_found += 1;
        self.last_flag = flag.clone();
        self.flag_log.push((flag, source));
        // Keep state bounded while the durable .ctf log remains the complete
        // history. The pinned panel displays only the newest few entries.
        if self.flag_log.len() > 256 {
            self.flag_log.remove(0);
        }
    }

    fn detect_stream_flags(&mut self, source: &str, chunk: &str) {
        if self.mode != "CTF" {
            return;
        }
        let buffer = self
            .flag_stream_buffers
            .entry(source.to_string())
            .or_default();
        buffer.push_str(chunk);
        // Wide enough that a flag split across a large streamed artifact
        // (base64 blobs, dumps) still matches; bounded so providers that
        // stream indefinitely cannot grow it without limit.
        if buffer.chars().count() > 16384 {
            *buffer = buffer
                .chars()
                .skip(buffer.chars().count() - 16384)
                .collect();
        }
        let buffer = buffer.clone();
        for hit in self.flag_detector.detect(source, &buffer) {
            self.record_flag(hit.flag, hit.source);
        }
    }

    fn apply_progress(&mut self, progress: Progress) {
        match progress {
            Progress::ToolBegin { id, title } => {
                self.dash_activity.push_back((
                    ActivityStat {
                        id: id.clone(),
                        title: crate::crash::redact(&title),
                        running: true,
                        ok: None,
                        elapsed_ms: 0,
                    },
                    std::time::Instant::now(),
                ));
                // Keep current tasks visible even after many completed calls.
                while self.dash_activity.len() > 96 {
                    if let Some(index) = self
                        .dash_activity
                        .iter()
                        .position(|(activity, _)| !activity.running)
                    {
                        self.dash_activity.remove(index);
                    } else {
                        self.dash_activity.pop_front();
                    }
                }
                self.set_task_state(TaskState::RunningTool);
                if self.first_tool_ms.is_none() {
                    self.first_tool_ms = self
                        .turn_started
                        .map(|started| started.elapsed().as_millis() as u64);
                }
                self.status = "Tool active".into();
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
                if self.verbosity == "compact"
                    && (title.starts_with("Read ") || title.starts_with("Searched "))
                {
                    let is_read = title.starts_with("Read ");
                    let row = match self.active_group {
                        Some(row) => row,
                        None => {
                            let row = self.entries.len();
                            // The increment below counts the first call as well.
                            let group = GroupCell::default();
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
                    let updated = self.groups.get_mut(&row).map(|group| {
                        group.inflight += 1;
                        group_text(group)
                    });
                    if let Some(text) = updated {
                        self.set_entry_text(row, text);
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
                        text: format!("◌ {title}"),
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
                if let Some((activity, _)) = self
                    .dash_activity
                    .iter_mut()
                    .rev()
                    .find(|(activity, _)| activity.id == id && activity.running)
                {
                    activity.running = false;
                    activity.ok = Some(ok);
                    activity.elapsed_ms = elapsed_ms.min(u64::MAX as u128) as u64;
                }
                self.detect_stream_flags(&format!("tool:{id}"), &output);
                if self.inflight_tools <= 1 {
                    self.set_task_state(if ok {
                        TaskState::WaitingForModel
                    } else {
                        TaskState::Failed
                    });
                    self.status = if ok {
                        "Processing result"
                    } else {
                        "Tool failed"
                    }
                    .into();
                }
                if !ok {
                    self.tool_failures += 1;
                }
                self.inflight_tools = self.inflight_tools.saturating_sub(1);
                self.turn_tool_ms += elapsed_ms;
                self.tool_ms_total += elapsed_ms;
                // Nested child: update its row under the root cell. Finished
                // children collapse into the tree summary line instead of
                // growing the tree pane without bound.
                if id.contains('/') {
                    if let Some(cell) = self.child_cells.get_mut(&id) {
                        if cell.running {
                            cell.running = false;
                            self.finished_children += 1;
                            self.finished_ms += elapsed_ms;
                        }
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
                }
            }
            Progress::Plan(items) => {
                self.plan = items;
                let complete = self.plan.iter().filter(|(_, done)| *done).count();
                let mut body = format!("☷ PLAN  {complete}/{} complete", self.plan.len());
                for (step, done) in self.plan.iter().take(8) {
                    let safe = crate::crash::redact(&step.replace(['\n', '\r'], " "));
                    let mark = if *done { "✓" } else { "○" };
                    body.push_str(&format!("\n   {mark} {safe}"));
                }
                if self.plan.len() > 8 {
                    body.push_str(&format!("\n   +{} more steps", self.plan.len() - 8));
                }
                if let Some(row) = self.plan_row {
                    self.set_entry_text(row, body);
                } else {
                    let row = self.entries.len();
                    self.push(Speaker::System, body);
                    self.plan_row = Some(row);
                }
            }
            Progress::Tool(message) => {
                self.finalize_think();
                self.push(Speaker::Tool, message);
            }
            Progress::Heartbeat => {
                if self.inflight_tools == 0 && self.draft.is_empty() {
                    self.status = "Model active".into();
                }
            }
            Progress::ModelStage(stage) => {
                // The stage is recorded even while tools are live: a stage
                // announces what the model is doing next, and dropping it
                // whenever a tool is in flight loses the only progress
                // signal a long turn gives.
                self.stage = stage.clone();
                if self.inflight_tools == 0 {
                    self.set_task_state(TaskState::WaitingForModel);
                    self.status = stage;
                }
            }
            Progress::ResetText => {
                // Reset means replacement, not completion. Provider retries and
                // forced-tool recovery must discard a partial failed stream.
                self.finalize_think();
                self.draft.clear();
            }
            Progress::TextDelta(piece) => {
                self.finalize_think();
                self.detect_stream_flags("model stream", &piece);
                self.set_task_state(TaskState::Streaming);
                if self.first_token_ms.is_none() {
                    self.first_token_ms = self
                        .turn_started
                        .map(|started| started.elapsed().as_millis() as u64);
                }
                self.status = "Streaming".into();
                // The model is producing output now, so the last announced
                // stage ("Sending prompt to model") is spent.
                self.stage = "Streaming".into();
                self.draft.push_str(&crate::crash::redact(&piece));
            }
            Progress::Usage(usage) => {
                self.turn_usage.add(usage);
                self.usage.add(usage);
            }
            Progress::Metrics(metrics) => self.metrics = metrics,
            Progress::FlagFound { flag, source } => {
                if self.mode != "CTF" {
                    return;
                }
                self.finalize_think();
                self.record_flag(flag.clone(), source.clone());
                self.push(
                    Speaker::System,
                    format!(
                        "🚩 FLAG DETECTED  {flag}\n   source: {source} · logged in .ctf/flags.log"
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

    /// Full text for a tool cell: header, nested child tools, and either the
    /// collapsed preview or the expanded output.
    fn cell_text(&self, id: &str, cell: &ToolCell) -> String {
        // Always redact displayed tool titles: command arguments can contain
        // environment credentials and verbose mode must not echo them.
        let title = crate::crash::redact(&display_tool_title(&cell.title));
        let mut out = if cell.running {
            let ms = self
                .dash_activity
                .iter()
                .rev()
                .find(|(activity, _)| activity.id == id && activity.running)
                .map(|(_, started)| started.elapsed().as_millis())
                .unwrap_or(0);
            format!(
                "◌  {title}   {}",
                format_duration_ms(ms.min(u64::MAX as u128) as u64)
            )
        } else {
            let icon = if cell.ok { "✓" } else { "×" };
            format!(
                " {icon}  {title}   {}",
                format_duration_ms(cell.elapsed_ms.min(u64::MAX as u128) as u64)
            )
        };
        if !id.contains('/') {
            out.push_str(&self.child_lines(id, 1));
        }
        if cell.running {
            // Show only a shell's own latest line, not unrelated process
            // output, and always redact credentials before displaying it.
            if let Some(cmd) = cell
                .title
                .strip_prefix("Ran shell: ")
                .or_else(|| cell.title.strip_prefix("Ran shell "))
            {
                if let Some(proc) = self
                    .dash_procs
                    .iter()
                    .rev()
                    .find(|proc| proc.running && proc.command.trim() == cmd.trim())
                {
                    if self.verbosity == "verbose" {
                        // Procfs values approximate the tracked command's
                        // active shell+subprocess tree, not system-wide usage.
                        let cpu = proc
                            .cpu_pct
                            .map(|value| format!("{value:.1}%"))
                            .unwrap_or_else(|| "n/a".into());
                        let ram = proc
                            .rss_kb
                            .map(|kb| format!("{:.1} MiB", kb as f64 / 1024.0))
                            .unwrap_or_else(|| "n/a".into());
                        out.push_str(&format!(
                            "\n     ↳ PID {} · CPU {} · RAM {} (process tree)",
                            proc.pid, cpu, ram,
                        ));
                    }
                    let limit = if self.verbosity == "verbose" { 2 } else { 1 };
                    let last: Vec<_> = proc
                        .tail
                        .lines()
                        .rev()
                        .filter(|line| !line.trim().is_empty())
                        .take(limit)
                        .collect();
                    for line in last.into_iter().rev() {
                        let safe = crate::crash::redact(line);
                        let safe: String = safe.chars().filter(|ch| !ch.is_control()).collect();
                        out.push_str(&format!("\n     ↳ {}", clip(&safe, 105)));
                    }
                }
            }
            return out;
        }
        let expanded = self.expanded.contains(&cell.row);
        if !cell.ok || expanded || self.verbosity == "verbose" {
            out.push_str(&expanded_block(&cell.output, cell.exit_code));
        } else {
            let lines = clean_output_lines(&cell.output);
            let content: Vec<&str> = lines
                .iter()
                .map(String::as_str)
                .filter(|line| !line.starts_with("exit="))
                .collect();
            if let Some(last) = content.last() {
                let safe = crate::crash::redact(last);
                let safe: String = safe.chars().filter(|ch| !ch.is_control()).collect();
                out.push_str(&format!(
                    "\n     ↳ {} line{} · {}",
                    content.len(),
                    if content.len() == 1 { "" } else { "s" },
                    clip(&safe, 90)
                ));
            } else if cell.exit_code.is_some() {
                out.push_str("\n     ↳ Completed · expand for details");
            }
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
                "◌"
            } else if child.ok {
                "✓"
            } else {
                "×"
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
            activities: self
                .dash_activity
                .iter()
                .rev()
                .map(|(activity, started)| {
                    let mut visible = activity.clone();
                    if visible.running {
                        visible.elapsed_ms =
                            started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    }
                    visible
                })
                .collect(),
            stage: self.stage.clone(),
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
            0 => self.dash_activity.len() + self.dash_procs.len(),
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
                // Operation details precede shell PIDs in the mixed panel.
                if let Some((activity, started)) =
                    self.dash_activity.iter().rev().nth(self.dash_sel)
                {
                    let elapsed_ms = if activity.running {
                        started.elapsed().as_millis().min(u64::MAX as u128) as u64
                    } else {
                        activity.elapsed_ms
                    };
                    let state = if activity.running {
                        "running"
                    } else if activity.ok == Some(true) {
                        "completed"
                    } else {
                        "failed"
                    };
                    let body = format!(
                        "tool {}\nid {}\nstate {}\nelapsed {:.2}s\n\nTool output: see its transcript cell (Enter/Ctrl+O).\nThis agent operation does not have its own shell PID.",
                        activity.title, activity.id, state, elapsed_ms as f64 / 1000.0
                    );
                    self.dash_detail = Some(("AGENT ACTIVITY".into(), body));
                    return;
                }
                let proc_index = self.dash_sel.saturating_sub(self.dash_activity.len());
                let selected = self.dash_procs.iter().rev().nth(proc_index).cloned();
                if let Some(proc) = selected {
                    let mut body = format!(
                        "pid {}\ncommand {}\ncwd {}\nage {}s\nstatus {}\ncpu {} · rss {}\n\nOUTPUT (live tail)\n",
                        proc.pid,
                        crate::crash::redact(&proc.command),
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
                        crate::crash::redact(&proc.tail)
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
        if self.dash_sel < self.dash_activity.len() {
            self.status = "Agent activity is not an OS process".into();
            return;
        }
        let selected = self
            .dash_procs
            .iter()
            .rev()
            .nth(self.dash_sel - self.dash_activity.len())
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
        let started = std::time::Instant::now();
        let (width, height) = terminal::size()?;
        let (frame, cursor) = self.compose(width, height);
        let outcome = self.renderer.draw(&frame, cursor, (width, height));
        // Slow frames go to the trace log; every frame would drown it.
        let ms = started.elapsed().as_millis();
        if ms > 16 {
            self.trace.log("render", ms, &format!("{width}x{height}"));
        }
        outcome
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
        let show_logo = self.entries.len() <= 1
            && self.input.is_empty()
            && !self.busy
            && self.tool_cells.is_empty()
            && self.finished_children == 0;
        let mut body_start = 0usize;
        if show_logo {
            let spin = SPINNER[self.spinner % SPINNER.len()];
            // Narrow terminals get a one-line wordmark instead of the block art.
            let (lines, wide) = if width_usize < 80 {
                (
                    vec![format!("WROSECODE v{}", env!("CARGO_PKG_VERSION"))],
                    false,
                )
            } else {
                (LOGO.iter().map(|line| (*line).to_string()).collect(), true)
            };
            for line in &lines {
                if body_start >= height_usize {
                    break;
                }
                // Keep the block wordmark on its own rows. Appending the
                // version to a logo row made the row exceed narrow usable
                // widths and could visually clip the final `D`.
                let text = clip(line, width_usize);
                let mut row = Row::new(text, colors.accent);
                row.colors = gradient_colors(&row.text, self.colors);
                frame[body_start] = row;
                body_start += 1;
            }
            if wide && body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        &format!("  WROSECODE v{}", env!("CARGO_PKG_VERSION")),
                        width_usize,
                    ),
                    colors.muted,
                );
                body_start += 1;
            }
            if body_start < height_usize && height_usize >= 16 {
                frame[body_start] = Row::new("", colors.muted);
                body_start += 1;
            }
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        &format!("  model    {} / {}", self.provider, self.model),
                        width_usize,
                    ),
                    colors.muted,
                );
                body_start += 1;
            }
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(&format!("  project  {}", self.root), width_usize),
                    colors.muted,
                );
                body_start += 1;
            }
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        &format!(
                            "  status   {}{}",
                            startup_connection(&self.provider_health),
                            if self.permission == "yolo" {
                                " · YOLO"
                            } else {
                                ""
                            },
                        ),
                        width_usize,
                    ),
                    colors.status,
                );
                body_start += 1;
            }
            if body_start < height_usize {
                frame[body_start] = Row::new(
                    clip(
                        &format!("  {spin} ready   type a request below"),
                        width_usize,
                    ),
                    colors.accent,
                );
                body_start += 1;
            }
        } else if body_start < height_usize {
            // Minimal chrome: brand · mode/state · provider/model ·
            // connection. Session and project live in Ctrl+B; the bar never
            // wraps and never carries debug numbers.
            let online = if self.provider_health.contains("unavailable")
                || self.provider_health.starts_with('0')
            {
                "○ Offline"
            } else {
                "● Online"
            };
            let state_word = if self.busy { "WORKING" } else { "READY" };
            let studio = studio_panel(colors.panel);
            let mut header = if studio {
                format!(
                    "  ◈ WROSECODE    ▸ {}   ·  {}",
                    self.mode,
                    if self.busy { "Working" } else { "Ready" }
                )
            } else {
                format!(" WROSECODE  ◈ {} · {}", self.mode, state_word)
            };
            if self.permission == "yolo" {
                header.push_str("  [YOLO]");
            }
            if width_usize >= 80 {
                let model_short = clip(&self.model, 28);
                header.push_str(&format!("   {}/{}  {}", self.provider, model_short, online));
            }
            if width_usize >= 112 && self.verbosity == "verbose" {
                header.push_str("  · VERBOSE");
            }
            let mut heading = Row::new(
                clip(&header, width_usize),
                if studio {
                    colors.muted
                } else if self.busy {
                    colors.status
                } else {
                    colors.accent
                },
            );
            if studio && !self.no_color {
                heading.colors = Some(
                    heading
                        .text
                        .chars()
                        .enumerate()
                        .map(|(index, _)| {
                            if index < 14 {
                                colors.accent
                            } else {
                                colors.muted
                            }
                        })
                        .collect(),
                );
            }
            frame[body_start] = heading;
            body_start += 1;
            // Thin rule under the bar; violet-tinted via muted so it stays
            // subtle on every palette.
            if body_start < height_usize && height_usize >= 24 {
                let rule = "─".repeat(width_usize.min(240));
                frame[body_start] = Row::new(clip(&rule, width_usize), colors.muted);
                body_start += 1;
            }
        }
        if body_start < height_usize {
            // Quiet breathing room below the app bar; avoid the old
            // full-width separator that made every message feel boxed.
            frame[body_start] = Row::new("", colors.muted);
            body_start += 1;
        }
        // Bottom-up composer geometry FIRST (pure arithmetic, no painting):
        // footer(1) + box bottom(1) + info(1) + input + chip(0/1) + box top.
        // Everything above — flags panel, transcript — must yield to the
        // rows this reserves; the composer is never overwritten.
        let compact_h = height_usize < 24;
        let footer_rows = 1usize;
        let info_rows = 1usize;
        let rule_rows = 1usize;
        let think_rows = usize::from(self.think_bar_visible());
        // Input text budget inside the box walls: `│ > ` … ` │`.
        let input_width = width_usize.saturating_sub(6).max(1);
        let input_visual_lines = wrap_input(&self.input, input_width);
        let input_rows = input_visual_lines.len().max(1);
        let chip_rows = usize::from(self.paste_chip.is_some());
        let min_input_rows = if height_usize >= 10 && !compact_h {
            3
        } else {
            1
        };
        // Cap the editor at eight rows on tall terminals (four when
        // compact). Short terminals shrink it to the space actually
        // available instead of painting over output.
        let input_cap = if compact_h { 4 } else { 8 };
        let input_capacity = height_usize
            .saturating_sub(
                body_start + rule_rows + chip_rows + info_rows + 1 + footer_rows + think_rows,
            )
            .clamp(1, input_cap);
        let visible_input_rows = input_rows.max(min_input_rows).min(input_capacity);
        let (cursor_row, _) = input_cursor_position(&input_visual_lines, self.cursor);
        let max_input_scroll = input_rows.saturating_sub(visible_input_rows);
        self.input_scroll = self.input_scroll.min(max_input_scroll);
        if cursor_row < self.input_scroll {
            self.input_scroll = cursor_row;
        } else if cursor_row >= self.input_scroll + visible_input_rows {
            self.input_scroll = cursor_row + 1 - visible_input_rows;
        }
        let box_bottom_row = height_usize.saturating_sub(footer_rows + 1);
        let info_row = box_bottom_row.saturating_sub(1);
        let input_top = info_row
            .saturating_sub(visible_input_rows)
            .max(usize::from(height_usize > 1));
        let chip_row = input_top.saturating_sub(1);
        let rule_row = chip_row.saturating_sub(chip_rows);
        if self.mode == "CTF" && !self.flag_log.is_empty() && body_start < height_usize {
            // Pinned accuracy panel: always the newest session flags (5 on
            // roomy screens, 2 when short), full text wrapped across rows —
            // never silently clipped — plus an exact overflow count. Every
            // row stays strictly above `panel_end`, which the bottom-up
            // layout reserved for the composer: the panel yields, the
            // composer never gets overwritten.
            let inner = width_usize.saturating_sub(4).max(1);
            let slots = if height_usize.saturating_sub(body_start + 4) >= 7 {
                5
            } else {
                2
            };
            let total = self.flag_log.len();
            let start = total.saturating_sub(slots);
            // Last row the panel may own; the composer owns `panel_end`
            // upward from here via rule/chip/input rows... see the bottom-up
            // geometry above (`rule_row - think_rows`).
            let panel_end = rule_row.saturating_sub(think_rows);
            if body_start >= panel_end {
                // No room even for the title: skip the panel entirely rather
                // than paint half of it into the composer.
            } else {
                frame[body_start] = Row::new(
                    clip(
                        "╭─ 🚩 FLAGS FOUND ─────────────────────────────",
                        width_usize,
                    ),
                    colors.accent,
                );
                body_start += 1;
            }
            let mut shown = 0usize;
            for (flag, source) in &self.flag_log[start..] {
                // One spare row stays free for the count/close line.
                if body_start + 1 >= panel_end {
                    break;
                }
                // One flag owns up to 3 wrapped text rows plus an explicit
                // overflow marker, so a giant body cannot flood the panel
                // while nothing is ever silently cut.
                let parts = wrap(flag, inner);
                let overflow = parts.len() > 3;
                for (index, part) in parts.into_iter().take(3).enumerate() {
                    if body_start + 1 >= panel_end {
                        break;
                    }
                    let prefix = if index == 0 { "│ " } else { "│   " };
                    frame[body_start] =
                        Row::new(clip(&format!("{prefix}{part}"), width_usize), colors.status);
                    body_start += 1;
                }
                if overflow && body_start + 1 < panel_end {
                    frame[body_start] = Row::new(
                        clip(
                            &format!("│   ⋯ +{} chars", flag.chars().count()),
                            width_usize,
                        ),
                        colors.muted,
                    );
                    body_start += 1;
                }
                if body_start + 1 < panel_end {
                    frame[body_start] = Row::new(
                        clip(&format!("│   ↳ {}", source), width_usize),
                        colors.muted,
                    );
                    body_start += 1;
                }
                shown += 1;
            }
            let hidden = total.saturating_sub(shown);
            if hidden > 0 && body_start < panel_end {
                frame[body_start] = Row::new(
                    clip(
                        &format!("│ +{hidden} more · /flags for history"),
                        width_usize,
                    ),
                    colors.muted,
                );
                body_start += 1;
            } else if body_start < panel_end {
                frame[body_start] = Row::new(
                    clip("╰────────────────────────────────────────────", width_usize),
                    colors.accent,
                );
                body_start += 1;
            }
        }
        // Transcript bounds from the precomputed bottom-up geometry: the
        // think bar sits directly above the composer box top, so the
        // transcript ends one row earlier while it is visible.
        let mut body_end = rule_row
            .saturating_sub(think_rows)
            .clamp(body_start, height_usize);
        // Tiny terminals: the transcript collapses to zero rows; header,
        // composer, and footer keep their own rows without overlap.
        if body_end < body_start {
            body_end = body_start.min(height_usize.saturating_sub(1));
        }
        let body_height = body_end.saturating_sub(body_start);
        if show_logo {
            self.view = TranscriptView {
                body_start,
                transcript_start: 0,
                height: 0,
            };
            self.top_entry = None;
            self.max_scroll = 0;
            self.scroll = 0;
            self.page_size = body_height.max(1);
        }
        // On wide terminals display live operations beside the conversation.
        // Narrow terminals keep the full-width transcript.
        let show_activity = self.activity_rail
            && width_usize >= 110
            && body_height >= 10
            && !show_logo
            && self.tool_calls > 0
            && self.picker.is_none()
            && !self.palette;
        let rail_width = if show_activity {
            (width_usize / 3).clamp(34, 46)
        } else {
            0
        };
        let transcript_width = width_usize.saturating_sub(rail_width + usize::from(show_activity));
        let transcript_height = if show_logo { 0 } else { body_height };
        // Wrapped rows come from the cache; scrolling reslices instead of
        // re-wrapping, and only the visible window is ever read below. The
        // transcript owns the full terminal width: no rail, no reservation.
        let verbose = self.verbosity == "verbose";
        let panel = input_panel_bg(colors);
        if !show_logo {
            self.ensure_transcript(transcript_width, verbose, panel);
        }
        let base = self.transcript_lines.len();
        // The live draft only joins the view when the viewport is pinned to
        // the bottom; otherwise its growth would shift a scrolled-up view.
        // Completed lines come from the incremental cache; only the trailing
        // partial line is wrapped per frame.
        let (draft_cached, draft_tail) = if self.scroll == 0 && !self.draft.is_empty() {
            self.prepare_draft(transcript_width, verbose, panel)
        } else {
            (0, Vec::new())
        };
        let total_lines = base + draft_cached + draft_tail.len();
        if !show_logo {
            self.max_scroll = total_lines.saturating_sub(transcript_height);
            self.scroll = self.scroll.min(self.max_scroll);
            self.page_size = transcript_height.max(1);
        }
        let transcript_end = total_lines.saturating_sub(self.scroll);
        let transcript_start = transcript_end.saturating_sub(transcript_height);
        let mut visible: Vec<&str> = Vec::with_capacity(transcript_height);
        for index in transcript_start..transcript_end {
            if index < base {
                if let Some(line) = self.transcript_lines.get(index) {
                    visible.push(line.as_str());
                }
            } else {
                let offset = index - base;
                if let Some(row) = self.draft_cache.rows.get(offset) {
                    visible.push(row.0.as_str());
                } else if let Some(row) = offset
                    .checked_sub(draft_cached)
                    .and_then(|tail_index| draft_tail.get(tail_index))
                {
                    visible.push(row.0.as_str());
                }
            }
        }
        // Remember where the transcript sits so a click (or Ctrl+O) can map a
        // terminal row back to its entry: line `transcript_start` paints on
        // frame row `body_start`.
        if !show_logo {
            self.view = TranscriptView {
                body_start,
                transcript_start,
                height: transcript_height,
            };
            self.top_entry = self.line_origin.get(transcript_start).copied();
        }

        // Every transcript content row that fits, full width, no header row:
        // the conversation starts immediately under the app bar.
        for row in 0..transcript_height {
            let line = visible
                .get(row)
                .map(|line| (*line).to_string())
                .unwrap_or_default();
            let mut painted = Row::new(clip(&line, transcript_width), colors.text);
            let idx = transcript_start + row;
            if idx < base {
                if let Some(entry_index) = self.line_origin.get(idx) {
                    let entry = &self.entries[*entry_index];
                    let (mut fg, style_bg) = transcript_style(entry, colors);
                    // Charcoal containers (code fences, tool output, snapshot)
                    // override the entry background per row.
                    let bg = self
                        .transcript_bg
                        .get(idx)
                        .copied()
                        .flatten()
                        .unwrap_or(style_bg);
                    let is_first = idx == 0 || self.line_origin.get(idx - 1) != Some(entry_index);
                    // Tool details echo their title's state in a dimmer shade:
                    // green on green, amber on amber, red on red.
                    if !self.no_color && matches!(entry.speaker, Speaker::Tool) && !is_first {
                        fg = if studio_panel(colors.panel) {
                            colors.muted
                        } else if entry.text.starts_with('✗') {
                            WROSE_ERROR
                        } else if entry.text.starts_with('◌') {
                            WROSE_AMBER
                        } else {
                            WROSE_DIM_GREEN
                        };
                    }
                    if !self.no_color
                        && studio_panel(colors.panel)
                        && matches!(entry.speaker, Speaker::Agent)
                        && is_first
                    {
                        fg = colors.accent;
                    }
                    // The user marker keeps the cyan accent while the prompt
                    // itself reads soft white.
                    if !self.no_color && matches!(entry.speaker, Speaker::User) && is_first {
                        painted.colors = Some(vec![colors.accent, colors.accent]);
                    }
                    painted.fg = fg;
                    painted.bg = bg;
                } else {
                    painted.bg = transcript_panel_bg(colors);
                }
            } else {
                let offset = idx - base;
                let bg = self
                    .draft_cache
                    .rows
                    .get(offset)
                    .and_then(|row| row.1)
                    .or_else(|| {
                        offset
                            .checked_sub(draft_cached)
                            .and_then(|tail_index| draft_tail.get(tail_index))
                            .and_then(|row| row.1)
                    });
                painted.bg = bg.unwrap_or_else(|| transcript_panel_bg(colors));
            }
            if !show_logo {
                frame[body_start + row] = painted;
            }
        }
        // Append the activity rail only to transcript rows. The composer
        // stays full width and all colors remain attached to their own cells.
        if show_activity {
            // Process snapshots carry a bounded live stdout/stderr tail.
            // Sample on a 150ms clock, without the more expensive git scan.
            if self
                .dash_data_at
                .is_none_or(|at| at.elapsed() >= Duration::from_millis(150))
            {
                self.dash_procs = crate::tools::shell::proc_snapshots();
                self.dash_data_at = Some(std::time::Instant::now());
            }
            let rail = activity_lines(self, rail_width, colors);
            for index in 0..transcript_height {
                let row = &mut frame[body_start + index];
                let left = pad_row(&row.text, transcript_width);
                let (activity, activity_fg, _) = rail
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), colors.muted, colors.background));
                let right = clip(&activity, rail_width);
                if !self.no_color {
                    let mut palette = row
                        .colors
                        .take()
                        .unwrap_or_else(|| vec![row.fg; row.text.chars().count()]);
                    palette.resize(transcript_width, row.fg);
                    palette.push(colors.muted);
                    palette.extend(std::iter::repeat_n(activity_fg, right.chars().count()));
                    row.colors = Some(palette);
                }
                row.text = format!("{left}│{right}");
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
        // Boxed composer geometry, resolved bottom-up so nothing overlaps:
        // footer(1) + box bottom(1) + info(1) + input + chip(0/1) + box top.
        // The transcript gets whatever rows remain above the box top.
        // (`input_top`, `chip_row`, `rule_row`, and `body_end` were resolved
        // with the layout above so the transcript and palette agree.)
        // Slim think status bar: one quiet row above the composer box while
        // a thinking level is active. No telemetry, just the live mode.
        if think_rows == 1 && body_end >= body_start && body_end < rule_row {
            let bar = format!(
                " ◈ THINK · {} · {}/20",
                self.think_label(),
                self.thinking_level.min(20),
            );
            let mut row = Row::new(clip(&bar, width_usize), colors.accent);
            row.bg = transcript_panel_bg(colors);
            if body_end < height_usize {
                frame[body_end] = row;
            }
        }
        // The composer's box top doubles as the live activity line: working
        // state, queued follow-ups, or the detached-view jump hint. There is
        // no separate telemetry bar.
        let rule_label = if self.busy {
            if let Some((activity, started)) = self
                .dash_activity
                .iter()
                .rev()
                .find(|(activity, _)| activity.running)
            {
                let name = display_tool_title(&activity.title);
                format!(
                    "◌ {}  ·  {:.1}s  ·  {} live  ·  Esc cancel",
                    clip(&name, 46),
                    started.elapsed().as_secs_f64(),
                    self.inflight_tools
                )
            } else if self.pending.is_empty() {
                format!(
                    "◌ {}  ·  Esc cancel",
                    clip(
                        if self.stage.is_empty() {
                            "Thinking"
                        } else {
                            &self.stage
                        },
                        55
                    )
                )
            } else {
                format!("◌ Working  ·  {} queued  ·  Esc cancel", self.pending.len())
            }
        } else if !self.is_following() && self.max_scroll > 0 {
            self.follow_marker().trim().to_string()
        } else {
            String::new()
        };
        // Focus chrome: the box walls glow cyan while editing, quiet gray
        // while navigating. Computed once per frame.
        let border = if composer_focused(self) && !self.no_color {
            colors.accent
        } else {
            colors.muted
        };
        if rule_row >= body_start && rule_row < input_top && rule_row < height_usize {
            let mut rule = Row::new(
                box_top_row(width_usize, &rule_label),
                if rule_label.is_empty() {
                    border
                } else {
                    colors.accent
                },
            );
            rule.bg = input_panel_bg(colors);
            frame[rule_row] = rule;
        }
        if let Some((_, pasted_lines)) = &self.paste_chip {
            if chip_row >= body_start && chip_row < input_top && chip_row < height_usize {
                let mut chip = Row::new(
                    clip(
                        &format!(
                            " [Pasted {pasted_lines} lines]  Enter insert · Enter again send · Esc discard"
                        ),
                        width_usize,
                    ),
                    colors.accent,
                );
                chip.bg = input_panel_bg(colors);
                frame[chip_row] = chip;
            }
        }
        // The input rows live inside the box: `│ > … │` over the charcoal
        // panel, one distinct editing surface.
        for frame_row in frame
            .iter_mut()
            .take(info_row.min(height_usize))
            .skip(input_top)
        {
            let mut row = Row::new("", colors.muted);
            row.bg = input_panel_bg(colors);
            *frame_row = row;
        }
        let rendered_input_rows = input_rows.min(visible_input_rows);
        let input_content_top = input_top + visible_input_rows.saturating_sub(rendered_input_rows);
        let input_color = input_syntax_color(&self.input, colors);
        let (cursor_row, cursor_column) = input_cursor_position(&input_visual_lines, self.cursor);
        let mut caret_x = 4usize + cursor_column;
        // Empty-draft placeholder; the persistent info line below always
        // carries mode and model.
        let placeholder = self.input.is_empty() && self.paste_chip.is_none();
        let placeholder_text = "Ask WROSECODE anything...".to_string();
        // Inner width between the box walls.
        let inner = width_usize.saturating_sub(4).max(1);
        // Text budget inside `│ > ` … ` │`.
        let text_width = inner.saturating_sub(2).max(1);
        for (index, line) in input_visual_lines
            .iter()
            .enumerate()
            .skip(self.input_scroll)
            .take(visible_input_rows)
        {
            let slot = input_content_top - input_top + index - self.input_scroll;
            let row_index = input_top + slot;
            let prefix = if line.start == 0 { "│ > " } else { "│   " };
            let (visible, fg) = if self.mask_input {
                (
                    line.text.chars().map(|_| '•').collect::<String>(),
                    input_color,
                )
            } else if placeholder && index == self.input_scroll {
                (clip(&placeholder_text, text_width), colors.muted)
            } else {
                (clip(&line.text, text_width), input_color)
            };
            let padded = format!(
                "{visible}{}",
                " ".repeat(text_width.saturating_sub(visible.chars().count()))
            );
            let mut input_row = Row::new(clip(&format!("{prefix}{padded} │"), width_usize), fg);
            input_row.bg = input_panel_bg(colors);
            paint_box_walls(&mut input_row, border, self.no_color);
            // `row_index` always lands inside the cleared input window: the
            // topmost row is `input_top` and the window holds exactly
            // `visible_input_rows` rows below the info line.
            if let Some(row) = frame.get_mut(row_index) {
                *row = input_row;
            }
            if index == cursor_row {
                caret_x = 4 + cursor_column;
            }
        }
        // Persistent composer info line inside the box: actual mode,
        // provider/model, and the command hint. The model is clipped first
        // so the line never wraps.
        if info_row > input_top && info_row < height_usize {
            let left = format!(
                "◈ {} · {}/{}",
                self.mode,
                self.provider,
                clip(&self.model, 32)
            );
            let hint = "/ for commands";
            let gap = inner
                .saturating_sub(left.chars().count())
                .saturating_sub(hint.len() + 2)
                .max(1);
            let content = clip(&format!("{left}{}  {hint}", " ".repeat(gap)), inner);
            let padded = format!(
                "{content}{}",
                " ".repeat(inner.saturating_sub(content.chars().count()))
            );
            let mut info = Row::new(clip(&format!("│ {padded} │"), width_usize), colors.muted);
            info.bg = input_panel_bg(colors);
            paint_box_walls(&mut info, border, self.no_color);
            frame[info_row] = info;
        }
        // Box bottom; skipped only when the viewport is too short to hold it.
        if box_bottom_row > input_top && box_bottom_row < height_usize {
            let mut bottom = Row::new(box_bottom_row_text(width_usize), border);
            bottom.bg = input_panel_bg(colors);
            frame[box_bottom_row] = bottom;
        }
        // Minimal footer: only shortcuts that actually exist (Tab toggles
        // the mode on an empty prompt, Ctrl+P opens the palette). Never
        // overlaps: it owns the last row and is clipped, never wrapped.
        if height_usize > 0 {
            let footer = if width_usize < 60 {
                "/ commands    Esc cancel".to_string()
            } else {
                if studio_panel(colors.panel) {
                    "  Tab mode   ·   Ctrl+P commands   ·   Ctrl+O details   ·   /activity panel   ·   Esc cancel".to_string()
                } else {
                    "Tab mode    Ctrl+P palette    / commands    Esc cancel".to_string()
                }
            };
            let mut row = Row::new(clip(&footer, width_usize), colors.muted);
            row.bg = transcript_panel_bg(colors);
            frame[height_usize - 1] = row;
        }
        // Opaque viewport: every cell carries an explicit background so no
        // terminal transparency or stale glyphs show through. Terminals with
        // forced transparency still composite, but WROSECODE never leaves a
        // Reset cell behind (set opaque in GNOME Terminal / Konsole /
        // Windows Terminal profile settings for a fully solid result).
        for row in &mut frame {
            if row.bg == Color::Reset {
                row.bg = colors.background;
            }
        }
        let x = caret_x.min(width_usize.saturating_sub(1)) as u16;
        let y = input_content_top
            .saturating_add(cursor_row.saturating_sub(self.input_scroll))
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
        self.history_index = None;
    }
    fn backspace(&mut self) {
        if let Some((index, _)) = self.input[..self.cursor].char_indices().last() {
            self.input.remove(index);
            self.cursor = index;
            self.history_index = None;
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
        self.input_scroll = 0;
    }
    fn take_input(&mut self) -> String {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.input_scroll = 0;
        self.history_index = None;
        line
    }

    fn line_home(&mut self) {
        self.cursor = self.input[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    fn line_end(&mut self) {
        self.cursor = self.input[self.cursor..]
            .find('\n')
            .map(|index| self.cursor + index)
            .unwrap_or(self.input.len());
    }

    fn move_vertical(&mut self, down: bool, terminal_width: usize) -> bool {
        let rows = wrap_input(&self.input, terminal_width.saturating_sub(5).max(1));
        let (row, column) = input_cursor_position(&rows, self.cursor);
        let target = if down {
            row.checked_add(1).filter(|next| *next < rows.len())
        } else {
            row.checked_sub(1)
        };
        let Some(target) = target else {
            return false;
        };
        let target_line = &rows[target];
        self.cursor = target_line
            .text
            .char_indices()
            .nth(column)
            .map(|(offset, _)| target_line.start + offset)
            .unwrap_or(target_line.end);
        true
    }
}

fn display_tool_title(title: &str) -> String {
    let title = title.trim();
    if let Some(command) = title
        .strip_prefix("Ran shell: ")
        .or_else(|| title.strip_prefix("Ran shell "))
    {
        format!("Run  {command}")
    } else if let Some(query) = title
        .strip_prefix("Searched grep ")
        .or_else(|| title.strip_prefix("Searched glob "))
        .or_else(|| title.strip_prefix("Searched "))
    {
        format!("Search  {query}")
    } else if let Some(path) = title
        .strip_prefix("Read read_file ")
        .or_else(|| title.strip_prefix("Read "))
    {
        format!("Read  {path}")
    } else if let Some(path) = title
        .strip_prefix("Edited search_replace ")
        .or_else(|| title.strip_prefix("Edited edit_file "))
        .or_else(|| title.strip_prefix("Edited write_file "))
        .or_else(|| title.strip_prefix("Edited "))
    {
        format!("Edit  {path}")
    } else if let Some(items) = title.strip_prefix("Plan update_plan ") {
        format!("Plan  {items}")
    } else if let Some(task) = title.strip_prefix("Delegated delegate_task ") {
        format!("Agent  {task}")
    } else {
        title.to_string()
    }
}

/// Build the persistent activity rail used by the wide layout. The rail is
/// deliberately derived from UI state, so progress events replace cells in
/// place instead of appending another log line. Each tuple is one terminal
/// row: text, foreground, background.
fn activity_lines(ui: &Ui, width: usize, colors: &Theme) -> Vec<(String, Color, Color)> {
    let width = width.max(8);
    let muted_bg = activity_bg(colors);
    let mut rows = Vec::new();
    let mut add = |text: String, fg: Color, bg: Color| {
        rows.push((clip(&text, width), fg, bg));
    };
    add(" ACTIVITY".into(), colors.accent, muted_bg);
    add(
        format!(
            " {}  {}",
            if ui.busy { "● RUNNING" } else { "○ READY" },
            ui.status
        ),
        if ui.busy { colors.status } else { colors.muted },
        muted_bg,
    );
    add("────────────────────────".into(), colors.muted, muted_bg);
    add(
        format!(
            " stage  {}",
            if ui.stage.is_empty() {
                &ui.status
            } else {
                &ui.stage
            }
        ),
        colors.muted,
        muted_bg,
    );
    add(format!(" task  {}", ui.category), colors.text, muted_bg);
    add(
        format!(" step  {}  ·  {} live", ui.turn_steps, ui.inflight_tools),
        colors.text,
        muted_bg,
    );
    add(
        format!(
            " usage {} in / {} out",
            compact_number(ui.usage.input),
            compact_number(ui.usage.output)
        ),
        colors.muted,
        muted_bg,
    );
    add("────────────────────────".into(), colors.muted, muted_bg);
    add(" LIVE TOOLS".into(), colors.accent, tool_panel_bg(colors));

    let running: Vec<_> = ui
        .dash_activity
        .iter()
        .rev()
        .filter(|(activity, _)| activity.running)
        .take(5)
        .collect();
    if running.is_empty() {
        add(
            " no active tools".into(),
            colors.muted,
            tool_panel_bg(colors),
        );
    } else {
        for (activity, started) in running.into_iter().rev() {
            add(
                format!(
                    " ◌ {}  {:.1}s",
                    display_tool_title(&activity.title),
                    started.elapsed().as_secs_f64()
                ),
                colors.status,
                tool_panel_bg(colors),
            );
        }
    }
    add(
        "────────────────────────".into(),
        colors.muted,
        tool_panel_bg(colors),
    );
    if let Some(proc) = ui
        .dash_procs
        .iter()
        .rev()
        .find(|proc| proc.running && !proc.tail.trim().is_empty())
    {
        add(
            " LIVE SHELL OUTPUT".into(),
            colors.accent,
            tool_panel_bg(colors),
        );
        // Shell registry streams stdout/stderr during execution. Its output
        // is not a model thought; keep only two redacted visible tail lines.
        let tail: Vec<&str> = proc
            .tail
            .lines()
            .filter(|line| !line.trim().is_empty())
            .rev()
            .take(2)
            .collect();
        for line in tail.into_iter().rev() {
            let safe = crate::crash::redact(line);
            let safe: String = safe.chars().filter(|ch| !ch.is_control()).collect();
            add(format!("  {}", safe), colors.muted, tool_panel_bg(colors));
        }
        add("────────────────────────".into(), colors.muted, muted_bg);
    }
    add(
        " RECENT RESULTS".into(),
        colors.accent,
        result_panel_bg(colors),
    );
    // Use the authoritative activity timeline: read/search success cells
    // may be grouped away in the transcript, but their result must still
    // appear in the rail like every other tool.
    let finished: Vec<_> = ui
        .dash_activity
        .iter()
        .rev()
        .filter(|(activity, _)| !activity.running)
        .take(4)
        .collect();
    if finished.is_empty() {
        add(
            " waiting for first result".into(),
            colors.muted,
            result_panel_bg(colors),
        );
    } else {
        for (activity, _) in finished.into_iter().rev() {
            let (icon, fg) = if activity.ok == Some(true) {
                ("✓", Color::Green)
            } else {
                ("×", Color::Red)
            };
            add(
                format!(
                    " {icon} {}  {}",
                    display_tool_title(&activity.title),
                    format_duration_ms(activity.elapsed_ms)
                ),
                fg,
                result_panel_bg(colors),
            );
        }
    }
    add(
        "────────────────────────".into(),
        colors.muted,
        result_panel_bg(colors),
    );
    if !ui.plan.is_empty() {
        let done = ui.plan.iter().filter(|(_, complete)| *complete).count();
        add(
            format!(" PLAN  {done}/{}", ui.plan.len()),
            colors.accent,
            muted_bg,
        );
        for (step, complete) in ui.plan.iter().take(3) {
            add(
                format!(" {} {}", if *complete { "✓" } else { "○" }, step),
                if *complete { Color::Green } else { colors.text },
                muted_bg,
            );
        }
    }
    add(
        format!(" tools {}  ·  errors {}", ui.tool_calls, ui.tool_failures),
        colors.muted,
        result_panel_bg(colors),
    );
    add(
        format!(
            " tokens {}  ·  cache {}%",
            compact_number(ui.usage.input + ui.usage.output),
            cache_percent(ui.usage)
        ),
        colors.muted,
        result_panel_bg(colors),
    );
    rows
}

#[derive(Clone, Debug)]
struct InputVisualLine {
    text: String,
    start: usize,
    end: usize,
}

/// Wrap the editor's source text for display while retaining byte offsets into
/// the original buffer. Explicit newlines create rows; long logical lines are
/// split only in this view and are never changed in the submitted prompt.
fn wrap_input(input: &str, width: usize) -> Vec<InputVisualLine> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut base = 0usize;
    let logical_lines: Vec<&str> = input.split('\n').collect();
    for (line_index, line) in logical_lines.iter().enumerate() {
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        if chars.is_empty() {
            rows.push(InputVisualLine {
                text: String::new(),
                start: base,
                end: base,
            });
        } else {
            for chunk in chars.chunks(width) {
                let start = base + chunk[0].0;
                let end = base
                    + chunk
                        .last()
                        .map(|(offset, ch)| offset + ch.len_utf8())
                        .unwrap_or(0);
                rows.push(InputVisualLine {
                    text: input[start..end].to_string(),
                    start,
                    end,
                });
            }
        }
        base += line.len();
        if line_index + 1 < logical_lines.len() {
            base += 1;
        }
    }
    rows
}

fn input_cursor_position(rows: &[InputVisualLine], cursor: usize) -> (usize, usize) {
    let row = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| cursor >= row.start && cursor <= row.end)
        .map(|(index, _)| index)
        .next_back()
        .unwrap_or(0);
    let line = &rows[row];
    let byte = cursor.clamp(line.start, line.end);
    (row, line.text[..byte - line.start].chars().count())
}

fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// Box top for the OpenCode-style chrome: `┌─ label ───┐`, exactly `width`
/// cells wide. An empty label yields a plain rule with corners.
fn box_top_row(width: usize, label: &str) -> String {
    if width < 4 {
        return clip(&"─".repeat(width.max(1)), width);
    }
    if label.is_empty() {
        return format!("┌{}┐", "─".repeat(width - 2));
    }
    let tag = format!("─ {label} ");
    let fill = "─".repeat(width.saturating_sub(3 + tag.chars().count()));
    clip(&format!("┌{tag}{fill}┐"), width)
}

/// Box bottom: `└──────┘`, exactly `width` cells wide.
fn box_bottom_row_text(width: usize) -> String {
    if width < 4 {
        return clip(&"─".repeat(width.max(1)), width);
    }
    format!("└{}┘", "─".repeat(width - 2))
}

/// One boxed body row: `│ {content} │`, padded to exactly `width` cells.
/// `content` must already fit `width - 4`.
fn box_body_row(content: &str, width: usize) -> String {
    let pad = width
        .saturating_sub(4)
        .saturating_sub(content.chars().count());
    clip(&format!("│ {content}{} │", " ".repeat(pad)), width)
}

/// The Ctrl+B session snapshot as a bordered transcript section: titled top
/// border, one boxed row per line, closing border. Never wider than `width`.
fn snapshot_box_lines(text: &str, width: usize) -> Vec<String> {
    let available = width.saturating_sub(4).max(1);
    let mut lines = vec![box_top_row(width, "SESSION")];
    for line in text.lines().skip(1) {
        for part in wrap(line, available) {
            lines.push(box_body_row(&part, width));
        }
    }
    lines.push(box_bottom_row_text(width));
    lines
}

/// The composer owns focus except while navigating the transcript, a
/// picker, or the dashboard. Focused chrome glows cyan; blurred chrome
/// falls back to quiet gray.
fn composer_focused(ui: &Ui) -> bool {
    !ui.navigation_mode && ui.picker.is_none() && !ui.dashboard
}

/// Recolor a boxed row's walls (`│ … │`) with the border colour, leaving
/// the interior in the row's own foreground. No-op on narrow rows and in
/// `NO_COLOR` sessions.
fn paint_box_walls(row: &mut Row, border: Color, no_color: bool) {
    if no_color {
        return;
    }
    let cells = row.text.chars().count();
    if cells < 6 {
        return;
    }
    let mut palette = vec![border, border];
    palette.extend(std::iter::repeat_n(row.fg, cells - 4));
    palette.push(border);
    palette.push(border);
    row.colors = Some(palette);
}

/// Last-two-components path for the session snapshot (`…/proj/wrose` keeps
/// the readout narrow without losing orientation).
fn short_path(path: &str, width: usize) -> String {
    let clipped = clip(path, width);
    if clipped.chars().count() < width {
        return clipped;
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() >= 2 {
        let tail = format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]);
        if tail.chars().count() <= width {
            return format!("…/{tail}");
        }
    }
    format!("…{}", clip(path, width.saturating_sub(1)))
}

fn startup_connection(provider_health: &str) -> String {
    if provider_health.contains("unavailable") {
        return "checking connection".into();
    }
    if provider_health.starts_with('0') || provider_health.contains("no providers") {
        return "setup needed".into();
    }
    if let Some((connected, _)) = provider_health.split_once(" connected") {
        let connected = connected.trim();
        if connected != "0" {
            return format!("{connected} connected");
        }
    }
    "ready".into()
}

/// One transcript entry as rendered rows, OpenCode-style: user prompts open
/// with `›`, assistant prose follows an `◈ WROSECODE` marker, and tool or
/// system rows render label-free (their first character already carries the
/// ✓/◌/✗ state). A tool call's command line is never truncated: it wraps
/// onto as many rows as it needs. Tool *output* stays capped with its own
/// explicit markers instead.
///
/// Returns `(row, background-override)` pairs: `Some(panel)` paints fenced
/// code blocks, tool output boxes, and the snapshot box on the charcoal
/// container background, `None` keeps the entry's style background.
fn entry_rows(
    entry: &Entry,
    left_width: usize,
    verbose: bool,
    panel: Color,
) -> Vec<(String, Option<Color>)> {
    if entry.speaker == Speaker::Agent {
        return render_agent_body(&entry.text, left_width, verbose, panel);
    }
    if entry.speaker == Speaker::User {
        return boxed_message_lines(entry.speaker, &entry.text, left_width, verbose)
            .into_iter()
            .map(|line| (line, None))
            .collect();
    }
    // The Ctrl+B session snapshot reads inside its own bordered section.
    if entry.speaker == Speaker::System && entry.text.starts_with("SESSION\n") {
        return snapshot_box_lines(&entry.text, left_width)
            .into_iter()
            .map(|line| (line, Some(panel)))
            .collect();
    }
    // Expanded tool output between the `▓WROSE-OUT▓` / `▓WROSE-END▓`
    // sentinels renders as a bordered box at the live pane width; the
    // sentinels themselves never reach the screen.
    if entry.speaker == Speaker::Tool {
        return render_tool_rows(&entry.text, left_width, verbose, panel);
    }
    let (first_prefix, cont_prefix, available) = ("", "  ", left_width.saturating_sub(2).max(1));
    let mut lines = Vec::new();
    let logical_lines: Vec<&str> = entry.text.lines().collect();
    for (part_index, line) in logical_lines.into_iter().enumerate() {
        // System rows always wrap: silent clipping once cut a 60-char flag
        // in half inside `/flags` output and `🚩 FLAG DETECTED` lines.
        for (wrap_index, part) in wrap(line, available).into_iter().enumerate() {
            let prefix = if part_index == 0 && wrap_index == 0 {
                first_prefix
            } else {
                cont_prefix
            };
            lines.push((clip(&format!("{prefix}{part}"), left_width), None));
        }
    }
    lines
}

/// Plain-text view of [`entry_rows`]: what the wrap-cache tests assert on.
/// Test-only: production rendering goes through `ensure_transcript`, which
/// also tracks per-row backgrounds.
#[cfg(test)]
fn entry_lines(entry: &Entry, left_width: usize, verbose: bool) -> Vec<String> {
    entry_rows(entry, left_width, verbose, Color::Black)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// Assistant result message as one bordered container, OpenCode-style:
/// a top border titled `◈ WROSECODE`, prose rows inside the walls, fenced
/// code blocks kept as their own labeled containers nested inside, closing
/// border. Indentation is preserved verbatim; long lines wrap inside the
/// walls. An unclosed fence renders to the end of the message instead of
/// leaking raw backticks.
fn render_agent_body(
    text: &str,
    width: usize,
    _verbose: bool,
    panel: Color,
) -> Vec<(String, Option<Color>)> {
    let width = width.max(12);
    let mut out: Vec<(String, Option<Color>)> = vec![agent_header(width, panel)];
    let mut body = BodyStream::default();
    for line in text.split('\n') {
        body.feed(line, width, panel, &mut out);
    }
    body.finish(width, panel, &mut out);
    if let Some(footer) = agent_footer(width, panel) {
        out.push(footer);
    }
    out
}

/// Tool entry rows with the expanded-output section (between the
/// `▓WROSE-OUT▓` / `▓WROSE-END▓` sentinels `cell_text` emits) rendered as a
/// bordered output box: titled top border with name, status, duration, and
/// exit code, one boxed row per output line, closing border. Long lines wrap
/// inside the walls; collapsed previews stay a single quiet line.
fn render_tool_rows(
    text: &str,
    left_width: usize,
    verbose: bool,
    panel: Color,
) -> Vec<(String, Option<Color>)> {
    let available = left_width.saturating_sub(2).max(1);
    let code_available = left_width.saturating_sub(6).max(1);
    let mut out = Vec::new();
    let mut in_box = false;
    for (part_index, line) in text.lines().enumerate() {
        if line.starts_with("▓WROSE-OUT▓") {
            let label = format!(
                "output{}",
                line.strip_prefix("▓WROSE-OUT▓")
                    .map(str::trim)
                    .filter(|rest| !rest.is_empty())
                    .map(|rest| format!(" · {rest}"))
                    .unwrap_or_default()
            );
            out.push((box_top_row(left_width, &label), Some(panel)));
            in_box = true;
            continue;
        }
        if line.starts_with("▓WROSE-END▓") {
            out.push((box_bottom_row_text(left_width), Some(panel)));
            in_box = false;
            continue;
        }
        if in_box {
            for part in wrap(line, code_available) {
                out.push((box_body_row(&part, left_width), Some(panel)));
            }
            continue;
        }
        // The first line of a tool cell is the command or task being run:
        // always show all of it, wrapped, so a long command is never cut
        // off mid-text the way silent clipping would.
        let wrap_line = verbose || part_index == 0;
        for (wrap_index, part) in if wrap_line {
            wrap(line, available)
        } else {
            vec![clip(line, available)]
        }
        .into_iter()
        .enumerate()
        {
            let prefix = if part_index == 0 && wrap_index == 0 {
                ""
            } else {
                "  "
            };
            out.push((clip(&format!("{prefix}{part}"), left_width), None));
        }
    }
    out
}

/// Bounded message treatment for the two primary conversational surfaces.
/// A user prompt opens with `›`; an assistant result renders through the
/// same bordered container the transcript uses (backgrounds stripped here —
/// the streaming draft paints plain until it commits).
/// Tool/system rows intentionally stay unboxed so the timeline remains dense.
fn boxed_message_lines(speaker: Speaker, text: &str, width: usize, verbose: bool) -> Vec<String> {
    let width = width.max(12);
    let mut lines = Vec::new();
    match speaker {
        Speaker::User => {
            // Grok-clean prompt: no box, just the `›` marker and the text.
            // The marker is repainted cyan at render time; wrapped lines
            // hang beneath it.
            let available = width.saturating_sub(4).max(1);
            let mut first = true;
            for line in text.split('\n') {
                for part in wrap(line, available) {
                    if first {
                        lines.push(clip(&format!("› {part}"), width));
                        first = false;
                    } else {
                        lines.push(clip(&format!("  {part}"), width));
                    }
                }
            }
            if first {
                lines.push("›".into());
            }
        }
        Speaker::Agent => {
            return render_agent_body(text, width, verbose, Color::Black)
                .into_iter()
                .map(|(line, _)| line)
                .collect();
        }
        _ => unreachable!("boxed_message_lines only accepts user or agent entries"),
    }
    lines
}

/// Keep transcript activity scannable at a glance. Foreground colours carry
/// the meaning on the dark viewport: user prompts read soft white with a
/// cyan marker, tool output speaks green (bright highlighted titles, dim
/// matching details), live activity glows muted amber, failures stay muted
/// red. Assistant results read as one calm bordered container on charcoal.
fn transcript_style(entry: &Entry, colors: &Theme) -> (Color, Color) {
    let lower = entry.text.to_ascii_lowercase();
    match entry.speaker {
        Speaker::User => (colors.text, transcript_panel_bg(colors)),
        Speaker::Agent => (colors.text, transcript_panel_bg(colors)),
        // Tool rows carry their state in the first character: running glows
        // amber, failures read red, everything finished settles green.
        Speaker::Tool if entry.text.starts_with('◌') => (
            if studio_panel(colors.panel) {
                Color::Rgb {
                    r: 224,
                    g: 185,
                    b: 132,
                }
            } else {
                WROSE_AMBER
            },
            tool_panel_bg(colors),
        ),
        Speaker::Tool if entry.text.starts_with('✗') || entry.text.starts_with('×') => {
            (WROSE_ERROR, tool_panel_bg(colors))
        }
        Speaker::Tool => (
            if studio_panel(colors.panel) {
                Color::Rgb {
                    r: 139,
                    g: 207,
                    b: 166,
                }
            } else {
                WROSE_SUCCESS
            },
            tool_panel_bg(colors),
        ),
        // The Ctrl+B snapshot box stays quiet muted even when its counters
        // mention failures or verification; the box is a readout, not an
        // error or a receipt.
        Speaker::System if entry.text.starts_with("SESSION\n") => {
            (colors.muted, transcript_panel_bg(colors))
        }
        Speaker::System if lower.contains("failed") || lower.contains("cancelled") => {
            (WROSE_ERROR, result_panel_bg(colors))
        }
        Speaker::System
            if lower.contains("result") || lower.contains("verified") || lower.contains("done") =>
        {
            (WROSE_DIM_GREEN, result_panel_bg(colors))
        }
        Speaker::System if lower.starts_with("◌") || lower.contains("running") => {
            (WROSE_AMBER, activity_bg(colors))
        }
        // Frozen reasoning cells settle into quiet gray notes.
        Speaker::System if lower.contains("thought") => (colors.muted, activity_bg(colors)),
        Speaker::System => (colors.muted, transcript_panel_bg(colors)),
    }
}

fn input_panel_bg(colors: &Theme) -> Color {
    // The charcoal composer panel, distinct from the viewport background.
    colors.panel
}

fn transcript_panel_bg(colors: &Theme) -> Color {
    if colors.background == Color::Reset {
        Color::Reset
    } else {
        colors.background
    }
}

fn tool_panel_bg(colors: &Theme) -> Color {
    transcript_panel_bg(colors)
}

fn activity_bg(colors: &Theme) -> Color {
    transcript_panel_bg(colors)
}

fn result_panel_bg(colors: &Theme) -> Color {
    transcript_panel_bg(colors)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    name: &'static str,
    text: Color,
    muted: Color,
    accent: Color,
    status: Color,
    background: Color,
    /// Composer panel: the charcoal editing surface.
    panel: Color,
}

/// WROSECODE premium accents. The base [`Theme`] stays five fields so every
/// existing palette keeps working; these are semantic overlays for the
/// single-column design.
const WROSE_CYAN: Color = Color::Rgb {
    r: 34,
    g: 211,
    b: 238,
};
const WROSE_SUCCESS: Color = Color::Green;
/// Dim companion for tool output continuations: the title line reads bright
/// highlighted green, the detail lines below it match in the same family.
const WROSE_DIM_GREEN: Color = Color::DarkGreen;
/// Muted gold for live activity: running tools, thinking ticks.
const WROSE_AMBER: Color = Color::DarkYellow;
const WROSE_WARN: Color = Color::Yellow;
/// Muted red for failures.
const WROSE_ERROR: Color = Color::DarkRed;
/// Near-black viewport background for the premium dark palette.
const WROSE_BG: Color = Color::Rgb { r: 4, g: 5, b: 10 };
const STUDIO_BG: Color = Color::Rgb {
    r: 13,
    g: 17,
    b: 24,
};
const STUDIO_PANEL: Color = Color::Rgb {
    r: 23,
    g: 29,
    b: 42,
};
const STUDIO_TEXT: Color = Color::Rgb {
    r: 222,
    g: 228,
    b: 240,
};
const STUDIO_MUTED: Color = Color::Rgb {
    r: 125,
    g: 139,
    b: 159,
};
const STUDIO_ACCENT: Color = Color::Rgb {
    r: 169,
    g: 178,
    b: 247,
};
const STUDIO_STATUS: Color = Color::Rgb {
    r: 126,
    g: 205,
    b: 213,
};

fn studio_panel(panel: Color) -> bool {
    panel == STUDIO_PANEL
}

/// Every selectable palette. `/theme` (picker or `<name>`) and `[ui] theme` in
/// config.toml all index this one list, so a theme cannot exist in one place
/// and be missing from another.
const THEMES: &[Theme] = &[
    Theme {
        name: "dark",
        text: Color::White,
        muted: Color::Grey,
        accent: WROSE_CYAN,
        status: Color::Yellow,
        background: WROSE_BG,
        panel: Color::Rgb {
            r: 12,
            g: 16,
            b: 25,
        },
    },
    Theme {
        name: "light",
        text: Color::Black,
        muted: Color::DarkGrey,
        accent: Color::Blue,
        status: Color::DarkYellow,
        background: Color::White,
        panel: Color::Rgb {
            r: 232,
            g: 232,
            b: 232,
        },
    },
    Theme {
        name: "solarized",
        text: Color::Grey,
        muted: Color::DarkCyan,
        accent: Color::Cyan,
        status: Color::DarkYellow,
        background: Color::Rgb { r: 0, g: 43, b: 54 },
        panel: Color::Rgb { r: 7, g: 54, b: 66 },
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
        panel: Color::Rgb {
            r: 52,
            g: 55,
            b: 70,
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
        panel: Color::Rgb {
            r: 59,
            g: 66,
            b: 82,
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
        panel: Color::Black,
    },
    Theme {
        name: "studio",
        text: STUDIO_TEXT,
        muted: STUDIO_MUTED,
        accent: STUDIO_ACCENT,
        status: STUDIO_STATUS,
        background: STUDIO_BG,
        panel: STUDIO_PANEL,
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
    #[serde(default)]
    panel: Option<String>,
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
    let panel = match &file.panel {
        Some(value) => hex_colour(value)?,
        None => background,
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
        panel,
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
        WROSE_WARN
    } else {
        colors.text
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[cfg(test)]
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
            let auth_prefix = ui.input[..start].trim_end() == "/auth";
            let matches = if auth_prefix {
                ["login", "status", "accounts", "switch", "logout"]
                    .into_iter()
                    .filter(|candidate| token.is_empty() || candidate.starts_with(&token))
                    .map(str::to_owned)
                    .collect()
            } else {
                completion_matches(agent, &token)
            };
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

/// Expanded cell body: a bounded head/tail slice of raw output lines so one
/// huge result cannot flood the transcript. Wrapped in `▓WROSE-OUT▓`/`▓WROSE-END▓`
/// sentinels carrying the exit-code label; `entry_rows` renders the section
/// as a bordered output box at the live pane width (a width the cell itself
/// never knows). The sentinels never reach the screen.
fn expanded_block(output: &str, exit_code: Option<i32>) -> String {
    const MAX: usize = 120;
    const HEAD: usize = 80;
    // A bounded 256 KB single line still expands to thousands of terminal
    // rows. Bound pathological rows while preserving normal output and the
    // original tool result retained in the cell.
    const MAX_CHARS_PER_LINE: usize = 1024;
    const MAX_VISIBLE_CHARS: usize = 32 * 1024;
    let mut block = String::from("\n▓WROSE-OUT▓");
    match exit_code {
        Some(code) => block.push_str(&format!(" exit={code}")),
        None => block.push_str(" no-exit"),
    }
    let lines = clean_output_lines(output);
    if lines.is_empty() {
        block.push_str("\n▓WROSE-END▓");
        return block;
    }
    if lines.len() > MAX {
        block.push_str(&format!("\n⋮ {} more lines", lines.len() - MAX));
    }
    let mut remaining = MAX_VISIBLE_CHARS;
    let mut clipped = false;
    let head = lines
        .iter()
        .take(if lines.len() <= MAX { MAX } else { HEAD });
    let tail = lines.iter().skip(if lines.len() <= MAX {
        lines.len()
    } else {
        lines.len() - (MAX - HEAD)
    });
    for line in head.chain(tail) {
        if remaining == 0 {
            clipped = true;
            break;
        }
        let max_chars = MAX_CHARS_PER_LINE.min(remaining);
        let mut chars = line.chars();
        let visible: String = chars.by_ref().take(max_chars).collect();
        let shortened = chars.next().is_some();
        remaining = remaining.saturating_sub(visible.chars().count());
        block.push('\n');
        block.push_str(&visible);
        if shortened {
            block.push_str(" … [line shortened for responsiveness]");
            clipped = true;
        }
    }
    if clipped {
        block.push_str("\n… [display capped; full tool result retained in session]");
    }
    block.push_str("\n▓WROSE-END▓");
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
        format!("◌ ▸ Exploring  {counts}")
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

/// Git refresh schedule: ~50ms of child processes every 5s while working,
/// every 30s while idle. Pure predicate so the cadence is unit-testable
/// without spawning git.
fn git_stale(at: Option<std::time::Instant>, now: std::time::Instant, busy: bool) -> bool {
    let ttl = if busy { 5 } else { 30 };
    at.is_none_or(|at| now.duration_since(at).as_secs() >= ttl)
}
/// Cheap git snapshot for the Ctrl+B session readout: current branch plus
/// added/removed totals. Empty branch means "not a git repo" and the
/// snapshot omits the GIT section.
/// All commands are read-only and bounded; failures degrade to empty.
fn git_sidebar_state(root: &str) -> (String, usize, i64, i64) {
    let branch = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if branch.is_empty() || branch == "HEAD" {
        // Detached HEAD or not a repo: try short hash, else omit section.
        let hash = std::process::Command::new("git")
            .args(["-C", root, "rev-parse", "--short", "HEAD"])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default();
        if hash.is_empty() {
            return (String::new(), 0, 0, 0);
        }
        let files = git_changed_files(root);
        let (added, removed, count) = git_totals(&files);
        return (format!("@{hash}"), count, added, removed);
    }
    let files = git_changed_files(root);
    let (added, removed, count) = git_totals(&files);
    (branch, count, added, removed)
}

fn git_totals(files: &[(String, i64, i64)]) -> (i64, i64, usize) {
    let mut added = 0i64;
    let mut removed = 0i64;
    for (_, a, r) in files {
        added += (*a).max(0);
        removed += *r;
    }
    (added, removed, files.len())
}

/// The eight panels as plain text rows, shared by the grid dashboard,
/// `/stats`, and the `wrosecode dashboard` / `exec` CLI (spec 3.2).
pub(crate) fn dashboard_panels(
    stats: &DashStats,
    width: usize,
) -> Vec<(&'static str, Vec<String>)> {
    // 0 PROCESSES — agent actions first, OS shell PIDs afterward.
    let mut procs = Vec::new();
    for activity in &stats.activities {
        let status = if activity.running {
            "◌"
        } else if activity.ok == Some(true) {
            "✓"
        } else {
            "×"
        };
        procs.push(format!(
            " {status} {:>5.1}s  {}",
            activity.elapsed_ms as f64 / 1000.0,
            clip(
                &display_tool_title(&activity.title),
                width.saturating_sub(12).max(8)
            )
        ));
    }
    if stats.procs.is_empty() && stats.activities.is_empty() {
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

    // 1 THINKING — model stage is a public status, not hidden thought text.
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
            " stage: {}",
            if stats.stage.is_empty() {
                &stats.status
            } else {
                &stats.stage
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
        // The focused panel scrolls its rows to keep a selected activity or
        // process visible. Without this, the 8-line panel hid older calls.
        let available = height.saturating_sub(1);
        let offset = if focus == index && available > 0 {
            selected
                .saturating_sub(available - 1)
                .min(rows.len().saturating_sub(available))
        } else {
            0
        };
        for (row_index, text) in rows.iter().enumerate().skip(offset) {
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
        // No model stage is surfaced on this path: `status` already carries
        // the verification state, and the panel falls back to it when the
        // stage is empty.
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
    let (cached_models, cached_at) = load_model_catalog_cache(&settings);
    ui.model_catalog = ready_model_rows(
        &settings,
        configured_model_rows(&settings)
            .into_iter()
            .chain(cached_models)
            .collect(),
    );
    ui.model_catalog_fetched = cached_at;
    ui.restore_flags(session.flags.clone());
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
        ui.entry_revisions.clear();
        ui.entry_rows_meta.clear();
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
            ui.entry_revisions.push(ui.next_entry_revision);
            ui.next_entry_revision = ui.next_entry_revision.wrapping_add(1);
        }
    }
    let mut last_checkpoint = std::time::Instant::now();
    // In-flight background checkpoint save (10s cadence below). While one
    // is running the tick is skipped instead of queueing: saves stay
    // strictly serial and a slow disk never stacks work behind the UI.
    let mut checkpoint: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    // Compare lightweight state versions before cloning a long transcript
    // for an idle checkpoint. Failed saves are never marked persisted.
    let mut saved_checkpoint_version: Option<(
        u64,
        usize,
        usize,
        usize,
        String,
        String,
        Vec<String>,
        String,
    )> = None;
    let mut pending_checkpoint_version: Option<(
        u64,
        usize,
        usize,
        usize,
        String,
        String,
        Vec<String>,
        String,
    )> = None;
    let mut last_spin = std::time::Instant::now();
    let mut last_tip = std::time::Instant::now();
    let mut last_chrome = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(10))
        .unwrap_or_else(std::time::Instant::now);
    let mut render_needed = true;
    // Initial chrome sync so the first frame already shows real MCP/LSP/git.
    ui.sync_chrome(&agent);
    // Repo maps are optional, deadline-bounded context loaded by the agent
    // only when needed. Never eagerly crawl a Desktop/home tree at startup.
    loop {
        if render_needed {
            ui.render()?;
            render_needed = false;
        }
        // Checkpoint on a background thread: serializing a large session
        // measured 1.1s at 5MB, which used to freeze every keystroke on the
        // event loop. Failures are traced and retried on the next tick, so
        // a transient disk error degrades instead of aborting the session.
        if last_checkpoint.elapsed() >= Duration::from_secs(10) {
            // Reap the previous save if it finished; a still-running save
            // means this tick is skipped, so saves stay strictly serial.
            if checkpoint
                .as_ref()
                .is_some_and(|handle| handle.is_finished())
            {
                if let Some(handle) = checkpoint.take() {
                    match handle.await {
                        Ok(Ok(())) => {
                            saved_checkpoint_version = pending_checkpoint_version.take();
                        }
                        Ok(Err(error)) => {
                            pending_checkpoint_version = None;
                            ui.trace.log("checkpoint", 0, &error.to_string());
                        }
                        Err(error) => {
                            pending_checkpoint_version = None;
                            ui.trace.log(
                                "checkpoint",
                                0,
                                &format!("background save worker failed: {error}"),
                            );
                        }
                    }
                }
            }
            let current_version = (
                ui.next_entry_revision,
                ui.entries.len(),
                agent.messages.len(),
                ui.flags_found,
                ui.provider.clone(),
                ui.model.clone(),
                agent.pinned.clone(),
                session.name.clone(),
            );
            if checkpoint.is_none() && saved_checkpoint_version.as_ref() != Some(&current_version) {
                sync_session(&mut session, &agent, &ui);
                let snapshot = session.clone();
                let writer = store.clone();
                pending_checkpoint_version = Some(current_version);
                checkpoint = Some(tokio::task::spawn_blocking(move || {
                    writer.save_session(&snapshot)
                }));
            }
            last_checkpoint = std::time::Instant::now();
        }
        if !event::poll(Duration::from_millis(16))? {
            // Idle discipline: animation and chrome ticks only repaint while
            // something can actually change (running turn / dashboard) or
            // the synced state moved. A quiet session costs no frames —
            // the identical-frame guard would skip the write anyway, but
            // skipping compose entirely saves the CPU too.
            let animated = ui.wants_animation();
            if animated && last_spin.elapsed() >= Duration::from_millis(90) {
                ui.spinner = ui.spinner.wrapping_add(1);
                last_spin = std::time::Instant::now();
                render_needed = true;
            }
            if animated && last_tip.elapsed() >= Duration::from_secs(4) {
                ui.advance_tip();
                last_tip = std::time::Instant::now();
                render_needed = true;
            }
            // Chrome refresh ~1Hz keeps MCP/LSP/git/model state live without
            // running git per frame.
            if last_chrome.elapsed() >= Duration::from_secs(1) {
                let before = ui.chrome_fingerprint();
                ui.sync_chrome(&agent);
                last_chrome = std::time::Instant::now();
                if animated || ui.chrome_fingerprint() != before {
                    render_needed = true;
                }
            }
            // Keep `live.json` current for `wrosecode dashboard` attaches.
            ui.maybe_write_live();
            continue;
        }
        let next = event::read()?;
        render_needed = true;
        if let Event::Paste(text) = &next {
            ui.accept_paste(text);
            continue;
        }
        if let Event::Mouse(mouse) = &next {
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
                // A click on a transcript row expands or collapses its cell
                // (spec 3.1): map the terminal row back through line_origin.
                MouseEventKind::Down(MouseButton::Left) => {
                    let row = mouse.row as usize;
                    let first = ui.view.body_start;
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
            // A cancelled turn arms the same window, so Ctrl+C, Ctrl+C stops
            // a run and exits.
            if ui.ctrl_c_idle() {
                break;
            }
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
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // The rail is gone; Ctrl+B now prints a one-shot session
                // snapshot (provider, usage, MCP/LSP, tasks, git, CTF) into
                // the transcript. `/stats` and `/debug` remain the deep views.
                ui.sync_chrome(&agent);
                ui.push(Speaker::System, ui.session_snapshot());
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
            KeyCode::Delete if ui.cursor < ui.input.len() => ui.delete_forward(),
            KeyCode::Left
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                ui.move_word_left()
            }
            KeyCode::Right
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                ui.move_word_right()
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => ui.move_word_left(),
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => ui.move_word_right(),
            KeyCode::Left => ui.move_left(),
            KeyCode::Right => ui.move_right(),
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => ui.scroll_home(),
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => ui.scroll_end(),
            KeyCode::Home if ui.input.is_empty() => ui.scroll_home(),
            KeyCode::End if ui.input.is_empty() => ui.scroll_end(),
            KeyCode::Home => ui.line_home(),
            KeyCode::End => ui.line_end(),
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
            KeyCode::Up if !ui.input.is_empty() => {
                let width = terminal::size()
                    .map(|(width, _)| width as usize)
                    .unwrap_or(80);
                if !ui.move_vertical(false, width)
                    && !ui.input.contains('\n')
                    && !ui.history.is_empty()
                {
                    let index = ui
                        .history_index
                        .unwrap_or(ui.history.len())
                        .saturating_sub(1);
                    ui.history_index = Some(index);
                    ui.set_input(ui.history[index].clone());
                }
            }
            KeyCode::Down if !ui.input.is_empty() => {
                let width = terminal::size()
                    .map(|(width, _)| width as usize)
                    .unwrap_or(80);
                if !ui.move_vertical(true, width) {
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
            }
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
                    ui.status = "Paste inserted · press Enter again to send".into();
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
                let command = line.trim();
                if command == "/quit" || command == "/exit" {
                    break;
                }
                if command.starts_with('/') {
                    if let Err(error) = run_command(
                        command,
                        &mut agent,
                        &mut ui,
                        &mut settings,
                        &mut session,
                        &mut events,
                        &mut permissions,
                        &_guard,
                    )
                    .await
                    {
                        ui.push(Speaker::System, format!("Error: {error:#}"));
                    }
                } else if let Some(fact) = command.strip_prefix("#!fact ") {
                    ui.push(Speaker::System, agent.memory.save(fact, true)?);
                } else if let Some(fact) = command.strip_prefix("#fact ") {
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
    // Restore the shell before final persistence. A slow filesystem must not
    // make quitting look like a frozen terminal. The guard's Drop is idempotent.
    crate::crash::restore();
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
    ui.set_task_state(TaskState::Submitting);
    ui.status = "Waiting for first token".into();
    ui.begin_turn();
    ui.render()?;
    let mut dirty = false;
    let mut last_animation = std::time::Instant::now();
    let project_root = agent.config.root.clone();
    let session_store = agent.store.clone();
    let metrics = agent.metrics.clone();
    let messages_before = agent.messages.len();
    // Normal turns should recover from a genuinely silent provider quickly.
    // High/max reasoning and CTF analysis keep the generous long-run limit.
    let idle_limit =
        if agent.mode == "ctf" || matches!(agent.think, ThinkLevel::High | ThinkLevel::Max) {
            provider::STREAM_IDLE_TIMEOUT
        } else {
            Duration::from_secs(65)
        };
    // The turn future borrows `agent`, so it is confined to this block. Once
    // the block ends the borrow is released and a cancelled turn can be
    // rolled back instead of leaving half a conversation behind.
    let outcome = {
        let turn = agent.turn(prompt);
        tokio::pin!(turn);
        // Progress events are applied immediately, but terminal writes are
        // coalesced to a human-scale frame budget. This keeps stream/tool
        // processing independent from renderer throughput.
        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_progress = std::time::Instant::now();
        // Immediate first paint, then at most one frame per 50 ms while
        // progress events flood in. The old design only painted on the 50 ms
        // ticker, so every first token waited for the next tick.
        let mut next_frame = std::time::Instant::now();
        let mut watchdog = tokio::time::interval(Duration::from_secs(1));
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = &mut turn => break TurnOutcome::Done(result),
                Some(message) = events.recv() => {
                    last_progress = std::time::Instant::now();
                    ui.apply_progress(message);
                    // Bound burst draining so a flood from parallel subagents
                    // cannot monopolize the executor and starve keyboard/render
                    // ticks. The queue retains the remainder for the next pass.
                    for _ in 0..64 {
                        let Ok(message) = events.try_recv() else { break; };
                        last_progress = std::time::Instant::now();
                        ui.apply_progress(message);
                    }
                    dirty = true;
                    if std::time::Instant::now() >= next_frame {
                        ui.render()?;
                        dirty = false;
                        next_frame = std::time::Instant::now() + Duration::from_millis(50);
                    }
                }
                Some(request) = permissions.recv() => {
                    last_progress = std::time::Instant::now();
                    if !ask_permission(ui, request).await? { return Ok(false); }
                    ui.status = "Working".into();
                    dirty = true;
                }
                _ = watchdog.tick() => {
                    // Tools have their own explicit timeouts. This watchdog is
                    // only for a model/transport that produces no bytes/events
                    // at all, keeping the TUI cancellable instead of hanging.
                    if ui.inflight_tools == 0 {
                        let idle = last_progress.elapsed();
                        if idle >= idle_limit {
                            break TurnOutcome::Done(Err(anyhow::anyhow!(
                                "model stalled: no provider activity for {} seconds",
                                idle_limit.as_secs()
                            )));
                        }
                        if idle >= Duration::from_secs(20) && ui.draft.is_empty() {
                            ui.status = format!(
                                "No model activity for {}s · automatic recovery active · Esc cancel",
                                idle.as_secs()
                            );
                            dirty = true;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if last_animation.elapsed() >= Duration::from_millis(140) {
                        ui.spinner = ui.spinner.wrapping_add(1);
                        // Advance the live `Thinking… n.ns` cell (spec 3.1).
                        ui.tick_thinking();
                        last_animation = std::time::Instant::now();
                        dirty = true;
                    }
                    // Keep `live.json` fresh while a turn runs (spec 3.2).
                    ui.maybe_write_live();
                    // Keep the terminal usable while the model thinks: keys
                    // land in the input box instead of being thrown away.
                    let (cancelled, input_changed) = drain_turn_keys(ui, scroll_step)?;
                    dirty |= input_changed;
                    if cancelled {
                        break TurnOutcome::Cancelled;
                    }
                    if dirty {
                        ui.render()?;
                        dirty = false;
                        next_frame = std::time::Instant::now() + Duration::from_millis(50);
                    }
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
    match outcome {
        TurnOutcome::Cancelled => {
            ui.set_task_state(TaskState::Cancelled);
            ui.status = "Cancelled".into();
            agent.messages.truncate(messages_before);
            // Arm the exit window: a second Ctrl+C within ~2s quits.
            ui.last_interrupt = Some(std::time::Instant::now());
            ui.push(
                Speaker::System,
                "Turn cancelled. Your input and any queued messages are kept.",
            );
        }
        TurnOutcome::Done(Ok(reply)) => {
            ui.set_task_state(TaskState::Completed);
            ui.status = "Done".into();
            ui.push(
                Speaker::Agent,
                if reply.is_empty() {
                    "(no response)".into()
                } else {
                    reply
                },
            );
        }
        TurnOutcome::Done(Err(error)) => {
            ui.set_task_state(TaskState::Failed);
            ui.status = "Failed".into();
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
fn drain_turn_keys(ui: &mut Ui, scroll_step: usize) -> Result<(bool, bool)> {
    let mut changed = false;
    while event::poll(Duration::ZERO)? {
        changed = true;
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
                    return Ok((true, changed));
                }
                match key.code {
                    KeyCode::Esc if ui.paste_chip.is_some() => {
                        ui.paste_chip = None;
                    }
                    KeyCode::Esc => return Ok((true, changed)),
                    KeyCode::Char(ch) if !control && !alt => {
                        ui.navigation_mode = false;
                        ui.insert(ch);
                    }
                    KeyCode::Backspace if !control => ui.backspace(),
                    KeyCode::Delete => ui.delete_forward(),
                    KeyCode::Left if alt || control => ui.move_word_left(),
                    KeyCode::Right if alt || control => ui.move_word_right(),
                    KeyCode::Left if !control => ui.move_left(),
                    KeyCode::Right if !control => ui.move_right(),
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        ui.insert('\n');
                    }
                    KeyCode::Enter if ui.input.is_empty() => ui.toggle_top_entry(),
                    KeyCode::Enter if ui.paste_chip.is_some() => {
                        if let Some((pasted, _)) = ui.paste_chip.take() {
                            ui.insert_str(&pasted);
                        }
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
                    KeyCode::Home => ui.line_home(),
                    KeyCode::End => ui.line_end(),
                    KeyCode::Up if !ui.input.is_empty() => {
                        let width = terminal::size()
                            .map(|(width, _)| width as usize)
                            .unwrap_or(80);
                        ui.move_vertical(false, width);
                    }
                    KeyCode::Down if !ui.input.is_empty() => {
                        let width = terminal::size()
                            .map(|(width, _)| width as usize)
                            .unwrap_or(80);
                        ui.move_vertical(true, width);
                    }
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
    Ok((false, changed))
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
            ui.push_result_block(&agent.step_log);
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
    session.flags = ui.flag_log.clone();
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

async fn restore_session(
    session: &Session,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
) -> Result<()> {
    if !session.provider_name.is_empty() && !session.model.is_empty() {
        switch_provider(agent, ui, settings, &session.provider_name, &session.model).await?;
    }
    apply_session_state(session, agent, ui);
    Ok(())
}

/// Point the agent's context and the scrollback at a session's stored
/// state (used by `/resume` and by a fork that rewound to an earlier
/// message).
fn apply_session_state(session: &Session, agent: &mut Agent, ui: &mut Ui) {
    agent.messages = session.messages.clone();
    agent.pinned = session.pinned.clone();
    // Old entry revisions and row-to-tool mappings refer to a different
    // session. Never reuse them for a restored transcript.
    ui.clear_transcript();
    ui.plan.clear();
    ui.dash_activity.clear();
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
    ui.entry_revisions = (0..ui.entries.len())
        .map(|_| {
            let revision = ui.next_entry_revision;
            ui.next_entry_revision = ui.next_entry_revision.wrapping_add(1);
            revision
        })
        .collect();
    ui.scroll = 0;
    ui.restore_flags(session.flags.clone());
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

/// Switch the agent mode and reconcile tool scopes in one place: CTF needs a
/// workspace allowlist, bug-bounty keeps only a scope armed by `/bugbounty`,
/// and the editing modes must not inherit a stale one.
fn switch_mode(agent: &mut Agent, ui: &mut Ui, mode: &str) -> Result<()> {
    let previous = agent.mode.clone();
    agent.set_mode(mode)?;
    if mode == "ctf" {
        let root = agent.config.root.clone();
        let scope = crate::autopilot::Scope::for_target("", &root, None);
        agent.tools.set_scope(Some(scope));
    } else if mode == "bugbounty" {
        if previous == "ctf" {
            agent.tools.set_scope(None);
        }
    } else {
        agent.tools.set_scope(None);
        agent.ctf.unlock_category();
    }
    ui.mode = if mode == "bugbounty" {
        "BUG_BOUNTY".into()
    } else {
        mode.to_ascii_uppercase()
    };
    Ok(())
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

/// Interactive model changes are transactional: prove that the candidate can
/// answer a bounded provider health check before replacing the working model.
/// Session restore and freshly-authenticated provider paths use the local-only
/// switch below so startup never pays this probe.
async fn switch_provider_checked(
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
    name: &str,
    model: &str,
) -> Result<()> {
    if name == agent.config.provider && model == agent.config.model {
        ui.status = format!("Already using {name} / {model}");
        return Ok(());
    }
    let profile = settings
        .profile(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    if !settings.can_use(profile) {
        ui.push(
            Speaker::System,
            format!("{name} isn't connected. Open /providers for an API key or /chatgpt login for ChatGPT."),
        );
        return Ok(());
    }
    let is_chatgpt = profile.kind == "openai_chatgpt_plan";
    let candidate = provider::create_profile(
        profile,
        settings.key(profile),
        model,
        agent.tools.client.clone(),
    )?;
    ui.status = format!("Checking {name} / {model}…");
    ui.render()?;
    // A provider probe can take seconds even when healthy. Keep the modal
    // responsive and cancellable; do not block the only terminal event reader
    // for the entire network deadline.
    let started = std::time::Instant::now();
    let mut probe = Box::pin(tokio::time::timeout(Duration::from_secs(15), async {
        if is_chatgpt {
            let available = candidate.list_models().await?;
            anyhow::ensure!(
                available
                    .iter()
                    .any(|available_model| available_model == model),
                "model is not in this ChatGPT account's catalog; run /models refresh"
            );
            Ok(())
        } else {
            candidate.test().await.map(|_| ())
        }
    }));
    let mut ticker = tokio::time::interval(Duration::from_millis(125));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let health = loop {
        tokio::select! {
            outcome = &mut probe => break Some(outcome),
            _ = ticker.tick() => {
                if event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press
                            && (key.code == KeyCode::Esc
                                || (key.code == KeyCode::Char('c')
                                    && key.modifiers.contains(KeyModifiers::CONTROL)))
                        {
                            break None;
                        }
                    }
                }
                ui.status = format!(
                    "Checking {name} / {model} · {}s · Esc cancel",
                    started.elapsed().as_secs()
                );
                ui.render()?;
            }
        }
    };
    match health {
        Some(Ok(Ok(_))) => {
            switch_provider(agent, ui, settings, name, model).await?;
            ui.status = format!("Ready · {name} / {model}");
            Ok(())
        }
        Some(Ok(Err(error))) => {
            ui.status = "Ready · model switch cancelled".into();
            anyhow::bail!(
                "model switch kept the previous model because {name}/{model} failed its connection check: {}",
                discovery_error(&error.to_string())
            )
        }
        Some(Err(_)) => {
            ui.status = "Ready · model switch cancelled".into();
            anyhow::bail!(
                "model switch kept the previous model because {name}/{model} did not respond within 15s"
            )
        }
        None => {
            // Dropping the probe future cancels the request. The old model
            // remains selected and can continue serving the next turn.
            ui.status = "Ready · model switch cancelled".into();
            Ok(())
        }
    }
}

async fn switch_provider(
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
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
    // Selection is a local state transition. Do not refresh OAuth or rewrite
    // the full provider document on picker selection; the dedicated atomic
    // active-model record restores the exact tuple on the next launch.
    settings.set_last_selection(name, model)?;
    if let Some(profile) = settings
        .providers
        .iter_mut()
        .find(|profile| profile.name == name)
    {
        profile.model = model.to_owned();
    }
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

async fn available_models(
    agent: &Agent,
    settings: &Settings,
) -> (Vec<(String, String)>, Vec<String>) {
    use futures::future::join_all;
    let profiles: Vec<_> = settings
        .providers
        .iter()
        .filter(|profile| settings.can_use(profile))
        .map(|profile| (profile.clone(), settings.key(profile)))
        .collect();
    let client = agent.tools.client.clone();
    let results = join_all(profiles.into_iter().map(|(profile, key)| {
        let client = client.clone();
        async move {
            let provider = match provider::create_profile(&profile, key, &profile.model, client) {
                Ok(provider) => provider,
                Err(error) => {
                    let _ = error;
                    return (
                        configured_model_row(&profile),
                        Some(format!("{} · model discovery unavailable", profile.name)),
                    );
                }
            };
            let discovered =
                match tokio::time::timeout(Duration::from_secs(5), provider.list_models()).await {
                    Ok(Ok(models)) => models,
                    Ok(Err(error)) => {
                        return (
                            configured_model_row(&profile),
                            Some(format!(
                                "{} · model discovery failed: {}",
                                profile.name,
                                discovery_error(&error.to_string())
                            )),
                        )
                    }
                    Err(_) => {
                        return (
                            configured_model_row(&profile),
                            Some(format!("{} · model discovery timed out", profile.name)),
                        )
                    }
                };
            let mut models = discovered;
            if !profile.model.is_empty() && profile.kind != "openai_chatgpt_plan" {
                models.push(profile.model.clone());
            }
            models.sort();
            models.dedup();
            (
                models
                    .into_iter()
                    .map(|model| (profile.name.clone(), model))
                    .collect::<Vec<_>>(),
                None,
            )
        }
    }))
    .await;
    let mut model_rows = Vec::new();
    let mut errors = Vec::new();
    for (rows, error) in results {
        model_rows.extend(rows);
        if let Some(error) = error {
            errors.push(error);
        }
    }
    (normalize_model_rows(model_rows), errors)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelCatalogCache {
    fetched_at: u64,
    models: Vec<(String, String)>,
}

fn configured_model_rows(settings: &Settings) -> Vec<(String, String)> {
    settings
        .providers
        .iter()
        .filter(|profile| settings.can_use(profile) && !profile.model.trim().is_empty())
        .map(|profile| (profile.name.clone(), profile.model.clone()))
        .collect()
}

fn ready_model_rows(settings: &Settings, rows: Vec<(String, String)>) -> Vec<(String, String)> {
    normalize_model_rows(
        rows.into_iter()
            .filter(|(name, _)| {
                settings
                    .profile(name)
                    .is_some_and(|profile| settings.can_use(profile))
            })
            .collect(),
    )
}

fn model_catalog_cache_path(settings: &Settings) -> std::path::PathBuf {
    settings.dir.join("model_catalog.json")
}

fn load_model_catalog_cache(
    settings: &Settings,
) -> (Vec<(String, String)>, Option<std::time::Instant>) {
    let path = model_catalog_cache_path(settings);
    let Ok(bytes) = std::fs::read(path) else {
        return (Vec::new(), None);
    };
    let Ok(cache) = serde_json::from_slice::<ModelCatalogCache>(&bytes) else {
        return (Vec::new(), None);
    };
    let now = std::time::SystemTime::now();
    let fetched = std::time::UNIX_EPOCH + Duration::from_secs(cache.fetched_at);
    let age = now.duration_since(fetched).unwrap_or_default();
    let fetched_instant = std::time::Instant::now()
        .checked_sub(age)
        .unwrap_or_else(std::time::Instant::now);
    (normalize_model_rows(cache.models), Some(fetched_instant))
}

fn save_model_catalog_cache(settings: &Settings, models: &[(String, String)]) {
    let cache = ModelCatalogCache {
        fetched_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        models: models.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec(&cache) else {
        return;
    };
    let path = model_catalog_cache_path(settings);
    let temporary = path.with_extension("json.tmp");
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

fn discovery_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("not in this chatgpt account") {
        return "model unavailable to this ChatGPT account; run /models refresh".into();
    }
    if lower.contains("bad or missing api key") || lower.contains("api key is required") {
        return "API key missing or rejected; open /providers to configure it".into();
    }
    if lower.contains("cannot connect to provider") {
        return "provider unreachable; check URL, DNS, TLS and connectivity".into();
    }
    if lower.contains("dns") || lower.contains("resolve") {
        "DNS resolution failed".into()
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "request timed out".into()
    } else if lower.contains("401") || lower.contains("403") || lower.contains("unauthorized") {
        "authentication or permission required".into()
    } else if lower.contains("404") || lower.contains("not found") {
        "model discovery endpoint unsupported".into()
    } else {
        "provider request failed".into()
    }
}

fn configured_model_row(profile: &ProviderProfile) -> Vec<(String, String)> {
    if profile.model.is_empty() {
        Vec::new()
    } else {
        vec![(profile.name.clone(), profile.model.clone())]
    }
}

fn normalize_model_rows(mut rows: Vec<(String, String)>) -> Vec<(String, String)> {
    // Provider/profile is part of identity: identical model IDs from two
    // providers must remain separate selectable rows.
    rows.sort();
    rows.dedup();
    rows
}

/// Replace one provider's rows with the complete result of its latest
/// discovery while preserving every other provider/profile in the catalog.
/// The configured model is retained as a manual fallback when a provider's
/// endpoint omits it from the remote response.
fn merge_provider_models(
    catalog: &[(String, String)],
    provider: &str,
    discovered: Vec<String>,
    configured: &str,
) -> Vec<(String, String)> {
    let mut merged = catalog
        .iter()
        .filter(|(name, _)| name != provider)
        .cloned()
        .collect::<Vec<_>>();
    merged.extend(
        discovered
            .into_iter()
            .filter(|model| !model.trim().is_empty())
            .map(|model| (provider.to_owned(), model)),
    );
    if !configured.trim().is_empty() {
        merged.push((provider.to_owned(), configured.to_owned()));
    }
    normalize_model_rows(merged)
}

/// Rows to keep from the previous catalog when discovery fails: exactly the
/// cached rows whose provider reported an error this refresh, so a flaky
/// network never wipes known models and a fixed provider still refreshes.
fn retain_failed_rows(catalog: &[(String, String)], errors: &[String]) -> Vec<(String, String)> {
    catalog
        .iter()
        .filter(|row| {
            errors
                .iter()
                .any(|error| error.starts_with(&format!("{} ·", row.0)))
        })
        .cloned()
        .collect()
}

/// True when a prompt looks like it wants the CTF autopilot (flag, challenge,
/// or CTF mentioned) so the `/ctf` offer shows once per session.
fn mentions_ctf(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("flag{") || lower.contains("ctf") || lower.contains("challenge")
}

/// `/fork [name]` branches at the current end; `/fork <n> [name]` keeps the
/// first `n` messages — the numbers `/history` prints. A leading token that
/// is not a number is the fork's name.
fn parse_fork_args(args: &str) -> (Option<usize>, String) {
    let mut tokens = args.split_whitespace();
    match tokens.next() {
        Some(token) => match token.parse::<usize>() {
            Ok(keep) => (Some(keep), tokens.collect::<Vec<_>>().join(" ")),
            Err(_) => (None, args.to_string()),
        },
        None => (None, String::new()),
    }
}

/// Build the copy-safe representation of the current session.
///
/// Clipboard export intentionally uses transcript state rather than the
/// rendered terminal buffer. This preserves complete text while avoiding
/// borders, truncation, spinner frames, and other presentation-only details.
/// Secrets are redacted at this boundary before they leave the process.
fn transcript_for_clipboard(entries: &[Entry], flags: &[(String, String)]) -> String {
    let mut output = String::new();
    for entry in entries {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(match entry.speaker {
            Speaker::User => "YOU",
            Speaker::Agent => "WROSECODE",
            Speaker::Tool => "TOOL",
            Speaker::System => "INFO",
        });
        output.push('\n');
        output.push_str(&crate::crash::redact(&entry.text));
    }

    if !flags.is_empty() {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str("FLAGS\n");
        for (index, (flag, source)) in flags.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            output.push_str(&crate::crash::redact(flag));
            if !source.trim().is_empty() {
                output.push_str("  · ");
                output.push_str(&crate::crash::redact(source));
            }
        }
    }

    output
}

#[allow(clippy::too_many_arguments)]
async fn run_command(
    line: &str,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
    session: &mut Session,
    events: &mut mpsc::UnboundedReceiver<Progress>,
    permissions: &mut mpsc::UnboundedReceiver<PermissionRequest>,
    terminal_guard: &TerminalGuard,
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
        "/copy" => {
            if !args.is_empty() && !args.eq_ignore_ascii_case("all") {
                ui.push(Speaker::System, "Usage: /copy [all]");
            } else {
                let text = transcript_for_clipboard(&ui.entries, &ui.flag_log);
                if text.is_empty() {
                    ui.status = "Nothing to copy · the current session is empty".into();
                } else {
                    match crate::ctf::copy_to_clipboard(&text) {
                        Ok(()) => {
                            ui.status = format!(
                                "Copied all session content · {} transcript entries",
                                ui.entries.len()
                            );
                        }
                        Err(error) => ui.push(
                            Speaker::System,
                            format!(
                                "Copy failed: {error}\nInstall wl-copy (Wayland), xclip, or xsel and retry /copy."
                            ),
                        ),
                    }
                }
            }
        }
        "/mode" => {
            const MODES: [(&str, &str); 5] = [
                ("general", "explore, plan, edit, verify"),
                ("ctf", "recon, exploit, flag verification"),
                ("bounty", "scoped bug bounty, non-destructive proof"),
                ("build", "full build agent"),
                ("plan", "read-only plan agent"),
            ];
            let requested = if args.trim().is_empty() {
                let labels: Vec<String> = MODES
                    .iter()
                    .map(|(mode, description)| {
                        let active =
                            agent.mode == *mode || (agent.mode == "bugbounty" && *mode == "bounty");
                        format!("{} {mode} · {description}", if active { "●" } else { " " })
                    })
                    .collect();
                picker(ui, "Mode", labels)?.map(|index| MODES[index].0.to_string())
            } else {
                Some(args.trim().to_ascii_lowercase())
            };
            if let Some(requested) = requested {
                let mode = if requested == "bounty" {
                    "bugbounty"
                } else {
                    requested.as_str()
                };
                if switch_mode(agent, ui, mode).is_err() {
                    ui.push(
                        Speaker::System,
                        "Usage: /mode general|ctf|bounty|build|plan",
                    );
                } else {
                    if mode == "bugbounty" {
                        ui.push(
                            Speaker::System,
                            "Bug-bounty mode. Run /bugbounty <target> --scope … to arm an authorized scope before any network test.",
                        );
                    }
                    ui.status = format!("{} mode", ui.mode);
                    ui.push(Speaker::System, format!("{} mode active", ui.mode));
                }
            }
        }
        "/build" | "/plan" => {
            let mode = name.trim_start_matches('/');
            switch_mode(agent, ui, mode)?;
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
            let modes = ["build", "plan", "general", "ctf", "bugbounty"];
            let user = crate::markdown::discover_agents(&agent.config.root);
            let mut labels: Vec<String> = modes
                .iter()
                .map(|mode| {
                    let active = agent.mode == *mode;
                    format!("{} {mode}", if active { "●" } else { " " })
                })
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
                    switch_mode(agent, ui, mode)?;
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
                ask_line(ui, "Verbosity (on/off/compact/normal/verbose)", &current)?
            } else {
                Some(args.into())
            };
            if let Some(mode) = mode {
                let canonical =
                    crate::config::normalize_verbosity(&mode.to_ascii_lowercase()).to_string();
                if matches!(canonical.as_str(), "compact" | "normal" | "verbose") {
                    ui.verbosity = canonical.clone();
                    // Switching off re-collapses previous automatic verbose
                    // expansions without losing user-expanded tool details.
                    let ids: Vec<String> = ui.tool_cells.keys().cloned().collect();
                    for id in ids {
                        ui.refresh_cell(&id);
                    }
                    ui.push(
                        Speaker::System,
                        format!("Verbosity: {canonical} · session only"),
                    );
                } else {
                    ui.push(
                        Speaker::System,
                        "Usage: /verbosity on|off|compact|normal|verbose",
                    );
                }
            }
        }
        "/activity" => match args.trim() {
            "" => ui.push(
                Speaker::System,
                format!(
                    "Activity rail: {} (wide terminals >=110 columns). Use /activity on|off",
                    if ui.activity_rail { "on" } else { "off" }
                ),
            ),
            "on" => {
                ui.activity_rail = true;
                ui.push(Speaker::System, "Live activity rail enabled");
            }
            "off" => {
                ui.activity_rail = false;
                ui.push(Speaker::System, "Live activity rail disabled");
            }
            _ => ui.push(Speaker::System, "Usage: /activity on|off"),
        },
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
            let query = args.trim().to_ascii_lowercase();
            let flags = ui
                .flag_log
                .iter()
                .filter(|(flag, source)| {
                    query.is_empty()
                        || flag.to_ascii_lowercase().contains(&query)
                        || source.to_ascii_lowercase().contains(&query)
                })
                .map(|(flag, source)| format!("{flag}\t{source}"))
                .collect::<Vec<_>>();
            ui.push(
                Speaker::System,
                if flags.is_empty() {
                    "No flags found in the current session".into()
                } else {
                    format!("Current session flags:\n{}", flags.join("\n"))
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
            // No target means SWITCH MODE only. Previously /ctf silently
            // started unlimited autopilot on ".", trapping later input in the
            // queue while the model spun forever without a challenge.
            if args.trim().is_empty() {
                agent.set_mode("ctf")?;
                ui.mode = "CTF".into();
                agent.ctf.begin_run();
                agent.ctf.unlock_category();
                let root = agent.config.root.clone();
                let scope = crate::autopilot::Scope::for_target("", &root, None);
                agent.tools.set_scope(Some(scope.clone()));
                ui.category = agent.ctf.category.clone();
                ui.status = "CTF mode ready".into();
                ui.push(Speaker::System, format!("SCOPE   {}", scope.describe()));
                ui.push(
                    Speaker::System,
                    "CTF agent ready. Enter a challenge prompt, or use /ctf <file|dir|url|description> for autonomous flag solving. Use /build to switch back.",
                );
                return Ok(());
            }
            let options = match crate::autopilot::parse_command_args(args) {
                Ok(options) => options,
                Err(error) => {
                    ui.push(
                        Speaker::System,
                        format!("{error}. Try /ctf [target or description] [--flag-format F] [--category C] [--remote host:port] [--budget B] [--parallel N]"),
                    );
                    return Ok(());
                }
            };
            agent.set_mode("ctf")?;
            ui.mode = "CTF".into();
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
            agent.ctf.begin_run();
            agent.ctf.lock_category();
            ui.status = "Scanning local CTF evidence…".into();
            ui.render()?;
            if let Some(verified) = crate::autopilot::preflight_local_flag_async(
                agent.ctf.clone(),
                root.clone(),
                options.target.clone(),
            )
            .await
            {
                let category = options
                    .category
                    .clone()
                    .unwrap_or_else(|| crate::ctf::categorize(&root, &options.target));
                agent.ctf.category = category.clone();
                ui.category = category.clone();
                ui.push(Speaker::System, format!("SCOPE   {}", scope.describe()));
                ui.push(
                    Speaker::System,
                    "TOOLS   local artifact verifier (model probes skipped)",
                );
                ui.push(Speaker::System, format!("GUESS   {category}"));
                ui.push(
                    Speaker::System,
                    format!("✔ flag verified: {} ({})", verified.flag, verified.evidence),
                );
                ui.flag_log
                    .push((verified.flag.clone(), format!("file:{}", options.target)));
                ui.verified = true;
                ui.status = "CTF solved".into();
                sync_session(session, agent, ui);
                if session.summary == "New session" {
                    session.summary = format!("ctf {}", options.target);
                }
                let flags = vec![verified.flag];
                match crate::report::writeup_challenge(&root, session, &flags, None) {
                    Ok(path) => ui.push(Speaker::System, format!("Writeup: {}", path.display())),
                    Err(error) => ui.push(Speaker::System, format!("Writeup failed: {error:#}")),
                }
                return Ok(());
            }
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
                let progress = if options.budget.steps == 0 {
                    format!("[{steps}]")
                } else {
                    format!("[{steps}/{}]", options.budget.steps)
                };
                ui.push(
                    Speaker::System,
                    format!("{progress} {}", head.replace('\n', " ")),
                );
                let tokens = agent.usage.input + agent.usage.output;
                let hits = agent.ctf.current_hits();
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
                    "Continue. {} step(s) used, {} tokens spent, budget {}.\n{brief}",
                    steps,
                    tokens,
                    options.budget.describe(),
                );
            }
            let tokens = agent.usage.input + agent.usage.output;
            let hits = agent.ctf.current_hits();
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
        "/bugbounty" => {
            // `/bugbounty` with no arguments switches mode only. It must not
            // invent a scope: reports are generated from verified findings,
            // and verification requires an explicitly armed program scope.
            let root = agent.config.root.clone();
            if args.trim().is_empty() {
                agent.set_mode("bugbounty")?;
                ui.mode = "BOUNTY".into();
                match crate::bugbounty::Workspace::load(&root)? {
                    Some(workspace) => {
                        ui.push(Speaker::System, workspace.status(&root));
                        ui.push(
                            Speaker::System,
                            "No network scope is armed. Rerun /bugbounty <target> --scope <host> to continue the run; the `bugbounty` tool can still list local findings.",
                        );
                    }
                    None => ui.push(
                        Speaker::System,
                        "Bug-bounty mode ready. Use /bugbounty <target> --scope host[,*.wildcard] [--scope-file PATH] [--exclude HOST] [--program NAME] [--budget …] [--parallel N]. /build switches back.",
                    ),
                }
                return Ok(());
            }
            let options = match crate::bugbounty::parse_command_args(args) {
                Ok(options) => options,
                Err(error) => {
                    ui.push(
                        Speaker::System,
                        format!(
                            "{error}. Try /bugbounty <target> --scope host[,*.wildcard] [--scope-file PATH] [--exclude HOST] [--program NAME] [--budget …] [--parallel N]"
                        ),
                    );
                    return Ok(());
                }
            };
            let scope = match crate::bugbounty::build_scope(&root, &options) {
                Ok(scope) => scope,
                Err(error) => {
                    ui.push(Speaker::System, format!("{error}"));
                    return Ok(());
                }
            };
            agent.set_mode("bugbounty")?;
            ui.mode = "BOUNTY".into();
            agent.tools.set_scope(Some(scope.clone()));
            let mut workspace = crate::bugbounty::Workspace::load_or_init(
                &root,
                &options.program,
                &options.target,
                &scope,
            )?;
            ui.push(Speaker::System, format!("PROGRAM {}", workspace.program));
            ui.push(Speaker::System, format!("SCOPE   {}", scope.describe()));
            ui.push(
                Speaker::System,
                format!(
                    "WORKSPACE {}",
                    crate::bugbounty::Workspace::path(&root).display()
                ),
            );
            if workspace.created_at != 0 {
                ui.push(Speaker::System, workspace.status(&root));
            }
            let mut brief = crate::bugbounty::brief(&options, &scope, &workspace, &root);
            let started = std::time::Instant::now();
            let mut steps = 0_usize;
            let mut last_reply: Option<String> = None;
            let mut repeats = 0_usize;
            let stop_reason: String;
            loop {
                ui.status = format!("Bug bounty · step {}", steps + 1);
                let entries_before = ui.entries.len();
                let messages_before = agent.messages.len();
                if !run_turn_queue(agent, ui, &brief, events, permissions).await? {
                    stop_reason = "permission denied".into();
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
                    stop_reason = if cancelled || agent.messages.len() <= messages_before {
                        "stopped by the user".into()
                    } else {
                        "the last turn produced no reply".into()
                    };
                    break;
                };
                steps += 1;
                workspace = crate::bugbounty::after_turn(&root, &workspace)?;
                let tokens = agent.usage.input + agent.usage.output;
                let head: String = reply.trim().chars().take(160).collect();
                let progress = if options.budget.steps == 0 {
                    format!("[{steps}]")
                } else {
                    format!("[{steps}/{}]", options.budget.steps)
                };
                ui.push(
                    Speaker::System,
                    format!("{progress} {}", head.replace('\n', " ")),
                );
                if reply.contains("BOUNTY COMPLETE") {
                    stop_reason = "the agent declared the run complete".into();
                    break;
                }
                if last_reply.as_deref() == Some(reply.trim()) {
                    repeats += 1;
                    if repeats >= 2 {
                        stop_reason =
                            "the agent repeated itself twice; stopping instead of looping".into();
                        break;
                    }
                } else {
                    repeats = 0;
                    last_reply = Some(reply.trim().to_string());
                }
                if let Some(reason) = options.budget.exhausted(steps, tokens, started.elapsed()) {
                    stop_reason = format!("budget exhausted ({reason})");
                    break;
                }
                brief = crate::bugbounty::continuation_brief(&workspace, &root, steps, tokens);
            }
            let tokens = agent.usage.input + agent.usage.output;
            let outcome =
                crate::bugbounty::finish(&root, &mut workspace, steps, tokens, &stop_reason);
            ui.push(Speaker::System, crate::bugbounty::stop_report(&outcome));
            sync_session(session, agent, ui);
            if session.summary == "New session" {
                session.summary = format!("bugbounty {}", options.target);
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
            agent.tools.set_scope(None);
            ui.clear_session_flags();
            ui.clear_transcript();
            ui.plan.clear();
            ui.dash_activity.clear();
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
                .map(|s| {
                    format!(
                        "{}  {}  {}",
                        crate::session::age_text(s.created),
                        s.name,
                        s.summary
                    )
                })
                .collect();
            if let Some(index) = picker(ui, "Sessions", labels)? {
                *session = sessions[index].clone();
                restore_session(session, agent, ui, settings).await?;
                ui.push(Speaker::System, format!("Resumed {}", session.name));
            }
        }
        "/history" => {
            if agent.messages.is_empty() {
                ui.push(Speaker::System, "No messages yet.".to_string());
            } else {
                let mut lines = crate::session::history(&agent.messages);
                lines.push("Fork back to any message with /fork <n> [name]".to_string());
                ui.push(Speaker::System, lines.join("\n"));
            }
        }
        "/fork" => {
            // Keep the current session on disk, then continue in a copy of
            // it: both sides share the history up to this point and diverge
            // from here. `/fork <n> [name]` branches from message n instead
            // of the current end (pi-mono fork-at-message); /history lists
            // the numbers.
            save_session(session, agent, ui, &session_dir)?;
            let parent = session.name.clone();
            let (keep, name) = parse_fork_args(args);
            let partial = keep.is_some_and(|keep| keep < session.messages.len());
            let mut forked = match session.branch(keep) {
                Ok(forked) => forked,
                Err(error) => {
                    ui.push(Speaker::System, format!("{error:#}"));
                    return Ok(());
                }
            };
            if !name.is_empty() {
                if let Err(error) = forked.rename(&session_dir, &name) {
                    ui.push(Speaker::System, format!("{error:#}"));
                    return Ok(());
                }
            }
            *session = forked;
            if partial {
                // A rewound context needs a rewound scrollback: drop the
                // row-keyed tool/timeline state, then reload the shorter
                // transcript the fork rebuilt from its kept messages.
                ui.clear_transcript();
                apply_session_state(session, agent, ui);
            }
            save_session(session, agent, ui, &session_dir)?;
            ui.push(
                Speaker::System,
                match keep {
                    Some(keep) => {
                        format!("Forked {parent} into {} at message {keep}", session.name)
                    }
                    None => format!("Forked {parent} into {}", session.name),
                },
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
        "/skills" if args == "reload" => {
            skills::invalidate_cache(&agent.config.root, &agent.config.skill_dirs);
            agent.skills = skills::discover(&agent.config.root, &agent.config.skill_dirs)?;
            ui.push(
                Speaker::System,
                format!("Reloaded {} skills from disk", agent.skills.len()),
            );
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
                        skills::invalidate_cache(&agent.config.root, &agent.config.skill_dirs);
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
                        skills::invalidate_cache(&agent.config.root, &agent.config.skill_dirs);
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
        "/auth" | "/chatgpt" => {
            auth_command(args, agent, ui, settings).await?;
        }
        "/providers" => {
            provider_screen(agent, ui, settings).await?;
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
            let force_refresh = args.trim().eq_ignore_ascii_case("refresh");
            let stale = ui
                .model_catalog_fetched
                .map(|when| when.elapsed() >= Duration::from_secs(600))
                .unwrap_or(true);
            // Normal picker opening is local-only. Network discovery is an
            // explicit refresh so DNS/TLS/provider latency never blocks model
            // selection or makes a stale catalog disappear.
            if force_refresh {
                let started = std::time::Instant::now();
                let (models, errors) = available_models(agent, settings).await;
                // Failed discovery never wipes what we knew: keep the cached
                // rows for exactly the providers that errored.
                let mut merged = models;
                merged.extend(retain_failed_rows(&ui.model_catalog, &errors));
                ui.model_catalog = ready_model_rows(settings, merged);
                ui.model_catalog_errors = errors;
                ui.model_catalog_fetched = Some(started);
                save_model_catalog_cache(settings, &ui.model_catalog);
            } else if stale {
                ui.model_catalog_errors = vec![
                    "Model catalog is stale · showing cached/configured models · use /models refresh"
                        .into(),
                ];
            }
            if force_refresh {
                for error in ui.model_catalog_errors.clone() {
                    ui.push(Speaker::System, error);
                }
            }
            let models = ready_model_rows(settings, ui.model_catalog.clone());
            if models.is_empty() {
                ui.push(
                    Speaker::System,
                    "No connected models. Run /chatgpt login or /providers, then /models refresh."
                        .to_string(),
                );
            } else {
                // Identity stays `(provider id, model)`; only the label
                // prefers the human display name. Sorted rows keep every
                // provider's models grouped together.
                let labels = models
                    .iter()
                    .map(|(name, model)| {
                        let display = settings
                            .profile(name)
                            .map(Settings::display)
                            .unwrap_or(name);
                        format!("{display}  /  {model}")
                    })
                    .collect();
                if let Some(index) = picker(ui, "Models", labels)? {
                    switch_provider_checked(
                        agent,
                        ui,
                        settings,
                        &models[index].0,
                        &models[index].1,
                    )
                    .await?;
                }
            }
        }
        "/mcps" | "/mcp" => {
            mcp_command(args, agent, ui, settings).await?;
        }
        "/editor" => {
            let path =
                std::env::temp_dir().join(format!("wrosecode-prompt-{}.txt", std::process::id()));
            std::fs::write(&path, "")?;
            terminal_guard.suspend();
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let parts: Vec<_> = editor.split_whitespace().collect();
            let status = std::process::Command::new(parts[0])
                .args(&parts[1..])
                .arg(&path)
                .status();
            terminal_guard.resume()?;
            ui.renderer.size = (0, 0);
            ui.renderer.full_redraw = true;
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
            "Step 1/8 · Provider ID valid • Enter accept • Esc cancel".into()
        } else {
            "Step 1/8 · Use a unique ID: a-z, 0-9, - or _".into()
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
        if existing.is_none() && name == "openai" {
            // `/connect openai` offers both doors: ChatGPT browser sign-in
            // next to the plain API-key path.
            let choice = picker(
                ui,
                "Auth",
                vec![
                    "Sign in with ChatGPT (browser OAuth)".into(),
                    "Use API key".into(),
                ],
            )?;
            match choice {
                Some(0) => return oauth_connect(name, agent, ui, settings).await,
                Some(_) => {}
                None => return Ok(()),
            }
        }
        let previous = existing.clone();
        // Step 2 — display label. Esc cancels the whole form.
        let default_display = previous
            .as_ref()
            .map(|profile| Settings::display(profile).to_string())
            .filter(|label| label != &name)
            .unwrap_or_default();
        let Some(display) = ask_line(
            ui,
            "Step 2/8 · Display name (blank keeps the ID)",
            &default_display,
        )?
        else {
            return Ok(());
        };
        // Step 3 — API compatibility, offered as a choice instead of a
        // free-text field. Esc cancels the whole form.
        let kind = if let Some(profile) = &previous {
            profile.kind.clone()
        } else {
            let choice = picker(
                ui,
                "Step 3/8 · API type",
                vec![
                    "OpenAI-compatible  ·  /v1/models + chat completions".into(),
                    "Anthropic-compatible  ·  /v1/models + messages".into(),
                ],
            )?;
            match choice {
                Some(0) => "openai_compat".to_string(),
                Some(_) => "anthropic".to_string(),
                None => return Ok(()),
            }
        };
        let default_url = previous.as_ref().map(|p| p.base_url.as_str()).unwrap_or("");
        let Some(base_url) = ask_line(ui, "Step 4/8 · Base URL", default_url)? else {
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
        let Some(entered_key) = ask_secret(ui, "Step 5/8 · API key (masked; empty keeps current)")?
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
        let mut profile = ProviderProfile {
            name: name.clone(),
            display_name: display.clone(),
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
            auth_method: previous
                .as_ref()
                .map(|p| p.auth_method.clone())
                .unwrap_or_else(|| "api_key".into()),
            oauth_client_id: previous
                .as_ref()
                .map(|p| p.oauth_client_id.clone())
                .unwrap_or_default(),
            oauth_issuer: previous
                .as_ref()
                .map(|p| p.oauth_issuer.clone())
                .unwrap_or_default(),
            oauth_scope: previous
                .as_ref()
                .map(|p| p.oauth_scope.clone())
                .unwrap_or_default(),
        };
        if entered_key.starts_with("env:") && key.is_none() {
            ui.push(
                Speaker::System,
                format!(
                    "{entered_key} is not set in the environment. Set it and retry /providers."
                ),
            );
            return Ok(());
        }
        if key.is_none() && !settings.no_key_needed(&profile) {
            ui.push(
                Speaker::System,
                format!("{name} needs an API key. Reopen /providers to finish setup, or use /chatgpt login."),
            );
            return Ok(());
        }
        ui.status = "Step 6/8 · Discovering every accessible model…".into();
        ui.render()?;
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
                } else if let Some(index) = picker(
                    ui,
                    "Step 6/8 · Choose an optional default model",
                    found.clone(),
                )? {
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
        let Some(model) = ask_line(ui, "Step 6/8 · Default model (optional)", &suggested)? else {
            return Ok(());
        };
        profile.model = model;
        if previous.is_none() {
            loop {
                let Some(header) =
                    ask_line(ui, "Step 6/8 · Extra header K=V (blank to finish)", "")?
                else {
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
        ui.status = "Step 7/8 · Testing provider connection…".into();
        ui.render()?;
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
                "Connection failed · type SAVE to keep, EDIT to retry, Esc to cancel",
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
        // Step 8: immediately populate the live catalog with every model. The
        // previous flow only retained the optional default, so users saw one
        // row until they manually ran `/models refresh`.
        ui.status = "Step 8/8 · Loading the complete model catalog…".into();
        ui.render()?;
        match provider::create_profile(
            &profile,
            key.clone(),
            &profile.model,
            agent.tools.client.clone(),
        ) {
            Ok(provider) => {
                match tokio::time::timeout(Duration::from_secs(15), provider.list_models()).await {
                    Ok(Ok(discovered)) if !discovered.is_empty() => {
                        ui.model_catalog = merge_provider_models(
                            &ui.model_catalog,
                            &name,
                            discovered,
                            &profile.model,
                        );
                        ui.model_catalog_errors
                            .retain(|error| !error.starts_with(&format!("{name} ·")));
                        ui.model_catalog_fetched = Some(std::time::Instant::now());
                        save_model_catalog_cache(settings, &ui.model_catalog);
                        let count = ui
                            .model_catalog
                            .iter()
                            .filter(|row| row.0 == name)
                            .count();
                        ui.push(
                            Speaker::System,
                            format!("{name}: discovered {count} accessible models"),
                        );
                    }
                    Ok(Ok(_)) => ui.push(
                        Speaker::System,
                        format!("{name}: discovery returned no models; configured model retained"),
                    ),
                    Ok(Err(error)) => ui.push(
                        Speaker::System,
                        format!(
                            "{name} · model discovery failed: {} · cached/configured models retained",
                            discovery_error(&error.to_string())
                        ),
                    ),
                    Err(_) => ui.push(
                        Speaker::System,
                        format!("{name} · model discovery timed out · cached/configured models retained"),
                    ),
                }
            }
            Err(error) => ui.push(
                Speaker::System,
                format!(
                    "{name} · model discovery unavailable: {}",
                    discovery_error(&error.to_string())
                ),
            ),
        }
        ui.status = "Ready · provider saved; run /models to select any discovered model".into();
        if !profile.model.is_empty() {
            switch_provider(agent, ui, settings, &name, &profile.model).await?;
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

/// `/connect openai` → "Sign in with ChatGPT": browser loopback or device
/// flow, tokens stored owner-only, profile recorded as `oauth` — the API-key
/// path stays untouched in `provider_form`.
async fn oauth_connect(
    name: String,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
) -> Result<()> {
    let previous = settings.profile(&name).cloned();
    let Some(base_url) = ask_line(
        ui,
        "Base URL",
        previous
            .as_ref()
            .map(|profile| profile.base_url.as_str())
            .unwrap_or("https://api.openai.com/v1"),
    )?
    else {
        return Ok(());
    };
    let base_url = Settings::normalize_url(&base_url)?;
    if base_url.is_empty() {
        anyhow::bail!("base URL is required");
    }
    let Some(model) = ask_line(
        ui,
        "Model",
        previous
            .as_ref()
            .map(|profile| profile.model.as_str())
            .unwrap_or(""),
    )?
    else {
        return Ok(());
    };
    let default_client = previous
        .as_ref()
        .map(|profile| profile.oauth_client_id.as_str())
        .unwrap_or("");
    let default_client = if default_client.is_empty() {
        std::env::var("WROSECODE_OAUTH_CLIENT_ID").unwrap_or_default()
    } else {
        default_client.into()
    };
    let Some(client_id) = ask_line(
        ui,
        "OAuth client ID (from your own OAuth client registration)",
        &default_client,
    )?
    else {
        return Ok(());
    };
    let cfg = crate::oauth::OAuthConfig::new(&client_id, "", "");
    cfg.validate()?;
    let method = picker(
        ui,
        "Sign in",
        vec![
            "Browser (localhost callback)".into(),
            "Device code (headless / remote)".into(),
        ],
    )?;
    let Some(method) = method else {
        return Ok(());
    };
    let client = agent.tools.client.clone();
    let tokens = if method == 0 {
        let (verifier, challenge) = crate::oauth::pkce()?;
        let state = crate::oauth::random_state()?;
        let url = crate::oauth::authorize_url(&cfg, &challenge, &state)?;
        ui.push(
            Speaker::System,
            format!("Open this URL to sign in with ChatGPT:\n{url}"),
        );
        let _ = crate::oauth::open_browser(&url);
        ui.push(
            Speaker::System,
            "Waiting for the browser callback…".to_string(),
        );
        ui.render()?;
        let code =
            crate::oauth::await_callback(cfg.callback_port, &state, crate::oauth::CALLBACK_TIMEOUT)
                .await?;
        crate::oauth::exchange_code(&client, &cfg, &code, &verifier).await?
    } else {
        let pending = crate::oauth::start_device(&client, &cfg).await?;
        ui.push(
            Speaker::System,
            format!("Open {} and enter code {}", pending.url, pending.user_code),
        );
        ui.render()?;
        crate::oauth::await_device(&client, &pending).await?
    };
    let home = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let path = crate::oauth::store_tokens(&home, &name, &tokens)?;
    settings.upsert_provider(ProviderProfile {
        name: name.clone(),
        display_name: String::new(),
        kind: "openai_compat".into(),
        base_url,
        model: model.clone(),
        key_ref: String::new(),
        headers: previous
            .as_ref()
            .map(|profile| profile.headers.clone())
            .unwrap_or_default(),
        think: previous.as_ref().and_then(|profile| profile.think),
        think_map: previous
            .as_ref()
            .and_then(|profile| profile.think_map.clone()),
        builtin: Settings::builtin(&name),
        auth_method: "oauth".into(),
        oauth_client_id: cfg.client_id.clone(),
        oauth_issuer: String::new(),
        oauth_scope: String::new(),
    })?;
    if !model.is_empty() {
        switch_provider(agent, ui, settings, &name, &model).await?;
    }
    ui.push(
        Speaker::System,
        format!(
            "Signed in{}; tokens stored owner-only at {}",
            tokens
                .account_id
                .as_deref()
                .map(|id| format!(" as {id}"))
                .unwrap_or_default(),
            path.display()
        ),
    );
    Ok(())
}

/// Official Sign in with ChatGPT flow for local/open-source plan usage.
/// Credentials are kept by chatgpt_auth, never in providers.toml.
async fn chatgpt_login(agent: &mut Agent, ui: &mut Ui, settings: &mut Settings) -> Result<()> {
    let store = crate::chatgpt_auth::Store::current()?;
    let label =
        ask_line(ui, "ChatGPT account label", "personal")?.unwrap_or_else(|| "personal".into());
    Settings::validate_name(&label)?;
    let previous = store.account(&label)?;
    let pending = crate::chatgpt_auth::begin(&store, previous).await?;
    ui.push(
        Speaker::System,
        "◆ CHATGPT AUTH\n  Opening browser…".to_string(),
    );
    let _ = crate::oauth::open_browser(&pending.url);
    ui.push(
        Speaker::System,
        "  ◌ Waiting for authorization…  (Esc cancels)".to_string(),
    );
    ui.render()?;
    let client = agent.tools.client.clone();
    let login_store = store.clone();
    let login_label = label.clone();
    let mut login = tokio::spawn(async move {
        crate::chatgpt_auth::finish(&login_store, &login_label, pending, &client).await
    });
    let started = std::time::Instant::now();
    let credentials = loop {
        tokio::select! {
            result = &mut login => {
                break result.context("ChatGPT login task stopped")??;
            }
            _ = tokio::time::sleep(Duration::from_millis(120)) => {
                if event::poll(Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        if key.code == KeyCode::Esc {
                            login.abort();
                            ui.push(Speaker::System, "× ChatGPT login cancelled".to_string());
                            return Ok(());
                        }
                    }
                }
                ui.status = format!("◌ Waiting for authorization… {}s · Esc cancel", started.elapsed().as_secs());
                ui.render()?;
            }
        }
    };
    let plan = if credentials.plan_enabled() {
        "enabled"
    } else {
        "not authorized"
    };
    let account_name = if credentials.email.is_empty() {
        "signed-in account"
    } else {
        &credentials.email
    };
    ui.push(
        Speaker::System,
        format!("✓ ChatGPT connected\n  account: {account_name}\n  plan usage: {plan}"),
    );
    if !credentials.plan_enabled() {
        ui.push(
            Speaker::System,
            "Plan usage was not authorized; no inference request was sent.".to_string(),
        );
        return Ok(());
    }
    let profile = ProviderProfile {
        name: label.clone(),
        display_name: String::new(),
        kind: "openai_chatgpt_plan".into(),
        base_url: "https://api.openai.com/v1".into(),
        model: String::new(),
        key_ref: String::new(),
        headers: BTreeMap::new(),
        auth_method: "chatgpt".into(),
        oauth_client_id: credentials.client_id.clone(),
        oauth_issuer: "https://auth.openai.com".into(),
        oauth_scope:
            "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct".into(),
        think: None,
        think_map: None,
        builtin: false,
    };
    settings.upsert_provider(profile)?;
    let saved = settings
        .profile(&label)
        .context("ChatGPT profile was not saved")?;
    let provider = provider::create_profile(saved, None, "", agent.tools.client.clone())?;
    let models = tokio::time::timeout(Duration::from_secs(10), provider.list_models())
        .await
        .context("ChatGPT model discovery timed out")??;
    let selected = if models.is_empty() {
        anyhow::bail!("ChatGPT returned no eligible models")
    } else if models.len() == 1 {
        models[0].clone()
    } else {
        let Some(index) = picker(ui, "ChatGPT models (plan usage)", models.clone())? else {
            return Ok(());
        };
        models[index].clone()
    };
    settings.set_default_model(&label, &selected)?;
    switch_provider(agent, ui, settings, &label, &selected).await?;
    ui.push(
        Speaker::System,
        format!("✓ Ready · {label} / {selected} · using ChatGPT plan"),
    );
    Ok(())
}

async fn auth_command(
    args: &str,
    agent: &mut Agent,
    ui: &mut Ui,
    settings: &mut Settings,
) -> Result<()> {
    match commands::parse_auth(args).map_err(|error| anyhow::anyhow!(error))? {
        commands::AuthCommand::LoginChatGpt => chatgpt_login(agent, ui, settings).await,
        commands::AuthCommand::Status => {
            let store = crate::chatgpt_auth::Store::current()?;
            match store.active()? {
                Some((label, credentials)) => {
                    let refresh_error = if credentials.expires_soon() {
                        crate::chatgpt_auth::refresh_if_needed(&agent.tools.client, &store, &label)
                            .await
                            .err()
                            .map(|error| error.to_string())
                    } else {
                        None
                    };
                    let current = store.account(&label)?.unwrap_or(credentials);
                    let token = match refresh_error {
                        Some(error) => format!("expiring (refresh failed: {error})"),
                        None if current.expires_soon() => "expiring".into(),
                        None => "valid".into(),
                    };
                    ui.push(
                        Speaker::System,
                        format!(
                            "ChatGPT\n  account: {label}\n  status: connected\n  plan usage: {}\n  token: {token}",
                            if current.plan_enabled() {
                                "enabled"
                            } else {
                                "not authorized"
                            }
                        ),
                    );
                }
                None => ui.push(
                    Speaker::System,
                    "ChatGPT\n  status: not connected".to_string(),
                ),
            }
            Ok(())
        }
        commands::AuthCommand::Accounts => {
            let store = crate::chatgpt_auth::Store::current()?;
            let rows = store
                .accounts()?
                .into_iter()
                .map(|(label, active, credentials)| {
                    format!(
                        "{} {label}  {}",
                        if active { "●" } else { "○" },
                        credentials
                            .map(|c| if c.plan_enabled() {
                                "plan enabled"
                            } else {
                                "identity only"
                            })
                            .unwrap_or("signed out")
                    )
                })
                .collect::<Vec<_>>();
            ui.push(
                Speaker::System,
                if rows.is_empty() {
                    "No ChatGPT accounts connected".into()
                } else {
                    format!("ChatGPT accounts\n{}", rows.join("\n"))
                },
            );
            Ok(())
        }
        commands::AuthCommand::Switch(label) => {
            let store = crate::chatgpt_auth::Store::current()?;
            store.switch(&label)?;
            let profile = settings
                .profile(&label)
                .cloned()
                .context("no provider profile for ChatGPT account")?;
            if profile.model.is_empty() {
                anyhow::bail!("ChatGPT account has no selected model; run /models")
            }
            switch_provider(agent, ui, settings, &label, &profile.model).await
        }
        commands::AuthCommand::Logout(label) => {
            let store = crate::chatgpt_auth::Store::current()?;
            let revoked = crate::chatgpt_auth::logout(&agent.tools.client, &store, &label).await?;
            ui.push(Speaker::System, if revoked { format!("✓ Logged out of ChatGPT account {label}") } else { format!("✓ Removed local ChatGPT credentials for {label}; remote revocation was unavailable") });
            Ok(())
        }
    }
}

async fn provider_screen(agent: &mut Agent, ui: &mut Ui, settings: &mut Settings) -> Result<()> {
    ui.set_input(String::new());
    ui.selected = 0;
    loop {
        let matches: Vec<_> = settings
            .providers
            .iter()
            .filter(|profile| {
                let query = ui.input.to_ascii_lowercase();
                profile.name.contains(&query)
                    || Settings::display(profile)
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .cloned()
            .collect();
        let mut rows = vec![
            "+ Sign in with ChatGPT  ·  browser OAuth".to_string(),
            "+ Add API-key / custom provider".to_string(),
        ];
        rows.extend(matches.iter().map(|profile| {
            format!(
                "{} {:<18} {:<14} {:<42} {}",
                if agent.config.provider == profile.name {
                    "●"
                } else {
                    "○"
                },
                Settings::display(profile),
                profile.kind,
                profile.base_url,
                settings.status(profile)
            )
        }));
        ui.selected = ui.selected.min(rows.len().saturating_sub(1));
        ui.picker = Some(("Providers".into(), rows));
        ui.picker_all = None;
        ui.picker_query.clear();
        ui.status = "Enter setup · a add custom · e edit · d delete · t test · Esc back".into();
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
                .checked_sub(2)
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
                KeyCode::Down => ui.selected = (ui.selected + 1).min(matches.len() + 1),
                KeyCode::PageUp => ui.selected = ui.selected.saturating_sub(5),
                KeyCode::PageDown => ui.selected = (ui.selected + 5).min(matches.len() + 1),
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
                        // Any entry — built-in defaults included — can go.
                        // Deleting the active provider requires a fallback
                        // with a model, a base URL, and a key first.
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
                                if let Some(fallback) = fallback {
                                    switch_provider(
                                        agent,
                                        ui,
                                        settings,
                                        &fallback.name,
                                        &fallback.model,
                                    )
                                    .await?;
                                } else {
                                    ui.push(
                                        Speaker::System,
                                        "Connect another provider before deleting the active one",
                                    );
                                }
                            }
                            ui.push(Speaker::System, format!("Removed {}", profile.name));
                        }
                    }
                }
                KeyCode::Enter if ui.selected == 0 => {
                    ui.picker = None;
                    chatgpt_login(agent, ui, settings).await?;
                }
                KeyCode::Enter if ui.selected == 1 => {
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
    fn copy_all_formats_the_session_and_redacts_secrets() {
        let mut ui = shell();
        ui.push(Speaker::User, "Inspect the project");
        ui.push(Speaker::Agent, "api_key=copy-secret-value");
        ui.flag_log
            .push(("FLAG{EXAMPLE}".into(), "README.md".into()));

        let copied = transcript_for_clipboard(&ui.entries, &ui.flag_log);

        assert!(copied.contains("YOU\nInspect the project"));
        assert!(copied.contains("WROSECODE\napi_key=***"));
        assert!(copied.contains("FLAGS\nFLAG{EXAMPLE}  · README.md"));
        assert!(!copied.contains("copy-secret-value"));
    }

    #[test]
    fn fork_args_split_a_message_number_from_the_name() {
        assert_eq!(parse_fork_args(""), (None, String::new()));
        assert_eq!(parse_fork_args("backup"), (None, "backup".to_string()));
        assert_eq!(parse_fork_args("4"), (Some(4), String::new()));
        assert_eq!(parse_fork_args("4 backup"), (Some(4), "backup".to_string()));
        // A name that merely starts with digits stays a name.
        assert_eq!(parse_fork_args("3x"), (None, "3x".to_string()));
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
            [
                "dark",
                "light",
                "solarized",
                "dracula",
                "nord",
                "mono",
                "studio"
            ]
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
        assert_eq!(narrow.len(), 2, "compact startup below 80 columns");
        assert!(
            strip_sgr(&narrow[0]).contains("WROSECODE v"),
            "the collapsed wordmark keeps the version: {:?}",
            narrow[0]
        );
        assert!(
            narrow[1].contains("starting workspace"),
            "the compact splash keeps a quiet status row: {:?}",
            narrow
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
    fn startup_screen_is_minimal_and_prompt_forward() {
        let mut ui = shell();
        let (frame, _) = ui.compose(120, 40);
        let joined = frame
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "WROSECODE v",
            "model",
            "project",
            "status",
            "Ask WROSECODE",
            "│ > ",
            "┌",
            "└",
        ] {
            assert!(
                joined.contains(needle),
                "startup screen shows {needle:?}:\n{joined}"
            );
        }
        for clutter in [
            "TRANSCRIPT",
            "session",
            "/providers",
            "tip ·",
            "ctf",
            "mcp",
            "skills",
            "resume",
        ] {
            assert!(
                !joined.contains(clutter),
                "startup screen hides clutter {clutter:?}:\n{joined}"
            );
        }
    }

    #[test]
    fn composer_info_line_names_mode_and_model() {
        let mut ui = shell();
        fill_transcript(&mut ui, 5);
        let (frame, _) = ui.compose(200, 40);
        // No telemetry bar: usage details live in Ctrl+B / /stats.
        assert!(
            !frame.iter().any(|row| row.text.contains("ctx ")),
            "no status bar telemetry"
        );
        let info = &frame[frame.len() - 3].text;
        assert!(info.contains("◈ BUILD"), "mode leads: {info}");
        assert!(info.contains(&ui.model), "model named: {info}");
        assert!(info.contains("/ for commands"), "hint present: {info}");
        assert!(
            frame[0].text.contains(&ui.provider) && frame[0].text.contains(&ui.model),
            "the minimal header names provider and model: {}",
            frame[0].text
        );
        assert!(
            frame[frame.len() - 1].text.contains("Esc cancel"),
            "footer shortcuts: {:?}",
            frame[frame.len() - 1].text
        );
        assert!(
            frame[frame.len() - 2].text.starts_with("└"),
            "composer box closes above the footer: {:?}",
            frame[frame.len() - 2].text
        );
    }

    #[test]
    fn idle_ctrl_c_arms_then_confirms_exit() {
        let mut ui = shell();
        assert!(
            !ui.ctrl_c_idle(),
            "the first idle press only arms the exit window"
        );
        assert!(
            ui.status.contains("press Ctrl+C again"),
            "the first press explains the second: {}",
            ui.status
        );
        assert!(ui.ctrl_c_idle(), "the second press inside the window exits");

        ui.input = "draft".into();
        ui.cursor = 5;
        assert!(
            !ui.ctrl_c_idle(),
            "a press with a draft clears instead of exiting"
        );
        assert!(ui.input.is_empty(), "the draft was cleared");
        assert!(
            ui.status.contains("Input cleared"),
            "clearing is reported: {}",
            ui.status
        );

        ui.last_interrupt = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(3))
                .expect("a recent-enough clock"),
        );
        assert!(
            !ui.ctrl_c_idle(),
            "an expired window does not exit on the next press"
        );
    }

    #[test]
    fn tool_command_lines_wrap_instead_of_clipping() {
        let command = format!("run {}", "x".repeat(200));
        let entry = Entry {
            speaker: Speaker::Tool,
            text: format!("{command}\noutput line"),
        };
        let rows = entry_lines(&entry, 40, false);
        assert!(
            rows.len() > 2,
            "a long command needs several wrapped rows: {rows:?}"
        );
        assert!(
            rows[0].starts_with("run "),
            "tool rows carry no label prefix: {:?}",
            rows[0]
        );
        assert!(
            !rows.iter().any(|row| row.contains("TOOL")),
            "no legacy labels: {rows:?}"
        );
        let mut seen = String::new();
        // The command wraps to six rows here; the seventh row is the
        // (separately capped) output body.
        assert_eq!(rows.len(), 7, "command rows plus output: {rows:?}");
        for (index, row) in rows.iter().enumerate().take(6) {
            let body = if index == 0 {
                row.as_str()
            } else {
                row.strip_prefix("  ").unwrap_or(row)
            };
            seen.push_str(body);
        }
        assert!(
            seen.contains(&command),
            "the full command remains visible: {rows:?}"
        );
        for row in &rows {
            assert!(
                row.chars().count() <= 40,
                "wrapped rows still fit the pane: {row:?}"
            );
        }
    }

    #[test]
    fn tool_output_states_read_green_amber_red_on_dark() {
        let mut ui = shell();
        ui.push(Speaker::User, "list files");
        ui.push(
            Speaker::Tool,
            "✓ Ran shell ls -la /home/kali/Desktop  0.0s\n   └─ 127 lines · expand for details",
        );
        ui.push(Speaker::Tool, "◌ Running tests");
        ui.push(Speaker::Tool, "✗ Edit file failed\n   access denied");
        let (frame, _) = ui.compose(100, 30);
        let title = frame
            .iter()
            .find(|row| row.text.contains("Ran shell ls"))
            .expect("tool title renders");
        assert_eq!(title.fg, Color::Green, "done title highlighted green");
        assert_ne!(title.bg, Color::Reset, "dark background, never transparent");
        let detail = frame
            .iter()
            .find(|row| row.text.contains("127 lines"))
            .expect("tool detail renders");
        assert_eq!(
            detail.fg, WROSE_DIM_GREEN,
            "detail lines match in muted green"
        );
        assert_eq!(detail.bg, title.bg, "one dark surface");
        let running = frame
            .iter()
            .find(|row| row.text.contains("Running tests"))
            .expect("running row renders");
        assert_eq!(
            running.fg, WROSE_AMBER,
            "running processes glow amber, not green"
        );
        let failed = frame
            .iter()
            .find(|row| row.text.contains("Edit file failed"))
            .expect("failed row renders");
        assert_eq!(failed.fg, WROSE_ERROR, "failures read muted red");
    }

    #[test]
    fn thinking_and_errors_keep_their_voice() {
        let mut ui = shell();
        ui.begin_turn();
        ui.push(Speaker::System, "✗ Provider request failed");
        let (frame, _) = ui.compose(100, 30);
        let thinking = frame
            .iter()
            .find(|row| row.text.contains("Thinking"))
            .expect("thinking cell renders");
        assert_eq!(thinking.fg, WROSE_AMBER, "thinking glows amber");
        let error = frame
            .iter()
            .find(|row| row.text.contains("Provider request failed"))
            .expect("error renders");
        assert_eq!(error.fg, WROSE_ERROR, "failures stay muted red");
    }

    #[test]
    fn timeline_does_not_reserve_a_subagent_tree() {
        let mut ui = shell();
        for index in 0..10 {
            ui.tool_cells.insert(
                format!("call-{index}"),
                ToolCell {
                    title: format!("Delegated part {index}"),
                    running: true,
                    ok: false,
                    output: String::new(),
                    elapsed_ms: 0,
                    exit_code: None,
                    row: index,
                },
            );
        }
        let (frame, _) = ui.compose(80, 24);
        let joined = frame
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("SUBAGENT TREE"),
            "no permanent tree:\n{joined}"
        );
        assert!(
            joined.contains("Ask WROSECODE"),
            "composer owns the bottom:\n{joined}"
        );

        for cell in ui.tool_cells.values_mut() {
            cell.running = false;
        }
        ui.finished_children = 4;
        ui.finished_ms = 12_400;
        let (frame, _) = ui.compose(80, 24);
        let joined = frame
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("SUBAGENT TREE"),
            "finished work has no reserved pane:\n{joined}"
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
    fn the_thinking_panel_reports_the_announced_model_stage() {
        let mut ui = shell();
        ui.apply_progress(Progress::ModelStage("Preparing request context…".into()));
        let stats = ui.dash_stats();
        assert_eq!(stats.stage, "Preparing request context…");
        let (_, thinking) = dashboard_panels(&stats, 100)
            .into_iter()
            .find(|(title, _)| *title == "THINKING")
            .expect("the THINKING panel");
        assert!(
            thinking
                .iter()
                .any(|line| line.contains("stage: Preparing request context…")),
            "{thinking:?}"
        );

        // A stage announced while a tool is live is still a stage: dropping
        // it would leave a long turn with no progress signal at all.
        ui.apply_progress(Progress::ToolBegin {
            id: "t1".into(),
            title: "Read read_file src/main.rs".into(),
        });
        ui.apply_progress(Progress::ModelStage("Sending prompt to model".into()));
        assert_eq!(ui.dash_stats().stage, "Sending prompt to model");

        // Transient UI state must not be mistaken for the model stage.
        ui.apply_progress(Progress::TextDelta("hi".into()));
        assert_eq!(ui.status, "Streaming");
        assert_eq!(ui.dash_stats().stage, "Streaming");
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
            !frame[0].text.contains("session"),
            "startup art should not expose session noise: {:?}",
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

    #[test]
    fn model_catalog_keeps_same_model_id_for_distinct_profiles() {
        let rows = normalize_model_rows(vec![
            ("groq".into(), "openai/gpt-oss-120b".into()),
            ("custom".into(), "openai/gpt-oss-120b".into()),
            ("groq".into(), "openai/gpt-oss-120b".into()),
        ]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.0 == "groq"));
        assert!(rows.iter().any(|row| row.0 == "custom"));
    }

    #[test]
    fn discovered_models_group_five_deep_under_one_provider() {
        // The reported bug: only one model per provider survived. Every
        // discovered id must remain its own selectable row, sorted.
        let rows = normalize_model_rows(vec![
            ("mine".into(), "model-3".into()),
            ("mine".into(), "model-1".into()),
            ("mine".into(), "model-5".into()),
            ("mine".into(), "model-2".into()),
            ("mine".into(), "model-4".into()),
        ]);
        assert_eq!(
            rows.iter().map(|row| row.1.as_str()).collect::<Vec<_>>(),
            vec!["model-1", "model-2", "model-3", "model-4", "model-5"],
            "all five models stay listed under the provider"
        );
    }

    #[test]
    fn saving_a_provider_replaces_its_old_default_with_all_discovered_models() {
        let catalog = vec![
            ("mine".into(), "old-default".into()),
            ("other".into(), "other-1".into()),
        ];
        let merged = merge_provider_models(
            &catalog,
            "mine",
            vec!["model-1".into(), "model-2".into(), "model-3".into()],
            "model-2",
        );
        assert_eq!(
            merged,
            vec![
                ("mine".into(), "model-1".into()),
                ("mine".into(), "model-2".into()),
                ("mine".into(), "model-3".into()),
                ("other".into(), "other-1".into()),
            ]
        );
    }

    #[test]
    fn empty_discovery_keeps_configured_model_as_manual_fallback() {
        let merged = merge_provider_models(
            &[("mine".into(), "old-default".into())],
            "mine",
            Vec::new(),
            "manual-model",
        );
        assert_eq!(merged, vec![("mine".into(), "manual-model".into())]);
    }

    #[test]
    fn failed_discovery_retains_only_the_errored_providers_rows() {
        let catalog = vec![
            ("up".into(), "a".into()),
            ("down".into(), "b".into()),
            ("down".into(), "c".into()),
        ];
        let kept = retain_failed_rows(&catalog, &["down · model discovery failed: x".into()]);
        assert_eq!(
            kept,
            vec![("down".into(), "b".into()), ("down".into(), "c".into())],
            "cached rows survive a failed refresh, others refresh away"
        );
        assert!(
            retain_failed_rows(&catalog, &[]).is_empty(),
            "a clean refresh replaces everything"
        );
    }

    #[test]
    fn studio_mode_uses_full_width_clean_tool_timeline() {
        let mut ui = shell();
        ui.theme = theme_index("studio");
        ui.verbosity = "normal".into();
        ui.push(Speaker::User, "inspect the project");
        ui.apply_progress(Progress::ToolBegin {
            id: "read-clean".into(),
            title: "Read read_file src/main.rs".into(),
        });
        ui.apply_progress(Progress::ToolEnd {
            id: "read-clean".into(),
            ok: true,
            output: "5 lines inspected".into(),
            elapsed_ms: 145,
        });
        ui.push(Speaker::Agent, "Done.\nAll relevant files were inspected.");
        let (frame, _) = ui.compose(132, 45);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Read  src/main.rs"), "{text}");
        assert!(text.contains("145ms"), "{text}");
        assert!(text.contains("5 lines inspected"), "{text}");
        assert!(text.contains("  ◈ WROSECODE"), "{text}");
        assert!(!text.contains("LIVE TOOLS"), "{text}");
        assert!(!text.contains("┌─ ◈ WROSECODE"), "{text}");
        assert!(frame.iter().all(|row| row.text.chars().count() <= 132));
        assert!(frame.iter().all(|row| row.bg != Color::Reset));
    }

    #[test]
    fn studio_streamed_and_final_answer_match() {
        let prompt = "Quick summary:\n\nA short paragraph.\n";
        for split in 1..=prompt.len() {
            if !prompt.is_char_boundary(split) {
                continue;
            }
            let mut ui = shell();
            ui.draft = prompt[..split].to_string();
            let (cached, tail) = ui.prepare_draft(79, false, STUDIO_PANEL);
            let mut rendered: Vec<String> = ui
                .draft_cache
                .rows
                .iter()
                .map(|(row, _)| row.clone())
                .collect();
            assert_eq!(rendered.len(), cached);
            rendered.extend(tail.into_iter().map(|(row, _)| row));
            let full: Vec<String> = render_agent_body(&prompt[..split], 79, false, STUDIO_PANEL)
                .into_iter()
                .map(|(row, _)| row)
                .collect();
            assert_eq!(rendered, full, "stream diverged at split {split}");
        }
    }

    #[test]
    fn shell_tool_card_shows_only_its_own_live_output() {
        let mut ui = shell();
        ui.verbosity = "verbose".into();
        ui.apply_progress(Progress::ToolBegin {
            id: "shell-own".into(),
            title: "Ran shell: cargo test".into(),
        });
        ui.dash_procs.push(crate::tools::shell::ProcSnapshot {
            pid: 999,
            command: "something else".into(),
            cwd: "/tmp".into(),
            started: std::time::Instant::now(),
            running: true,
            exit_code: None,
            timed_out: false,
            cpu_pct: None,
            rss_kb: None,
            tail: "unrelated output".into(),
        });
        ui.dash_procs.push(crate::tools::shell::ProcSnapshot {
            pid: 1000,
            command: "cargo test".into(),
            cwd: "/tmp".into(),
            started: std::time::Instant::now(),
            running: true,
            exit_code: None,
            timed_out: false,
            cpu_pct: Some(32.5),
            rss_kb: Some(8192),
            tail: "Compiling crate\nRunning 5 tests".into(),
        });
        ui.refresh_cell("shell-own");
        let row = ui.tool_rows["shell-own"];
        let text = &ui.entries[row].text;
        assert!(text.contains("Run  cargo test"), "{text}");
        assert!(text.contains("Running 5 tests"), "{text}");
        assert!(text.contains("Compiling crate"), "{text}");
        assert!(
            text.contains("PID 1000 · CPU 32.5% · RAM 8.0 MiB (process tree)"),
            "{text}"
        );
        assert!(!text.contains("unrelated output"), "{text}");
        // Off is a presentation change: no CPU/RAM line or long output.
        ui.verbosity = "normal".into();
        ui.refresh_cell("shell-own");
        let compact = &ui.entries[row].text;
        assert!(!compact.contains("PID 1000"), "{compact}");
        assert!(compact.contains("Running 5 tests"), "{compact}");
        assert!(!compact.contains("Compiling crate"), "{compact}");
    }

    #[test]
    fn wide_layout_shows_the_live_activity_rail() {
        let mut ui = shell();
        ui.activity_rail = true;
        ui.push(Speaker::User, "inspect the repository");
        ui.apply_progress(Progress::ToolBegin {
            id: "shell-1".into(),
            title: "Ran shell: rg --files".into(),
        });
        let (running, _) = ui.compose(140, 32);
        let running_text = running
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(running_text.contains("ACTIVITY"), "{running_text}");
        assert!(running_text.contains("LIVE TOOLS"), "{running_text}");
        assert!(running_text.contains("◌ Ran shell"));
        // Footer / box bottom / info own the last three rows; the prompt
        // box sits just above them.
        let prompt_row = running.len() - 4;
        assert!(running[prompt_row].text.starts_with("│ >"));
        assert!(
            running[running.len() - 1].text.contains("Esc cancel"),
            "footer owns the last row"
        );
        assert!(
            running[running.len() - 2].text.starts_with("└"),
            "composer box closes above the footer"
        );
        assert!(
            running[running.len() - 3].text.contains("◈ BUILD"),
            "info line names the mode"
        );

        ui.apply_progress(Progress::ToolEnd {
            id: "shell-1".into(),
            ok: true,
            output: "exit=0\nfiles".into(),
            elapsed_ms: 42,
        });
        let finished = ui
            .compose(140, 32)
            .0
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(finished.contains("✓"));
    }

    #[test]
    fn activity_rail_is_responsive_and_toggleable() {
        let mut ui = shell();
        ui.activity_rail = true;
        ui.push(Speaker::User, "inspect code and update plan");
        ui.apply_progress(Progress::ToolBegin {
            id: "read".into(),
            title: "Read read_file src/main.rs".into(),
        });
        ui.apply_progress(Progress::ToolBegin {
            id: "mcp".into(),
            title: "Ran mcp__workspace__search query".into(),
        });
        let (wide, _) = ui.compose(117, 39);
        let text = wide
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("LIVE TOOLS"), "{text}");
        assert!(text.contains("read_file"), "{text}");
        assert!(text.contains("mcp__workspace__search"), "{text}");
        assert!(wide.iter().all(|row| row.text.chars().count() <= 117));

        let (narrow, _) = ui.compose(85, 39);
        let narrow_text = narrow
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!narrow_text.contains("LIVE TOOLS"));
        ui.activity_rail = false;
        let (disabled, _) = ui.compose(117, 39);
        assert!(!disabled.iter().any(|row| row.text.contains("LIVE TOOLS")));
    }

    #[test]
    fn plan_progress_updates_single_main_transcript_cell() {
        let mut ui = shell();
        ui.begin_turn();
        ui.apply_progress(Progress::Plan(vec![
            ("Discover relevant files".into(), false),
            ("Run tests".into(), false),
        ]));
        let row = ui.plan_row.expect("visible plan row");
        assert!(ui.entries[row].text.contains("0/2 complete"));
        let length = ui.entries.len();
        ui.apply_progress(Progress::Plan(vec![
            ("Discover relevant files".into(), true),
            ("Run tests".into(), false),
        ]));
        assert_eq!(ui.plan_row, Some(row));
        assert_eq!(
            ui.entries.len(),
            length,
            "update does not spam the transcript"
        );
        assert!(ui.entries[row].text.contains("1/2 complete"));
        assert!(ui.entries[row].text.contains("✓ Discover relevant files"));
    }

    #[test]
    fn normal_read_group_finishes_when_the_last_tool_completes() {
        let mut ui = shell();
        ui.verbosity = "compact".into();
        ui.apply_progress(Progress::ToolBegin {
            id: "grouped-read".into(),
            title: "Read read_file Cargo.toml".into(),
        });
        let group_row = ui.ro_ids["grouped-read"].0;
        assert!(ui.entries[group_row].text.contains("Exploring"));
        ui.apply_progress(Progress::ToolEnd {
            id: "grouped-read".into(),
            ok: true,
            output: "package name".into(),
            elapsed_ms: 13,
        });
        assert_eq!(ui.groups[&group_row].inflight, 0);
        assert!(ui.entries[group_row].text.contains("Explored"));
        assert!(!ui.entries[group_row].text.contains("Exploring"));
    }

    #[test]
    fn activity_rail_retains_grouped_read_search_results() {
        let mut ui = shell();
        ui.activity_rail = true;
        ui.verbosity = "compact".into();
        ui.push(Speaker::User, "search source");
        ui.apply_progress(Progress::ToolBegin {
            id: "browse".into(),
            title: "Searched grep security".into(),
        });
        ui.apply_progress(Progress::ToolEnd {
            id: "browse".into(),
            ok: true,
            output: "no matches".into(),
            elapsed_ms: 9,
        });
        assert!(
            !ui.tool_cells.contains_key("browse"),
            "read group is collapsed"
        );
        let (frame, _) = ui.compose(117, 39);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("RECENT RESULTS"), "{text}");
        assert!(text.contains("Search  security"), "{text}");
    }

    #[test]
    fn wide_activity_rail_shows_redacted_streaming_shell_output() {
        let mut ui = shell();
        ui.activity_rail = true;
        ui.push(Speaker::User, "run a local command");
        ui.apply_progress(Progress::ToolBegin {
            id: "shell-1".into(),
            title: "Ran shell: cargo test".into(),
        });
        ui.dash_procs.push(crate::tools::shell::ProcSnapshot {
            pid: 123,
            command: "cargo test".into(),
            cwd: "/tmp".into(),
            started: std::time::Instant::now(),
            running: true,
            exit_code: None,
            timed_out: false,
            cpu_pct: None,
            rss_kb: None,
            tail: "Compiling module\nRunning unit tests\n".into(),
        });
        ui.dash_data_at = Some(std::time::Instant::now());
        let (frame, _) = ui.compose(117, 39);
        let visible = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("LIVE SHELL OUTPUT"), "{visible}");
        assert!(visible.contains("Running unit tests"), "{visible}");
    }

    #[test]
    fn model_tool_elapsed_updates_while_it_is_running() {
        let mut ui = shell();
        ui.verbosity = "verbose".into();
        ui.apply_progress(Progress::ToolBegin {
            id: "scan".into(),
            title: "Searched grep security".into(),
        });
        let row = ui.tool_cells["scan"].row;
        ui.tick_thinking();
        assert!(
            ui.entries[row].text.contains("0ms"),
            "{}",
            ui.entries[row].text
        );
        ui.apply_progress(Progress::ToolEnd {
            id: "scan".into(),
            ok: true,
            output: "found test result".into(),
            elapsed_ms: 100,
        });
        assert!(ui.entries[row].text.contains("✓"));
        assert!(ui.entries[row].text.contains("100ms"));
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
    fn long_session_frame_render_skips_unchanged_transcript_entries() {
        let mut ui = shell();
        fill_transcript(&mut ui, 1200);
        ui.compose(110, 35);
        let rebuilt = ui.transcript_rebuilds;
        let rewrapped = ui.entries_rewrapped;
        for _ in 0..40 {
            ui.compose(110, 35);
        }
        assert_eq!(
            ui.transcript_rebuilds, rebuilt,
            "idle frames do not rebuild"
        );
        assert_eq!(ui.entries_rewrapped, rewrapped);
        let index = ui.entries.len() - 3;
        ui.set_entry_text(index, "edited recent row".into());
        ui.compose(110, 35);
        assert_eq!(
            ui.entries_rewrapped - rewrapped,
            (ui.entries.len() - index) as u64,
            "only the changed entry and its descendants need wrapping"
        );
    }

    #[test]
    fn clearing_transcript_invalidates_old_activity_references() {
        let mut ui = shell();
        ui.push(Speaker::System, "plan row");
        ui.plan_row = Some(ui.entries.len() - 1);
        ui.think_row = Some(ui.entries.len() - 1);
        ui.compose(110, 35);
        ui.clear_transcript();
        assert!(ui.plan_row.is_none());
        assert!(ui.think_row.is_none());
        // Completion event from an old task must be harmless after a reset.
        ui.set_entry_text(9000, "stale task output".into());
        ui.push(Speaker::User, "a new session");
        ui.compose(110, 35);
        assert!(ui
            .entries
            .iter()
            .all(|entry| entry.text != "stale task output"));
    }

    #[test]
    fn huge_single_line_tool_output_does_not_expand_into_unbounded_rows() {
        let giant = "λ".repeat(180_000);
        let block = expanded_block(&giant, Some(0));
        assert!(block.contains("line shortened for responsiveness"));
        assert!(block.contains("display capped"));
        assert!(block.len() < 10_000, "UI text is bounded: {}", block.len());
        let rendered =
            render_tool_rows(&format!(" ✓  tool result{block}"), 100, true, Color::Black);
        assert!(
            rendered.len() < 150,
            "would stall renderer: {} rows",
            rendered.len()
        );
        let normal = expanded_block("line one\nline two", Some(0));
        assert!(normal.contains("line one\nline two"));
        assert!(!normal.contains("line shortened"));
    }

    #[test]
    fn transcript_cache_only_rebuilds_when_the_wrapped_lines_change() {
        let mut ui = shell();
        fill_transcript(&mut ui, 40);
        ui.compose(80, 24);
        let first = ui.transcript_rebuilds;
        assert_eq!(first, 1);
        let rewrapped = ui.entries_rewrapped;
        assert_eq!(rewrapped, ui.entries.len() as u64);

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
        assert_eq!(
            ui.entries_rewrapped - rewrapped,
            1,
            "appending must wrap only the new entry"
        );

        // Growth that does not add entries (the streaming draft, spinner
        // ticks) must never re-wrap committed entries.
        let before_draft = ui.entries_rewrapped;
        ui.draft.push_str("streaming line\n");
        ui.compose(80, 24);
        ui.draft.push_str("second streaming line\n");
        ui.compose(80, 24);
        assert_eq!(
            ui.entries_rewrapped, before_draft,
            "draft growth must not invalidate the committed transcript"
        );

        ui.compose(60, 24);
        let after_width = ui.transcript_rebuilds;
        assert!(after_width > after_entry, "a different width re-wraps");

        ui.verbosity = "verbose".into();
        ui.compose(80, 24);
        assert!(
            ui.transcript_rebuilds > after_width,
            "verbosity changes the wrapped output"
        );
    }

    #[test]
    fn long_single_line_agent_answers_wrap_instead_of_clipping() {
        // Regression: non-verbose mode clipped every prose line to one
        // screen width, silently hiding the rest of a long paragraph or a
        // minified one-line answer. The full text must reach the transcript.
        let answer = "X".repeat(500);
        for verbose in [false, true] {
            let rows = render_agent_body(&answer, 40, verbose, Color::Black);
            let visible: String = rows
                .iter()
                .map(|(row, _)| row.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                visible.matches('X').count(),
                answer.len(),
                "verbose={verbose}: the wrapped body must contain every character"
            );
        }
    }

    #[test]
    fn incremental_draft_render_matches_full_render() {
        // The live draft uses an append-only cache: complete lines are
        // wrapped once and only the trailing partial line is re-wrapped.
        // Its output must be byte-identical to rendering the whole text,
        // at every possible split point, including fence boundaries.
        let text = "intro line\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\
                    a very long tail line that needs wrapping because it exceeds the width \
                    by quite a margin\nlast\n";
        let panel = Color::Black;
        // Split 0 is not a live-draft state: `compose` only prepares the
        // draft when it is non-empty. Every incremental state that can
        // actually render must match the full render.
        for split in 1..=text.len() {
            if !text.is_char_boundary(split) {
                continue;
            }
            let mut ui = shell();
            let (width, verbose) = (60usize, false);
            ui.draft.push_str(&text[..split]);
            let (_cached, tail) = ui.prepare_draft(width, verbose, panel);
            let mut incremental: Vec<String> = ui
                .draft_cache
                .rows
                .iter()
                .map(|(row, _)| row.clone())
                .collect();
            incremental.extend(tail.into_iter().map(|(row, _)| row));
            let full: Vec<String> = render_agent_body(&text[..split], width, verbose, panel)
                .into_iter()
                .map(|(row, _)| row)
                .collect();
            assert_eq!(
                incremental, full,
                "split {split} diverged from the full render"
            );
        }
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
            // Footer owns the last row, the box bottom the one above, the
            // info line above that (tiny terminals clip the footer text).
            assert!(
                frame[height - 1].text.contains("Esc cancel")
                    || frame[height - 1].text.contains("/ commands"),
                "the footer owns the last row: {:?}",
                frame[height - 1].text
            );
            assert!(
                frame[height - 2].text.starts_with("└"),
                "the composer box closes above the footer: {:?}",
                frame[height - 2].text
            );
            assert!(
                frame[0].text.starts_with(" WROSECODE"),
                "row 0 is the slim header: {:?}",
                frame[0].text
            );

            if height >= 24 {
                // Tall terminals: header + thin rule + breathing room, then
                // the transcript starts immediately in prompt bubbles.
                assert!(
                    frame[1].text.chars().any(|ch| ch == '─'),
                    "row 1 is the thin rule: {:?}",
                    frame[1].text
                );
                assert!(
                    frame[2].text.is_empty(),
                    "row 2 is breathing room: {:?}",
                    frame[2].text
                );
                assert!(
                    frame[3..9].iter().any(|row| row.text.starts_with("› ")),
                    "prompts open the transcript: {:?}",
                    frame[3].text
                );
                assert!(
                    frame.iter().any(|row| row.text.contains("Ask WROSECODE")),
                    "the composer labels its purpose"
                );
                // Empty draft: placeholder row inside the box.
                assert_eq!(
                    cursor,
                    (4, (height - 4) as u16),
                    "the caret sits in the prompt box"
                );
                assert!(
                    frame[height - 4].text.starts_with("│ >"),
                    "the prompt sits inside the box: {:?}",
                    frame[height - 4].text
                );
                assert!(
                    frame[height - 3].text.contains("◈ BUILD"),
                    "the info line names the mode: {:?}",
                    frame[height - 3].text
                );
            } else if height >= 7 {
                assert!(
                    frame[1].text.is_empty() || frame[1].text.starts_with("│ >"),
                    "row 1 is breathing room or composer: {:?}",
                    frame[1].text
                );
                assert_eq!(
                    cursor.1,
                    (height - 4) as u16,
                    "the caret sits in the prompt box: {cursor:?}"
                );
            } else {
                // A four-row terminal keeps header, prompt, info, footer.
                assert_eq!(cursor.1, 1, "the caret sits in the prompt: {cursor:?}");
                assert!(
                    frame[1].text.starts_with("│ >"),
                    "row 1 is the prompt: {:?}",
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
    fn medium_terminal_stays_single_column() {
        let mut ui = shell();
        fill_transcript(&mut ui, 4);
        let (frame, _) = ui.compose(120, 35);
        assert!(
            frame.iter().all(|row| row.text.chars().count() <= 120),
            "no overflow at 120 columns"
        );
        assert!(
            frame[frame.len() - 3].text.contains("◈ BUILD"),
            "info line survives at 120 columns"
        );
        assert!(
            frame.iter().any(|row| row.text.starts_with("› ")),
            "prompts render unboxed at 120 columns"
        );
    }

    /// Single-column contract across the required terminal sizes: exact row
    /// counts, full-width conversation with prompt bubbles, a boxed
    /// composer, footer ownership, and an opaque viewport.
    #[test]
    fn premium_layout_holds_across_required_sizes() {
        for (width, height) in [
            (160u16, 45u16),
            (140, 40),
            (120, 35),
            (100, 30),
            (80, 24),
            (60, 20),
        ] {
            let mut ui = shell();
            fill_transcript(&mut ui, 12);
            ui.mcp_live = vec![("gh".into(), 3)];
            let (frame, cursor) = ui.compose(width, height);
            let (w, h) = (width as usize, height as usize);
            assert_eq!(frame.len(), h, "exact rows at {w}x{h}");
            for (index, row) in frame.iter().enumerate() {
                assert!(
                    row.text.chars().count() <= w,
                    "row {index} overflows at {w}x{h}: {:?}",
                    row.text
                );
                // Opaque viewport: no transparent cells leak the desktop.
                assert_ne!(
                    row.bg,
                    Color::Reset,
                    "row {index} has explicit bg at {w}x{h}"
                );
            }
            assert!(
                frame[0].text.starts_with(" WROSECODE"),
                "header at {w}x{h}: {:?}",
                frame[0].text
            );
            assert!(
                frame[h - 1].text.contains("Esc cancel"),
                "footer owns last row at {w}x{h}: {:?}",
                frame[h - 1].text
            );
            assert!(
                frame[h - 2].text.starts_with("└"),
                "composer box closes at {w}x{h}: {:?}",
                frame[h - 2].text
            );
            assert!(
                frame[h - 3].text.contains("◈ BUILD"),
                "info line inside box at {w}x{h}"
            );
            assert!(
                frame.iter().any(|row| row.text.contains("◈ THINK")),
                "think status bar at {w}x{h}"
            );
            // Grok-clean prompts: a cyan `›` marker, no boxes in the
            // transcript (composer and snapshot keep theirs).
            assert!(
                frame.iter().any(|row| row.text.starts_with("› ")),
                "user marker at {w}x{h}"
            );
            assert!(
                frame
                    .iter()
                    .any(|row| row.text.starts_with("┌") && row.text.chars().count() == w),
                "full-width composer box at {w}x{h}"
            );
            assert!(cursor.1 < h as u16, "cursor inside frame at {w}x{h}");
        }
    }

    #[test]
    fn think_mode_shows_status_bar() {
        let mut ui = shell();
        assert!(ui.think_bar_visible(), "medium ships visible");
        let (frame, _) = ui.compose(100, 30);
        let bar = frame
            .iter()
            .find(|row| row.text.contains("THINK"))
            .expect("think status bar renders");
        assert!(
            bar.text.contains("medium") && bar.text.contains("5/20"),
            "bar names the live level: {:?}",
            bar.text
        );
        assert_ne!(bar.bg, Color::Reset, "bar sits on the dark viewport");
        // The bar owns exactly one row, directly above the composer box.
        let box_top = frame
            .iter()
            .rposition(|row| row.text.starts_with("┌"))
            .expect("composer box opens");
        let bar_row = frame.iter().position(|row| row.text.contains("THINK"));
        assert_eq!(bar_row, Some(box_top - 1), "bar hugs the box top");

        ui.think = ThinkLevel::Off;
        assert!(!ui.think_bar_visible(), "off hides the bar");
        let (frame, _) = ui.compose(100, 30);
        assert!(
            !frame.iter().any(|row| row.text.contains("THINK")),
            "no bar, no reserved row"
        );
    }

    #[test]
    fn git_refresh_backs_off_while_idle() {
        let now = std::time::Instant::now();
        let ten_seconds_ago = now - std::time::Duration::from_secs(10);
        assert!(
            git_stale(Some(ten_seconds_ago), now, true),
            "10s stale refreshes while busy"
        );
        assert!(
            !git_stale(Some(ten_seconds_ago), now, false),
            "10s stale stays cached while idle"
        );
        assert!(git_stale(None, now, false), "first sync always runs");
    }

    #[test]
    fn idle_sessions_want_no_animation() {
        let mut ui = shell();
        assert!(
            !ui.wants_animation(),
            "a quiet prompt costs no periodic frames"
        );
        ui.busy = true;
        assert!(ui.wants_animation(), "a running turn animates");
        ui.busy = false;
        ui.dashboard = true;
        assert!(ui.wants_animation(), "live panels animate");
    }

    #[test]
    fn chrome_fingerprint_moves_only_on_visible_change() {
        let mut ui = shell();
        let calm = ui.chrome_fingerprint();
        assert_eq!(
            ui.chrome_fingerprint(),
            calm,
            "re-reading idle state is stable"
        );
        ui.usage.input += 1;
        assert_ne!(ui.chrome_fingerprint(), calm, "token growth repaints");
        let moved = ui.chrome_fingerprint();
        ui.spinner = ui.spinner.wrapping_add(1);
        assert_eq!(
            ui.chrome_fingerprint(),
            moved,
            "spinner ticks alone never repaint"
        );
    }

    #[test]
    fn ctrl_b_snapshot_lists_real_state() {
        let mut ui = shell();
        fill_transcript(&mut ui, 2);
        ui.mcp_live = vec![("local-tools".into(), 7)];
        ui.lsp_enabled = true;
        ui.lsp_live = vec![("rust-analyzer".into(), true)];
        ui.lsp_dead = vec!["typescript-language-server --stdio".into()];
        ui.git_branch = "main".into();
        ui.git_files = 2;
        ui.git_added = 10;
        ui.git_removed = 3;
        let snapshot = ui.session_snapshot();
        for needle in [
            "SESSION",
            &ui.model,
            "local-tools",
            "rust-analyzer",
            "typescript-language-server",
            "unavailable",
            "main",
            "/stats",
        ] {
            assert!(
                snapshot.contains(needle),
                "snapshot shows {needle:?}:\n{snapshot}"
            );
        }
        // Nothing connected: no invented servers, GIT omitted outside repos.
        let mut ui = shell();
        ui.git_branch.clear();
        let snapshot = ui.session_snapshot();
        assert!(snapshot.contains("no servers connected"), "{snapshot}");
        assert!(
            !snapshot.contains("GIT") && !snapshot.contains("git "),
            "{snapshot}"
        );
        assert!(
            snapshot.contains("SESSION") && snapshot.contains("details"),
            "snapshot reads as one block: {snapshot}"
        );
    }

    #[test]
    fn snapshot_shows_only_real_integrations() {
        let mut ui = shell();
        // LSP with nothing live reports exactly that — never fake servers.
        ui.lsp_enabled = true;
        assert!(
            ui.session_snapshot().contains("no live servers"),
            "{}",
            ui.session_snapshot()
        );
        ui.lsp_enabled = false;
        assert!(
            !ui.session_snapshot().contains("lsp"),
            "disabled LSP stays hidden"
        );
        // CTF section only in CTF mode.
        assert!(!ui.session_snapshot().contains("ctf"));
        ui.mode = "CTF".into();
        assert!(ui.session_snapshot().contains("ctf"));
    }

    #[test]
    fn unicode_and_long_prompts_wrap_without_overlap() {
        let mut ui = shell();
        ui.push(Speaker::User, format!("héllo {} 🎉", "wörld ".repeat(40)));
        ui.set_input("emoji 🎉 test ".repeat(20));
        ui.cursor = ui.input.len();
        for (w, h) in [(160u16, 45u16), (100, 30), (80, 24)] {
            let (frame, cursor) = ui.compose(w, h);
            assert_eq!(frame.len(), h as usize);
            for row in &frame {
                assert!(row.text.chars().count() <= w as usize);
                assert_ne!(row.bg, Color::Reset);
            }
            assert!(cursor.1 < h);
        }
    }

    #[test]
    fn slash_palette_floats_above_composer_without_overlap() {
        let mut ui = shell();
        fill_transcript(&mut ui, 8);
        ui.set_input("/mod".into());
        ui.cursor = ui.input.len();
        ui.palette = true;
        let (frame, _) = ui.compose(120, 35);
        let palette_row = frame
            .iter()
            .position(|row| row.text.contains("/models"))
            .expect("palette lists /models");
        let footer = frame.len() - 1;
        let prompt = frame.len() - 4;
        let box_top = frame
            .iter()
            .rposition(|row| row.text.starts_with("┌"))
            .expect("composer box opens above the prompt");
        assert!(palette_row < box_top, "palette floats above composer");
        assert!(frame[footer].text.contains("Esc cancel"), "footer intact");
        assert!(frame[prompt].text.starts_with("│ >"), "composer intact");
    }

    #[test]
    fn user_marker_glows_while_banner_stays_quiet() {
        let mut ui = shell();
        ui.push(Speaker::User, String::from("list dir in desktop"));
        let (frame, _) = ui.compose(100, 32);
        // Welcome banner stays quiet muted even though it mentions thinking.
        let banner = frame
            .iter()
            .find(|row| row.text.contains("WROSECODE v"))
            .expect("banner renders");
        assert_eq!(banner.fg, Color::Grey, "banner muted: {:?}", banner.fg);
        // User marker carries the cyan accent via per-character colours.
        let prompt = frame
            .iter()
            .find(|row| row.text.starts_with("› "))
            .expect("prompt renders");
        assert_eq!(
            prompt.colors,
            Some(vec![theme(0).accent, theme(0).accent]),
            "marker accent"
        );
    }

    #[test]
    fn assistant_fences_render_labeled_containers() {
        let mut ui = shell();
        ui.push(
            Speaker::Agent,
            "try this:\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\ndone.",
        );
        let (frame, _) = ui.compose(100, 30);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("┌─ rust"), "labeled top border:\n{text}");
        assert!(text.contains("fn main() {"), "code kept:\n{text}");
        assert!(
            text.contains("│     println!(\"hi\");"),
            "indentation preserved:\n{text}"
        );
        assert!(!text.contains("```"), "no raw fences leak:\n{text}");
        // The whole result — prose and code — sits in one charcoal box.
        let code_row = frame
            .iter()
            .find(|row| row.text.contains("fn main()"))
            .expect("code row renders");
        assert_ne!(code_row.bg, Color::Reset);
        let prose = frame
            .iter()
            .find(|row| row.text.contains("try this:"))
            .expect("prose renders");
        assert_eq!(prose.bg, code_row.bg, "one container for the whole result");
        assert!(
            text.contains("┌─ ◈ WROSECODE"),
            "result box carries the title:\n{text}"
        );
    }

    #[test]
    fn fence_edge_cases_stay_graceful() {
        // Unclosed fence renders to the end instead of leaking backticks.
        let rows = entry_rows(
            &Entry {
                speaker: Speaker::Agent,
                text: "half:\n```python\nx = 1".into(),
            },
            60,
            false,
            Color::Black,
        );
        let text: Vec<&str> = rows.iter().map(|(line, _)| line.as_str()).collect();
        assert!(text.iter().any(|line| line.contains("┌─ python")));
        assert!(text.iter().any(|line| line.contains("x = 1")));
        assert!(!text.iter().any(|line| line.contains("```")));
        // No language tag degrades to a `code` label, not an empty one.
        let rows = entry_rows(
            &Entry {
                speaker: Speaker::Agent,
                text: "```\nplain\n```".into(),
            },
            60,
            false,
            Color::Black,
        );
        assert!(rows.iter().any(|(line, _)| line.contains("┌─ code")));
        // A very long code line wraps inside the walls instead of clipping.
        let long = "y = \"".to_string() + &"z".repeat(200) + "\"";
        let rows = entry_rows(
            &Entry {
                speaker: Speaker::Agent,
                text: format!("```\n{long}\n```"),
            },
            60,
            false,
            Color::Black,
        );
        for (line, _) in &rows {
            assert!(line.chars().count() <= 60, "row fits: {line:?}");
        }
        assert!(
            rows.iter()
                .filter(|(line, _)| line.starts_with("│"))
                .count()
                > 3,
            "long line wraps to several boxed rows"
        );
        // Plain prose gets the result box but no code container inside it.
        let rows = entry_rows(
            &Entry {
                speaker: Speaker::Agent,
                text: "just words".into(),
            },
            60,
            false,
            Color::Black,
        );
        assert_eq!(
            rows.iter()
                .filter(|(line, _)| line.starts_with("┌"))
                .count(),
            1,
            "only the result box, no code container"
        );
    }

    #[test]
    fn expanded_tool_output_renders_a_bordered_box() {
        let mut ui = shell();
        ui.push(Speaker::User, "run it");
        let row = ui.entries.len();
        ui.tool_cells.insert(
            "t1".into(),
            ToolCell {
                title: "shell ls".into(),
                running: false,
                ok: true,
                output: "exit=0\na\nb".into(),
                elapsed_ms: 42,
                exit_code: Some(0),
                row,
            },
        );
        ui.expanded.insert(row);
        let text = ui.cell_text("t1", &ui.tool_cells["t1"].clone());
        ui.push(Speaker::Tool, text);
        let (frame, _) = ui.compose(100, 30);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("┌─ output · exit=0"),
            "box header names status and exit:\n{text}"
        );
        assert!(text.contains("│ a"), "output boxed:\n{text}");
        assert!(
            !text.contains("▓WROSE-OUT▓") && !text.contains("▓WROSE-END▓"),
            "sentinels never reach the screen:\n{text}"
        );
        let body = frame
            .iter()
            .find(|row| row.text.contains("│ a"))
            .expect("output row renders");
        let title = frame
            .iter()
            .find(|row| row.text.contains("shell ls"))
            .expect("title renders");
        assert_ne!(body.bg, title.bg, "output box contrasts with title");
    }

    #[test]
    fn composer_box_glows_cyan_only_while_editing() {
        let mut ui = shell();
        fill_transcript(&mut ui, 2);
        let (frame, _) = ui.compose(100, 30);
        let bottom = frame
            .iter()
            .rfind(|row| row.text.starts_with("└"))
            .expect("composer box closes");
        assert_eq!(bottom.fg, theme(0).accent, "focused chrome glows cyan");
        let prompt = frame
            .iter()
            .find(|row| row.text.starts_with("│ >"))
            .expect("prompt row renders");
        let walls: Vec<Color> = prompt.colors.clone().expect("walls painted");
        assert_eq!(walls[0], theme(0).accent, "left wall cyan");
        assert_eq!(*walls.last().expect("right wall"), theme(0).accent);

        ui.navigation_mode = true;
        let (frame, _) = ui.compose(100, 30);
        let bottom = frame
            .iter()
            .rfind(|row| row.text.starts_with("└"))
            .expect("box still closes");
        assert_eq!(bottom.fg, theme(0).muted, "blurred chrome stays gray");
    }

    #[test]
    fn yolo_permission_is_visible_in_splash_and_header() {
        let mut ui = shell();
        ui.permission = "yolo".into();
        let (splash, _) = ui.compose(90, 25);
        assert!(splash.iter().any(|row| row.text.contains("YOLO")));
        ui.push(Speaker::System, "working");
        let (frame, _) = ui.compose(90, 25);
        assert!(frame.iter().any(|row| row.text.contains("[YOLO]")));
    }

    #[test]
    fn boxes_hold_at_every_required_size() {
        let code = "here:\n```bash\nls -la /home/kali/Desktop\n```";
        for (w, h) in [(160u16, 45u16), (120, 35), (80, 24)] {
            let mut ui = shell();
            ui.push(Speaker::Agent, String::from(code));
            let (frame, cursor) = ui.compose(w, h);
            let (w, h) = (w as usize, h as usize);
            assert_eq!(frame.len(), h);
            for (index, row) in frame.iter().enumerate() {
                assert!(
                    row.text.chars().count() <= w,
                    "row {index} overflows at {w}x{h}"
                );
                assert_ne!(row.bg, Color::Reset, "opaque at {w}x{h}");
            }
            assert!(
                frame.iter().any(|row| row.text.contains("┌─ bash")),
                "code container at {w}x{h}"
            );
            assert!(
                frame[h - 1].text.contains("Esc cancel"),
                "footer at {w}x{h}"
            );
            assert!(cursor.1 < h as u16, "cursor visible at {w}x{h}");
        }
    }

    #[test]
    fn flags_panel_pins_last_five_full_flags() {
        let mut ui = shell();
        ui.mode = "CTF".into();
        fill_transcript(&mut ui, 2);
        for i in 0..7 {
            ui.record_flag(
                format!("HTB{{flag_{i}_y{}}}", "x".repeat(30)),
                format!("tool:{i}"),
            );
        }
        assert_eq!(ui.flag_log.len(), 7);
        let (frame, _) = ui.compose(100, 30);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Newest five shown in full — wrapped rows rejoin to the whole flag.
        let rejoined: String = text
            .lines()
            .filter(|line| line.starts_with("│"))
            .map(|line| {
                line.strip_prefix("│ ")
                    .or_else(|| line.strip_prefix("│"))
                    .unwrap_or(line)
            })
            .collect::<Vec<_>>()
            .join("");
        for i in 2..7 {
            assert!(
                rejoined.contains(&format!("flag_{i}_")),
                "flag {i} shown whole:\n{text}"
            );
        }
        assert!(
            !rejoined.contains("flag_0_") && !rejoined.contains("flag_1_"),
            "older flags fall off the pinned window:\n{text}"
        );
        assert!(
            text.contains("+2 more · /flags"),
            "exact overflow count:\n{text}"
        );
        // Short screens pin two with an exact count instead.
        let (frame, _) = ui.compose(100, 12);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("FLAGS FOUND") && text.contains("+6 more · /flags"),
            "compact panel:\n{text}"
        );
    }

    #[test]
    fn git_omitted_from_snapshot_outside_repo() {
        let mut ui = shell();
        ui.git_branch.clear();
        assert!(
            !ui.session_snapshot().contains("git"),
            "no GIT outside a repo"
        );
        ui.git_branch = "main".into();
        ui.git_files = 2;
        ui.git_added = 10;
        ui.git_removed = 3;
        let snapshot = ui.session_snapshot();
        assert!(
            snapshot.contains("main"),
            "branch shown in a repo: {snapshot}"
        );
        assert!(snapshot.contains("+10"), "{snapshot}");
    }

    #[test]
    fn provider_retry_reset_discards_partial_stream_instead_of_committing_it() {
        let mut ui = shell();
        ui.apply_progress(Progress::TextDelta("partial failed answer".into()));
        assert_eq!(ui.draft, "partial failed answer");
        ui.apply_progress(Progress::ResetText);
        assert!(ui.draft.is_empty());
        assert!(
            !ui.entries
                .iter()
                .any(|entry| entry.text.contains("partial failed answer")),
            "retry leaked a failed partial response into the transcript"
        );
        ui.apply_progress(Progress::Heartbeat);
        assert!(ui.status == "Model active" || ui.status == "Streaming");
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
            frame
                .iter()
                .any(|row| row.text.contains("↓ 0 new lines  (End to jump)")),
            "the composer rule advertises the jump hint while detached"
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
    fn queued_messages_appear_in_the_composer_rule() {
        let mut ui = shell();
        fill_transcript(&mut ui, 10);
        ui.busy = true;
        ui.pending.push("second request".into());
        let (frame, _) = ui.compose(80, 24);
        let rule = frame
            .iter()
            .map(|row| &row.text)
            .find(|text| text.contains("1 queued"))
            .expect("rule shows the queued count");
        assert!(rule.contains("Working"), "rule: {rule}");
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

        // The chip owns a row between the composer rule and the prompt.
        let (frame, _) = ui.compose(100, 30);
        assert!(
            frame
                .iter()
                .any(|row| row.text.contains("[Pasted 300 lines]")),
            "chip row present"
        );
        assert!(
            frame[frame.len() - 1].text.contains("Esc cancel"),
            "footer survives the chip"
        );
        assert!(
            frame[frame.len() - 3].text.contains("◈ BUILD"),
            "info line survives the chip"
        );
    }

    #[test]
    fn multiline_input_grows_then_scrolls() {
        let mut ui = shell();
        ui.set_input("one".into());
        assert_eq!(wrap_input(&ui.input, 95).len(), 1);

        ui.set_input("one\ntwo".into());
        assert_eq!(
            wrap_input(&ui.input, 95).len(),
            2,
            "the draft grows by line"
        );
        let (frame, (_, y)) = ui.compose(100, 30);
        assert_eq!(
            y, 26,
            "the caret sits on the draft's last row inside the box"
        );
        assert!(
            frame[25].text.starts_with("│ > one"),
            "{:?}",
            frame[25].text
        );
        assert!(frame[26].text.contains("two"), "{:?}", frame[26].text);
        assert!(
            frame[27].text.contains("◈ BUILD"),
            "info line inside the box: {:?}",
            frame[27].text
        );
        assert!(
            frame[28].text.starts_with("└"),
            "box closes above the footer: {:?}",
            frame[28].text
        );
        assert!(
            frame[29].text.contains("Esc cancel"),
            "footer owns the last row: {:?}",
            frame[29].text
        );

        // Ten lines remain in the source while the eight-row viewport follows
        // the caret and makes the earlier rows reachable by moving it up.
        let long = (0..10)
            .map(|index| format!("line{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_input(long);
        ui.cursor = ui.input.len();
        assert_eq!(wrap_input(&ui.input, 95).len(), 10, "all rows are counted");
        let (frame, (_, y)) = ui.compose(100, 30);
        assert_eq!(y, 26, "the caret stays on the bottom row of the box");
        assert!(
            frame[26].text.contains("line9"),
            "the window follows the caret: {:?}",
            frame[26].text
        );
        assert!(
            frame[19].text.contains("line2"),
            "the viewport shows the latest rows: {:?}",
            frame[19].text
        );
        assert!(
            frame[19].text.starts_with("│   "),
            "a scrolled window marks its first row: {:?}",
            frame[19].text
        );
        assert_eq!(
            ui.input.split('\n').count(),
            10,
            "rendering preserves all input"
        );

        // The caret walks up: the window follows it back toward the top.
        ui.cursor = 0;
        let (frame, (_, y)) = ui.compose(100, 30);
        assert_eq!(y, 19, "caret on the first visible row");
        assert!(
            frame[19].text.starts_with("│ > line0"),
            "{:?}",
            frame[19].text
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
    fn mouse_capture_defaults_off_for_native_selection() {
        // `auto` keeps the terminal's own drag-to-select: mouse reporting
        // stays off unless explicitly enabled.
        let profile = TerminalProfile::decide(true, None, &env("xterm-256color"));
        assert!(
            !profile.mouse,
            "auto must not capture the mouse: {profile:?}"
        );
        let forced = TerminalProfile::decide(
            true,
            None,
            &TerminalEnv {
                term: "xterm-256color".into(),
                force_mouse: true,
                ..TerminalEnv::default()
            },
        );
        assert!(forced.mouse, "WROSECODE_MOUSE=1 opts back in");
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
    fn normal_mode_suppresses_ctf_flag_banner_and_panel() {
        let mut ui = shell();
        ui.record_flag("flag{old_session}".into(), "saved".into());
        ui.apply_progress(Progress::FlagFound {
            flag: "flag{should_be_hidden}".into(),
            source: "tool".into(),
        });
        assert_eq!(ui.flag_log.len(), 1);
        assert_eq!(ui.flags_found, 1);
        assert!(!ui
            .entries
            .iter()
            .any(|entry| entry.text.contains("FLAG DETECTED")));
        let (frame, _) = ui.compose(80, 24);
        assert!(!frame.iter().any(|row| row.text.contains("FLAGS FOUND")));
    }

    #[test]
    fn flags_are_pinned_deduplicated_and_do_not_touch_input() {
        let mut ui = shell();
        ui.mode = "CTF".into();
        ui.set_input("draft text".into());
        let cursor = ui.cursor;
        ui.apply_progress(Progress::FlagFound {
            flag: "flag{one}".into(),
            source: "tool:stdout".into(),
        });
        ui.apply_progress(Progress::FlagFound {
            flag: "flag{one}".into(),
            source: "model".into(),
        });
        ui.apply_progress(Progress::FlagFound {
            flag: "HTB{two}".into(),
            source: "tool:stderr".into(),
        });
        assert_eq!(ui.flags_found, 2);
        assert_eq!(ui.flag_log.len(), 2);
        assert_eq!(ui.input, "draft text");
        assert_eq!(ui.cursor, cursor);
        let (frame, _) = ui.compose(80, 24);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("FLAGS FOUND"));
        assert!(text.contains("flag{one}"));
        assert!(text.contains("HTB{two}"));
        assert!(text.contains("│") && text.contains("╰"));
    }

    #[test]
    fn split_stream_flags_from_assistant_and_tool_output_are_pinned() {
        let mut ui = shell();
        ui.mode = "CTF".into();
        ui.set_input("keep me".into());
        let cursor = ui.cursor;
        ui.apply_progress(Progress::TextDelta("answer flag{split_".into()));
        ui.apply_progress(Progress::TextDelta("assistant}".into()));
        ui.apply_progress(Progress::ToolEnd {
            id: "stderr-1".into(),
            ok: true,
            output: "captured HTB{stderr_case}".into(),
            elapsed_ms: 1,
        });
        assert!(ui
            .flag_log
            .iter()
            .any(|(flag, _)| flag == "flag{split_assistant}"));
        assert!(ui
            .flag_log
            .iter()
            .any(|(flag, _)| flag == "HTB{stderr_case}"));
        assert_eq!(ui.input, "keep me");
        assert_eq!(ui.cursor, cursor);
    }

    #[test]
    fn pinned_flags_are_bounded_on_narrow_resize_and_survive_clear() {
        let mut ui = shell();
        ui.mode = "CTF".into();
        for index in 0..40 {
            ui.apply_progress(Progress::FlagFound {
                flag: format!("flag{{{index}}}"),
                source: "stream".into(),
            });
        }
        let (frame, _) = ui.compose(40, 10);
        assert!(frame.iter().all(|row| row.text.chars().count() <= 40));
        assert!(frame.iter().any(|row| row.text.contains("more")));
        ui.clear_transcript();
        assert_eq!(ui.flag_log.len(), 40);
        assert_eq!(ui.flags_found, 40);
    }

    #[test]
    fn rate_limit_errors_classify_separately() {
        assert_eq!(classify_error("HTTP 429 Too Many Requests"), "rate_limit");
        assert_eq!(classify_error("rate limit exceeded"), "rate_limit");
        assert_eq!(classify_error("connection refused"), "network");
    }

    #[test]
    fn logo_is_clear_block_wordmark() {
        assert_eq!(LOGO.len(), 5);
        assert!(LOGO[0].contains("██╗"));
        assert!(LOGO[4].contains("███████"));
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
            "ACTIVITY / PROCESSES",
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
    fn dashboard_shows_every_agent_tool_not_only_shell_processes() {
        let mut ui = shell();
        ui.apply_progress(Progress::ToolBegin {
            id: "read-1".into(),
            title: "Read read_file src/main.rs".into(),
        });
        ui.apply_progress(Progress::ToolBegin {
            id: "delegate-2".into(),
            title: "Delegated delegate_task review changes".into(),
        });
        let stats = ui.dash_stats();
        assert_eq!(stats.activities.len(), 2);
        assert!(stats.activities.iter().all(|activity| activity.running));
        let panel = dashboard_panels(&stats, 90).remove(0).1.join("\n");
        assert!(panel.contains("Read  src/main.rs"), "{panel}");
        assert!(panel.contains("Agent  review changes"), "{panel}");

        ui.apply_progress(Progress::ToolEnd {
            id: "read-1".into(),
            ok: true,
            output: "file contents".into(),
            elapsed_ms: 18,
        });
        let stats = ui.dash_stats();
        assert!(!stats.activities[1].running);
        assert_eq!(stats.activities[1].ok, Some(true));
        assert_eq!(stats.activities[1].elapsed_ms, 18);

        ui.dashboard = true;
        ui.dash_focus = 0;
        ui.dash_sel = 0;
        ui.dash_open();
        let (title, body) = ui.dash_detail.as_ref().expect("operation detail");
        assert_eq!(title, "AGENT ACTIVITY");
        assert!(body.contains("delegate_task"), "{body}");
        ui.dash_signal(true);
        assert_eq!(ui.status, "Agent activity is not an OS process");
    }

    #[test]
    fn verbose_mode_preserves_individual_read_tool_cells() {
        let mut ui = shell();
        ui.verbosity = "verbose".into();
        ui.apply_progress(Progress::ToolBegin {
            id: "read-verbose".into(),
            title: "Read read_file src/main.rs".into(),
        });
        assert!(ui.ro_ids.is_empty(), "verbose reads should not be grouped");
        assert!(ui.tool_rows.contains_key("read-verbose"));
        ui.apply_progress(Progress::ToolEnd {
            id: "read-verbose".into(),
            ok: true,
            output: "visible read result".into(),
            elapsed_ms: 5,
        });
        let row = *ui.tool_rows.get("read-verbose").expect("tool row");
        assert!(ui.entries[row].text.contains("visible read result"));
    }

    #[test]
    fn dashboard_scrolls_to_selected_tool_in_bounded_process_panel() {
        let stats = DashStats {
            activities: (0..30)
                .map(|index| ActivityStat {
                    id: format!("a{index}"),
                    title: format!("Read read_file unique-{index}"),
                    running: true,
                    ..ActivityStat::default()
                })
                .collect(),
            ..DashStats::default()
        };
        let (frame, _) = compose_grid(&stats, 100, 24, dashboard_theme(true), 0, 25, None);
        let text = frame
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("unique-25"),
            "selected row must stay visible: {text}"
        );
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
        ui.verbosity = "verbose".into();
        ui.push(Speaker::Agent, "the flag is flag{ok}");
        ui.task_begin();
        ui.verified = true;
        let steps = vec![
            "read_file chal.txt".to_string(),
            "decode flag{ok}".to_string(),
        ];
        ui.push_result_block(&steps);
        let text = &ui.entries.last().expect("a result block was pushed").text;
        assert!(text.contains("── RESULT ─ ✔ verified"), "{text}");
        assert!(text.contains("answer: the flag is flag{ok}"), "{text}");
        assert!(text.contains("proof: none"), "{text}");
        assert!(text.contains("steps "), "{text}");
        assert!(text.contains("1. read_file chal.txt"), "{text}");
        assert!(text.contains("2. decode flag{ok}"), "{text}");
        // The acceptance shape: time split, steps, tokens, cache hit %,
        // cost, and the saved-cache readout all live in one block.
        assert!(
            text.contains("time ") && text.contains("(model ") && text.contains("· wait "),
            "{text}"
        );
        assert!(text.contains("% cache"), "{text}");
        assert!(text.contains("cost "), "{text}");
        assert!(text.contains("saved ~"), "{text}");

        let mut ui = shell();
        ui.verbosity = "verbose".into();
        ui.task_begin();
        ui.record_error("network");
        ui.push_result_block(&[]);
        let text = &ui.entries.last().expect("a result block was pushed").text;
        assert!(text.contains("── RESULT ─ ✗ failed"), "{text}");

        let mut ui = shell();
        ui.verbosity = "verbose".into();
        ui.task_begin();
        ui.push_result_block(&[]);
        let text = &ui.entries.last().expect("a result block was pushed").text;
        assert!(
            text.starts_with("── RESULT ─────────────────────────"),
            "{text}"
        );
        assert!(!text.contains("unverified"), "{text}");
    }

    #[test]
    fn streamed_text_and_process_snapshots_are_redacted_for_display() {
        let mut ui = shell();
        ui.apply_progress(Progress::TextDelta("api_key=display-test-secret".into()));
        assert_eq!(ui.draft, "api_key=***", "draft was {:?}", ui.draft);

        let snapshot = crate::tools::shell::ProcSnapshot {
            pid: 4242,
            command: "curl --token display-test-bearer-secret https://example.test".into(),
            cwd: std::env::temp_dir().display().to_string(),
            started: std::time::Instant::now(),
            running: true,
            exit_code: None,
            timed_out: false,
            cpu_pct: None,
            rss_kb: None,
            tail: "password=display-test-password".into(),
        };
        let stats = ProcStat::from(&snapshot);
        assert!(
            !stats.command.contains("display-test-bearer-secret"),
            "{}",
            stats.command
        );
        assert!(stats.command.contains("--token ***"), "{}", stats.command);
        assert!(
            !stats.tail.contains("display-test-password"),
            "{}",
            stats.tail
        );
    }

    /// The dashboard's Ctrl+K path: a selected tracked process is really
    /// signalled, and the status line reports it. `sleep` is spawned as a
    /// plain child (not a group leader), so this also walks the plain-pid
    /// fallback in `shell::signal`.
    #[test]
    fn dashboard_kill_signals_the_selected_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep must be on PATH");
        let pid = child.id();
        let mut ui = shell();
        ui.dashboard = true;
        ui.dash_focus = 0;
        ui.dash_sel = 0;
        ui.dash_procs = vec![crate::tools::shell::ProcSnapshot {
            pid,
            command: "sleep 30".into(),
            cwd: std::env::temp_dir().display().to_string(),
            started: std::time::Instant::now(),
            running: true,
            exit_code: None,
            timed_out: false,
            cpu_pct: None,
            rss_kb: None,
            tail: String::new(),
        }];
        ui.dash_signal(true);
        assert_eq!(
            ui.status,
            format!("SIGTERM sent to pid {pid}"),
            "the kill is reported"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the selected process survived the dashboard kill"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // With nothing selected the dashboard reports instead of guessing.
        let mut ui = shell();
        ui.dashboard = true;
        ui.dash_procs = Vec::new();
        ui.dash_signal(true);
        assert_eq!(ui.status, "No process selected");
    }

    /// The rendering budget from the speed pass: building a frame on top of
    /// a 10k-line transcript — cache invalidated every round, so the timed
    /// work is the wrap rebuild plus the viewport compose — stays inside its
    /// budget in release runs (dev runs only print the number).
    #[test]
    fn composing_a_10k_line_transcript_stays_within_budget() {
        let mut ui = shell();
        fill_transcript(&mut ui, 10_000);
        crate::benchmarks::budget("compose 10k-line transcript", 100, 5, || {
            ui.transcript_key = None;
            let (frame, _) = ui.compose(120, 40);
            assert_eq!(frame.len(), 40, "one row per screen line");
        });
    }

    /// Resize redraws correctly: a size change repaints from a clean slate
    /// (full clear, tail rows wiped), while a byte-identical frame at the
    /// same size emits nothing at all — no flicker, no jumping.
    #[test]
    fn a_resize_repaints_every_row_and_a_stable_frame_emits_nothing() {
        let mut renderer = Renderer::new();
        let rows: Vec<Row> = (0..8)
            .map(|index| Row::new(format!("row {index}"), Color::Reset))
            .collect();

        let mut first = Vec::new();
        renderer
            .draw_to(&mut first, &rows, (0, 0), (20, 8))
            .expect("first frame");
        assert!(
            String::from_utf8_lossy(&first).contains("\u{1b}[2J"),
            "the first frame clears the screen"
        );

        let mut stable = Vec::new();
        renderer
            .draw_to(&mut stable, &rows, (0, 0), (20, 8))
            .expect("same frame");
        assert!(
            stable.len() < 64,
            "an unchanged frame at an unchanged size emits only the cursor tail: {stable:?}"
        );
        assert!(
            !String::from_utf8_lossy(&stable).contains("\u{1b}[K"),
            "no row of a stable frame is rewritten"
        );

        let mut resized = Vec::new();
        renderer
            .draw_to(&mut resized, &rows[..5], (0, 0), (30, 5))
            .expect("resized frame");
        let bytes = String::from_utf8_lossy(&resized);
        assert!(
            bytes.contains("\u{1b}[2J"),
            "a size change starts from a clean slate: {bytes:?}"
        );
        assert!(
            bytes.matches("\u{1b}[2K").count() >= 5,
            "every row of the new frame is erased before it is painted: {bytes:?}"
        );
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

        // Routine successful cells remain compact in normal mode.
        let cell = ui.tool_cells["t1"].clone();
        let text = ui.cell_text("t1", &cell);
        assert!(text.contains("✓  shell ls   1.5s"), "{text}");
        assert!(text.contains("10 lines · line 10"), "{text}");
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
        assert!(text.contains("×  shell false"), "{text}");
        assert!(text.contains("▓WROSE-OUT▓ exit=1"), "{text}");
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
        assert!(text.contains("◌  shell sleep"), "{text}");
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
        assert!(tree.contains("└─ ◌ grep b  …"), "{tree}");

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
        assert_eq!(group_text(&live), "◌ ▸ Exploring  1 file · 0 searches");
    }
}
