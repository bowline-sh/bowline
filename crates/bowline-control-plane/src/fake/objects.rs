use super::*;
use crate::ObjectControlPlaneClient;

impl ObjectControlPlaneClient for FakeControlPlaneClient {
    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent> {
        self.ensure_workspace(&request.workspace_id)?;

        let mut state = self.state.lock().expect("fake control plane poisoned");
        state.upload_intent_requests.push(request.clone());
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
                checksum_sha256: request.checksum_sha256,
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
        if update.retention_state == RetentionState::DeleteEligible {
            return Err(ControlPlaneError::Conflict {
                resource: "object retention",
                reason: "delete-eligible retention requires GC authority",
            });
        }

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

    fn create_storage_gc_delete_intent(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<DeleteIntent> {
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
        if state
            .object_retention_states
            .get(&(workspace_id.clone(), object_key.to_string()))
            .copied()
            != Some(RetentionState::DeleteEligible)
        {
            return Err(ControlPlaneError::Conflict {
                resource: "storage GC delete intent",
                reason: "object is not delete-eligible",
            });
        }
        Ok(DeleteIntent {
            workspace_id: workspace_id.clone(),
            object_key: object_key.to_string(),
            object_kind: pointer.kind,
            key_epoch: pointer.key_epoch,
            signed_url: SignedUrlIntent {
                url: self.fake_signed_url("delete", object_key, None),
                expires_at: self.clock.now(),
            },
        })
    }

    fn head_object_metadata(
        &self,
        workspace_id: &WorkspaceId,
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
            .get(&(workspace_id.clone(), object_key.to_string()))
            .copied()
        {
            metadata.retention_state = retention_state;
        }
        Ok(metadata)
    }

    fn list_storage_gc_objects(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<bowline_storage::StorageObjectRef>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        let refs = state
            .object_pointers
            .get(workspace_id)
            .into_iter()
            .flatten()
            .map(|pointer| {
                let retention_state = state
                    .object_retention_states
                    .get(&(workspace_id.clone(), pointer.object_key.clone()))
                    .copied()
                    .unwrap_or(RetentionState::Current);
                Ok(bowline_storage::StorageObjectRef {
                    key: StorageObjectKey::new(pointer.object_key.clone()).map_err(|_| {
                        ControlPlaneError::InvalidObjectKey {
                            reason: "GC object key is invalid",
                        }
                    })?,
                    retention_state,
                    referenced_by_current_head: false,
                    referenced_by_snapshot: None,
                    referenced_by_work_view_base: false,
                    referenced_by_active_overlay: false,
                    verified: true,
                })
            })
            .collect::<ControlPlaneResult<Vec<_>>>()?;
        Ok(refs)
    }

    fn delete_object_metadata_after_gc(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<bool> {
        self.ensure_workspace(workspace_id)?;
        validate_object_key(object_key)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        if state
            .object_retention_states
            .get(&(workspace_id.clone(), object_key.to_string()))
            .copied()
            != Some(RetentionState::DeleteEligible)
        {
            return Err(ControlPlaneError::Conflict {
                resource: "storage GC metadata deletion",
                reason: "object is not delete-eligible",
            });
        }
        state
            .object_retention_states
            .remove(&(workspace_id.clone(), object_key.to_string()));
        state
            .committed_object_keys
            .remove(&(workspace_id.clone(), object_key.to_string()));
        state
            .object_keys
            .remove(&(workspace_id.clone(), object_key.to_string()));
        if let Some(pointers) = state.object_pointers.get_mut(workspace_id) {
            let before = pointers.len();
            pointers.retain(|pointer| pointer.object_key != object_key);
            return Ok(pointers.len() != before);
        }
        Ok(false)
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
        let already_committed = state.committed_object_keys.contains(&(
            commit.workspace_id.clone(),
            commit.object.object_key.clone(),
        ));
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
        // Idempotent re-commits must not duplicate the pointer list (GC lists
        // walk this Vec, so duplicates inflate storage GC results).
        if !already_committed {
            state
                .object_pointers
                .entry(commit.workspace_id.clone())
                .or_default()
                .push(commit.object.clone());
        }
        pointer_storage_metadata(&commit.object)
    }
}
