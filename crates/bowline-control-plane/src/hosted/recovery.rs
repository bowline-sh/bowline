use super::*;
use crate::RecoveryControlPlaneClient;

impl RecoveryControlPlaneClient for HostedControlPlaneClient {
    fn create_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:createRecoveryEnvelope",
            args([
                ("ciphertext", Value::from(input.ciphertext)),
                ("createdByDeviceId", Value::from(input.created_by_device_id)),
                (
                    "createdByDeviceProof",
                    Value::from(input.created_by_device_proof),
                ),
                ("envelopeId", Value::from(input.envelope_id)),
                ("fingerprint", Value::from(input.fingerprint)),
                (
                    "recoveryProofVerifier",
                    Value::from(input.recovery_proof_verifier),
                ),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn verify_recovery_envelope(
        &self,
        workspace_id: &str,
        envelope_id: &str,
        verified_by_device_id: &str,
        verified_by_device_proof: &str,
        recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:verifyRecoveryEnvelope",
            args([
                ("envelopeId", Value::from(envelope_id.to_string())),
                ("recoveryProof", Value::from(recovery_proof.to_string())),
                (
                    "verifiedByDeviceId",
                    Value::from(verified_by_device_id.to_string()),
                ),
                (
                    "verifiedByDeviceProof",
                    Value::from(verified_by_device_proof.to_string()),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn rotate_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:rotateRecoveryEnvelope",
            args([
                ("ciphertext", Value::from(input.ciphertext)),
                ("createdByDeviceId", Value::from(input.created_by_device_id)),
                (
                    "createdByDeviceProof",
                    Value::from(input.created_by_device_proof),
                ),
                ("envelopeId", Value::from(input.envelope_id)),
                ("fingerprint", Value::from(input.fingerprint)),
                (
                    "recoveryProofVerifier",
                    Value::from(input.recovery_proof_verifier),
                ),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn revoke_recovery_envelope(
        &self,
        workspace_id: &str,
        envelope_id: &str,
        revoked_by_device_id: &str,
        revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:revokeRecoveryEnvelope",
            args([
                ("envelopeId", Value::from(envelope_id.to_string())),
                (
                    "revokedByDeviceId",
                    Value::from(revoked_by_device_id.to_string()),
                ),
                (
                    "revokedByDeviceProof",
                    Value::from(revoked_by_device_proof.to_string()),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn list_recovery_envelopes(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        let value = self.public_query(
            "recovery:getRecoveryEnvelopes",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(workspace_id))?),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let Value::Array(values) = value else {
            return Err(shape_error("recovery envelope list must be an array"));
        };
        values.iter().map(parse_recovery_envelope).collect()
    }

    fn authorize_device_with_recovery(
        &self,
        input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let value = self.public_mutation(
            "recovery:authorizeDeviceWithRecovery",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
                ),
                ("ciphertext", Value::from(input.encrypted_grant_ciphertext)),
                ("envelopeId", Value::from(input.envelope_id)),
                ("expiresInTicks", number_value(input.expires_in_ticks)),
                (
                    "grantAcceptanceProofVerifier",
                    Value::from(input.grant_acceptance_proof_verifier),
                ),
                ("keyEpoch", number_value(u64::from(input.key_epoch))),
                ("recoveryProof", Value::from(input.recovery_proof)),
                ("requestId", Value::from(input.request_id)),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_device_approval(&value)
    }
}
