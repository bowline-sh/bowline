use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bowline_core::ids::{ContentId, DeviceId, WorkspaceId};
use sha2::{Digest, Sha256};

use crate::{ControlPlaneTimestamp, ObjectKind, ObjectPointer};

pub type ByteRange = bowline_storage::ByteRange;

pub const CURRENT_SNAPSHOT_AUTHORITY_FORMAT_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sha256Checksum(String);

impl Sha256Checksum {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self(STANDARD.encode(Sha256::digest(bytes)))
    }

    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(STANDARD.encode(digest))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
    pub checksum_sha256: Sha256Checksum,
    pub content_id: Option<ContentId>,
    pub object_key: Option<String>,
}

impl UploadIntentRequest {
    pub fn new(
        workspace_id: impl Into<String>,
        object_kind: ObjectKind,
        byte_len: u64,
        checksum_sha256: Sha256Checksum,
    ) -> Self {
        Self {
            workspace_id: WorkspaceId::new(workspace_id),
            object_kind,
            byte_len,
            checksum_sha256,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadataCommit {
    pub workspace_id: WorkspaceId,
    pub object: ObjectPointer,
    pub committed_by_device_id: DeviceId,
}
