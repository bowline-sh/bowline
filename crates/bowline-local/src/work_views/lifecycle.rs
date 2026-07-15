use std::{fs, io};

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkLifecycleCommandOutput},
    events::EventName,
    status::{RepairCommand, WorkspaceStatus},
    work_views::{
        WorkCommandAction, WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    },
};

use crate::metadata::MetadataStore;

use super::{
    WorkSelectorOptions, WorkViewError,
    paths::{
        append_work_event, ensure_no_symlink_ancestors, ensure_path_inside, expand_display_path,
        open_store, resolve_work_view, work_namespace_root,
    },
    status_all_command,
};

#[cfg(test)]
use super::{
    accept_operation::resolve_accept_paths,
    advance_partial_exposed_base_from_live_tree,
    snapshot_accept::{SnapshotAcceptOutcome, accept_snapshot},
};

#[cfg(test)]
pub(crate) fn accept_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let mut work_view = resolve_work_view(&store, &options.selector)?;
    if !matches!(
        work_view.lifecycle,
        WorkViewLifecycle::Active | WorkViewLifecycle::ReviewReady
    ) {
        return Err(WorkViewError::InactiveWorkView {
            name: work_view.name,
        });
    }
    let partial = !options.paths.is_empty();
    let accepted_paths = resolve_accept_paths(&store, &work_view, &options.paths)?;
    let cache_root = options
        .db_path
        .as_deref()
        .and_then(std::path::Path::parent)
        .map(|state_root| state_root.join("cache"));
    let conflicts =
        match accept_snapshot(&store, &work_view, &accepted_paths, cache_root.as_deref())? {
            SnapshotAcceptOutcome::Clean => Vec::new(),
            SnapshotAcceptOutcome::Conflicted(conflicts) => conflicts,
            SnapshotAcceptOutcome::PolicyDrift(records) => records
                .into_iter()
                .map(|record| format!("policy-drift:{}", record.reason.code()))
                .collect(),
        };
    if !conflicts.is_empty() {
        work_view.lifecycle = WorkViewLifecycle::ReviewReady;
        work_view.sync_state = WorkViewSyncState::Attention;
        work_view.attention = conflicts
            .iter()
            .map(|path| format!("Manual review needed before accepting {path}."))
            .collect();
        work_view.updated_at = options.generated_at.clone();
        store.upsert_work_view(&work_view)?;
        append_work_event(
            &store,
            EventName::WorkReviewReady,
            &work_view,
            &options.generated_at,
        );
        let next_actions = vec![RepairCommand::inspect(
            "Inspect work-view diff".to_string(),
            Some(format!("bowline work review {}", options.selector)),
        )];
        return Ok(WorkLifecycleCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Accept,
            generated_at: options.generated_at,
            action: WorkCommandAction::ReviewReady,
            paths: Vec::new(),
            partial: false,
            work_view,
            status: WorkspaceStatus {
                level: bowline_core::status::StatusLevel::Attention,
                attention_items: vec![
                    "Accept needs review before touching the main view.".to_string(),
                ],
            },
            next_actions,
        });
    }

    if partial {
        work_view = advance_partial_exposed_base_from_live_tree(
            &store,
            &work_view,
            &accepted_paths,
            &options.generated_at,
        )?;
        return Ok(WorkLifecycleCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Accept,
            generated_at: options.generated_at,
            action: WorkCommandAction::Accepted,
            paths: accepted_paths.into_iter().collect(),
            partial: true,
            work_view,
            status: WorkspaceStatus::healthy(),
            next_actions: vec![RepairCommand::inspect(
                "Review remaining work-view changes".to_string(),
                Some(format!("bowline work review {}", options.selector)),
            )],
        });
    }

    work_view.lifecycle = WorkViewLifecycle::Accepted;
    work_view.visibility = WorkViewVisibility::Hidden;
    work_view.sync_state = WorkViewSyncState::Synced;
    work_view.attention.clear();
    work_view.retention = WorkViewRetention {
        state: WorkViewRetentionState::Retained,
        retain_until: None,
        restorable: true,
    };
    work_view.updated_at = options.generated_at.clone();
    store.upsert_work_view(&work_view)?;
    append_work_event(
        &store,
        EventName::WorkAccepted,
        &work_view,
        &options.generated_at,
    );
    let status_command = status_all_command(&store, &work_view.workspace_id)?;
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Accept,
        generated_at: options.generated_at,
        action: WorkCommandAction::Accepted,
        paths: Vec::new(),
        partial: false,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![RepairCommand::inspect(
            "Inspect workspace status".to_string(),
            Some(status_command),
        )],
    })
}

