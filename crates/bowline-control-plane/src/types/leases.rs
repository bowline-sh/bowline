use bowline_core::ids::{DeviceId, LeaseId, ProjectId, SnapshotId, WorkViewId, WorkspaceId};

use crate::{CompactEventKind, ControlPlaneTimestamp};

// Slim cross-device handoff / session record. The supervisor dispatch/output
// states were removed with the agent-supervisor stack: a handoff lease is simply
// one whose `target_device_ref` names a trusted host that materializes the
// workspace on arrival. `status_code` carries the non-supervisor handoff marker
// (e.g. "pending" then "handoff-materialized").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub lease_id: LeaseId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub device_id: DeviceId,
    pub target_device_ref: Option<String>,
    pub origin_device_ref: Option<String>,
    pub write_target_mode: LeaseWriteTargetMode,
    pub work_view_id: Option<WorkViewId>,
    pub base_snapshot_id: SnapshotId,
    pub task_label: Option<String>,
    pub version: u64,
    pub session_state: LeaseSessionState,
    pub status_code: String,
    pub created_at: ControlPlaneTimestamp,
    pub updated_at: ControlPlaneTimestamp,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseSessionState {
    Provisional,
    Open,
    Completed,
}

impl LeaseSessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Open => "open",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseWriteTargetMode {
    Direct,
    WorkView,
}

impl LeaseWriteTargetMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::WorkView => "work-view",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseCreate {
    pub workspace_id: WorkspaceId,
    pub lease_id: LeaseId,
    pub project_id: ProjectId,
    pub device_id: DeviceId,
    pub target_device_ref: Option<String>,
    pub origin_device_ref: Option<String>,
    pub write_target_mode: LeaseWriteTargetMode,
    pub work_view_id: Option<WorkViewId>,
    pub base_snapshot_id: SnapshotId,
    pub task_label: Option<String>,
    pub session_state: LeaseSessionState,
    pub status_code: String,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseUpdate {
    pub workspace_id: WorkspaceId,
    pub lease_id: LeaseId,
    pub expected_version: u64,
    pub updated_by_device_id: DeviceId,
    pub session_state: Option<LeaseSessionState>,
    pub status_code: Option<String>,
    pub event_kind: Option<CompactEventKind>,
}
