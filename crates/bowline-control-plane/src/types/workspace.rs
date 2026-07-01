use crate::{ControlPlaneTimestamp, WorkViewRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub workspace_id: String,
    pub version: u64,
    pub snapshot_id: String,
    pub updated_at: ControlPlaneTimestamp,
    pub updated_by_device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleWorkspaceRef {
    pub expected_version: u64,
    pub current: WorkspaceRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleWorkViewOverlayHead {
    pub expected_overlay_version: u64,
    pub current: WorkViewRecord,
}
