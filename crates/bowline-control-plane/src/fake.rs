use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_storage::{
    ObjectKey as StorageObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use sha2::{Digest, Sha256};

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput, ByteRange, CompactEvent,
    CompactEventKind, CompareAndSwapError, ConflictMetadataPublish, ConflictMetadataRecord,
    ConflictResolutionMark, ControlPlaneClient, ControlPlaneError, ControlPlaneResult,
    ControlPlaneTimestamp, DeleteIntent, DeleteIntentRequest, DeterministicClock,
    DeterministicIdGenerator, DeviceApproval, DeviceApprovalInput, DeviceApprovalRequestList,
    DeviceDenial, DeviceDenialInput, DeviceRequest, DeviceRequestInput, DeviceRequestState,
    DeviceRevocationInput, DownloadIntent, DownloadIntentRequest, FirstAuthorizedDeviceInput,
    GrantAcceptanceInput, Lease, LeaseCreate, LeaseExecutionState, LeaseOutputState, LeaseUpdate,
    LeaseWriteTargetMode, ObjectKind, ObjectManifestCommit, ObjectManifestRecord,
    ObjectMetadataCommit, ObjectPointer, ObjectRetentionStateUpdate,
    RecoveryDeviceAuthorizationInput, RecoveryEnvelopeInput, RecoveryEnvelopeRecord,
    RecoveryEnvelopeState, RevokedDeviceRecord, SignedUrlIntent, StaleWorkViewOverlayHead,
    StaleWorkspaceRef, UploadIntent, UploadIntentRequest, UploadVerificationIntentRequest,
    WorkViewCreate, WorkViewLifecycleState, WorkViewLifecycleUpdate, WorkViewOverlayCommit,
    WorkViewRecord, WorkViewUpdateError, WorkspaceRef, validate_object_key,
};

#[derive(Debug, Clone)]
pub struct FakeControlPlaneClient {
    clock: DeterministicClock,
    ids: DeterministicIdGenerator,
    local_device_id: Option<String>,
    state: Arc<Mutex<FakeControlPlaneState>>,
}

impl FakeControlPlaneClient {
    pub fn new(clock: DeterministicClock, ids: DeterministicIdGenerator) -> Self {
        Self {
            clock,
            ids,
            local_device_id: None,
            state: Arc::new(Mutex::new(FakeControlPlaneState::default())),
        }
    }

    pub fn with_local_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.local_device_id = Some(device_id.into());
        self
    }

    pub fn create_workspace(&self, workspace_id: impl Into<String>) -> WorkspaceRef {
        self.create_workspace_ref(&workspace_id.into())
            .expect("fake workspace creation is infallible")
    }

    pub fn append_event(
        &self,
        workspace_id: &str,
        kind: CompactEventKind,
        subject: &str,
    ) -> CompactEvent {
        let event = self.build_event(workspace_id, kind, subject);
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(event.clone());
        event
    }

    pub fn request_device(
        &self,
        workspace_id: &str,
        device_id: &str,
        device_name: &str,
    ) -> DeviceRequest {
        let mut input = DeviceRequestInput::new(
            workspace_id,
            device_id,
            device_name,
            format!("age1{device_id}"),
            format!("fp_{device_id}"),
            "phase4-smoke",
        );
        input.device_authorization_proof_verifier = format!("dapv_harness_{device_id}");
        self.create_device_request(input)
            .expect("fake device request is infallible")
    }

    pub fn grant_device(
        &self,
        request_id: &str,
        approved_by_device_id: &str,
    ) -> Option<DeviceApproval> {
        self.approve_device_request_for_harness(DeviceApprovalInput {
            request_id: request_id.to_string(),
            approved_by_device_id: approved_by_device_id.to_string(),
            approved_by_device_proof: String::new(),
            encrypted_grant_ciphertext: "bowline-harness-grant".to_string(),
            grant_acceptance_proof_verifier: String::new(),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .ok()
    }

    pub fn create_lease_for_harness(&self, workspace_id: &str, device_id: &str) -> Lease {
        let created_at = self.clock.now();
        self.create_lease(LeaseCreate {
            lease_id: self.ids.next_id("lease"),
            workspace_id: workspace_id.to_string(),
            project_id: "project-harness".to_string(),
            device_id: device_id.to_string(),
            write_target_mode: LeaseWriteTargetMode::Direct,
            work_view_id: None,
            base_snapshot_id: "empty".to_string(),
            execution_state: LeaseExecutionState::Active,
            output_state: LeaseOutputState::Empty,
            status_code: "active".to_string(),
            output_object: None,
            audit_object: None,
            expires_at: crate::ControlPlaneTimestamp {
                tick: created_at.tick + 3_600,
            },
        })
        .expect("fake harness lease creation is infallible without trust")
    }

    pub fn put_object_pointer(&self, workspace_id: &str, pointer: ObjectPointer) -> CompactEvent {
        let event = self.build_event(
            workspace_id,
            CompactEventKind::ObjectPointerAdded,
            &pointer.object_key,
        );
        let mut state = self.state.lock().expect("fake control plane poisoned");
        state
            .object_keys
            .insert((workspace_id.to_string(), pointer.object_key.clone()));
        state
            .committed_object_keys
            .insert((workspace_id.to_string(), pointer.object_key.clone()));
        state.object_retention_states.insert(
            (workspace_id.to_string(), pointer.object_key.clone()),
            RetentionState::Current,
        );
        state
            .object_pointers
            .entry(workspace_id.to_string())
            .or_default()
            .push(pointer);
        state
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(event.clone());
        event
    }

    pub fn object_pointers(&self, workspace_id: &str) -> Vec<ObjectPointer> {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .object_pointers
            .get(workspace_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn make_next_overlay_commit_stale_with_same_object_for_harness(
        &self,
        workspace_id: &str,
        work_view_id: &str,
    ) {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .same_object_stale_overlay_commits
            .insert((workspace_id.to_string(), work_view_id.to_string()));
    }

    pub fn events(&self, workspace_id: &str) -> Vec<CompactEvent> {
        self.list_events(workspace_id).unwrap_or_default()
    }

    pub fn approve_device_request_for_harness(
        &self,
        input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let request = state
            .device_requests
            .get(&input.request_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::DeviceRequestMissing {
                request_id: input.request_id.clone(),
            })?;

        let grant_acceptance_proof_verifier = input.grant_acceptance_proof_verifier.clone();
        let approval = self.build_device_approval(&request, input, true);

        state
            .grants
            .insert(approval.request_id.clone(), approval.clone());
        state
            .grant_acceptance_proof_verifiers
            .insert(approval.request_id.clone(), grant_acceptance_proof_verifier);
        state
            .events
            .entry(approval.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &approval.workspace_id,
                CompactEventKind::DeviceHarnessApproved,
                &approval.grant_id,
            ));

        Ok(approval)
    }

    fn build_event(
        &self,
        workspace_id: &str,
        kind: CompactEventKind,
        subject: &str,
    ) -> CompactEvent {
        CompactEvent {
            event_id: self.ids.next_id("event"),
            workspace_id: workspace_id.to_string(),
            at: self.clock.now(),
            kind,
            subject: subject.to_string(),
        }
    }

    fn ensure_workspace(&self, workspace_id: &str) -> ControlPlaneResult<()> {
        if self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .workspace_refs
            .contains_key(workspace_id)
        {
            Ok(())
        } else {
            Err(ControlPlaneError::WorkspaceMissing {
                workspace_id: workspace_id.to_string(),
            })
        }
    }

    fn ensure_local_device(&self, device_id: &str) -> ControlPlaneResult<()> {
        match self.local_device_id.as_deref() {
            Some(local_device_id) if local_device_id != device_id => {
                Err(ControlPlaneError::Limited {
                    capability: "fake-device-proof",
                    reason: "fake operation must be performed by the configured local device",
                })
            }
            _ => Ok(()),
        }
    }

    fn fake_signed_url(&self, action: &str, object_key: &str, range: Option<&ByteRange>) -> String {
        match range {
            Some(range) => format!(
                "fake://r2/{object_key}?action={action}&offset={}&length={}",
                range.offset, range.length
            ),
            None => format!("fake://r2/{object_key}?action={action}"),
        }
    }

    fn build_device_approval(
        &self,
        request: &DeviceRequest,
        input: DeviceApprovalInput,
        harness_only: bool,
    ) -> DeviceApproval {
        let granted_at = self.clock.now();
        DeviceApproval {
            grant_id: self.ids.next_id("device-grant"),
            request_id: request.request_id.clone(),
            workspace_id: request.workspace_id.clone(),
            device_id: request.device_id.clone(),
            device_name: request.device_name.clone(),
            platform: request.platform.clone(),
            device_fingerprint: request.device_fingerprint.clone(),
            approved_by_device_id: input.approved_by_device_id,
            encrypted_grant_ciphertext: input.encrypted_grant_ciphertext,
            key_epoch: input.key_epoch,
            granted_at,
            expires_at: crate::ControlPlaneTimestamp {
                tick: granted_at.tick + input.expires_in_ticks,
            },
            accepted_at: None,
            harness_only,
        }
    }

    fn authorized_device(
        &self,
        request: &DeviceRequest,
        approved_by_device_id: Option<String>,
        authorized_at: crate::ControlPlaneTimestamp,
    ) -> AuthorizedDeviceRecord {
        AuthorizedDeviceRecord {
            workspace_id: request.workspace_id.clone(),
            device_id: request.device_id.clone(),
            device_name: request.device_name.clone(),
            platform: request.platform.clone(),
            device_fingerprint: request.device_fingerprint.clone(),
            authorized_at,
            authorized_by_device_id: approved_by_device_id,
            revoked_at: None,
        }
    }

    fn ensure_authorized_approver(
        state: &FakeControlPlaneState,
        workspace_id: &str,
        device_id: &str,
        proof: &str,
        action: &str,
        subject: &str,
    ) -> ControlPlaneResult<()> {
        match state
            .authorized_devices
            .get(&(workspace_id.to_string(), device_id.to_string()))
        {
            Some(device) if device.revoked_at.is_none() => {
                let Some(verifier) = state
                    .device_authorization_proof_verifiers
                    .get(&(workspace_id.to_string(), device_id.to_string()))
                else {
                    return Err(ControlPlaneError::Limited {
                        capability: "device-trust",
                        reason: "trusted device proof is missing",
                    });
                };
                if device_authorization_proof_valid(
                    verifier,
                    proof,
                    workspace_id,
                    device_id,
                    action,
                    subject,
                ) {
                    Ok(())
                } else {
                    Err(ControlPlaneError::Limited {
                        capability: "device-trust",
                        reason: "trusted device proof does not match",
                    })
                }
            }
            _ => Err(ControlPlaneError::Limited {
                capability: "device-trust",
                reason: "approver is not a trusted non-revoked device",
            }),
        }
    }

    fn ensure_not_revoked(
        state: &FakeControlPlaneState,
        workspace_id: &str,
        device_id: &str,
    ) -> ControlPlaneResult<()> {
        if state
            .revoked_devices
            .contains_key(&(workspace_id.to_string(), device_id.to_string()))
        {
            return Err(ControlPlaneError::Limited {
                capability: "device-trust",
                reason: "revoked devices cannot receive new grants",
            });
        }
        Ok(())
    }

    fn ensure_trusted_device_if_configured(
        state: &FakeControlPlaneState,
        workspace_id: &str,
        device_id: Option<&str>,
    ) -> ControlPlaneResult<()> {
        let workspace_has_trust = state
            .authorized_devices
            .keys()
            .any(|(authorized_workspace, _)| authorized_workspace == workspace_id)
            || state
                .revoked_devices
                .keys()
                .any(|(revoked_workspace, _)| revoked_workspace == workspace_id);
        if !workspace_has_trust {
            return Ok(());
        }

        let Some(device_id) = device_id else {
            return Err(ControlPlaneError::Limited {
                capability: "device-trust",
                reason: "trusted-device workspace access requires a local device",
            });
        };
        match state
            .authorized_devices
            .get(&(workspace_id.to_string(), device_id.to_string()))
        {
            Some(device) if device.revoked_at.is_none() => Ok(()),
            _ => Err(ControlPlaneError::Limited {
                capability: "device-trust",
                reason: "device is not trusted for this workspace",
            }),
        }
    }

    fn ensure_revocation_keeps_trust_path(
        state: &FakeControlPlaneState,
        workspace_id: &str,
        device_id: &str,
    ) -> ControlPlaneResult<()> {
        let another_trusted_device =
            state
                .authorized_devices
                .keys()
                .any(|(authorized_workspace, authorized_device)| {
                    authorized_workspace == workspace_id && authorized_device != device_id
                });
        if another_trusted_device {
            return Ok(());
        }
        let active_recovery_key = state.recovery_envelopes.values().any(|envelope| {
            envelope.workspace_id == workspace_id && envelope.state == RecoveryEnvelopeState::Active
        });
        if active_recovery_key {
            return Ok(());
        }
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "cannot revoke the last trusted device without another trusted device or active Recovery Key",
        })
    }
}

