//! The device-approval flow: a new device publishes a request, an already
//! trusted device approves it by sealing the workspace key for the requester,
//! and the requester accepts the encrypted grant. Every step re-derives the
//! matching code and the proof verifiers locally rather than trusting what the
//! control plane echoes back.

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, DeviceApprovalInput, DeviceRequestInput,
    DeviceRequestInputDraft, EncryptedGrantRequest, FETCH_DEVICE_GRANT_ACTION,
    GrantAcceptanceInput,
};
use bowline_core::{
    commands::{CONTRACT_VERSION, DeviceCommandAction, DevicesCommandOutput},
    devices::{
        DeviceApprovalRequest, DeviceApprovalRequestState, DeviceFingerprint, DevicePlatform,
        DeviceRecord, DeviceTrustState, EncryptedDeviceGrant, RecoveryKeyState,
    },
    ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId},
    status::RepairCommand,
};

use crate::device_keys::DeviceKeyStore;

use super::{TrustError, cache_device_proof_verifier, grants, platform_from_str, platform_string};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequestOptions {
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub device_name: String,
    pub platform: DevicePlatform,
    pub host: Option<String>,
    pub root: Option<String>,
    pub runtime: Option<String>,
    pub generated_at: String,
}

pub fn create_device_request<C, K>(
    control_plane: &C,
    key_store: &K,
    options: DeviceRequestOptions,
) -> Result<DeviceApprovalRequest, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let identity = key_store.load_or_create_device_identity()?;
    let device_authorization_proof_verifier =
        grants::device_authorization_proof_verifier(&identity).map_err(TrustError::Grant)?;
    let matching_code = matching_code(
        &options.workspace_id,
        &options.device_id,
        identity.public_key.as_str(),
        &device_authorization_proof_verifier,
    );
    let mut input = DeviceRequestInput::new(DeviceRequestInputDraft {
        workspace_id: options.workspace_id.clone(),
        device_id: options.device_id.clone(),
        device_name: options.device_name.clone(),
        device_public_key: identity.public_key.as_str().to_string(),
        // Published once, at enrollment, so any future holder of the workspace
        // key can seal a re-grant to this device without ever having met it.
        device_public_key_proof: super::key_regrants::device_public_key_attestation(
            &identity,
            &options.workspace_id,
            &options.device_id,
        )?,
        device_fingerprint: identity.fingerprint.as_str().to_string(),
        device_authorization_proof_verifier: device_authorization_proof_verifier.clone(),
        matching_code: matching_code.clone(),
    });
    input.platform = platform_string(options.platform).to_string();
    input.host = options.host;
    input.root = options.root.as_deref().map(home_relative_root);
    input.runtime = options.runtime;
    let request = control_plane.create_device_request(input)?;
    // The matching code is the only out-of-band check in the approval flow, so
    // the requesting screen must show what this device computed from its own
    // identity, never the control plane's echo. A server that substituted the
    // public key or the verifier to make itself approvable fails here instead
    // of rendering a code that matches the approver's screen.
    let mut request = core_request_from_control_plane(request);
    reject_substituted_request_echo(&request, &identity, &matching_code)?;
    request.matching_code = matching_code;
    cache_device_proof_verifier(
        key_store,
        options.workspace_id.clone(),
        options.device_id.clone(),
        device_authorization_proof_verifier,
    )?;
    Ok(request)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveDeviceOptions {
    pub workspace_id: WorkspaceId,
    pub request_id: DeviceApprovalRequestId,
    pub approver_device_id: DeviceId,
    pub generated_at: String,
}

