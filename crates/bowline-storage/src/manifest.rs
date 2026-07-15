use std::{error::Error, fmt};

use bowline_core::{
    ids::{ManifestId, SnapshotId, WorkspaceId},
    workspace_graph::{SNAPSHOT_SCHEMA_VERSION, SnapshotManifest},
};
use serde::{Deserialize, Serialize};

use crate::{
    ObjectKey, ObjectKind,
    envelope::{EnvelopeContext, EnvelopeError, StorageKey, open, seal, workspace_id_hash},
    store::stable_object_hash,
};

const SNAPSHOT_ROOT_MAGIC: &[u8; 4] = b"BWSR";
const SNAPSHOT_ROOT_HEADER_BYTES: usize = SNAPSHOT_ROOT_MAGIC.len() + 2 + 4;
const LEGACY_FLAT_MANIFEST_FORMAT_VERSION: u16 = 1;
pub const SNAPSHOT_ROOT_FORMAT_VERSION: u16 = 2;
pub const SNAPSHOT_ROOT_MAX_PLAINTEXT_BYTES: usize = 64 * 1024;
pub const SNAPSHOT_ROOT_MAX_SEALED_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestPointer {
    pub manifest_id: ManifestId,
    pub snapshot_id: SnapshotId,
    pub object_key: ObjectKey,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
    pub kind: ManifestPointerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManifestPointerKind {
    Snapshot,
    AgentOverlay,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSnapshotManifest {
    pub pointer: ManifestPointer,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocatorIndexBinding {
    pub manifest_id: ManifestId,
    pub snapshot_id: SnapshotId,
    pub locator_table_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocatorIndexPointer {
    pub locator_index_id: String,
    pub manifest_id: ManifestId,
    pub snapshot_id: SnapshotId,
    pub object_key: ObjectKey,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
    pub format_version: u16,
    pub locator_table_digest: String,
}

impl LocatorIndexPointer {
    pub fn binding(&self) -> LocatorIndexBinding {
        LocatorIndexBinding {
            manifest_id: self.manifest_id.clone(),
            snapshot_id: self.snapshot_id.clone(),
            locator_table_digest: self.locator_table_digest.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedLocatorIndex {
    pub pointer: LocatorIndexPointer,
    pub bytes: Vec<u8>,
}

pub fn seal_snapshot_manifest(
    manifest_id: ManifestId,
    manifest: &SnapshotManifest,
    key: StorageKey,
    key_epoch: u32,
) -> Result<SealedSnapshotManifest, ManifestError> {
    if manifest.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchemaVersion(
            manifest.schema_version,
        ));
    }
    let payload = serde_json::to_vec(manifest).map_err(|_| ManifestError::InvalidRootEncoding)?;
    let plaintext = encode_snapshot_root(&payload)?;
    let object_key = ObjectKey::from_manifest_id(&manifest_id)?;
    let context = manifest_context(
        &manifest_id,
        manifest,
        key_epoch,
        SNAPSHOT_ROOT_FORMAT_VERSION,
    );
    let bytes = seal(&plaintext, key, &context)?.into_bytes();
    validate_root_sealed_size(bytes.len())?;
    let pointer = ManifestPointer {
        manifest_id,
        snapshot_id: manifest.snapshot_id.clone(),
        object_key,
        byte_len: bytes.len() as u64,
        hash: stable_object_hash(&bytes),
        key_epoch,
        kind: ManifestPointerKind::Snapshot,
    };
    Ok(SealedSnapshotManifest { pointer, bytes })
}

pub fn open_snapshot_manifest(
    sealed: &SealedSnapshotManifest,
    key: StorageKey,
    workspace_id: &WorkspaceId,
) -> Result<SnapshotManifest, ManifestError> {
    validate_snapshot_root_pointer(sealed)?;
    let context =
        manifest_pointer_context(&sealed.pointer, workspace_id, SNAPSHOT_ROOT_FORMAT_VERSION);
    let plaintext = match open(&sealed.bytes, key, &context) {
        Ok(plaintext) => plaintext,
        Err(current_error) => {
            let legacy_context = manifest_pointer_context(
                &sealed.pointer,
                workspace_id,
                LEGACY_FLAT_MANIFEST_FORMAT_VERSION,
            );
            if open(&sealed.bytes, key, &legacy_context).is_ok() {
                return Err(ManifestError::UnsupportedFormat {
                    record: "snapshot root",
                    version: LEGACY_FLAT_MANIFEST_FORMAT_VERSION,
                });
            }
            return Err(current_error.into());
        }
    };
    let payload = decode_snapshot_root(&plaintext)?;
    let manifest: SnapshotManifest =
        serde_json::from_slice(payload).map_err(|_| ManifestError::InvalidRootEncoding)?;
    // The schema version lives inside AEAD-authenticated plaintext. Accepting
    // any other version would silently reintroduce a migration reader.
    if manifest.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchemaVersion(
            manifest.schema_version,
        ));
    }
    if manifest.workspace_id != *workspace_id {
        return Err(ManifestError::RootIdentity("workspace_id"));
    }
    if manifest.snapshot_id != sealed.pointer.snapshot_id {
        return Err(ManifestError::RootIdentity("snapshot_id"));
    }
    Ok(manifest)
}

fn validate_snapshot_root_pointer(sealed: &SealedSnapshotManifest) -> Result<(), ManifestError> {
    validate_root_sealed_size(sealed.bytes.len())?;
    if sealed.pointer.kind != ManifestPointerKind::Snapshot {
        return Err(ManifestError::PointerIntegrity("kind"));
    }
    if sealed.pointer.byte_len != sealed.bytes.len() as u64 {
        return Err(ManifestError::PointerIntegrity("byte_len"));
    }
    if sealed.pointer.hash != stable_object_hash(&sealed.bytes) {
        return Err(ManifestError::PointerIntegrity("hash"));
    }
    let expected_key = ObjectKey::from_manifest_id(&sealed.pointer.manifest_id)?;
    if sealed.pointer.object_key != expected_key {
        return Err(ManifestError::PointerIntegrity("object_key"));
    }
    Ok(())
}

fn encode_snapshot_root(payload: &[u8]) -> Result<Vec<u8>, ManifestError> {
    validate_root_plaintext_size(payload.len())?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| ManifestError::OversizedRecord {
        record: "snapshot root",
        encoded_bytes: payload.len() as u64,
        maximum_bytes: SNAPSHOT_ROOT_MAX_PLAINTEXT_BYTES as u64,
    })?;
    let mut plaintext = Vec::with_capacity(SNAPSHOT_ROOT_HEADER_BYTES + payload.len());
    plaintext.extend_from_slice(SNAPSHOT_ROOT_MAGIC);
    plaintext.extend_from_slice(&SNAPSHOT_ROOT_FORMAT_VERSION.to_le_bytes());
    plaintext.extend_from_slice(&payload_len.to_le_bytes());
    plaintext.extend_from_slice(payload);
    Ok(plaintext)
}

fn decode_snapshot_root(plaintext: &[u8]) -> Result<&[u8], ManifestError> {
    if plaintext.len() < SNAPSHOT_ROOT_HEADER_BYTES {
        return Err(ManifestError::InvalidRootEncoding);
    }
    if &plaintext[..SNAPSHOT_ROOT_MAGIC.len()] != SNAPSHOT_ROOT_MAGIC {
        return Err(ManifestError::UnsupportedFormat {
            record: "snapshot root",
            version: 0,
        });
    }
    let version_offset = SNAPSHOT_ROOT_MAGIC.len();
    let version = u16::from_le_bytes([plaintext[version_offset], plaintext[version_offset + 1]]);
    if version != SNAPSHOT_ROOT_FORMAT_VERSION {
        return Err(ManifestError::UnsupportedFormat {
            record: "snapshot root",
            version,
        });
    }
    let length_offset = version_offset + 2;
    let payload_len = u32::from_le_bytes([
        plaintext[length_offset],
        plaintext[length_offset + 1],
        plaintext[length_offset + 2],
        plaintext[length_offset + 3],
    ]) as usize;
    validate_root_plaintext_size(payload_len)?;
    let expected_len = SNAPSHOT_ROOT_HEADER_BYTES
        .checked_add(payload_len)
        .ok_or(ManifestError::InvalidRootEncoding)?;
    if plaintext.len() != expected_len {
        return Err(ManifestError::InvalidRootEncoding);
    }
    Ok(&plaintext[SNAPSHOT_ROOT_HEADER_BYTES..])
}

fn validate_root_plaintext_size(encoded_bytes: usize) -> Result<(), ManifestError> {
    if encoded_bytes > SNAPSHOT_ROOT_MAX_PLAINTEXT_BYTES {
        return Err(ManifestError::OversizedRecord {
            record: "snapshot root",
            encoded_bytes: encoded_bytes as u64,
            maximum_bytes: SNAPSHOT_ROOT_MAX_PLAINTEXT_BYTES as u64,
        });
    }
    Ok(())
}

fn validate_root_sealed_size(encoded_bytes: usize) -> Result<(), ManifestError> {
    if encoded_bytes > SNAPSHOT_ROOT_MAX_SEALED_BYTES {
        return Err(ManifestError::OversizedRecord {
            record: "sealed snapshot root",
            encoded_bytes: encoded_bytes as u64,
            maximum_bytes: SNAPSHOT_ROOT_MAX_SEALED_BYTES as u64,
        });
    }
    Ok(())
}

fn manifest_context(
    manifest_id: &ManifestId,
    manifest: &SnapshotManifest,
    key_epoch: u32,
    format_version: u16,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(manifest.workspace_id.as_str()),
        object_kind: ObjectKind::SnapshotManifest,
        object_id: manifest_id.as_str().to_string(),
        record_id: manifest.snapshot_id.as_str().to_string(),
        key_epoch,
        format_version,
    }
}

fn manifest_pointer_context(
    pointer: &ManifestPointer,
    workspace_id: &WorkspaceId,
    format_version: u16,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
        object_kind: ObjectKind::SnapshotManifest,
        object_id: pointer.manifest_id.as_str().to_string(),
        record_id: pointer.snapshot_id.as_str().to_string(),
        key_epoch: pointer.key_epoch,
        format_version,
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Envelope(EnvelopeError),
    InvalidRootEncoding,
    OversizedRecord {
        record: &'static str,
        encoded_bytes: u64,
        maximum_bytes: u64,
    },
    PointerIntegrity(&'static str),
    RootIdentity(&'static str),
    UnsupportedSchemaVersion(u16),
    UnsupportedFormat {
        record: &'static str,
        version: u16,
    },
    ObjectKey(crate::ByteStoreError),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(error) => write!(formatter, "manifest envelope failed: {error}"),
            Self::InvalidRootEncoding => {
                formatter.write_str("snapshot root encoding did not parse")
            }
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
                    "manifest pointer {field} did not match sealed bytes"
                )
            }
            Self::RootIdentity(field) => {
                write!(
                    formatter,
                    "snapshot root {field} did not match its pointer context"
                )
            }
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "snapshot root schema version {version} is unsupported; current version is {SNAPSHOT_SCHEMA_VERSION}"
                )
            }
            Self::UnsupportedFormat { record, version } => {
                write!(formatter, "unsupported {record} format version {version}")
            }
            Self::ObjectKey(error) => write!(formatter, "manifest object key failed: {error}"),
        }
    }
}

