use std::{collections::BTreeSet, error::Error, fmt};

use bowline_control_plane::{
    CompareAndSwapError, ControlPlaneClient, ControlPlaneError, ControlPlaneTimestamp, ObjectKind,
    ObjectManifestCommit, ObjectManifestRecord, ObjectPointer, UploadIntentRequest, WorkspaceRef,
};
use bowline_storage::{
    ByteStore, ByteStoreError, ObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata,
    PackRecordInput, PackfileError, StorageKey, seal_snapshot_manifest, write_source_packs,
};

use super::SnapshotCandidate;

const SOURCE_PACK_TARGET_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadOutcome {
    Advanced {
        workspace_ref: WorkspaceRef,
        object_manifest: ObjectManifestRecord,
    },
    Stale {
        stale: bowline_control_plane::StaleWorkspaceRef,
        object_manifest: ObjectManifestRecord,
    },
}

pub fn upload_snapshot_candidate(
    candidate: &SnapshotCandidate,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<UploadOutcome, UploadError> {
    upload_snapshot_candidate_with_checkpoints(
        candidate,
        control_plane,
        byte_store,
        storage_key,
        key_epoch,
        |_, _| Ok(()),
    )
}

pub fn upload_snapshot_candidate_with_checkpoints(
    candidate: &SnapshotCandidate,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
    mut checkpoint: impl FnMut(&str, String) -> Result<(), UploadError>,
) -> Result<UploadOutcome, UploadError> {
    if let Some(object_manifest) = control_plane.get_snapshot_manifest_pointer(
        candidate.base.workspace_id.as_str(),
        candidate.snapshot.manifest.snapshot_id.as_str(),
    )? {
        if object_manifest.manifest_id != candidate.manifest_id.as_str() {
            return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
                resource: "object manifest",
                reason: "snapshot is already committed with a different manifest ID",
            }));
        }
        checkpoint(
            "object-manifest-reused",
            format!(
                "{{\"snapshotId\":{},\"manifestId\":{}}}",
                json_string(candidate.snapshot.manifest.snapshot_id.as_str()),
                json_string(&object_manifest.manifest_id),
            ),
        )?;
        return finish_upload(candidate, control_plane, object_manifest, &mut checkpoint);
    }

    let pack_inputs = candidate
        .snapshot
        .files
        .iter()
        .map(|(content_id, bytes)| PackRecordInput {
            content_id: content_id.clone(),
            bytes: bytes.clone(),
        })
        .collect::<Vec<_>>();
    let packs = write_source_packs(
        candidate.snapshot.manifest.workspace_id.clone(),
        &pack_inputs,
        SOURCE_PACK_TARGET_BYTES,
        storage_key,
        key_epoch,
    )?;
    checkpoint(
        "source-packs-written",
        format!(
            "{{\"snapshotId\":{},\"packCount\":{},\"recordCount\":{}}}",
            json_string(candidate.snapshot.manifest.snapshot_id.as_str()),
            packs.len(),
            pack_inputs.len(),
        ),
    )?;

    let mut manifest = candidate.snapshot.manifest.clone();
    for pack in &packs {
        for locator in &pack.locators {
            for entry in &mut manifest.entries {
                if entry.content_id.as_ref() == Some(&locator.content_id) {
                    entry.locator = Some(locator.clone());
                    entry.hydration_state = bowline_core::workspace_graph::HydrationState::Cold;
                }
            }
        }
    }

    let mut pack_pointers = Vec::new();
    let mut included_pack_keys = BTreeSet::<String>::new();
    for pack in &packs {
        let metadata = ensure_uploaded_object(
            control_plane,
            byte_store,
            UploadObjectRequest {
                workspace_id: candidate.base.workspace_id.as_str(),
                control_plane_kind: ObjectKind::SourcePack,
                storage_kind: StorageObjectKind::SourcePack,
                key: pack.object_key.clone(),
                content_id: pack.pack_id.as_str(),
                bytes: &pack.bytes,
                key_epoch,
                device_id: Some(&candidate.device_id),
            },
        )?;
        checkpoint(
            "source-pack-uploaded",
            format!(
                "{{\"objectKey\":{},\"contentId\":{},\"byteLen\":{},\"hash\":{}}}",
                json_string(metadata.key.as_str()),
                json_string(pack.pack_id.as_str()),
                metadata.byte_len,
                json_string(&metadata.hash),
            ),
        )?;
        included_pack_keys.insert(metadata.key.as_str().to_string());
        pack_pointers.push(ObjectPointer {
            object_key: metadata.key.as_str().to_string(),
            content_id: pack.pack_id.as_str().to_string(),
            byte_len: metadata.byte_len,
            hash: metadata.hash,
            key_epoch,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp {
                tick: metadata.created_at_unix_ms,
            },
        });
    }
    for entry in &manifest.entries {
        let Some(locator) = &entry.locator else {
            continue;
        };
        let Some(pack_id) = locator.pack_id.as_ref() else {
            continue;
        };
        let object_key = ObjectKey::from_pack_id(pack_id)?;
        if !included_pack_keys.insert(object_key.as_str().to_string()) {
            continue;
        }
        let metadata = control_plane
            .head_object_metadata(candidate.base.workspace_id.as_str(), object_key.as_str())?;
        if metadata.kind != StorageObjectKind::SourcePack {
            return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
                resource: "object metadata",
                reason: "manifest locator points at a non-source-pack object",
            }));
        }
        checkpoint(
            "source-pack-reused",
            format!(
                "{{\"objectKey\":{},\"contentId\":{},\"byteLen\":{},\"hash\":{}}}",
                json_string(metadata.key.as_str()),
                json_string(pack_id.as_str()),
                metadata.byte_len,
                json_string(&metadata.hash),
            ),
        )?;
        pack_pointers.push(ObjectPointer {
            object_key: metadata.key.as_str().to_string(),
            content_id: pack_id.as_str().to_string(),
            byte_len: metadata.byte_len,
            hash: metadata.hash,
            key_epoch: metadata.key_epoch,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp {
                tick: metadata.created_at_unix_ms,
            },
        });
    }

    let sealed = seal_snapshot_manifest(
        candidate.manifest_id.clone(),
        &manifest,
        storage_key,
        key_epoch,
    )?;
    let manifest_metadata = ensure_uploaded_object(
        control_plane,
        byte_store,
        UploadObjectRequest {
            workspace_id: candidate.base.workspace_id.as_str(),
            control_plane_kind: ObjectKind::SnapshotManifest,
            storage_kind: StorageObjectKind::SnapshotManifest,
            key: sealed.pointer.object_key.clone(),
            content_id: sealed.pointer.snapshot_id.as_str(),
            bytes: &sealed.bytes,
            key_epoch,
            device_id: Some(&candidate.device_id),
        },
    )?;
    checkpoint(
        "snapshot-manifest-uploaded",
        format!(
            "{{\"objectKey\":{},\"snapshotId\":{},\"byteLen\":{},\"hash\":{}}}",
            json_string(manifest_metadata.key.as_str()),
            json_string(sealed.pointer.snapshot_id.as_str()),
            manifest_metadata.byte_len,
            json_string(&manifest_metadata.hash),
        ),
    )?;
    let manifest_pointer = ObjectPointer {
        object_key: manifest_metadata.key.as_str().to_string(),
        content_id: sealed.pointer.snapshot_id.as_str().to_string(),
        byte_len: manifest_metadata.byte_len,
        hash: manifest_metadata.hash,
        key_epoch,
        kind: ObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp {
            tick: manifest_metadata.created_at_unix_ms,
        },
    };
    let object_manifest = control_plane.commit_object_manifest(ObjectManifestCommit {
        workspace_id: candidate.base.workspace_id.as_str().to_string(),
        snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str().to_string(),
        manifest_id: candidate.manifest_id.as_str().to_string(),
        manifest_object: manifest_pointer,
        pack_objects: pack_pointers,
        committed_by_device_id: candidate.device_id.as_str().to_string(),
    })?;
    checkpoint(
        "object-manifest-committed",
        format!(
            "{{\"snapshotId\":{},\"manifestId\":{},\"packCount\":{}}}",
            json_string(candidate.snapshot.manifest.snapshot_id.as_str()),
            json_string(&object_manifest.manifest_id),
            object_manifest.pack_objects.len(),
        ),
    )?;

    finish_upload(candidate, control_plane, object_manifest, &mut checkpoint)
}

