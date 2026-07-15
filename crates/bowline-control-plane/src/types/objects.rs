use std::fmt;

use bowline_core::ids::{ContentId, DeviceId, ManifestId, SnapshotId, WorkspaceId};

use crate::{ControlPlaneTimestamp, ObjectKind, ObjectPointer};

pub type ByteRange = bowline_storage::ByteRange;

pub const CURRENT_SNAPSHOT_AUTHORITY_FORMAT_VERSION: u16 = 2;

#[derive(Clone, PartialEq, Eq)]
pub struct SignedUrlIntent {
    pub url: String,
    pub expires_at: ControlPlaneTimestamp,
}

impl fmt::Debug for SignedUrlIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedUrlIntent")
            .field("url", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIntentRequest {
    pub workspace_id: WorkspaceId,
    pub object_kind: ObjectKind,
    pub byte_len: u64,
    pub content_id: Option<ContentId>,
    pub object_key: Option<String>,
}

impl UploadIntentRequest {
    pub fn new(workspace_id: impl Into<String>, object_kind: ObjectKind, byte_len: u64) -> Self {
        Self {
            workspace_id: WorkspaceId::new(workspace_id),
            object_kind,
            byte_len,
            content_id: None,
            object_key: None,
        }
    }

    pub fn with_content_id(mut self, content_id: impl Into<String>) -> Self {
        self.content_id = Some(ContentId::new(content_id));
        self
    }

    pub fn with_object_key(mut self, object_key: impl Into<String>) -> Self {
        self.object_key = Some(object_key.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIntent {
    pub workspace_id: WorkspaceId,
    pub object_key: String,
    pub object_kind: ObjectKind,
    pub byte_len: u64,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadVerificationIntentRequest {
    pub workspace_id: WorkspaceId,
    pub object_key: String,
    pub byte_len: u64,
    pub content_id: Option<ContentId>,
}

impl UploadVerificationIntentRequest {
    pub fn new(
        workspace_id: impl Into<String>,
        object_key: impl Into<String>,
        byte_len: u64,
    ) -> Self {
        Self {
            workspace_id: WorkspaceId::new(workspace_id),
            object_key: object_key.into(),
            byte_len,
            content_id: None,
        }
    }

    pub fn with_content_id(mut self, content_id: impl Into<String>) -> Self {
        self.content_id = Some(ContentId::new(content_id));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadIntentRequest {
    pub workspace_id: WorkspaceId,
    pub object_key: String,
    pub range: Option<ByteRange>,
}

impl DownloadIntentRequest {
    pub fn full(workspace_id: impl Into<String>, object_key: impl Into<String>) -> Self {
        Self {
            workspace_id: WorkspaceId::new(workspace_id),
            object_key: object_key.into(),
            range: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadIntent {
    pub workspace_id: WorkspaceId,
    pub object_key: String,
    pub range: Option<ByteRange>,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRetentionStateUpdate {
    pub workspace_id: WorkspaceId,
    pub object_key: String,
    pub retention_state: bowline_storage::RetentionState,
}

impl ObjectRetentionStateUpdate {
    pub fn new(
        workspace_id: impl Into<String>,
        object_key: impl Into<String>,
        retention_state: bowline_storage::RetentionState,
    ) -> Self {
        Self {
            workspace_id: WorkspaceId::new(workspace_id),
            object_key: object_key.into(),
            retention_state,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteIntent {
    pub workspace_id: WorkspaceId,
    pub object_key: String,
    pub object_kind: ObjectKind,
    pub key_epoch: u32,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataRecordKind {
    NamespacePage,
    ContentLayout,
    SegmentPage,
}

impl MetadataRecordKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NamespacePage => "namespace-page",
            Self::ContentLayout => "content-layout",
            Self::SegmentPage => "segment-page",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataSidecar {
    pub child_logical_ids: Vec<String>,
    pub direct_object_keys: Vec<String>,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBindingInput {
    pub logical_id: String,
    pub record_kind: MetadataRecordKind,
    pub object: ObjectPointer,
    pub sidecar: MetadataSidecar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBindingCommit {
    pub workspace_id: WorkspaceId,
    pub bindings: Vec<MetadataBindingInput>,
    pub committed_by_device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadataCommit {
    pub workspace_id: WorkspaceId,
    pub object: ObjectPointer,
    pub committed_by_device_id: DeviceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataBindingOutcome {
    BoundNew,
    ExistingWinner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBindingRecord {
    pub logical_id: String,
    pub record_kind: MetadataRecordKind,
    pub object: ObjectPointer,
    pub sidecar: MetadataSidecar,
    pub outcome: Option<MetadataBindingOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBindingBatch {
    pub workspace_id: WorkspaceId,
    pub bindings: Vec<MetadataBindingRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRootCommit {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub manifest_id: ManifestId,
    pub manifest_object: ObjectPointer,
    pub namespace_root_id: String,
    pub extra_root_logical_ids: Vec<String>,
    pub committed_by_device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRootRecord {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub manifest_id: ManifestId,
    pub manifest_object: ObjectPointer,
    pub namespace_root_id: String,
    pub extra_root_logical_ids: Vec<String>,
    pub complete: bool,
    pub committed_by_device_id: DeviceId,
    pub committed_at: ControlPlaneTimestamp,
}
