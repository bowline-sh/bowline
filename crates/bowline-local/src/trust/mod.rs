use std::{error::Error, fmt};

use bowline_control_plane::{
    AuthorizedDeviceRecord, ControlPlaneClient, ControlPlaneError, DeviceApprovalInput,
    DeviceRequestInput, FirstAuthorizedDeviceInput, GrantAcceptanceInput,
};
use bowline_core::{
    commands::{CONTRACT_VERSION, DeviceCommandAction, DevicesCommandOutput},
    devices::{
        DeviceApprovalRequest, DeviceApprovalRequestState, DeviceFingerprint, DevicePlatform,
        DeviceRecord, DeviceTrustState, EncryptedDeviceGrant, RecoveryKeyState,
    },
    ids::{DeviceApprovalRequestId, DeviceId, EncryptedDeviceGrantId, WorkspaceId},
    status::SafeAction,
};

use crate::device_keys::{DeviceKeyError, DeviceKeyStore, WorkspaceKeyMaterial};

pub mod grants;
pub mod recovery;

#[derive(Debug)]
pub enum TrustError {
    ControlPlane(ControlPlaneError),
    DeviceKeys(DeviceKeyError),
    MissingWorkspaceKey(WorkspaceId),
    MissingPendingRequest(String),
    Grant(grants::GrantError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstDeviceTrustRoot {
    pub local_device: DeviceRecord,
    pub recovery_key: RecoveryKeyState,
}

pub fn ensure_first_device_trust_root<C, K>(
    control_plane: &C,
    key_store: &K,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    device_name: impl Into<String>,
    platform: DevicePlatform,
    generated_at: impl Into<String>,
) -> Result<FirstDeviceTrustRoot, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let device_name = device_name.into();
    let generated_at = generated_at.into();
    let identity = key_store.load_or_create_device_identity()?;
    let trust = control_plane.list_device_trust(workspace_id.as_str())?;
    if let Some(existing) = trust
        .authorized_devices
        .iter()
        .find(|device| device.device_id == device_id.as_str())
        .cloned()
    {
        if existing.device_fingerprint != identity.fingerprint.as_str() {
            return Err(ControlPlaneError::Limited {
                capability: "device-trust",
                reason: "local device identity does not match the existing trust root",
            }
            .into());
        }
        if key_store.load_workspace_key(&workspace_id)?.is_none() {
            return Err(TrustError::MissingWorkspaceKey(workspace_id));
        }
        return Ok(FirstDeviceTrustRoot {
            local_device: device_record_from_authorized(existing, workspace_id, true, generated_at),
            recovery_key: RecoveryKeyState::missing(),
        });
    }
    if !trust.authorized_devices.is_empty() {
        return Err(ControlPlaneError::Conflict {
            resource: "first authorized device",
            reason: "workspace already has a trust root",
        }
        .into());
    }
    if !trust.revoked_devices.is_empty()
        || !control_plane
            .list_recovery_envelopes(workspace_id.as_str())?
            .is_empty()
    {
        return Err(ControlPlaneError::Conflict {
            resource: "first authorized device",
            reason: "workspace already has trust history",
        }
        .into());
    }

    let device_authorization_proof_verifier =
        grants::device_authorization_proof_verifier(&identity);
    if key_store.load_workspace_key(&workspace_id)?.is_none() {
        let generated = WorkspaceKeyMaterial::generate(workspace_id.clone(), 1)?;
        key_store.store_workspace_key(generated)?;
    }
    let authorized = control_plane.create_first_authorized_device(FirstAuthorizedDeviceInput {
        workspace_id: workspace_id.as_str().to_string(),
        device_id: device_id.as_str().to_string(),
        device_name: device_name.clone(),
        platform: platform_string(platform).to_string(),
        device_fingerprint: identity.fingerprint.as_str().to_string(),
        device_authorization_proof_verifier,
    })?;

    Ok(FirstDeviceTrustRoot {
        local_device: device_record_from_authorized(authorized, workspace_id, true, generated_at),
        recovery_key: RecoveryKeyState::missing(),
    })
}

fn device_record_from_authorized(
    authorized: AuthorizedDeviceRecord,
    workspace_id: WorkspaceId,
    is_current_device: bool,
    updated_at: String,
) -> DeviceRecord {
    DeviceRecord {
        id: DeviceId::new(authorized.device_id),
        name: authorized.device_name,
        workspace_id,
        platform: platform_from_str(&authorized.platform),
        trust_state: DeviceTrustState::Trusted,
        device_fingerprint: DeviceFingerprint::new(authorized.device_fingerprint),
        authorized_at: Some(authorized.authorized_at.to_string()),
        updated_at,
        is_current_device,
        limitation_reason: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequestOptions {
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub device_name: String,
    pub platform: DevicePlatform,
    pub host: Option<String>,
    pub root: Option<String>,
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
    let matching_code = matching_code(
        options.workspace_id.as_str(),
        options.device_id.as_str(),
        identity.public_key.as_str(),
    );
    let mut input = DeviceRequestInput::new(
        options.workspace_id.as_str(),
        options.device_id.as_str(),
        &options.device_name,
        identity.public_key.as_str(),
        identity.fingerprint.as_str(),
        matching_code,
    );
    input.platform = platform_string(options.platform).to_string();
    input.host = options.host;
    input.root = options.root;
    input.device_authorization_proof_verifier =
        grants::device_authorization_proof_verifier(&identity);

    let request = control_plane.create_device_request(input)?;
    Ok(core_request_from_control_plane(request))
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
    let trust = control_plane.list_device_trust(options.workspace_id.as_str())?;
    let request = trust
        .pending_requests
        .iter()
        .find(|request| request.request_id == options.request_id.as_str())
        .cloned()
        .ok_or_else(|| {
            TrustError::MissingPendingRequest(options.request_id.as_str().to_string())
        })?;
    let finish_command = request
        .root
        .as_ref()
        .map(|root| {
            format!(
                "bowline login --root {} --no-poll --json",
                crate::bootstrap::ssh::shell_quote(root)
            )
        })
        .unwrap_or_else(|| "bowline login --no-poll --json".to_string());
    let workspace_key = key_store
        .load_workspace_key(&options.workspace_id)?
        .ok_or_else(|| TrustError::MissingWorkspaceKey(options.workspace_id.clone()))?;
    let identity = key_store.load_or_create_device_identity()?;
    let approved_by_device_proof = grants::device_authorization_proof(
        &identity,
        &options.workspace_id,
        &options.approver_device_id,
        "approve-device-request",
        options.request_id.as_str(),
    );
    let ciphertext = grants::encrypt_workspace_key_for_request(&workspace_key, &request)
        .map_err(TrustError::Grant)?;
    let requester_device_id = DeviceId::new(request.device_id.clone());
    let grant_acceptance_proof =
        grants::grant_acceptance_proof(&workspace_key, &options.request_id, &requester_device_id);
    let grant_acceptance_proof_verifier =
        grants::grant_acceptance_proof_verifier(&grant_acceptance_proof);
    let approval = control_plane.approve_device_request(DeviceApprovalInput {
        request_id: request.request_id.clone(),
        approved_by_device_id: options.approver_device_id.as_str().to_string(),
        approved_by_device_proof,
        encrypted_grant_ciphertext: ciphertext,
        grant_acceptance_proof_verifier,
        key_epoch: workspace_key.key_epoch,
        expires_in_ticks: 600,
    })?;
    let approved_device = DeviceRecord {
        id: DeviceId::new(approval.device_id.clone()),
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
        next_actions: vec![SafeAction {
            label: format!(
                "{} can finish login on the requesting device",
                approval.device_name
            ),
            command: Some(finish_command),
        }],
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
    let Some(grant) =
        control_plane.get_encrypted_device_grant(request_id.as_str(), device_id.as_str())?
    else {
        return Err(TrustError::MissingPendingRequest(
            request_id.as_str().to_string(),
        ));
    };
    let identity = key_store.load_or_create_device_identity()?;
    let material =
        grants::decrypt_workspace_key_from_grant(&identity, &grant).map_err(TrustError::Grant)?;
    if &material.workspace_id != workspace_id {
        return Err(TrustError::Grant(grants::GrantError::WorkspaceMismatch));
    }
    let grant_acceptance_proof = grants::grant_acceptance_proof(&material, request_id, device_id);
    let accepted = control_plane.confirm_device_grant_accepted(GrantAcceptanceInput {
        request_id: request_id.as_str().to_string(),
        device_id: device_id.as_str().to_string(),
        grant_acceptance_proof,
    })?;
    key_store.store_workspace_key(material)?;
    Ok(EncryptedDeviceGrant {
        grant_id: EncryptedDeviceGrantId::new(accepted.grant_id),
        request_id: request_id.clone(),
        workspace_id: workspace_id.clone(),
        requester_device_id: device_id.clone(),
        requester_device_fingerprint: DeviceFingerprint::new(accepted.device_fingerprint),
        approver_device_id: DeviceId::new(accepted.approved_by_device_id),
        key_epoch: accepted.key_epoch,
        ciphertext: accepted.encrypted_grant_ciphertext,
        created_at: accepted.granted_at.to_string(),
        expires_at: accepted.expires_at.to_string(),
        state: bowline_core::devices::EncryptedDeviceGrantState::Accepted,
        accepted_at: accepted.accepted_at.map(|timestamp| timestamp.to_string()),
    })
}

impl fmt::Display for TrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::DeviceKeys(error) => error.fmt(formatter),
            Self::MissingWorkspaceKey(workspace_id) => {
                write!(
                    formatter,
                    "workspace key for `{}` is not available on this device",
                    workspace_id.as_str()
                )
            }
            Self::MissingPendingRequest(request_id) => {
                write!(formatter, "device request `{request_id}` is not pending")
            }
            Self::Grant(error) => error.fmt(formatter),
        }
    }
}

impl Error for TrustError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::DeviceKeys(error) => Some(error),
            Self::Grant(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ControlPlaneError> for TrustError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<DeviceKeyError> for TrustError {
    fn from(error: DeviceKeyError) -> Self {
        Self::DeviceKeys(error)
    }
}

pub(crate) fn core_request_from_control_plane(
    request: bowline_control_plane::DeviceRequest,
) -> DeviceApprovalRequest {
    DeviceApprovalRequest {
        request_id: DeviceApprovalRequestId::new(request.request_id),
        workspace_id: WorkspaceId::new(request.workspace_id),
        requester_device_id: DeviceId::new(request.device_id),
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
        root: request.root,
    }
}

fn matching_code(workspace_id: &str, device_id: &str, public_key: &str) -> String {
    let hash = blake3::hash(format!("{workspace_id}:{device_id}:{public_key}").as_bytes());
    format!("bowline-{}", &hash.to_hex()[..6])
}

fn platform_string(platform: DevicePlatform) -> &'static str {
    match platform {
        DevicePlatform::Macos => "macos",
        DevicePlatform::Linux => "linux",
        DevicePlatform::Unknown => "unknown",
    }
}

fn platform_from_str(value: &str) -> DevicePlatform {
    match value {
        "macos" | "darwin" => DevicePlatform::Macos,
        "linux" => DevicePlatform::Linux,
        _ => DevicePlatform::Unknown,
    }
}

pub fn devices_output_for_request(
    generated_at: String,
    request: DeviceApprovalRequest,
) -> DevicesCommandOutput {
    let approve_command = format!("bowline approve {}", request.request_id.as_str());
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
        next_actions: vec![SafeAction {
            label: "Approve this device from an already trusted device".to_string(),
            command: Some(approve_command),
        }],
    }
}