impl Error for ManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
            Self::ObjectKey(error) => Some(error),
            _ => None,
        }
    }
}

impl From<EnvelopeError> for ManifestError {
    fn from(error: EnvelopeError) -> Self {
        Self::Envelope(error)
    }
}

impl From<crate::ByteStoreError> for ManifestError {
    fn from(error: crate::ByteStoreError) -> Self {
        Self::ObjectKey(error)
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        ids::{ManifestDigest, ManifestId, NamespacePageId, ProjectId, SnapshotId, WorkspaceId},
        workspace_graph::{RefKind, SnapshotKind, WorkspaceRef},
    };

    use super::*;

    fn pointer_exposes_path(pointer: &ManifestPointer, path_segments: &[&str]) -> bool {
        let pointer_json = serde_json::to_string(pointer).expect("pointer serializes");
        path_segments
            .iter()
            .any(|segment| !segment.is_empty() && pointer_json.contains(segment))
    }

    #[test]
    fn sealed_manifest_round_trips_and_hides_plaintext_paths() {
        let manifest = test_manifest();
        let sealed = seal_snapshot_manifest(
            ManifestId::new("mf_0011223344556677"),
            &manifest,
            StorageKey::deterministic(11),
            1,
        )
        .expect("manifest seals");

        for secret in [
            "acme/web",
            "src/index.ts",
            ".env.local",
            ".git/HEAD",
            "../shared",
        ] {
            assert!(
                !sealed
                    .bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "sealed manifest leaked {secret}"
            );
        }
        assert!(!pointer_exposes_path(
            &sealed.pointer,
            &["acme", "web", "src", ".env", ".git"]
        ));

        let opened = open_snapshot_manifest(
            &sealed,
            StorageKey::deterministic(11),
            &WorkspaceId::new("ws_code"),
        )
        .expect("manifest opens");
        assert_eq!(opened, manifest);
    }

