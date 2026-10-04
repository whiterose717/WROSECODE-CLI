//! Thinking levels (spec PHASE 4): `off | low | medium | high | max | auto`,
//! cycled with `Ctrl+T`, set with `/think <level>` and `--think`, defaulted
//! per profile in `providers.toml`. Each concrete level maps to the provider's
//! native control (Anthropic extended-thinking budget tokens, OpenAI-style
//! `reasoning_effort`) through a per-model `think_map`.
//!
//! `auto` starts at `medium`, escalates one level after repeated failures,
//! and drops back down after progress — the transitions are decided by
//! [`auto_think`] and reported to the transcript as
//! `think: medium → high (no progress ×3)`.

use clap::ValueEnum;
use serde::Deserialize;

/// The six thinking levels. The first five are concrete strengths (each has
/// a fixed 0–20 anchor used by the existing speed-tier / worker logic);
/// `Auto` is a control mode that manages its anchor at runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkLevel {
    Off,
    Low,
    Medium,
    High,
    Max,
    Auto,
}

/// One entry of a per-model `think_map`: what the provider actually expects
/// for this level — a `reasoning_effort` string for OpenAI-style APIs, or a
/// budget-token count for Anthropic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThinkValue {
    Text(String),
    Number(u64),
}

impl ThinkLevel {
    /// Cycle order for `Ctrl+T` and `[` / `]`.
    pub const ALL: [ThinkLevel; 6] = [
        ThinkLevel::Off,
        ThinkLevel::Low,
        ThinkLevel::Medium,
        ThinkLevel::High,
        ThinkLevel::Max,
        ThinkLevel::Auto,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ThinkLevel::Off => "off",
            ThinkLevel::Low => "low",
            ThinkLevel::Medium => "medium",
            ThinkLevel::High => "high",
            ThinkLevel::Max => "max",
            ThinkLevel::Auto => "auto",
        }
    }

    /// Case-insensitive parse of the level name.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(ThinkLevel::Off),
            "low" => Some(ThinkLevel::Low),
            "medium" => Some(ThinkLevel::Medium),
            "high" => Some(ThinkLevel::High),
            "max" => Some(ThinkLevel::Max),
            "auto" => Some(ThinkLevel::Auto),
            _ => None,
        }
    }

    /// Next level in cycle order (`max → auto → off`).
    pub fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|level| *level == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Previous level in cycle order (`off → auto → max`).
    pub fn prev(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|level| *level == self)
            .unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// The 0–20 strength this level maps to. `Auto` reports its starting
    /// point (`medium`); the live anchor while auto-managed lives in
    /// `Agent::thinking_level`.
    pub fn anchor(self) -> u8 {
        match self {
            ThinkLevel::Off => 0,
            ThinkLevel::Low => 3,
            ThinkLevel::Medium => 5,
            ThinkLevel::High => 10,
            ThinkLevel::Max => 20,
            ThinkLevel::Auto => 5,
        }
    }

    /// The concrete level a 0–20 strength belongs to (used to translate the
    /// managed anchor back into a provider-facing level).
    pub fn from_level(level: u8) -> Self {
        match level {
            0 => ThinkLevel::Off,
            1..=4 => ThinkLevel::Low,
            5..=9 => ThinkLevel::Medium,
            10..=19 => ThinkLevel::High,
            _ => ThinkLevel::Max,
        }
    }

    pub fn is_auto(self) -> bool {
        self == ThinkLevel::Auto
    }

    /// Next concrete level up (never yields `Auto`).
    fn escalate(self) -> Option<Self> {
        match self {
            ThinkLevel::Off => Some(ThinkLevel::Low),
            ThinkLevel::Low => Some(ThinkLevel::Medium),
            ThinkLevel::Medium => Some(ThinkLevel::High),
            ThinkLevel::High => Some(ThinkLevel::Max),
            _ => None,
        }
    }

    /// Previous concrete level down, floored at `Low`.
    pub(crate) fn relax(self) -> Option<Self> {
        match self {
            ThinkLevel::Max => Some(ThinkLevel::High),
            ThinkLevel::High => Some(ThinkLevel::Medium),
            ThinkLevel::Medium => Some(ThinkLevel::Low),
            _ => None,
        }
    }
}

/// What the `auto` controller should do after one step of the run loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AutoAction {
    None,
    Escalate { to: ThinkLevel, reason: String },
    Drop { to: ThinkLevel, reason: String },
}

