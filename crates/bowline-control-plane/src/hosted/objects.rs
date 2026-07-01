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
            request.content_id.as_deref(),
        );
        let created_by_device_proof = self.device_proof(
            &request.workspace_id,
            "create-upload-intent",
            &proof_subject,
        )?;
        let mut request_args = args([
            ("byteLength", number_value(request.byte_len)),
            ("createdByDeviceId", Value::from(self.device_id.clone())),
            ("createdByDeviceProof", Value::from(created_by_device_proof)),
            ("kind", Value::from(request.object_kind.as_str())),
            ("objectKey", Value::from(object_key)),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(content_id) = request.content_id {
            request_args.insert("contentId".to_string(), Value::from(content_id));
        }

        let value = self.public_action("objects:createUploadIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(UploadIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            object_kind: parse_object_kind(&string_field(object, "kind")?)?,
            byte_len: u64_field(object, "byteLength")?,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
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
        let mut request_args = args([
            ("objectKey", Value::from(request.object_key.clone())),
            ("requestedByDeviceId", Value::from(self.device_id.clone())),
            (
                "requestedByDeviceProof",
                Value::from(requested_by_device_proof),
            ),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(range) = request.range {
            request_args.insert("offset".to_string(), number_value(range.offset));
            request_args.insert("length".to_string(), number_value(range.length));
        }

        let value = self.public_action("objects:createDownloadIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(DownloadIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            range: request.range,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
        })
    }

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        let proof_subject = upload_verification_proof_subject(
            &request.object_key,
            request.byte_len,
            request.content_id.as_deref(),
        );
        let requested_by_device_proof = self.device_proof(
            &request.workspace_id,
            "verify-upload-intent",
            &proof_subject,
        )?;
        let mut request_args = args([
            ("byteLength", number_value(request.byte_len)),
            ("objectKey", Value::from(request.object_key.clone())),
            ("requestedByDeviceId", Value::from(self.device_id.clone())),
            (
                "requestedByDeviceProof",
                Value::from(requested_by_device_proof),
            ),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(content_id) = request.content_id {
            request_args.insert("contentId".to_string(), Value::from(content_id));
        }

        let value = self.public_action("objects:createUploadVerificationIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(DownloadIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            range: None,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
        })
    }

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<ObjectMetadata> {
        if update.retention_state == RetentionState::DeleteEligible {
            return Err(ControlPlaneError::Limited {
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
        let value = self.public_action(
            "objects:markObjectRetentionState",
            args([
                ("objectKey", Value::from(update.object_key.clone())),
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                (
                    "retentionState",
                    Value::from(retention_state_value(update.retention_state)),
                ),
                ("workspaceId", Value::from(update.workspace_id.clone())),
            ]),
        )?;
        parse_storage_metadata(&value)
    }

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent> {
        let proof_subject = delete_intent_proof_subject(&request);
        let requested_by_device_proof = self.device_proof(
            &request.workspace_id,
            "create-delete-intent",
            &proof_subject,
        )?;
        let mut request_args = args([
            ("objectKey", Value::from(request.object_key.clone())),
            ("requestedByDeviceId", Value::from(self.device_id.clone())),
            (
                "requestedByDeviceProof",
                Value::from(requested_by_device_proof),
            ),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(object_kind) = request.object_kind {
            request_args.insert("kind".to_string(), Value::from(object_kind.as_str()));
        }
        if let Some(key_epoch) = request.key_epoch {
            request_args.insert("keyEpoch".to_string(), number_value(u64::from(key_epoch)));
        }

        let value = self.public_action("objects:createDeleteIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(DeleteIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            object_kind: parse_object_kind(&string_field(object, "kind")?)?,
            key_epoch: u64_field(object, "keyEpoch")? as u32,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
        })
    }

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<ObjectMetadata> {
        let requested_by_device_proof =
            self.device_proof(workspace_id, "head-object-metadata", object_key)?;
        let value = self.public_query(
            "object_queries:getObjectMetadata",
            args([
                ("objectKey", Value::from(object_key.to_string())),
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        if matches!(value, Value::Null) {
            return Err(ControlPlaneError::ObjectMissing {
                object_key: object_key.to_string(),
            });
        }
        parse_storage_metadata(&value)
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
        let value = self.public_action(
            "objects:commitUploadedObjectMetadata",
            args([
                (
                    "committedByDeviceId",
                    Value::from(commit.committed_by_device_id),
                ),
                (
                    "committedByDeviceProof",
                    Value::from(committed_by_device_proof),
                ),
                ("object", object_pointer_value(&commit.object)),
                ("workspaceId", Value::from(commit.workspace_id)),
            ]),
        )?;
        parse_storage_metadata(&value)
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord> {
        self.require_local_device(&commit.committed_by_device_id)?;
        let proof_subject = object_manifest_proof_subject(&commit);
        let committed_by_device_proof = self.device_proof(
            &commit.workspace_id,
            "commit-object-manifest",
            &proof_subject,
        )?;
        let value = self.public_action(
            "objects:commitObjectManifest",
            args([
                (
                    "committedByDeviceId",
                    Value::from(commit.committed_by_device_id.clone()),
                ),
                (
                    "committedByDeviceProof",
                    Value::from(committed_by_device_proof),
                ),
                ("manifestId", Value::from(commit.manifest_id.clone())),
                ("snapshotId", Value::from(commit.snapshot_id.clone())),
                (
                    "manifestObject",
                    object_pointer_value(&commit.manifest_object),
                ),
                ("packObjects", object_pointer_array(&commit.pack_objects)),
                ("workspaceId", Value::from(commit.workspace_id.clone())),
            ]),
        )?;
        let object = value_object(&value)?;
        Ok(ObjectManifestRecord {
            workspace_id: string_field(object, "workspaceId")?,
            snapshot_id: string_field(object, "snapshotId")?,
            manifest_id: string_field(object, "manifestId")?,
            manifest_object: commit.manifest_object,
            pack_objects: commit.pack_objects,
            committed_by_device_id: string_field(object, "committedByDeviceId")?,
            committed_at: current_timestamp(),
        })
    }

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>> {
        let requested_by_device_proof = self.device_proof(
            workspace_id,
            "get-snapshot-manifest-pointer",
            &snapshot_manifest_pointer_proof_subject(snapshot_id),
        )?;
        let value = self.public_query(
            "object_queries:getSnapshotManifestPointer",
            args([
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("snapshotId", Value::from(snapshot_id.to_string())),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        parse_object_manifest_record(&value).map(Some)
    }
}