    #[test]
    fn sealed_manifest_uses_fresh_envelope_nonce_for_same_candidate() {
        let manifest = test_manifest();
        let first = seal_snapshot_manifest(
            ManifestId::new("mf_0011223344556677"),
            &manifest,
            StorageKey::deterministic(11),
            1,
        )
        .expect("first manifest seals");
        let retry = seal_snapshot_manifest(
            ManifestId::new("mf_0011223344556677"),
            &manifest,
            StorageKey::deterministic(11),
            1,
        )
        .expect("retry manifest seals");

        assert_ne!(first.bytes, retry.bytes);
        assert_ne!(first.pointer.hash, retry.pointer.hash);
        assert_eq!(first.pointer.manifest_id, retry.pointer.manifest_id);
        assert_eq!(
            open_snapshot_manifest(
                &first,
                StorageKey::deterministic(11),
                &manifest.workspace_id
            )
            .expect("first opens"),
            manifest
        );
        assert_eq!(
            open_snapshot_manifest(
                &retry,
                StorageKey::deterministic(11),
                &manifest.workspace_id
            )
            .expect("retry opens"),
            manifest
        );
    }

    #[test]
    fn manifest_open_rejects_pointer_integrity_mismatch() {
        let manifest = test_manifest();
        let sealed = seal_snapshot_manifest(
            ManifestId::new("mf_0011223344556677"),
            &manifest,
            StorageKey::deterministic(11),
            1,
        )
        .expect("manifest seals");

        let mut wrong_len = sealed.clone();
        wrong_len.pointer.byte_len += 1;
        assert!(matches!(
            open_snapshot_manifest(
                &wrong_len,
                StorageKey::deterministic(11),
                &WorkspaceId::new("ws_code")
            ),
            Err(ManifestError::PointerIntegrity("byte_len"))
        ));

        let mut wrong_hash = sealed;
        wrong_hash.pointer.hash = "b3_bad".to_string();
        assert!(matches!(
            open_snapshot_manifest(
                &wrong_hash,
                StorageKey::deterministic(11),
                &WorkspaceId::new("ws_code")
            ),
            Err(ManifestError::PointerIntegrity("hash"))
        ));
    }