fn finish_upload(
    candidate: &SnapshotCandidate,
    control_plane: &dyn ControlPlaneClient,
    object_manifest: ObjectManifestRecord,
    checkpoint: &mut impl FnMut(&str, String) -> Result<(), UploadError>,
) -> Result<UploadOutcome, UploadError> {
    match control_plane.compare_and_swap_workspace_ref(
        candidate.base.workspace_id.as_str(),
        candidate.base.version,
        candidate.snapshot.manifest.snapshot_id.as_str(),
        candidate.device_id.as_str(),
    ) {
        Ok(workspace_ref) => {
            checkpoint(
                "workspace-ref-advanced",
                format!(
                    "{{\"snapshotId\":{},\"version\":{}}}",
                    json_string(&workspace_ref.snapshot_id),
                    workspace_ref.version,
                ),
            )?;
            Ok(UploadOutcome::Advanced {
                workspace_ref,
                object_manifest,
            })
        }
        Err(CompareAndSwapError::StaleRef(stale)) => {
            checkpoint(
                "workspace-ref-stale",
                format!(
                    "{{\"attemptedSnapshotId\":{},\"currentSnapshotId\":{},\"currentVersion\":{}}}",
                    json_string(candidate.snapshot.manifest.snapshot_id.as_str()),
                    json_string(&stale.current.snapshot_id),
                    stale.current.version,
                ),
            )?;
            Ok(UploadOutcome::Stale {
                stale,
                object_manifest,
            })
        }
        Err(error) => Err(UploadError::CompareAndSwap(error)),
    }
}