impl Default for FakeControlPlaneClient {
    fn default() -> Self {
        Self::new(
            DeterministicClock::default(),
            DeterministicIdGenerator::default(),
        )
    }
}

impl ControlPlaneClient for FakeControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef> {
        let mut state = self.state.lock().expect("fake control plane poisoned");

        if let Some(existing_ref) = state.workspace_refs.get(workspace_id) {
            return Ok(existing_ref.clone());
        }

        let workspace_ref = WorkspaceRef {
            workspace_id: workspace_id.to_string(),
            version: 0,
            snapshot_id: "empty".to_string(),
            updated_at: self.clock.now(),
            updated_by_device_id: None,
        };

        state
            .workspace_refs
            .insert(workspace_id.to_string(), workspace_ref.clone());
        state
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(self.build_event(
                workspace_id,
                CompactEventKind::WorkspaceCreated,
                &workspace_ref.snapshot_id,
            ));

        Ok(workspace_ref)
    }

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>> {
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .workspace_refs
            .get(workspace_id)
            .cloned())
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let current = state
            .workspace_refs
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| CompareAndSwapError::WorkspaceMissing {
                workspace_id: workspace_id.to_string(),
            })?;

        if current.version != expected_version {
            return Err(CompareAndSwapError::StaleRef(StaleWorkspaceRef {
                expected_version,
                current,
            }));
        }

        let next_ref = WorkspaceRef {
            workspace_id: workspace_id.to_string(),
            version: current.version + 1,
            snapshot_id: new_snapshot_id.to_string(),
            updated_at: self.clock.now(),
            updated_by_device_id: Some(writer_device_id.to_string()),
        };

        state
            .workspace_refs
            .insert(workspace_id.to_string(), next_ref.clone());
        state
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(self.build_event(
                workspace_id,
                CompactEventKind::WorkspaceRefAdvanced,
                new_snapshot_id,
            ));

        Ok(next_ref)
    }

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>> {
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .events
            .get(workspace_id)
            .cloned()
            .unwrap_or_default())
    }

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        self.ensure_local_device(&input.detected_by_device_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let key = (input.workspace_id.clone(), input.conflict_id.clone());
        if let Some(existing) = state.conflicts.get(&key)
            && (conflict_metadata_same_occurrence(
                existing,
                &input.base_snapshot_id,
                &input.remote_snapshot_id,
            ) || existing.state == "unresolved")
        {
            return Ok(existing.clone());
        }
        let detected_at = self.clock.now();
        let record = ConflictMetadataRecord {
            workspace_id: input.workspace_id.clone(),
            conflict_id: input.conflict_id.clone(),
            conflict_kind: input.conflict_kind,
            paths: input.paths,
            contains_secrets: input.contains_secrets,
            state: "unresolved".to_string(),
            base_snapshot_id: input.base_snapshot_id,
            remote_snapshot_id: input.remote_snapshot_id,
            detected_by_device_id: input.detected_by_device_id,
            bundle_object: input.bundle_object,
            detected_at,
            resolved_by_device_id: None,
            resolved_at: None,
        };
        state.conflicts.insert(key, record.clone());
        state
            .events
            .entry(input.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &input.workspace_id,
                CompactEventKind::ConflictDetected,
                &input.conflict_id,
            ));
        Ok(record)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.ensure_workspace(workspace_id)?;
        self.ensure_local_device(requested_by_device_id)?;
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .conflicts
            .values()
            .filter(|record| record.workspace_id == workspace_id && record.state == "unresolved")
            .cloned()
            .collect())
    }

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        self.ensure_local_device(&input.resolved_by_device_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let key = (input.workspace_id.clone(), input.conflict_id.clone());
        let record = state
            .conflicts
            .get_mut(&key)
            .ok_or(ControlPlaneError::Conflict {
                resource: "conflict metadata",
                reason: "conflict does not exist",
            })?;
        if record.state == input.resolution.as_str() {
            return Ok(record.clone());
        }
        if record.state != "unresolved" {
            return Err(ControlPlaneError::Conflict {
                resource: "conflict metadata",
                reason: "conflict metadata is already terminal",
            });
        }
        let resolved_at = self.clock.now();
        record.state = input.resolution.as_str().to_string();
        record.resolved_by_device_id = Some(input.resolved_by_device_id.clone());
        record.resolved_at = Some(resolved_at);
        let record = record.clone();
        state
            .events
            .entry(input.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &input.workspace_id,
                CompactEventKind::ConflictResolved,
                &input.conflict_id,
            ));
        Ok(record)
    }

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

    fn create_work_view(&self, input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.created_by_device_id),
        )?;
        let Some(workspace_ref) = state.workspace_refs.get(&input.workspace_id) else {
            return Err(ControlPlaneError::WorkspaceMissing {
                workspace_id: input.workspace_id,
            });
        };
        if workspace_ref.snapshot_id != input.base_snapshot_id
            && !state
                .manifests_by_snapshot
                .contains_key(&(input.workspace_id.clone(), input.base_snapshot_id.clone()))
        {
            return Err(ControlPlaneError::Conflict {
                resource: "work view",
                reason: "base snapshot has not been committed",
            });
        }
        if !input.visible_path.starts_with(".work/") {
            return Err(ControlPlaneError::Conflict {
                resource: "work view",
                reason: "visible path must be a relative .work namespace path",
            });
        }
        let key = (input.workspace_id.clone(), input.work_view_id.clone());
        if let Some(existing) = state.work_views.get(&key) {
            return Ok(existing.clone());
        }
        if state.work_views.values().any(|view| {
            view.workspace_id == input.workspace_id
                && view.project_id == input.project_id
                && view.name.eq_ignore_ascii_case(&input.name)
        }) {
            return Err(ControlPlaneError::Conflict {
                resource: "work view",
                reason: "work view name already exists for this project",
            });
        }

        let now = self.clock.now();
        let record = WorkViewRecord {
            workspace_id: input.workspace_id,
            work_view_id: input.work_view_id,
            project_id: input.project_id,
            name: input.name,
            visible_path: input.visible_path,
            base_snapshot_id: input.base_snapshot_id,
            base_workspace_version: input.base_workspace_version,
            overlay_head: None,
            overlay_version: 0,
            lifecycle: WorkViewLifecycleState::Active,
            created_by_device_id: input.created_by_device_id.clone(),
            updated_by_device_id: input.created_by_device_id,
            created_at: now,
            updated_at: now,
        };
        state.work_views.insert(key, record.clone());
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                CompactEventKind::WorkCreated,
                &record.work_view_id,
            ));
        Ok(record)
    }

    fn list_work_views(
        &self,
        workspace_id: &str,
        include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            workspace_id,
            self.local_device_id.as_deref(),
        )?;
        let mut records = state
            .work_views
            .values()
            .filter(|view| {
                view.workspace_id == workspace_id
                    && (include_all
                        || matches!(
                            view.lifecycle,
                            WorkViewLifecycleState::Active | WorkViewLifecycleState::ReviewReady
                        ))
            })
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.visible_path.cmp(&right.visible_path));
        Ok(records)
    }

    fn update_work_view_lifecycle(
        &self,
        input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.updated_by_device_id),
        )?;
        let key = (input.workspace_id.clone(), input.work_view_id.clone());
        let record =
            state
                .work_views
                .get_mut(&key)
                .ok_or_else(|| ControlPlaneError::WorkViewMissing {
                    work_view_id: input.work_view_id.clone(),
                })?;
        record.lifecycle = input.lifecycle;
        record.updated_by_device_id = input.updated_by_device_id;
        record.updated_at = self.clock.now();
        let record = record.clone();
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                work_event_for_lifecycle(record.lifecycle),
                &record.work_view_id,
            ));
        Ok(record)
    }

    fn restore_work_view(
        &self,
        workspace_id: &str,
        work_view_id: &str,
        restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.ensure_workspace(workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            workspace_id,
            Some(restored_by_device_id),
        )?;
        let key = (workspace_id.to_string(), work_view_id.to_string());
        let record =
            state
                .work_views
                .get_mut(&key)
                .ok_or_else(|| ControlPlaneError::WorkViewMissing {
                    work_view_id: work_view_id.to_string(),
                })?;
        record.lifecycle = WorkViewLifecycleState::Active;
        record.updated_by_device_id = restored_by_device_id.to_string();
        record.updated_at = self.clock.now();
        let record = record.clone();
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                CompactEventKind::WorkRestored,
                &record.work_view_id,
            ));
        Ok(record)
    }

    fn commit_work_view_overlay(
        &self,
        input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        self.ensure_workspace(&input.workspace_id)?;
        validate_object_key(&input.overlay_object.object_key)?;
        if input.overlay_object.kind != ObjectKind::AgentOverlay {
            return Err(ControlPlaneError::InvalidObjectKey {
                reason: "work view overlays must point at overlay objects",
            }
            .into());
        }

        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.committed_by_device_id),
        )?;
        let key = (input.workspace_id.clone(), input.work_view_id.clone());
        let current = state.work_views.get(&key).cloned().ok_or_else(|| {
            WorkViewUpdateError::WorkViewMissing {
                work_view_id: input.work_view_id.clone(),
            }
        })?;
        if current.overlay_version != input.expected_overlay_version {
            return Err(WorkViewUpdateError::StaleOverlayHead(Box::new(
                StaleWorkViewOverlayHead {
                    expected_overlay_version: input.expected_overlay_version,
                    current,
                },
            )));
        }
        validate_committed_pointer(
            &state,
            &input.workspace_id,
            &input.overlay_object,
            ObjectKind::AgentOverlay,
        )?;

        if state
            .same_object_stale_overlay_commits
            .remove(&(input.workspace_id.clone(), input.work_view_id.clone()))
        {
            let pointers = state
                .object_pointers
                .entry(input.workspace_id.clone())
                .or_default();
            if !pointers
                .iter()
                .any(|pointer| pointer.object_key == input.overlay_object.object_key)
            {
                pointers.push(input.overlay_object.clone());
            }
            state.object_keys.insert((
                input.workspace_id.clone(),
                input.overlay_object.object_key.clone(),
            ));
            state.committed_object_keys.insert((
                input.workspace_id.clone(),
                input.overlay_object.object_key.clone(),
            ));
            state.object_retention_states.insert(
                (
                    input.workspace_id.clone(),
                    input.overlay_object.object_key.clone(),
                ),
                RetentionState::Current,
            );
            let record = state
                .work_views
                .get_mut(&key)
                .expect("work view exists after lookup");
            record.overlay_head = Some(input.overlay_object);
            record.overlay_version += 1;
            record.updated_by_device_id = input.committed_by_device_id;
            record.updated_at = self.clock.now();
            return Err(WorkViewUpdateError::StaleOverlayHead(Box::new(
                StaleWorkViewOverlayHead {
                    expected_overlay_version: input.expected_overlay_version,
                    current: record.clone(),
                },
            )));
        }

        state.object_keys.insert((
            input.workspace_id.clone(),
            input.overlay_object.object_key.clone(),
        ));
        state.committed_object_keys.insert((
            input.workspace_id.clone(),
            input.overlay_object.object_key.clone(),
        ));
        state.object_retention_states.insert(
            (
                input.workspace_id.clone(),
                input.overlay_object.object_key.clone(),
            ),
            RetentionState::Current,
        );
        let pointers = state
            .object_pointers
            .entry(input.workspace_id.clone())
            .or_default();
        if !pointers
            .iter()
            .any(|pointer| pointer.object_key == input.overlay_object.object_key)
        {
            pointers.push(input.overlay_object.clone());
        }

        let record = state
            .work_views
            .get_mut(&key)
            .expect("work view exists after lookup");
        record.overlay_head = Some(input.overlay_object);
        record.overlay_version += 1;
        record.updated_by_device_id = input.committed_by_device_id;
        record.updated_at = self.clock.now();
        let record = record.clone();
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                CompactEventKind::WorkUpdated,
                &record.work_view_id,
            ));
        Ok(record)
    }

    fn create_lease(&self, input: LeaseCreate) -> ControlPlaneResult<Lease> {
        self.ensure_workspace(&input.workspace_id)?;
        validate_lease_create(&input)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.device_id),
        )?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.output_object.as_ref())?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.audit_object.as_ref())?;

        if let Some(existing) = state.leases.get(&input.lease_id) {
            if lease_create_matches(existing, &input) {
                return Ok(existing.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "agent lease",
                reason: "lease ID already exists with different metadata",
            });
        }

        let created_at = self.clock.now();
        let lease = Lease {
            lease_id: input.lease_id,
            workspace_id: input.workspace_id,
            project_id: input.project_id,
            device_id: input.device_id,
            write_target_mode: input.write_target_mode,
            work_view_id: input.work_view_id,
            base_snapshot_id: input.base_snapshot_id,
            version: 0,
            execution_state: input.execution_state,
            output_state: input.output_state,
            status_code: input.status_code,
            output_object: input.output_object,
            audit_object: input.audit_object,
            created_at,
            updated_at: created_at,
            expires_at: input.expires_at,
        };
        state.leases.insert(lease.lease_id.clone(), lease.clone());
        state
            .events
            .entry(lease.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &lease.workspace_id,
                CompactEventKind::LeaseCreated,
                &lease.lease_id,
            ));
        Ok(lease)
    }

    fn update_lease(&self, input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        self.ensure_workspace(&input.workspace_id)?;
        validate_lease_update(&input)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.updated_by_device_id),
        )?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.output_object.as_ref())?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.audit_object.as_ref())?;

        let key = input.lease_id.clone();
        let existing =
            state
                .leases
                .get(&key)
                .cloned()
                .ok_or_else(|| ControlPlaneError::LeaseMissing {
                    lease_id: input.lease_id.clone(),
                })?;
        if existing.workspace_id != input.workspace_id {
            return Err(ControlPlaneError::LeaseMissing {
                lease_id: input.lease_id,
            });
        }
        if existing.version != input.expected_version {
            return Err(ControlPlaneError::Conflict {
                resource: "agent lease",
                reason: "lease version is stale",
            });
        }

        let updated_at = self.clock.now();
        let lease = state
            .leases
            .get_mut(&key)
            .expect("lease exists after lookup");
        if let Some(execution_state) = input.execution_state {
            lease.execution_state = execution_state;
        }
        if let Some(output_state) = input.output_state {
            lease.output_state = output_state;
        }
        if let Some(status_code) = input.status_code {
            lease.status_code = status_code;
        }
        if let Some(output_object) = input.output_object {
            lease.output_object = Some(output_object);
        }
        if let Some(audit_object) = input.audit_object {
            lease.audit_object = Some(audit_object);
        }
        lease.version += 1;
        lease.updated_at = updated_at;
        let lease = lease.clone();
        state
            .events
            .entry(lease.workspace_id.clone())
            .or_default()
            .push(
                self.build_event(
                    &lease.workspace_id,
                    input
                        .event_kind
                        .unwrap_or_else(|| lease_event_for_update(&lease)),
                    &lease.lease_id,
                ),
            );
        Ok(lease)
    }

    fn list_leases(&self, workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            workspace_id,
            self.local_device_id.as_deref(),
        )?;
        let mut leases = state
            .leases
            .values()
            .filter(|lease| lease.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| left.lease_id.cmp(&right.lease_id));
        Ok(leases)
    }

    fn create_bootstrap_session(
        &self,
        input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        self.ensure_workspace(&input.workspace_id)?;
        let created_at = self.clock.now();
        Ok(BootstrapSession {
            session_id: self.ids.next_id("bootstrap-session"),
            workspace_id: input.workspace_id,
            token: self.ids.next_id("bootstrap-token"),
            expires_at: ControlPlaneTimestamp {
                tick: created_at.tick + input.expires_in_ticks,
            },
        })
    }

    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        self.ensure_workspace(&input.workspace_id)?;
        if input.device_public_key.is_empty()
            || input.device_fingerprint.is_empty()
            || input.device_authorization_proof_verifier.is_empty()
        {
            return Err(ControlPlaneError::Conflict {
                resource: "device request",
                reason: "device public key, fingerprint, and proof verifier are required",
            });
        }

        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_not_revoked(&state, &input.workspace_id, &input.device_id)?;
        if state
            .authorized_devices
            .contains_key(&(input.workspace_id.clone(), input.device_id.clone()))
        {
            return Err(ControlPlaneError::Conflict {
                resource: "device request",
                reason: "authorized devices cannot request trust again",
            });
        }
        let request_key = (input.workspace_id.clone(), input.device_id.clone());
        if let Some(existing) = state.device_request_by_device.get(&request_key)
            && let Some(request) = state.device_requests.get(existing)
        {
            if request.device_public_key == input.device_public_key
                && request.device_fingerprint == input.device_fingerprint
                && state.pending_device_proof_verifiers.get(existing)
                    == Some(&input.device_authorization_proof_verifier)
                && request.state == DeviceRequestState::Pending
            {
                return Ok(request.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "device request",
                reason: "pending device conflicts with existing metadata",
            });
        }

        let requested_at = self.clock.now();
        let request = DeviceRequest {
            request_id: self.ids.next_id("device-request"),
            workspace_id: input.workspace_id,
            device_id: input.device_id,
            device_name: input.device_name,
            platform: input.platform,
            device_public_key: input.device_public_key,
            device_fingerprint: input.device_fingerprint,
            matching_code: input.matching_code,
            account_id: input.account_id,
            host: input.host,
            root: input.root,
            requested_at,
            expires_at: crate::ControlPlaneTimestamp {
                tick: requested_at.tick + input.expires_in_ticks,
            },
            state: DeviceRequestState::Pending,
        };

        state
            .device_request_by_device
            .insert(request_key, request.request_id.clone());
        state
            .device_requests
            .insert(request.request_id.clone(), request.clone());
        state.pending_device_proof_verifiers.insert(
            request.request_id.clone(),
            input.device_authorization_proof_verifier,
        );
        state
            .events
            .entry(request.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &request.workspace_id,
                CompactEventKind::DeviceApprovalRequested,
                &request.request_id,
            ));

        Ok(request)
    }

    fn create_first_authorized_device(
        &self,
        input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        if input.device_authorization_proof_verifier.is_empty() {
            return Err(ControlPlaneError::Conflict {
                resource: "first authorized device",
                reason: "device proof verifier is required",
            });
        }
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let workspace_has_authorized_device = state
            .authorized_devices
            .values()
            .any(|device| device.workspace_id == input.workspace_id && device.revoked_at.is_none());
        let workspace_has_trust_history = state
            .revoked_devices
            .values()
            .any(|device| device.workspace_id == input.workspace_id)
            || state
                .recovery_envelopes
                .values()
                .any(|envelope| envelope.workspace_id == input.workspace_id);
        let key = (input.workspace_id.clone(), input.device_id.clone());
        if let Some(existing) = state.authorized_devices.get(&key) {
            return Ok(existing.clone());
        }
        if workspace_has_authorized_device || workspace_has_trust_history {
            return Err(ControlPlaneError::Conflict {
                resource: "first authorized device",
                reason: "workspace already has trust history",
            });
        }
        let authorized_at = self.clock.now();
        let device = AuthorizedDeviceRecord {
            workspace_id: input.workspace_id.clone(),
            device_id: input.device_id.clone(),
            device_name: input.device_name,
            platform: input.platform,
            device_fingerprint: input.device_fingerprint,
            authorized_at,
            authorized_by_device_id: None,
            revoked_at: None,
        };
        state.authorized_devices.insert(key.clone(), device.clone());
        state
            .device_authorization_proof_verifiers
            .insert(key, input.device_authorization_proof_verifier);
        state
            .events
            .entry(input.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &input.workspace_id,
                CompactEventKind::DeviceApproved,
                &device.device_id,
            ));
        Ok(device)
    }

    fn list_device_trust(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        let mut pending_requests = state
            .device_requests
            .values()
            .filter(|request| {
                if request.workspace_id != workspace_id {
                    return false;
                }
                match request.state {
                    DeviceRequestState::Pending => true,
                    DeviceRequestState::Approved => state
                        .grants
                        .get(&request.request_id)
                        .map(|grant| grant.accepted_at.is_none())
                        .unwrap_or(true),
                    DeviceRequestState::Denied | DeviceRequestState::Expired => false,
                }
            })
            .cloned()
            .collect::<Vec<_>>();
        pending_requests.sort_by(|left, right| left.request_id.cmp(&right.request_id));

        let mut authorized_devices = state
            .authorized_devices
            .values()
            .filter(|device| device.workspace_id == workspace_id && device.revoked_at.is_none())
            .cloned()
            .collect::<Vec<_>>();
        authorized_devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));

        let mut revoked_devices = state
            .revoked_devices
            .values()
            .filter(|device| device.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        revoked_devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));

        Ok(DeviceApprovalRequestList {
            pending_requests,
            authorized_devices,
            revoked_devices,
        })
    }

    fn approve_device_request(
        &self,
        input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        if let Some(existing) = state.grants.get(&input.request_id) {
            return Ok(existing.clone());
        }
        let request = state
            .device_requests
            .get(&input.request_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::DeviceRequestMissing {
                request_id: input.request_id.clone(),
            })?;
        if request.state != DeviceRequestState::Pending {
            return Err(ControlPlaneError::Conflict {
                resource: "device request",
                reason: "only pending requests can be approved",
            });
        }
        if request.expires_at <= self.clock.peek() {
            let request_mut = state
                .device_requests
                .get_mut(&input.request_id)
                .expect("request exists");
            request_mut.state = DeviceRequestState::Expired;
            return Err(ControlPlaneError::Conflict {
                resource: "device request",
                reason: "device request has expired",
            });
        }
        Self::ensure_authorized_approver(
            &state,
            &request.workspace_id,
            &input.approved_by_device_id,
            &input.approved_by_device_proof,
            "approve-device-request",
            &input.request_id,
        )?;
        Self::ensure_not_revoked(&state, &request.workspace_id, &request.device_id)?;

        let grant_acceptance_proof_verifier = input.grant_acceptance_proof_verifier.clone();
        let approval = self.build_device_approval(&request, input, false);
        let request_mut = state
            .device_requests
            .get_mut(&approval.request_id)
            .expect("request exists");
        request_mut.state = DeviceRequestState::Approved;
        state
            .grants
            .insert(approval.request_id.clone(), approval.clone());
        state
            .grant_acceptance_proof_verifiers
            .insert(approval.request_id.clone(), grant_acceptance_proof_verifier);

        Ok(approval)
    }

    fn deny_device_request(&self, input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let request = state
            .device_requests
            .get(&input.request_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::DeviceRequestMissing {
                request_id: input.request_id.clone(),
            })?;
        if request.state != DeviceRequestState::Pending {
            return Err(ControlPlaneError::Conflict {
                resource: "device request",
                reason: "only pending requests can be denied",
            });
        }
        Self::ensure_authorized_approver(
            &state,
            &request.workspace_id,
            &input.denied_by_device_id,
            &input.denied_by_device_proof,
            "deny-device-request",
            &input.request_id,
        )?;
        let denied_at = self.clock.now();
        let request_mut = state
            .device_requests
            .get_mut(&input.request_id)
            .expect("request exists");
        request_mut.state = DeviceRequestState::Denied;
        let denial = DeviceDenial {
            request_id: input.request_id,
            workspace_id: request.workspace_id,
            device_id: request.device_id,
            denied_by_device_id: input.denied_by_device_id,
            denied_at,
            reason: input.reason,
        };
        state
            .denials
            .insert(denial.request_id.clone(), denial.clone());
        state
            .events
            .entry(denial.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &denial.workspace_id,
                CompactEventKind::DeviceDenied,
                &denial.request_id,
            ));
        Ok(denial)
    }

    fn revoke_device(
        &self,
        input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            &input.workspace_id,
            &input.revoked_by_device_id,
            &input.revoked_by_device_proof,
            "revoke-device",
            &input.device_id,
        )?;
        let key = (input.workspace_id.clone(), input.device_id.clone());
        Self::ensure_revocation_keeps_trust_path(&state, &input.workspace_id, &input.device_id)?;
        let Some(mut authorized) = state.authorized_devices.remove(&key) else {
            return Err(ControlPlaneError::Conflict {
                resource: "authorized device",
                reason: "device is not authorized for this workspace",
            });
        };
        let revoked_at = self.clock.now();
        authorized.revoked_at = Some(revoked_at);
        let revoked = RevokedDeviceRecord {
            workspace_id: input.workspace_id.clone(),
            device_id: input.device_id.clone(),
            device_name: authorized.device_name,
            platform: authorized.platform,
            device_fingerprint: authorized.device_fingerprint,
            revoked_at,
            revoked_by_device_id: input.revoked_by_device_id,
            reason: input.reason,
        };
        state.revoked_devices.insert(key, revoked.clone());
        state
            .device_authorization_proof_verifiers
            .remove(&(input.workspace_id.clone(), input.device_id.clone()));
        state
            .events
            .entry(input.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &input.workspace_id,
                CompactEventKind::DeviceRevoked,
                &input.device_id,
            ));
        Ok(revoked)
    }

    fn get_encrypted_device_grant(
        &self,
        request_id: &str,
        device_id: &str,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        let state = self.state.lock().expect("fake control plane poisoned");
        let Some(grant) = state.grants.get(request_id) else {
            return Ok(None);
        };
        if grant.device_id != device_id {
            return Ok(None);
        }
        if grant.expires_at <= self.clock.peek() {
            return Err(ControlPlaneError::Limited {
                capability: "device-grant",
                reason: "grant has expired",
            });
        }
        Ok(Some(grant.clone()))
    }

    fn confirm_device_grant_accepted(
        &self,
        input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let (mut grant, first_acceptance) = {
            let grant = state.grants.get(&input.request_id).ok_or_else(|| {
                ControlPlaneError::DeviceRequestMissing {
                    request_id: input.request_id.clone(),
                }
            })?;
            if grant.device_id != input.device_id {
                return Err(ControlPlaneError::Limited {
                    capability: "device-grant",
                    reason: "grant can only be accepted by the requesting device",
                });
            }
            if grant.expires_at <= self.clock.peek() {
                return Err(ControlPlaneError::Limited {
                    capability: "device-grant",
                    reason: "grant has expired",
                });
            }
            (grant.clone(), grant.accepted_at.is_none())
        };
        if first_acceptance {
            let request = state
                .device_requests
                .get(&input.request_id)
                .cloned()
                .ok_or_else(|| ControlPlaneError::DeviceRequestMissing {
                    request_id: input.request_id.clone(),
                })?;
            Self::ensure_not_revoked(&state, &request.workspace_id, &request.device_id)?;
            let expected_acceptance_proof = state
                .grant_acceptance_proof_verifiers
                .get(&input.request_id)
                .ok_or(ControlPlaneError::Limited {
                    capability: "device-grant",
                    reason: "grant acceptance proof is missing",
                })?;
            if expected_acceptance_proof
                != &grant_acceptance_proof_verifier(&input.grant_acceptance_proof)
            {
                return Err(ControlPlaneError::Limited {
                    capability: "device-grant",
                    reason: "grant acceptance proof does not match",
                });
            }
            let pending_verifier = state
                .pending_device_proof_verifiers
                .get(&input.request_id)
                .cloned()
                .ok_or(ControlPlaneError::Limited {
                    capability: "device-trust",
                    reason: "pending device proof is missing",
                })?;
            let accepted_at = self.clock.now();
            let grant_mut = state
                .grants
                .get_mut(&input.request_id)
                .expect("grant exists after immutable lookup");
            grant_mut.accepted_at = Some(accepted_at);
            grant = grant_mut.clone();
            let authorized = self.authorized_device(
                &request,
                Some(grant.approved_by_device_id.clone()),
                accepted_at,
            );
            state.authorized_devices.insert(
                (
                    authorized.workspace_id.clone(),
                    authorized.device_id.clone(),
                ),
                authorized,
            );
            state.device_authorization_proof_verifiers.insert(
                (request.workspace_id.clone(), request.device_id.clone()),
                pending_verifier,
            );
            state
                .events
                .entry(grant.workspace_id.clone())
                .or_default()
                .push(self.build_event(
                    &grant.workspace_id,
                    CompactEventKind::DeviceApproved,
                    &grant.device_id,
                ));
        } else if state
            .revoked_devices
            .contains_key(&(grant.workspace_id.clone(), grant.device_id.clone()))
            || !state
                .authorized_devices
                .contains_key(&(grant.workspace_id.clone(), grant.device_id.clone()))
        {
            return Err(ControlPlaneError::Limited {
                capability: "device-trust",
                reason: "accepted grant no longer authorizes this device",
            });
        }
        Ok(grant)
    }

    fn create_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            &input.workspace_id,
            &input.created_by_device_id,
            &input.created_by_device_proof,
            "create-recovery-envelope",
            &input.envelope_id,
        )?;
        let key = (input.workspace_id.clone(), input.envelope_id.clone());
        if let Some(existing) = state.recovery_envelopes.get(&key) {
            let existing_proof = state.recovery_proof_verifiers.get(&key);
            if existing.ciphertext == input.ciphertext
                && existing.fingerprint == input.fingerprint
                && existing.created_by_device_id == input.created_by_device_id
                && existing_proof == Some(&input.recovery_proof_verifier)
            {
                return Ok(existing.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "recovery-envelope",
                reason: "envelope id already exists with different metadata",
            });
        }
        let record = RecoveryEnvelopeRecord {
            workspace_id: input.workspace_id.clone(),
            envelope_id: input.envelope_id,
            created_by_device_id: input.created_by_device_id,
            ciphertext: input.ciphertext,
            fingerprint: input.fingerprint,
            state: RecoveryEnvelopeState::GeneratedUnverified,
            created_at: self.clock.now(),
            verified_at: None,
            rotated_at: None,
            revoked_at: None,
        };
        let record_key = (record.workspace_id.clone(), record.envelope_id.clone());
        state
            .recovery_proof_verifiers
            .insert(record_key.clone(), input.recovery_proof_verifier);
        state.recovery_envelopes.insert(record_key, record.clone());
        state
            .events
            .entry(input.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &input.workspace_id,
                CompactEventKind::RecoveryKeyCreated,
                &record.envelope_id,
            ));
        Ok(record)
    }

    fn verify_recovery_envelope(
        &self,
        workspace_id: &str,
        envelope_id: &str,
        verified_by_device_id: &str,
        verified_by_device_proof: &str,
        recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            workspace_id,
            verified_by_device_id,
            verified_by_device_proof,
            "verify-recovery-envelope",
            envelope_id,
        )?;
        let key = (workspace_id.to_string(), envelope_id.to_string());
        let expected_verifier = state.recovery_proof_verifiers.get(&key).ok_or_else(|| {
            ControlPlaneError::ObjectMissing {
                object_key: envelope_id.to_string(),
            }
        })?;
        if expected_verifier
            != &recovery_proof_verifier_from_proof(recovery_proof, workspace_id, envelope_id)
        {
            return Err(ControlPlaneError::Conflict {
                resource: "recovery-envelope",
                reason: "Recovery Key proof does not match the envelope",
            });
        }
        let record = state.recovery_envelopes.get_mut(&key).ok_or_else(|| {
            ControlPlaneError::ObjectMissing {
                object_key: envelope_id.to_string(),
            }
        })?;
        match record.state {
            RecoveryEnvelopeState::GeneratedUnverified => {}
            RecoveryEnvelopeState::Active => return Ok(record.clone()),
            RecoveryEnvelopeState::Rotated | RecoveryEnvelopeState::Revoked => {
                return Err(ControlPlaneError::Limited {
                    capability: "recovery-key",
                    reason: "rotated or revoked Recovery Keys cannot be verified",
                });
            }
        }
        record.state = RecoveryEnvelopeState::Active;
        record.verified_at = Some(self.clock.now());
        let record = record.clone();
        state
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(self.build_event(
                workspace_id,
                CompactEventKind::RecoveryKeyVerified,
                envelope_id,
            ));
        Ok(record)
    }

    fn rotate_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            &input.workspace_id,
            &input.created_by_device_id,
            &input.created_by_device_proof,
            "rotate-recovery-envelope",
            &input.envelope_id,
        )?;
        let record_key = (input.workspace_id.clone(), input.envelope_id.clone());
        if let Some(existing) = state.recovery_envelopes.get(&record_key) {
            let existing_proof = state.recovery_proof_verifiers.get(&record_key);
            if existing.ciphertext == input.ciphertext
                && existing.fingerprint == input.fingerprint
                && existing.created_by_device_id == input.created_by_device_id
                && existing_proof == Some(&input.recovery_proof_verifier)
            {
                return Ok(existing.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "recovery-envelope",
                reason: "envelope id already exists with different metadata",
            });
        }
        let rotated_at = self.clock.now();
        for record in state.recovery_envelopes.values_mut() {
            if record.workspace_id == input.workspace_id
                && matches!(
                    record.state,
                    RecoveryEnvelopeState::Active | RecoveryEnvelopeState::GeneratedUnverified
                )
            {
                record.state = RecoveryEnvelopeState::Rotated;
                record.rotated_at = Some(rotated_at);
            }
        }
        let record = RecoveryEnvelopeRecord {
            workspace_id: input.workspace_id.clone(),
            envelope_id: input.envelope_id,
            created_by_device_id: input.created_by_device_id,
            ciphertext: input.ciphertext,
            fingerprint: input.fingerprint,
            state: RecoveryEnvelopeState::GeneratedUnverified,
            created_at: rotated_at,
            verified_at: None,
            rotated_at: None,
            revoked_at: None,
        };
        let record_key = (record.workspace_id.clone(), record.envelope_id.clone());
        state
            .recovery_proof_verifiers
            .insert(record_key.clone(), input.recovery_proof_verifier);
        state.recovery_envelopes.insert(record_key, record.clone());
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                CompactEventKind::RecoveryKeyRotated,
                &record.envelope_id,
            ));
        Ok(record)
    }

    fn revoke_recovery_envelope(
        &self,
        workspace_id: &str,
        envelope_id: &str,
        revoked_by_device_id: &str,
        revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            workspace_id,
            revoked_by_device_id,
            revoked_by_device_proof,
            "revoke-recovery-envelope",
            envelope_id,
        )?;
        let key = (workspace_id.to_string(), envelope_id.to_string());
        let record = state.recovery_envelopes.get_mut(&key).ok_or_else(|| {
            ControlPlaneError::ObjectMissing {
                object_key: envelope_id.to_string(),
            }
        })?;
        record.state = RecoveryEnvelopeState::Revoked;
        record.revoked_at = Some(self.clock.now());
        let record = record.clone();
        state
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(self.build_event(
                workspace_id,
                CompactEventKind::RecoveryKeyRevoked,
                envelope_id,
            ));
        Ok(record)
    }

    fn list_recovery_envelopes(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        let state = self.state.lock().expect("fake control plane poisoned");
        let mut envelopes = state
            .recovery_envelopes
            .values()
            .filter(|envelope| envelope.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        envelopes.sort_by(|left, right| left.envelope_id.cmp(&right.envelope_id));
        Ok(envelopes)
    }

    fn authorize_device_with_recovery(
        &self,
        input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        self.ensure_workspace(&input.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let envelope = state
            .recovery_envelopes
            .get(&(input.workspace_id.clone(), input.envelope_id.clone()))
            .ok_or_else(|| ControlPlaneError::ObjectMissing {
                object_key: input.envelope_id.clone(),
            })?;
        if envelope.state != RecoveryEnvelopeState::Active {
            return Err(ControlPlaneError::Limited {
                capability: "recovery-key",
                reason: "only active Recovery Keys can authorize a device",
            });
        }
        let expected_verifier = state
            .recovery_proof_verifiers
            .get(&(input.workspace_id.clone(), input.envelope_id.clone()))
            .ok_or_else(|| ControlPlaneError::ObjectMissing {
                object_key: input.envelope_id.clone(),
            })?;
        if expected_verifier
            != &recovery_proof_verifier_from_proof(
                &input.recovery_proof,
                &input.workspace_id,
                &input.envelope_id,
            )
        {
            return Err(ControlPlaneError::Conflict {
                resource: "recovery-envelope",
                reason: "Recovery Key proof does not match the envelope",
            });
        }
        let request = state
            .device_requests
            .get(&input.request_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::DeviceRequestMissing {
                request_id: input.request_id.clone(),
            })?;
        if request.workspace_id != input.workspace_id {
            return Err(ControlPlaneError::Conflict {
                resource: "device-request",
                reason: "request does not belong to this workspace",
            });
        }
        if request.state != DeviceRequestState::Pending {
            return Err(ControlPlaneError::Conflict {
                resource: "device-request",
                reason: "only pending requests can be recovered",
            });
        }
        if request.expires_at <= self.clock.peek() {
            state.device_requests.insert(
                input.request_id.clone(),
                DeviceRequest {
                    state: DeviceRequestState::Expired,
                    ..request.clone()
                },
            );
            return Err(ControlPlaneError::Conflict {
                resource: "device-request",
                reason: "device request has expired",
            });
        }
        if let Some(existing_grant) = state.grants.get(&input.request_id) {
            return Ok(existing_grant.clone());
        }
        let granted_at = self.clock.now();
        let grant = DeviceApproval {
            grant_id: self.ids.next_id("recovery-grant"),
            request_id: request.request_id.clone(),
            workspace_id: request.workspace_id.clone(),
            device_id: request.device_id.clone(),
            device_name: request.device_name.clone(),
            platform: request.platform.clone(),
            device_fingerprint: request.device_fingerprint.clone(),
            approved_by_device_id: format!("recovery:{}", input.envelope_id),
            encrypted_grant_ciphertext: input.encrypted_grant_ciphertext,
            key_epoch: input.key_epoch,
            granted_at,
            expires_at: ControlPlaneTimestamp {
                tick: granted_at.tick + input.expires_in_ticks,
            },
            accepted_at: None,
            harness_only: false,
        };
        state.device_requests.insert(
            input.request_id.clone(),
            DeviceRequest {
                state: DeviceRequestState::Approved,
                ..request.clone()
            },
        );
        state.grant_acceptance_proof_verifiers.insert(
            input.request_id.clone(),
            input.grant_acceptance_proof_verifier,
        );
        state.grants.insert(input.request_id.clone(), grant.clone());
        Ok(grant)
    }
}

#[derive(Debug, Default)]
struct FakeControlPlaneState {
    workspace_refs: BTreeMap<String, WorkspaceRef>,
    events: BTreeMap<String, Vec<CompactEvent>>,
    device_requests: BTreeMap<String, DeviceRequest>,
    device_request_by_device: BTreeMap<(String, String), String>,
    pending_device_proof_verifiers: BTreeMap<String, String>,
    authorized_devices: BTreeMap<(String, String), AuthorizedDeviceRecord>,
    device_authorization_proof_verifiers: BTreeMap<(String, String), String>,
    revoked_devices: BTreeMap<(String, String), RevokedDeviceRecord>,
    grants: BTreeMap<String, DeviceApproval>,
    grant_acceptance_proof_verifiers: BTreeMap<String, String>,
    denials: BTreeMap<String, DeviceDenial>,
    recovery_envelopes: BTreeMap<(String, String), RecoveryEnvelopeRecord>,
    recovery_proof_verifiers: BTreeMap<(String, String), String>,
    leases: BTreeMap<String, Lease>,
    upload_reservations: BTreeMap<(String, String), UploadReservation>,
    upload_idempotency_keys: BTreeMap<String, String>,
    committed_object_keys: BTreeSet<(String, String)>,
    object_retention_states: BTreeMap<(String, String), RetentionState>,
    same_object_stale_overlay_commits: BTreeSet<(String, String)>,
    object_keys: BTreeSet<(String, String)>,
    object_pointers: BTreeMap<String, Vec<ObjectPointer>>,
    object_manifests: BTreeMap<(String, String), ObjectManifestRecord>,
    manifests_by_snapshot: BTreeMap<(String, String), String>,
    work_views: BTreeMap<(String, String), WorkViewRecord>,
    conflicts: BTreeMap<(String, String), ConflictMetadataRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadReservation {
    workspace_id: String,
    object_kind: ObjectKind,
    byte_len: u64,
    content_id: Option<String>,
    intent: UploadIntent,
}

impl UploadReservation {
    fn matches_request(&self, request: &UploadIntentRequest) -> bool {
        self.workspace_id == request.workspace_id
            && self.object_kind == request.object_kind
            && self.byte_len == request.byte_len
            && self.content_id == request.content_id
    }
}

fn generated_object_key(kind: ObjectKind, seed: u64) -> String {
    match kind {
        ObjectKind::SourcePack => format!("packs_pk_{seed:016x}"),
        ObjectKind::IndexPack | ObjectKind::LocatorIndex => format!("indexes_ix_{seed:016x}"),
        ObjectKind::SnapshotManifest => format!("manifests_mf_{seed:016x}"),
        ObjectKind::AgentOverlay => format!("packs_pk_{seed:016x}"),
    }
}

fn upload_idempotency_key(request: &UploadIntentRequest) -> Option<String> {
    request.object_key.as_ref().map_or_else(
        || {
            request.content_id.as_ref().map(|content_id| {
                format!(
                    "content:{}:{}:{}:{}",
                    request.workspace_id,
                    request.object_kind.as_str(),
                    content_id,
                    request.byte_len
                )
            })
        },
        |object_key| Some(format!("object-key:{}:{object_key}", request.workspace_id)),
    )
}

fn validate_lease_create(input: &LeaseCreate) -> ControlPlaneResult<()> {
    validate_opaque_id(&input.lease_id, "lease ID")?;
    validate_opaque_id(&input.project_id, "project ID")?;
    validate_opaque_id(&input.device_id, "device ID")?;
    match input.write_target_mode {
        LeaseWriteTargetMode::Direct => {
            if input.work_view_id.is_some() {
                return Err(ControlPlaneError::Conflict {
                    resource: "agent lease",
                    reason: "direct leases must not carry a work view ID",
                });
            }
        }
        LeaseWriteTargetMode::WorkView => {
            let Some(work_view_id) = input.work_view_id.as_deref() else {
                return Err(ControlPlaneError::Conflict {
                    resource: "agent lease",
                    reason: "work-view leases require a work view ID",
                });
            };
            validate_opaque_id(work_view_id, "work view ID")?;
        }
    }
    validate_opaque_id(&input.base_snapshot_id, "base snapshot ID")?;
    validate_status_code(&input.status_code)?;
    Ok(())
}

fn conflict_metadata_same_occurrence(
    existing: &ConflictMetadataRecord,
    base_snapshot_id: &str,
    remote_snapshot_id: &str,
) -> bool {
    existing.base_snapshot_id == base_snapshot_id
        && existing.remote_snapshot_id == remote_snapshot_id
}

fn validate_lease_update(input: &LeaseUpdate) -> ControlPlaneResult<()> {
    validate_opaque_id(&input.lease_id, "lease ID")?;
    validate_opaque_id(&input.updated_by_device_id, "device ID")?;
    if let Some(status_code) = &input.status_code {
        validate_status_code(status_code)?;
    }
    match input.event_kind {
        None
        | Some(
            CompactEventKind::LeaseBlocked
            | CompactEventKind::LeaseCleanupCompleted
            | CompactEventKind::LeaseCompleted
            | CompactEventKind::LeaseExpired
            | CompactEventKind::LeaseHydrationRequested
            | CompactEventKind::LeaseRevoked
            | CompactEventKind::LeaseReviewReady
            | CompactEventKind::LeaseToolDenied
            | CompactEventKind::LeaseToolInvoked
            | CompactEventKind::LeaseUpdated
            | CompactEventKind::OverlayChanged
            | CompactEventKind::PublishRequested,
        ) => Ok(()),
        Some(_) => Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "event kind is not a lease event",
        }),
    }
}