#[cfg(test)]
mod tests {
    use bowline_control_plane::{
        ControlPlaneClient, ControlPlaneError, DeterministicClock, DeterministicIdGenerator,
        DeviceApprovalInput, FakeControlPlaneClient,
    };
    use bowline_core::{
        devices::DevicePlatform,
        ids::{DeviceId, WorkspaceId},
    };

    use super::{
        ApproveDeviceOptions, DeviceRequestOptions, TrustError, accept_device_grant,
        approve_device_request, create_device_request, ensure_first_device_trust_root, grants,
    };
    use crate::{
        device_keys::{
            AccountTokens, DeviceIdentity, DeviceKeyError, DeviceKeyStore, SecretUnavailableReason,
            WorkspaceKeyMaterial,
        },
        fakes::FakeKeychain,
    };

    #[derive(Debug, Default)]
    struct FailingWorkspaceKeyStore {
        inner: FakeKeychain,
    }

    impl DeviceKeyStore for FailingWorkspaceKeyStore {
        fn load_or_create_device_identity(&self) -> Result<DeviceIdentity, DeviceKeyError> {
            self.inner.load_or_create_device_identity()
        }

        fn store_account_tokens(&self, tokens: AccountTokens) -> Result<(), DeviceKeyError> {
            self.inner.store_account_tokens(tokens)
        }

