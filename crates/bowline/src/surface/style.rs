//! Terminal design system for Bowline's human-facing output.
//!
//! One source of truth for color, glyphs, and layout so every command reads as
//! one product. Styling degrades automatically: `--json`, `NO_COLOR`, a piped
//! stdout, or `TERM=dumb` all fall back to plain text, and the plain path is the
//! one exercised by golden tests (so fixtures stay deterministic).

use std::env;
use std::io::{self, IsTerminal};

use bowline_core::commands::StatusCommandOutput;
use bowline_core::status::{RepairCommand, StatusLevel};
use crossterm::style::Stylize;

/// Fixed rule width used whenever we are not attached to an interactive
/// terminal (pipes, tests). Keeps golden output stable.
const DEFAULT_WIDTH: usize = 52;
const MAX_WIDTH: usize = 72;

/// How to present human output for a single invocation. Computed once, threaded
/// into every renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// Emit ANSI color/attributes.
    pub color: bool,
    /// Use Unicode glyphs (vs ASCII fallbacks).
    pub unicode: bool,
    /// Detected terminal width, when interactive.
    pub width: Option<u16>,
}

impl Presentation {
    /// Detect the presentation for the current process.
    ///
    /// Color is on only when: not `--json`, `NO_COLOR` unset, and either
    /// `CLICOLOR_FORCE` is set to a non-empty non-zero value or stdout is a TTY
    /// on a non-dumb terminal. Unicode stays on even when piped (UTF-8 is safe);
    /// it only drops to ASCII on `TERM=dumb` or when `BOWLINE_ASCII` is set.
    pub fn detect(json: bool) -> Self {
        if json {
            return Self {
                color: false,
                unicode: false,
                width: None,
            };
        }
        let term = env::var("TERM").unwrap_or_default();
        let dumb = term == "dumb";
        let no_color = env::var_os("NO_COLOR").is_some();
        let clicolor_force = env::var("CLICOLOR_FORCE")
            .ok()
            .is_some_and(|value| !value.is_empty() && value != "0");
        let is_tty = io::stdout().is_terminal();
        let color = !no_color && (clicolor_force || (is_tty && !dumb));
        let unicode = !dumb && env::var_os("BOWLINE_ASCII").is_none();
        // Only consult the live terminal when we are actually painting it;
        // otherwise keep width deterministic for pipes and tests.
        let width = if color { detect_width() } else { None };
        Self {
            color,
            unicode,
            width,
        }
    }

    /// A plain, deterministic presentation (no color, Unicode glyphs). Handy for
    /// tests and non-interactive fallbacks.
    #[cfg(test)]
    pub fn plain() -> Self {
        Self {
            color: false,
            unicode: true,
            width: None,
        }
    }

    /// Width to draw horizontal rules at.
    pub fn rule_width(&self) -> usize {
        self.width
            .map(|w| (w as usize).saturating_sub(2).clamp(24, MAX_WIDTH))
            .unwrap_or(DEFAULT_WIDTH)
    }
}

fn detect_width() -> Option<u16> {
    if let Ok(cols) = env::var("COLUMNS")
        && let Ok(parsed) = cols.trim().parse::<u16>()
        && parsed > 0
    {
        return Some(parsed);
    }
    crossterm::terminal::size()
        .ok()
        .map(|(w, _)| w)
        .filter(|w| *w > 0)
}

/// A semantic color role. Mapped to named ANSI colors so output adapts to the
/// user's terminal theme instead of fighting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Ready,
    Preparing,
    Attention,
    Limited,
    /// De-emphasized text: keys, secondary notes, timestamps.
    Label,
    /// Commands and links.
    Accent,
    /// Emphasis without color.
    Strong,
}

/// The one-line verdict shown at the top of a status surface. Richer than
/// `StatusLevel`: it splits the calm "in progress" case out of Healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    Preparing,
    NeedsYou,
    Limited,
}