fn validate_opaque_id(value: &str, label: &'static str) -> ControlPlaneResult<()> {
    if value.is_empty()
        || value.len() > 160
        || value.contains('/')
        || value.contains('\\')
        || value.contains('.')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
    {
        return Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: label,
        });
    }
    Ok(())
}

fn validate_status_code(status_code: &str) -> ControlPlaneResult<()> {
    if status_code.is_empty()
        || status_code.len() > 80
        || status_code.contains('/')
        || status_code.contains('\\')
        || status_code.contains('.')
        || !status_code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
    {
        return Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "status code must be compact and pathless",
        });
    }
    Ok(())
}

fn validate_optional_lease_pointer(
    state: &FakeControlPlaneState,
    workspace_id: &str,
    pointer: Option<&ObjectPointer>,
) -> ControlPlaneResult<()> {
    let Some(pointer) = pointer else {
        return Ok(());
    };
    validate_object_key(&pointer.object_key)?;
    validate_committed_pointer(state, workspace_id, pointer, ObjectKind::AgentOverlay)
}

fn lease_create_matches(existing: &Lease, input: &LeaseCreate) -> bool {
    existing.workspace_id == input.workspace_id
        && existing.project_id == input.project_id
        && existing.device_id == input.device_id
        && existing.write_target_mode == input.write_target_mode
        && existing.work_view_id == input.work_view_id
        && existing.base_snapshot_id == input.base_snapshot_id
        && existing.execution_state == input.execution_state
        && existing.output_state == input.output_state
        && existing.status_code == input.status_code
        && existing.output_object == input.output_object
        && existing.audit_object == input.audit_object
        && existing.expires_at == input.expires_at
}

