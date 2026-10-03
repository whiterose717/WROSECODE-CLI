use crate::agent::Agent;
use crate::harness::Harness;
use crate::provider::Progress;
use crate::provider::Usage;
use crate::tools::PermissionRequest;
use crate::{
    commands, project, provider,
    session::Session,
    settings::{McpServerDef, ProviderProfile, Settings},
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
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, PartialEq)]
struct Row {
    text: String,
    fg: Color,
    bg: Color,
}

impl Row {
    fn new(text: impl Into<String>, fg: Color) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: Color::Reset,
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
                Clear(ClearType::CurrentLine),
                Print(&row.text)
            )?;
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
        }
    }
}

impl TerminalProfile {
    /// `configured_alternate` comes from `[ui] alternate_screen`; `mouse_capture`
    /// is `"auto"`, `"on"` or `"off"`.
    pub fn detect(configured_alternate: bool, mouse_capture: Option<&str>) -> Self {
        Self::decide(
            configured_alternate,
            mouse_capture,
            &TerminalEnv::from_process(),
        )
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
        Self {
            alternate,
            mouse,
            full_redraw,
        }
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(profile: TerminalProfile) -> io::Result<Self> {
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
        Ok(Self)
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
}

/// State for cycling through Tab completions with the same token.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletionState {
    token_start: usize,
    matches: Vec<String>,
    index: usize,
}

const LOGO: [&str; 3] = [
    "╦ ╦╔╗ ╭─╮╔═╗╔═╗╭─╮╭─╮╔═╗╔═╗",
    "║ ║╠╩╗│ │╠═╝╠═╗│  │ │║ ║╠═╗",
    "╚═╝╚═╝╰─╯╚═╝╚═╝╰─╯╰─╯╚═╝╚═╝",
];

impl Ui {
    fn new(agent: &Agent, session_id: String) -> Self {
        Self {
            renderer: Renderer::new(),
            entries: vec![Entry {
                speaker: Speaker::System,
                text: format!(
                    "WROSECODE v{} · session {}\n\
                     Type a request · /help commands · Ctrl+P palette\n\
                     Tab build/plan · [ ] thinking · Esc then j/k to scroll",
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
            root: agent.config.root.display().to_string(),
            provider: agent.config.provider.clone(),
            model: agent.config.model.clone(),
            harness: agent.harness.name().into(),
            mode: "BUILD".into(),
            permission: format!("{:?}", agent.config.permission).to_ascii_lowercase(),
            palette: false,
            selected: 0,
            picker: None,
            picker_all: None,
            picker_query: String::new(),
            mask_input: false,
            tool_rows: HashMap::new(),
            verbosity: agent.config.verbosity.clone(),
            tool_calls: 0,
            tool_failures: 0,
            session_started: std::time::Instant::now(),
            session_id,
            category: agent.ctf.category.clone(),
            thinking_level: agent.thinking_level,
            usage: Usage::default(),
            flags_found: 0,
            navigation_mode: false,
            theme: theme_index(&agent.config.ui_theme),
            pane_percent: 68,
            dragging_separator: false,
            tool_timeline: Vec::new(),
            metrics: crate::metrics::MetricsSnapshot::default(),
            timeout_seconds: agent.config.shell_timeout_seconds,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_position: 0,
            budget_usd: agent.config.budget_usd,
            last_flag: String::new(),
            turn_tokens: Vec::new(),
            turn_usage: Usage::default(),
            turn_started: None,
            last_turn_ms: 0,
            error_count: 0,
            last_error_kind: "",
        }
    }

    fn begin_turn(&mut self) {
        self.turn_usage = Usage::default();
        self.turn_started = Some(std::time::Instant::now());
    }

    fn end_turn(&mut self) {
        if let Some(started) = self.turn_started.take() {
            self.last_turn_ms = started.elapsed().as_millis() as u64;
            self.turn_tokens.push(self.turn_usage.output.max(1));
            if self.turn_tokens.len() > 64 {
                self.turn_tokens.remove(0);
            }
        }
        self.turn_usage = Usage::default();
    }

    fn record_error(&mut self, kind: &'static str) {
        self.error_count += 1;
        self.last_error_kind = kind;
    }

    /// Insert a transcript row while keeping a scrolled-up viewport anchored.
    fn push_scrolled(&mut self, entry: Entry, added_lines: usize) {
        if self.scroll > 0 {
            self.scroll = self.scroll.saturating_add(added_lines);
        }
        self.entries.push(entry);
    }

    fn push(&mut self, speaker: Speaker, text: impl Into<String>) {
        let text = text.into();
        if self.scroll > 0 {
            self.scroll = self.scroll.saturating_add(text.lines().count() + 1);
        }
        self.entries.push(Entry { speaker, text });
    }

    fn apply_progress(&mut self, progress: Progress) {
        match progress {
            Progress::ToolBegin { id, title } => {
                self.tool_calls += 1;
                let row = self.entries.len();
                self.push_scrolled(
                    Entry {
                        speaker: Speaker::Tool,
                        text: format!(" ⠋ {title}  …"),
                    },
                    1,
                );
                self.tool_rows.insert(id, row);
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
                if let Some(index) = self.tool_rows.remove(&id) {
                    let preview = shape_preview(&output, if ok { 3 } else { 15 });
                    let icon = if ok { "✓" } else { "✗" };
                    let old_lines = self.entries[index].text.lines().count().max(1);
                    self.entries[index].text = format!(
                        " {icon} {}  {:.1}s{}",
                        self.entries[index]
                            .text
                            .trim_start_matches(" ⠋ ")
                            .trim_end_matches("  …"),
                        elapsed_ms as f64 / 1000.0,
                        if preview.is_empty() {
                            String::new()
                        } else {
                            format!("\n   └ {preview}")
                        }
                    );
                    let new_lines = self.entries[index].text.lines().count().max(1);
                    if self.scroll > 0 {
                        self.scroll = self
                            .scroll
                            .saturating_add(new_lines.saturating_sub(old_lines));
                    }
                    self.tool_timeline.push(format!(
                        "{icon} {} · {:.1}s",
                        self.entries[index].text.lines().next().unwrap_or("tool"),
                        elapsed_ms as f64 / 1000.0
                    ));
                    if self.tool_timeline.len() > 200 {
                        self.tool_timeline.remove(0);
                    }
                }
            }
            Progress::Tool(message) => self.push(Speaker::Tool, message),
            Progress::ResetText => {
                if !self.draft.is_empty() {
                    let completed = std::mem::take(&mut self.draft);
                    self.push(Speaker::Agent, completed);
                }
            }
            Progress::TextDelta(piece) => self.draft.push_str(&piece),
            Progress::Usage(usage) => {
                self.turn_usage.add(usage);
                self.usage.add(usage);
            }
            Progress::Metrics(metrics) => self.metrics = metrics,
            Progress::FlagFound { flag, source } => {
                self.flags_found += 1;
                self.last_flag = flag.clone();
                self.push(
                    Speaker::System,
                    format!(
                        "🚩 FLAG FOUND  {flag}\n   Clipboard copy attempted · source: {source} · logged in .ctf/flags.log"
                    ),
                );
            }
        }
    }

    fn render(&mut self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let width_usize = width as usize;
        let height_usize = height as usize;
        let mut frame = vec![Row::new("", Color::Reset); height_usize];
        if height_usize == 0 {
            return Ok(());
        }
        let colors = theme(self.theme);
        let show_logo = self.entries.len() <= 1 && self.input.is_empty() && !self.busy;
        let show_alert = !self.last_flag.is_empty();
        let mut body_start = 0usize;
        if show_logo {
            let spin = SPINNER[self.spinner % SPINNER.len()];
            for (index, line) in LOGO.iter().enumerate() {
                if body_start >= height_usize {
                    break;
                }
                let tail = match index {
                    0 => format!(
                        "  {spin}  session {} · [{}]",
                        self.session_id, self.category
                    ),
                    2 => format!(
                        "  WROSECODE v{} · {} · {}",
                        env!("CARGO_PKG_VERSION"),
                        speed_tier(self.thinking_level),
                        self.root
                    ),
                    _ => String::new(),
                };
                frame[body_start] =
                    Row::new(clip(&format!("{line}{tail}"), width_usize), colors.accent);
                body_start += 1;
            }
        }
        if body_start < height_usize {
            frame[body_start] = Row::new(
                clip(
                    &format!(
                        " {} • {} / {} • {} • {} • [{}] • think {} · {}",
                        self.mode,
                        self.provider,
                        self.model,
                        self.harness,
                        self.permission,
                        self.category,
                        self.thinking_level,
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
        let body_end = height_usize.saturating_sub(3);
        let body_height = body_end.saturating_sub(body_start);
        let left_width = width_usize
            .saturating_mul(self.pane_percent)
            .checked_div(100)
            .unwrap_or(0)
            .clamp(24, width_usize.saturating_sub(22).max(24));
        let right_width = width_usize.saturating_sub(left_width + 1);
        let top_height = body_height.saturating_mul(2).checked_div(3).unwrap_or(0);
        let bottom_height = body_height.saturating_sub(top_height);
        let mut transcript = Vec::new();
        // Only stream the live draft into the viewport when it is pinned to the
        // bottom; otherwise its growth would shift a scrolled-up view every tick.
        let draft_entry = (self.scroll == 0 && !self.draft.is_empty()).then(|| Entry {
            speaker: Speaker::Agent,
            text: self.draft.clone(),
        });
        for entry in self.entries.iter().chain(draft_entry.iter()) {
            let label = match entry.speaker {
                Speaker::User => "YOU",
                Speaker::Agent => "WROSE",
                Speaker::Tool => "TOOL",
                Speaker::System => "INFO",
            };
            let available = left_width.saturating_sub(9).max(1);
            for (part_index, line) in entry.text.lines().enumerate() {
                for (wrap_index, part) in if self.verbosity == "verbose" {
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
                    transcript.push(clip(&format!("{prefix}{part}"), left_width));
                }
            }
            transcript.push(String::new());
        }
        let transcript_height = top_height.saturating_sub(1);
        let max_scroll = transcript.len().saturating_sub(transcript_height);
        self.scroll = self.scroll.min(max_scroll);
        let transcript_end = transcript.len().saturating_sub(self.scroll);
        let transcript_start = transcript_end.saturating_sub(transcript_height);

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
                    " TRANSCRIPT  {} {}",
                    self.entries.len(),
                    if self.entries.len() == 1 {
                        "entry"
                    } else {
                        "entries"
                    }
                )
            } else {
                transcript
                    .get(transcript_start + row - 1)
                    .filter(|_| transcript_start + row - 1 < transcript_end)
                    .cloned()
                    .unwrap_or_default()
            };
            let right = dashboard.get(row).cloned().unwrap_or_default();
            frame[body_start + row] = Row::new(
                join_panes(&left, &right, left_width, right_width),
                theme(self.theme).text,
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
                    " root [{}] think {}/20 · {}",
                    self.category,
                    self.thinking_level,
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
                    theme(self.theme).muted,
                );
            }
        }
        let choices: Vec<String> = if self.picker.is_some() {
            picker_view(self)
        } else if self.palette {
            commands::filtered(&self.input)
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
                    fg: theme(self.theme).accent,
                    bg: Color::Black,
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
                        theme(self.theme).accent
                    } else {
                        Color::Black
                    },
                };
            }
        }
        let status_row = height_usize.saturating_sub(3);
        let spinner = SPINNER;
        let status_text = if self.busy {
            format!(
                " {} {} • {} · {}w • tok {}/{} • cost {} • cache {}% • timeout {}s • tools {}:{} • {}s • think {}",
                spinner[self.spinner % spinner.len()],
                self.status,
                speed_tier(self.thinking_level),
                level_workers(self.thinking_level),
                self.usage.input,
                self.usage.output,
                self.metrics
                    .cost_usd
                    .map(|cost| format!("${cost:.4}"))
                    .unwrap_or_else(|| "n/a".into()),
                cache_percent(self.usage),
                self.timeout_seconds,
                self.tool_calls,
                self.tool_rows.len(),
                self.session_started.elapsed().as_secs(),
                self.thinking_level
            )
        } else {
            format!(
                " {} • {} · {}w • tok {}/{} • cost {} • cache {}% • tools {}:{} • errors {} • flags {} • {}s • think {}",
                self.status, speed_tier(self.thinking_level),
                level_workers(self.thinking_level), self.usage.input, self.usage.output,
                self.metrics.cost_usd.map(|cost| format!("${cost:.4}")).unwrap_or_else(|| "n/a".into()),
                cache_percent(self.usage), self.tool_calls,
                self.tool_rows.len(), self.tool_failures, self.flags_found,
                self.session_started.elapsed().as_secs(), self.thinking_level
            )
        };
        if status_row < height_usize {
            frame[status_row] = Row::new(clip(&status_text, width_usize), theme(self.theme).status);
        }
        if height_usize > 1 {
            frame[height_usize - 2] = Row::new("─".repeat(width_usize), theme(self.theme).muted);
        }
        let input_width = width_usize.saturating_sub(3);
        let chars: Vec<char> = self.input.chars().collect();
        let cursor_chars = self.input[..self.cursor].chars().count();
        let start = cursor_chars.saturating_sub(input_width.saturating_sub(1));
        let visible: String = if self.mask_input {
            chars
                .iter()
                .skip(start)
                .take(input_width)
                .map(|_| '•')
                .collect()
        } else {
            chars
                .iter()
                .skip(start)
                .take(input_width)
                .map(|ch| if *ch == '\n' { '↵' } else { *ch })
                .collect()
        };
        frame[height_usize - 1] = Row::new(
            format!("> {visible}"),
            input_syntax_color(&self.input, theme(self.theme)),
        );
        for row in &mut frame {
            if row.bg == Color::Reset {
                row.bg = theme(self.theme).background;
            }
        }
        let x = (2 + cursor_chars.saturating_sub(start)).min(width_usize.saturating_sub(1)) as u16;
        self.renderer
            .draw(&frame, (x, height.saturating_sub(1)), (width, height))
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

#[derive(Clone, Copy)]
struct Theme {
    name: &'static str,
    text: Color,
    muted: Color,
    accent: Color,
    status: Color,
    background: Color,
}

fn theme(index: usize) -> Theme {
    const THEMES: [Theme; 5] = [
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
    ];
    THEMES[index % THEMES.len()]
}

fn theme_index(name: &str) -> usize {
    match name.to_ascii_lowercase().as_str() {
        "wrose-dark" | "dark" => 0,
        "light" => 1,
        "solarized" => 2,
        "dracula" => 3,
        "nord" => 4,
        _ => 0,
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

fn input_syntax_color(input: &str, colors: Theme) -> Color {
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
        "oom" => "Close other panes with Ctrl+U and lower the thinking level with `[`.",
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
            for spec in commands::filtered(command) {
                matches.push(spec.name.to_string());
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

fn shape_preview(output: &str, limit: usize) -> String {
    let clean = output.replace('\u{1b}', "");
    let lines: Vec<_> = clean
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() <= limit {
        return lines.join(" ");
    }
    let head = limit.min(3);
    let tail = limit.saturating_sub(head);
    format!(
        "{} … {} more lines … {}",
        lines[..head].join(" "),
        lines.len().saturating_sub(limit),
        lines[lines.len() - tail..].join(" ")
    )
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
) -> Result<()> {
    let profile = TerminalProfile::detect(
        agent.config.alternate_screen,
        agent.config.mouse_capture.as_deref(),
    );
    let _terminal = TerminalGuard::enter(profile)?;
    let (event_tx, mut events) = mpsc::unbounded_channel();
    let (permission_tx, mut permissions) = mpsc::unbounded_channel();
    agent.event_tx = Some(event_tx);
    agent.tools.permission_tx = Some(permission_tx);
    let mut settings = Settings::load()?;
    let session_dir = Session::dir()?;
    let mut session = initial_session.unwrap_or_else(Session::fresh);
    let mut ui = Ui::new(&agent, session.name.clone());
    ui.renderer.full_redraw = profile.full_redraw;
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
            continue;
        }
        let next = event::read()?;
        if let Event::Paste(text) = &next {
            for ch in text.chars() {
                if ch != '\r' {
                    ui.insert(ch);
                }
            }
            ui.palette = ui.input.starts_with('/') && !ui.input.contains(' ');
            ui.selected = 0;
            continue;
        }
        if let Event::Mouse(mouse) = &next {
            let (terminal_width, _) = terminal::size()?;
            let separator = terminal_width as usize * ui.pane_percent / 100;
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
                MouseEventKind::ScrollUp => {
                    ui.scroll = ui.scroll.saturating_add(agent.config.smooth_scroll_lines)
                }
                MouseEventKind::ScrollDown => {
                    ui.scroll = ui.scroll.saturating_sub(agent.config.smooth_scroll_lines)
                }
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
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('c')
        {
            break;
        }
        match key.code {
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
                ui.set_input(String::new());
                ui.entries.clear();
                ui.tool_rows.clear();
                ui.scroll = 0;
                ui.last_flag.clear();
                ui.palette = false;
                ui.selected = 0;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.set_input(String::new());
                ui.palette = false;
                ui.selected = 0;
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.entries.clear();
                ui.tool_rows.clear();
                ui.tool_timeline.clear();
                ui.scroll = 0;
                ui.last_flag.clear();
                ui.palette = false;
                ui.selected = 0;
                ui.push(
                    Speaker::System,
                    "Transcript cleared. Session context is preserved.",
                );
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.set_input("/".into());
                ui.palette = true;
                ui.selected = 0;
                ui.status = "Command palette".into();
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.theme = (ui.theme + 1) % 5;
                ui.status = format!("Theme: {}", theme(ui.theme).name);
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
                agent.thinking_level = agent.thinking_level.saturating_sub(1);
                ui.thinking_level = agent.thinking_level;
            }
            KeyCode::Char(']') if ui.input.is_empty() => {
                agent.thinking_level = (agent.thinking_level + 1).min(20);
                ui.thinking_level = agent.thinking_level;
            }
            KeyCode::Char('k') if ui.navigation_mode => {
                ui.scroll = ui.scroll.saturating_add(1);
            }
            KeyCode::Char('j') if ui.navigation_mode => {
                ui.scroll = ui.scroll.saturating_sub(1);
            }
            KeyCode::Char(' ') if ui.navigation_mode => ui.scroll = ui.scroll.saturating_sub(10),
            KeyCode::Char('b') if ui.navigation_mode => ui.scroll = ui.scroll.saturating_add(10),
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
            KeyCode::Char(ch) => {
                ui.navigation_mode = false;
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
            KeyCode::Left => ui.move_left(),
            KeyCode::Right => ui.move_right(),
            KeyCode::Home => ui.cursor = 0,
            KeyCode::End => ui.cursor = ui.input.len(),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.scroll = ui.scroll.saturating_add(5);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ui.scroll = ui.scroll.saturating_sub(5);
            }
            KeyCode::Up if ui.palette => {
                let len = commands::filtered(&ui.input).len();
                if len > 0 {
                    ui.selected = (ui.selected + len - 1) % len;
                }
            }
            KeyCode::Up if ui.picker.is_some() => {
                ui.selected = ui.selected.saturating_sub(1);
            }
            KeyCode::Down if ui.palette => {
                let len = commands::filtered(&ui.input).len();
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
            KeyCode::Up if ui.input.is_empty() && ui.scroll > 0 => {
                ui.scroll = ui.scroll.saturating_add(1)
            }
            KeyCode::Down if ui.input.is_empty() && ui.scroll > 0 => {
                ui.scroll = ui.scroll.saturating_sub(1)
            }
            KeyCode::Up if ui.input.is_empty() && ui.history_index.is_none() => {
                ui.scroll = ui.scroll.saturating_add(1);
            }
            KeyCode::Down if ui.input.is_empty() && ui.history_index.is_none() => {
                ui.scroll = ui.scroll.saturating_sub(1);
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
            KeyCode::PageUp => ui.scroll = ui.scroll.saturating_add(5),
            KeyCode::PageDown => ui.scroll = ui.scroll.saturating_sub(5),
            KeyCode::Tab if !ui.input.is_empty() => complete_input(&mut ui, &agent),
            KeyCode::Tab => {
                let next = if agent.mode == "build" {
                    "plan"
                } else {
                    "build"
                };
                agent.set_mode(next)?;
                ui.mode = next.to_ascii_uppercase();
                ui.status = format!("{} mode", ui.mode);
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                ui.insert('\n');
                ui.palette = false;
            }
            KeyCode::Enter => {
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
                        } else {
                            let _ = crate::ctf::copy_to_clipboard(choice);
                            ui.status = "Flag history entry copied".into();
                        }
                    }
                    continue;
                }
                if ui.palette {
                    let choices = commands::filtered(&ui.input);
                    if let Some(command) = choices.get(ui.selected) {
                        ui.set_input(command.name.into());
                    }
                    ui.palette = false;
                    ui.selected = 0;
                }
                let line = ui.take_input();
                if line.is_empty() {
                    continue;
                }
                ui.history.push(line.clone());
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
                    if !run_prompt(&mut agent, &mut ui, &line, &mut events, &mut permissions)
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

async fn run_prompt(
    agent: &mut Agent,
    ui: &mut Ui,
    prompt: &str,
    events: &mut mpsc::UnboundedReceiver<Progress>,
    permissions: &mut mpsc::UnboundedReceiver<PermissionRequest>,
) -> Result<bool> {
    ui.category = crate::ctf::categorize(&agent.config.root, prompt);
    ui.busy = true;
    ui.status = "Thinking".into();
    ui.begin_turn();
    ui.render()?;
    let project_root = agent.config.root.clone();
    let session_store = agent.store.clone();
    let metrics = agent.metrics.clone();
    let turn = agent.turn(prompt);
    tokio::pin!(turn);
    let mut ticker = tokio::time::interval(Duration::from_millis(16));
    loop {
        tokio::select! {
            result = &mut turn => {
                while let Ok(message) = events.try_recv() { ui.apply_progress(message); }
                ui.draft.clear();
                ui.end_turn();
                ui.busy = false;
                ui.status = "Ready".into();
                match result {
                    Ok(reply) => ui.push(Speaker::Agent, if reply.is_empty() { "(no response)".into() } else { reply }),
                    Err(error) => {
                        crate::ctf::log_error(&project_root, "agent turn", &format!("{error:#}"));
                        let kind = classify_error(&error.to_string());
                        if kind == "rate_limit" {
                            metrics.record_rate_limit();
                        }
                        let _ = session_store.record_error(
                            &ui.session_id,
                            kind,
                            &error.to_string(),
                            "agent turn",
                        );
                        ui.record_error(kind);
                        ui.push(Speaker::System, format!(
                            "Error [{}]: {error:#}\n   Fix: {}",
                            kind,
                            error_suggestion(kind)
                        ));
                    }
                }
                ui.render()?;
                return Ok(true);
            }
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
                if event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') => return Ok(false),
                        Event::Key(key) if key.code == KeyCode::PageUp => ui.scroll = ui.scroll.saturating_add(5),
                        Event::Key(key) if key.code == KeyCode::PageDown => ui.scroll = ui.scroll.saturating_sub(5),
                        _ => {}
                    }
                }
                ui.render()?;
            }
        }
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
                    let _ = request.response.send(false);
                    return Ok(false);
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
        ui.push(
            Speaker::System,
            format!("Unknown command: {name}. Type /help."),
        );
        return Ok(());
    }
    match name {
        "/help" => ui.push(Speaker::System, commands::help()),
        "/build" | "/plan" => {
            let mode = name.trim_start_matches('/');
            agent.set_mode(mode)?;
            ui.mode = mode.to_ascii_uppercase();
            ui.push(Speaker::System, format!("{mode} agent active"));
        }
        "/agents" => {
            let modes = ["build", "plan", "general"];
            let labels = modes
                .iter()
                .map(|mode| format!("{} {mode}", if agent.mode == *mode { "●" } else { " " }))
                .collect();
            if let Some(index) = picker(ui, "Agents", labels)? {
                agent.set_mode(modes[index])?;
                ui.mode = modes[index].to_ascii_uppercase();
                ui.push(Speaker::System, format!("{} agent active", modes[index]));
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
            ui.push(Speaker::System, format!("Provider/model: {} / {}\nPermission: {}\nAgent: {}\nSkills: {}\nMemory facts: {}\nRepo map files: {}\nLast API latency: {}\nVersion: {} ({})", agent.config.provider, agent.config.model, ui.permission, agent.mode, agent.skills.len(), agent.memory.list()?.len(), map_size, latency, env!("CARGO_PKG_VERSION"), option_env!("WROSECODE_GIT_COMMIT").unwrap_or("unknown")));
        }
        "/verbosity" => {
            let current = ui.verbosity.clone();
            let mode = if args.is_empty() { ask_line(ui, "Verbosity (compact/normal/verbose)", &current)? } else { Some(args.into()) };
            if let Some(mode) = mode { if matches!(mode.as_str(), "compact" | "normal" | "verbose") { ui.verbosity = mode.clone(); ui.push(Speaker::System, format!("Verbosity: {mode}")); } else { ui.push(Speaker::System, "Verbosity must be compact, normal, or verbose"); } }
        }
        "/stats" => ui.push(Speaker::System, format!("Model: {}\nProvider: {}\n{}\nLast API latency: {}\nTranscript entries: {}\nTool cells: {}\nScroll: {}", agent.config.model, agent.config.provider, agent.stats_line(), agent.last_api_latency.map(|d| format!("{} ms", d.as_millis())).unwrap_or_else(|| "none".into()), ui.entries.len(), ui.tool_rows.len(), ui.scroll)),
        "/export" => {
            let (json, csv) = agent.metrics.export(&agent.config.root.join(".ctf/reports"))?;
            ui.push(Speaker::System, format!("Exported:\n{}\n{}", json.display(), csv.display()));
        }
        "/flags" => {
            let flags = agent.ctf.history(args);
            ui.push(Speaker::System, if flags.is_empty() { "No matching flags".into() } else { flags.join("\n") });
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
            let flags: Vec<String> = agent.ctf.history("").into_iter().filter_map(|line| line.split('\t').nth(3).map(str::to_string)).collect();
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
            ui.push(Speaker::System, format!("Writeup generated: {}", path.display()));
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
        "/new" => {
            save_session(session, agent, ui, &session_dir)?;
            *session = Session::fresh();
            agent.messages.clear();
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
        "/sessions" => {
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
            let models = available_models(agent, settings).await;
            let labels = models
                .iter()
                .map(|(name, model)| format!("{name}  /  {model}"))
                .collect();
            if let Some(index) = picker(ui, "Models", labels)? {
                switch_provider(agent, ui, settings, &models[index].0, &models[index].1)?;
            }
        }
        "/mcps" => {
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
                ui.push(Speaker::User, prompt.clone());
                run_prompt(agent, ui, &prompt, events, permissions).await?;
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
        let Some(bin) = ask_line(ui, "MCP executable", "")? else {
            return Ok(());
        };
        let Some(arguments) = ask_line(ui, "MCP arguments (space separated)", "")? else {
            return Ok(());
        };
        let def = McpServerDef {
            name: name.clone(),
            bin,
            args: arguments.split_whitespace().map(str::to_owned).collect(),
        };
        let mcp = crate::tools::mcp::Mcp::connect(&def.bin, &def.args).await?;
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
                "{}: connected — {}",
                def.name,
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
    fn sparkline_degrades_gracefully() {
        assert_eq!(sparkline(&[], 10), "·");
        assert!(!sparkline(&[1, 2, 3], 4).is_empty());
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
}
