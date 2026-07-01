use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    commands::{AgentLeaseOutputState, CONTRACT_VERSION, CommandName, WorkLifecycleCommandOutput},
    events::EventName,
    status::{SafeAction, StatusLevel, WorkspaceStatus},
    work_views::{
        WorkCommandAction, WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    },
    workspace_graph::normalize_workspace_path,
};

use crate::metadata::MetadataStore;

use super::{
    WorkSelectorOptions, WorkViewError, overlay,
    paths::{
        append_work_event, clean_accept_policy, ensure_no_symlink_ancestors, ensure_path_inside,
        expand_display_path, file_content_hash, files_under, is_clean_accept_policy_eligible,
        is_ignored_clean_accept_policy, is_secret_bearing_work_path,
        is_source_control_metadata_path, main_project_root, open_store, resolve_work_view,
        work_namespace_root, work_view_base_has_path, workspace_path_for_project_file,
    },
};

pub fn accept_work_view(
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
    let conflicts = apply_clean_work_view_files(&store, &work_view)?;
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
        return Ok(WorkLifecycleCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Accept,
            generated_at: options.generated_at,
            action: WorkCommandAction::ReviewReady,
            work_view,
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec![
                    "Accept needs review before touching the main view.".to_string(),
                ],
            },
            next_actions: vec![SafeAction {
                label: "Inspect work-view diff".to_string(),
                command: Some(format!("bowline review {}", options.selector)),
            }],
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
    mark_matching_agent_leases_accepted(&store, &work_view, &options.generated_at)?;
    append_work_event(
        &store,
        EventName::WorkAccepted,
        &work_view,
        &options.generated_at,
    );
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Accept,
        generated_at: options.generated_at,
        action: WorkCommandAction::Accepted,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "Inspect workspace status".to_string(),
            command: Some("bowline status --all".to_string()),
        }],
    })
}

fn mark_matching_agent_leases_accepted(
    store: &MetadataStore,
    work_view: &WorkView,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    mark_matching_agent_leases_output_state(
        store,
        work_view,
        AgentLeaseOutputState::Accepted,
        "accepted",
        generated_at,
    )
}

fn mark_matching_agent_leases_discarded(
    store: &MetadataStore,
    work_view: &WorkView,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    mark_matching_agent_leases_output_state(
        store,
        work_view,
        AgentLeaseOutputState::Discarded,
        "discarded",
        generated_at,
    )
}

fn mark_matching_agent_leases_output_state(
    store: &MetadataStore,
    work_view: &WorkView,
    output_state: AgentLeaseOutputState,
    status_summary: &str,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    for mut lease in store.agent_leases(&work_view.workspace_id)? {
        if lease.work_view_id != work_view.id {
            continue;
        }
        if matches!(
            lease.output_state,
            AgentLeaseOutputState::Accepted | AgentLeaseOutputState::Discarded
        ) {
            continue;
        }
        lease.output_state = output_state;
        lease.status_summary = status_summary.to_string();
        lease.updated_at = generated_at.to_string();
        store.upsert_agent_lease(&lease)?;
    }
    Ok(())
}

pub fn discard_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    transition_work_view(
        options,
        CommandName::Discard,
        WorkCommandAction::Discarded,
        WorkViewLifecycle::Discarded,
        WorkViewVisibility::Hidden,
        WorkViewRetention {
            state: WorkViewRetentionState::Retained,
            retain_until: None,
            restorable: true,
        },
        EventName::WorkDiscarded,
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
        },
    )
}

