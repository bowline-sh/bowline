use super::*;
use crate::ObjectControlPlaneClient;

impl ObjectControlPlaneClient for FakeControlPlaneClient {
    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent> {
        self.ensure_workspace(&request.workspace_id)?;

        let mut state = self.state.lock().expect("fake control plane poisoned");
        let idempotency_key = upload_idempotency_key(&request);
        if let Some(key) = idempotency_key.as_ref()
            && let Some(object_key) = state.upload_idempotency_keys.get(key)
        {
            let reservation = state
                .upload_reservations
                .get(&(request.workspace_id.clone(), object_key.clone()))
                .expect("idempotency key points at an upload reservation");
            if state
                .committed_object_keys
                .contains(&(request.workspace_id.clone(), object_key.clone()))
            {
                return Err(ControlPlaneError::Conflict {
                    resource: "upload intent",
                    reason: "object key is already committed",
                });
            }
            if reservation.matches_request(&request) {
                return Ok(reservation.intent.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "upload intent",
                reason: "idempotency key was reused with different metadata",
            });
        }

        let object_key = request
            .object_key
            .clone()
            .unwrap_or_else(|| generated_object_key(request.object_kind, self.clock.now().tick));
        validate_object_key(&object_key)?;
        let workspace_object_key = (request.workspace_id.clone(), object_key.clone());
        if state.committed_object_keys.contains(&workspace_object_key) {
            return Err(ControlPlaneError::Conflict {
                resource: "upload intent",
                reason: "object key is already committed",
            });
        }
        if let Some(reservation) = state.upload_reservations.get(&workspace_object_key) {
            if reservation.matches_request(&request) {
                return Ok(reservation.intent.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "upload intent",
                reason: "object key is already reserved with different metadata",
            });
        }

        let expires_at = self.clock.now();

        let intent = UploadIntent {
            workspace_id: request.workspace_id.clone(),
            object_key: object_key.clone(),
            object_kind: request.object_kind,
            byte_len: request.byte_len,
            signed_url: SignedUrlIntent {
                url: self.fake_signed_url("upload", &object_key, None),
                expires_at,
            },
        };
        state.upload_reservations.insert(
            workspace_object_key,
            UploadReservation {
                workspace_id: request.workspace_id,
                object_kind: request.object_kind,
                byte_len: request.byte_len,
                content_id: request.content_id,
                intent: intent.clone(),
            },
        );
        if let Some(key) = idempotency_key {
            state.upload_idempotency_keys.insert(key, object_key);
        }

        Ok(intent)
    }

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        self.ensure_workspace(&request.workspace_id)?;
        validate_object_key(&request.object_key)?;

        let state = self.state.lock().expect("fake control plane poisoned");
        if !state
            .object_keys
            .contains(&(request.workspace_id.clone(), request.object_key.clone()))
        {
            return Err(ControlPlaneError::ObjectMissing {
                object_key: request.object_key,
            });
        }
        drop(state);

