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
                    referenced_by_active_lease: false,
                    referenced_by_conflict_bundle: false,
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

    fn commit_metadata_bindings(
        &self,
        commit: MetadataBindingCommit,
    ) -> ControlPlaneResult<MetadataBindingBatch> {
        self.ensure_workspace(&commit.workspace_id)?;
        if commit.bindings.is_empty() || commit.bindings.len() > 16 {
            return Err(ControlPlaneError::Conflict {
                resource: "metadata binding batch",
                reason: "metadata binding batch exceeds the bounded contract",
            });
        }
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &commit.workspace_id,
            Some(&commit.committed_by_device_id),
        )?;
        let mut staged = state.clone();
        let mut records = Vec::with_capacity(commit.bindings.len());
        let mut logical_ids = BTreeSet::new();
        for binding in commit.bindings {
            validate_metadata_binding_input(&binding)?;
            if !logical_ids.insert(binding.logical_id.clone()) {
                return Err(ControlPlaneError::Conflict {
                    resource: "metadata binding batch",
                    reason: "metadata binding batch contains a duplicate logical ID",
                });
            }
            validate_object_key(&binding.object.object_key)?;
            let key = (commit.workspace_id.clone(), binding.logical_id.clone());
            if let Some(existing) = staged.metadata_bindings.get(&key).cloned() {
                if existing.record_kind != binding.record_kind
                    || existing.sidecar != binding.sidecar
                {
                    return Err(ControlPlaneError::Conflict {
                        resource: "metadata binding",
                        reason: "logical ID is already bound to a different sidecar",
                    });
                }
                validate_metadata_dependencies(&staged, &commit.workspace_id, &binding.sidecar)?;
                if binding.object.object_key != existing.object.object_key {
                    validate_committed_pointer(
                        &staged,
                        &commit.workspace_id,
                        &binding.object,
                        ObjectKind::SnapshotMetadataPage,
                    )?;
                    staged.object_keys.insert((
                        commit.workspace_id.clone(),
                        binding.object.object_key.clone(),
                    ));
                    staged.committed_object_keys.insert((
                        commit.workspace_id.clone(),
                        binding.object.object_key.clone(),
                    ));
                    staged.object_retention_states.insert(
                        (
                            commit.workspace_id.clone(),
                            binding.object.object_key.clone(),
                        ),
                        RetentionState::OrphanCandidate,
                    );
                    staged
                        .object_pointers
                        .entry(commit.workspace_id.clone())
                        .or_default()
                        .push(binding.object.clone());
                }
                staged.object_retention_states.insert(
                    (
                        commit.workspace_id.clone(),
                        existing.object.object_key.clone(),
                    ),
                    RetentionState::Current,
                );
                for object_key in &binding.sidecar.direct_object_keys {
                    staged.object_retention_states.insert(
                        (commit.workspace_id.clone(), object_key.clone()),
                        RetentionState::Current,
                    );
                }
                let mut winner = existing;
                winner.outcome = Some(MetadataBindingOutcome::ExistingWinner);
                records.push(winner);
                continue;
            }
            if binding.object.kind != ObjectKind::SnapshotMetadataPage {
                return Err(ControlPlaneError::InvalidObjectKey {
                    reason: "metadata bindings must point at snapshot metadata pages",
                });
            }
            validate_metadata_dependencies(&staged, &commit.workspace_id, &binding.sidecar)?;
            for object_key in &binding.sidecar.direct_object_keys {
                staged.object_retention_states.insert(
                    (commit.workspace_id.clone(), object_key.clone()),
                    RetentionState::Current,
                );
            }
            validate_committed_pointer(
                &staged,
                &commit.workspace_id,
                &binding.object,
                ObjectKind::SnapshotMetadataPage,
            )?;
            staged.object_keys.insert((
                commit.workspace_id.clone(),
                binding.object.object_key.clone(),
            ));
            staged.committed_object_keys.insert((
                commit.workspace_id.clone(),
                binding.object.object_key.clone(),
            ));
            staged.object_retention_states.insert(
                (
                    commit.workspace_id.clone(),
                    binding.object.object_key.clone(),
                ),
                RetentionState::Current,
            );
            staged
                .object_pointers
                .entry(commit.workspace_id.clone())
                .or_default()
                .push(binding.object.clone());
            let record = MetadataBindingRecord {
                logical_id: binding.logical_id,
                record_kind: binding.record_kind,
                object: binding.object,
                sidecar: binding.sidecar,
                outcome: Some(MetadataBindingOutcome::BoundNew),
            };
            staged.metadata_bindings.insert(key, record.clone());
            records.push(record);
        }
        *state = staged;
        Ok(MetadataBindingBatch {
            workspace_id: commit.workspace_id,
            bindings: records,
        })
    }

    fn resolve_metadata_bindings(
        &self,
        workspace_id: &WorkspaceId,
        logical_ids: &[String],
    ) -> ControlPlaneResult<MetadataBindingBatch> {
        self.ensure_workspace(workspace_id)?;
        if logical_ids.is_empty() || logical_ids.len() > 16 {
            return Err(ControlPlaneError::Conflict {
                resource: "metadata binding resolution batch",
                reason: "metadata binding resolution batch exceeds the bounded contract",
            });
        }
        let mut requested = BTreeSet::new();
        for logical_id in logical_ids {
            validate_logical_id(logical_id)?;
            if !requested.insert(logical_id) {
                return Err(ControlPlaneError::Conflict {
                    resource: "metadata binding resolution batch",
                    reason: "metadata binding resolution batch contains a duplicate logical ID",
                });
            }
        }
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let local_device_id = self.local_device_id.as_deref().map(DeviceId::new);
        Self::ensure_trusted_device_if_configured(&state, workspace_id, local_device_id.as_ref())?;
        state
            .metadata_binding_resolution_requests
            .push(logical_ids.to_vec());
        let bindings = logical_ids
            .iter()
            .filter_map(|logical_id| {
                state
                    .metadata_bindings
                    .get(&(workspace_id.clone(), logical_id.clone()))
                    .cloned()
            })
            .map(|mut record| {
                record.outcome = None;
                record
            })
            .collect();
        Ok(MetadataBindingBatch {
            workspace_id: workspace_id.clone(),
            bindings,
        })
    }

    fn commit_snapshot_root(
        &self,
        commit: SnapshotRootCommit,
    ) -> ControlPlaneResult<SnapshotRootRecord> {
        self.ensure_workspace(&commit.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &commit.workspace_id,
            Some(&commit.committed_by_device_id),
        )?;
        validate_logical_id_kind(&commit.namespace_root_id, MetadataRecordKind::NamespacePage)?;
        if commit.extra_root_logical_ids.len() > 16 {
            return Err(ControlPlaneError::Conflict {
                resource: "snapshot root",
                reason: "snapshot root exceeds the bounded extra-root contract",
            });
        }
        for logical_id in &commit.extra_root_logical_ids {
            validate_logical_id(logical_id)?;
        }
        for logical_id in
            std::iter::once(&commit.namespace_root_id).chain(commit.extra_root_logical_ids.iter())
        {
            if !state
                .metadata_bindings
                .contains_key(&(commit.workspace_id.clone(), logical_id.clone()))
            {
                return Err(ControlPlaneError::Conflict {
                    resource: "snapshot root",
                    reason: "snapshot root references an incomplete metadata graph",
                });
            }
        }
        let key = (commit.workspace_id.clone(), commit.snapshot_id.clone());
        if let Some(existing) = state.snapshot_roots.get(&key) {
            if existing.manifest_id == commit.manifest_id
                && existing.manifest_object == commit.manifest_object
                && existing.namespace_root_id == commit.namespace_root_id
                && existing.extra_root_logical_ids == commit.extra_root_logical_ids
            {
                return Ok(existing.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "snapshot root",
                reason: "snapshot is already committed with a different root",
            });
        }
        validate_committed_pointer(
            &state,
            &commit.workspace_id,
            &commit.manifest_object,
            ObjectKind::SnapshotManifest,
        )?;
        let record = SnapshotRootRecord {
            workspace_id: commit.workspace_id.clone(),
            snapshot_id: commit.snapshot_id,
            manifest_id: commit.manifest_id,
            manifest_object: commit.manifest_object,
            namespace_root_id: commit.namespace_root_id,
            extra_root_logical_ids: commit.extra_root_logical_ids,
            complete: true,
            committed_by_device_id: commit.committed_by_device_id,
            committed_at: self.clock.now(),
        };
        let event = self.build_event(
            &record.workspace_id,
            CompactEventKind::SnapshotRootCommitted,
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
        state.snapshot_roots.insert(key, record.clone());
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(event);
        Ok(record)
    }

    fn get_snapshot_root(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> ControlPlaneResult<Option<SnapshotRootRecord>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        let local_device_id = self.local_device_id.as_deref().map(DeviceId::new);
        Self::ensure_trusted_device_if_configured(&state, workspace_id, local_device_id.as_ref())?;
        Ok(state
            .snapshot_roots
            .get(&(workspace_id.clone(), snapshot_id.clone()))
            .cloned())
    }
}

