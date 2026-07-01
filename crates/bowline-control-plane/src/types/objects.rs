use std::fmt;

use crate::{ControlPlaneTimestamp, ObjectKind, ObjectPointer};

pub type ByteRange = bowline_storage::ByteRange;

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
    pub workspace_id: String,
    pub object_kind: ObjectKind,
    pub byte_len: u64,
    pub content_id: Option<String>,
    pub object_key: Option<String>,
}

impl UploadIntentRequest {
    pub fn new(workspace_id: impl Into<String>, object_kind: ObjectKind, byte_len: u64) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_kind,
            byte_len,
            content_id: None,
            object_key: None,
        }
    }

    pub fn with_content_id(mut self, content_id: impl Into<String>) -> Self {
        self.content_id = Some(content_id.into());
        self
    }

    pub fn with_object_key(mut self, object_key: impl Into<String>) -> Self {
        self.object_key = Some(object_key.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIntent {
    pub workspace_id: String,
    pub object_key: String,
    pub object_kind: ObjectKind,
    pub byte_len: u64,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadVerificationIntentRequest {
    pub workspace_id: String,
    pub object_key: String,
    pub byte_len: u64,
    pub content_id: Option<String>,
}

impl UploadVerificationIntentRequest {
    pub fn new(
        workspace_id: impl Into<String>,
        object_key: impl Into<String>,
        byte_len: u64,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            byte_len,
            content_id: None,
        }
    }

    pub fn with_content_id(mut self, content_id: impl Into<String>) -> Self {
        self.content_id = Some(content_id.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadIntentRequest {
    pub workspace_id: String,
    pub object_key: String,
    pub range: Option<ByteRange>,
}

impl DownloadIntentRequest {
    pub fn full(workspace_id: impl Into<String>, object_key: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            range: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadIntent {
    pub workspace_id: String,
    pub object_key: String,
    pub range: Option<ByteRange>,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRetentionStateUpdate {
    pub workspace_id: String,
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
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            retention_state,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteIntentRequest {
    pub workspace_id: String,
    pub object_key: String,
    pub object_kind: Option<ObjectKind>,
    pub key_epoch: Option<u32>,
}

impl DeleteIntentRequest {
    pub fn new(workspace_id: impl Into<String>, object_key: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            object_kind: None,
            key_epoch: None,
        }
    }

    pub fn with_object_kind(mut self, object_kind: ObjectKind) -> Self {
        self.object_kind = Some(object_kind);
        self
    }

    pub fn with_key_epoch(mut self, key_epoch: u32) -> Self {
        self.key_epoch = Some(key_epoch);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteIntent {
    pub workspace_id: String,
    pub object_key: String,
    pub object_kind: ObjectKind,
    pub key_epoch: u32,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectManifestCommit {
    pub workspace_id: String,
    pub snapshot_id: String,
    pub manifest_id: String,
    pub manifest_object: ObjectPointer,
    pub pack_objects: Vec<ObjectPointer>,
    pub committed_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadataCommit {
    pub workspace_id: String,
    pub object: ObjectPointer,
    pub committed_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectManifestRecord {
    pub workspace_id: String,
    pub snapshot_id: String,
    pub manifest_id: String,
    pub manifest_object: ObjectPointer,
    pub pack_objects: Vec<ObjectPointer>,
    pub committed_by_device_id: String,
    pub committed_at: ControlPlaneTimestamp,
}
