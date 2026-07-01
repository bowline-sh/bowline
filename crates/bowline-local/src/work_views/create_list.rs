use std::fs;

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkListCommandOutput, WorkonCommandOutput},
    events::EventName,
    status::{SafeAction, WorkspaceStatus},
    work_views::{
        WorkCommandAction, WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    },
};

use crate::metadata::MetadataStore;

use super::{
    WorkListOptions, WorkViewError, WorkonOptions, materialize,
    paths::{
        append_work_event, collect_work_view_base_files, display_path,
        ensure_fresh_materialization_path, ensure_no_symlink_ancestors, expand_display_path,
        main_project_root, open_store, project_has_pending_local_writes,
        remove_materialization_tree, status_for_work_views, validate_work_view_name, visible_path,
        work_view_id,
    },
};

pub fn create_work_view(options: WorkonOptions) -> Result<WorkonCommandOutput, WorkViewError> {
    validate_work_view_name(&options.name)?;
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let root = store
        .current_workspace_root()?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let project = store
        .current_project_by_path(&options.project_path)?
        .ok_or_else(|| WorkViewError::MissingProject {
            path: options.project_path.clone(),
        })?;
    if !store
        .work_views_by_name(&workspace.id, Some(&project.id), &options.name)?
        .is_empty()
    {
        return Err(WorkViewError::NameCollision {
            name: options.name,
            project_path: project.path,
        });
    }
    let visible_path = visible_path(&root, &project.path, &options.name);
    ensure_no_symlink_ancestors(
        &visible_path,
        &expand_display_path(&root),
        "work view materialization escapes workspace",
    )?;
    let base_snapshot_id = store
        .project_latest_snapshot_id(&workspace.id, &project.id)?
        .ok_or_else(|| WorkViewError::MissingBaseSnapshot {
            path: project.path.clone(),
        })?;
    if project_has_pending_local_writes(&store, &workspace.id, &project.id, &project.path)? {
        return Err(WorkViewError::DirtyProject { path: project.path });
    }
    let work_view = WorkView {
        id: work_view_id(workspace.id.as_str(), project.id.as_str(), &options.name),
        workspace_id: workspace.id.clone(),
        project_id: project.id,
        project_path: project.path,
        name: options.name,
        visible_path: display_path(&visible_path),
        base_snapshot_id,
        overlay_head: "overlay_empty".to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: WorkViewLifecycle::Active,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: false,
        },
        owner_device_id: options.owner_device_id,
        followed_by: Vec::new(),
        host_materializations: vec![display_path(&visible_path)],
        attention: Vec::new(),
        created_at: options.generated_at.clone(),
        updated_at: options.generated_at.clone(),
    };
    let base_files = collect_work_view_base_files(&store, &work_view)?;
    ensure_fresh_materialization_path(&visible_path)?;
    fs::create_dir_all(&visible_path)?;
    if let Some(main_root) = main_project_root(&store, &work_view)? {
        materialize::materialize_base_files(&main_root, &visible_path)?;
    }
    let metadata_result =
        persist_new_work_view(&store, &work_view, &base_files, &options.generated_at);
    if let Err(error) = metadata_result {
        remove_materialization_tree(&visible_path);
        return Err(error);
    }
    append_work_event(
        &store,
        EventName::WorkCreated,
        &work_view,
        &options.generated_at,
    );
    Ok(WorkonCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Workon,
        generated_at: options.generated_at,
        action: WorkCommandAction::Created,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "Open the work view".to_string(),
            command: Some("cd .work/<project>/<name>".to_string()),
        }],
    })
}

fn persist_new_work_view(
    store: &MetadataStore,
    work_view: &WorkView,
    base_files: &[(String, String)],
    captured_at: &str,
) -> Result<(), WorkViewError> {
    store.upsert_work_view(work_view)?;
    store.replace_work_view_base_files(
        &work_view.workspace_id,
        &work_view.id,
        base_files,
        captured_at,
    )?;
    Ok(())
}

pub fn list_work_views(options: WorkListOptions) -> Result<WorkListCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let work_views = store.work_views(
        &workspace.id,
        options.include_hidden,
        options.current_device_id.as_ref(),
    )?;
    let status = status_for_work_views(&work_views);
    Ok(WorkListCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Work,
        generated_at: options.generated_at,
        action: WorkCommandAction::Listed,
        workspace_id: workspace.id,
        work_views,
        include_hidden: options.include_hidden,
        status,
        next_actions: vec![SafeAction {
            label: "Start a work view".to_string(),
            command: Some("bowline workon <name>".to_string()),
        }],
    })
}
