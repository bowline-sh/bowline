use super::generated::{
    HostedRecoveryAuthorizeDeviceWithRecoveryRequest, HostedRecoveryCreateRecoveryEnvelopeRequest,
    HostedRecoveryDeviceGrant, HostedRecoveryEnvelope, HostedRecoveryEnvelopeState,
    HostedRecoveryGetRecoveryEnvelopesRequest, HostedRecoveryRevokeRecoveryEnvelopeRequest,
    HostedRecoveryRotateRecoveryEnvelopeRequest, HostedRecoveryVerifyRecoveryEnvelopeRequest,
    RecoveryAuthorizeDeviceWithRecovery, RecoveryCreateRecoveryEnvelope,
    RecoveryGetRecoveryEnvelopes, RecoveryRevokeRecoveryEnvelope, RecoveryRotateRecoveryEnvelope,
    RecoveryVerifyRecoveryEnvelope,
};
use super::*;
use crate::RecoveryControlPlaneClient;

impl RecoveryControlPlaneClient for HostedControlPlaneClient {
    fn create_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        // The recovery proof material is signed by the caller and rides the typed
        // request unchanged; this boundary never re-derives a proof subject.
        let request = HostedRecoveryCreateRecoveryEnvelopeRequest {
            ciphertext: input.ciphertext,
            created_by_device_id: input.created_by_device_id.as_str().to_string(),
            created_by_device_proof: input.created_by_device_proof,
            envelope_id: input.envelope_id.as_str().to_string(),
            fingerprint: input.fingerprint,
            key_epoch: input.key_epoch,
            recovery_proof_verifier: input.recovery_proof_verifier,
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        RecoveryEnvelopeRecord::try_from(self.call::<RecoveryCreateRecoveryEnvelope>(&request)?)
    }

    fn verify_recovery_envelope(
        &self,
        workspace_id: &WorkspaceId,
        envelope_id: &RecoveryEnvelopeId,
        verified_by_device_id: &DeviceId,
        verified_by_device_proof: &str,
        recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let request = HostedRecoveryVerifyRecoveryEnvelopeRequest {
            envelope_id: envelope_id.as_str().to_string(),
            recovery_proof: recovery_proof.to_string(),
            verified_by_device_id: verified_by_device_id.as_str().to_string(),
            verified_by_device_proof: verified_by_device_proof.to_string(),
            workspace_id: workspace_id.as_str().to_string(),
        };
        RecoveryEnvelopeRecord::try_from(self.call::<RecoveryVerifyRecoveryEnvelope>(&request)?)
    }

    fn rotate_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let request = HostedRecoveryRotateRecoveryEnvelopeRequest {
            ciphertext: input.ciphertext,
            created_by_device_id: input.created_by_device_id.as_str().to_string(),
            created_by_device_proof: input.created_by_device_proof,
            envelope_id: input.envelope_id.as_str().to_string(),
            fingerprint: input.fingerprint,
            key_epoch: input.key_epoch,
            recovery_proof_verifier: input.recovery_proof_verifier,
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        RecoveryEnvelopeRecord::try_from(self.call::<RecoveryRotateRecoveryEnvelope>(&request)?)
    }

    fn revoke_recovery_envelope(
        &self,
        workspace_id: &WorkspaceId,
        envelope_id: &RecoveryEnvelopeId,
        revoked_by_device_id: &DeviceId,
        revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let request = HostedRecoveryRevokeRecoveryEnvelopeRequest {
            envelope_id: envelope_id.as_str().to_string(),
            revoked_by_device_id: revoked_by_device_id.as_str().to_string(),
            revoked_by_device_proof: revoked_by_device_proof.to_string(),
            workspace_id: workspace_id.as_str().to_string(),
        };
        RecoveryEnvelopeRecord::try_from(self.call::<RecoveryRevokeRecoveryEnvelope>(&request)?)
    }

    fn list_recovery_envelopes(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        self.call_reauthenticating::<RecoveryGetRecoveryEnvelopes, _>(
            Some(workspace_id.as_str()),
            || {
                Ok(HostedRecoveryGetRecoveryEnvelopesRequest {
                    account_session_id: self
                        .verified_account_session_id(Some(workspace_id.as_str()))?,
                    workspace_id: workspace_id.as_str().to_string(),
                })
            },
        )?
        .into_iter()
        .map(RecoveryEnvelopeRecord::try_from)
        .collect()
    }

