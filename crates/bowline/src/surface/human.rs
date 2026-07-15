//! Human-facing renderers for the status family, built on the shared terminal
//! design system in [`super::style`]. Every function takes a [`Presentation`]
//! and degrades to plain text when color is off, so the same code path serves
//! interactive terminals, pipes, and golden tests.

use bowline_core::commands::{StatusCommandOutput, WatchFrame};
use bowline_core::status::{
    ComponentState, FreshnessVerdict, LimitedCapability, RepairCommand, StatusAttention,
    StatusAvailability,
};

use crate::io_helpers::root_flag;

use super::style::{self, Presentation, Role, Verdict};

/// Render the one-shot `bowline status` surface: a verdict banner, a one-line
/// message, an aligned body, any items needing the user, and next actions.
pub fn render_status(output: &StatusCommandOutput, pres: &Presentation) -> String {
    let verdict = Verdict::from_output(output);
    let mut lines: Vec<String> = Vec::new();

    lines.push(banner_line(output, verdict, pres));
    lines.push(format!("  {}", style::rule(verdict.role(), pres)));
    lines.push(String::new());
    lines.push(format!("  {}", verdict_message(output, verdict)));

    let body = body_rows(output, pres);
    if !body.is_empty() {
        lines.push(String::new());
        lines.extend(body);
    }

    let bullets = attention_bullets(output, verdict, pres);
    if !bullets.is_empty() {
        lines.push(String::new());
        lines.extend(bullets);
    }

    let actions = next_action_lines(output, verdict, pres);
    if !actions.is_empty() {
        lines.push(String::new());
        lines.push(format!("  {}", style::section("Next", pres)));
        lines.extend(actions);
    }

    lines.push(String::new());
    lines.join("\n")
}

/// Render a single `--watch` frame.
pub fn render_watch_frame(frame: &WatchFrame, now: &str, pres: &Presentation) -> String {
    match frame {
        WatchFrame::Status { status, .. } => {
            let verdict = Verdict::from_output(status);
            let time = style::clock(now).unwrap_or_default();
            let word = verdict.word().to_lowercase();
            let detail: &str = status
                .status
                .attention_items
                .first()
                .map(String::as_str)
                .unwrap_or_else(|| default_detail(verdict));
            format!(
                "{time}  {} {}   {}\n",
                style::paint(verdict.glyph(pres.unicode), verdict.role(), pres),
                style::paint(&word, verdict.role(), pres),
                detail
            )
        }
        WatchFrame::Event { event, .. } => format!(
            "  {} {}: {}\n",
            style::paint("event", Role::Label, pres),
            kebab(&event.name),
            event.summary
        ),
        WatchFrame::Error { error, .. } => format!(
            "  {} {}: {}\n",
            style::paint("error", Role::Limited, pres),
            error.error.code,
            error.error.message
        ),
    }
}

// ---------------------------------------------------------------------------
// Status building blocks
// ---------------------------------------------------------------------------

fn banner_line(output: &StatusCommandOutput, verdict: Verdict, pres: &Presentation) -> String {
    let workspace = output.resolved_workspace_root.as_deref().unwrap_or("local");
    let head = format!(
        "  {} {} {}",
        style::chip(verdict, pres),
        style::paint("·", Role::Label, pres),
        style::paint(workspace, Role::Strong, pres),
    );
    match banner_meta(output, verdict) {
        Some(meta) => format!("{head}   {}", style::paint(&meta, Role::Label, pres)),
        None => head,
    }
}

fn banner_meta(output: &StatusCommandOutput, verdict: Verdict) -> Option<String> {
    let rel = output
        .event_watermarks
        .last_scan_at
        .as_deref()
        .and_then(|ts| style::relative_time(ts, &output.generated_at))?;
    Some(match verdict {
        Verdict::Ready => format!("synced {rel}"),
        _ => format!("updated {rel}"),
    })
}

fn verdict_message(output: &StatusCommandOutput, verdict: Verdict) -> String {
    match verdict {
        Verdict::Ready => "Everything is in sync.".to_string(),
        Verdict::Preparing => preparing_message(output),
        Verdict::NeedsYou => {
            let items = &output.status.attention_items;
            match items.len() {
                0 => "Something needs your attention.".to_string(),
                1 => items[0].clone(),
                n => format!("{n} things need you."),
            }
        }
        Verdict::Limited => {
            let base = output
                .status
                .attention_items
                .first()
                .cloned()
                .unwrap_or_else(|| "Some capabilities are unavailable.".to_string());
            format!("{base} Your code is safe.")
        }
    }
}

fn preparing_message(output: &StatusCommandOutput) -> String {
    let sync_not_ready = !matches!(
        output.event_watermarks.sync_state,
        Some(ComponentState::Ready)
    );
    if sync_not_ready {
        if output.event_watermarks.last_scan_at.is_none() {
            "First sync hasn't started. Nothing needs you.".to_string()
        } else {
            "Catching up on sync. Nothing needs you.".to_string()
        }
    } else {
        "Preparing workspace status. Nothing needs you.".to_string()
    }
}

