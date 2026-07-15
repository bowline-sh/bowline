use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, ControlPlaneTimestamp, ObjectKind, ObjectMetadataCommit,
    ObjectPointer, UploadIntentRequest,
};
use bowline_core::ids::{ContentId, DeviceId, PackId};
use bowline_storage::{
    ByteStore, ByteStoreError, ObjectKind as StorageObjectKind, PackRecordInput, PackWriteOutput,
    PackWriter, StorageKey, stable_object_hash,
};

use super::WorkViewOverlaySyncError;

pub(super) fn derive_overlay_payload_pack(
    workspace_id: &bowline_core::ids::WorkspaceId,
    payload_bytes: &[u8],
    payload_content_id: ContentId,
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<PackWriteOutput, WorkViewOverlaySyncError> {
    let mut pack_id = blake3::Hasher::new();
    pack_id.update(b"bowline-overlay-pack-v2\0");
    pack_id.update(workspace_id.as_str().as_bytes());
    pack_id.update(payload_content_id.as_str().as_bytes());
    pack_id.update(&key_epoch.to_le_bytes());
    let writer = PackWriter::new(
        workspace_id.clone(),
        PackId::new(format!("pk_{}", pack_id.finalize().to_hex())),
        storage_key,
        key_epoch,
    );
    Ok(writer.write(&[PackRecordInput {
        content_id: payload_content_id,
        bytes: payload_bytes.to_vec(),
    }])?)
}

pub(super) fn overlay_pointer_matches_pack(
    pointer: &ObjectPointer,
    pack: &PackWriteOutput,
) -> bool {
    pointer.kind == ObjectKind::AgentOverlay
        && overlay_pack_payload_content_id(pack)
            .is_some_and(|content_id| pointer.content_id == content_id)
}

pub(super) fn overlay_pack_payload_content_id(pack: &PackWriteOutput) -> Option<&str> {
    pack.locators
        .first()
        .map(|locator| locator.content_id.as_str())
}

pub(super) struct UploadedOverlayObject {
    pub(super) pointer: ObjectPointer,
    pub(super) reused: bool,
}

pub(super) fn upload_overlay_payload_with_checkpoint(
    workspace_id: &bowline_core::ids::WorkspaceId,
    device_id: &DeviceId,
    pack: PackWriteOutput,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    key_epoch: u32,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<UploadedOverlayObject, WorkViewOverlaySyncError> {
    checkpoint()?;
    match control_plane.head_object_metadata(workspace_id, pack.object_key.as_str()) {
        Ok(metadata) => {
            validate_overlay_object_metadata(
                &metadata,
                &pack.object_key,
                pack.bytes.len() as u64,
                None,
                key_epoch,
            )?;
            checkpoint()?;
            let existing = byte_store.get_object(&pack.object_key)?;
            if existing.len() as u64 != metadata.byte_len
                || stable_object_hash(&existing) != metadata.hash
            {
                return Err(ByteStoreError::CorruptObject {
                    key: pack.object_key,
                    reason: "reused overlay object bytes do not match committed metadata",
                }
                .into());
            }
            return Ok(UploadedOverlayObject {
                reused: true,
                pointer: ObjectPointer {
                    object_key: pack.object_key.as_str().to_string(),
                    content_id: overlay_pack_payload_content_id(&pack)
                        .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?
                        .into(),
                    byte_len: metadata.byte_len,
                    hash: metadata.hash,
                    key_epoch: metadata.key_epoch,
                    kind: ObjectKind::AgentOverlay,
                    created_at: ControlPlaneTimestamp {
                        tick: metadata.created_at_unix_ms,
                    },
                },
            });
        }
        Err(ControlPlaneError::ObjectMissing { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    checkpoint()?;
    control_plane.create_upload_intent(
        UploadIntentRequest::new(
            workspace_id.as_str(),
            ObjectKind::AgentOverlay,
            pack.bytes.len() as u64,
        )
        .with_object_key(pack.object_key.as_str())
        .with_content_id(
            overlay_pack_payload_content_id(&pack)
                .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?,
        ),
    )?;

    checkpoint()?;
    let metadata = match byte_store.put_object_with_content_id_at_epoch(
        pack.object_key.clone(),
        StorageObjectKind::AgentOverlay,
        overlay_pack_payload_content_id(&pack)
            .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?,
        &pack.bytes,
        key_epoch,
        Some(device_id),
    ) {
        Ok(metadata) => metadata,
        Err(ByteStoreError::ObjectAlreadyExists(existing_key))
            if existing_key == pack.object_key =>
        {
            checkpoint()?;
            byte_store.head_object(&pack.object_key)?
        }
        Err(error) => return Err(error.into()),
    };
    validate_overlay_object_metadata(
        &metadata,
        &pack.object_key,
        pack.bytes.len() as u64,
        Some(&pack.bytes),
        key_epoch,
    )?;
    let pointer = ObjectPointer {
        object_key: pack.object_key.as_str().to_string(),
        content_id: overlay_pack_payload_content_id(&pack)
            .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?
            .into(),
        byte_len: metadata.byte_len,
        hash: metadata.hash,
        key_epoch: metadata.key_epoch,
        kind: ObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp {
            tick: metadata.created_at_unix_ms,
        },
    };
    checkpoint()?;
    control_plane.commit_uploaded_object_metadata(ObjectMetadataCommit {
        workspace_id: workspace_id.clone(),
        object: pointer.clone(),
        committed_by_device_id: device_id.clone(),
    })?;
    Ok(UploadedOverlayObject {
        pointer,
        reused: false,
    })
}

fn validate_overlay_object_metadata(
    metadata: &bowline_storage::ObjectMetadata,
    object_key: &bowline_storage::ObjectKey,
    expected_byte_len: u64,
    uploaded_bytes: Option<&[u8]>,
    key_epoch: u32,
) -> Result<(), WorkViewOverlaySyncError> {
    if metadata.key != *object_key
        || metadata.kind != StorageObjectKind::AgentOverlay
        || metadata.byte_len != expected_byte_len
        || uploaded_bytes
            .is_some_and(|bytes| metadata.hash != format!("b3_{}", blake3::hash(bytes).to_hex()))
        || metadata.key_epoch != key_epoch
    {
        return Err(ControlPlaneError::Conflict {
            resource: "overlay object metadata",
            reason: "stored overlay object metadata does not match encrypted pack bytes",
        }
        .into());
    }
    Ok(())
}