    fn authorize_device_with_recovery(
        &self,
        input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        DeviceApproval::try_from(
            self.call_reauthenticating::<RecoveryAuthorizeDeviceWithRecovery, _>(
                Some(input.workspace_id.as_str()),
                || {
                    Ok(HostedRecoveryAuthorizeDeviceWithRecoveryRequest {
                        account_session_id: self
                            .verified_account_session_id(Some(input.workspace_id.as_str()))?,
                        ciphertext: input.encrypted_grant_ciphertext.clone(),
                        envelope_id: input.envelope_id.as_str().to_string(),
                        expires_in_ticks: input.expires_in_ticks,
                        grant_acceptance_proof_verifier: input
                            .grant_acceptance_proof_verifier
                            .clone(),
                        key_epoch: input.key_epoch,
                        recovery_proof: input.recovery_proof.clone(),
                        request_id: input.request_id.as_str().to_string(),
                        workspace_id: input.workspace_id.as_str().to_string(),
                    })
                },
            )?,
        )
    }
}

/// Convert a decoded Recovery Key envelope transport record into the
/// control-plane domain type, re-validating the closed lifecycle enum and
/// canonical timestamps at the boundary just as the former
/// `parse_recovery_envelope` did.
impl TryFrom<HostedRecoveryEnvelope> for RecoveryEnvelopeRecord {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedRecoveryEnvelope) -> Result<Self, Self::Error> {
        Ok(Self {
            workspace_id: WorkspaceId::new(dto.workspace_id),
            envelope_id: RecoveryEnvelopeId::new(dto.envelope_id),
            created_by_device_id: DeviceId::new(dto.created_by_device_id),
            ciphertext: dto.ciphertext,
            fingerprint: dto.fingerprint,
            key_epoch: dto.key_epoch,
            state: recovery_envelope_state_from_dto(dto.state),
            created_at: parse_control_timestamp(dto.created_at.as_str())
                .map_err(|error| add_field_context(error, "createdAt"))?,
            verified_at: optional_timestamp_from_dto(dto.verified_at, "verifiedAt")?,
            rotated_at: optional_timestamp_from_dto(dto.rotated_at, "rotatedAt")?,
            revoked_at: optional_timestamp_from_dto(dto.revoked_at, "revokedAt")?,
        })
    }
}

/// Convert the encrypted device grant returned by `authorizeDeviceWithRecovery`
/// into the shared `DeviceApproval` domain type. Recovery grants always carry a
/// concrete requester identity, so `deviceId`/`deviceFingerprint` are read
/// directly and `harness_only` is always false.
impl TryFrom<HostedRecoveryDeviceGrant> for DeviceApproval {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedRecoveryDeviceGrant) -> Result<Self, Self::Error> {
        Ok(Self {
            grant_id: EncryptedDeviceGrantId::new(dto.grant_id),
            request_id: DeviceApprovalRequestId::new(dto.request_id),
            workspace_id: WorkspaceId::new(dto.workspace_id),
            device_id: DeviceId::new(dto.device_id),
            device_name: dto.device_name,
            platform: dto.platform,
            device_fingerprint: dto.device_fingerprint,
            approved_by_device_id: DeviceId::new(dto.approver_device_id),
            encrypted_grant_ciphertext: dto.ciphertext,
            key_epoch: dto.key_epoch,
            granted_at: parse_control_timestamp(dto.created_at.as_str())
                .map_err(|error| add_field_context(error, "createdAt"))?,
            expires_at: parse_control_timestamp(dto.expires_at.as_str())
                .map_err(|error| add_field_context(error, "expiresAt"))?,
            accepted_at: optional_timestamp_from_dto(dto.accepted_at, "acceptedAt")?,
            harness_only: false,
        })
    }
}

