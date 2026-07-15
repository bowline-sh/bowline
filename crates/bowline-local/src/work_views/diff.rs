use std::{
    collections::BTreeMap,
    path::{Component, Path},
};

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkDiffCommandOutput},
    status::RepairCommand,
    work_views::{WorkCommandAction, WorkDiffEntry, WorkView},
};

use crate::glob::{MAX_GLOB_MATCH_BYTES, glob_matches};
use crate::metadata::MetadataStore;

use super::{
    WorkSelectorOptions, WorkViewError, overlay,
    paths::{
        ensure_no_symlink_ancestors, ensure_path_inside, expand_display_path, open_store,
        resolve_work_view, status_for_changes, work_namespace_root,
    },
    shell_word,
};

pub fn diff_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkDiffCommandOutput, WorkViewError> {
    diff_work_view_with_checkpoint(options, || true)
}

pub fn diff_work_view_with_checkpoint(
    options: WorkSelectorOptions,
    mut checkpoint: impl FnMut() -> bool,
) -> Result<WorkDiffCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    let changes = diff_entries(&store, &work_view, &options.paths, &mut checkpoint)?;
    let status = status_for_changes(&changes);
    let next_actions = vec![RepairCommand::mutating(
        "Accept work view".to_string(),
        Some(accept_command(&options.selector, &options.paths)),
    )];
    Ok(WorkDiffCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Diff,
        generated_at: options.generated_at,
        action: WorkCommandAction::Diffed,
        work_view,
        changes,
        status,
        next_actions,
    })
}

fn accept_command(selector: &str, paths: &[String]) -> String {
    let mut command = format!("bowline work accept {}", shell_word(selector));
    for path in paths {
        command.push_str(" --path ");
        command.push_str(&shell_word(path));
    }
    command
}

fn diff_entries(
    store: &MetadataStore,
    work_view: &WorkView,
    path_patterns: &[String],
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<Vec<WorkDiffEntry>, WorkViewError> {
    let deltas =
        selected_overlay_deltas_with_checkpoint(store, work_view, path_patterns, checkpoint)?;
    if deltas.is_empty() {
        return Ok(Vec::new());
    }
    Ok(overlay::diff_entries_from_deltas(work_view, &deltas))
}

pub(super) fn selected_overlay_deltas(
    store: &MetadataStore,
    work_view: &WorkView,
    path_patterns: &[String],
) -> Result<Vec<overlay::OverlayDelta>, WorkViewError> {
    selected_overlay_deltas_with_checkpoint(store, work_view, path_patterns, &mut || true)
}

fn selected_overlay_deltas_with_checkpoint(
    store: &MetadataStore,
    work_view: &WorkView,
    path_patterns: &[String],
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<Vec<overlay::OverlayDelta>, WorkViewError> {
    let deltas = unpublished_overlay_deltas(store, work_view, checkpoint)?;
    if path_patterns.is_empty() {
        return Ok(deltas);
    }
    let selectors = normalize_path_selectors(path_patterns)?;
    let selected = deltas
        .into_iter()
        .filter(|delta| delta_matches_selectors(delta, &selectors))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(WorkViewError::EmptyPathSelection {
            patterns: selectors,
        });
    }
    Ok(selected)
}

fn normalize_path_selectors(patterns: &[String]) -> Result<Vec<String>, WorkViewError> {
    let mut selectors = Vec::new();
    for pattern in patterns {
        if pattern.len() > MAX_GLOB_MATCH_BYTES {
            return Err(WorkViewError::InvalidPathSelector {
                selector: pattern.clone(),
                reason: format!("must be at most {MAX_GLOB_MATCH_BYTES} bytes"),
            });
        }
        let normalized = bowline_core::workspace_graph::normalize_workspace_path(pattern);
        if normalized.is_empty() || Path::new(&normalized).is_absolute() {
            return Err(WorkViewError::InvalidPathSelector {
                selector: pattern.clone(),
                reason: "must be a project-relative path or glob".to_string(),
            });
        }
        if Path::new(&normalized)
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(WorkViewError::InvalidPathSelector {
                selector: pattern.clone(),
                reason: "must not contain parent-directory components".to_string(),
            });
        }
        selectors.push(normalized);
    }
    selectors.sort();
    selectors.dedup();
    Ok(selectors)
}

fn delta_matches_selectors(delta: &overlay::OverlayDelta, selectors: &[String]) -> bool {
    let path = normalized_path(&delta.path);
    let rename_from = match &delta.kind {
        overlay::OverlayDeltaKind::Rename { from } if !from.as_os_str().is_empty() => {
            Some(normalized_path(from))
        }
        _ => None,
    };
    selectors.iter().any(|selector| {
        selector_matches_path(selector, &path)
            || rename_from
                .as_ref()
                .is_some_and(|from| selector_matches_path(selector, from))
    })
}

fn selector_matches_path(selector: &str, path: &str) -> bool {
    selector == path || glob_matches(selector, path)
}

fn normalized_path(path: &Path) -> String {
    bowline_core::workspace_graph::normalize_workspace_path(&path.display().to_string())
}

fn unpublished_overlay_deltas(
    store: &MetadataStore,
    work_view: &WorkView,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<Vec<overlay::OverlayDelta>, WorkViewError> {
    let mut deltas_by_path = overlay::logged_overlay_deltas(store, work_view)?
        .into_iter()
        .map(|delta| (delta.path.clone(), delta))
        .collect::<BTreeMap<_, _>>();
    for delta in filesystem_overlay_deltas_with_checkpoint(store, work_view, checkpoint)? {
        if matches!(&delta.kind, overlay::OverlayDeltaKind::Delete)
            || !deltas_by_path.contains_key(&delta.path)
        {
            deltas_by_path.insert(delta.path.clone(), delta);
        }
    }
    Ok(deltas_by_path.into_values().collect())
}

fn filesystem_overlay_deltas_with_checkpoint(
    store: &MetadataStore,
    work_view: &WorkView,
    checkpoint: &mut dyn FnMut() -> bool,
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
    overlay::filesystem_overlay_deltas_with_checkpoint(store, work_view, &work_root, checkpoint)
}
