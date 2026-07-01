use std::collections::BTreeSet;

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkDiffCommandOutput},
    status::SafeAction,
    work_views::{WorkCommandAction, WorkDiffEntry, WorkView},
};

use crate::metadata::MetadataStore;

use super::{
    WorkSelectorOptions, WorkViewError, overlay,
    paths::{
        ensure_no_symlink_ancestors, ensure_path_inside, expand_display_path, open_store,
        resolve_work_view, status_for_changes, work_namespace_root,
    },
};

pub fn diff_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkDiffCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    let changes = diff_entries(&store, &work_view)?;
    let status = status_for_changes(&changes);
    Ok(WorkDiffCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Diff,
        generated_at: options.generated_at,
        action: WorkCommandAction::Diffed,
        work_view,
        changes,
        status,
        next_actions: vec![SafeAction {
            label: "Accept work view".to_string(),
            command: Some(format!("bowline accept {}", options.selector)),
        }],
    })
}

fn diff_entries(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<WorkDiffEntry>, WorkViewError> {
    let mut deltas = overlay::logged_overlay_deltas(store, work_view)?;
    let logged_paths = deltas
        .iter()
        .map(|delta| delta.path.clone())
        .collect::<BTreeSet<_>>();
    deltas.extend(
        filesystem_overlay_deltas(store, work_view)?
            .into_iter()
            .filter(|delta| !logged_paths.contains(&delta.path)),
    );
    if deltas.is_empty() {
        return Ok(Vec::new());
    }
    Ok(overlay::diff_entries_from_deltas(work_view, &deltas))
}

pub(super) fn filesystem_overlay_deltas(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<overlay::OverlayDelta>, WorkViewError> {
    let work_root = expand_display_path(&work_view.visible_path);
    let Some(namespace_root) = work_namespace_root(store, work_view)? else {
        return Ok(Vec::new());
    };
    ensure_path_inside(
        &work_root,
        &namespace_root,
        "work view must live under .work",
    )?;
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    ensure_no_symlink_ancestors(
        &namespace_root,
        &workspace_root,
        "work view namespace escapes .work",
    )?;
    ensure_no_symlink_ancestors(&work_root, &namespace_root, "work view root escapes .work")?;
    overlay::filesystem_overlay_deltas(store, work_view, &work_root)
}
