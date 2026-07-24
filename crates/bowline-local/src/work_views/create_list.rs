//! Work-view listing, rehosted onto the manifest engine. Creation is now a
//! daemon/manifest-engine operation (see `crates/bowline/src/work.rs`); the
//! surviving in-process surface is the metadata list plus the aux-index engine
//! truth projection.

use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkListCommandOutput},
    status::RepairCommand,
    work_views::{WorkCommandAction, WorkView},
};

use crate::metadata::MetadataStore;

use super::{
    WorkListOptions, WorkViewError,
    paths::{expand_display_path, open_store, reconcile_aux_work_views, status_for_work_views},
};

pub fn list_work_views(options: WorkListOptions) -> Result<WorkListCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    reconcile_aux_work_views(&store)?;
    let mut work_views = store.work_views(
        &workspace.id,
        options.include_hidden,
        options.current_device_id.as_ref(),
    )?;
    overlay_aux_engine_truth(&store, &mut work_views)?;
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
        next_actions: vec![RepairCommand::mutating(
            "Start a work view".to_string(),
            Some("bowline work create <name>".to_string()),
        )],
    })
}

/// Overlay the synced aux-index engine truth (base/overlay manifest keys +
/// lifecycle) onto metadata-registry rows. Rows without an aux record (created
/// before the manifest rewire or in metadata-seeded tests) keep their metadata
/// fields — there is no engine state to project for them.
pub fn overlay_aux_engine_truth(
    store: &MetadataStore,
    work_views: &mut [WorkView],
) -> Result<(), WorkViewError> {
    use crate::sync::manifest_engine::aux_index::WorkViewId as AuxWorkViewId;
    use crate::sync::manifest_engine::work_view_cli::{overlay_engine_truth, read_aux_index_file};

    let Some(root) = store.current_workspace_root()? else {
        return Ok(());
    };
    let aux = read_aux_index_file(&expand_display_path(&root))?;
    if aux.work_views.is_empty() {
        return Ok(());
    }
    for view in work_views {
        if let Some(record) = aux.get(&AuxWorkViewId::new(view.id.as_str())) {
            overlay_engine_truth(view, record);
        }
    }
    Ok(())
}