fn validate_metadata_dependencies(
    state: &FakeControlPlaneState,
    workspace_id: &WorkspaceId,
    sidecar: &MetadataSidecar,
) -> ControlPlaneResult<()> {
    for child in &sidecar.child_logical_ids {
        if !state
            .metadata_bindings
            .contains_key(&(workspace_id.clone(), child.clone()))
        {
            return Err(ControlPlaneError::Conflict {
                resource: "metadata binding",
                reason: "metadata sidecar references a missing child binding",
            });
        }
    }
    for object_key in &sidecar.direct_object_keys {
        if !state
            .committed_object_keys
            .contains(&(workspace_id.clone(), object_key.clone()))
        {
            return Err(ControlPlaneError::Conflict {
                resource: "metadata binding",
                reason: "metadata sidecar references an unavailable object",
            });
        }
    }
    Ok(())
}

fn validate_metadata_binding_input(binding: &MetadataBindingInput) -> ControlPlaneResult<()> {
    validate_logical_id_kind(&binding.logical_id, binding.record_kind)?;
    if binding.object.kind != ObjectKind::SnapshotMetadataPage
        || binding.object.content_id.as_str() != binding.logical_id
    {
        return Err(ControlPlaneError::Conflict {
            resource: "metadata binding",
            reason: "metadata page identity does not match its logical ID",
        });
    }
    validate_blake3_hash(&binding.object.hash, "metadata page")?;
    if binding.object.key_epoch == 0 {
        return Err(ControlPlaneError::Conflict {
            resource: "metadata binding",
            reason: "metadata page key epoch must be positive",
        });
    }
    validate_blake3_hash(&binding.sidecar.digest, "metadata sidecar")?;
    if binding.sidecar.child_logical_ids.len() + binding.sidecar.direct_object_keys.len() > 256 {
        return Err(ControlPlaneError::Conflict {
            resource: "metadata binding",
            reason: "metadata sidecar exceeds the bounded edge contract",
        });
    }
    validate_sorted_unique(
        &binding.sidecar.child_logical_ids,
        "metadata child logical IDs",
    )?;
    validate_sorted_unique(
        &binding.sidecar.direct_object_keys,
        "metadata direct object keys",
    )?;
    for logical_id in &binding.sidecar.child_logical_ids {
        validate_logical_id(logical_id)?;
    }
    for object_key in &binding.sidecar.direct_object_keys {
        validate_object_key(object_key)?;
    }
    Ok(())
}

