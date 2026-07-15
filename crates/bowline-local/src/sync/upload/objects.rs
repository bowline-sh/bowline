use super::*;
use crate::sync::upload::packs::UploadPackSpoolFile;
use bowline_core::workspace_graph::SnapshotManifest;
use bowline_storage::{
    ManifestPointerKind, ObjectContentId, ObjectHash, PutObjectReaderRequest,
    SealedSnapshotManifest, open_snapshot_manifest,
};

pub(super) fn ensure_uploaded_source_pack<C>(
    request: &PackUploadRequest<'_, C>,
    pack: &PreparedSourcePack,
) -> Result<UploadedObject, UploadError>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    let pack = &pack.0;
    ensure_uploaded_streaming_object(
        request.control_plane,
        request.byte_store,
        UploadStreamingObjectRequest {
            workspace_id: &request.candidate.base.workspace_id,
            storage_kind: StorageObjectKind::SourcePack,
            key: pack.output.object_key.clone(),
            content_id: pack.output.pack_id.as_str(),
            byte_len: pack.output.byte_len,
            hash: &pack.output.hash,
            key_epoch: request.key_epoch,
            device_id: Some(&request.candidate.device_id),
            spool: &pack.spool,
        },
    )
}

pub(super) fn ensure_uploaded_object(
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    request: UploadObjectRequest<'_>,
) -> Result<UploadedObject, UploadError> {
    match control_plane.head_object_metadata(request.workspace_id, request.key.as_str()) {
        Ok(metadata) => {
            validate_uploaded_metadata(
                &metadata,
                &request.key,
                request.storage_kind,
                request.bytes,
                request.key_epoch,
            )?;
            return Ok(UploadedObject {
                metadata,
                wrote_object: false,
            });
        }
        Err(ControlPlaneError::ObjectMissing { .. }) => {}
        Err(error) => return Err(UploadError::ControlPlane(error)),
    }

    if let Some(reusable_manifest) = request.reusable_snapshot_manifest {
        match byte_store.head_object(&request.key) {
            Ok(metadata) => {
                validate_reusable_manifest_object(
                    byte_store,
                    &metadata,
                    &request.key,
                    reusable_manifest,
                )?;
                return Ok(UploadedObject {
                    metadata,
                    wrote_object: false,
                });
            }
            Err(ByteStoreError::MissingObject { .. }) => {}
            Err(error) => return Err(UploadError::ByteStore(error)),
        }
    }

    if !<dyn ByteStore>::creates_upload_intents(byte_store) {
        control_plane.create_upload_intent(
            UploadIntentRequest::new(
                request.workspace_id.as_str(),
                ObjectKind::try_from(request.storage_kind)?,
                request.bytes.len() as u64,
            )
            .with_object_key(request.key.as_str())
            .with_content_id(request.content_id),
        )?;
    }
    let uploaded = put_or_read_existing(
        byte_store,
        request.key.clone(),
        request.storage_kind,
        request.content_id,
        request.bytes,
        request.key_epoch,
        request.device_id,
    )?;
    validate_uploaded_metadata(
        &uploaded.metadata,
        &request.key,
        request.storage_kind,
        request.bytes,
        request.key_epoch,
    )?;
    Ok(uploaded)
}

pub(super) fn ensure_uploaded_streaming_object(
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    request: UploadStreamingObjectRequest<'_>,
) -> Result<UploadedObject, UploadError> {
    match control_plane.head_object_metadata(request.workspace_id, request.key.as_str()) {
        Ok(metadata) => {
            validate_streaming_uploaded_metadata(
                &metadata,
                &request.key,
                request.storage_kind,
                request.byte_len,
                request.hash,
                request.key_epoch,
            )?;
            return Ok(UploadedObject {
                metadata,
                wrote_object: false,
            });
        }
        Err(ControlPlaneError::ObjectMissing { .. }) => {}
        Err(error) => return Err(UploadError::ControlPlane(error)),
    }

    if !<dyn ByteStore>::creates_upload_intents(byte_store) {
        control_plane.create_upload_intent(
            UploadIntentRequest::new(
                request.workspace_id.as_str(),
                ObjectKind::try_from(request.storage_kind)?,
                request.byte_len,
            )
            .with_object_key(request.key.as_str())
            .with_content_id(request.content_id),
        )?;
    }
    let uploaded = put_or_read_existing_streaming(byte_store, &request)?;
    validate_streaming_uploaded_metadata(
        &uploaded.metadata,
        &request.key,
        request.storage_kind,
        request.byte_len,
        request.hash,
        request.key_epoch,
    )?;
    Ok(uploaded)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UploadedObject {
    pub(super) metadata: ObjectMetadata,
    pub(super) wrote_object: bool,
}

pub(super) struct UploadObjectRequest<'a> {
    pub(super) workspace_id: &'a bowline_core::ids::WorkspaceId,
    pub(super) storage_kind: StorageObjectKind,
    pub(super) key: ObjectKey,
    pub(super) content_id: &'a str,
    pub(super) bytes: &'a [u8],
    pub(super) key_epoch: u32,
    pub(super) device_id: Option<&'a bowline_core::ids::DeviceId>,
    pub(super) reusable_snapshot_manifest: Option<ReusableSnapshotManifest<'a>>,
}