        fn load_account_tokens(&self) -> Result<Option<AccountTokens>, DeviceKeyError> {
            self.inner.load_account_tokens()
        }

        fn store_workspace_key(&self, _key: WorkspaceKeyMaterial) -> Result<(), DeviceKeyError> {
            Err(DeviceKeyError::Unavailable(
                "test workspace key write failed".to_string(),
            ))
        }

        fn load_workspace_key(
            &self,
            workspace_id: &WorkspaceId,
        ) -> Result<Option<WorkspaceKeyMaterial>, DeviceKeyError> {
            self.inner.load_workspace_key(workspace_id)
        }

        fn mark_secret_unavailable(
            &self,
            reason: SecretUnavailableReason,
        ) -> Result<(), DeviceKeyError> {
            self.inner.mark_secret_unavailable(reason)
        }
    }

    #[test]
    fn rejected_first_device_does_not_store_generated_workspace_key() {
        let control_plane = FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("first-device-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-first-device");
        control_plane.create_workspace(workspace_id.as_str());
        let first_keychain = FakeKeychain::default();
        ensure_first_device_trust_root(
            &control_plane,
            &first_keychain,
            workspace_id.clone(),
            DeviceId::new("device-1"),
            "Trusted Mac",
            DevicePlatform::Macos,
            "t000000000001",
        )
        .expect("first device");

        let rejected_keychain = FakeKeychain::default();
        let error = ensure_first_device_trust_root(
            &control_plane,
            &rejected_keychain,
            workspace_id.clone(),
            DeviceId::new("device-2"),
            "Second Mac",
            DevicePlatform::Macos,
            "t000000000002",
        )
        .expect_err("second first-device init is rejected");

        assert!(matches!(error, TrustError::ControlPlane(_)));
        assert!(
            rejected_keychain
                .load_workspace_key(&workspace_id)
                .expect("keychain readable")
                .is_none()
        );
    }

