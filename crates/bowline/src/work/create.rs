//! Work-view creation: fork the current synced head into a new view directory,
//! registering the git project on first use so `work create <path>` succeeds
//! against any repo under the workspace root.

use std::collections::BTreeSet;
use std::path::PathBuf;

use bowline_core::commands::{CONTRACT_VERSION, CommandName, WorkCreateCommandOutput};
use bowline_core::events::EventName;
use bowline_core::ids::{DeviceId, SnapshotId};
use bowline_core::status::{RepairCommand, WorkspaceStatus};
use bowline_core::work_views::{
    OVERLAY_HEAD_EMPTY, WorkCommandAction, WorkView, WorkViewLifecycle as WireLifecycle,
    WorkViewRetention, WorkViewRetentionState, WorkViewSyncState, WorkViewVisibility,
};
use bowline_local::metadata::{MetadataStore, ProjectRecord};
use bowline_local::scanner::scan_workspace_scoped;
use bowline_local::sync::manifest_engine::aux_index::{
    WorkViewId as AuxWorkViewId, WorkViewLifecycle as AuxLifecycle, WorkViewRecord,
};
use bowline_local::sync::manifest_engine::manifest::ManifestKey;
use bowline_local::work_views::{
    WorkViewError, append_work_event, display_path, expand_display_path, open_store,
    overlay_aux_engine_truth, reconcile_aux_work_views, validate_work_view_name, visible_path,
    work_view_id,
};

use super::{AuxState, WorkCommandError, WorkCreateArgs, WorkCreateRpcResult, call_work_rpc};

pub fn run_work_create(
    args: WorkCreateArgs,
    db_path: Option<PathBuf>,
    owner_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkCreateCommandOutput, WorkCommandError> {
    validate_work_view_name(&args.name)?;
    // Base selection died with the snapshot model: a view always forks from the
    // current synced head (the manifest CAS ref). An explicit `--from` selector
    // has nothing to resolve against.
    if let Some(selector) = args.from {
        return Err(WorkViewError::UnknownBaseSnapshot { selector }.into());
    }
    let store = open_store(db_path.as_deref())?;
    let workspace = store
        .current_workspace()
        .map_err(WorkViewError::from)?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let root = store
        .current_workspace_root()
        .map_err(WorkViewError::from)?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let project = resolve_or_register_git_project(
        &store,
        &workspace.id,
        &root,
        &args.project_path,
        &generated_at,
    )?;
    reconcile_aux_work_views(&store)?;

    let existing = store
        .work_views_by_name(&workspace.id, Some(&project.id), &args.name)
        .map_err(WorkViewError::from)?;
    if let [row] = existing.as_slice() {
        if matches!(
            row.lifecycle,
            WireLifecycle::Active | WireLifecycle::ReviewReady
        ) {
            let mut row = row.clone();
            let visible = expand_display_path(&row.visible_path);
            if !visible.is_dir() {
                let aux_state = AuxState::load(&store)?;
                let record = aux_state.record(&row, &args.name)?;
                let _: WorkCreateRpcResult = call_work_rpc(
                    "create",
                    "work.create",
                    serde_json::json!({
                        "viewDir": visible.display().to_string(),
                        "projectPath": &project.path,
                        "overlayManifestKey": record.overlay_manifest_key.as_str(),
                    }),
                )?;
                row.host_materializations = vec![display_path(&visible)];
                store.upsert_work_view(&row).map_err(WorkViewError::from)?;
            }
            overlay_aux_engine_truth(&store, std::slice::from_mut(&mut row))?;
            return Ok(work_create_output(
                WorkCommandAction::Reused,
                row,
                generated_at,
            ));
        }
        return Err(WorkViewError::NameCollision {
            name: args.name,
            project_path: project.path,
        }
        .into());
    }

    let visible = visible_path(&root, &project.path, &args.name);
    let created: WorkCreateRpcResult = call_work_rpc(
        "create",
        "work.create",
        serde_json::json!({
            "viewDir": visible.display().to_string(),
            "projectPath": &project.path,
        }),
    )?;
    let base = created.base_manifest_key;

    let mut aux_state = AuxState::load(&store)?;
    let id = work_view_id(workspace.id.as_str(), project.id.as_str(), &args.name);
    aux_state.aux.upsert(
        AuxWorkViewId::new(id.as_str()),
        WorkViewRecord {
            project_id: project.id.clone(),
            project_path: project.path.clone(),
            name: args.name.clone(),
            owner_device_id: owner_device_id.clone(),
            created_at: generated_at.clone(),
            updated_at: generated_at.clone(),
            base_manifest_key: ManifestKey::new(base.clone()),
            overlay_manifest_key: ManifestKey::new(base.clone()),
            lifecycle: AuxLifecycle::Active,
        },
    );
    aux_state.write()?;

    let work_view = WorkView {
        id,
        workspace_id: workspace.id,
        project_id: project.id,
        project_path: project.path,
        name: args.name,
        visible_path: display_path(&visible),
        base_snapshot_id: SnapshotId::new(base),
        overlay_head: OVERLAY_HEAD_EMPTY.to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: WireLifecycle::Active,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: false,
        },
        owner_device_id: Some(owner_device_id),
        followed_by: Vec::new(),
        host_materializations: vec![display_path(&visible)],
        attention: Vec::new(),
        created_at: generated_at.clone(),
        updated_at: generated_at.clone(),
    };
    store
        .upsert_work_view(&work_view)
        .map_err(WorkViewError::from)?;
    append_work_event(&store, EventName::WorkCreated, &work_view, &generated_at);
    Ok(work_create_output(
        WorkCommandAction::Created,
        work_view,
        generated_at,
    ))
}

