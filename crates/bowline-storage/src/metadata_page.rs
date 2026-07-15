use std::{error::Error, fmt};

use bowline_core::ids::{ContentLayoutId, NamespacePageId, SegmentPageId, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::{
    ObjectKey, ObjectKind,
    envelope::{EnvelopeContext, EnvelopeError, StorageKey, open, seal, workspace_id_hash},
    store::stable_object_hash,
};

const METADATA_PAGE_MAGIC: &[u8; 4] = b"BWMP";
const METADATA_PAGE_HEADER_BYTES: usize = METADATA_PAGE_MAGIC.len() + 2 + 1 + 4;
pub const SNAPSHOT_METADATA_PAGE_FORMAT_VERSION: u16 = 1;
pub const SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES: usize = 16 * 1024;
pub const SNAPSHOT_METADATA_PAGE_MAX_SEALED_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotMetadataRecordKind {
    NamespacePage,
    ContentLayout,
    SegmentPage,
}

impl SnapshotMetadataRecordKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NamespacePage => "namespace-page",
            Self::ContentLayout => "content-layout",
            Self::SegmentPage => "segment-page",
        }
    }

    const fn discriminant(self) -> u8 {
        match self {
            Self::NamespacePage => 1,
            Self::ContentLayout => 2,
            Self::SegmentPage => 3,
        }
    }

    fn from_discriminant(discriminant: u8) -> Result<Self, MetadataPageError> {
        match discriminant {
            1 => Ok(Self::NamespacePage),
            2 => Ok(Self::ContentLayout),
            3 => Ok(Self::SegmentPage),
            _ => Err(MetadataPageError::InvalidEncoding(
                "unknown metadata record kind",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum SnapshotMetadataRecordId {
    NamespacePage(NamespacePageId),
    ContentLayout(ContentLayoutId),
    SegmentPage(SegmentPageId),
}

impl SnapshotMetadataRecordId {
    pub const fn kind(&self) -> SnapshotMetadataRecordKind {
        match self {
            Self::NamespacePage(_) => SnapshotMetadataRecordKind::NamespacePage,
            Self::ContentLayout(_) => SnapshotMetadataRecordKind::ContentLayout,
            Self::SegmentPage(_) => SnapshotMetadataRecordKind::SegmentPage,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::NamespacePage(id) => id.as_str(),
            Self::ContentLayout(id) => id.as_str(),
            Self::SegmentPage(id) => id.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMetadataPagePointer {
    pub logical_id: SnapshotMetadataRecordId,
    pub object_key: ObjectKey,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
    pub format_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSnapshotMetadataPage {
    pub pointer: SnapshotMetadataPagePointer,
    pub bytes: Vec<u8>,
}

pub fn seal_snapshot_metadata_page(
    workspace_id: &WorkspaceId,
    logical_id: SnapshotMetadataRecordId,
    canonical_bytes: &[u8],
    key: StorageKey,
    key_epoch: u32,
) -> Result<SealedSnapshotMetadataPage, MetadataPageError> {
    validate_logical_id(&logical_id)?;
    validate_canonical_size(canonical_bytes.len())?;
    let object_key = ObjectKey::new_snapshot_metadata_page()?;
    let plaintext = encode_metadata_page(logical_id.kind(), canonical_bytes)?;
    let context = metadata_page_context(
        workspace_id,
        &logical_id,
        &object_key,
        key_epoch,
        SNAPSHOT_METADATA_PAGE_FORMAT_VERSION,
    );
    let bytes = seal(&plaintext, key, &context)?.into_bytes();
    validate_sealed_size(bytes.len())?;
    let pointer = SnapshotMetadataPagePointer {
        logical_id,
        object_key,
        byte_len: bytes.len() as u64,
        hash: stable_object_hash(&bytes),
        key_epoch,
        format_version: SNAPSHOT_METADATA_PAGE_FORMAT_VERSION,
    };
    Ok(SealedSnapshotMetadataPage { pointer, bytes })
}

pub fn open_snapshot_metadata_page(
    sealed: &SealedSnapshotMetadataPage,
    workspace_id: &WorkspaceId,
    key: StorageKey,
) -> Result<Vec<u8>, MetadataPageError> {
    validate_pointer(sealed)?;
    validate_logical_id(&sealed.pointer.logical_id)?;
    if sealed.pointer.format_version != SNAPSHOT_METADATA_PAGE_FORMAT_VERSION {
        return Err(MetadataPageError::UnsupportedFormat {
            record: "snapshot metadata page",
            version: sealed.pointer.format_version,
        });
    }
    let context = metadata_page_context(
        workspace_id,
        &sealed.pointer.logical_id,
        &sealed.pointer.object_key,
        sealed.pointer.key_epoch,
        sealed.pointer.format_version,
    );
    let plaintext = open(&sealed.bytes, key, &context)?;
    decode_metadata_page(sealed.pointer.logical_id.kind(), &plaintext)
}

fn validate_pointer(sealed: &SealedSnapshotMetadataPage) -> Result<(), MetadataPageError> {
    validate_sealed_size(sealed.bytes.len())?;
    if sealed.pointer.byte_len != sealed.bytes.len() as u64 {
        return Err(MetadataPageError::PointerIntegrity("byte_len"));
    }
    if sealed.pointer.hash != stable_object_hash(&sealed.bytes) {
        return Err(MetadataPageError::PointerIntegrity("hash"));
    }
    if !sealed
        .pointer
        .object_key
        .as_str()
        .starts_with(ObjectKey::SNAPSHOT_METADATA_PAGE_PREFIX)
    {
        return Err(MetadataPageError::PointerIntegrity("object_key"));
    }
    Ok(())
}

fn validate_logical_id(logical_id: &SnapshotMetadataRecordId) -> Result<(), MetadataPageError> {
    let prefix = match logical_id.kind() {
        SnapshotMetadataRecordKind::NamespacePage => "nsp_",
        SnapshotMetadataRecordKind::ContentLayout => "ctl_",
        SnapshotMetadataRecordKind::SegmentPage => "sgp_",
    };
    let Some(suffix) = logical_id.as_str().strip_prefix(prefix) else {
        return Err(MetadataPageError::InvalidLogicalId(logical_id.kind()));
    };
    if suffix.len() != 64
        || !suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(MetadataPageError::InvalidLogicalId(logical_id.kind()));
    }
    Ok(())
}

fn encode_metadata_page(
    kind: SnapshotMetadataRecordKind,
    canonical_bytes: &[u8],
) -> Result<Vec<u8>, MetadataPageError> {
    validate_canonical_size(canonical_bytes.len())?;
    let encoded_len =
        u32::try_from(canonical_bytes.len()).map_err(|_| MetadataPageError::OversizedRecord {
            record: kind.as_str(),
            encoded_bytes: canonical_bytes.len() as u64,
            maximum_bytes: SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES as u64,
        })?;
    let mut plaintext = Vec::with_capacity(METADATA_PAGE_HEADER_BYTES + canonical_bytes.len());
    plaintext.extend_from_slice(METADATA_PAGE_MAGIC);
    plaintext.extend_from_slice(&SNAPSHOT_METADATA_PAGE_FORMAT_VERSION.to_le_bytes());
    plaintext.push(kind.discriminant());
    plaintext.extend_from_slice(&encoded_len.to_le_bytes());
    plaintext.extend_from_slice(canonical_bytes);
    Ok(plaintext)
}

fn decode_metadata_page(
    expected_kind: SnapshotMetadataRecordKind,
    plaintext: &[u8],
) -> Result<Vec<u8>, MetadataPageError> {
    if plaintext.len() < METADATA_PAGE_HEADER_BYTES {
        return Err(MetadataPageError::InvalidEncoding(
            "metadata page header is truncated",
        ));
    }
    if &plaintext[..METADATA_PAGE_MAGIC.len()] != METADATA_PAGE_MAGIC {
        return Err(MetadataPageError::UnsupportedFormat {
            record: "snapshot metadata page",
            version: 0,
        });
    }
    let version_offset = METADATA_PAGE_MAGIC.len();
    let version = u16::from_le_bytes([plaintext[version_offset], plaintext[version_offset + 1]]);
    if version != SNAPSHOT_METADATA_PAGE_FORMAT_VERSION {
        return Err(MetadataPageError::UnsupportedFormat {
            record: "snapshot metadata page",
            version,
        });
    }
    let kind = SnapshotMetadataRecordKind::from_discriminant(plaintext[version_offset + 2])?;
    if kind != expected_kind {
        return Err(MetadataPageError::RecordKindMismatch {
            expected: expected_kind,
            actual: kind,
        });
    }
    let length_offset = version_offset + 3;
    let encoded_len = u32::from_le_bytes([
        plaintext[length_offset],
        plaintext[length_offset + 1],
        plaintext[length_offset + 2],
        plaintext[length_offset + 3],
    ]) as usize;
    validate_canonical_size(encoded_len)?;
    let expected_len = METADATA_PAGE_HEADER_BYTES.checked_add(encoded_len).ok_or(
        MetadataPageError::InvalidEncoding("metadata page length overflowed"),
    )?;
    if plaintext.len() != expected_len {
        return Err(MetadataPageError::InvalidEncoding(
            "metadata page length did not match payload",
        ));
    }
    Ok(plaintext[METADATA_PAGE_HEADER_BYTES..].to_vec())
}

fn validate_canonical_size(encoded_bytes: usize) -> Result<(), MetadataPageError> {
    if encoded_bytes > SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES {
        return Err(MetadataPageError::OversizedRecord {
            record: "snapshot metadata page",
            encoded_bytes: encoded_bytes as u64,
            maximum_bytes: SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES as u64,
        });
    }
    Ok(())
}

fn validate_sealed_size(encoded_bytes: usize) -> Result<(), MetadataPageError> {
    if encoded_bytes > SNAPSHOT_METADATA_PAGE_MAX_SEALED_BYTES {
        return Err(MetadataPageError::OversizedRecord {
            record: "sealed snapshot metadata page",
            encoded_bytes: encoded_bytes as u64,
            maximum_bytes: SNAPSHOT_METADATA_PAGE_MAX_SEALED_BYTES as u64,
        });
    }
    Ok(())
}

fn metadata_page_context(
    workspace_id: &WorkspaceId,
    logical_id: &SnapshotMetadataRecordId,
    object_key: &ObjectKey,
    key_epoch: u32,
    format_version: u16,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
        object_kind: ObjectKind::SnapshotMetadataPage,
        object_id: object_key.as_str().to_string(),
        record_id: logical_id.as_str().to_string(),
        key_epoch,
        format_version,
    }
}

#[derive(Debug)]
pub enum MetadataPageError {
    Envelope(EnvelopeError),
    InvalidEncoding(&'static str),
    InvalidLogicalId(SnapshotMetadataRecordKind),
    ObjectKey(crate::ByteStoreError),
    OversizedRecord {
        record: &'static str,
        encoded_bytes: u64,
        maximum_bytes: u64,
    },
    PointerIntegrity(&'static str),
    RecordKindMismatch {
        expected: SnapshotMetadataRecordKind,
        actual: SnapshotMetadataRecordKind,
    },
    UnsupportedFormat {
        record: &'static str,
        version: u16,
    },
}

impl fmt::Display for MetadataPageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(error) => write!(formatter, "metadata page envelope failed: {error}"),
            Self::InvalidEncoding(reason) => {
                write!(formatter, "metadata page encoding is invalid: {reason}")
            }
            Self::InvalidLogicalId(kind) => write!(
                formatter,
                "{} logical ID is not a current opaque digest ID",
                kind.as_str()
            ),
            Self::ObjectKey(error) => write!(formatter, "metadata page object key failed: {error}"),
            Self::OversizedRecord {
                record,
                encoded_bytes,
                maximum_bytes,
            } => write!(
                formatter,
                "{record} exceeds its encoded-byte limit: {encoded_bytes} > {maximum_bytes}"
            ),
            Self::PointerIntegrity(field) => {
                write!(
                    formatter,
                    "metadata page pointer {field} did not match sealed bytes"
                )
            }
            Self::RecordKindMismatch { expected, actual } => write!(
                formatter,
                "metadata page record kind {} did not match expected {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::UnsupportedFormat { record, version } => {
                write!(formatter, "unsupported {record} format version {version}")
            }
        }
    }
}

impl Error for MetadataPageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
            Self::ObjectKey(error) => Some(error),
            Self::InvalidEncoding(_)
            | Self::InvalidLogicalId(_)
            | Self::OversizedRecord { .. }
            | Self::PointerIntegrity(_)
            | Self::RecordKindMismatch { .. }
            | Self::UnsupportedFormat { .. } => None,
        }
    }
}

impl From<EnvelopeError> for MetadataPageError {
    fn from(error: EnvelopeError) -> Self {
        Self::Envelope(error)
    }
}

impl From<crate::ByteStoreError> for MetadataPageError {
    fn from(error: crate::ByteStoreError) -> Self {
        Self::ObjectKey(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logical_id() -> SnapshotMetadataRecordId {
        SnapshotMetadataRecordId::NamespacePage(NamespacePageId::new(format!(
            "nsp_{}",
            "11".repeat(32)
        )))
    }

    #[test]
    fn metadata_page_round_trips_with_random_physical_identity() {
        let workspace_id = WorkspaceId::new("ws_code");
        let canonical = b"BWNP\x01\0canonical namespace bytes";
        let first = seal_snapshot_metadata_page(
            &workspace_id,
            logical_id(),
            canonical,
            StorageKey::deterministic(7),
            3,
        )
        .expect("first page seals");
        let retry = seal_snapshot_metadata_page(
            &workspace_id,
            logical_id(),
            canonical,
            StorageKey::deterministic(7),
            3,
        )
        .expect("retry page seals");

        assert_ne!(first.pointer.object_key, retry.pointer.object_key);
        assert_ne!(first.bytes, retry.bytes);
        assert_eq!(first.pointer.logical_id, retry.pointer.logical_id);
        assert_eq!(
            open_snapshot_metadata_page(&first, &workspace_id, StorageKey::deterministic(7))
                .expect("page opens"),
            canonical
        );
    }

    #[test]
    fn metadata_page_pointer_and_context_tampering_are_rejected() {
        let workspace_id = WorkspaceId::new("ws_code");
        let sealed = seal_snapshot_metadata_page(
            &workspace_id,
            logical_id(),
            b"canonical",
            StorageKey::deterministic(7),
            3,
        )
        .expect("page seals");

        let mut wrong_hash = sealed.clone();
        wrong_hash.pointer.hash = "b3_wrong".to_string();
        assert!(matches!(
            open_snapshot_metadata_page(&wrong_hash, &workspace_id, StorageKey::deterministic(7)),
            Err(MetadataPageError::PointerIntegrity("hash"))
        ));

        let mut wrong_logical_id = sealed;
        wrong_logical_id.pointer.logical_id = SnapshotMetadataRecordId::NamespacePage(
            NamespacePageId::new(format!("nsp_{}", "22".repeat(32))),
        );
        assert!(matches!(
            open_snapshot_metadata_page(
                &wrong_logical_id,
                &workspace_id,
                StorageKey::deterministic(7)
            ),
            Err(MetadataPageError::Envelope(_))
        ));
    }

    #[test]
    fn metadata_page_size_and_version_limits_are_typed() {
        let oversized = vec![0_u8; SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES + 1];
        assert!(matches!(
            seal_snapshot_metadata_page(
                &WorkspaceId::new("ws_code"),
                logical_id(),
                &oversized,
                StorageKey::deterministic(7),
                3,
            ),
            Err(MetadataPageError::OversizedRecord { .. })
        ));

        let mut sealed = seal_snapshot_metadata_page(
            &WorkspaceId::new("ws_code"),
            logical_id(),
            b"canonical",
            StorageKey::deterministic(7),
            3,
        )
        .expect("page seals");
        sealed.pointer.format_version = 0;
        assert!(matches!(
            open_snapshot_metadata_page(
                &sealed,
                &WorkspaceId::new("ws_code"),
                StorageKey::deterministic(7)
            ),
            Err(MetadataPageError::UnsupportedFormat {
                record: "snapshot metadata page",
                version: 0
            })
        ));
    }

    #[test]
    fn metadata_page_record_kind_is_inside_authenticated_plaintext() {
        let plaintext = encode_metadata_page(
            SnapshotMetadataRecordKind::ContentLayout,
            b"canonical layout bytes",
        )
        .expect("encoding");
        assert!(matches!(
            decode_metadata_page(SnapshotMetadataRecordKind::NamespacePage, &plaintext),
            Err(MetadataPageError::RecordKindMismatch {
                expected: SnapshotMetadataRecordKind::NamespacePage,
                actual: SnapshotMetadataRecordKind::ContentLayout,
            })
        ));
    }

    #[test]
    fn metadata_page_rejects_path_bearing_or_wrong_kind_logical_ids() {
        for logical_id in [
            SnapshotMetadataRecordId::NamespacePage(NamespacePageId::new("src/secret.env")),
            SnapshotMetadataRecordId::NamespacePage(NamespacePageId::new(format!(
                "ctl_{}",
                "11".repeat(32)
            ))),
        ] {
            assert!(matches!(
                seal_snapshot_metadata_page(
                    &WorkspaceId::new("ws_code"),
                    logical_id,
                    b"canonical",
                    StorageKey::deterministic(7),
                    3,
                ),
                Err(MetadataPageError::InvalidLogicalId(
                    SnapshotMetadataRecordKind::NamespacePage
                ))
            ));
        }
    }
}
