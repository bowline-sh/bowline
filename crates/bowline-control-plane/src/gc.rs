use bowline_core::ids::WorkspaceId;
use bowline_storage::{
    ByteStore, StorageGcDeleteFailure, StorageGcExecutionReport, execute_gc_plan, plan_gc,
};

use crate::{ControlPlaneResult, ObjectControlPlaneClient};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlaneGcSweepReport {
    pub execution: StorageGcExecutionReport,
    pub metadata_failures: Vec<StorageGcDeleteFailure>,
}

pub fn sweep_storage_gc(
    control_plane: &impl ObjectControlPlaneClient,
    workspace_id: &str,
    store: &impl ByteStore,
) -> ControlPlaneResult<ControlPlaneGcSweepReport> {
    let workspace_id = WorkspaceId::new(workspace_id);
    let latest_objects = control_plane.list_storage_gc_objects(&workspace_id)?;
    let plan = plan_gc(&latest_objects);
    let execution = execute_gc_plan(&plan, &latest_objects, store);
    let mut metadata_failures = Vec::new();

    for key in &execution.deleted {
        if let Err(error) =
            control_plane.delete_object_metadata_after_gc(&workspace_id, key.as_str())
        {
            metadata_failures.push(StorageGcDeleteFailure {
                key: key.clone(),
                reason: error.to_string(),
            });
        }
    }

    Ok(ControlPlaneGcSweepReport {
        execution,
        metadata_failures,
    })
}
