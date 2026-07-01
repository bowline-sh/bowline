use super::*;
use crate::{DeviceControlPlaneClient, LeaseControlPlaneClient, WorkspaceControlPlaneClient};

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
        let mut input = DeviceRequestInput::new(DeviceRequestInputDraft {
            workspace_id: workspace_id.to_string(),
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
            device_public_key: format!("age1{device_id}"),
            device_fingerprint: format!("fp_{device_id}"),
            matching_code: "phase4-smoke".to_string(),
        });
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

    pub(super) fn build_event(
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

    pub(super) fn ensure_workspace(&self, workspace_id: &str) -> ControlPlaneResult<()> {
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

    pub(super) fn ensure_local_device(&self, device_id: &str) -> ControlPlaneResult<()> {
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

    pub(super) fn fake_signed_url(
        &self,
        action: &str,
        object_key: &str,
        range: Option<&ByteRange>,
    ) -> String {
        match range {
            Some(range) => format!(
                "fake://r2/{object_key}?action={action}&offset={}&length={}",
                range.offset, range.length
            ),
            None => format!("fake://r2/{object_key}?action={action}"),
        }
    }

    pub(super) fn build_device_approval(
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

    pub(super) fn authorized_device(
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

    pub(super) fn ensure_authorized_approver(
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

    pub(super) fn ensure_not_revoked(
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

    pub(super) fn ensure_trusted_device_if_configured(
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

    pub(super) fn ensure_revocation_keeps_trust_path(
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