#[derive(Clone, Copy)]
pub(super) struct ReusableSnapshotManifest<'a> {
    pub(super) pointer: &'a ManifestPointer,
    pub(super) manifest: &'a SnapshotManifest,
    pub(super) storage_key: StorageKey,
}

pub(super) struct UploadStreamingObjectRequest<'a> {
    pub(super) workspace_id: &'a bowline_core::ids::WorkspaceId,
    pub(super) storage_kind: StorageObjectKind,
    pub(super) key: ObjectKey,
    pub(super) content_id: &'a str,
    pub(super) byte_len: u64,
    pub(super) hash: &'a str,
    pub(super) key_epoch: u32,
    pub(super) device_id: Option<&'a bowline_core::ids::DeviceId>,
    pub(super) spool: &'a UploadPackSpoolFile,
}

pub(super) fn put_or_read_existing(
    byte_store: &dyn ByteStore,
    key: ObjectKey,
    kind: StorageObjectKind,
    content_id: &str,
    bytes: &[u8],
    key_epoch: u32,
    device_id: Option<&bowline_core::ids::DeviceId>,
) -> Result<UploadedObject, ByteStoreError> {
    match byte_store.put_object_with_content_id_at_epoch(
        key.clone(),
        kind,
        content_id,
        bytes,
        key_epoch,
        device_id,
    ) {
        Ok(metadata) => Ok(UploadedObject {
            metadata,
            wrote_object: true,
        }),
        Err(ByteStoreError::ObjectAlreadyExists(existing_key)) if existing_key == key => {
            let metadata = byte_store.head_object(&key)?;
            Ok(UploadedObject {
                metadata,
                wrote_object: false,
            })
        }
        Err(error) => Err(error),
    }
}

fn put_or_read_existing_streaming(
    byte_store: &dyn ByteStore,
    request: &UploadStreamingObjectRequest<'_>,
) -> Result<UploadedObject, ByteStoreError> {
    match byte_store.put_object_reader_with_content_id_at_epoch(PutObjectReaderRequest {
        key: request.key.clone(),
        kind: request.storage_kind,
        content_id: ObjectContentId::from_pack_id(&PackId::new(request.content_id)),
        source: request.spool,
        byte_len: request.byte_len,
        expected_hash: ObjectHash::from_stable_hash(request.hash.to_string()),
        key_epoch: request.key_epoch,
        created_by_device_id: request.device_id,
    }) {
        Ok(metadata) => Ok(UploadedObject {
            metadata,
            wrote_object: true,
        }),
        Err(ByteStoreError::ObjectAlreadyExists(existing_key)) if existing_key == request.key => {
            let metadata = byte_store.head_object(&request.key)?;
            Ok(UploadedObject {
                metadata,
                wrote_object: false,
            })
        }
        Err(error) => Err(error),
    }
}

pub(super) fn validate_uploaded_metadata(
    metadata: &ObjectMetadata,
    key: &ObjectKey,
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

fn validate_streaming_uploaded_metadata(
    metadata: &ObjectMetadata,
    key: &ObjectKey,
    kind: StorageObjectKind,
    byte_len: u64,
    hash: &str,
    key_epoch: u32,
) -> Result<(), UploadError> {
    if metadata.key != *key
        || metadata.kind != kind
        || metadata.byte_len != byte_len
        || metadata.hash != hash
        || metadata.key_epoch != key_epoch
    {
        return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "object metadata",
            reason: "committed object metadata does not match deterministic streaming upload",
        }));
    }
    Ok(())
}

fn validate_reusable_manifest_metadata(
    metadata: &ObjectMetadata,
    key: &ObjectKey,
    key_epoch: u32,
) -> Result<(), UploadError> {
    if &metadata.key != key
        || metadata.kind != StorageObjectKind::SnapshotManifest
        || metadata.key_epoch != key_epoch
    {
        return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "snapshot manifest object metadata",
            reason: "existing snapshot manifest object metadata does not match snapshot manifest upload",
        }));
    }
    Ok(())
}

fn validate_reusable_manifest_object(
    byte_store: &dyn ByteStore,
    metadata: &ObjectMetadata,
    key: &ObjectKey,
    reusable: ReusableSnapshotManifest<'_>,
) -> Result<(), UploadError> {
    validate_reusable_manifest_metadata(metadata, key, reusable.pointer.key_epoch)?;
    let bytes = byte_store.get_object(key)?;
    let sealed = SealedSnapshotManifest {
        pointer: ManifestPointer {
            manifest_id: reusable.pointer.manifest_id.clone(),
            snapshot_id: reusable.pointer.snapshot_id.clone(),
            object_key: metadata.key.clone(),
            byte_len: metadata.byte_len,
            hash: metadata.hash.clone(),
            key_epoch: metadata.key_epoch,
            kind: ManifestPointerKind::Snapshot,
        },
        bytes,
    };
    let existing_manifest = open_snapshot_manifest(
        &sealed,
        reusable.storage_key,
        &reusable.manifest.workspace_id,
    )?;
    if existing_manifest != *reusable.manifest {
        return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "snapshot manifest object",
            reason: "existing snapshot manifest object does not match snapshot manifest upload",
        }));
    }
    Ok(())
}
