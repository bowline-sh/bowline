use super::generated::{
    HostedMetadataBindingInput, HostedMetadataBindingOutcome, HostedMetadataBindingRecord,
    HostedMetadataPagePointer, HostedMetadataPagePointerInput, HostedMetadataRecordKind,
    HostedMetadataSidecar, HostedObjectKind, HostedObjectMetadata, HostedObjectPointer,
    HostedObjectPointerInput, HostedObjectQueriesGetObjectMetadataRequest,
    HostedObjectQueriesGetSnapshotRootRequest, HostedObjectQueriesResolveMetadataBindingsRequest,
    HostedObjectsCommitMetadataBindingsRequest, HostedObjectsCommitSnapshotRootRequest,
    HostedObjectsCommitUploadedObjectMetadataRequest, HostedObjectsCreateDownloadIntentRequest,
    HostedObjectsCreateStorageGcDeleteIntentRequest, HostedObjectsCreateUploadIntentRequest,
    HostedObjectsCreateUploadVerificationIntentRequest,
    HostedObjectsMarkObjectRetentionStateRequest,
    HostedRetentionDeleteObjectMetadataAfterGcRequest, HostedRetentionListStorageGcObjectsRequest,
    HostedRetentionState, HostedSnapshotManifestPointer, HostedSnapshotManifestPointerInput,
    HostedSnapshotRootRecord, HostedStorageGcObjectRef, ObjectQueriesGetObjectMetadata,
    ObjectQueriesGetSnapshotRoot, ObjectQueriesResolveMetadataBindings,
    ObjectsCommitMetadataBindings, ObjectsCommitSnapshotRoot, ObjectsCommitUploadedObjectMetadata,
    ObjectsCreateDownloadIntent, ObjectsCreateStorageGcDeleteIntent, ObjectsCreateUploadIntent,
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

    fn commit_metadata_bindings(
        &self,
        commit: MetadataBindingCommit,
    ) -> ControlPlaneResult<MetadataBindingBatch> {
        self.require_local_device(&commit.committed_by_device_id)?;
        let proof_subject = metadata_bindings_proof_subject(&commit.bindings);
        let committed_by_device_proof = self.device_proof(
            &commit.workspace_id,
            "commit-metadata-bindings",
            &proof_subject,
        )?;
        let typed_request = HostedObjectsCommitMetadataBindingsRequest {
            bindings: commit
                .bindings
                .iter()
                .map(metadata_binding_to_dto)
                .collect(),
            committed_by_device_id: commit.committed_by_device_id.as_str().to_string(),
            committed_by_device_proof,
            workspace_id: commit.workspace_id.as_str().to_string(),
        };
        let response = self.call::<ObjectsCommitMetadataBindings>(&typed_request)?;
        metadata_binding_commit_response(response, &commit)
    }

    fn resolve_metadata_bindings(
        &self,
        workspace_id: &WorkspaceId,
        logical_ids: &[String],
    ) -> ControlPlaneResult<MetadataBindingBatch> {
        let requested_by_device_proof = self.device_proof(
            workspace_id,
            "resolve-metadata-bindings",
            &resolve_metadata_bindings_proof_subject(logical_ids),
        )?;
        let typed_request = HostedObjectQueriesResolveMetadataBindingsRequest {
            logical_ids: logical_ids.to_vec(),
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            workspace_id: workspace_id.as_str().to_string(),
        };
        let response = self.call::<ObjectQueriesResolveMetadataBindings>(&typed_request)?;
        metadata_binding_resolve_response(response, workspace_id, logical_ids)
    }

    fn commit_snapshot_root(
        &self,
        commit: SnapshotRootCommit,
    ) -> ControlPlaneResult<SnapshotRootRecord> {
        self.require_local_device(&commit.committed_by_device_id)?;
        let committed_by_device_proof = self.device_proof(
            &commit.workspace_id,
            "commit-snapshot-root",
            &snapshot_root_proof_subject(&commit),
        )?;
        let typed_request = HostedObjectsCommitSnapshotRootRequest {
            committed_by_device_id: commit.committed_by_device_id.as_str().to_string(),
            committed_by_device_proof,
            extra_root_logical_ids: commit.extra_root_logical_ids.clone(),
            manifest_id: commit.manifest_id.as_str().to_string(),
            manifest_object: snapshot_manifest_pointer_to_dto(&commit.manifest_object),
            namespace_root_id: commit.namespace_root_id.clone(),
            snapshot_id: commit.snapshot_id.as_str().to_string(),
            workspace_id: commit.workspace_id.as_str().to_string(),
        };
        let response = self.call::<ObjectsCommitSnapshotRoot>(&typed_request)?;
        snapshot_root_from_dto(response, Some(&commit))
    }

    fn get_snapshot_root(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> ControlPlaneResult<Option<SnapshotRootRecord>> {
        let requested_by_device_proof = self.device_proof(
            workspace_id,
            "get-snapshot-root",
            &snapshot_root_query_proof_subject(snapshot_id.as_str()),
        )?;
        let typed_request = HostedObjectQueriesGetSnapshotRootRequest {
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            snapshot_id: snapshot_id.as_str().to_string(),
            workspace_id: workspace_id.as_str().to_string(),
        };
        match self.call::<ObjectQueriesGetSnapshotRoot>(&typed_request)? {
            Some(dto) => snapshot_root_from_dto(dto, None).and_then(|record| {
                ensure_equal(
                    "workspaceId",
                    record.workspace_id.as_str(),
                    workspace_id.as_str(),
                )?;
                ensure_equal(
                    "snapshotId",
                    record.snapshot_id.as_str(),
                    snapshot_id.as_str(),
                )?;
                Ok(Some(record))
            }),
            None => Ok(None),
        }
    }
}

/// Map the closed hosted object-kind enum onto the control-plane object taxonomy.
fn object_kind_from_dto(kind: HostedObjectKind) -> ObjectKind {
    match kind {
        HostedObjectKind::SourcePack => ObjectKind::SourcePack,
        HostedObjectKind::LocatorIndex => ObjectKind::LocatorIndex,
        HostedObjectKind::SnapshotManifest => ObjectKind::SnapshotManifest,
        HostedObjectKind::SnapshotMetadataPage => ObjectKind::SnapshotMetadataPage,
        HostedObjectKind::AgentOverlay => ObjectKind::AgentOverlay,
        HostedObjectKind::ConflictBundle => ObjectKind::ConflictBundle,
    }
}

/// Encode a control-plane object kind for the wire. AgentOverlay serializes to
/// the canonical `overlay-pack` value, matching `ObjectKind::as_str` and the
/// hand-assembled proof subjects.
fn object_kind_to_dto(kind: ObjectKind) -> HostedObjectKind {
    match kind {
        ObjectKind::SourcePack => HostedObjectKind::SourcePack,
        ObjectKind::LocatorIndex => HostedObjectKind::LocatorIndex,
        ObjectKind::SnapshotManifest => HostedObjectKind::SnapshotManifest,
        ObjectKind::SnapshotMetadataPage => HostedObjectKind::SnapshotMetadataPage,
        ObjectKind::AgentOverlay => HostedObjectKind::AgentOverlay,
        ObjectKind::ConflictBundle => HostedObjectKind::ConflictBundle,
    }
}

/// Map the hosted object-kind enum onto the storage object taxonomy used by
/// `ObjectMetadata`.
fn storage_object_kind_from_dto(kind: HostedObjectKind) -> StorageObjectKind {
    match kind {
        HostedObjectKind::SourcePack => StorageObjectKind::SourcePack,
        HostedObjectKind::LocatorIndex => StorageObjectKind::LocatorIndex,
        HostedObjectKind::SnapshotManifest => StorageObjectKind::SnapshotManifest,
        HostedObjectKind::SnapshotMetadataPage => StorageObjectKind::SnapshotMetadataPage,
        HostedObjectKind::AgentOverlay => StorageObjectKind::AgentOverlay,
        HostedObjectKind::ConflictBundle => StorageObjectKind::ConflictBundle,
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
                reason: "object keys must be generated opaque pack, manifest, or overlay keys",
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

/// Convert a decoded object-pointer DTO into the control-plane domain type,
/// re-validating the canonical timestamp at the boundary just as the former
/// `parse_object_pointer` did.
pub(super) fn object_pointer_from_dto(
    dto: HostedObjectPointer,
) -> ControlPlaneResult<ObjectPointer> {
    Ok(ObjectPointer {
        object_key: dto.object_key,
        content_id: ContentId::new(dto.content_id),
        byte_len: dto.byte_length,
        hash: dto.hash,
        key_epoch: dto.key_epoch,
        kind: object_kind_from_dto(dto.kind),
        created_at: parse_control_timestamp(&dto.created_at)
            .map_err(|error| add_field_context(error, "createdAt"))?,
    })
}

/// Convert a decoded garbage-collection object DTO into the storage domain type,
/// re-validating the opaque object key at the boundary just as the former
/// `parse_storage_gc_object_ref` did.
fn storage_gc_object_ref_from_dto(
    dto: HostedStorageGcObjectRef,
) -> ControlPlaneResult<bowline_storage::StorageObjectRef> {
    let key = StorageObjectKey::new(dto.key).map_err(|_| ControlPlaneError::InvalidObjectKey {
        reason: "GC object keys must be generated opaque pack, manifest, or overlay keys",
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
        referenced_by_active_lease: dto.referenced_by_active_lease,
        referenced_by_conflict_bundle: dto.referenced_by_conflict_bundle,
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
fn metadata_binding_to_dto(binding: &MetadataBindingInput) -> HostedMetadataBindingInput {
    HostedMetadataBindingInput {
        logical_id: binding.logical_id.clone(),
        object: HostedMetadataPagePointerInput {
            byte_length: binding.object.byte_len,
            content_id: binding.object.content_id.as_str().to_string(),
            hash: binding.object.hash.clone(),
            key_epoch: binding.object.key_epoch,
            kind: "snapshot-metadata-page".to_string(),
            object_key: binding.object.object_key.clone(),
        },
        record_kind: metadata_record_kind_to_dto(binding.record_kind),
        sidecar: metadata_sidecar_to_dto(&binding.sidecar),
    }
}

fn metadata_sidecar_to_dto(sidecar: &MetadataSidecar) -> HostedMetadataSidecar {
    HostedMetadataSidecar {
        child_logical_ids: sidecar.child_logical_ids.clone(),
        direct_object_keys: sidecar.direct_object_keys.clone(),
        digest: sidecar.digest.clone(),
    }
}

fn metadata_record_kind_to_dto(kind: MetadataRecordKind) -> HostedMetadataRecordKind {
    match kind {
        MetadataRecordKind::NamespacePage => HostedMetadataRecordKind::NamespacePage,
        MetadataRecordKind::ContentLayout => HostedMetadataRecordKind::ContentLayout,
        MetadataRecordKind::SegmentPage => HostedMetadataRecordKind::SegmentPage,
    }
}

fn metadata_record_kind_from_dto(kind: HostedMetadataRecordKind) -> MetadataRecordKind {
    match kind {
        HostedMetadataRecordKind::NamespacePage => MetadataRecordKind::NamespacePage,
        HostedMetadataRecordKind::ContentLayout => MetadataRecordKind::ContentLayout,
        HostedMetadataRecordKind::SegmentPage => MetadataRecordKind::SegmentPage,
    }
}

fn metadata_binding_commit_response(
    response: super::generated::HostedMetadataBindingsResponse,
    commit: &MetadataBindingCommit,
) -> ControlPlaneResult<MetadataBindingBatch> {
    ensure_equal(
        "workspaceId",
        &response.workspace_id,
        commit.workspace_id.as_str(),
    )?;
    if response.bindings.len() != commit.bindings.len() {
        return Err(ControlPlaneError::Storage(
            "binding commit response must cover every requested logical id exactly once"
                .to_string(),
        ));
    }
    let inputs = commit
        .bindings
        .iter()
        .map(|binding| (binding.logical_id.as_str(), binding))
        .collect::<std::collections::BTreeMap<_, _>>();
    if inputs.len() != commit.bindings.len() {
        return Err(ControlPlaneError::Storage(
            "binding commit request contains duplicate logical ids".to_string(),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut bindings = Vec::with_capacity(response.bindings.len());
    for dto in response.bindings {
        if !seen.insert(dto.logical_id.clone()) {
            return Err(ControlPlaneError::Storage(
                "binding commit response contains duplicate logical ids".to_string(),
            ));
        }
        let input = inputs.get(dto.logical_id.as_str()).ok_or_else(|| {
            ControlPlaneError::Storage("binding result was not requested".to_string())
        })?;
        if metadata_record_kind_from_dto(dto.record_kind) != input.record_kind {
            return Err(ControlPlaneError::Storage(
                "binding result record kind differs from request".to_string(),
            ));
        }
        let outcome = dto.outcome.ok_or_else(|| {
            ControlPlaneError::Storage("binding commit response omitted outcome".to_string())
        })?;
        let record = metadata_binding_record_from_dto(dto, Some(outcome))?;
        ensure_equal(
            "sidecar.digest",
            &record.sidecar.digest,
            &input.sidecar.digest,
        )?;
        bindings.push(record);
    }
    Ok(MetadataBindingBatch {
        workspace_id: commit.workspace_id.clone(),
        bindings,
    })
}

fn metadata_binding_resolve_response(
    response: super::generated::HostedMetadataBindingsResponse,
    workspace_id: &WorkspaceId,
    logical_ids: &[String],
) -> ControlPlaneResult<MetadataBindingBatch> {
    ensure_equal("workspaceId", &response.workspace_id, workspace_id.as_str())?;
    let requested = logical_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let mut seen = std::collections::BTreeSet::new();
    let mut bindings = Vec::with_capacity(response.bindings.len());
    for dto in response.bindings {
        if !requested.contains(dto.logical_id.as_str()) || !seen.insert(dto.logical_id.clone()) {
            return Err(ControlPlaneError::Storage(
                "resolved binding must be a unique requested logical id".to_string(),
            ));
        }
        if dto.outcome.is_some() {
            return Err(ControlPlaneError::Storage(
                "resolved binding unexpectedly included a commit outcome".to_string(),
            ));
        }
        bindings.push(metadata_binding_record_from_dto(dto, None)?);
    }
    Ok(MetadataBindingBatch {
        workspace_id: workspace_id.clone(),
        bindings,
    })
}

fn metadata_binding_record_from_dto(
    dto: HostedMetadataBindingRecord,
    outcome: Option<HostedMetadataBindingOutcome>,
) -> ControlPlaneResult<MetadataBindingRecord> {
    let sidecar = dto.sidecar.ok_or_else(|| {
        ControlPlaneError::Storage("metadata binding response omitted sidecar".to_string())
    })?;
    if let Some(digest) = dto.sidecar_digest.as_deref() {
        ensure_equal("sidecarDigest", digest, &sidecar.digest)?;
    }
    ensure_equal("object.contentId", &dto.object.content_id, &dto.logical_id)?;
    let object = metadata_page_pointer_from_dto(dto.object)?;
    Ok(MetadataBindingRecord {
        logical_id: dto.logical_id,
        record_kind: metadata_record_kind_from_dto(dto.record_kind),
        object,
        sidecar: MetadataSidecar {
            child_logical_ids: sidecar.child_logical_ids,
            direct_object_keys: sidecar.direct_object_keys,
            digest: sidecar.digest,
        },
        outcome: outcome.map(|value| match value {
            HostedMetadataBindingOutcome::BoundNew => MetadataBindingOutcome::BoundNew,
            HostedMetadataBindingOutcome::ExistingWinner => MetadataBindingOutcome::ExistingWinner,
        }),
    })
}

fn metadata_page_pointer_from_dto(
    dto: HostedMetadataPagePointer,
) -> ControlPlaneResult<ObjectPointer> {
    ensure_equal("object.kind", &dto.kind, "snapshot-metadata-page")?;
    Ok(ObjectPointer {
        object_key: dto.object_key,
        content_id: ContentId::new(dto.content_id),
        byte_len: dto.byte_length,
        hash: dto.hash,
        key_epoch: dto.key_epoch,
        kind: ObjectKind::SnapshotMetadataPage,
        created_at: parse_control_timestamp(&dto.created_at)
            .map_err(|error| add_field_context(error, "createdAt"))?,
    })
}

fn snapshot_manifest_pointer_to_dto(pointer: &ObjectPointer) -> HostedSnapshotManifestPointerInput {
    HostedSnapshotManifestPointerInput {
        byte_length: pointer.byte_len,
        content_id: pointer.content_id.as_str().to_string(),
        hash: pointer.hash.clone(),
        key_epoch: pointer.key_epoch,
        kind: "snapshot-manifest".to_string(),
        object_key: pointer.object_key.clone(),
    }
}

fn snapshot_manifest_pointer_from_dto(
    dto: HostedSnapshotManifestPointer,
) -> ControlPlaneResult<ObjectPointer> {
    ensure_equal("manifestObject.kind", &dto.kind, "snapshot-manifest")?;
    Ok(ObjectPointer {
        object_key: dto.object_key,
        content_id: ContentId::new(dto.content_id),
        byte_len: dto.byte_length,
        hash: dto.hash,
        key_epoch: dto.key_epoch,
        kind: ObjectKind::SnapshotManifest,
        created_at: parse_control_timestamp(&dto.created_at)
            .map_err(|error| add_field_context(error, "manifestObject.createdAt"))?,
    })
}

fn snapshot_root_from_dto(
    dto: HostedSnapshotRootRecord,
    expected: Option<&SnapshotRootCommit>,
) -> ControlPlaneResult<SnapshotRootRecord> {
    if !dto.complete {
        return Err(ControlPlaneError::Storage(
            "snapshot root response is incomplete".to_string(),
        ));
    }
    if let Some(commit) = expected {
        ensure_equal(
            "workspaceId",
            &dto.workspace_id,
            commit.workspace_id.as_str(),
        )?;
        ensure_equal("snapshotId", &dto.snapshot_id, commit.snapshot_id.as_str())?;
        ensure_equal("manifestId", &dto.manifest_id, commit.manifest_id.as_str())?;
        ensure_equal(
            "namespaceRootId",
            &dto.namespace_root_id,
            &commit.namespace_root_id,
        )?;
        if dto.extra_root_logical_ids != commit.extra_root_logical_ids {
            return Err(ControlPlaneError::Storage(
                "snapshot root response changed extra roots".to_string(),
            ));
        }
    }
    Ok(SnapshotRootRecord {
        workspace_id: WorkspaceId::new(dto.workspace_id),
        snapshot_id: SnapshotId::new(dto.snapshot_id),
        manifest_id: ManifestId::new(dto.manifest_id),
        manifest_object: snapshot_manifest_pointer_from_dto(dto.manifest_object)?,
        namespace_root_id: dto.namespace_root_id,
        extra_root_logical_ids: dto.extra_root_logical_ids,
        complete: dto.complete,
        committed_by_device_id: DeviceId::new(dto.committed_by_device_id),
        committed_at: parse_control_timestamp(&dto.committed_at)
            .map_err(|error| add_field_context(error, "committedAt"))?,
    })
}

fn ensure_equal(field: &'static str, actual: &str, expected: &str) -> ControlPlaneResult<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(ControlPlaneError::Storage(format!(
            "{field} did not match the requested semantic identity"
        )))
    }
}
