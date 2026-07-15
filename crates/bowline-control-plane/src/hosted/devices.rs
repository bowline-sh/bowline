use super::generated::{
    DevicesApproveDeviceRequest, DevicesConfirmGrantAccepted,
    DevicesConfirmGrantAcceptedWithBootstrap, DevicesCreateBootstrapSession,
    DevicesCreateFirstAuthorizedDevice, DevicesCreatePendingDevice,
    DevicesCreatePendingDeviceWithBootstrap, DevicesDenyDeviceRequest, DevicesGetEncryptedGrant,
    DevicesGetEncryptedGrantWithBootstrap, DevicesListDeviceTrust, DevicesRevokeDevice,
    HostedAuthorizedDevice, HostedBootstrapSession, HostedDeviceApproval, HostedDeviceDenial,
    HostedDeviceRequest, HostedDeviceRequestState, HostedDevicesApproveDeviceRequestRequest,
    HostedDevicesConfirmGrantAcceptedRequest,
    HostedDevicesConfirmGrantAcceptedWithBootstrapRequest,
    HostedDevicesCreateBootstrapSessionRequest, HostedDevicesCreateFirstAuthorizedDeviceRequest,
    HostedDevicesCreatePendingDeviceRequest, HostedDevicesCreatePendingDeviceWithBootstrapRequest,
    HostedDevicesDenyDeviceRequestRequest, HostedDevicesGetEncryptedGrantRequest,
    HostedDevicesGetEncryptedGrantWithBootstrapRequest, HostedDevicesListDeviceTrustRequest,
    HostedDevicesListDeviceTrustResponse, HostedDevicesRevokeDeviceRequest, HostedRevokedDevice,
};
use super::*;
use crate::DeviceControlPlaneClient;