impl Verdict {
    /// Derive the verdict from a composed status output.
    ///
    /// Limited and Needs-you come straight from the level. A Healthy workspace
    /// that is still catching up on sync reads as the calm `Preparing`; a
    /// settled one reads `Ready`.
    pub fn from_output(output: &StatusCommandOutput) -> Self {
        match output.status_summary.presentation_level() {
            StatusLevel::Limited => Verdict::Limited,
            StatusLevel::Attention => Verdict::NeedsYou,
            StatusLevel::Healthy => {
                if is_in_progress(output) {
                    Verdict::Preparing
                } else {
                    Verdict::Ready
                }
            }
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Verdict::Ready => "READY",
            Verdict::Preparing => "PREPARING",
            Verdict::NeedsYou => "NEEDS YOU",
            Verdict::Limited => "LIMITED",
        }
    }

    pub fn role(self) -> Role {
        match self {
            Verdict::Ready => Role::Ready,
            Verdict::Preparing => Role::Preparing,
            Verdict::NeedsYou => Role::Attention,
            Verdict::Limited => Role::Limited,
        }
    }

    pub fn glyph(self, unicode: bool) -> &'static str {
        match (self, unicode) {
            (Verdict::Ready, true) => "●",
            (Verdict::Preparing, true) => "◐",
            (Verdict::NeedsYou, true) => "▲",
            (Verdict::Limited, true) => "■",
            (Verdict::Ready, false) => "+",
            (Verdict::Preparing, false) => "~",
            (Verdict::NeedsYou, false) => "!",
            (Verdict::Limited, false) => "x",
        }
    }
}

/// A Healthy workspace still doing startup/catch-up work.
fn is_in_progress(output: &StatusCommandOutput) -> bool {
    let sync_catching_up = !matches!(
        output.event_watermarks.sync_state,
        Some(bowline_core::status::ComponentState::Ready)
    );
    let has_workspace = output
        .workspace_summary
        .as_ref()
        .and_then(|summary| summary.observed.as_ref())
        .is_some();
    sync_catching_up && has_workspace
}

// ---------------------------------------------------------------------------
// Painting helpers. Each returns a plain `String` when color is off, so callers
// compose freely without caring about the presentation mode.
// ---------------------------------------------------------------------------

/// Paint `text` in a role's color/attribute.
pub fn paint(text: &str, role: Role, pres: &Presentation) -> String {
    if !pres.color {
        return text.to_string();
    }
    match role {
        Role::Ready => text.green().to_string(),
        Role::Preparing => text.cyan().to_string(),
        Role::Attention => text.yellow().to_string(),
        Role::Limited => text.red().to_string(),
        Role::Label => text.dim().to_string(),
        Role::Accent => text.cyan().to_string(),
        Role::Strong => text.bold().to_string(),
    }
}

/// The verdict "chip": a filled color band in interactive mode, plain text
/// otherwise. Includes its own surrounding padding when colored.
pub fn chip(verdict: Verdict, pres: &Presentation) -> String {
    let glyph = verdict.glyph(pres.unicode);
    let inner = format!("{glyph} {}", verdict.word());
    if !pres.color {
        return inner;
    }
    let padded = format!(" {inner} ");
    match verdict.role() {
        Role::Ready => padded.black().on_green().bold().to_string(),
        Role::Preparing => padded.black().on_cyan().bold().to_string(),
        Role::Attention => padded.black().on_yellow().bold().to_string(),
        Role::Limited => padded.white().on_red().bold().to_string(),
        _ => padded.bold().to_string(),
    }
}

/// A horizontal rule at the current width, in a role's color.
pub fn rule(role: Role, pres: &Presentation) -> String {
    let ch = if pres.unicode { "─" } else { "-" };
    let line = ch.repeat(pres.rule_width());
    paint(&line, role, pres)
}

/// An aligned key/value row: `  Label   value`.
pub fn kv(label: &str, value: &str, pres: &Presentation) -> String {
    let key = format!("{label:<11}");
    format!("  {}{}", paint(&key, Role::Label, pres), value)
}

/// A bulleted attention/limited line: `  ! text`.
pub fn bullet(role: Role, text: &str, pres: &Presentation) -> String {
    let glyph = match (role, pres.unicode) {
        (Role::Limited, true) => "■",
        (Role::Limited, false) => "x",
        (_, true) => "▲",
        (_, false) => "!",
    };
    format!("  {} {}", paint(glyph, role, pres), text)
}

