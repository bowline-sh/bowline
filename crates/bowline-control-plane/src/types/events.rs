use std::fmt;

use bowline_core::ids::{ContentId, EventId, WorkspaceId};

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
    ObjectPointerAdded,
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
            Self::ObjectPointerAdded => "object_pointer.added",
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
    // Manifest-sync engine (Plan 110): opaque sealed objects the server stores as
    // ciphertext it cannot read. `Blob` <-> storage `WorkspaceFileV1`,
    // `Manifest` <-> storage `WorkspaceManifestV1`.
    Blob,
    Manifest,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Manifest => "manifest",
        }
    }
}

impl From<bowline_storage::ObjectKind> for ObjectKind {
    fn from(kind: bowline_storage::ObjectKind) -> Self {
        match kind {
            bowline_storage::ObjectKind::WorkspaceFileV1 => Self::Blob,
            bowline_storage::ObjectKind::WorkspaceManifestV1 => Self::Manifest,
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
