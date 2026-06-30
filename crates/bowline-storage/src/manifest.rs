use std::{error::Error, fmt};

use bowline_core::{
    ids::{ContentId, ManifestId, SnapshotId, WorkspaceId},
    workspace_graph::{ContentLocator, SnapshotManifest},
};
use serde::{Deserialize, Serialize};

use crate::{
    ObjectKey, ObjectKind,
    envelope::{EnvelopeContext, EnvelopeError, StorageKey, open, seal, workspace_id_hash},
    store::stable_object_hash,
};

const MANIFEST_FORMAT_VERSION: u16 = 1;

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
pub struct IndexPackPointer {
    pub index_pack_id: String,
    pub snapshot_id: SnapshotId,
    pub object_key: ObjectKey,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedIndexPack {
    pub pointer: IndexPackPointer,
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
    let plaintext = serde_json::to_vec(manifest).expect("snapshot manifest serializes");
    let object_key = ObjectKey::from_manifest_id(&manifest_id)?;
    let context = manifest_context(&manifest_id, manifest, key_epoch);
    let bytes = seal(&plaintext, key, &context)?.into_bytes();
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
    if sealed.pointer.byte_len != sealed.bytes.len() as u64 {
        return Err(ManifestError::PointerIntegrity("byte_len"));
    }
    if sealed.pointer.hash != stable_object_hash(&sealed.bytes) {
        return Err(ManifestError::PointerIntegrity("hash"));
    }

    let context = EnvelopeContext {
        workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
        object_kind: ObjectKind::SnapshotManifest,
        object_id: sealed.pointer.manifest_id.as_str().to_string(),
        record_id: sealed.pointer.snapshot_id.as_str().to_string(),
        key_epoch: sealed.pointer.key_epoch,
        format_version: MANIFEST_FORMAT_VERSION,
    };
    let plaintext = open(&sealed.bytes, key, &context)?;
    serde_json::from_slice(&plaintext).map_err(|_| ManifestError::InvalidManifestJson)
}

pub fn remap_locator(
    manifest: &SnapshotManifest,
    content_id: &ContentId,
    new_locator: ContentLocator,
) -> Result<SnapshotManifest, ManifestError> {
    if new_locator.content_id != *content_id {
        return Err(ManifestError::LocatorContentMismatch);
    }

    let mut remapped = manifest.clone();
    for entry in &mut remapped.entries {
        if entry.content_id.as_ref() == Some(content_id) {
            entry.locator = Some(new_locator.clone());
        }
    }
    Ok(remapped)
}

fn manifest_context(
    manifest_id: &ManifestId,
    manifest: &SnapshotManifest,
    key_epoch: u32,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(manifest.workspace_id.as_str()),
        object_kind: ObjectKind::SnapshotManifest,
        object_id: manifest_id.as_str().to_string(),
        record_id: manifest.snapshot_id.as_str().to_string(),
        key_epoch,
        format_version: MANIFEST_FORMAT_VERSION,
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Envelope(EnvelopeError),
    InvalidManifestJson,
    PointerIntegrity(&'static str),
    LocatorContentMismatch,
    ObjectKey(crate::ByteStoreError),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(error) => write!(formatter, "manifest envelope failed: {error}"),
            Self::InvalidManifestJson => formatter.write_str("manifest JSON did not parse"),
            Self::PointerIntegrity(field) => {
                write!(
                    formatter,
                    "manifest pointer {field} did not match sealed bytes"
                )
            }
            Self::LocatorContentMismatch => {
                formatter.write_str("manifest locator content ID did not match entry content ID")
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
        ids::{ContentId, ManifestId, PackId, ProjectId, SnapshotId, WorkspaceId},
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{
            ContentLocator, ContentStorage, HydrationState, NamespaceEntry, NamespaceEntryKind,
            RefKind, SnapshotKind, WorkspaceRef,
        },
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
    fn locator_remap_preserves_entry_identity() {
        let manifest = test_manifest();
        let content_id = ContentId::new("cid_src");
        let new_locator = ContentLocator {
            content_id: content_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: 12,
            pack_id: Some(PackId::new("pk_new_00000001")),
            offset: Some(99),
            length: Some(40),
            chunk_ids: Vec::new(),
        };
        let remapped =
            remap_locator(&manifest, &content_id, new_locator.clone()).expect("locator remaps");
        let original_entry = manifest
            .entries
            .iter()
            .find(|entry| entry.content_id.as_ref() == Some(&content_id))
            .expect("original entry");
        let remapped_entry = remapped
            .entries
            .iter()
            .find(|entry| entry.content_id.as_ref() == Some(&content_id))
            .expect("remapped entry");

        assert_eq!(remapped_entry.path, original_entry.path);
        assert_eq!(remapped_entry.content_id, original_entry.content_id);
        assert_eq!(remapped_entry.locator, Some(new_locator));
    }

    #[test]
    fn locator_remap_rejects_mismatched_content_id() {
        let manifest = test_manifest();
        let new_locator = ContentLocator {
            content_id: ContentId::new("cid_other"),
            storage: ContentStorage::Packed,
            raw_size: 12,
            pack_id: Some(PackId::new("pk_new_00000001")),
            offset: Some(99),
            length: Some(40),
            chunk_ids: Vec::new(),
        };

        assert!(matches!(
            remap_locator(&manifest, &ContentId::new("cid_src"), new_locator),
            Err(ManifestError::LocatorContentMismatch)
        ));
    }

    fn test_manifest() -> SnapshotManifest {
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: SnapshotId::new("snap_workspace_head"),
            workspace_id: WorkspaceId::new("ws_code"),
            project_id: Some(ProjectId::new("proj_acme_web")),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: Some(SnapshotId::new("snap_base")),
            entries: vec![
                NamespaceEntry {
                    path: "acme/web".to_string(),
                    kind: NamespaceEntryKind::Directory,
                    classification: PathClassification::WorkspaceSync,
                    mode: MaterializationMode::StructureOnly,
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                    content_id: None,
                    locator: None,
                    symlink_target: None,
                    byte_len: None,
                    hydration_state: HydrationState::StructureOnly,
                },
                NamespaceEntry {
                    path: "acme/web/src/index.ts".to_string(),
                    kind: NamespaceEntryKind::File,
                    classification: PathClassification::WorkspaceSync,
                    mode: MaterializationMode::EncryptedSync,
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                    content_id: Some(ContentId::new("cid_src")),
                    locator: Some(ContentLocator {
                        content_id: ContentId::new("cid_src"),
                        storage: ContentStorage::Packed,
                        raw_size: 12,
                        pack_id: Some(PackId::new("pk_old_00000001")),
                        offset: Some(12),
                        length: Some(40),
                        chunk_ids: Vec::new(),
                    }),
                    symlink_target: None,
                    byte_len: Some(12),
                    hydration_state: HydrationState::Cold,
                },
                NamespaceEntry {
                    path: "acme/web/.env.local".to_string(),
                    kind: NamespaceEntryKind::File,
                    classification: PathClassification::ProjectEnv,
                    mode: MaterializationMode::ProjectEnv,
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
                    content_id: Some(ContentId::new("cid_env")),
                    locator: None,
                    symlink_target: None,
                    byte_len: Some(24),
                    hydration_state: HydrationState::Cold,
                },
                NamespaceEntry {
                    path: "acme/web/docs/latest".to_string(),
                    kind: NamespaceEntryKind::Symlink,
                    classification: PathClassification::WorkspaceSync,
                    mode: MaterializationMode::EncryptedSync,
                    access: vec![AccessFlag::HumanReadable],
                    content_id: None,
                    locator: None,
                    symlink_target: Some("../shared".to_string()),
                    byte_len: None,
                    hydration_state: HydrationState::Local,
                },
            ],
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: SnapshotId::new("snap_workspace_head"),
                kind: RefKind::Workspace,
            }],
        }
    }
}