impl DeviceControlPlaneClient for HostedControlPlaneClient {
    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            // The bootstrap branch omits leaseHandoffDigest/setupReceiptsDigest;
            // those are supplied server-side from the bound bootstrap session.
            let request = HostedDevicesCreatePendingDeviceWithBootstrapRequest {
                bootstrap_token: bootstrap_token.clone(),
                device_authorization_proof_verifier: input
                    .device_authorization_proof_verifier
                    .clone(),
                device_fingerprint: input.device_fingerprint.clone(),
                device_id: input.device_id.as_str().to_string(),
                device_name: input.device_name.clone(),
                device_public_key: input.device_public_key.clone(),
                expires_at: None,
                expires_in_ticks: Some(input.expires_in_ticks),
                host: input.host.clone(),
                lease_handoff_digest: None,
                lease_id: input.lease_id.as_ref().map(|id| id.as_str().to_string()),
                matching_code: input.matching_code.clone(),
                platform: input.platform.clone(),
                request_id: None,
                root: input.root.clone(),
                runtime: input.runtime.clone(),
                setup_receipts_digest: None,
                workspace_id: input.workspace_id.as_str().to_string(),
            };
            return DeviceRequest::try_from(
                self.call::<DevicesCreatePendingDeviceWithBootstrap>(&request)?,
            );
        }

        let request = HostedDevicesCreatePendingDeviceRequest {
            account_session_id: self
                .verified_account_session_id(Some(input.workspace_id.as_str()))?,
            device_authorization_proof_verifier: input.device_authorization_proof_verifier.clone(),
            device_fingerprint: input.device_fingerprint.clone(),
            device_id: input.device_id.as_str().to_string(),
            device_name: input.device_name.clone(),
            device_public_key: input.device_public_key.clone(),
            expires_at: None,
            expires_in_ticks: Some(input.expires_in_ticks),
            host: input.host.clone(),
            lease_handoff_digest: input.lease_handoff_digest.clone(),
            lease_id: input.lease_id.as_ref().map(|id| id.as_str().to_string()),
            matching_code: input.matching_code.clone(),
            platform: input.platform.clone(),
            request_id: None,
            root: input.root.clone(),
            runtime: input.runtime.clone(),
            setup_receipts_digest: input.setup_receipts_digest.clone(),
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        DeviceRequest::try_from(self.call::<DevicesCreatePendingDevice>(&request)?)
    }

    fn create_bootstrap_session(
        &self,
        input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        let token = generate_bootstrap_token()?;
        let token_hash = sha256_token_hash(token.as_bytes());
        let proof_subject = bootstrap_session_proof_subject(&input, &token_hash);
        let mut account_session_id = None;
        let mut created_by_device_id = None;
        let mut created_by_device_proof = None;
        if self.account_session_auth_available() {
            account_session_id =
                Some(self.verified_account_session_id(Some(input.workspace_id.as_str()))?);
        } else {
            // The device proof is signed over the hand-assembled bootstrap
            // session proof subject and rides the typed request unchanged.
            created_by_device_proof = Some(self.device_proof(
                &input.workspace_id,
                "create-bootstrap-session",
                &proof_subject,
            )?);
            created_by_device_id = Some(self.device_id.clone());
        }
        let request = HostedDevicesCreateBootstrapSessionRequest {
            account_session_id,
            bootstrap_token: token.clone(),
            created_by_device_id,
            created_by_device_proof,
            expires_in_ticks: Some(input.expires_in_ticks),
            host: input.host.clone(),
            lease_handoff_digest: input.lease_handoff_digest.clone(),
            lease_id: input.lease_id.as_ref().map(|id| id.as_str().to_string()),
            root: input.root.clone(),
            runtime: input.runtime.clone(),
            setup_receipts_digest: input.setup_receipts_digest.clone(),
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        bootstrap_session_from_dto(self.call::<DevicesCreateBootstrapSession>(&request)?, token)
    }

    fn create_first_authorized_device(
        &self,
        input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        let request = HostedDevicesCreateFirstAuthorizedDeviceRequest {
            account_session_id: self
                .verified_account_session_id(Some(input.workspace_id.as_str()))?,
            device_authorization_proof_verifier: input.device_authorization_proof_verifier,
            device_fingerprint: input.device_fingerprint,
            device_id: input.device_id.as_str().to_string(),
            device_name: input.device_name,
            platform: input.platform,
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        AuthorizedDeviceRecord::try_from(self.call::<DevicesCreateFirstAuthorizedDevice>(&request)?)
    }

    fn list_device_trust(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        let request = HostedDevicesListDeviceTrustRequest {
            account_session_id: self.verified_account_session_id(Some(workspace_id.as_str()))?,
            workspace_id: workspace_id.as_str().to_string(),
        };
        DeviceApprovalRequestList::try_from(self.call::<DevicesListDeviceTrust>(&request)?)
    }

    fn approve_device_request(
        &self,
        input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let request = HostedDevicesApproveDeviceRequestRequest {
            approver_device_id: input.approved_by_device_id.as_str().to_string(),
            approver_device_proof: input.approved_by_device_proof,
            ciphertext: input.encrypted_grant_ciphertext,
            expires_at: None,
            expires_in_ticks: Some(input.expires_in_ticks),
            grant_id: None,
            grant_acceptance_proof_verifier: input.grant_acceptance_proof_verifier,
            key_epoch: input.key_epoch,
            request_id: input.request_id.as_str().to_string(),
        };
        DeviceApproval::try_from(self.call::<DevicesApproveDeviceRequest>(&request)?)
    }

    fn deny_device_request(&self, input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        let request = HostedDevicesDenyDeviceRequestRequest {
            denied_by_device_id: input.denied_by_device_id.as_str().to_string(),
            denied_by_device_proof: input.denied_by_device_proof,
            reason: input.reason,
            request_id: input.request_id.as_str().to_string(),
        };
        DeviceDenial::try_from(self.call::<DevicesDenyDeviceRequest>(&request)?)
    }

    fn revoke_device(
        &self,
        input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        let request = HostedDevicesRevokeDeviceRequest {
            device_id: input.device_id.as_str().to_string(),
            reason: input.reason,
            revoked_by_device_id: input.revoked_by_device_id.as_str().to_string(),
            revoked_by_device_proof: input.revoked_by_device_proof,
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        RevokedDeviceRecord::try_from(self.call::<DevicesRevokeDevice>(&request)?)
    }

    fn get_encrypted_device_grant(
        &self,
        request_id: &DeviceApprovalRequestId,
        device_id: &DeviceId,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let request = HostedDevicesGetEncryptedGrantWithBootstrapRequest {
                bootstrap_token: bootstrap_token.clone(),
                device_id: device_id.as_str().to_string(),
                request_id: request_id.as_str().to_string(),
            };
            return self
                .call::<DevicesGetEncryptedGrantWithBootstrap>(&request)?
                .map(DeviceApproval::try_from)
                .transpose();
        }

        let request = HostedDevicesGetEncryptedGrantRequest {
            device_id: device_id.as_str().to_string(),
            request_id: request_id.as_str().to_string(),
        };
        self.call::<DevicesGetEncryptedGrant>(&request)?
            .map(DeviceApproval::try_from)
            .transpose()
    }

    fn confirm_device_grant_accepted(
        &self,
        input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let request = HostedDevicesConfirmGrantAcceptedWithBootstrapRequest {
                bootstrap_token: bootstrap_token.clone(),
                device_id: input.device_id.as_str().to_string(),
                grant_acceptance_proof: input.grant_acceptance_proof.clone(),
                request_id: input.request_id.as_str().to_string(),
            };
            return DeviceApproval::try_from(
                self.call::<DevicesConfirmGrantAcceptedWithBootstrap>(&request)?,
            );
        }

        let request = HostedDevicesConfirmGrantAcceptedRequest {
            device_id: input.device_id.as_str().to_string(),
            grant_acceptance_proof: input.grant_acceptance_proof,
            request_id: input.request_id.as_str().to_string(),
        };
        DeviceApproval::try_from(self.call::<DevicesConfirmGrantAccepted>(&request)?)
    }
}

