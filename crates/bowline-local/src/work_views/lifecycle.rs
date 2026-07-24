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

/// Local aliases for the manifest-engine aux-index surface, disambiguated from
/// this module's same-named wire types.
mod aux_cli {
    pub(super) use crate::sync::manifest_engine::aux_index::{
        WorkViewId as AuxWorkViewId, WorkViewLifecycle as AuxWorkViewLifecycle,
    };
    pub(super) use crate::sync::manifest_engine::manifest::ManifestKey as AuxManifestKey;
    pub(super) use crate::sync::manifest_engine::work_view_cli::{
        read_aux_index_file, write_aux_index_file,
    };
}

use super::{
    WorkSelectorOptions, WorkViewError,
    paths::{
        append_work_event, ensure_no_symlink_ancestors, ensure_path_inside, expand_display_path,
        open_store, resolve_work_view, work_namespace_root,
    },
    status_all_command,
};

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

/// Mirror a lifecycle transition into the manifest-engine aux index, the synced
/// engine truth for work views (Plan 112). The aux file may legitimately have
/// no record for a metadata-only row (created before the manifest rewire or in
/// a metadata-seeded test); there is no engine state to transition then.
fn sync_aux_lifecycle(
    store: &MetadataStore,
    work_view: &WorkView,
    target: aux_cli::AuxWorkViewLifecycle,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(());
    };
    let root = expand_display_path(&root);
    let mut aux = aux_cli::read_aux_index_file(&root)?;
    let id = aux_cli::AuxWorkViewId::new(work_view.id.as_str());
    let Some(record) = aux.work_views.get_mut(&id) else {
        return Ok(());
    };
    record.lifecycle = target;
    record.updated_at = generated_at.to_string();
    aux_cli::write_aux_index_file(&root, &aux)?;
    Ok(())
}

/// The metadata + aux updates a successful daemon-side accept applies. A full
/// accept retires the view; a partial accept advances the view's base to the
/// published head (so remaining changes diff cleanly) and keeps it active.
pub struct WorkAcceptTransition {
    pub paths: Vec<String>,
    /// Paths whose overlay deletion the workspace overrode (see
    /// [`WorkLifecycleCommandOutput::discarded_deletions`]); surfaced in the
    /// output so the user learns the deletion did not land.
    pub discarded_deletions: Vec<String>,
    pub partial: bool,
    /// The overlay key the daemon captured before merging.
    pub captured_overlay: String,
    /// Project-scoped accepted state used as the next partial-review base.
    pub accepted_base: Option<String>,
}

/// Apply the accepted-state transition after the daemon has merged + published
/// the overlay (Plan 112 rewire: the CLI owns work-view state, the daemon owns
/// the engine operation).
pub fn apply_accept_success(
    store: MetadataStore,
    mut work_view: WorkView,
    generated_at: String,
    transition: WorkAcceptTransition,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    sync_aux_accept(&store, &work_view, &transition, &generated_at)?;
    if transition.partial {
        work_view.updated_at = generated_at.clone();
        store.upsert_work_view(&work_view)?;
        append_work_event(&store, EventName::WorkAccepted, &work_view, &generated_at);
        let next_actions = vec![RepairCommand::inspect(
            "Review remaining work-view changes".to_string(),
            Some(format!("bowline work review {}", work_view.name)),
        )];
        return Ok(WorkLifecycleCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Accept,
            generated_at,
            action: WorkCommandAction::Accepted,
            paths: transition.paths,
            discarded_deletions: transition.discarded_deletions,
            partial: true,
            work_view,
            status: WorkspaceStatus::healthy(),
            next_actions,
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
    work_view.updated_at = generated_at.clone();
    store.upsert_work_view(&work_view)?;
    append_work_event(&store, EventName::WorkAccepted, &work_view, &generated_at);
    let status_command = status_all_command(&store, &work_view.workspace_id)?;
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Accept,
        generated_at,
        action: WorkCommandAction::Accepted,
        paths: Vec::new(),
        discarded_deletions: transition.discarded_deletions,
        partial: false,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![RepairCommand::inspect(
            "Inspect workspace status".to_string(),
            Some(status_command),
        )],
    })
}

/// Advance the aux record for an accept: full accept retires it; partial accept
/// re-bases it on the published head and keeps it active.
fn sync_aux_accept(
    store: &MetadataStore,
    work_view: &WorkView,
    transition: &WorkAcceptTransition,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(());
    };
    let root = expand_display_path(&root);
    let mut aux = aux_cli::read_aux_index_file(&root)?;
    let id = aux_cli::AuxWorkViewId::new(work_view.id.as_str());
    let Some(record) = aux.work_views.get_mut(&id) else {
        return Ok(());
    };
    record.overlay_manifest_key = aux_cli::AuxManifestKey::new(transition.captured_overlay.clone());
    if transition.partial {
        if let Some(base) = &transition.accepted_base {
            record.base_manifest_key = aux_cli::AuxManifestKey::new(base.clone());
        }
    } else {
        record.lifecycle = aux_cli::AuxWorkViewLifecycle::Accepted;
    }
    record.updated_at = generated_at.to_string();
    aux_cli::write_aux_index_file(&root, &aux)?;
    Ok(())
}

fn transition_work_view_with_store(
    store: MetadataStore,
    mut work_view: WorkView,
    generated_at: String,
    transition: WorkViewTransition,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    sync_aux_lifecycle(
        &store,
        &work_view,
        match transition.lifecycle {
            WorkViewLifecycle::Discarded => aux_cli::AuxWorkViewLifecycle::Discarded,
            _ => aux_cli::AuxWorkViewLifecycle::Active,
        },
        &generated_at,
    )?;
    work_view.lifecycle = transition.lifecycle;
    work_view.visibility = transition.visibility;
    work_view.sync_state = WorkViewSyncState::LocalOnly;
    work_view.attention.clear();
    work_view.retention = transition.retention;
    work_view.updated_at = generated_at.clone();
    store.upsert_work_view(&work_view)?;
    append_work_event(&store, transition.event_name, &work_view, &generated_at);
    let status_command = status_all_command(&store, &work_view.workspace_id)?;
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: transition.command,
        generated_at,
        action: transition.action,
        paths: Vec::new(),
        discarded_deletions: Vec::new(),
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
