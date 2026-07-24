use super::generated::{
    HostedObjectKind, HostedObjectMetadata, HostedObjectPointerInput,
    HostedObjectQueriesGetObjectMetadataRequest, HostedObjectsCommitUploadedObjectMetadataRequest,
    HostedObjectsCreateDownloadIntentRequest, HostedObjectsCreateStorageGcDeleteIntentRequest,
    HostedObjectsCreateUploadIntentRequest, HostedObjectsCreateUploadVerificationIntentRequest,
    HostedObjectsMarkObjectRetentionStateRequest,
    HostedRetentionDeleteObjectMetadataAfterGcRequest, HostedRetentionListStorageGcObjectsRequest,
    HostedRetentionState, HostedStorageGcObjectRef, ObjectQueriesGetObjectMetadata,
    ObjectsCommitUploadedObjectMetadata, ObjectsCreateDownloadIntent,
    ObjectsCreateStorageGcDeleteIntent, ObjectsCreateUploadIntent,
    ObjectsCreateUploadVerificationIntent, ObjectsMarkObjectRetentionState,
    RetentionDeleteObjectMetadataAfterGc, RetentionListStorageGcObjects,
};
use super::*;
use crate::ObjectControlPlaneClient;

impl ObjectControlPlaneClient for HostedControlPlaneClient {
    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent> {
        let object_key = request.object_key.clone().unwrap_or_else(|| {
            self.generated_object_key(request.object_kind, &request.workspace_id)
        });
        let proof_subject = upload_intent_proof_subject(
            &object_key,
            request.object_kind,
            request.byte_len,
            &request.checksum_sha256,
            request
                .content_id
                .as_ref()
                .map(|content_id| content_id.as_str()),
        );
        let created_by_device_proof = self.device_proof(
            &request.workspace_id,
            "create-upload-intent",
            &proof_subject,
        )?;
        let typed_request = HostedObjectsCreateUploadIntentRequest {
            authority_format_version: CURRENT_SNAPSHOT_AUTHORITY_FORMAT_VERSION,
            byte_length: request.byte_len,
            checksum_sha256: request.checksum_sha256.as_str().to_string(),
            content_id: request
                .content_id
                .map(|content_id| content_id.as_str().to_string()),
            created_by_device_id: self.device_id.clone(),
            created_by_device_proof,
            kind: object_kind_to_dto(request.object_kind),
            object_key,
            workspace_id: request.workspace_id.as_str().to_string(),
        };
        let response = self.call::<ObjectsCreateUploadIntent>(&typed_request)?;
        Ok(UploadIntent {
            workspace_id: WorkspaceId::new(response.workspace_id),
            object_key: response.object_key,
            object_kind: object_kind_from_dto(response.kind),
            byte_len: response.byte_length,
            signed_url: signed_url_from_dto(response.signed_url, &response.expires_at)?,
        })
    }

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        let proof_subject = download_intent_proof_subject(&request.object_key, request.range);
        let requested_by_device_proof = self.device_proof(
            &request.workspace_id,
            "create-download-intent",
            &proof_subject,
        )?;
        let typed_request = HostedObjectsCreateDownloadIntentRequest {
            length: request.range.map(|range| range.length),
            object_key: request.object_key.clone(),
            offset: request.range.map(|range| range.offset),
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            workspace_id: request.workspace_id.as_str().to_string(),
        };
        let response = self.call::<ObjectsCreateDownloadIntent>(&typed_request)?;
        Ok(DownloadIntent {
            workspace_id: WorkspaceId::new(response.workspace_id),
            object_key: response.object_key,
            range: request.range,
            signed_url: signed_url_from_dto(response.signed_url, &response.expires_at)?,
        })
    }

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        let proof_subject = upload_verification_proof_subject(
            &request.object_key,
            request.byte_len,
            request
                .content_id
                .as_ref()
                .map(|content_id| content_id.as_str()),
        );
        let requested_by_device_proof = self.device_proof(
            &request.workspace_id,
            "verify-upload-intent",
            &proof_subject,
        )?;
        let typed_request = HostedObjectsCreateUploadVerificationIntentRequest {
            byte_length: request.byte_len,
            content_id: request
                .content_id
                .map(|content_id| content_id.as_str().to_string()),
            object_key: request.object_key.clone(),
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            workspace_id: request.workspace_id.as_str().to_string(),
        };
        let response = self.call::<ObjectsCreateUploadVerificationIntent>(&typed_request)?;
        Ok(DownloadIntent {
            workspace_id: WorkspaceId::new(response.workspace_id),
            object_key: response.object_key,
            range: None,
            signed_url: signed_url_from_dto(response.signed_url, &response.expires_at)?,
        })
    }

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<ObjectMetadata> {
        if update.retention_state == RetentionState::DeleteEligible {
            return Err(ControlPlaneError::Unsupported {
                capability: "hosted-object-retention",
                reason: "delete-eligible retention requires hosted GC authority.",
            });
        }
        let proof_subject =
            object_retention_proof_subject(&update.object_key, update.retention_state);
        let requested_by_device_proof = self.device_proof(
            &update.workspace_id,
            "mark-object-retention-state",
            &proof_subject,
        )?;
        let typed_request = HostedObjectsMarkObjectRetentionStateRequest {
            object_key: update.object_key,
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            retention_state: retention_state_to_dto(update.retention_state),
            workspace_id: update.workspace_id.as_str().to_string(),
        };
        object_metadata_from_dto(self.call::<ObjectsMarkObjectRetentionState>(&typed_request)?)
    }

    fn create_storage_gc_delete_intent(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<DeleteIntent> {
        let typed_request = HostedObjectsCreateStorageGcDeleteIntentRequest {
            auth_token: self.control_plane_token.clone(),
            object_key: object_key.to_string(),
            workspace_id: workspace_id.as_str().to_string(),
        };
        let response = self.call::<ObjectsCreateStorageGcDeleteIntent>(&typed_request)?;
        Ok(DeleteIntent {
            workspace_id: WorkspaceId::new(response.workspace_id),
            object_key: response.object_key,
            object_kind: object_kind_from_dto(response.kind),
            key_epoch: response.key_epoch,
            signed_url: signed_url_from_dto(response.signed_url, &response.expires_at)?,
        })
    }

    fn head_object_metadata(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<ObjectMetadata> {
        let requested_by_device_proof =
            self.device_proof(workspace_id, "head-object-metadata", object_key)?;
        let typed_request = HostedObjectQueriesGetObjectMetadataRequest {
            object_key: object_key.to_string(),
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            workspace_id: workspace_id.as_str().to_string(),
        };
        match self.call::<ObjectQueriesGetObjectMetadata>(&typed_request)? {
            Some(dto) => object_metadata_from_dto(dto),
            None => Err(ControlPlaneError::ObjectMissing {
                object_key: object_key.to_string(),
            }),
        }
    }

    fn list_storage_gc_objects(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<bowline_storage::StorageObjectRef>> {
        let mut cursor = None;
        let mut objects = Vec::new();

        loop {
            let request = HostedRetentionListStorageGcObjectsRequest {
                auth_token: self.control_plane_token.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                cursor: cursor.clone(),
            };
            let page = self.call::<RetentionListStorageGcObjects>(&request)?;
            for object in page.objects {
                objects.push(storage_gc_object_ref_from_dto(object)?);
            }
            if page.is_done {
                break;
            }
            cursor = Some(page.continue_cursor);
        }

        Ok(objects)
    }

    fn delete_object_metadata_after_gc(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<bool> {
        let request = HostedRetentionDeleteObjectMetadataAfterGcRequest {
            auth_token: self.control_plane_token.clone(),
            object_key: object_key.to_string(),
            workspace_id: workspace_id.as_str().to_string(),
        };
        Ok(self
            .call::<RetentionDeleteObjectMetadataAfterGc>(&request)?
            .deleted)
    }

    fn commit_uploaded_object_metadata(
        &self,
        commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<ObjectMetadata> {
        self.require_local_device(&commit.committed_by_device_id)?;
        let proof_subject = object_metadata_proof_subject(&commit.object);
        let committed_by_device_proof = self.device_proof(
            &commit.workspace_id,
            "commit-uploaded-object-metadata",
            &proof_subject,
        )?;
        let typed_request = HostedObjectsCommitUploadedObjectMetadataRequest {
            committed_by_device_id: commit.committed_by_device_id.as_str().to_string(),
            committed_by_device_proof,
            object: object_pointer_to_dto(&commit.object),
            workspace_id: commit.workspace_id.as_str().to_string(),
        };
        object_metadata_from_dto(self.call::<ObjectsCommitUploadedObjectMetadata>(&typed_request)?)
    }
}

/// Map the closed hosted object-kind enum onto the control-plane object taxonomy.
fn object_kind_from_dto(kind: HostedObjectKind) -> ObjectKind {
    match kind {
        HostedObjectKind::Blob => ObjectKind::Blob,
        HostedObjectKind::Manifest => ObjectKind::Manifest,
    }
}

/// Encode a control-plane object kind for the wire, matching
/// `ObjectKind::as_str` and the hand-assembled proof subjects.
fn object_kind_to_dto(kind: ObjectKind) -> HostedObjectKind {
    match kind {
        ObjectKind::Blob => HostedObjectKind::Blob,
        ObjectKind::Manifest => HostedObjectKind::Manifest,
    }
}

/// Map the hosted object-kind enum onto the storage object taxonomy used by
/// `ObjectMetadata`.
fn storage_object_kind_from_dto(kind: HostedObjectKind) -> StorageObjectKind {
    match kind {
        HostedObjectKind::Blob => StorageObjectKind::WorkspaceFileV1,
        HostedObjectKind::Manifest => StorageObjectKind::WorkspaceManifestV1,
    }
}

/// Encode a storage retention state for the wire. Wire values match the former
/// `retention_state_value` encoding one-to-one.
fn retention_state_to_dto(state: RetentionState) -> HostedRetentionState {
    match state {
        RetentionState::Pending => HostedRetentionState::Pending,
        RetentionState::Current => HostedRetentionState::Current,
        RetentionState::OrphanCandidate => HostedRetentionState::OrphanCandidate,
        RetentionState::Retained => HostedRetentionState::Retained,
        RetentionState::DeleteEligible => HostedRetentionState::DeleteEligible,
    }
}

/// Encode a control-plane object pointer for a typed commit request. Carries the
/// signed fields (objectKey, contentId, byteLength, hash, keyEpoch, kind); the
/// domain `created_at` is intentionally dropped because it is neither signed nor
/// read by the server, and its tick encoding is not a wire timestamp.
pub(super) fn object_pointer_to_dto(pointer: &ObjectPointer) -> HostedObjectPointerInput {
    HostedObjectPointerInput {
        object_key: pointer.object_key.clone(),
        content_id: pointer.content_id.as_str().to_string(),
        byte_length: pointer.byte_len,
        hash: pointer.hash.clone(),
        key_epoch: pointer.key_epoch,
        kind: object_kind_to_dto(pointer.kind),
    }
}

/// Build a signed-URL intent from a decoded URL and its canonical expiry string.
fn signed_url_from_dto(url: String, expires_at: &str) -> ControlPlaneResult<SignedUrlIntent> {
    Ok(SignedUrlIntent {
        url,
        expires_at: parse_control_timestamp(expires_at)
            .map_err(|error| add_field_context(error, "expiresAt"))?,
    })
}

/// Convert a decoded object-metadata DTO into the storage domain type,
/// re-validating the opaque object key and canonical timestamp at the boundary
/// just as the former `parse_storage_metadata` did.
fn object_metadata_from_dto(dto: HostedObjectMetadata) -> ControlPlaneResult<ObjectMetadata> {
    Ok(ObjectMetadata {
        key: StorageObjectKey::new(dto.object_key).map_err(|_| {
            ControlPlaneError::InvalidObjectKey {
                reason: "object keys must be sealed-hash b_/m_ keys",
            }
        })?,
        kind: storage_object_kind_from_dto(dto.kind),
        byte_len: dto.byte_length,
        hash: dto.hash,
        key_epoch: dto.key_epoch,
        created_by_device_id: None,
        created_at_unix_ms: parse_control_timestamp(&dto.created_at)
            .map_err(|error| add_field_context(error, "createdAt"))?
            .tick,
        retention_state: retention_state_from_dto(dto.retention_state),
        retain_until_unix_ms: None,
    })
}

/// Convert a decoded garbage-collection object DTO into the storage domain type,
/// re-validating the opaque object key at the boundary just as the former
/// `parse_storage_gc_object_ref` did.
fn storage_gc_object_ref_from_dto(
    dto: HostedStorageGcObjectRef,
) -> ControlPlaneResult<bowline_storage::StorageObjectRef> {
    let key = StorageObjectKey::new(dto.key).map_err(|_| ControlPlaneError::InvalidObjectKey {
        reason: "GC object keys must be sealed-hash b_/m_ keys",
    })?;
    Ok(bowline_storage::StorageObjectRef {
        key,
        retention_state: retention_state_from_dto(dto.retention_state),
        referenced_by_current_head: dto.referenced_by_current_head,
        referenced_by_snapshot: dto
            .referenced_by_snapshot
            .map(bowline_core::ids::SnapshotId::new),
        referenced_by_work_view_base: dto.referenced_by_work_view_base,
        referenced_by_active_overlay: dto.referenced_by_active_overlay,
        verified: dto.verified,
    })
}

fn retention_state_from_dto(state: HostedRetentionState) -> RetentionState {
    match state {
        HostedRetentionState::Pending => RetentionState::Pending,
        HostedRetentionState::Current => RetentionState::Current,
        HostedRetentionState::OrphanCandidate => RetentionState::OrphanCandidate,
        HostedRetentionState::Retained => RetentionState::Retained,
        HostedRetentionState::DeleteEligible => RetentionState::DeleteEligible,
    }
}

#[cfg(test)]
#[path = "objects/tests.rs"]
mod tests;