pub fn approve_device_request<C, K>(
    control_plane: &C,
    key_store: &K,
    options: ApproveDeviceOptions,
) -> Result<DevicesCommandOutput, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let trust = control_plane.list_device_trust(&options.workspace_id)?;
    let request = trust
        .pending_requests
        .iter()
        .find(|request| {
            request.request_id == DeviceApprovalRequestId::new(options.request_id.as_str())
        })
        .cloned()
        .ok_or_else(|| {
            TrustError::MissingPendingRequest(options.request_id.as_str().to_string())
        })?;
    let expected_matching_code = matching_code(
        &request.workspace_id,
        &request.device_id,
        &request.device_public_key,
        &request.device_authorization_proof_verifier,
    );
    if request.matching_code != expected_matching_code {
        return Err(ControlPlaneError::Conflict {
            resource: "device-request",
            reason: "matching code does not bind requester verifier",
        }
        .into());
    }
    let finish_command = request
        .root
        .as_ref()
        .map(|root| {
            format!(
                "bowline setup --root {} --json",
                bowline_core::shell::quote_word(root)
            )
        })
        .unwrap_or_else(|| "bowline setup --root <path> --json".to_string());
    let keyring = key_store
        .load_workspace_keyring(&options.workspace_id)?
        .filter(|keyring| !keyring.is_empty())
        .ok_or_else(|| TrustError::MissingWorkspaceKey(options.workspace_id.clone()))?;
    // The approver hands over every epoch it holds, not only the newest: the
    // workspace's existing objects are sealed under the older ones, and a
    // device that could not open them would join blind to its own history.
    let workspace_keys = keyring.materials();
    let key_epoch = keyring.established_key_epoch();
    let identity = key_store.load_or_create_device_identity()?;
    let approved_by_device_proof = grants::device_authorization_proof(
        &identity,
        &options.workspace_id,
        &options.approver_device_id,
        "approve-device-request",
        &grants::device_request_proof_subject(&options.request_id),
    )
    .map_err(TrustError::Grant)?;
    let ciphertext = grants::encrypt_workspace_keys_for_request(
        &workspace_keys,
        &request,
        grants::GrantSealSource::Approver {
            identity: &identity,
            device_id: options.approver_device_id.clone(),
        },
    )
    .map_err(TrustError::Grant)?;
    let requester_device_id = request.device_id.clone();
    let grant_acceptance_proof = grants::grant_acceptance_proof(
        &workspace_keys,
        &grants::GrantScope::DeviceEnrollment {
            request_id: options.request_id.clone(),
        },
        &requester_device_id,
    );
    let grant_acceptance_proof_verifier =
        grants::grant_acceptance_proof_verifier(&grant_acceptance_proof);
    let approval = control_plane.approve_device_request(DeviceApprovalInput {
        request_id: request.request_id.clone(),
        approved_by_device_id: options.approver_device_id.clone(),
        approved_by_device_proof,
        encrypted_grant_ciphertext: ciphertext,
        grant_acceptance_proof_verifier,
        key_epoch,
        expires_in_ticks: 600,
    })?;
    cache_device_proof_verifier(
        key_store,
        options.workspace_id.clone(),
        request.device_id.clone(),
        request.device_authorization_proof_verifier.clone(),
    )?;
    let approved_device = DeviceRecord {
        id: approval.device_id.clone(),
        name: approval.device_name.clone(),
        workspace_id: options.workspace_id.clone(),
        platform: platform_from_str(&approval.platform),
        trust_state: DeviceTrustState::Pending,
        device_fingerprint: DeviceFingerprint::new(approval.device_fingerprint.clone()),
        authorized_at: None,
        updated_at: options.generated_at.clone(),
        is_current_device: false,
        limitation_reason: Some(
            "waiting for the requester to accept its encrypted grant".to_string(),
        ),
    };

    Ok(DevicesCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Devices,
        generated_at: options.generated_at,
        action: DeviceCommandAction::Approve,
        workspace_id: Some(options.workspace_id),
        local_device: None,
        devices: vec![approved_device.clone()],
        revoked_devices: Vec::new(),
        pending_requests: Vec::new(),
        created_request: None,
        approved_device: Some(approved_device),
        denied_request: None,
        revoked_device: None,
        recovery_key: Some(RecoveryKeyState::missing()),
        next_actions: vec![RepairCommand::mutating(
            format!(
                "{} can finish login on the requesting device",
                approval.device_name
            ),
            Some(finish_command),
        )],
    })
}