fn transition_work_view(
    options: WorkSelectorOptions,
    command: CommandName,
    action: WorkCommandAction,
    lifecycle: WorkViewLifecycle,
    visibility: WorkViewVisibility,
    retention: WorkViewRetention,
    event_name: EventName,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    transition_work_view_with_store(
        store,
        work_view,
        options.generated_at,
        WorkViewTransition {
            command,
            action,
            lifecycle,
            visibility,
            retention,
            event_name,
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
    store.upsert_work_view(&work_view)?;
    if transition.action == WorkCommandAction::Discarded {
        mark_matching_agent_leases_discarded(&store, &work_view, &generated_at)?;
    }
    append_work_event(&store, transition.event_name, &work_view, &generated_at);
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: transition.command,
        generated_at,
        action: transition.action,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "List work views".to_string(),
            command: Some("bowline status --all".to_string()),
        }],
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
fn apply_clean_work_view_files(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<String>, WorkViewError> {
    let Some(main_root) = main_project_root(store, work_view)? else {
        return Ok(Vec::new());
    };
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
    if !work_root.exists() {
        return Ok(Vec::new());
    }

    let mut conflicts = overlay_review_paths(store, work_view)?;
    let deletes = work_view_deletions(store, work_view)?;
    let deleted_relative_paths = deletes.iter().cloned().collect::<BTreeSet<_>>();
    let mut deleted_files = Vec::new();
    for delete in deletes {
        let main_file = main_root.join(&delete);
        ensure_path_inside(
            &main_file,
            &main_root,
            "accepted deletions must stay inside the main project",
        )?;
        let destination_workspace_path = workspace_path_for_project_file(work_view, &delete);
        let policy_source = main_file.exists().then_some(main_file.as_path());
        let policy = clean_accept_policy(
            store,
            &workspace_root,
            &work_view.workspace_id,
            &destination_workspace_path,
            policy_source,
        )?;
        if is_ignored_clean_accept_policy(policy.classification, policy.mode) {
            continue;
        }
        if is_secret_bearing_work_path(&delete)
            || is_source_control_metadata_path(&delete)
            || !is_clean_accept_policy_eligible(policy.classification, policy.mode)
            || (main_file.exists()
                && !main_matches_work_view_base(store, work_view, &delete, &main_file)?)
        {
            conflicts.push(normalize_workspace_path(&delete.display().to_string()));
        }
        deleted_files.push(main_file);
    }

    let mut files = Vec::new();
    for file in files_under(&work_root)? {
        let relative = file
            .strip_prefix(&work_root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .to_path_buf();
        if deleted_relative_paths.contains(&relative) {
            continue;
        }
        let main_file = main_root.join(&relative);
        ensure_path_inside(
            &main_file,
            &main_root,
            "accepted files must stay inside the main project",
        )?;
        let destination_workspace_path = workspace_path_for_project_file(work_view, &relative);
        let policy = clean_accept_policy(
            store,
            &workspace_root,
            &work_view.workspace_id,
            &destination_workspace_path,
            Some(&file),
        )?;
        if is_ignored_clean_accept_policy(policy.classification, policy.mode) {
            continue;
        }
        if is_secret_bearing_work_path(&relative)
            || is_source_control_metadata_path(&relative)
            || !is_clean_accept_policy_eligible(policy.classification, policy.mode)
            || (!main_file.exists() && work_view_base_has_path(store, work_view, &relative)?)
            || (main_file.exists()
                && fs::read(&file)? != fs::read(&main_file)?
                && !main_matches_work_view_base(store, work_view, &relative, &main_file)?)
        {
            conflicts.push(normalize_workspace_path(&relative.display().to_string()));
            continue;
        }
        files.push((file, main_file));
    }
    if !conflicts.is_empty() {
        return Ok(conflicts);
    }

    for deleted_file in deleted_files {
        ensure_no_symlink_ancestors(
            &deleted_file,
            &main_root,
            "accepted deletion escapes project",
        )?;
        if let Ok(metadata) = fs::symlink_metadata(&deleted_file) {
            if metadata.file_type().is_symlink() {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: deleted_file.display().to_string(),
                    reason: "accepted deletion refuses symlink targets",
                });
            }
            if metadata.is_dir() {
                fs::remove_dir_all(&deleted_file)?;
            } else {
                fs::remove_file(&deleted_file)?;
            }
        }
    }

    for (source, destination) in files {
        if let Some(parent) = destination.parent() {
            ensure_no_symlink_ancestors(parent, &main_root, "destination parent escapes project")?;
            fs::create_dir_all(parent)?;
        }
        ensure_no_symlink_ancestors(&destination, &main_root, "destination escapes project")?;
        fs::copy(source, destination)?;
    }
    Ok(Vec::new())
}

fn overlay_review_paths(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<String>, WorkViewError> {
    Ok(overlay::logged_overlay_deltas(store, work_view)?
        .into_iter()
        .filter(|delta| delta.kind.requires_review())
        .map(|delta| normalize_workspace_path(&delta.path.display().to_string()))
        .collect())
}
fn work_view_deletions(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<PathBuf>, WorkViewError> {
    let visible_prefix = normalize_workspace_path(
        &store.workspace_relative_path(&work_view.workspace_id, &work_view.visible_path)?,
    );
    let work_root = expand_display_path(&work_view.visible_path);
    let mut final_delete_state = BTreeMap::<PathBuf, bool>::new();
    for write in store.local_write_log(&work_view.workspace_id)? {
        let path = normalize_workspace_path(
            &store.workspace_relative_path(&work_view.workspace_id, &write.path)?,
        );
        let Some(relative) = relative_to_work_view(&path, &visible_prefix) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        let relative = PathBuf::from(relative);
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        if matches!(write.operation.as_str(), "rename" | "renamed")
            && let Some(source_path) = write.source_path.as_deref()
        {
            let source_path = normalize_workspace_path(
                &store.workspace_relative_path(&work_view.workspace_id, source_path)?,
            );
            let source_relative = relative_to_work_view(&source_path, &visible_prefix)
                .filter(|relative| !relative.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(source_path));
            if !is_source_control_metadata_path(&source_relative) {
                final_delete_state.insert(source_relative, true);
            }
        }
        if matches!(write.operation.as_str(), "delete" | "deleted") {
            final_delete_state.insert(relative, true);
        } else {
            final_delete_state.insert(relative, false);
        }
    }
    for (relative, _hash) in store.work_view_base_files(&work_view.workspace_id, &work_view.id)? {
        let relative = PathBuf::from(relative);
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        if !work_root.join(&relative).exists() {
            final_delete_state.insert(relative, true);
        }
    }
    let mut deletes = final_delete_state
        .into_iter()
        .filter_map(|(path, is_deleted)| is_deleted.then_some(path))
        .collect::<Vec<_>>();
    deletes.sort();
    deletes.dedup();
    Ok(deletes)
}

fn relative_to_work_view<'a>(path: &'a str, visible_prefix: &str) -> Option<&'a str> {
    if path == visible_prefix {
        return Some("");
    }
    path.strip_prefix(visible_prefix)
        .and_then(|relative| relative.strip_prefix('/'))
}
fn main_matches_work_view_base(
    store: &MetadataStore,
    work_view: &WorkView,
    relative: &Path,
    main_file: &Path,
) -> Result<bool, WorkViewError> {
    let relative_path = normalize_workspace_path(&relative.display().to_string());
    let Some(base_hash) =
        store.work_view_base_hash(&work_view.workspace_id, &work_view.id, &relative_path)?
    else {
        return Ok(false);
    };
    if !main_file.is_file() {
        return Ok(false);
    }
    Ok(file_content_hash(main_file)? == base_hash)
}
