use crate::{ControlPlaneTimestamp, ObjectPointer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewLifecycleState {
    Active,
    ReviewReady,
    Accepted,
    Discarded,
    Expired,
    Archived,
}

impl WorkViewLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReviewReady => "review-ready",
            Self::Accepted => "accepted",
            Self::Discarded => "discarded",
            Self::Expired => "expired",
            Self::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewCreate {
    pub workspace_id: String,
    pub work_view_id: String,
    pub project_id: String,
    pub name: String,
    pub visible_path: String,
    pub base_snapshot_id: String,
    pub base_workspace_version: u64,
    pub created_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewLifecycleUpdate {
    pub workspace_id: String,
    pub work_view_id: String,
    pub lifecycle: WorkViewLifecycleState,
    pub updated_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlayCommit {
    pub workspace_id: String,
    pub work_view_id: String,
    pub expected_overlay_version: u64,
    pub overlay_object: ObjectPointer,
    pub committed_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewRecord {
    pub workspace_id: String,
    pub work_view_id: String,
    pub project_id: String,
    pub name: String,
    pub visible_path: String,
    pub base_snapshot_id: String,
    pub base_workspace_version: u64,
    pub overlay_head: Option<ObjectPointer>,
    pub overlay_version: u64,
    pub lifecycle: WorkViewLifecycleState,
    pub created_by_device_id: String,
    pub updated_by_device_id: String,
    pub created_at: ControlPlaneTimestamp,
    pub updated_at: ControlPlaneTimestamp,
}