pub fn accept_device_grant<C, K>(
    control_plane: &C,
    key_store: &K,
    workspace_id: &WorkspaceId,
    request_id: &DeviceApprovalRequestId,
    device_id: &DeviceId,
) -> Result<EncryptedDeviceGrant, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let identity = key_store.load_or_create_device_identity()?;
    // The requester is not a trusted device yet, so it authenticates with the
    // same key whose verifier it published when it opened the request.
    let requested_by_device_proof = grants::device_authorization_proof(
        &identity,
        workspace_id,
        device_id,
        FETCH_DEVICE_GRANT_ACTION,
        &grants::device_request_proof_subject(request_id),
    )
    .map_err(TrustError::Grant)?;
    let Some(grant) = control_plane.get_encrypted_device_grant(EncryptedGrantRequest {
        request_id: request_id.clone(),
        device_id: device_id.clone(),
        requested_by_device_proof,
    })?
    else {
        return Err(TrustError::MissingPendingRequest(
            request_id.as_str().to_string(),
        ));
    };
    let approver_device_id = grant.approved_by_device_id.clone();
    // The approver's verifier is read off the authenticated trust list, never
    // out of the grant payload: caching a verifier the grant itself supplied
    // would let whoever wrote the grant install itself as a trusted approver.
    let published_approver_proof_verifier = control_plane
        .list_device_trust(workspace_id)?
        .authorized_devices
        .into_iter()
        .find(|device| device.device_id == approver_device_id)
        .and_then(|device| device.device_authorization_proof_verifier)
        .ok_or(TrustError::Grant(grants::GrantError::AuthorizerMismatch))?;
    let scope = grants::GrantScope::DeviceEnrollment {
        request_id: request_id.clone(),
    };
    let workspace_keys = grants::open_approver_sealed_grant(grants::SealedGrantCheck {
        identity: &identity,
        ciphertext: &grant.encrypted_grant_ciphertext,
        expected_workspace_id: workspace_id,
        expected_scope: scope.clone(),
        expected_recipient_device_id: &grant.device_id,
        expected_recipient_fingerprint: &grant.device_fingerprint,
        expected_key_epoch: grant.key_epoch,
        sealer_device_id: &approver_device_id,
        published_sealer_proof_verifier: &published_approver_proof_verifier,
    })
    .map_err(TrustError::Grant)?;
    let grant_acceptance_proof = grants::grant_acceptance_proof(&workspace_keys, &scope, device_id);
    let accepted = control_plane.confirm_device_grant_accepted(GrantAcceptanceInput {
        request_id: request_id.clone(),
        device_id: device_id.clone(),
        grant_acceptance_proof,
    })?;
    // Adopt every epoch at once: the ring is what makes objects sealed before
    // this device existed readable, and the established epoch is whichever one
    // the grant was issued at.
    let mut keyring = key_store
        .load_workspace_keyring(workspace_id)?
        .unwrap_or_else(|| crate::device_keys::WorkspaceKeyring::empty(workspace_id.clone()));
    for key in workspace_keys {
        keyring.insert(key);
    }
    keyring.set_established_key_epoch(grant.key_epoch);
    key_store.store_workspace_keyring(keyring)?;
    cache_device_proof_verifier(
        key_store,
        workspace_id.clone(),
        approver_device_id.clone(),
        published_approver_proof_verifier,
    )?;
    cache_device_proof_verifier(
        key_store,
        workspace_id.clone(),
        device_id.clone(),
        grants::device_authorization_proof_verifier(&identity).map_err(TrustError::Grant)?,
    )?;
    Ok(EncryptedDeviceGrant {
        grant_id: accepted.grant_id,
        request_id: request_id.clone(),
        workspace_id: workspace_id.clone(),
        requester_device_id: device_id.clone(),
        requester_device_fingerprint: DeviceFingerprint::new(accepted.device_fingerprint),
        approver_device_id: accepted.approved_by_device_id,
        key_epoch: accepted.key_epoch,
        ciphertext: accepted.encrypted_grant_ciphertext,
        created_at: accepted.granted_at.to_string(),
        expires_at: accepted.expires_at.to_string(),
        state: bowline_core::devices::EncryptedDeviceGrantState::Accepted,
        accepted_at: accepted.accepted_at.map(|timestamp| timestamp.to_string()),
    })
}

