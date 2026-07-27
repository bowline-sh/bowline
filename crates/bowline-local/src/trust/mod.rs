use std::{error::Error, fmt};

use bowline_control_plane::{
    AuthorizedDeviceRecord, ControlPlaneClient, ControlPlaneError, FirstAuthorizedDeviceInput,
};
use bowline_core::{
    devices::{
        DeviceFingerprint, DevicePlatform, DeviceRecord, DeviceTrustState, RecoveryKeyState,
    },
    ids::{DeviceId, WorkspaceId},
};

use crate::device_keys::{
    DeviceKeyError, DeviceKeyStore, DeviceProofVerifier, WorkspaceKeyMaterial, WorkspaceKeyring,
};

mod device_approval;
pub mod device_proofs;
pub mod grants;
pub mod key_regrants;
pub mod recovery;
pub mod recovery_envelopes;

pub use device_approval::{
    ApproveDeviceOptions, DeviceRequestOptions, accept_device_grant, approve_device_request,
    create_device_request, devices_output_for_request,
};
pub use key_regrants::{
    KeyEpochConvergence, converge_workspace_key_epoch, device_public_key_attestation,
};

#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod key_regrant_tests;

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
    let trust = control_plane.list_device_trust(&workspace_id)?;
    if let Some(existing) = trust
        .authorized_devices
        .iter()
        .find(|device| device.device_id == DeviceId::new(device_id.as_str()))
        .cloned()
    {
        if existing.device_fingerprint != identity.fingerprint.as_str() {
            return Err(ControlPlaneError::Rejected {
                code: bowline_control_plane::RejectionCode::DeviceNotTrusted,
                message: "local device identity does not match the existing trust root".to_string(),
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
            .list_recovery_envelopes(&workspace_id)?
            .is_empty()
    {
        return Err(ControlPlaneError::Conflict {
            resource: "first authorized device",
            reason: "workspace already has trust history",
        }
        .into());
    }

    let device_authorization_proof_verifier =
        grants::device_authorization_proof_verifier(&identity).map_err(TrustError::Grant)?;
    if key_store.load_workspace_key(&workspace_id)?.is_none() {
        let generated = WorkspaceKeyMaterial::generate(workspace_id.clone(), 1)?;
        key_store.store_workspace_keyring(WorkspaceKeyring::from_material(generated))?;
    }
    let authorized = control_plane.create_first_authorized_device(FirstAuthorizedDeviceInput {
        workspace_id: workspace_id.clone(),
        device_id: device_id.clone(),
        device_name: device_name.clone(),
        platform: platform_string(platform).to_string(),
        device_fingerprint: identity.fingerprint.as_str().to_string(),
        device_public_key: identity.public_key.as_str().to_string(),
        device_public_key_proof: key_regrants::device_public_key_attestation(
            &identity,
            &workspace_id,
            &device_id,
        )?,
        device_authorization_proof_verifier,
    })?;
    cache_device_proof_verifier(
        key_store,
        workspace_id.clone(),
        device_id.clone(),
        grants::device_authorization_proof_verifier(&identity).map_err(TrustError::Grant)?,
    )?;

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
        id: authorized.device_id,
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
fn cache_device_proof_verifier<K>(
    key_store: &K,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    proof_verifier: String,
) -> Result<(), TrustError>
where
    K: DeviceKeyStore + ?Sized,
{
    key_store.store_device_proof_verifier(DeviceProofVerifier {
        workspace_id,
        device_id,
        proof_verifier,
    })?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use bowline_control_plane::{
        DeterministicClock, DeterministicIdGenerator, DeviceControlPlaneClient,
        FakeControlPlaneClient,
    };
    use bowline_core::{
        devices::DevicePlatform,
        ids::{DeviceId, WorkspaceId},
    };

    use super::{TrustError, ensure_first_device_trust_root};
    use crate::{
        device_keys::{
            AccountTokens, DeviceIdentity, DeviceKeyError, DeviceKeyStore, DeviceProofVerifier,
            WorkspaceKeyring,
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

        fn clear_account_tokens(&self) -> Result<bool, DeviceKeyError> {
            self.inner.clear_account_tokens()
        }

        fn store_workspace_keyring(
            &self,
            _keyring: WorkspaceKeyring,
        ) -> Result<(), DeviceKeyError> {
            Err(DeviceKeyError::Unavailable(
                "test workspace key write failed".to_string(),
            ))
        }

        fn load_workspace_keyring(
            &self,
            workspace_id: &WorkspaceId,
        ) -> Result<Option<WorkspaceKeyring>, DeviceKeyError> {
            self.inner.load_workspace_keyring(workspace_id)
        }

        fn store_device_proof_verifier(
            &self,
            verifier: DeviceProofVerifier,
        ) -> Result<(), DeviceKeyError> {
            self.inner.store_device_proof_verifier(verifier)
        }

        fn load_device_proof_verifiers(&self) -> Result<Vec<DeviceProofVerifier>, DeviceKeyError> {
            self.inner.load_device_proof_verifiers()
        }

        fn replace_device_proof_verifiers_for_workspace(
            &self,
            workspace_id: &WorkspaceId,
            verifiers: Vec<DeviceProofVerifier>,
        ) -> Result<(), DeviceKeyError> {
            self.inner
                .replace_device_proof_verifiers_for_workspace(workspace_id, verifiers)
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
            .list_device_trust(&workspace_id)
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
        keychain.delete_secret(&crate::device_keys::workspace_keyring_secret_name(
            &workspace_id,
        ));

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
            .list_device_trust(&workspace_id)
            .expect("trust list");
        assert_eq!(trust.authorized_devices.len(), 1);
        assert_eq!(original_key.workspace_id, workspace_id);
    }
}
