use bowline_core::ids::{DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId};

use crate::ControlPlaneTimestamp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub workspace_id: WorkspaceId,
    pub version: u64,
    /// The manifest-backed head key. `None` on a version-0 genesis ref (the
    /// workspace exists but has no head yet); `Some` on every real head
    /// (version >= 1).
    pub snapshot_id: Option<SnapshotId>,
    pub updated_at: ControlPlaneTimestamp,
    pub updated_by_device_id: Option<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRefHistoryRecord {
    pub workspace_id: WorkspaceId,
    pub version: u64,
    /// `None` for the genesis advance (version 1): there is no prior head to
    /// restore to.
    pub base_snapshot_id: Option<SnapshotId>,
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