/// The hosted service stores the pending request's root verbatim, so the
/// absolute path — which carries the account name and the home directory layout
/// — is rewritten as `~`-relative before it leaves the device. The approver
/// only needs enough to render a `bowline setup --root` hint, and `~` expands
/// on the requesting machine.
fn home_relative_root(root: &str) -> String {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return root.to_string();
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return root.to_string();
    }
    match root.strip_prefix(home.as_ref()) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => root.to_string(),
    }
}

/// The requesting device must render its own matching code, never the control
/// plane's echo — otherwise a server that substitutes the requester's key can
/// show both screens a code it computed and the human comparison proves nothing.
pub(super) fn reject_substituted_request_echo(
    request: &DeviceApprovalRequest,
    identity: &crate::device_keys::DeviceIdentity,
    local_matching_code: &str,
) -> Result<(), TrustError> {
    if request.device_public_key.as_str() != identity.public_key.as_str()
        || request.device_fingerprint.as_str() != identity.fingerprint.as_str()
        || request.matching_code != local_matching_code
    {
        return Err(ControlPlaneError::Conflict {
            resource: "device-request",
            reason: "control plane echoed device identity this device did not send",
        }
        .into());
    }
    Ok(())
}

pub(crate) fn core_request_from_control_plane(
    request: bowline_control_plane::DeviceRequest,
) -> DeviceApprovalRequest {
    DeviceApprovalRequest {
        request_id: request.request_id,
        workspace_id: request.workspace_id,
        requester_device_id: request.device_id,
        device_name: request.device_name,
        platform: platform_from_str(&request.platform),
        device_public_key: bowline_core::devices::PublicDeviceKey::new(request.device_public_key),
        device_fingerprint: DeviceFingerprint::new(request.device_fingerprint),
        matching_code: request.matching_code,
        requested_at: request.requested_at.to_string(),
        expires_at: request.expires_at.to_string(),
        state: match request.state {
            bowline_control_plane::DeviceRequestState::Pending => {
                DeviceApprovalRequestState::Pending
            }
            bowline_control_plane::DeviceRequestState::Approved => {
                DeviceApprovalRequestState::Approved
            }
            bowline_control_plane::DeviceRequestState::Denied => DeviceApprovalRequestState::Denied,
            bowline_control_plane::DeviceRequestState::Expired => {
                DeviceApprovalRequestState::Expired
            }
        },
        host: request.host,
        // The hosted device request no longer carries agent-lease coupling
        // (Plan 111); the core fields die with the next core sweep.
        root: request.root,
        setup_receipts_digest: request.setup_receipts_digest,
    }
}

pub(super) use grants::device_matching_code as matching_code;

pub fn devices_output_for_request(
    generated_at: String,
    request: DeviceApprovalRequest,
) -> DevicesCommandOutput {
    DevicesCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Devices,
        generated_at,
        action: DeviceCommandAction::Request,
        workspace_id: Some(request.workspace_id.clone()),
        local_device: None,
        devices: Vec::new(),
        revoked_devices: Vec::new(),
        pending_requests: vec![request.clone()],
        created_request: Some(request),
        approved_device: None,
        denied_request: None,
        revoked_device: None,
        recovery_key: Some(RecoveryKeyState::missing()),
        next_actions: vec![RepairCommand::mutating(
            "Approve this device from an already trusted device".to_string(),
            None,
        )],
    }
}

#[cfg(test)]
mod tests {
    use bowline_control_plane::{
        ControlPlaneError, DeterministicClock, DeterministicIdGenerator, DeviceApprovalInput,
        DeviceControlPlaneClient, FakeControlPlaneClient,
    };
    use bowline_core::{
        devices::DevicePlatform,
        ids::{DeviceId, WorkspaceId},
    };

    use super::{
        DeviceRequestOptions, accept_device_grant, create_device_request,
        devices_output_for_request,
    };
    use crate::{
        device_keys::DeviceKeyStore,
        fakes::FakeKeychain,
        trust::{TrustError, ensure_first_device_trust_root, grants},
    };