fn ensure_uploaded_object(
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    request: UploadObjectRequest<'_>,
) -> Result<ObjectMetadata, UploadError> {
    match control_plane.head_object_metadata(request.workspace_id, request.key.as_str()) {
        Ok(metadata) => {
            validate_uploaded_metadata(
                &metadata,
                &request.key,
                request.storage_kind,
                request.bytes,
                request.key_epoch,
            )?;
            return Ok(metadata);
        }
        Err(ControlPlaneError::ObjectMissing { .. }) => {}
        Err(error) => return Err(UploadError::ControlPlane(error)),
    }

    control_plane.create_upload_intent(
        UploadIntentRequest::new(
            request.workspace_id,
            request.control_plane_kind,
            request.bytes.len() as u64,
        )
        .with_object_key(request.key.as_str())
        .with_content_id(request.content_id),
    )?;
    let metadata = put_or_read_existing(
        byte_store,
        request.key.clone(),
        request.storage_kind,
        request.content_id,
        request.bytes,
        request.key_epoch,
        request.device_id,
    )?;
    validate_uploaded_metadata(
        &metadata,
        &request.key,
        request.storage_kind,
        request.bytes,
        request.key_epoch,
    )?;
    Ok(metadata)
}

struct UploadObjectRequest<'a> {
    workspace_id: &'a str,
    control_plane_kind: ObjectKind,
    storage_kind: StorageObjectKind,
    key: bowline_storage::ObjectKey,
    content_id: &'a str,
    bytes: &'a [u8],
    key_epoch: u32,
    device_id: Option<&'a bowline_core::ids::DeviceId>,
}

fn put_or_read_existing(
    byte_store: &dyn ByteStore,
    key: bowline_storage::ObjectKey,
    kind: StorageObjectKind,
    content_id: &str,
    bytes: &[u8],
    key_epoch: u32,
    device_id: Option<&bowline_core::ids::DeviceId>,
) -> Result<ObjectMetadata, ByteStoreError> {
    match byte_store.put_object_with_content_id_at_epoch(
        key.clone(),
        kind,
        content_id,
        bytes,
        key_epoch,
        device_id,
    ) {
        Ok(metadata) => Ok(metadata),
        Err(ByteStoreError::ObjectAlreadyExists(existing_key)) if existing_key == key => {
            byte_store.head_object(&key)
        }
        Err(error) => Err(error),
    }
}

fn validate_uploaded_metadata(
    metadata: &ObjectMetadata,
    key: &bowline_storage::ObjectKey,
    kind: StorageObjectKind,
    bytes: &[u8],
    key_epoch: u32,
) -> Result<(), UploadError> {
    let expected_hash = format!("b3_{}", blake3::hash(bytes).to_hex());
    if metadata.key != *key
        || metadata.kind != kind
        || metadata.byte_len != bytes.len() as u64
        || metadata.hash != expected_hash
        || metadata.key_epoch != key_epoch
    {
        return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "object metadata",
            reason: "committed object metadata does not match deterministic upload",
        }));
    }
    Ok(())
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid>\"".to_string())
}

#[derive(Debug)]
pub enum UploadError {
    ControlPlane(ControlPlaneError),
    ByteStore(ByteStoreError),
    Packfile(PackfileError),
    Manifest(bowline_storage::ManifestError),
    CompareAndSwap(CompareAndSwapError),
    Checkpoint(String),
}

impl fmt::Display for UploadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Packfile(error) => error.fmt(formatter),
            Self::Manifest(error) => error.fmt(formatter),
            Self::CompareAndSwap(error) => error.fmt(formatter),
            Self::Checkpoint(error) => write!(formatter, "sync checkpoint failed: {error}"),
        }
    }
}

impl Error for UploadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Packfile(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::CompareAndSwap(error) => Some(error),
            Self::Checkpoint(_) => None,
        }
    }
}

impl From<ControlPlaneError> for UploadError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<ByteStoreError> for UploadError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<PackfileError> for UploadError {
    fn from(error: PackfileError) -> Self {
        Self::Packfile(error)
    }
}

impl From<bowline_storage::ManifestError> for UploadError {
    fn from(error: bowline_storage::ManifestError) -> Self {
        Self::Manifest(error)
    }
}
