use std::fs;

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkCleanupCommandOutput},
    events::EventName,
    status::{RepairCommand, WorkspaceStatus},
    work_views::{
        WorkCommandAction, WorkViewLifecycle, WorkViewRetentionState, WorkViewVisibility,
    },
};

use super::{
    WorkCleanupOptions, WorkViewError,
    paths::{
        append_workspace_event, display_path, ensure_existing_path_inside_real,
        ensure_no_symlink_ancestors, ensure_path_inside, expand_display_path, open_store,
        work_namespace_root,
    },
    status_all_command,
};

pub fn cleanup_work_views(
    options: WorkCleanupOptions,
) -> Result<WorkCleanupCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let candidates = store
        .work_views(&workspace.id, true, None)?
        .into_iter()
        .filter(|view| {
            matches!(
                view.lifecycle,
                WorkViewLifecycle::Accepted | WorkViewLifecycle::Discarded
            ) && !matches!(view.retention.state, WorkViewRetentionState::DeleteEligible)
        })
        .collect::<Vec<_>>();
    let previewed_paths = candidates
        .iter()
        .flat_map(|view| view.host_materializations.iter().cloned())
        .collect::<Vec<_>>();
    let mut deleted_paths = Vec::new();
    if options.apply {
        for mut view in candidates {
            let namespace_root =
                work_namespace_root(&store, &view)?.ok_or(WorkViewError::MissingWorkspaceRoot)?;
            let workspace_root = expand_display_path(
                store
                    .current_workspace_root()?
                    .ok_or(WorkViewError::MissingWorkspaceRoot)?,
            );
            ensure_no_symlink_ancestors(
                &namespace_root,
                &workspace_root,
                "cleanup namespace escapes .work",
            )?;
            for path in &view.host_materializations {
                let path = expand_display_path(path);
                ensure_path_inside(&path, &namespace_root, "cleanup is limited to .work")?;
                ensure_no_symlink_ancestors(
                    &path,
                    &namespace_root,
                    "cleanup target escapes .work",
                )?;
                if path.exists() {
                    ensure_existing_path_inside_real(
                        &path,
                        &namespace_root,
                        "cleanup target escapes .work",
                    )?;
                    fs::remove_dir_all(&path)?;
                    deleted_paths.push(display_path(&path));
                }
            }
            // Cleanup is a terminal scrub, not a lifecycle transition: remove
            // materializations and mark the row delete-eligible while preserving
            // whether the work was accepted or discarded.
            view.visibility = WorkViewVisibility::Hidden;
            view.retention.state = WorkViewRetentionState::DeleteEligible;
            view.retention.retain_until = None;
            view.retention.restorable = false;
            view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&view)?;
        }
        append_workspace_event(
            &store,
            EventName::WorkCleanupCompleted,
            &workspace.id,
            &options.generated_at,
            "Cleaned up retained work views",
        );
    } else {
        append_workspace_event(
            &store,
            EventName::WorkCleanupPreviewed,
            &workspace.id,
            &options.generated_at,
            "Previewed retained work-view cleanup",
        );
    }

    let status_command = status_all_command(&store, &workspace.id)?;
    Ok(WorkCleanupCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Cleanup,
        generated_at: options.generated_at,
        action: if options.apply {
            WorkCommandAction::CleanupApplied
        } else {
            WorkCommandAction::CleanupPreviewed
        },
        workspace_id: workspace.id,
        previewed_paths,
        deleted_paths,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![RepairCommand::inspect(
            "List retained work views".to_string(),
            Some(status_command),
        )],
    })
}