fn lease_event_for_update(lease: &Lease) -> CompactEventKind {
    if lease.output_state == LeaseOutputState::ReviewReady {
        return CompactEventKind::LeaseReviewReady;
    }
    match lease.execution_state {
        LeaseExecutionState::Active => CompactEventKind::LeaseUpdated,
        LeaseExecutionState::Blocked => CompactEventKind::LeaseBlocked,
        LeaseExecutionState::Completed => CompactEventKind::LeaseCompleted,
        LeaseExecutionState::Expired => CompactEventKind::LeaseExpired,
        LeaseExecutionState::Revoked => CompactEventKind::LeaseRevoked,
    }
}

fn device_authorization_proof_valid(
    verifier: &str,
    proof: &str,
    workspace_id: &str,
    device_id: &str,
    action: &str,
    subject: &str,
) -> bool {
    let Some(public_key) = verifier.strip_prefix("dapv_p256_v1_") else {
        return false;
    };
    let Some(signature) = proof.strip_prefix("dapp_p256_v1_") else {
        return false;
    };
    let Ok(public_key) = BASE64_URL.decode(public_key) else {
        return false;
    };
    let Ok(signature) = BASE64_URL.decode(signature) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_sec1_bytes(&public_key) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    verifying_key
        .verify(
            &device_authorization_message(&[
                "bowline device authorization proof v2",
                workspace_id,
                device_id,
                action,
                subject,
            ]),
            &signature,
        )
        .is_ok()
}

