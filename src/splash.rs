//! First-launch splash.
//!
//! `main` paints this the moment the terminal is taken over, before provider
//! setup, skill discovery, metrics, MCP servers, and session restore. That
//! ordering is the point: the shell appears while the expensive startup work
//! is still running, so the first frame lands inside
//! [`FIRST_PAINT_BUDGET_MS`] instead of after it.

use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::Instant;

use crossterm::{
    cursor, execute,
    terminal::{Clear, ClearType},
};

use crate::tui::TerminalProfile;

/// Startup budget for the first painted frame. A splash that takes longer
/// than this is decoration standing between the user and their shell, so the
/// value is part of the contract and shows up in the splash itself.
pub const FIRST_PAINT_BUDGET_MS: u128 = 50;

static FIRST_PAINT_MS: OnceLock<u128> = OnceLock::new();

/// The wordmark. Lives here because the splash owns branding; `Ui::compose`
/// renders the same block as its header while the transcript is empty.
pub const LOGO: [&str; 3] = [
    "╦ ╦╔╗ ╭─╮╔═╗╔═╗╭─╮╭─╮╔═╗╔═╗",
    "║ ║╠╩╗│ │╠═╝╠═╗│  │ │║ ║╠═╗",
    "╚═╝╚═╝╰─╯╚═╝╚═╝╰─╯╰─╯╚═╝╚═╝",
];

/// Milliseconds from process start to the first painted frame, recorded by
/// [`paint`]. `None` until the splash is painted (headless runs never do).
pub fn first_paint_ms() -> Option<u128> {
    FIRST_PAINT_MS.get().copied()
}

/// Whether the recorded first paint is inside [`FIRST_PAINT_BUDGET_MS`].
pub fn within_budget() -> bool {
    matches!(first_paint_ms(), Some(ms) if ms <= FIRST_PAINT_BUDGET_MS)
}

/// Splash rows as plain text so the drawing path and the tests share one
/// shape. `paint_ms` of `None` omits the timing line (nothing was measured).
pub fn banner(paint_ms: Option<u128>) -> Vec<String> {
    let mut lines: Vec<String> = LOGO.iter().map(|logo| (*logo).to_string()).collect();
    lines.push(format!(
        "  WROSECODE v{} · agentic coding and CTF shell",
        env!("CARGO_PKG_VERSION")
    ));
    if let Some(ms) = paint_ms {
        lines.push(timing_line(ms));
    }
    lines
}

fn timing_line(ms: u128) -> String {
    if ms <= FIRST_PAINT_BUDGET_MS {
        format!("  first paint {ms}ms · budget {FIRST_PAINT_BUDGET_MS}ms")
    } else {
        format!("  first paint {ms}ms · over budget {FIRST_PAINT_BUDGET_MS}ms")
    }
}

/// Splash rows for a specific terminal: wordmark rows first (gradient-painted
/// to match `colors`), then the plain tail lines. Below 80 columns the block
/// art collapses to a single `WROSECODE v…` line.
pub fn paint_lines(
    width: u16,
    colors: crate::tui::ColorSupport,
    paint_ms: Option<u128>,
) -> Vec<String> {
    if width < 80 {
        let mut painted = vec![crate::tui::gradient_paint(
            &format!("WROSECODE v{}", env!("CARGO_PKG_VERSION")),
            colors,
        )];
        if let Some(ms) = paint_ms {
            painted.push(timing_line(ms));
        }
        return painted;
    }
    banner(paint_ms)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            if index < LOGO.len() {
                crate::tui::gradient_paint(&row, colors)
            } else {
                row
            }
        })
        .collect()
}

/// Progressive reveal stage of the startup wordmark: stage 0 draws the first
/// logo row, each later stage one more, until the full banner is showing.
/// Callers repaint between init steps they were already doing (provider
/// setup, skills, MCP), so a slow startup visibly sweeps the banner in while
/// a fast one just flashes past — the stages themselves never sleep, adding
/// no launch latency.
pub fn paint_stage(profile: TerminalProfile, boot: &Instant, stage: u8) -> io::Result<()> {
    let ms = boot.elapsed().as_millis();
    if stage == 0 {
        let _ = FIRST_PAINT_MS.set(ms);
    }
    let width = crossterm::terminal::size()
        .map(|(width, _)| width)
        .unwrap_or(80);
    let lines = reveal_lines(paint_lines(width, profile.colors, Some(ms)), stage);
    let mut out = io::stdout().lock();
    if profile.alternate {
        execute!(out, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
    }
    for line in lines {
        writeln!(out, "{line}")?;
    }
    out.flush()
}

/// Hide wordmark rows beyond the reveal stage (later rows paint blank, then
/// fill in on the next stage). The tail lines always show: version, timing
/// and status stay readable from the very first frame.
fn reveal_lines(mut lines: Vec<String>, stage: u8) -> Vec<String> {
    let shown = (stage as usize + 1).min(LOGO.len());
    for (index, line) in lines.iter_mut().enumerate().take(LOGO.len()) {
        if index >= shown {
            line.clear();
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_fifty_milliseconds() {
        assert_eq!(FIRST_PAINT_BUDGET_MS, 50);
    }

    #[test]
    fn banner_carries_the_wordmark_version_and_timing() {
        let lines = banner(Some(12));
        assert_eq!(lines.len(), 5, "three logo rows, version, timing");
        assert!(lines[0].starts_with("╦ ╦"));
        assert!(
            lines[3].contains(env!("CARGO_PKG_VERSION")),
            "the version row: {:?}",
            lines[3]
        );
        assert!(
            lines[4].contains("first paint 12ms") && lines[4].contains("budget 50ms"),
            "the timing row: {:?}",
            lines[4]
        );
    }

    #[test]
    fn banner_reports_a_budget_miss_instead_of_hiding_it() {
        let over = banner(Some(90));
        assert!(
            over[4].contains("over budget 50ms"),
            "an over-budget splash says so: {:?}",
            over[4]
        );
    }

    #[test]
    fn no_measurement_means_no_timing_line() {
        let lines = banner(None);
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| !line.contains("first paint")));
        // Nothing painted yet: budget check fails closed.
        assert!(!within_budget());
        assert!(first_paint_ms().is_none());
    }

    #[test]
    fn reveal_stages_draw_the_wordmark_progressively() {
        let full = banner(Some(3));
        let stage0 = reveal_lines(full.clone(), 0);
        assert!(!stage0[0].is_empty(), "first row shows at once");
        assert!(stage0[1].is_empty() && stage0[2].is_empty());
        // Tail lines (version, timing) stay readable from frame one.
        assert!(stage0[3].contains(env!("CARGO_PKG_VERSION")));
        assert!(stage0[4].contains("first paint"));
        let stage2 = reveal_lines(full.clone(), 2);
        assert_eq!(stage2, full, "the last stage is the full banner");
        let beyond = reveal_lines(full.clone(), 9);
        assert_eq!(beyond, full, "stages past the end clamp");
    }
}
