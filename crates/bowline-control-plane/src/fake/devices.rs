use super::*;
use crate::{
    DeviceControlPlaneClient, device_request_proof_subject, device_revocation_proof_subject,
};

impl DeviceControlPlaneClient for FakeControlPlaneClient {
    fn create_bootstrap_session(
        &self,
        input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        self.ensure_workspace(&input.workspace_id)?;
        let created_at = self.clock.now();
        Ok(BootstrapSession {
            session_id: bowline_core::ids::BootstrapSessionId::new(
                self.ids.next_id("bootstrap-session"),
            ),
            workspace_id: input.workspace_id,
            token: self.ids.next_id("bootstrap-token"),
            lease_id: input.lease_id,
            lease_handoff_digest: input.lease_handoff_digest,
            runtime: input.runtime,
            setup_receipts_digest: input.setup_receipts_digest,
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
            request_id: DeviceApprovalRequestId::new(self.ids.next_id("device-request")),
            workspace_id: input.workspace_id,
            device_id: input.device_id,
            device_name: input.device_name,
            platform: input.platform,
            device_public_key: input.device_public_key,
            device_fingerprint: input.device_fingerprint,
            device_authorization_proof_verifier: input.device_authorization_proof_verifier.clone(),
            matching_code: input.matching_code,
            account_id: input.account_id,
            host: input.host,
            lease_handoff_digest: input.lease_handoff_digest,
            lease_id: input.lease_id,
            root: input.root,
            runtime: input.runtime,
            setup_receipts_digest: input.setup_receipts_digest,
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
            device_authorization_proof_verifier: Some(
                input.device_authorization_proof_verifier.clone(),
            ),
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
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        let mut pending_requests = state
            .device_requests
            .values()
            .filter(|request| {
                if &request.workspace_id != workspace_id {
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
            .filter(|device| &device.workspace_id == workspace_id && device.revoked_at.is_none())
            .cloned()
            .collect::<Vec<_>>();
        for device in &mut authorized_devices {
            device.device_authorization_proof_verifier = state
                .device_authorization_proof_verifiers
                .get(&(device.workspace_id.clone(), device.device_id.clone()))
                .cloned();
        }
        authorized_devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));

        let mut revoked_devices = state
            .revoked_devices
            .values()
            .filter(|device| &device.workspace_id == workspace_id)
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
            &device_request_proof_subject(&input.request_id),
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
            &device_request_proof_subject(&input.request_id),
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
            &device_revocation_proof_subject(&input.device_id),
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
        request_id: &DeviceApprovalRequestId,
        device_id: &DeviceId,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        let state = self.state.lock().expect("fake control plane poisoned");
        let Some(grant) = state.grants.get(request_id) else {
            return Ok(None);
        };
        if &grant.device_id != device_id {
            return Ok(None);
        }
        if grant.expires_at <= self.clock.peek() {
            return Err(ControlPlaneError::Rejected {
                code: RejectionCode::InvalidRequest,
                message: "grant has expired".to_string(),
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
                return Err(device_not_trusted(
                    "grant can only be accepted by the requesting device",
                ));
            }
            if grant.expires_at <= self.clock.peek() {
                return Err(ControlPlaneError::Rejected {
                    code: RejectionCode::InvalidRequest,
                    message: "grant has expired".to_string(),
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
                .ok_or_else(|| ControlPlaneError::Rejected {
                    code: RejectionCode::InvalidRequest,
                    message: "grant acceptance proof is missing".to_string(),
                })?;
            if expected_acceptance_proof
                != &grant_acceptance_proof_verifier(&input.grant_acceptance_proof)
            {
                return Err(device_not_trusted("grant acceptance proof does not match"));
            }
            let pending_verifier = state
                .pending_device_proof_verifiers
                .get(&input.request_id)
                .cloned()
                .ok_or_else(|| ControlPlaneError::Rejected {
                    code: RejectionCode::InvalidRequest,
                    message: "pending device proof is missing".to_string(),
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
            return Err(device_not_trusted(
                "accepted grant no longer authorizes this device",
            ));
        }
        Ok(grant)
    }
}