    #[test]
    fn first_device_key_store_failure_does_not_publish_remote_trust_root() {
        let control_plane = FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("first-device-store-failure-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-first-device-store-failure");
        control_plane.create_workspace(workspace_id.as_str());
        let failing_keychain = FailingWorkspaceKeyStore::default();

        let error = ensure_first_device_trust_root(
            &control_plane,
            &failing_keychain,
            workspace_id.clone(),
            DeviceId::new("device-1"),
            "Trusted Mac",
            DevicePlatform::Macos,
            "t000000000001",
        )
        .expect_err("workspace key persistence failure rejects first-device setup");

        assert!(matches!(
            error,
            TrustError::DeviceKeys(DeviceKeyError::Unavailable(_))
        ));
        let trust = control_plane
            .list_device_trust(workspace_id.as_str())
            .expect("trust list");
        assert!(trust.authorized_devices.is_empty());
    }

    #[test]
    fn idempotent_first_device_retry_requires_existing_workspace_key() {
        let control_plane = FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("first-device-idempotent-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-first-device-idempotent");
        control_plane.create_workspace(workspace_id.as_str());
        let keychain = FakeKeychain::default();
        ensure_first_device_trust_root(
            &control_plane,
            &keychain,
            workspace_id.clone(),
            DeviceId::new("device-1"),
            "Trusted Mac",
            DevicePlatform::Macos,
            "t000000000001",
        )
        .expect("first device");
        let original_key = keychain
            .load_workspace_key(&workspace_id)
            .expect("keychain readable")
            .expect("workspace key exists");
        keychain.delete_secret(&format!("workspace-key-v1:{}", workspace_id.as_str()));

        let error = ensure_first_device_trust_root(
            &control_plane,
            &keychain,
            workspace_id.clone(),
            DeviceId::new("device-1"),
            "Trusted Mac",
            DevicePlatform::Macos,
            "t000000000002",
        )
        .expect_err("retry without local key must not mint a replacement key");

        assert!(matches!(error, TrustError::MissingWorkspaceKey(_)));
        assert!(
            keychain
                .load_workspace_key(&workspace_id)
                .expect("keychain readable")
                .is_none()
        );
        let trust = control_plane
            .list_device_trust(workspace_id.as_str())
            .expect("trust list");
        assert_eq!(trust.authorized_devices.len(), 1);
        assert_eq!(original_key.workspace_id, workspace_id);
    }