        let expires_at = self.clock.now();
        Ok(DownloadIntent {
            workspace_id: request.workspace_id,
            object_key: request.object_key.clone(),
            range: request.range,
            signed_url: SignedUrlIntent {
                url: self.fake_signed_url("download", &request.object_key, request.range.as_ref()),
                expires_at,
            },
        })
    }

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        self.ensure_workspace(&request.workspace_id)?;
        validate_object_key(&request.object_key)?;

        let state = self.state.lock().expect("fake control plane poisoned");
        let reservation = state
            .upload_reservations
            .get(&(request.workspace_id.clone(), request.object_key.clone()))
            .ok_or_else(|| ControlPlaneError::ObjectMissing {
                object_key: request.object_key.clone(),
            })?;
        if reservation.workspace_id != request.workspace_id
            || reservation.byte_len != request.byte_len
            || reservation.content_id != request.content_id
        {
            return Err(ControlPlaneError::Conflict {
                resource: "upload verification",
                reason: "reservation metadata does not match verification request",
            });
        }
        drop(state);

        Ok(DownloadIntent {
            workspace_id: request.workspace_id,
            object_key: request.object_key.clone(),
            range: None,
            signed_url: SignedUrlIntent {
                url: self.fake_signed_url("verify-upload", &request.object_key, None),
                expires_at: self.clock.now(),
            },
        })
    }

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<ObjectMetadata> {
        self.ensure_workspace(&update.workspace_id)?;
        validate_object_key(&update.object_key)?;

        let mut state = self.state.lock().expect("fake control plane poisoned");
        let pointer = state
            .object_pointers
            .get(&update.workspace_id)
            .and_then(|pointers| {
                pointers
                    .iter()
                    .find(|pointer| pointer.object_key == update.object_key)
            })
            .ok_or_else(|| ControlPlaneError::ObjectMissing {
                object_key: update.object_key.clone(),
            })?;
        let mut metadata = pointer_storage_metadata(pointer)?;
        metadata.retention_state = update.retention_state;
        state.object_retention_states.insert(
            (update.workspace_id, update.object_key),
            update.retention_state,
        );
        Ok(metadata)
    }

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent> {
        self.ensure_workspace(&request.workspace_id)?;
        validate_object_key(&request.object_key)?;

        let state = self.state.lock().expect("fake control plane poisoned");
        let pointer = state
            .object_pointers
            .get(&request.workspace_id)
            .and_then(|pointers| {
                pointers
                    .iter()
                    .find(|pointer| pointer.object_key == request.object_key)
            })
            .ok_or_else(|| ControlPlaneError::ObjectMissing {
                object_key: request.object_key.clone(),
            })?;
        if let Some(expected_kind) = request.object_kind
            && pointer.kind != expected_kind
        {
            return Err(ControlPlaneError::Conflict {
                resource: "delete intent",
                reason: "object kind does not match committed metadata",
            });
        }
        if let Some(expected_epoch) = request.key_epoch
            && pointer.key_epoch != expected_epoch
        {
            return Err(ControlPlaneError::Conflict {
                resource: "delete intent",
                reason: "key epoch does not match committed metadata",
            });
        }
        if state
            .object_retention_states
            .get(&(request.workspace_id.clone(), request.object_key.clone()))
            .copied()
            != Some(RetentionState::DeleteEligible)
        {
            return Err(ControlPlaneError::Conflict {
                resource: "delete intent",
                reason: "object is not delete-eligible",
            });
        }
        Ok(DeleteIntent {
            workspace_id: request.workspace_id,
            object_key: request.object_key.clone(),
            object_kind: pointer.kind,
            key_epoch: pointer.key_epoch,
            signed_url: SignedUrlIntent {
                url: self.fake_signed_url("delete", &request.object_key, None),
                expires_at: self.clock.now(),
            },
        })
    }

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<ObjectMetadata> {
        self.ensure_workspace(workspace_id)?;
        validate_object_key(object_key)?;

        let state = self.state.lock().expect("fake control plane poisoned");
        let pointer = state
            .object_pointers
            .get(workspace_id)
            .and_then(|pointers| {
                pointers
                    .iter()
                    .find(|pointer| pointer.object_key == object_key)
            })
            .ok_or_else(|| ControlPlaneError::ObjectMissing {
                object_key: object_key.to_string(),
            })?;

        let mut metadata = pointer_storage_metadata(pointer)?;
        if let Some(retention_state) = state
            .object_retention_states
            .get(&(workspace_id.to_string(), object_key.to_string()))
            .copied()
        {
            metadata.retention_state = retention_state;
        }
        Ok(metadata)
    }

    fn commit_uploaded_object_metadata(
        &self,
        commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<ObjectMetadata> {
        self.ensure_workspace(&commit.workspace_id)?;
        validate_object_key(&commit.object.object_key)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &commit.workspace_id,
            Some(&commit.committed_by_device_id),
        )?;
        validate_committed_pointer(
            &state,
            &commit.workspace_id,
            &commit.object,
            commit.object.kind,
        )?;
        state.object_keys.insert((
            commit.workspace_id.clone(),
            commit.object.object_key.clone(),
        ));
        state.committed_object_keys.insert((
            commit.workspace_id.clone(),
            commit.object.object_key.clone(),
        ));
        state.object_retention_states.insert(
            (
                commit.workspace_id.clone(),
                commit.object.object_key.clone(),
            ),
            RetentionState::Current,
        );
        state
            .object_pointers
            .entry(commit.workspace_id.clone())
            .or_default()
            .push(commit.object.clone());
        pointer_storage_metadata(&commit.object)
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord> {
        self.ensure_workspace(&commit.workspace_id)?;
        validate_object_key(&commit.manifest_object.object_key)?;
        if commit.manifest_object.kind != ObjectKind::SnapshotManifest {
            return Err(ControlPlaneError::InvalidObjectKey {
                reason: "manifest commits must point at a manifest object",
            });
        }

        for pointer in &commit.pack_objects {
            validate_object_key(&pointer.object_key)?;
            if pointer.kind != ObjectKind::SourcePack {
                return Err(ControlPlaneError::InvalidObjectKey {
                    reason: "manifest pack entries must point at pack objects",
                });
            }
        }

        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &commit.workspace_id,
            Some(&commit.committed_by_device_id),
        )?;
        let manifest_key = (commit.workspace_id.clone(), commit.manifest_id.clone());
        let snapshot_key = (commit.workspace_id.clone(), commit.snapshot_id.clone());
        if let Some(existing_manifest_id) = state.manifests_by_snapshot.get(&snapshot_key)
            && existing_manifest_id != &commit.manifest_id
        {
            return Err(ControlPlaneError::Conflict {
                resource: "object manifest",
                reason: "snapshot is already committed with a different manifest ID",
            });
        }
        if let Some(existing) = state.object_manifests.get(&manifest_key) {
            if manifest_commit_matches(existing, &commit) {
                return Ok(existing.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "object manifest",
                reason: "manifest ID already exists with different object metadata",
            });
        }
        validate_committed_pointer(
            &state,
            &commit.workspace_id,
            &commit.manifest_object,
            ObjectKind::SnapshotManifest,
        )?;
        for pointer in &commit.pack_objects {
            validate_committed_pointer(
                &state,
                &commit.workspace_id,
                pointer,
                ObjectKind::SourcePack,
            )?;
        }

        let record = ObjectManifestRecord {
            workspace_id: commit.workspace_id.clone(),
            snapshot_id: commit.snapshot_id.clone(),
            manifest_id: commit.manifest_id.clone(),
            manifest_object: commit.manifest_object.clone(),
            pack_objects: commit.pack_objects.clone(),
            committed_by_device_id: commit.committed_by_device_id,
            committed_at: self.clock.now(),
        };

        let event = self.build_event(
            &record.workspace_id,
            CompactEventKind::ObjectManifestCommitted,
            &record.manifest_id,
        );

        state.object_keys.insert((
            record.workspace_id.clone(),
            record.manifest_object.object_key.clone(),
        ));
        state.committed_object_keys.insert((
            record.workspace_id.clone(),
            record.manifest_object.object_key.clone(),
        ));
        state.object_retention_states.insert(
            (
                record.workspace_id.clone(),
                record.manifest_object.object_key.clone(),
            ),
            RetentionState::Current,
        );
        state
            .object_pointers
            .entry(record.workspace_id.clone())
            .or_default()
            .push(record.manifest_object.clone());
        for pointer in &record.pack_objects {
            state
                .object_keys
                .insert((record.workspace_id.clone(), pointer.object_key.clone()));
            state
                .committed_object_keys
                .insert((record.workspace_id.clone(), pointer.object_key.clone()));
            state.object_retention_states.insert(
                (record.workspace_id.clone(), pointer.object_key.clone()),
                RetentionState::Current,
            );
            state
                .object_pointers
                .entry(record.workspace_id.clone())
                .or_default()
                .push(pointer.clone());
        }
        state.object_manifests.insert(manifest_key, record.clone());
        state
            .manifests_by_snapshot
            .insert(snapshot_key, record.manifest_id.clone());
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(event);

        Ok(record)
    }

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            workspace_id,
            self.local_device_id.as_deref(),
        )?;
        Ok(state
            .manifests_by_snapshot
            .get(&(workspace_id.to_string(), snapshot_id.to_string()))
            .and_then(|manifest_id| {
                state
                    .object_manifests
                    .get(&(workspace_id.to_string(), manifest_id.clone()))
            })
            .cloned())
    }
}
