use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewAcceptOperationState {
    Queued,
    Claimed,
    WaitingRetry,
    ReviewRequired,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewAcceptReviewReason {
    PolicyDrift,
    MergeConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewAcceptFailureReason {
    Transient,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewAcceptCheckpointStep {
    CandidateBuilt,
    MainFenceRechecked,
    ObjectsUploaded,
    SnapshotStaged,
    MainPublished,
    WorkspaceRefPublished,
    LifecyclePublished,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkViewAcceptResourceKey {
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    work_view_id: WorkViewId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewAcceptCandidateObservation {
    pub observed_main_snapshot_id: SnapshotId,
    pub observed_ref_version: u64,
    pub observed_ref_snapshot_id: SnapshotId,
    pub target_snapshot_id: SnapshotId,
}

impl WorkViewAcceptResourceKey {
    pub fn new(workspace_id: WorkspaceId, project_id: ProjectId, work_view_id: WorkViewId) -> Self {
        Self {
            workspace_id,
            project_id,
            work_view_id,
        }
    }

    pub fn as_string(&self) -> String {
        format!(
            "work_view_accept:{}:{}:{}",
            self.workspace_id.as_str(),
            self.project_id.as_str(),
            self.work_view_id.as_str()
        )
    }

    pub(super) fn matches(&self, record: &WorkViewAcceptOperationRecord) -> bool {
        self.workspace_id == record.workspace_id
            && self.project_id == record.project_id
            && self.work_view_id == record.work_view_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewAcceptOperationRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub work_view_id: WorkViewId,
    pub device_id: DeviceId,
    pub resource_key: WorkViewAcceptResourceKey,
    pub idempotency_key: String,
    pub state: WorkViewAcceptOperationState,
    pub selected_paths: Option<Vec<String>>,
    pub input_json: String,
    pub observed_main_snapshot_id: Option<SnapshotId>,
    pub observed_ref_version: Option<u64>,
    pub observed_ref_snapshot_id: Option<SnapshotId>,
    pub target_snapshot_id: Option<SnapshotId>,
    pub result_json: Option<String>,
    pub review_reason: Option<WorkViewAcceptReviewReason>,
    pub failure_reason: Option<WorkViewAcceptFailureReason>,
    pub cancellation_requested_at: Option<String>,
    pub last_error: Option<String>,
    pub claimed_by: Option<String>,
    pub claim_token: Option<String>,
    pub claim_generation: u64,
    pub heartbeat_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub attempt_count: u32,
    pub next_attempt_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewAcceptCheckpointRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub operation_id: String,
    pub claim_generation: u64,
    pub step: WorkViewAcceptCheckpointStep,
    pub payload_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewAcceptClaimHandle {
    pub(super) operation_id: String,
    pub(super) owner: String,
    pub(super) token: String,
    pub(super) generation: u64,
}

impl WorkViewAcceptClaimHandle {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedWorkViewAcceptOperation {
    pub operation: WorkViewAcceptOperationRecord,
    pub claim: WorkViewAcceptClaimHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewAcceptClaimCheck {
    Owned,
    CancellationRequested,
    OwnershipLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewAcceptClaimTransition {
    Applied,
    OwnershipLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewAcceptCancellationOutcome {
    Cancelled,
    Requested,
    AlreadyTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkViewAcceptEnqueueOutcome {
    Inserted(WorkViewAcceptOperationRecord),
    Existing(WorkViewAcceptOperationRecord),
}

impl WorkViewAcceptEnqueueOutcome {
    pub fn operation(&self) -> &WorkViewAcceptOperationRecord {
        match self {
            Self::Inserted(record) | Self::Existing(record) => record,
        }
    }
    pub fn inserted(&self) -> bool {
        matches!(self, Self::Inserted(_))
    }
}
