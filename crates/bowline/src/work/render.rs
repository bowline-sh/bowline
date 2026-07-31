use super::*;
use crate::surface::style::{self, Presentation, Role};

pub fn render_work_create_human(output: &WorkCreateCommandOutput) -> String {
    let pres = Presentation::detect(false);
    format!(
        "{}  {}\n{}  {}\n{}  {}\n\n",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres),
        style::section("Path", &pres),
        output.work_view.visible_path,
        style::section("State", &pres),
        style::paint("active", Role::Ready, &pres),
    )
}

pub fn render_list_human(output: &WorkListCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Work views", &pres),
        output.work_views.len()
    )];
    lines.extend(output.work_views.iter().map(|view| {
        format!(
            "  {}  {}  {}",
            style::paint(&view.name, Role::Strong, &pres),
            style::paint(&view.visible_path, Role::Label, &pres),
            style::paint(&style::kebab(&view.lifecycle), Role::Label, &pres),
        )
    }));
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_diff_human(output: &WorkDiffCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres)
    )];
    if output.changes.is_empty() {
        lines.push(format!(
            "  {}",
            style::paint("No local changes recorded.", Role::Label, &pres)
        ));
    } else {
        lines.extend(output.changes.iter().map(|change| {
            let redacted = if change.contains_secrets {
                style::paint("  (redacted)", Role::Label, &pres)
            } else {
                String::new()
            };
            format!(
                "  {} {}{redacted}",
                style::paint(&style::kebab(&change.kind), Role::Label, &pres),
                change.path,
            )
        }));
    }
    append_unresolved_human(&mut lines, &output.unresolved_paths, &pres);
    lines.push(String::new());
    lines.join("\n")
}

fn append_unresolved_human(
    lines: &mut Vec<String>,
    unresolved_paths: &[WorkUnresolvedPath],
    presentation: &Presentation,
) {
    if unresolved_paths.is_empty() {
        return;
    }
    lines.push(format!(
        "{}  these edits were not captured",
        style::section("Unresolved", presentation)
    ));
    lines.extend(unresolved_paths.iter().map(|issue| {
        format!(
            "  {}  {}",
            style::paint(&issue.path, Role::Limited, presentation),
            issue.reason
        )
    }));
}

pub fn render_lifecycle_human(run: &WorkLifecycleRunOutput) -> String {
    let output = &run.output;
    let pres = Presentation::detect(false);
    let mut text = format!(
        "{}  {}\n{}  {}\n",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres),
        style::section("State", &pres),
        style::kebab(&output.work_view.lifecycle),
    );
    if !output.discarded_deletions.is_empty() {
        text.push_str(&format!(
            "{}  the workspace edited these since the fork, so the deletion did not land\n",
            style::section("Kept (deletion skipped)", &pres),
        ));
        for path in &output.discarded_deletions {
            text.push_str(&format!("  {}\n", style::paint(path, Role::Label, &pres)));
        }
    }
    if !output.aside_refused_paths.is_empty() {
        text.push_str(&format!(
            "{}  no second copy can sit beside these, so the view's version was not kept\n",
            style::section("Kept (view version dropped)", &pres),
        ));
        for path in &output.aside_refused_paths {
            text.push_str(&format!("  {}\n", style::paint(path, Role::Label, &pres)));
        }
    }
    if !output.unresolved_paths.is_empty() {
        text.push_str(&format!(
            "{}  accepted only because every path and reason was explicitly acknowledged\n",
            style::section("Unresolved", &pres),
        ));
        for issue in &output.unresolved_paths {
            text.push_str(&format!(
                "  {}  {}\n",
                style::paint(&issue.path, Role::Limited, &pres),
                issue.reason
            ));
        }
    }
    if !run.conflict_asides.is_empty() {
        text.push_str(&format!(
            "{}  the merge could not reconcile these, so both versions were kept side by side\n",
            style::section("Conflicts", &pres),
        ));
        for path in &run.conflict_asides {
            text.push_str(&format!(
                "  {}\n",
                style::paint(path, Role::Attention, &pres)
            ));
            text.push_str(&format!(
                "{}\n",
                style::next_action(
                    &format!(
                        "bowline resolve {} --diff",
                        bowline_core::shell::quote_word(path)
                    ),
                    "compare both versions",
                    &pres,
                )
            ));
        }
    }
    text.push('\n');
    text
}

