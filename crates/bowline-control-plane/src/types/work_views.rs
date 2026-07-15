use bowline_core::ids::{DeviceId, ProjectId, SnapshotId, WorkViewId, WorkspaceId};

use crate::{ControlPlaneTimestamp, ObjectPointer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewLifecycleState {
    Active,
    ReviewReady,
    Accepted,
    Discarded,
}

impl WorkViewLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReviewReady => "review-ready",
            Self::Accepted => "accepted",
            Self::Discarded => "discarded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewCreate {
    pub workspace_id: WorkspaceId,
    pub work_view_id: WorkViewId,
    pub project_id: ProjectId,
    pub name: String,
    pub visible_path: String,
    pub base_snapshot_id: SnapshotId,
    pub base_workspace_version: u64,
    pub expires_at: Option<String>,
    pub retain_until: Option<String>,
    pub created_by_device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewLifecycleUpdate {
    pub workspace_id: WorkspaceId,
    pub work_view_id: WorkViewId,
    pub lifecycle: WorkViewLifecycleState,
    pub updated_by_device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlayCommit {
    pub workspace_id: WorkspaceId,
    pub work_view_id: WorkViewId,
    pub expected_overlay_version: u64,
    pub overlay_object: ObjectPointer,
    pub committed_by_device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewRecord {
    pub workspace_id: WorkspaceId,
    pub work_view_id: WorkViewId,
    pub project_id: ProjectId,
    pub name: String,
    pub visible_path: String,
    pub base_snapshot_id: SnapshotId,
    pub base_workspace_version: u64,
    pub overlay_head: Option<ObjectPointer>,
    pub overlay_version: u64,
    pub lifecycle: WorkViewLifecycleState,
    pub created_by_device_id: DeviceId,
    pub updated_by_device_id: DeviceId,
    pub created_at: ControlPlaneTimestamp,
    pub updated_at: ControlPlaneTimestamp,
}
