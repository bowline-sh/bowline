use crate::{CompactEventKind, ControlPlaneTimestamp, ObjectPointer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub lease_id: String,
    pub workspace_id: String,
    pub project_id: String,
    pub device_id: String,
    pub write_target_mode: LeaseWriteTargetMode,
    pub work_view_id: Option<String>,
    pub base_snapshot_id: String,
    pub version: u64,
    pub execution_state: LeaseExecutionState,
    pub output_state: LeaseOutputState,
    pub status_code: String,
    pub output_object: Option<ObjectPointer>,
    pub audit_object: Option<ObjectPointer>,
    pub created_at: ControlPlaneTimestamp,
    pub updated_at: ControlPlaneTimestamp,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseExecutionState {
    Active,
    Blocked,
    Completed,
    Expired,
    Revoked,
}

impl LeaseExecutionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOutputState {
    Empty,
    Dirty,
    ReviewReady,
    Accepted,
    Discarded,
    Conflicted,
    Retained,
}

impl LeaseOutputState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Dirty => "dirty",
            Self::ReviewReady => "review-ready",
            Self::Accepted => "accepted",
            Self::Discarded => "discarded",
            Self::Conflicted => "conflicted",
            Self::Retained => "retained",
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
    pub workspace_id: String,
    pub lease_id: String,
    pub project_id: String,
    pub device_id: String,
    pub write_target_mode: LeaseWriteTargetMode,
    pub work_view_id: Option<String>,
    pub base_snapshot_id: String,
    pub execution_state: LeaseExecutionState,
    pub output_state: LeaseOutputState,
    pub status_code: String,
    pub output_object: Option<ObjectPointer>,
    pub audit_object: Option<ObjectPointer>,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseUpdate {
    pub workspace_id: String,
    pub lease_id: String,
    pub expected_version: u64,
    pub updated_by_device_id: String,
    pub execution_state: Option<LeaseExecutionState>,
    pub output_state: Option<LeaseOutputState>,
    pub status_code: Option<String>,
    pub output_object: Option<ObjectPointer>,
    pub audit_object: Option<ObjectPointer>,
    pub event_kind: Option<CompactEventKind>,
}