/// Convert a decoded pending device request transport record into the
/// control-plane domain type, re-validating the closed lifecycle enum and
/// canonical timestamps at the boundary just as the former `parse_device_request`
/// did.
impl TryFrom<HostedDeviceRequest> for DeviceRequest {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedDeviceRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            request_id: DeviceApprovalRequestId::new(dto.request_id),
            workspace_id: WorkspaceId::new(dto.workspace_id),
            device_id: DeviceId::new(dto.device_id),
            device_name: dto.device_name,
            platform: dto.platform,
            device_public_key: dto.device_public_key,
            device_fingerprint: dto.device_fingerprint,
            device_authorization_proof_verifier: dto.device_authorization_proof_verifier,
            matching_code: dto.matching_code,
            account_id: dto.account_id.map(AccountId::new),
            host: dto.host,
            lease_handoff_digest: dto.lease_handoff_digest,
            lease_id: dto.lease_id.map(LeaseId::new),
            root: dto.root,
            runtime: dto.runtime,
            setup_receipts_digest: dto.setup_receipts_digest,
            requested_at: parse_control_timestamp(&dto.requested_at)
                .map_err(|error| add_field_context(error, "requestedAt"))?,
            expires_at: parse_control_timestamp(&dto.expires_at)
                .map_err(|error| add_field_context(error, "expiresAt"))?,
            state: device_request_state_from_dto(dto.state),
        })
    }
}

/// Convert a decoded authorized device transport record into the domain type.
impl TryFrom<HostedAuthorizedDevice> for AuthorizedDeviceRecord {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedAuthorizedDevice) -> Result<Self, Self::Error> {
        Ok(Self {
            workspace_id: WorkspaceId::new(dto.workspace_id),
            device_id: DeviceId::new(dto.device_id),
            device_name: dto.device_name,
            platform: dto.platform,
            device_fingerprint: dto.device_fingerprint,
            authorized_at: parse_control_timestamp(&dto.authorized_at)
                .map_err(|error| add_field_context(error, "authorizedAt"))?,
            authorized_by_device_id: dto.authorized_by_device_id.map(DeviceId::new),
            device_authorization_proof_verifier: dto.device_authorization_proof_verifier,
            revoked_at: optional_timestamp_from_dto(dto.revoked_at, "revokedAt")?,
        })
    }
}

