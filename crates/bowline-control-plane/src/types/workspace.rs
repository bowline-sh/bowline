use bowline_core::ids::{DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId};

use crate::{ControlPlaneTimestamp, WorkViewRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub workspace_id: WorkspaceId,
    pub version: u64,
    pub snapshot_id: SnapshotId,
    pub updated_at: ControlPlaneTimestamp,
    pub updated_by_device_id: Option<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRefHistoryRecord {
    pub workspace_id: WorkspaceId,
    pub version: u64,
    pub base_snapshot_id: SnapshotId,
    pub target_snapshot_id: SnapshotId,
    pub occurred_at: String,
    pub advanced_by_device_id: Option<DeviceId>,
    pub caused_by_event_id: Option<EventId>,
    pub project_id: Option<ProjectId>,
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
