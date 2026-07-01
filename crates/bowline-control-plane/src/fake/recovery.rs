use super::*;
use crate::RecoveryControlPlaneClient;

impl RecoveryControlPlaneClient for FakeControlPlaneClient {
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