/// Convert a decoded revoked device transport record into the domain type.
impl TryFrom<HostedRevokedDevice> for RevokedDeviceRecord {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedRevokedDevice) -> Result<Self, Self::Error> {
        Ok(Self {
            workspace_id: WorkspaceId::new(dto.workspace_id),
            device_id: DeviceId::new(dto.device_id),
            device_name: dto.device_name,
            platform: dto.platform,
            device_fingerprint: dto.device_fingerprint,
            revoked_at: parse_control_timestamp(&dto.revoked_at)
                .map_err(|error| add_field_context(error, "revokedAt"))?,
            revoked_by_device_id: DeviceId::new(dto.revoked_by_device_id),
            reason: dto.reason,
        })
    }
}

/// Convert a decoded encrypted device grant into the shared `DeviceApproval`
/// domain type. The transport record carries the requesting device identity
/// directly (deviceId/deviceFingerprint), so `harness_only` is always false.
impl TryFrom<HostedDeviceApproval> for DeviceApproval {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedDeviceApproval) -> Result<Self, Self::Error> {
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
            granted_at: parse_control_timestamp(&dto.created_at)
                .map_err(|error| add_field_context(error, "createdAt"))?,
            expires_at: parse_control_timestamp(&dto.expires_at)
                .map_err(|error| add_field_context(error, "expiresAt"))?,
            accepted_at: optional_timestamp_from_dto(dto.accepted_at, "acceptedAt")?,
            harness_only: false,
        })
    }
}

/// Convert a decoded device denial transport record into the domain type.
impl TryFrom<HostedDeviceDenial> for DeviceDenial {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedDeviceDenial) -> Result<Self, Self::Error> {
        Ok(Self {
            request_id: DeviceApprovalRequestId::new(dto.request_id),
            workspace_id: WorkspaceId::new(dto.workspace_id),
            device_id: DeviceId::new(dto.device_id),
            denied_by_device_id: DeviceId::new(dto.denied_by_device_id),
            denied_at: parse_control_timestamp(&dto.denied_at)
                .map_err(|error| add_field_context(error, "deniedAt"))?,
            reason: dto.reason,
        })
    }
}

/// Convert the decoded device trust view into the domain list, re-validating
/// every nested record at the boundary.
impl TryFrom<HostedDevicesListDeviceTrustResponse> for DeviceApprovalRequestList {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedDevicesListDeviceTrustResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            pending_requests: dto
                .pending_requests
                .into_iter()
                .map(DeviceRequest::try_from)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
            authorized_devices: dto
                .authorized_devices
                .into_iter()
                .map(AuthorizedDeviceRecord::try_from)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
            revoked_devices: dto
                .revoked_devices
                .into_iter()
                .map(RevokedDeviceRecord::try_from)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
        })
    }
}

/// Build a `BootstrapSession` from the decoded transport record plus the
/// client-generated one-time token, which the server never echoes back.
fn bootstrap_session_from_dto(
    dto: HostedBootstrapSession,
    token: String,
) -> ControlPlaneResult<BootstrapSession> {
    Ok(BootstrapSession {
        session_id: BootstrapSessionId::new(dto.session_id),
        workspace_id: WorkspaceId::new(dto.workspace_id),
        token,
        lease_id: dto.lease_id.map(LeaseId::new),
        lease_handoff_digest: dto.lease_handoff_digest,
        runtime: dto.runtime,
        setup_receipts_digest: dto.setup_receipts_digest,
        expires_at: parse_control_timestamp(&dto.expires_at)
            .map_err(|error| add_field_context(error, "expiresAt"))?,
    })
}

