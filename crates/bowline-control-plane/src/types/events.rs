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

/// Every kind the hosted event log can emit. The hosted decoder maps the
/// generated wire enum onto this one with a total match, so a new server kind
/// is a compile error at contract-generation time rather than a runtime failure
/// that poisons the whole event listing.
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
    WorkspaceStatusPublished,
    WorkspaceKeyRotated,
    WorkspaceKeySeeded,
    WorkspaceKeyRegrantOffered,
    WorkspaceKeyRegrantSettled,
    MemberInvited,
    MemberJoined,
    MemberRemoved,
    NamespaceArchived,
    NamespaceArchiveRestored,
    NamespacePurgePending,
    NamespacePurgeCancelled,
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
            Self::WorkspaceStatusPublished => "workspace_status.published",
            Self::WorkspaceKeyRotated => "workspace_key.rotated",
            Self::WorkspaceKeySeeded => "workspace_key.seeded",
            Self::WorkspaceKeyRegrantOffered => "workspace_key.regrant_offered",
            Self::WorkspaceKeyRegrantSettled => "workspace_key.regrant_settled",
            Self::MemberInvited => "member.invited",
            Self::MemberJoined => "member.joined",
            Self::MemberRemoved => "member.removed",
            Self::NamespaceArchived => "namespace.archived",
            Self::NamespaceArchiveRestored => "namespace.archive_restored",
            Self::NamespacePurgePending => "namespace.purge_pending",
            Self::NamespacePurgeCancelled => "namespace.purge_cancelled",
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
