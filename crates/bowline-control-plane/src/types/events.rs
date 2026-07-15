use std::fmt;

use bowline_core::ids::{ConflictId, ContentId, DeviceId, EventId, SnapshotId, WorkspaceId};

use crate::ControlPlaneTimestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactEvent {
    pub event_id: EventId,
    pub workspace_id: WorkspaceId,
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
    LeaseClaimed,
    LeaseCompleted,
    LeaseCreated,
    LeaseDispatched,
    LeaseReviewReady,
    LeaseUpdated,
    SnapshotRootCommitted,
    ObjectPointerAdded,
    OverlayChanged,
    WorkAccepted,
    WorkCleanupCompleted,
    WorkCleanupPreviewed,
    WorkCreated,
    WorkDiscarded,
    WorkRestored,
    WorkReviewReady,
    WorkUpdated,
    WorkspaceCreated,
    WorkspaceRefAdvanced,
}

impl CompactEventKind {
    /// Lease session lifecycle events emitted by the control-plane client.
    /// `overlay.changed` remains a valid persisted event kind, but it is not a
    /// lease session lifecycle update.
    pub const LEASE_UPDATE_EVENT_KINDS: &'static [CompactEventKind] = &[
        Self::LeaseCreated,
        Self::LeaseUpdated,
        Self::LeaseDispatched,
        Self::LeaseClaimed,
        Self::LeaseCompleted,
        Self::LeaseReviewReady,
    ];

    pub fn is_lease_update_event(self) -> bool {
        Self::LEASE_UPDATE_EVENT_KINDS.contains(&self)
    }

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
            Self::LeaseClaimed => "lease.claimed",
            Self::LeaseCompleted => "lease.completed",
            Self::LeaseCreated => "lease.created",
            Self::LeaseDispatched => "lease.dispatched",
            Self::LeaseReviewReady => "lease.review_ready",
            Self::LeaseUpdated => "lease.updated",
            Self::SnapshotRootCommitted => "snapshot_root.committed",
            Self::ObjectPointerAdded => "object_pointer.added",
            Self::OverlayChanged => "overlay.changed",
            Self::WorkAccepted => "work.accepted",
            Self::WorkCleanupCompleted => "work.cleanup_completed",
            Self::WorkCleanupPreviewed => "work.cleanup_previewed",
            Self::WorkCreated => "work.created",
            Self::WorkDiscarded => "work.discarded",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectKind {
    SourcePack,
    LocatorIndex,
    SnapshotMetadataPage,
    SnapshotManifest,
    AgentOverlay,
    ConflictBundle,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourcePack => "source-pack",
            Self::LocatorIndex => "locator-index",
            Self::SnapshotMetadataPage => "snapshot-metadata-page",
            Self::SnapshotManifest => "snapshot-manifest",
            Self::AgentOverlay => "overlay-pack",
            Self::ConflictBundle => "conflict-bundle",
        }
    }
}

impl TryFrom<bowline_storage::ObjectKind> for ObjectKind {
    type Error = bowline_storage::ByteStoreError;

    fn try_from(kind: bowline_storage::ObjectKind) -> Result<Self, Self::Error> {
        match kind {
            bowline_storage::ObjectKind::SourcePack => Ok(Self::SourcePack),
            bowline_storage::ObjectKind::SnapshotManifest => Ok(Self::SnapshotManifest),
            bowline_storage::ObjectKind::SnapshotMetadataPage => Ok(Self::SnapshotMetadataPage),
            bowline_storage::ObjectKind::LocatorIndex => Ok(Self::LocatorIndex),
            bowline_storage::ObjectKind::AgentOverlay => Ok(Self::AgentOverlay),
            bowline_storage::ObjectKind::ConflictBundle => Ok(Self::ConflictBundle),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectPointer {
    pub object_key: String,
    pub content_id: ContentId,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
    pub kind: ObjectKind,
    pub created_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictOccurrenceReconcile {
    pub workspace_id: WorkspaceId,
    pub conflict_id: ConflictId,
    pub conflict_kind: String,
    pub paths: Vec<String>,
    pub contains_secrets: bool,
    pub base_snapshot_id: SnapshotId,
    pub remote_snapshot_id: SnapshotId,
    pub occurrence_version: u64,
    pub desired_state: ConflictOccurrenceState,
    pub device_id: DeviceId,
    pub reason: String,
    pub bundle_object: Option<ObjectPointer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictOccurrenceState {
    Unresolved,
    Accepted,
    Rejected,
}

impl ConflictOccurrenceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMetadataRecord {
    pub workspace_id: WorkspaceId,
    pub conflict_id: ConflictId,
    pub conflict_kind: String,
    pub paths: Vec<String>,
    pub contains_secrets: bool,
    pub state: ConflictOccurrenceState,
    pub base_snapshot_id: SnapshotId,
    pub remote_snapshot_id: SnapshotId,
    pub occurrence_version: u64,
    pub reason: String,
    pub detected_by_device_id: DeviceId,
    pub bundle_object: Option<ObjectPointer>,
    pub detected_at: ControlPlaneTimestamp,
    pub resolved_by_device_id: Option<DeviceId>,
    pub resolved_at: Option<ControlPlaneTimestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictReconcileOutcome {
    Applied,
    Idempotent,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictReconcileResult {
    pub conflict: ConflictMetadataRecord,
    pub outcome: ConflictReconcileOutcome,
}