    #[test]
    fn manifest_writer_rejects_unsupported_schema_version() {
        let mut manifest = test_manifest();
        manifest.schema_version = SNAPSHOT_SCHEMA_VERSION + 1;
        let sealed = seal_snapshot_manifest(
            ManifestId::new("mf_0011223344556677"),
            &manifest,
            StorageKey::deterministic(11),
            1,
        );
        assert!(matches!(
            sealed,
            Err(ManifestError::UnsupportedSchemaVersion(version))
                if version == SNAPSHOT_SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn manifest_schema_version_guard_is_after_aead_auth() {
        let mut manifest = test_manifest();
        manifest.schema_version = SNAPSHOT_SCHEMA_VERSION + 1;
        let sealed = seal_test_snapshot_root(
            &manifest,
            StorageKey::deterministic(11),
            SNAPSHOT_ROOT_FORMAT_VERSION,
            true,
        );

        let opened = open_snapshot_manifest(
            &sealed,
            StorageKey::deterministic(22),
            &manifest.workspace_id,
        );
        assert!(matches!(opened, Err(ManifestError::Envelope(_))));
        assert!(!matches!(
            opened,
            Err(ManifestError::UnsupportedSchemaVersion(_))
        ));
    }

    #[test]
    fn legacy_flat_manifest_fails_explicitly_without_returning_plaintext() {
        let manifest = test_manifest();
        let sealed = seal_test_snapshot_root(
            &manifest,
            StorageKey::deterministic(11),
            LEGACY_FLAT_MANIFEST_FORMAT_VERSION,
            false,
        );

        assert!(matches!(
            open_snapshot_manifest(
                &sealed,
                StorageKey::deterministic(11),
                &manifest.workspace_id,
            ),
            Err(ManifestError::UnsupportedFormat {
                record: "snapshot root",
                version: LEGACY_FLAT_MANIFEST_FORMAT_VERSION,
            })
        ));
    }

    #[test]
    fn snapshot_root_pointer_identity_is_verified() {
        let manifest = test_manifest();
        let sealed = seal_snapshot_manifest(
            ManifestId::new("mf_0011223344556677"),
            &manifest,
            StorageKey::deterministic(11),
            1,
        )
        .expect("manifest seals");

        let mut wrong_snapshot = sealed.clone();
        wrong_snapshot.pointer.snapshot_id = SnapshotId::new("snap_wrong");
        assert!(matches!(
            open_snapshot_manifest(
                &wrong_snapshot,
                StorageKey::deterministic(11),
                &manifest.workspace_id,
            ),
            Err(ManifestError::Envelope(_))
        ));

        let mut wrong_key = sealed;
        wrong_key.pointer.object_key =
            ObjectKey::from_manifest_id(&ManifestId::new("mf_ffeeddccbbaa0099"))
                .expect("other manifest key");
        assert!(matches!(
            open_snapshot_manifest(
                &wrong_key,
                StorageKey::deterministic(11),
                &manifest.workspace_id,
            ),
            Err(ManifestError::PointerIntegrity("object_key"))
        ));
    }

    #[test]
    fn snapshot_root_plaintext_limit_is_typed() {
        let oversized = vec![0_u8; SNAPSHOT_ROOT_MAX_PLAINTEXT_BYTES + 1];
        assert!(matches!(
            encode_snapshot_root(&oversized),
            Err(ManifestError::OversizedRecord {
                record: "snapshot root",
                ..
            })
        ));
    }

    fn seal_test_snapshot_root(
        manifest: &SnapshotManifest,
        key: StorageKey,
        format_version: u16,
        current_wrapper: bool,
    ) -> SealedSnapshotManifest {
        let manifest_id = ManifestId::new("mf_0011223344556677");
        let payload = serde_json::to_vec(manifest).expect("test manifest serializes");
        let plaintext = if current_wrapper {
            encode_snapshot_root(&payload).expect("root encodes")
        } else {
            payload
        };
        let context = manifest_context(&manifest_id, manifest, 1, format_version);
        let bytes = seal(&plaintext, key, &context)
            .expect("test root seals")
            .into_bytes();
        SealedSnapshotManifest {
            pointer: ManifestPointer {
                object_key: ObjectKey::from_manifest_id(&manifest_id).expect("manifest key"),
                manifest_id,
                snapshot_id: manifest.snapshot_id.clone(),
                byte_len: bytes.len() as u64,
                hash: stable_object_hash(&bytes),
                key_epoch: 1,
                kind: ManifestPointerKind::Snapshot,
            },
            bytes,
        }
    }

    fn test_manifest() -> SnapshotManifest {
        SnapshotManifest {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: SnapshotId::new("snap_workspace_head"),
            workspace_id: WorkspaceId::new("ws_code"),
            project_id: Some(ProjectId::new("proj_acme_web")),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: Some(SnapshotId::new("snap_base")),
            namespace_root_id: NamespacePageId::new(format!("nsp_{}", "11".repeat(32))),
            semantic_manifest_digest: ManifestDigest::new(format!("md_{}", "22".repeat(32))),
            entry_count: 4,
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: SnapshotId::new("snap_workspace_head"),
                kind: RefKind::Workspace,
            }],
        }
    }
}