fn body_rows(output: &StatusCommandOutput, pres: &Presentation) -> Vec<String> {
    let mut rows = Vec::new();
    rows.push(style::kv(
        "Capability",
        availability_label(output.status_summary.availability),
        pres,
    ));
    rows.push(style::kv(
        "Action",
        attention_label(output.status_summary.attention),
        pres,
    ));
    let observed = output
        .workspace_summary
        .as_ref()
        .and_then(|summary| summary.observed.as_ref());
    if let Some(observed) = observed {
        let workspace = format!(
            "{} · {} · {}",
            style::count_noun(observed.repo_count, "repo", "repos"),
            style::count_noun(observed.workspace_sync_path_count, "file", "files"),
            style::count_noun(observed.env_file_count, "env file", "env files"),
        );
        rows.push(style::kv("Workspace", &workspace, pres));
        rows.push(style::kv("Sync", sync_label(output), pres));
    }
    rows.push(style::kv(
        "Freshness",
        freshness_label(output.freshness),
        pres,
    ));
    rows
}

fn availability_label(availability: StatusAvailability) -> &'static str {
    match availability {
        StatusAvailability::Ready => "ready",
        StatusAvailability::Degraded => "degraded",
        StatusAvailability::Unavailable => "unavailable",
    }
}

fn attention_label(attention: StatusAttention) -> &'static str {
    match attention {
        StatusAttention::None => "none requested",
        StatusAttention::Recommended => "recommended",
        StatusAttention::Required => "required",
    }
}

fn freshness_label(verdict: FreshnessVerdict) -> &'static str {
    match verdict {
        FreshnessVerdict::Current => "current",
        FreshnessVerdict::Behind => "behind",
        FreshnessVerdict::Diverged => "diverged",
        FreshnessVerdict::Unknown => "unknown",
    }
}

fn sync_label(output: &StatusCommandOutput) -> &'static str {
    match output.event_watermarks.sync_state {
        Some(ComponentState::Ready) => "up to date",
        Some(ComponentState::Degraded) => "degraded, retrying",
        Some(ComponentState::Unavailable) => "paused",
        None => "starting…",
    }
}

fn attention_bullets(
    output: &StatusCommandOutput,
    verdict: Verdict,
    pres: &Presentation,
) -> Vec<String> {
    let mut out = Vec::new();
    match verdict {
        // A single item is already the headline message; only list when there
        // are several distinct things to act on.
        Verdict::NeedsYou if output.status.attention_items.len() > 1 => {
            for item in &output.status.attention_items {
                out.push(style::bullet(Role::Attention, item, pres));
            }
        }
        Verdict::Limited => {
            for limit in &output.limits {
                out.push(limit_bullet(limit, pres));
                if !limit.still_works.is_empty() {
                    out.push(format!(
                        "      {}",
                        style::paint(
                            &format!("still works: {}", limit.still_works.join(", ")),
                            Role::Label,
                            pres,
                        )
                    ));
                }
            }
        }
        _ => {}
    }
    out
}

fn limit_bullet(limit: &LimitedCapability, pres: &Presentation) -> String {
    style::bullet(
        Role::Limited,
        &format!("{}: {}", limit.capability, limit.unavailable_because),
        pres,
    )
}

fn next_action_lines(
    output: &StatusCommandOutput,
    verdict: Verdict,
    pres: &Presentation,
) -> Vec<String> {
    let mut out: Vec<String> = output
        .next_actions
        .iter()
        .map(|action| action_line(action, pres))
        .collect();
    // Trusted local surface: render pending device-approval affordances (their
    // concrete `bowline device approve --code …` command) as runnable next actions.
    for approval in &output.device_approvals {
        out.push(style::next_action(
            &approval.approve_command,
            &format!("approve device (code {})", approval.code),
            pres,
        ));
    }
    if out.is_empty() && verdict == Verdict::Preparing {
        out.push(style::next_action(
            &status_watch_command(output),
            "follow progress live",
            pres,
        ));
    }
    out
}

fn status_watch_command(output: &StatusCommandOutput) -> String {
    format!(
        "bowline status{} --watch",
        root_flag(output.resolved_workspace_root.as_deref())
    )
}

fn action_line(action: &RepairCommand, pres: &Presentation) -> String {
    match &action.command {
        Some(command) => style::next_action(command, &action.label, pres),
        None => format!("  {}", style::paint(&action.label, Role::Label, pres)),
    }
}

fn default_detail(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Ready => "in sync",
        Verdict::Preparing => "preparing",
        Verdict::NeedsYou => "needs attention",
        Verdict::Limited => "limited",
    }
}

fn kebab(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}
