use std::fmt;

use bowline_core::ids::WorkspaceId;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MetadataIdentityKey([u8; 32]);

impl MetadataIdentityKey {
    pub fn derive(workspace_id: &WorkspaceId, workspace_content_key: [u8; 32]) -> Self {
        let mut material = Vec::with_capacity(8 + workspace_id.as_str().len() + 32);
        material.extend_from_slice(&(workspace_id.as_str().len() as u64).to_be_bytes());
        material.extend_from_slice(workspace_id.as_str().as_bytes());
        material.extend_from_slice(&workspace_content_key);
        Self(blake3::derive_key(
            "bowline metadata logical identity key v1",
            &material,
        ))
    }

    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for MetadataIdentityKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MetadataIdentityKey(<redacted>)")
    }
}

pub const NAMESPACE_PAGE_FORMAT_VERSION: u16 = 2;
pub const CONTENT_LAYOUT_FORMAT_VERSION: u16 = 1;
pub const SEGMENT_PAGE_FORMAT_VERSION: u16 = 1;
/// Canonical zero padding gives every namespace page the same hard floor,
/// including the root, so minimum-size exceptions cannot leak into persistence.
pub const NAMESPACE_PAGE_MIN_BYTES: usize = 512;
pub const NAMESPACE_PAGE_TARGET_BYTES: usize = 4 * 1024;
pub const NAMESPACE_PAGE_MAX_BYTES: usize = 16 * 1024;
pub const SEGMENT_PAGE_TARGET_BYTES: usize = 8 * 1024;
pub const SEGMENT_PAGE_MAX_BYTES: usize = 16 * 1024;
pub const INLINE_SEGMENT_MAX_BYTES: usize = 768;
pub const MAX_NAMESPACE_DEPTH: usize = 4_096;
pub const MAX_SEGMENTS_PER_LAYOUT: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
pub struct MetadataRecordSummary {
    pub kind: MetadataRecordKind,
    pub logical_id: String,
    pub encoded_bytes: u64,
    pub child_logical_ids: Vec<String>,
    pub direct_pack_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPlaintextRecord {
    pub summary: MetadataRecordSummary,
    pub plaintext: Vec<u8>,
}