/// The `auto` policy (spec PHASE 4):
///
/// * after `fail_streak >= 3` failing steps, escalate one level —
///   `Escalate { reason: "no progress ×3" }`;
/// * after an escalation, a clean step (no tool failures) drops back one
///   level — `Drop { reason: "progress" }`;
/// * anything else leaves the level alone.
///
/// `level` is the live 0–20 anchor; `escalated` records that an escalation
/// is still awaiting its progress step.
pub(crate) fn auto_think(fail_streak: u8, level: u8, escalated: bool, clean: bool) -> AutoAction {
    if fail_streak >= 3 {
        match ThinkLevel::from_level(level).escalate() {
            Some(to) => AutoAction::Escalate {
                to,
                reason: format!("no progress ×{fail_streak}"),
            },
            None => AutoAction::None,
        }
    } else if escalated && clean {
        match ThinkLevel::from_level(level).relax() {
            Some(to) => AutoAction::Drop {
                to,
                reason: "progress".into(),
            },
            None => AutoAction::None,
        }
    } else {
        AutoAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_report_names() {
        for level in ThinkLevel::ALL {
            assert_eq!(ThinkLevel::parse(level.name()), Some(level));
            assert_eq!(
                ThinkLevel::parse(&level.name().to_ascii_uppercase()),
                Some(level)
            );
        }
        assert_eq!(ThinkLevel::parse("  Deep  "), None);
        assert_eq!(ThinkLevel::parse("0"), None);
    }

    #[test]
    fn cycle_wraps_in_both_directions() {
        let order = ThinkLevel::ALL;
        for (index, level) in order.iter().enumerate() {
            assert_eq!(level.next(), order[(index + 1) % order.len()]);
            assert_eq!(level.prev(), order[(index + order.len() - 1) % order.len()]);
        }
        assert_eq!(ThinkLevel::Max.next(), ThinkLevel::Auto);
        assert_eq!(ThinkLevel::Auto.next(), ThinkLevel::Off);
        assert_eq!(ThinkLevel::Off.prev(), ThinkLevel::Auto);
    }

    #[test]
    fn anchors_map_to_the_existing_speed_bands() {
        assert_eq!(ThinkLevel::Off.anchor(), 0);
        assert_eq!(ThinkLevel::Low.anchor(), 3);
        assert_eq!(ThinkLevel::Medium.anchor(), 5);
        assert_eq!(ThinkLevel::High.anchor(), 10);
        assert_eq!(ThinkLevel::Max.anchor(), 20);
        // The shipped config default (thinking_level = 5) is `medium`.
        assert_eq!(ThinkLevel::from_level(5), ThinkLevel::Medium);
        assert_eq!(ThinkLevel::from_level(0), ThinkLevel::Off);
        assert_eq!(ThinkLevel::from_level(3), ThinkLevel::Low);
        assert_eq!(ThinkLevel::from_level(17), ThinkLevel::High);
        assert_eq!(ThinkLevel::from_level(20), ThinkLevel::Max);
        // Every anchor round-trips through from_level.
        for level in ThinkLevel::ALL {
            if level != ThinkLevel::Auto {
                assert_eq!(ThinkLevel::from_level(level.anchor()), level);
            }
        }
    }

    #[test]
    fn auto_escalates_after_three_failures_and_drops_on_progress() {
        // Three failing steps escalate one level.
        let action = auto_think(3, ThinkLevel::Medium.anchor(), false, false);
        assert_eq!(
            action,
            AutoAction::Escalate {
                to: ThinkLevel::High,
                reason: "no progress ×3".into()
            }
        );
        // A clean step after an escalation drops back one level.
        let action = auto_think(0, ThinkLevel::High.anchor(), true, true);
        assert_eq!(
            action,
            AutoAction::Drop {
                to: ThinkLevel::Medium,
                reason: "progress".into()
            }
        );
        // Below three failures nothing happens.
        assert_eq!(auto_think(2, 5, false, false), AutoAction::None);
        // A clean step without a prior escalation does nothing.
        assert_eq!(auto_think(0, 5, false, true), AutoAction::None);
        // Already at max: no escalation.
        assert_eq!(
            auto_think(9, ThinkLevel::Max.anchor(), false, false),
            AutoAction::None
        );
        // Low cannot drop below low.
        assert_eq!(
            auto_think(0, ThinkLevel::Low.anchor(), true, true),
            AutoAction::None
        );
        // Escalation climbs all the way up: low → medium → high → max.
        let action = auto_think(3, ThinkLevel::Low.anchor(), false, false);
        assert_eq!(
            action,
            AutoAction::Escalate {
                to: ThinkLevel::Medium,
                reason: "no progress ×3".into()
            }
        );
        let action = auto_think(3, ThinkLevel::High.anchor(), false, false);
        assert_eq!(
            action,
            AutoAction::Escalate {
                to: ThinkLevel::Max,
                reason: "no progress ×3".into()
            }
        );
    }
}
