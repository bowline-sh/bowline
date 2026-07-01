use std::fmt;

use crate::ControlPlaneTimestamp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactEvent {
    pub event_id: String,
    pub workspace_id: String,
    pub at: ControlPlaneTimestamp,
    pub kind: CompactEventKind,
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactEventKind {
    DeviceHarnessApproved,
    DeviceApprovalRequested,
    DeviceApproved,
    DeviceDenied,
    DeviceRevoked,
    DeviceRequested,
    RecoveryKeyCreated,
    RecoveryKeyVerified,
    RecoveryKeyRotated,
    RecoveryKeyRevoked,
    AuthLoginStarted,
    AuthLoginCompleted,
    ConflictDetected,
    ConflictResolved,
    LeaseBlocked,
    LeaseCleanupCompleted,
    LeaseCompleted,
    LeaseCreated,
    LeaseExpired,
    LeaseHydrationRequested,
    LeaseRevoked,
    LeaseReviewReady,
    LeaseToolDenied,
    LeaseToolInvoked,
    LeaseUpdated,
    ObjectManifestCommitted,
    ObjectPointerAdded,
    OverlayChanged,
    PublishRequested,
    WorkAccepted,
    WorkArchived,
    WorkCleanupCompleted,
    WorkCleanupPreviewed,
    WorkCreated,
    WorkDiscarded,
    WorkExpired,
    WorkRestored,
    WorkReviewReady,
    WorkUpdated,
    WorkspaceCreated,
    WorkspaceRefAdvanced,
}

impl CompactEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeviceHarnessApproved => "device.harness_approved",
            Self::DeviceApprovalRequested => "device.approval_requested",
            Self::DeviceApproved => "device.approved",
            Self::DeviceDenied => "device.denied",
            Self::DeviceRevoked => "device.revoked",
            Self::DeviceRequested => "device.requested",
            Self::RecoveryKeyCreated => "recovery_key.created",
            Self::RecoveryKeyVerified => "recovery_key.verified",
            Self::RecoveryKeyRotated => "recovery_key.rotated",
            Self::RecoveryKeyRevoked => "recovery_key.revoked",
            Self::AuthLoginStarted => "auth.login_started",
            Self::AuthLoginCompleted => "auth.login_completed",
            Self::ConflictDetected => "conflict.detected",
            Self::ConflictResolved => "conflict.resolved",
            Self::LeaseBlocked => "lease.blocked",
            Self::LeaseCleanupCompleted => "lease.cleanup_completed",
            Self::LeaseCompleted => "lease.completed",
            Self::LeaseCreated => "lease.created",
            Self::LeaseExpired => "lease.expired",
            Self::LeaseHydrationRequested => "lease.hydration_requested",
            Self::LeaseRevoked => "lease.revoked",
            Self::LeaseReviewReady => "lease.review_ready",
            Self::LeaseToolDenied => "lease.tool_denied",
            Self::LeaseToolInvoked => "lease.tool_invoked",
            Self::LeaseUpdated => "lease.updated",
            Self::ObjectManifestCommitted => "object_manifest.committed",
            Self::ObjectPointerAdded => "object_pointer.added",
            Self::OverlayChanged => "overlay.changed",
            Self::PublishRequested => "publish.requested",
            Self::WorkAccepted => "work.accepted",
            Self::WorkArchived => "work.archived",
            Self::WorkCleanupCompleted => "work.cleanup_completed",
            Self::WorkCleanupPreviewed => "work.cleanup_previewed",
            Self::WorkCreated => "work.created",
            Self::WorkDiscarded => "work.discarded",
            Self::WorkExpired => "work.expired",
            Self::WorkRestored => "work.restored",
            Self::WorkReviewReady => "work.review_ready",
            Self::WorkUpdated => "work.updated",
            Self::WorkspaceCreated => "workspace.created",
            Self::WorkspaceRefAdvanced => "workspace_ref.advanced",
        }
    }
}

impl fmt::Display for CompactEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    SourcePack,
    IndexPack,
    LocatorIndex,
    SnapshotManifest,
    AgentOverlay,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourcePack => "source-pack",
            Self::IndexPack => "index-pack",
            Self::LocatorIndex => "locator-index",
            Self::SnapshotManifest => "snapshot-manifest",
            Self::AgentOverlay => "overlay-pack",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectPointer {
    pub object_key: String,
    pub content_id: String,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
    pub kind: ObjectKind,
    pub created_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMetadataPublish {
    pub workspace_id: String,
    pub conflict_id: String,
    pub conflict_kind: String,
    pub paths: Vec<String>,
    pub contains_secrets: bool,
    pub base_snapshot_id: String,
    pub remote_snapshot_id: String,
    pub detected_by_device_id: String,
    pub bundle_object: Option<ObjectPointer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictResolutionMark {
    pub workspace_id: String,
    pub conflict_id: String,
    pub resolved_by_device_id: String,
    pub resolution: ConflictResolutionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolutionState {
    Accepted,
    Rejected,
}

impl ConflictResolutionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMetadataRecord {
    pub workspace_id: String,
    pub conflict_id: String,
    pub conflict_kind: String,
    pub paths: Vec<String>,
    pub contains_secrets: bool,
    pub state: String,
    pub base_snapshot_id: String,
    pub remote_snapshot_id: String,
    pub detected_by_device_id: String,
    pub bundle_object: Option<ObjectPointer>,
    pub detected_at: ControlPlaneTimestamp,
    pub resolved_by_device_id: Option<String>,
    pub resolved_at: Option<ControlPlaneTimestamp>,
}