    #[test]
    fn approve_output_points_requester_at_login_finish_command() {
        let control_plane = FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("approve-finish-action-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-approve-finish-action");
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
        let requester_keychain = FakeKeychain::default();
        let request = create_device_request(
            &control_plane,
            &requester_keychain,
            DeviceRequestOptions {
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("fresh-linux"),
                device_name: "Fresh Linux".to_string(),
                platform: DevicePlatform::Linux,
                host: None,
                root: Some("~/Code Projects".to_string()),
                generated_at: "t000000000002".to_string(),
            },
        )
        .expect("fresh device request");

        let output = approve_device_request(
            &control_plane,
            &trusted_keychain,
            ApproveDeviceOptions {
                workspace_id,
                request_id: request.request_id,
                approver_device_id: DeviceId::new("trusted-device"),
                generated_at: "t000000000003".to_string(),
            },
        )
        .expect("approve device request");

        assert_eq!(
            output
                .next_actions
                .first()
                .and_then(|action| action.command.as_deref()),
            Some("bowline login --root '~/Code Projects' --no-poll --json")
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
                generated_at: "t000000000002".to_string(),
            },
        )
        .expect("fresh device request");
        let pending_request = control_plane
            .list_device_trust(workspace_id.as_str())
            .expect("trust list")
            .pending_requests
            .into_iter()
            .find(|pending| pending.request_id == request.request_id.as_str())
            .expect("pending request");
        let ciphertext =
            grants::encrypt_workspace_key_for_request(&workspace_key, &pending_request)
                .expect("grant ciphertext");
        control_plane
            .approve_device_request_for_harness(DeviceApprovalInput {
                request_id: request.request_id.as_str().to_string(),
                approved_by_device_id: "trusted-device".to_string(),
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
            TrustError::ControlPlane(ControlPlaneError::Limited {
                capability: "device-grant",
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
            .list_device_trust(workspace_id.as_str())
            .expect("trust list");
        assert!(
            !trust
                .authorized_devices
                .iter()
                .any(|device| device.device_id == requester_device_id.as_str())
        );
    }
}
