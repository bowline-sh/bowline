use super::*;
use crate::DeviceControlPlaneClient;

impl DeviceControlPlaneClient for HostedControlPlaneClient {
    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let mut request_args = args([
                ("bootstrapToken", Value::from(bootstrap_token.clone())),
                ("deviceId", Value::from(input.device_id.clone())),
                ("deviceName", Value::from(input.device_name.clone())),
                (
                    "deviceFingerprint",
                    Value::from(input.device_fingerprint.clone()),
                ),
                (
                    "deviceAuthorizationProofVerifier",
                    Value::from(input.device_authorization_proof_verifier.clone()),
                ),
                (
                    "devicePublicKey",
                    Value::from(input.device_public_key.clone()),
                ),
                ("expiresInTicks", number_value(input.expires_in_ticks)),
                ("matchingCode", Value::from(input.matching_code.clone())),
                ("platform", Value::from(input.platform.clone())),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]);
            if let Some(host) = input.host {
                request_args.insert("host".to_string(), Value::from(host));
            }
            if let Some(root) = input.root {
                request_args.insert("root".to_string(), Value::from(root));
            }

            let value =
                self.public_mutation("devices:createPendingDeviceWithBootstrap", request_args)?;
            return parse_device_request(&value);
        }

        let mut request_args = args([
            (
                "accountSessionId",
                Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
            ),
            ("deviceId", Value::from(input.device_id.clone())),
            ("deviceName", Value::from(input.device_name.clone())),
            (
                "deviceFingerprint",
                Value::from(input.device_fingerprint.clone()),
            ),
            (
                "deviceAuthorizationProofVerifier",
                Value::from(input.device_authorization_proof_verifier.clone()),
            ),
            (
                "devicePublicKey",
                Value::from(input.device_public_key.clone()),
            ),
            ("expiresInTicks", number_value(input.expires_in_ticks)),
            ("matchingCode", Value::from(input.matching_code.clone())),
            ("platform", Value::from(input.platform.clone())),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(host) = input.host {
            request_args.insert("host".to_string(), Value::from(host));
        }
        if let Some(root) = input.root {
            request_args.insert("root".to_string(), Value::from(root));
        }

        let value = self.public_mutation("devices:createPendingDevice", request_args)?;
        parse_device_request(&value)
    }

    fn create_bootstrap_session(
        &self,
        input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        let token = generate_bootstrap_token()?;
        let token_hash = sha256_token_hash(token.as_bytes());
        let proof_subject = bootstrap_session_proof_subject(&input, &token_hash);
        let mut request_args = args([
            ("bootstrapToken", Value::from(token.clone())),
            ("expiresInTicks", number_value(input.expires_in_ticks)),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
            );
        } else {
            let created_by_device_proof = self.device_proof(
                &input.workspace_id,
                "create-bootstrap-session",
                &proof_subject,
            )?;
            request_args.insert(
                "createdByDeviceId".to_string(),
                Value::from(self.device_id.clone()),
            );
            request_args.insert(
                "createdByDeviceProof".to_string(),
                Value::from(created_by_device_proof),
            );
        }
        if let Some(host) = input.host {
            request_args.insert("host".to_string(), Value::from(host));
        }
        if let Some(root) = input.root {
            request_args.insert("root".to_string(), Value::from(root));
        }

        let value = self.public_mutation("devices:createBootstrapSession", request_args)?;
        parse_bootstrap_session(&value, token)
    }

    fn create_first_authorized_device(
        &self,
        input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        let value = self.public_mutation(
            "devices:createFirstAuthorizedDevice",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
                ),
                ("deviceFingerprint", Value::from(input.device_fingerprint)),
                (
                    "deviceAuthorizationProofVerifier",
                    Value::from(input.device_authorization_proof_verifier),
                ),
                ("deviceId", Value::from(input.device_id)),
                ("deviceName", Value::from(input.device_name)),
                ("platform", Value::from(input.platform)),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_authorized_device(&value)
    }

    fn list_device_trust(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        let value = self.public_query(
            "devices:listDeviceTrust",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(workspace_id))?),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let object = value_object(&value)?;
        Ok(DeviceApprovalRequestList {
            pending_requests: array_field(object, "pendingRequests")?
                .iter()
                .map(parse_device_request)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
            authorized_devices: array_field(object, "authorizedDevices")?
                .iter()
                .map(parse_authorized_device)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
            revoked_devices: array_field(object, "revokedDevices")?
                .iter()
                .map(parse_revoked_device)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
        })
    }

    fn approve_device_request(
        &self,
        input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let value = self.public_mutation(
            "devices:approveDeviceRequest",
            args([
                ("approverDeviceId", Value::from(input.approved_by_device_id)),
                (
                    "approverDeviceProof",
                    Value::from(input.approved_by_device_proof),
                ),
                ("ciphertext", Value::from(input.encrypted_grant_ciphertext)),
                ("expiresInTicks", number_value(input.expires_in_ticks)),
                (
                    "grantAcceptanceProofVerifier",
                    Value::from(input.grant_acceptance_proof_verifier),
                ),
                ("keyEpoch", number_value(input.key_epoch.into())),
                ("requestId", Value::from(input.request_id)),
            ]),
        )?;
        parse_device_approval(&value)
    }

    fn deny_device_request(&self, input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        let value = self.public_mutation(
            "devices:denyDeviceRequest",
            args([
                ("deniedByDeviceId", Value::from(input.denied_by_device_id)),
                (
                    "deniedByDeviceProof",
                    Value::from(input.denied_by_device_proof),
                ),
                ("reason", Value::from(input.reason)),
                ("requestId", Value::from(input.request_id)),
            ]),
        )?;
        parse_device_denial(&value)
    }

    fn revoke_device(
        &self,
        input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        let value = self.public_mutation(
            "devices:revokeDevice",
            args([
                ("deviceId", Value::from(input.device_id)),
                ("reason", Value::from(input.reason)),
                ("revokedByDeviceId", Value::from(input.revoked_by_device_id)),
                (
                    "revokedByDeviceProof",
                    Value::from(input.revoked_by_device_proof),
                ),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_revoked_device(&value)
    }

    fn get_encrypted_device_grant(
        &self,
        request_id: &str,
        device_id: &str,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let value = self.public_query(
                "devices:getEncryptedGrantWithBootstrap",
                args([
                    ("bootstrapToken", Value::from(bootstrap_token.clone())),
                    ("deviceId", Value::from(device_id.to_string())),
                    ("requestId", Value::from(request_id.to_string())),
                ]),
            )?;
            return if matches!(value, Value::Null) {
                Ok(None)
            } else {
                parse_device_approval(&value).map(Some)
            };
        }

        let value = self.public_query(
            "devices:getEncryptedGrant",
            args([
                ("deviceId", Value::from(device_id.to_string())),
                ("requestId", Value::from(request_id.to_string())),
            ]),
        )?;
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            parse_device_approval(&value).map(Some)
        }
    }

    fn confirm_device_grant_accepted(
        &self,
        input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let value = self.public_mutation(
                "devices:confirmGrantAcceptedWithBootstrap",
                args([
                    ("bootstrapToken", Value::from(bootstrap_token.clone())),
                    ("deviceId", Value::from(input.device_id)),
                    (
                        "grantAcceptanceProof",
                        Value::from(input.grant_acceptance_proof),
                    ),
                    ("requestId", Value::from(input.request_id)),
                ]),
            )?;
            return parse_device_approval(&value);
        }

        let value = self.public_mutation(
            "devices:confirmGrantAccepted",
            args([
                ("deviceId", Value::from(input.device_id)),
                (
                    "grantAcceptanceProof",
                    Value::from(input.grant_acceptance_proof),
                ),
                ("requestId", Value::from(input.request_id)),
            ]),
        )?;
        parse_device_approval(&value)
    }
}