    #[test]
    fn request_output_does_not_reuse_requester_root_for_approval_command() {
        let control_plane = FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("request-output-action-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-request-output");
        control_plane.create_workspace(workspace_id.as_str());
        let requester_keychain = FakeKeychain::default();
        let request = create_device_request(
            &control_plane,
            &requester_keychain,
            DeviceRequestOptions {
                workspace_id,
                device_id: DeviceId::new("fresh-linux"),
                device_name: "Fresh Linux".to_string(),
                platform: DevicePlatform::Linux,
                host: None,
                root: Some("~/Remote Code".to_string()),
                runtime: None,
                generated_at: "t000000000002".to_string(),
            },
        )
        .expect("fresh device request");

        let output = devices_output_for_request("t000000000003".to_string(), request);

        assert_eq!(
            output
                .created_request
                .as_ref()
                .and_then(|request| request.root.as_deref()),
            Some("~/Remote Code")
        );
        assert_eq!(
            output
                .next_actions
                .first()
                .and_then(|action| action.command.as_deref()),
            None
        );
    }

    #[test]
    fn rejected_grant_acceptance_does_not_store_decrypted_workspace_key() {
        let control_plane = FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("grant-acceptance-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-grant-acceptance");
        control_plane.create_workspace(workspace_id.as_str());
        let trusted_keychain = FakeKeychain::default();
        ensure_first_device_trust_root(
            &control_plane,
            &trusted_keychain,
            workspace_id.clone(),
            DeviceId::new("trusted-device"),
            "Trusted Mac",
            DevicePlatform::Macos,
            "t000000000001",
        )
        .expect("first device");
        let workspace_key = trusted_keychain
            .load_workspace_key(&workspace_id)
            .expect("trusted keychain readable")
            .expect("trusted keychain has workspace key");
        let requester_keychain = FakeKeychain::default();
        let requester_device_id = DeviceId::new("fresh-linux");
        let request = create_device_request(
            &control_plane,
            &requester_keychain,
            DeviceRequestOptions {
                workspace_id: workspace_id.clone(),
                device_id: requester_device_id.clone(),
                device_name: "Fresh Linux".to_string(),
                platform: DevicePlatform::Linux,
                host: None,
                root: Some("~/Code".to_string()),
                runtime: None,
                generated_at: "t000000000002".to_string(),
            },
        )
        .expect("fresh device request");
        let pending_request = control_plane
            .list_device_trust(&workspace_id)
            .expect("trust list")
            .pending_requests
            .into_iter()
            .find(|pending| pending.request_id == request.request_id)
            .expect("pending request");
        let ciphertext = grants::encrypt_workspace_keys_for_request(
            std::slice::from_ref(&workspace_key),
            &pending_request,
            grants::GrantSealSource::Approver {
                identity: &trusted_keychain
                    .load_or_create_device_identity()
                    .expect("trusted identity"),
                device_id: DeviceId::new("trusted-device"),
            },
        )
        .expect("grant ciphertext");
        control_plane
            .approve_device_request_for_harness(DeviceApprovalInput {
                request_id: request.request_id.clone(),
                approved_by_device_id: DeviceId::new("trusted-device"),
                approved_by_device_proof: String::new(),
                encrypted_grant_ciphertext: ciphertext,
                grant_acceptance_proof_verifier: "gap_wrong".to_string(),
                key_epoch: workspace_key.key_epoch,
                expires_in_ticks: 600,
            })
            .expect("harness approval");

        let error = accept_device_grant(
            &control_plane,
            &requester_keychain,
            &workspace_id,
            &request.request_id,
            &requester_device_id,
        )
        .expect_err("acceptance proof mismatch rejects the grant");

        assert!(matches!(
            error,
            TrustError::ControlPlane(ControlPlaneError::Rejected {
                code: bowline_control_plane::RejectionCode::DeviceNotTrusted,
                ..
            })
        ));
        assert!(
            requester_keychain
                .load_workspace_key(&workspace_id)
                .expect("requester keychain readable")
                .is_none()
        );
        let trust = control_plane
            .list_device_trust(&workspace_id)
            .expect("trust list");
        assert!(
            !trust
                .authorized_devices
                .iter()
                .any(|device| device.device_id == DeviceId::new(requester_device_id.as_str()))
        );
    }
}