pub fn render_cleanup_human(output: &WorkCleanupCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Cleanup candidates", &pres),
        output.previewed_paths.len()
    )];
    if output.deleted_paths.is_empty() {
        lines.extend(
            output
                .previewed_paths
                .iter()
                .map(|path| format!("  {}", style::paint(path, Role::Label, &pres))),
        );
    } else {
        lines.extend(
            output
                .deleted_paths
                .iter()
                .map(|path| format!("  {} {path}", style::paint("deleted", Role::Limited, &pres))),
        );
    }
    append_unresolved_human(&mut lines, &output.unresolved_paths, &pres);
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::ids::{ProjectId, SnapshotId, WorkViewId, WorkspaceId};
    use bowline_core::work_views::{
        WorkViewLifecycle as WireLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    };

    fn accepted_run(conflict_asides: Vec<String>) -> WorkLifecycleRunOutput {
        WorkLifecycleRunOutput {
            output: WorkLifecycleCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: CommandName::Accept,
                generated_at: "2026-07-25T12:00:00Z".to_string(),
                action: WorkCommandAction::Accepted,
                paths: vec!["src/main.rs".to_string()],
                discarded_deletions: Vec::new(),
                aside_refused_paths: Vec::new(),
                unresolved_paths: Vec::new(),
                partial: false,
                work_view: WorkView {
                    id: WorkViewId::new("wv_conflicts"),
                    workspace_id: WorkspaceId::new("ws_conflicts"),
                    project_id: ProjectId::new("proj_conflicts"),
                    project_path: "~/Code/app".to_string(),
                    name: "fix-login".to_string(),
                    visible_path: "~/Code/app.work/fix-login".to_string(),
                    base_snapshot_id: SnapshotId::new("snap_base"),
                    overlay_head: "overlay_head".to_string(),
                    overlay_version: 1,
                    env_profile: "default".to_string(),
                    lifecycle: WireLifecycle::Accepted,
                    visibility: WorkViewVisibility::DefaultVisible,
                    sync_state: WorkViewSyncState::Synced,
                    retention: WorkViewRetention {
                        state: WorkViewRetentionState::Retained,
                        retain_until: None,
                        restorable: true,
                    },
                    owner_device_id: None,
                    followed_by: Vec::new(),
                    host_materializations: Vec::new(),
                    attention: Vec::new(),
                    created_at: "2026-07-25T11:00:00Z".to_string(),
                    updated_at: "2026-07-25T12:00:00Z".to_string(),
                },
                status: WorkspaceStatus::healthy(),
                next_actions: Vec::new(),
            },
            conflict_asides,
        }
    }

    #[test]
    fn accept_reports_conflict_asides_in_json() {
        let run = accepted_run(vec!["src/main.rs.bowline-conflict".to_string()]);
        let json = serde_json::to_value(run.json_payload()).expect("payload serializes");
        assert_eq!(
            json["conflictAsides"],
            serde_json::json!(["src/main.rs.bowline-conflict"])
        );
    }

    #[test]
    fn accept_without_conflicts_omits_the_field() {
        let run = accepted_run(Vec::new());
        let json = serde_json::to_value(run.json_payload()).expect("payload serializes");
        assert!(json.get("conflictAsides").is_none());
    }

    #[test]
    fn accept_human_output_names_the_conflict_files() {
        let run = accepted_run(vec!["src/main.rs.bowline-conflict".to_string()]);
        let human = render_lifecycle_human(&run);
        assert!(human.contains("src/main.rs.bowline-conflict"));
        assert!(human.contains("Conflicts"));
    }
}