/// A `Next` action line: `  → command   description`.
pub fn next_action(command: &str, description: &str, pres: &Presentation) -> String {
    let arrow = if pres.unicode { "→" } else { "->" };
    let painted_cmd = paint(command, Role::Accent, pres);
    if description.is_empty() {
        format!("  {arrow} {painted_cmd}")
    } else {
        format!(
            "  {arrow} {painted_cmd}   {}",
            paint(description, Role::Label, pres)
        )
    }
}

/// A dim section header.
pub fn section(title: &str, pres: &Presentation) -> String {
    paint(title, Role::Label, pres)
}

/// Choose the singular or plural noun for a count: `pluralize(1, "file", "files")`.
pub fn pluralize<'a>(count: u64, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

/// A `count noun` phrase with correct pluralization: `count_noun(1, "env file", "env files")`.
pub fn count_noun(count: u64, singular: &str, plural: &str) -> String {
    format!("{count} {}", pluralize(count, singular, plural))
}

/// A shared `Next` actions block: a dim header plus `→ command   label` lines.
/// Returns empty when there are no actions.
pub fn next_actions_block(actions: &[RepairCommand], pres: &Presentation) -> Vec<String> {
    if actions.is_empty() {
        return Vec::new();
    }
    let mut out = vec![section("Next", pres)];
    for action in actions {
        out.push(match &action.command {
            Some(command) => next_action(command, &action.label, pres),
            None => format!("  {}", paint(&action.label, Role::Label, pres)),
        });
    }
    out
}

/// The kebab-case serde label for an enum value, so callers never leak `{:?}`
/// Debug formatting into user-facing text.
pub fn kebab(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// Format `from` relative to `reference` (both RFC3339): "just now", "3m ago".
/// Returns `None` if either timestamp fails to parse.
pub fn relative_time(from: &str, reference: &str) -> Option<String> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let from = OffsetDateTime::parse(from, &Rfc3339).ok()?;
    let reference = OffsetDateTime::parse(reference, &Rfc3339).ok()?;
    let secs = (reference - from).whole_seconds();
    Some(humanize_secs(secs))
}

/// The wall-clock time-of-day ("12:44:31") from an RFC3339 timestamp.
pub fn clock(rfc3339: &str) -> Option<String> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let ts = OffsetDateTime::parse(rfc3339, &Rfc3339).ok()?;
    Some(format!(
        "{:02}:{:02}:{:02}",
        ts.hour(),
        ts.minute(),
        ts.second()
    ))
}

fn humanize_secs(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 45 {
        return "just now".to_string();
    }
    let mins = (secs + 30) / 60;
    if mins < 60 {
        return format!("{}m ago", mins.max(1));
    }
    let hours = (mins + 30) / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = (hours + 12) / 24;
    format!("{days}d ago")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralize_picks_singular_only_for_one() {
        assert_eq!(pluralize(1, "file", "files"), "file");
        assert_eq!(pluralize(0, "file", "files"), "files");
        assert_eq!(pluralize(2, "file", "files"), "files");
        assert_eq!(count_noun(1, "env file", "env files"), "1 env file");
        assert_eq!(count_noun(0, "env file", "env files"), "0 env files");
    }

    #[test]
    fn plain_presentation_emits_no_ansi() {
        let pres = Presentation::plain();
        assert_eq!(paint("hello", Role::Limited, &pres), "hello");
        assert_eq!(chip(Verdict::Preparing, &pres), "◐ PREPARING");
        assert!(!rule(Role::Label, &pres).contains('\u{1b}'));
    }

    #[test]
    fn json_presentation_is_plain_ascii() {
        let pres = Presentation::detect(true);
        assert!(!pres.color);
        assert!(!pres.unicode);
        assert_eq!(chip(Verdict::Limited, &pres), "x LIMITED");
    }

    #[test]
    fn relative_time_humanizes() {
        let base = "2026-07-01T12:00:00Z";
        assert_eq!(
            relative_time("2026-07-01T11:59:40Z", base).as_deref(),
            Some("just now")
        );
        assert_eq!(
            relative_time("2026-07-01T11:57:00Z", base).as_deref(),
            Some("3m ago")
        );
        assert_eq!(
            relative_time("2026-07-01T09:00:00Z", base).as_deref(),
            Some("3h ago")
        );
        assert_eq!(relative_time("not-a-time", base), None);
    }
}