fn device_authorization_message(fields: &[&str]) -> Vec<u8> {
    let mut message = Vec::new();
    for field in fields {
        message.extend_from_slice(&(field.len() as u64).to_le_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    message
}

fn recovery_proof_verifier_from_proof(
    proof: &str,
    workspace_id: &str,
    envelope_id: &str,
) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof verifier v2",
        workspace_id,
        envelope_id,
        proof,
    ]);
    format!("rkpv_{}", &hash[..32])
}

fn grant_acceptance_proof_verifier(proof: &str) -> String {
    let hash = sha256_proof_fields(&["bowline grant acceptance proof verifier v1", proof]);
    format!("gapv_{}", &hash[..32])
}

fn sha256_proof_fields(fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn validate_committed_pointer(
    state: &FakeControlPlaneState,
    workspace_id: &str,
    pointer: &ObjectPointer,
    expected_kind: ObjectKind,
) -> ControlPlaneResult<()> {
    if let Some(existing) = state
        .object_pointers
        .get(workspace_id)
        .and_then(|pointers| {
            pointers
                .iter()
                .find(|existing| existing.object_key == pointer.object_key)
        })
    {
        if existing == pointer {
            return Ok(());
        }
        return Err(ControlPlaneError::Conflict {
            resource: "object pointer",
            reason: "committed object metadata does not match manifest pointer",
        });
    }

    let reservation = state
        .upload_reservations
        .get(&(workspace_id.to_string(), pointer.object_key.clone()))
        .ok_or_else(|| ControlPlaneError::ObjectMissing {
            object_key: pointer.object_key.clone(),
        })?;
    if reservation.workspace_id != workspace_id {
        return Err(ControlPlaneError::Conflict {
            resource: "upload intent",
            reason: "object key is reserved for another workspace",
        });
    }
    if reservation.object_kind != expected_kind
        || reservation.byte_len != pointer.byte_len
        || !pointer.hash.starts_with("b3_")
        || pointer.key_epoch == 0
        || reservation.content_id.as_deref() != Some(pointer.content_id.as_str())
    {
        return Err(ControlPlaneError::Conflict {
            resource: "upload intent",
            reason: "reserved object metadata does not match committed pointer",
        });
    }
    Ok(())
}

fn manifest_commit_matches(existing: &ObjectManifestRecord, commit: &ObjectManifestCommit) -> bool {
    existing.workspace_id == commit.workspace_id
        && existing.snapshot_id == commit.snapshot_id
        && existing.manifest_id == commit.manifest_id
        && existing.manifest_object == commit.manifest_object
        && existing.pack_objects == commit.pack_objects
}

fn pointer_storage_metadata(pointer: &ObjectPointer) -> ControlPlaneResult<ObjectMetadata> {
    Ok(ObjectMetadata {
        key: StorageObjectKey::new(pointer.object_key.clone()).map_err(|_| {
            ControlPlaneError::InvalidObjectKey {
                reason: "object keys must be generated opaque pack, manifest, or overlay keys",
            }
        })?,
        kind: match pointer.kind {
            ObjectKind::SourcePack => StorageObjectKind::SourcePack,
            ObjectKind::IndexPack => StorageObjectKind::IndexPack,
            ObjectKind::LocatorIndex => StorageObjectKind::LocatorIndex,
            ObjectKind::SnapshotManifest => StorageObjectKind::SnapshotManifest,
            ObjectKind::AgentOverlay => StorageObjectKind::AgentOverlay,
        },
        byte_len: pointer.byte_len,
        hash: pointer.hash.clone(),
        key_epoch: pointer.key_epoch,
        created_by_device_id: None,
        created_at_unix_ms: pointer.created_at.tick,
        retention_state: RetentionState::Current,
        retain_until_unix_ms: None,
    })
}

fn work_event_for_lifecycle(lifecycle: WorkViewLifecycleState) -> CompactEventKind {
    match lifecycle {
        WorkViewLifecycleState::Active => CompactEventKind::WorkUpdated,
        WorkViewLifecycleState::ReviewReady => CompactEventKind::WorkReviewReady,
        WorkViewLifecycleState::Accepted => CompactEventKind::WorkAccepted,
        WorkViewLifecycleState::Discarded => CompactEventKind::WorkDiscarded,
        WorkViewLifecycleState::Expired => CompactEventKind::WorkExpired,
        WorkViewLifecycleState::Archived => CompactEventKind::WorkArchived,
    }
}