fn validate_logical_id_kind(
    logical_id: &str,
    record_kind: MetadataRecordKind,
) -> ControlPlaneResult<()> {
    validate_logical_id(logical_id)?;
    let required_prefix = match record_kind {
        MetadataRecordKind::NamespacePage => "nsp_",
        MetadataRecordKind::ContentLayout => "ctl_",
        MetadataRecordKind::SegmentPage => "sgp_",
    };
    if !logical_id.starts_with(required_prefix) {
        return Err(ControlPlaneError::Conflict {
            resource: "metadata binding",
            reason: "metadata logical ID does not match its record kind",
        });
    }
    Ok(())
}

fn validate_logical_id(logical_id: &str) -> ControlPlaneResult<()> {
    let Some((prefix, digest)) = logical_id.split_once('_') else {
        return Err(invalid_logical_id());
    };
    if !matches!(prefix, "nsp" | "ctl" | "sgp") || !is_lower_hex(digest, 64) {
        return Err(invalid_logical_id());
    }
    Ok(())
}

fn invalid_logical_id() -> ControlPlaneError {
    ControlPlaneError::Conflict {
        resource: "metadata binding",
        reason: "metadata logical ID is invalid",
    }
}

fn validate_blake3_hash(value: &str, resource: &'static str) -> ControlPlaneResult<()> {
    if value
        .strip_prefix("b3_")
        .is_none_or(|digest| !is_lower_hex(digest, 64))
    {
        return Err(ControlPlaneError::Conflict {
            resource,
            reason: "metadata digest is not a canonical BLAKE3 hash",
        });
    }
    Ok(())
}

fn validate_sorted_unique(values: &[String], resource: &'static str) -> ControlPlaneResult<()> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ControlPlaneError::Conflict {
            resource,
            reason: "metadata sidecar edges must be sorted and unique",
        });
    }
    Ok(())
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
