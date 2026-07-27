//! Projecting unreconciled conflict-asides into status.
//!
//! The engine preserves a divergence as an ordinary file beside the one it
//! conflicts with, and that file's presence *is* the unresolved state — there is
//! no conflict record to read. An aside also syncs, so a device that received one
//! never ran the pull that made it and has no event for it either. Status
//! therefore asks the workspace directly.

use crate::conflicts::{
    ConflictAside, ConflictError, ProjectScope, in_project_scope, list_conflicts,
};

use super::*;

/// Cap on how many conflicts get their own status item. Every conflict still
/// counts toward the attention summary and the fact set; only the per-path
/// itemization is bounded, exactly as blocked paths are.
const MAX_CONFLICT_ITEMS: usize = 20;

pub(super) struct ConflictReport {
    pub(super) asides: Vec<ConflictAside>,
    /// Why the scan could not answer, when it could not.
    unavailable: Option<ConflictError>,
}

impl ConflictReport {
    /// Paths a conflict signal may be keyed by: the aside itself, and the file it
    /// sits beside. Legacy conflict events name one or the other, so both count
    /// as "still unresolved" while the aside is on disk.
    pub(super) fn unresolved_paths(&self) -> BTreeSet<String> {
        self.asides
            .iter()
            .flat_map(|conflict| {
                [
                    conflict.aside.as_str().to_string(),
                    conflict.origin.as_str().to_string(),
                ]
            })
            .collect()
    }
}

/// Read every unreconciled conflict under `workspace_root`, narrowed to
/// `scope` when status is project-scoped.
pub(super) fn observe_conflicts(
    workspace_root: Option<&str>,
    scope: Option<&ProjectScope>,
) -> ConflictReport {
    let Some(workspace_root) = workspace_root.map(std::path::Path::new) else {
        return ConflictReport {
            asides: Vec::new(),
            unavailable: None,
        };
    };
    match list_conflicts(workspace_root) {
        Ok(asides) => ConflictReport {
            asides: asides
                .into_iter()
                .filter(|conflict| in_project_scope(conflict, scope))
                .collect(),
            unavailable: None,
        },
        Err(error) => ConflictReport {
            asides: Vec::new(),
            unavailable: Some(error),
        },
    }
}

pub(super) fn apply_conflict_asides(
    report: &ConflictReport,
    workspace_id: &WorkspaceId,
    workspace_root_label: &str,
    acc: &mut StatusAccumulator,
) {
    if let Some(error) = &report.unavailable {
        acc.limits.push(LimitedCapability {
            capability: "conflict-detection".to_string(),
            support_capability: None,
            unavailable_because: error.to_string(),
            still_works: vec![
                "Sync, status, and every other command continue.".to_string(),
                "Conflict-aside files remain on disk and are safe to reconcile by hand."
                    .to_string(),
            ],
            path: None,
        });
        return;
    }
    if report.asides.is_empty() {
        return;
    }

    for conflict in &report.asides {
        acc.observe_fact(
            "sync.conflict_unresolved",
            format!("conflict-aside:{}", conflict.aside.as_str()),
            format!("conflict:path:{}", conflict.origin.as_str()),
            StatusFactScope::Path,
            Some(conflict.origin.as_str()),
        );
    }
    for conflict in report.asides.iter().take(MAX_CONFLICT_ITEMS) {
        acc.items.push(conflict_item(conflict));
    }
    if report.asides.len() > MAX_CONFLICT_ITEMS {
        let additional = report.asides.len() - MAX_CONFLICT_ITEMS;
        let mut item = base_status_item(
            StatusItemKind::Conflict,
            &format!(
                "{} more unreconciled; run `bowline conflicts` for the full list.",
                plural_phrase(additional as u64, "conflict", "conflicts"),
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        acc.items.push(item);
    }

    acc.attention_items.push(format!(
        "{} unreconciled: {}.",
        plural_phrase(report.asides.len() as u64, "conflict", "conflicts"),
        report
            .asides
            .iter()
            .take(3)
            .map(|conflict| conflict.origin.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    ));
    acc.next_actions
        .push(list_conflicts_action(workspace_root_label));
}

fn conflict_item(conflict: &ConflictAside) -> StatusItem {
    let summary = if conflict.origin_missing {
        format!(
            "The incoming version of {} is preserved at {}; the file itself is gone locally.",
            conflict.origin.as_str(),
            conflict.aside.as_str(),
        )
    } else {
        format!(
            "{} kept your version; the incoming one is preserved beside it at {}.",
            conflict.origin.as_str(),
            conflict.aside.as_str(),
        )
    };
    let mut item = base_status_item(StatusItemKind::Conflict, &summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Conflict,
        id: conflict.aside.as_str().to_string(),
        path: Some(conflict.origin.as_str().to_string()),
    });
    item.path = Some(conflict.aside.as_str().to_string());
    item.event_name = Some(EventName::ConflictCreated);
    item
}

pub(super) fn list_conflicts_action(root: &str) -> RepairCommand {
    // Listing is read-only; `bowline resolve` is the mutation, and it is one
    // command away from here with the exact paths already printed.
    RepairCommand::inspect(
        "Review unreconciled conflicts".to_string(),
        Some(format!("bowline conflicts --root {}", shell_word(root))),
    )
}
