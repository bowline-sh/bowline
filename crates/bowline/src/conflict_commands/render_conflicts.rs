use bowline_core::commands::{ConflictAction, ConflictsCommandOutput, ResolveCommandOutput};

use crate::surface::style::{self, Presentation, Role};

fn presentation() -> Presentation {
    Presentation::detect(false)
}

pub(super) fn render_conflicts_human(output: &ConflictsCommandOutput) -> String {
    let pres = presentation();
    if output.conflicts.is_empty() {
        return format!(
            "{}  {}\n",
            style::section("Conflicts", &pres),
            style::paint("none waiting", Role::Ready, &pres),
        );
    }

    let mut lines = vec![format!(
        "{}  {}",
        style::section("Conflicts", &pres),
        style::paint(
            &plural(output.conflicts.len(), "file waiting", "files waiting"),
            Role::Attention,
            &pres,
        ),
    )];
    for conflict in &output.conflicts {
        lines.push(String::new());
        lines.push(format!(
            "  {}",
            style::paint(&conflict.origin_path, Role::Strong, &pres)
        ));
        lines.push(format!(
            "    {} {}",
            style::paint("incoming", Role::Label, &pres),
            conflict.aside_path,
        ));
        if conflict.origin_missing {
            lines.push(format!(
                "    {}",
                style::paint(
                    "the file itself is gone locally; only the incoming version is left",
                    Role::Attention,
                    &pres,
                )
            ));
        }
    }
    lines.push(String::new());
    lines.push(style::section("Next", &pres));
    for action in &output.next_actions {
        match &action.command {
            Some(command) => lines.push(style::next_action(command, &action.label, &pres)),
            None => lines.push(format!("  {}", action.label)),
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

pub(super) fn render_conflicts_quiet(output: &ConflictsCommandOutput) -> String {
    crate::render::bare_values(
        output
            .conflicts
            .iter()
            .map(|conflict| conflict.aside_path.as_str()),
    )
}

pub(super) fn render_resolve_human(output: &ResolveCommandOutput) -> String {
    let pres = presentation();
    let mut lines = Vec::new();
    match output.action {
        ConflictAction::Diff => {
            lines.push(format!(
                "{}  {}",
                style::section("Compare", &pres),
                style::paint(&output.conflict.origin_path, Role::Strong, &pres),
            ));
            match (&output.diff, output.diff_unavailable) {
                (Some(diff), _) => {
                    lines.push(String::new());
                    lines.push(diff.trim_end().to_string());
                }
                // The reason is printed rather than swallowed: an empty answer
                // reads as "no differences", which a refused read is not.
                (None, Some(reason)) => lines.push(format!(
                    "  {}",
                    style::paint(reason.message(), Role::Label, &pres)
                )),
                (None, None) => lines.push(format!(
                    "  {}",
                    style::paint("no comparison was produced", Role::Label, &pres)
                )),
            }
        }
        ConflictAction::KeepLocal => lines.push(format!(
            "{}  {} {}",
            style::section("Resolved", &pres),
            style::paint("kept your version of", Role::Ready, &pres),
            output.conflict.origin_path,
        )),
        ConflictAction::TakeRemote => lines.push(format!(
            "{}  {} {}",
            style::section("Resolved", &pres),
            style::paint("adopted the incoming version of", Role::Ready, &pres),
            output.conflict.origin_path,
        )),
    }
    lines.push(String::new());
    lines.push(style::section("Next", &pres));
    for action in &output.next_actions {
        match &action.command {
            Some(command) => lines.push(style::next_action(command, &action.label, &pres)),
            None => lines.push(format!("  {}", action.label)),
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}