pub fn discard_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    transition_work_view_with_store(
        store,
        work_view,
        options.generated_at,
        WorkViewTransition {
            command: CommandName::Discard,
            action: WorkCommandAction::Discarded,
            lifecycle: WorkViewLifecycle::Discarded,
            visibility: WorkViewVisibility::Hidden,
            retention: WorkViewRetention {
                state: WorkViewRetentionState::Retained,
                retain_until: None,
                restorable: true,
            },
            event_name: EventName::WorkDiscarded,
            reject_active_accept: true,
        },
    )
}

pub fn restore_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    ensure_restorable_work_view(&work_view)?;
    ensure_restorable_materialization(&store, &work_view)?;
    transition_work_view_with_store(
        store,
        work_view,
        options.generated_at,
        WorkViewTransition {
            command: CommandName::Restore,
            action: WorkCommandAction::Restored,
            lifecycle: WorkViewLifecycle::Active,
            visibility: WorkViewVisibility::DefaultVisible,
            retention: WorkViewRetention {
                state: WorkViewRetentionState::Current,
                retain_until: None,
                restorable: false,
            },
            event_name: EventName::WorkRestored,
            reject_active_accept: false,
        },
    )
}

struct WorkViewTransition {
    command: CommandName,
    action: WorkCommandAction,
    lifecycle: WorkViewLifecycle,
    visibility: WorkViewVisibility,
    retention: WorkViewRetention,
    event_name: EventName,
    reject_active_accept: bool,
}

fn transition_work_view_with_store(
    store: MetadataStore,
    mut work_view: WorkView,
    generated_at: String,
    transition: WorkViewTransition,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    work_view.lifecycle = transition.lifecycle;
    work_view.visibility = transition.visibility;
    work_view.sync_state = WorkViewSyncState::LocalOnly;
    work_view.attention.clear();
    work_view.retention = transition.retention;
    work_view.updated_at = generated_at.clone();
    if transition.reject_active_accept {
        if let Some(operation) = store.upsert_work_view_unless_accept_active(&work_view)? {
            return Err(WorkViewError::AcceptOperationPending {
                operation_id: operation.id,
                state: operation.state,
            });
        }
    } else {
        store.upsert_work_view(&work_view)?;
    }
    append_work_event(&store, transition.event_name, &work_view, &generated_at);
    let status_command = status_all_command(&store, &work_view.workspace_id)?;
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: transition.command,
        generated_at,
        action: transition.action,
        paths: Vec::new(),
        partial: false,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![RepairCommand::inspect(
            "List work views".to_string(),
            Some(status_command),
        )],
    })
}
fn ensure_restorable_work_view(work_view: &WorkView) -> Result<(), WorkViewError> {
    if work_view.retention.restorable
        && matches!(work_view.retention.state, WorkViewRetentionState::Retained)
    {
        return Ok(());
    }
    Err(WorkViewError::UnrestorableWorkView {
        name: work_view.name.clone(),
    })
}

fn ensure_restorable_materialization(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<(), WorkViewError> {
    let work_root = expand_display_path(&work_view.visible_path);
    let namespace_root =
        work_namespace_root(store, work_view)?.ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    ensure_path_inside(
        &work_root,
        &namespace_root,
        "work view must live under .work",
    )?;
    ensure_no_symlink_ancestors(
        &namespace_root,
        &workspace_root,
        "work view namespace escapes .work",
    )?;
    ensure_no_symlink_ancestors(&work_root, &namespace_root, "work view root escapes .work")?;
    match fs::symlink_metadata(&work_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(WorkViewError::UnsafeWorkViewPath {
            path: work_root.display().to_string(),
            reason: "work view materialization path already exists",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&work_root)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
