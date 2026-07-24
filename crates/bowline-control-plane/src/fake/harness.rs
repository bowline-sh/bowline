use super::*;
use crate::{DeviceControlPlaneClient, WorkspaceControlPlaneClient};

impl FakeControlPlaneClient {
    pub fn new(clock: DeterministicClock, ids: DeterministicIdGenerator) -> Self {
        Self {
            clock,
            ids,
            local_device_id: None,
            state: Arc::new(Mutex::new(FakeControlPlaneState::default())),
            signed_url_overrides: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn with_local_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.local_device_id = Some(device_id.into());
        self
    }

    pub fn create_workspace(&self, workspace_id: impl Into<String>) -> WorkspaceRef {
        // Pure establishment: seeds a headless version-0 genesis ref. The first
        // real head arrives later via a genesis compare-and-swap.
        let workspace_id = WorkspaceId::new(workspace_id);
        self.create_workspace_ref(&workspace_id)
            .expect("fake workspace creation is infallible")
    }

    pub fn append_event(
        &self,
        workspace_id: &str,
        kind: CompactEventKind,
        subject: &str,
    ) -> CompactEvent {
        let workspace_id = WorkspaceId::new(workspace_id);
        let event = self.build_event(&workspace_id, kind, subject);
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .events
            .entry(workspace_id)
            .or_default()
            .push(event.clone());
        event
    }

    pub fn set_workspace_key_epoch(&self, workspace_id: &str, key_epoch: u32) {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .workspace_key_epochs
            .insert(WorkspaceId::new(workspace_id), key_epoch);
    }

    pub fn revoke_device_grant(&self, request_id: &DeviceApprovalRequestId) {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .revoked_grants
            .insert(request_id.clone());
    }

    pub fn request_device(
        &self,
        workspace_id: &str,
        device_id: &str,
        device_name: &str,
    ) -> DeviceRequest {
        let input = DeviceRequestInput::new(DeviceRequestInputDraft {
            workspace_id: WorkspaceId::new(workspace_id),
            device_id: DeviceId::new(device_id),
            device_name: device_name.to_string(),
            device_public_key: format!("age1{device_id}"),
            device_fingerprint: format!("fp_{device_id}"),
            device_authorization_proof_verifier: format!("dapv_harness_{device_id}"),
            matching_code: "phase4-smoke".to_string(),
        });
        self.create_device_request(input)
            .expect("fake device request is infallible")
    }

    pub fn grant_device(
        &self,
        request_id: &str,
        approved_by_device_id: &str,
    ) -> Option<DeviceApproval> {
        self.approve_device_request_for_harness(DeviceApprovalInput {
            request_id: DeviceApprovalRequestId::new(request_id),
            approved_by_device_id: DeviceId::new(approved_by_device_id),
            approved_by_device_proof: String::new(),
            encrypted_grant_ciphertext: "bowline-harness-grant".to_string(),
            grant_acceptance_proof_verifier: String::new(),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .ok()
    }

    pub fn put_object_pointer(&self, workspace_id: &str, pointer: ObjectPointer) -> CompactEvent {
        let workspace_id = WorkspaceId::new(workspace_id);
        let event = self.build_event(
            &workspace_id,
            CompactEventKind::ObjectPointerAdded,
            &pointer.object_key,
        );
        let mut state = self.state.lock().expect("fake control plane poisoned");
        state
            .object_keys
            .insert((workspace_id.clone(), pointer.object_key.clone()));
        state
            .committed_object_keys
            .insert((workspace_id.clone(), pointer.object_key.clone()));
        state.object_retention_states.insert(
            (workspace_id.clone(), pointer.object_key.clone()),
            RetentionState::Current,
        );
        state
            .object_pointers
            .entry(workspace_id.clone())
            .or_default()
            .push(pointer);
        state
            .events
            .entry(workspace_id)
            .or_default()
            .push(event.clone());
        event
    }

    pub fn object_pointers(&self, workspace_id: &str) -> Vec<ObjectPointer> {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .object_pointers
            .get(&WorkspaceId::new(workspace_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Arm a one-shot CAS-stale race: the next `compare_and_swap_workspace_ref`
    /// for `workspace_id` swaps the stored ref to `current` and returns a
    /// `StaleRef` carrying it, regardless of the caller's expected version. This
    /// simulates a remote advance discovered at CAS time, so tests can exercise
    /// the runtime `Upload -> Stale` arm without pre-advancing the ref (which
    /// would instead be observed up front and planned as a stale merge).
    pub fn make_next_workspace_ref_cas_stale_for_harness(
        &self,
        workspace_id: &str,
        current: WorkspaceRef,
    ) {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .next_workspace_ref_cas_stale
            .insert(WorkspaceId::new(workspace_id), current);
    }

    pub fn events(&self, workspace_id: &str) -> Vec<CompactEvent> {
        self.list_events(&WorkspaceId::new(workspace_id))
            .unwrap_or_default()
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
        workspace_id: &WorkspaceId,
        kind: CompactEventKind,
        subject: impl AsRef<str>,
    ) -> CompactEvent {
        CompactEvent {
            event_id: bowline_core::ids::EventId::new(self.ids.next_id("event")),
            workspace_id: workspace_id.clone(),
            at: self.clock.now(),
            kind,
            subject: subject.as_ref().to_string(),
        }
    }

    pub(super) fn ensure_workspace(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<()> {
        self.ensure_online()?;
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
                workspace_id: workspace_id.clone(),
            })
        }
    }

    pub(super) fn fake_signed_url(
        &self,
        action: &str,
        object_key: &str,
        range: Option<&ByteRange>,
    ) -> String {
        if let Some(url) = self
            .signed_url_overrides
            .lock()
            .expect("fake signed URL overrides poisoned")
            .get(action)
        {
            return url.clone();
        }
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
            grant_id: bowline_core::ids::EncryptedDeviceGrantId::new(
                self.ids.next_id("device-grant"),
            ),
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
        approved_by_device_id: Option<DeviceId>,
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
            device_authorization_proof_verifier: None,
            revoked_at: None,
        }
    }

    pub(super) fn ensure_authorized_approver(
        state: &FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        proof: &str,
        action: &str,
        subject: &str,
    ) -> ControlPlaneResult<()> {
        match state
            .authorized_devices
            .get(&(workspace_id.clone(), device_id.clone()))
        {
            Some(device) if device.revoked_at.is_none() => {
                let Some(verifier) = state
                    .device_authorization_proof_verifiers
                    .get(&(workspace_id.clone(), device_id.clone()))
                else {
                    return Err(device_not_trusted("trusted device proof is missing"));
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
                    Err(device_not_trusted("trusted device proof does not match"))
                }
            }
            _ => Err(device_not_trusted(
                "approver is not a trusted non-revoked device",
            )),
        }
    }

    pub(super) fn ensure_not_revoked(
        state: &FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> ControlPlaneResult<()> {
        if state
            .revoked_devices
            .contains_key(&(workspace_id.clone(), device_id.clone()))
        {
            return Err(device_not_trusted(
                "revoked devices cannot receive new grants",
            ));
        }
        Ok(())
    }

    pub(super) fn ensure_online(&self) -> ControlPlaneResult<()> {
        if self.is_offline() {
            return Err(Self::offline_transport_error());
        }
        Ok(())
    }

    pub(super) fn ensure_trusted_device_if_configured(
        state: &FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: Option<&DeviceId>,
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
            return Err(device_not_trusted(
                "trusted-device workspace access requires a local device",
            ));
        };
        match state
            .authorized_devices
            .get(&(workspace_id.clone(), device_id.clone()))
        {
            Some(device) if device.revoked_at.is_none() => Ok(()),
            _ => Err(device_not_trusted(
                "device is not trusted for this workspace",
            )),
        }
    }

    pub(super) fn ensure_revocation_keeps_trust_path(
        state: &FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
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
            &envelope.workspace_id == workspace_id
                && envelope.state == RecoveryEnvelopeState::Active
        });
        if active_recovery_key {
            return Ok(());
        }
        Err(device_not_trusted(
            "cannot revoke the last trusted device without another trusted device or active Recovery Key",
        ))
    }
}