fn resolve_or_register_git_project(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    workspace_root: &str,
    requested_path: &str,
    generated_at: &str,
) -> Result<ProjectRecord, WorkCommandError> {
    if let Some(project) = store
        .current_project_by_path(requested_path)
        .map_err(WorkViewError::from)?
    {
        return Ok(project);
    }

    let root =
        std::fs::canonicalize(expand_display_path(workspace_root)).map_err(WorkViewError::from)?;
    let requested = std::fs::canonicalize(requested_path).map_err(WorkViewError::from)?;
    if !requested.starts_with(&root) {
        return Err(WorkViewError::MissingProject {
            path: requested_path.to_string(),
        }
        .into());
    }
    let mut candidate = if requested.is_dir() {
        requested.as_path()
    } else {
        requested.parent().unwrap_or(requested.as_path())
    };
    let git_root = loop {
        if candidate.join(".git").exists() {
            break candidate;
        }
        if candidate == root {
            return Err(WorkViewError::MissingProject {
                path: requested_path.to_string(),
            }
            .into());
        }
        candidate = candidate
            .parent()
            .filter(|parent| parent.starts_with(&root))
            .ok_or_else(|| WorkViewError::MissingProject {
                path: requested_path.to_string(),
            })?;
    };
    let relative = git_root
        .strip_prefix(&root)
        .map_err(|_| WorkViewError::MissingProject {
            path: requested_path.to_string(),
        })?;
    let relative = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let report =
        scan_workspace_scoped(&root, &BTreeSet::from([relative.clone()])).map_err(|_| {
            WorkViewError::MissingProject {
                path: requested_path.to_string(),
            }
        })?;
    let observed = report
        .projects
        .into_iter()
        .find(|project| project.path == relative && project.has_git_repo)
        .ok_or_else(|| WorkViewError::MissingProject {
            path: requested_path.to_string(),
        })?;
    let root_id = store
        .accepted_root_id_for_path(workspace_id, workspace_root)
        .map_err(WorkViewError::from)?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    store
        .insert_project(
            &observed.id,
            workspace_id,
            &root_id,
            &observed.path,
            generated_at,
        )
        .map_err(WorkViewError::from)?;
    store
        .current_project_by_path(requested_path)
        .map_err(WorkViewError::from)?
        .ok_or_else(|| {
            WorkViewError::MissingProject {
                path: requested_path.to_string(),
            }
            .into()
        })
}

fn work_create_output(
    action: WorkCommandAction,
    work_view: WorkView,
    generated_at: String,
) -> WorkCreateCommandOutput {
    let next_actions = vec![RepairCommand::inspect(
        "Open the work view".to_string(),
        Some(format!(
            "cd {}",
            bowline_core::shell::quote_word(&work_view.visible_path)
        )),
    )];
    WorkCreateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::WorkCreate,
        generated_at,
        action,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions,
    }
}
