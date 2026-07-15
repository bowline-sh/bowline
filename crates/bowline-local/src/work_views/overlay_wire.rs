use bowline_core::{
    ids::{ContentId, SnapshotId, WorkViewId},
    workspace_graph::{
        ContentLocator, ContentStorage, FileExecutability, normalize_workspace_path,
    },
};
use serde::{Deserialize, Serialize};

pub(super) const OVERLAY_FORMAT_VERSION: u16 = 2;
pub(super) const OVERLAY_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_OVERLAY_ENCRYPTED_CHUNK_BYTES: u64 = OVERLAY_CHUNK_BYTES as u64 + 64 * 1024;
pub(super) const MAX_OVERLAY_ENTRIES: usize = 100_000;
pub(super) const MAX_OVERLAY_MANIFEST_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OverlayManifest {
    pub(super) format_version: u16,
    pub(super) work_view_id: WorkViewId,
    pub(super) base_snapshot_id: SnapshotId,
    pub(super) entries: Vec<OverlayManifestEntry>,
}

impl OverlayManifest {
    pub(super) fn new(
        work_view_id: WorkViewId,
        base_snapshot_id: SnapshotId,
        entries: Vec<OverlayManifestEntry>,
    ) -> Result<Self, OverlayWireError> {
        if entries.len() > MAX_OVERLAY_ENTRIES {
            return Err(OverlayWireError::EntryLimitExceeded);
        }
        let manifest = Self {
            format_version: OVERLAY_FORMAT_VERSION,
            work_view_id,
            base_snapshot_id,
            entries,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>, OverlayWireError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_OVERLAY_MANIFEST_BYTES {
            return Err(OverlayWireError::ManifestLimitExceeded);
        }
        Ok(bytes)
    }

    pub(super) fn operations(&self) -> &[OverlayManifestEntry] {
        &self.entries
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, OverlayWireError> {
        if bytes.len() > MAX_OVERLAY_MANIFEST_BYTES {
            return Err(OverlayWireError::ManifestLimitExceeded);
        }
        let manifest = serde_json::from_slice::<Self>(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), OverlayWireError> {
        if self.format_version != OVERLAY_FORMAT_VERSION {
            return Err(OverlayWireError::UnsupportedVersion(self.format_version));
        }
        if self.entries.len() > MAX_OVERLAY_ENTRIES {
            return Err(OverlayWireError::EntryLimitExceeded);
        }
        let mut previous = None::<&str>;
        for entry in &self.entries {
            let normalized = normalize_workspace_path(&entry.path);
            if normalized.is_empty()
                || normalized != entry.path
                || normalized.starts_with('/')
                || normalized.split('/').any(|component| component == "..")
            {
                return Err(OverlayWireError::UnsafePath);
            }
            if previous.is_some_and(|path| path >= entry.path.as_str()) {
                return Err(OverlayWireError::NonCanonicalEntries);
            }
            entry.validate()?;
            previous = Some(&entry.path);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OverlayManifestEntry {
    pub(super) path: String,
    pub(super) operation: OverlayOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) from: Option<String>,
    pub(super) contains_secrets: bool,
    pub(super) executability: FileExecutability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) content: Option<OverlayContent>,
}

impl OverlayManifestEntry {
    fn validate(&self) -> Result<(), OverlayWireError> {
        let needs_content = matches!(
            self.operation,
            OverlayOperation::Create | OverlayOperation::Modify | OverlayOperation::Rename
        );
        if needs_content != self.content.is_some() {
            return Err(OverlayWireError::InvalidEntry);
        }
        if (self.operation == OverlayOperation::Rename) != self.from.is_some() {
            return Err(OverlayWireError::InvalidEntry);
        }
        if let Some(from) = &self.from {
            let normalized = normalize_workspace_path(from);
            if normalized.is_empty()
                || normalized != *from
                || normalized.starts_with('/')
                || normalized.split('/').any(|component| component == "..")
            {
                return Err(OverlayWireError::UnsafePath);
            }
            if from == &self.path {
                return Err(OverlayWireError::InvalidEntry);
            }
        }
        if let Some(content) = &self.content {
            content.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum OverlayOperation {
    Create,
    Modify,
    Delete,
    Rename,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OverlayContent {
    pub(super) content_id: ContentId,
    pub(super) byte_len: u64,
    pub(super) chunks: Vec<OverlayContentChunk>,
}

impl OverlayContent {
    fn validate(&self) -> Result<(), OverlayWireError> {
        if self.byte_len == 0 && !self.chunks.is_empty() {
            return Err(OverlayWireError::InvalidContentLayout);
        }
        let mut total = 0_u64;
        for (ordinal, chunk) in self.chunks.iter().enumerate() {
            let locator_range_is_bounded = match (
                chunk.locator.offset,
                chunk.locator.length,
                chunk.locator.pack_id.as_ref(),
            ) {
                (Some(offset), Some(length), Some(_)) => {
                    length > 0
                        && length <= MAX_OVERLAY_ENCRYPTED_CHUNK_BYTES
                        && offset.checked_add(length).is_some()
                }
                _ => false,
            };
            if chunk.ordinal != ordinal as u32
                || chunk.plaintext_len == 0
                || chunk.plaintext_len > OVERLAY_CHUNK_BYTES as u64
                || chunk.locator.storage != ContentStorage::Packed
                || !locator_range_is_bounded
                || chunk.locator.content_id != chunk.content_id
                || chunk.locator.raw_size != chunk.plaintext_len
            {
                return Err(OverlayWireError::InvalidContentLayout);
            }
            total = total
                .checked_add(chunk.plaintext_len)
                .ok_or(OverlayWireError::InvalidContentLayout)?;
        }
        if total != self.byte_len {
            return Err(OverlayWireError::InvalidContentLayout);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OverlayContentChunk {
    pub(super) ordinal: u32,
    pub(super) content_id: ContentId,
    pub(super) plaintext_len: u64,
    pub(super) object_key: String,
    pub(super) key_epoch: u32,
    pub(super) locator: ContentLocator,
}

#[derive(Debug)]
pub enum OverlayWireError {
    Json(serde_json::Error),
    UnsupportedVersion(u16),
    EntryLimitExceeded,
    ManifestLimitExceeded,
    UnsafePath,
    NonCanonicalEntries,
    InvalidEntry,
    InvalidContentLayout,
}

impl std::fmt::Display for OverlayWireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "overlay manifest JSON is invalid: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "overlay manifest format {version} is unsupported"
                )
            }
            Self::EntryLimitExceeded => write!(formatter, "overlay manifest entry limit exceeded"),
            Self::ManifestLimitExceeded => {
                write!(formatter, "overlay manifest byte limit exceeded")
            }
            Self::UnsafePath => write!(formatter, "overlay manifest contains an unsafe path"),
            Self::NonCanonicalEntries => {
                write!(
                    formatter,
                    "overlay manifest entries are not uniquely sorted"
                )
            }
            Self::InvalidEntry => write!(formatter, "overlay manifest entry is inconsistent"),
            Self::InvalidContentLayout => write!(formatter, "overlay content layout is invalid"),
        }
    }
}

impl std::error::Error for OverlayWireError {}

impl From<serde_json::Error> for OverlayWireError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        ids::PackId,
        workspace_graph::{ContentLocator, ContentStorage},
    };

    use super::*;

    #[test]
    fn v2_manifest_round_trips_without_inline_file_bytes() {
        let content_id = ContentId::new("cid_chunk");
        let manifest = OverlayManifest::new(
            WorkViewId::new("work_one"),
            SnapshotId::new("snap_base"),
            vec![OverlayManifestEntry {
                path: "src/main.rs".to_string(),
                operation: OverlayOperation::Modify,
                from: None,
                contains_secrets: false,
                executability: FileExecutability::Regular,
                content: Some(OverlayContent {
                    content_id: ContentId::new("cid_file"),
                    byte_len: 3,
                    chunks: vec![OverlayContentChunk {
                        ordinal: 0,
                        content_id: content_id.clone(),
                        plaintext_len: 3,
                        object_key: "packs_one".to_string(),
                        key_epoch: 1,
                        locator: ContentLocator {
                            content_id,
                            storage: ContentStorage::Packed,
                            raw_size: 3,
                            pack_id: Some(PackId::new("pk_one")),
                            offset: Some(1),
                            length: Some(2),
                        },
                    }],
                }),
            }],
        )
        .expect("manifest");
        let bytes = manifest.encode().expect("encode");
        let json = String::from_utf8(bytes.clone()).expect("utf8");

        assert!(!json.contains("\"bytes\""));
        assert_eq!(OverlayManifest::decode(&bytes).expect("decode"), manifest);
    }

    #[test]
    fn decoder_rejects_legacy_inline_overlay_authority() {
        let legacy =
            br#"{"schemaVersion":1,"workViewId":"work","baseSnapshotId":"snap","entries":[]}"#;
        assert!(OverlayManifest::decode(legacy).is_err());
    }

    #[test]
    fn validator_rejects_self_rename() {
        let manifest = OverlayManifest {
            format_version: OVERLAY_FORMAT_VERSION,
            work_view_id: WorkViewId::new("work_one"),
            base_snapshot_id: SnapshotId::new("snap_base"),
            entries: vec![OverlayManifestEntry {
                path: "src/main.rs".to_string(),
                operation: OverlayOperation::Rename,
                from: Some("src/main.rs".to_string()),
                contains_secrets: false,
                executability: FileExecutability::Regular,
                content: Some(OverlayContent {
                    content_id: ContentId::new("cid_empty"),
                    byte_len: 0,
                    chunks: Vec::new(),
                }),
            }],
        };

        assert!(matches!(
            manifest.encode(),
            Err(OverlayWireError::InvalidEntry)
        ));
    }

    #[test]
    fn validator_rejects_unbounded_encrypted_chunk_range() {
        let content_id = ContentId::new("cid_chunk");
        let manifest = OverlayManifest {
            format_version: OVERLAY_FORMAT_VERSION,
            work_view_id: WorkViewId::new("work_one"),
            base_snapshot_id: SnapshotId::new("snap_base"),
            entries: vec![OverlayManifestEntry {
                path: "large.bin".to_string(),
                operation: OverlayOperation::Create,
                from: None,
                contains_secrets: false,
                executability: FileExecutability::Regular,
                content: Some(OverlayContent {
                    content_id: ContentId::new("cid_file"),
                    byte_len: 1,
                    chunks: vec![OverlayContentChunk {
                        ordinal: 0,
                        content_id: content_id.clone(),
                        plaintext_len: 1,
                        object_key: "packs_one".to_string(),
                        key_epoch: 1,
                        locator: ContentLocator {
                            content_id,
                            storage: ContentStorage::Packed,
                            raw_size: 1,
                            pack_id: Some(PackId::new("pk_one")),
                            offset: Some(u64::MAX - 1),
                            length: Some(MAX_OVERLAY_ENCRYPTED_CHUNK_BYTES),
                        },
                    }],
                }),
            }],
        };

        assert!(matches!(
            manifest.encode(),
            Err(OverlayWireError::InvalidContentLayout)
        ));
    }

    #[test]
    fn entry_bound_accepts_one_hundred_thousand_and_rejects_the_next() {
        let entry = |ordinal: usize| OverlayManifestEntry {
            path: format!("files/{ordinal:06}.deleted"),
            operation: OverlayOperation::Delete,
            from: None,
            contains_secrets: false,
            executability: FileExecutability::Regular,
            content: None,
        };
        let entries = (0..MAX_OVERLAY_ENTRIES).map(entry).collect::<Vec<_>>();
        let manifest = OverlayManifest::new(
            WorkViewId::new("work_scale"),
            SnapshotId::new("snap_scale"),
            entries.clone(),
        )
        .expect("maximum entry count");
        assert!(manifest.encode().expect("bounded manifest").len() < MAX_OVERLAY_MANIFEST_BYTES);

        let mut over_limit = entries;
        over_limit.push(entry(MAX_OVERLAY_ENTRIES));
        assert!(matches!(
            OverlayManifest::new(
                WorkViewId::new("work_scale"),
                SnapshotId::new("snap_scale"),
                over_limit,
            ),
            Err(OverlayWireError::EntryLimitExceeded)
        ));
    }

    #[test]
    fn ten_gibibyte_layout_is_represented_by_bounded_segments() {
        const TEN_GIB: u64 = 10 * 1024 * 1024 * 1024;
        let chunk_count = TEN_GIB.div_ceil(OVERLAY_CHUNK_BYTES as u64);
        let chunks = (0..chunk_count)
            .map(|ordinal| {
                let content_id = ContentId::new(format!("cid_segment_{ordinal:04}"));
                OverlayContentChunk {
                    ordinal: u32::try_from(ordinal).expect("bounded chunk ordinal"),
                    content_id: content_id.clone(),
                    plaintext_len: OVERLAY_CHUNK_BYTES as u64,
                    object_key: format!("overlay_segment_{ordinal:04}"),
                    key_epoch: 1,
                    locator: ContentLocator {
                        content_id,
                        storage: ContentStorage::Packed,
                        raw_size: OVERLAY_CHUNK_BYTES as u64,
                        pack_id: Some(PackId::new(format!("pack_{ordinal:04}"))),
                        offset: Some(64),
                        length: Some(OVERLAY_CHUNK_BYTES as u64 + 1024),
                    },
                }
            })
            .collect::<Vec<_>>();
        let content = OverlayContent {
            content_id: ContentId::new("cid_ten_gib"),
            byte_len: TEN_GIB,
            chunks,
        };

        content.validate().expect("bounded 10 GiB layout");
        assert_eq!(content.chunks.len(), 2_560);
        assert!(
            content
                .chunks
                .iter()
                .all(|chunk| chunk.plaintext_len <= OVERLAY_CHUNK_BYTES as u64)
        );
    }
}