fn device_request_state_from_dto(state: HostedDeviceRequestState) -> DeviceRequestState {
    match state {
        HostedDeviceRequestState::Pending => DeviceRequestState::Pending,
        HostedDeviceRequestState::Approved => DeviceRequestState::Approved,
        HostedDeviceRequestState::Denied => DeviceRequestState::Denied,
        HostedDeviceRequestState::Expired => DeviceRequestState::Expired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_dto() -> HostedDeviceRequest {
        HostedDeviceRequest {
            request_id: "request_1".to_string(),
            workspace_id: "ws_code".to_string(),
            device_id: "device_1".to_string(),
            device_name: "MacBook".to_string(),
            platform: "macos".to_string(),
            device_public_key: "device_public_key_1".to_string(),
            device_fingerprint: "fingerprint_1".to_string(),
            device_authorization_proof_verifier: "dapv_p256_v1_verifier".to_string(),
            matching_code: "123456".to_string(),
            account_id: Some("account_1".to_string()),
            host: Some("mac-mini".to_string()),
            lease_handoff_digest: None,
            lease_id: Some("lease_remote_1".to_string()),
            root: None,
            runtime: Some("codex-cloud".to_string()),
            setup_receipts_digest: None,
            requested_at: "t1730000000000".to_string(),
            expires_at: "t1730000001000".to_string(),
            state: HostedDeviceRequestState::Pending,
        }
    }

    fn approval_dto() -> HostedDeviceApproval {
        HostedDeviceApproval {
            grant_id: "device-grant:request_1".to_string(),
            request_id: "request_1".to_string(),
            workspace_id: "ws_code".to_string(),
            device_id: "device_1".to_string(),
            device_name: "MacBook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fingerprint_1".to_string(),
            approver_device_id: "device_owner".to_string(),
            ciphertext: "grant_ciphertext".to_string(),
            key_epoch: 3,
            created_at: "t1730000004000".to_string(),
            expires_at: "t1730000005000".to_string(),
            accepted_at: None,
        }
    }

    #[test]
    fn device_request_dto_maps_identity_optionals_and_state() {
        let record = DeviceRequest::try_from(request_dto()).expect("request");
        assert_eq!(record.request_id.as_str(), "request_1");
        assert_eq!(record.device_id.as_str(), "device_1");
        assert_eq!(record.platform, "macos");
        assert_eq!(
            record.account_id.as_ref().map(|id| id.as_str()),
            Some("account_1")
        );
        assert_eq!(
            record.lease_id.as_ref().map(|id| id.as_str()),
            Some("lease_remote_1")
        );
        assert_eq!(record.lease_handoff_digest, None);
        assert_eq!(record.requested_at.tick, 1_730_000_000_000);
        assert_eq!(record.expires_at.tick, 1_730_000_001_000);
        assert_eq!(record.state, DeviceRequestState::Pending);
    }

    #[test]
    fn device_request_dto_reports_malformed_timestamp_field() {
        let mut dto = request_dto();
        dto.expires_at = "not-a-timestamp".to_string();
        assert_parse_error_field(DeviceRequest::try_from(dto), "expiresAt");

        let mut requested = request_dto();
        requested.requested_at = "bad".to_string();
        assert_parse_error_field(DeviceRequest::try_from(requested), "requestedAt");
    }

    #[test]
    fn device_request_state_maps_every_variant() {
        let pairs = [
            (
                HostedDeviceRequestState::Pending,
                DeviceRequestState::Pending,
            ),
            (
                HostedDeviceRequestState::Approved,
                DeviceRequestState::Approved,
            ),
            (HostedDeviceRequestState::Denied, DeviceRequestState::Denied),
            (
                HostedDeviceRequestState::Expired,
                DeviceRequestState::Expired,
            ),
        ];
        for (dto, domain) in pairs {
            assert_eq!(device_request_state_from_dto(dto), domain);
        }
    }

    #[test]
    fn approval_dto_maps_grant_fields_and_rejects_bad_expiry() {
        let approval = DeviceApproval::try_from(approval_dto()).expect("approval");
        assert_eq!(approval.grant_id.as_str(), "device-grant:request_1");
        assert_eq!(approval.device_id.as_str(), "device_1");
        assert_eq!(approval.approved_by_device_id.as_str(), "device_owner");
        assert_eq!(approval.key_epoch, 3);
        assert_eq!(approval.accepted_at, None);
        assert!(!approval.harness_only);
        assert_eq!(approval.granted_at.tick, 1_730_000_004_000);

        let mut dto = approval_dto();
        dto.created_at = "nope".to_string();
        assert_parse_error_field(DeviceApproval::try_from(dto), "createdAt");
    }

    #[test]
    fn authorized_and_revoked_dtos_map_identity_and_timestamps() {
        let authorized = AuthorizedDeviceRecord::try_from(HostedAuthorizedDevice {
            workspace_id: "ws_code".to_string(),
            device_id: "device_1".to_string(),
            device_name: "MacBook".to_string(),
            platform: "linux".to_string(),
            device_fingerprint: "fingerprint_1".to_string(),
            authorized_at: "t1730000002000".to_string(),
            authorized_by_device_id: Some("device_owner".to_string()),
            device_authorization_proof_verifier: Some("dapv_p256_v1_verifier".to_string()),
            revoked_at: None,
        })
        .expect("authorized");
        assert_eq!(authorized.authorized_at.tick, 1_730_000_002_000);
        assert_eq!(
            authorized
                .authorized_by_device_id
                .as_ref()
                .map(|id| id.as_str()),
            Some("device_owner")
        );
        assert_eq!(authorized.revoked_at, None);

        let revoked = RevokedDeviceRecord::try_from(HostedRevokedDevice {
            workspace_id: "ws_code".to_string(),
            device_id: "device_1".to_string(),
            device_name: "MacBook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fingerprint_1".to_string(),
            revoked_at: "t1730000003000".to_string(),
            revoked_by_device_id: "device_owner".to_string(),
            reason: "rotated".to_string(),
        })
        .expect("revoked");
        assert_eq!(revoked.revoked_at.tick, 1_730_000_003_000);
        assert_eq!(revoked.revoked_by_device_id.as_str(), "device_owner");
    }

    #[test]
    fn denial_dto_maps_identity_and_reason() {
        let denial = DeviceDenial::try_from(HostedDeviceDenial {
            request_id: "request_1".to_string(),
            workspace_id: "ws_code".to_string(),
            device_id: "device_1".to_string(),
            denied_by_device_id: "device_owner".to_string(),
            denied_at: "t1730000000000".to_string(),
            reason: "unrecognized host".to_string(),
        })
        .expect("denial");
        assert_eq!(denial.request_id.as_str(), "request_1");
        assert_eq!(denial.denied_by_device_id.as_str(), "device_owner");
        assert_eq!(denial.reason, "unrecognized host");
    }

    #[test]
    fn bootstrap_session_dto_preserves_scope_and_token() {
        let session = bootstrap_session_from_dto(
            HostedBootstrapSession {
                session_id: "bootstrap-session-1".to_string(),
                workspace_id: "ws_code".to_string(),
                lease_id: Some("lease_remote_1".to_string()),
                lease_handoff_digest: Some("lease_handoff_blake3:def456".to_string()),
                runtime: Some("codex-cloud".to_string()),
                setup_receipts_digest: Some("setup_receipts_blake3:abc123".to_string()),
                expires_at: "2026-07-02T12:00:00Z".to_string(),
            },
            "token-secret".to_string(),
        )
        .expect("session");
        assert_eq!(session.session_id.as_str(), "bootstrap-session-1");
        assert_eq!(session.token, "token-secret");
        assert_eq!(
            session.lease_id.as_ref().map(|id| id.as_str()),
            Some("lease_remote_1")
        );
        assert_eq!(session.runtime.as_deref(), Some("codex-cloud"));
    }

    #[test]
    fn list_device_trust_dto_maps_every_collection() {
        let list = DeviceApprovalRequestList::try_from(HostedDevicesListDeviceTrustResponse {
            pending_requests: vec![request_dto()],
            authorized_devices: vec![HostedAuthorizedDevice {
                workspace_id: "ws_code".to_string(),
                device_id: "device_auth".to_string(),
                device_name: "MacBook".to_string(),
                platform: "macos".to_string(),
                device_fingerprint: "fingerprint_auth".to_string(),
                authorized_at: "t1730000002000".to_string(),
                authorized_by_device_id: None,
                device_authorization_proof_verifier: None,
                revoked_at: None,
            }],
            revoked_devices: vec![],
        })
        .expect("list");
        assert_eq!(list.pending_requests.len(), 1);
        assert_eq!(list.authorized_devices.len(), 1);
        assert!(list.revoked_devices.is_empty());
        assert_eq!(list.pending_requests[0].device_id.as_str(), "device_1");
        assert_eq!(list.authorized_devices[0].device_id.as_str(), "device_auth");
    }

    fn assert_parse_error_field<T: std::fmt::Debug>(result: ControlPlaneResult<T>, field: &str) {
        let error = result.expect_err("malformed value must reject");
        assert!(
            error.to_string().contains(&format!("`{field}`")),
            "error must identify field `{field}`, got: {error}"
        );
    }
}