fn recovery_envelope_state_from_dto(state: HostedRecoveryEnvelopeState) -> RecoveryEnvelopeState {
    match state {
        HostedRecoveryEnvelopeState::GeneratedUnverified => {
            RecoveryEnvelopeState::GeneratedUnverified
        }
        HostedRecoveryEnvelopeState::Active => RecoveryEnvelopeState::Active,
        HostedRecoveryEnvelopeState::Rotated => RecoveryEnvelopeState::Rotated,
        HostedRecoveryEnvelopeState::Revoked => RecoveryEnvelopeState::Revoked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope_dto() -> HostedRecoveryEnvelope {
        HostedRecoveryEnvelope {
            workspace_id: "ws_code".to_string(),
            envelope_id: "rk_default".to_string(),
            created_by_device_id: "dev_creator".to_string(),
            ciphertext: "ciphertext_default".to_string(),
            fingerprint: "fingerprint_default".to_string(),
            key_epoch: 3,
            state: HostedRecoveryEnvelopeState::Active,
            created_at: wire_timestamp("2026-06-23T12:00:00Z"),
            verified_at: Some(wire_timestamp("2026-06-23T12:00:01Z")),
            rotated_at: None,
            revoked_at: None,
        }
    }

    fn grant_dto() -> HostedRecoveryDeviceGrant {
        HostedRecoveryDeviceGrant {
            grant_id: "recovery-grant:request_recovery".to_string(),
            request_id: "request_recovery".to_string(),
            workspace_id: "ws_code".to_string(),
            device_id: "requester_device".to_string(),
            device_name: "requester".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "requester_fingerprint".to_string(),
            approver_device_id: "recovery:rk_default".to_string(),
            ciphertext: "grant_ciphertext".to_string(),
            key_epoch: 3,
            created_at: wire_timestamp("2026-06-23T12:00:00Z"),
            expires_at: wire_timestamp("2026-06-23T12:00:01Z"),
            accepted_at: None,
        }
    }

    #[test]
    fn envelope_dto_maps_identity_state_and_timestamps() {
        let record = RecoveryEnvelopeRecord::try_from(envelope_dto()).expect("envelope");
        assert_eq!(record.workspace_id.as_str(), "ws_code");
        assert_eq!(record.envelope_id.as_str(), "rk_default");
        assert_eq!(record.created_by_device_id.as_str(), "dev_creator");
        assert_eq!(record.key_epoch, 3);
        assert_eq!(record.state, RecoveryEnvelopeState::Active);
        assert!(record.verified_at.is_some());
        assert_eq!(record.rotated_at, None);
        assert_eq!(record.revoked_at, None);
    }

    #[test]
    fn envelope_dto_rejects_malformed_timestamps() {
        for field in ["revokedAt", "createdAt"] {
            let error = decode_dto_with_field::<_, HostedRecoveryEnvelope>(
                &envelope_dto(),
                field,
                serde_json::json!("not-a-timestamp"),
            )
            .expect_err("malformed timestamp must reject");
            assert!(error.contains("RFC 3339"), "field {field}: {error}");
        }
    }

    #[test]
    fn recovery_state_maps_every_variant() {
        let pairs = [
            (
                HostedRecoveryEnvelopeState::GeneratedUnverified,
                RecoveryEnvelopeState::GeneratedUnverified,
            ),
            (
                HostedRecoveryEnvelopeState::Active,
                RecoveryEnvelopeState::Active,
            ),
            (
                HostedRecoveryEnvelopeState::Rotated,
                RecoveryEnvelopeState::Rotated,
            ),
            (
                HostedRecoveryEnvelopeState::Revoked,
                RecoveryEnvelopeState::Revoked,
            ),
        ];
        for (dto, domain) in pairs {
            assert_eq!(recovery_envelope_state_from_dto(dto), domain);
        }
    }

    #[test]
    fn grant_dto_maps_requester_identity_and_grant_fields() {
        let approval = DeviceApproval::try_from(grant_dto()).expect("grant");
        assert_eq!(
            approval.grant_id.as_str(),
            "recovery-grant:request_recovery"
        );
        assert_eq!(approval.request_id.as_str(), "request_recovery");
        assert_eq!(approval.device_id.as_str(), "requester_device");
        assert_eq!(approval.device_fingerprint, "requester_fingerprint");
        assert_eq!(
            approval.approved_by_device_id.as_str(),
            "recovery:rk_default"
        );
        assert_eq!(approval.key_epoch, 3);
        assert_eq!(approval.accepted_at, None);
        assert!(!approval.harness_only);
    }

    #[test]
    fn grant_dto_rejects_malformed_expiry() {
        let error = decode_dto_with_field::<_, HostedRecoveryDeviceGrant>(
            &grant_dto(),
            "expiresAt",
            serde_json::json!("nope"),
        )
        .expect_err("malformed timestamp must reject");
        assert!(error.contains("RFC 3339"), "{error}");
    }
}
