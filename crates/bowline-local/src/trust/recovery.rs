use std::{error::Error, fmt};

use bip39::{Language, Mnemonic};
use bowline_control_plane::{
    ControlPlaneClient, GrantAcceptanceInput, RecoveryDeviceAuthorizationInput,
    RecoveryEnvelopeInput, RecoveryEnvelopeRecord, RecoveryEnvelopeState,
};
use bowline_core::{
    commands::{CONTRACT_VERSION, RecoveryCommandAction, RecoveryCommandOutput},
    devices::{
        EncryptedDeviceGrant, EncryptedDeviceGrantState, RecoveryKeyLifecycle, RecoveryKeyState,
    },
    ids::{
        DeviceApprovalRequestId, DeviceId, EncryptedDeviceGrantId, RecoveryEnvelopeId, WorkspaceId,
    },
    status::RepairCommand,
};

use crate::{
    device_keys::{DeviceKeyError, DeviceKeyStore, DeviceProofVerifier},
    trust::{self, DeviceRequestOptions, TrustError, grants},
};

#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedRecoveryKey {
    pub words: String,
    pub fingerprint: String,
}

impl fmt::Debug for GeneratedRecoveryKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneratedRecoveryKey")
            .field("words", &grants::redacted_words_debug(&self.words))
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

#[derive(Debug)]
pub enum RecoveryError {
    DeviceKeys(DeviceKeyError),
    Trust(TrustError),
    Bip39(String),
    Grant(grants::GrantError),
    MissingWorkspaceKey(WorkspaceId),
    MissingRecoveryEnvelope(RecoveryEnvelopeId),
    InvalidWords,
}

pub fn generate_recovery_key() -> Result<GeneratedRecoveryKey, RecoveryError> {
    let mnemonic = Mnemonic::generate_in(Language::English, 24)
        .map_err(|error| RecoveryError::Bip39(error.to_string()))?;
    let words = mnemonic.to_string();
    Ok(GeneratedRecoveryKey {
        fingerprint: grants::recovery_fingerprint(&words),
        words,
    })
}

pub fn create_recovery_key<C, K>(
    control_plane: &C,
    key_store: &K,
    workspace_id: WorkspaceId,
    created_by_device_id: DeviceId,
    generated_at: String,
) -> Result<(GeneratedRecoveryKey, RecoveryCommandOutput), RecoveryError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let recovery_key = generate_recovery_key()?;
    let workspace_key = key_store
        .load_workspace_key(&workspace_id)?
        .ok_or_else(|| RecoveryError::MissingWorkspaceKey(workspace_id.clone()))?;
    let identity = key_store.load_or_create_device_identity()?;
    let authorizer = recovery_device_authorizer(&identity, created_by_device_id.clone())?;
    let ciphertext =
        grants::encrypted_recovery_envelope(&workspace_key, &recovery_key.words, vec![authorizer])?;
    let envelope_id = RecoveryEnvelopeId::new(format!("rk_{}", recovery_key.fingerprint));
    let recovery_proof_verifier =
        grants::recovery_proof_verifier(&recovery_key.words, &workspace_id, envelope_id.as_str());
    let proof_subject = grants::recovery_envelope_payload_proof_subject(
        envelope_id.as_str(),
        &recovery_key.fingerprint,
        &recovery_proof_verifier,
        &ciphertext,
    );
    let created_by_device_proof = grants::device_authorization_proof(
        &identity,
        &workspace_id,
        &created_by_device_id,
        "create-recovery-envelope",
        &proof_subject,
    )?;
    let envelope = control_plane.create_recovery_envelope(RecoveryEnvelopeInput {
        workspace_id: workspace_id.clone(),
        envelope_id: envelope_id.clone(),
        created_by_device_id: created_by_device_id.clone(),
        created_by_device_proof,
        ciphertext,
        fingerprint: recovery_key.fingerprint.clone(),
        recovery_proof_verifier,
    })?;
    let state = RecoveryKeyState {
        lifecycle: RecoveryKeyLifecycle::GeneratedUnverified,
        envelope_id: Some(RecoveryEnvelopeId::new(envelope.envelope_id)),
        fingerprint: Some(recovery_key.fingerprint.clone()),
        created_at: Some(generated_at.clone()),
        verified_at: None,
        rotated_at: None,
        revoked_at: None,
    };

    Ok((
        recovery_key,
        RecoveryCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::Recover,
            generated_at,
            action: RecoveryCommandAction::Create,
            workspace_id: Some(workspace_id),
            recovery_key: state,
            device_request: None,
            encrypted_grant: None,
            next_actions: vec![RepairCommand::mutating(
                "Verify the generated Recovery Key".to_string(),
                Some(format!(
                    "printf '%s\\n' '<recovery-words>' | bowline recover verify {}",
                    envelope_id.as_str()
                )),
            )],
        },
    ))
}

pub fn verify_recovery_key<C, K>(
    control_plane: &C,
    key_store: &K,
    workspace_id: WorkspaceId,
    envelope_id: RecoveryEnvelopeId,
    verified_by_device_id: DeviceId,
    words: &str,
    generated_at: String,
) -> Result<RecoveryCommandOutput, RecoveryError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    Mnemonic::parse_in_normalized(Language::English, words)
        .map_err(|_| RecoveryError::InvalidWords)?;
    let envelope = control_plane
        .list_recovery_envelopes(&workspace_id)?
        .into_iter()
        .find(|envelope| envelope.envelope_id == envelope_id.as_str())
        .ok_or_else(|| RecoveryError::MissingRecoveryEnvelope(envelope_id.clone()))?;
    if !matches!(
        envelope.state,
        RecoveryEnvelopeState::GeneratedUnverified | RecoveryEnvelopeState::Active
    ) {
        return Err(RecoveryError::MissingRecoveryEnvelope(envelope_id));
    }
    let expected_fingerprint = grants::recovery_fingerprint(words);
    if envelope.fingerprint != expected_fingerprint {
        return Err(RecoveryError::InvalidWords);
    }
    let recovery_payload = grants::decrypt_recovery_envelope(&envelope.ciphertext, words)
        .map_err(|_| RecoveryError::InvalidWords)?;
    if recovery_payload.workspace_key.workspace_id != workspace_id {
        return Err(RecoveryError::Grant(grants::GrantError::WorkspaceMismatch));
    }
    let identity = key_store.load_or_create_device_identity()?;
    let verified_by_device_proof = grants::device_authorization_proof(
        &identity,
        &workspace_id,
        &verified_by_device_id,
        "verify-recovery-envelope",
        &grants::recovery_envelope_proof_subject(envelope_id.as_str()),
    )?;
    let envelope = control_plane.verify_recovery_envelope(
        &workspace_id,
        &envelope_id,
        &verified_by_device_id,
        &verified_by_device_proof,
        &grants::recovery_proof(words, &workspace_id, envelope_id.as_str()),
    )?;
    Ok(RecoveryCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Recover,
        generated_at: generated_at.clone(),
        action: RecoveryCommandAction::Verify,
        workspace_id: Some(workspace_id),
        recovery_key: RecoveryKeyState {
            lifecycle: RecoveryKeyLifecycle::Active,
            envelope_id: Some(envelope_id),
            fingerprint: Some(envelope.fingerprint),
            created_at: Some(envelope.created_at.to_string()),
            verified_at: Some(generated_at),
            rotated_at: None,
            revoked_at: None,
        },
        device_request: None,
        encrypted_grant: None,
        next_actions: Vec::new(),
    })
}

pub fn rotate_recovery_key<C, K>(
    control_plane: &C,
    key_store: &K,
    workspace_id: WorkspaceId,
    rotated_by_device_id: DeviceId,
    generated_at: String,
) -> Result<(GeneratedRecoveryKey, RecoveryCommandOutput), RecoveryError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let recovery_key = generate_recovery_key()?;
    let workspace_key = key_store
        .load_workspace_key(&workspace_id)?
        .ok_or_else(|| RecoveryError::MissingWorkspaceKey(workspace_id.clone()))?;
    let identity = key_store.load_or_create_device_identity()?;
    let authorizer = recovery_device_authorizer(&identity, rotated_by_device_id.clone())?;
    let ciphertext =
        grants::encrypted_recovery_envelope(&workspace_key, &recovery_key.words, vec![authorizer])?;
    let envelope_id = RecoveryEnvelopeId::new(format!("rk_{}", recovery_key.fingerprint));
    let recovery_proof_verifier =
        grants::recovery_proof_verifier(&recovery_key.words, &workspace_id, envelope_id.as_str());
    let proof_subject = grants::recovery_envelope_payload_proof_subject(
        envelope_id.as_str(),
        &recovery_key.fingerprint,
        &recovery_proof_verifier,
        &ciphertext,
    );
    let created_by_device_proof = grants::device_authorization_proof(
        &identity,
        &workspace_id,
        &rotated_by_device_id,
        "rotate-recovery-envelope",
        &proof_subject,
    )?;
    let envelope = control_plane.rotate_recovery_envelope(RecoveryEnvelopeInput {
        workspace_id: workspace_id.clone(),
        envelope_id: envelope_id.clone(),
        created_by_device_id: rotated_by_device_id.clone(),
        created_by_device_proof,
        ciphertext,
        fingerprint: recovery_key.fingerprint.clone(),
        recovery_proof_verifier,
    })?;

    Ok((
        recovery_key,
        RecoveryCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::Recover,
            generated_at,
            action: RecoveryCommandAction::Rotate,
            workspace_id: Some(workspace_id),
            recovery_key: recovery_state_from_envelope(
                &envelope,
                RecoveryKeyLifecycle::GeneratedUnverified,
            ),
            device_request: None,
            encrypted_grant: None,
            next_actions: vec![RepairCommand::mutating(
                "Verify the rotated Recovery Key".to_string(),
                Some(format!(
                    "printf '%s\\n' '<recovery-words>' | bowline recover verify {}",
                    envelope.envelope_id.as_str()
                )),
            )],
        },
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseRecoveryKeyOptions {
    pub workspace_id: WorkspaceId,
    pub envelope_id: RecoveryEnvelopeId,
    pub words: String,
    pub device_id: DeviceId,
    pub device_name: String,
    pub platform: bowline_core::devices::DevicePlatform,
    pub generated_at: String,
}

pub fn use_recovery_key<C, K>(
    control_plane: &C,
    key_store: &K,
    options: UseRecoveryKeyOptions,
) -> Result<RecoveryCommandOutput, RecoveryError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    Mnemonic::parse_in_normalized(Language::English, &options.words)
        .map_err(|_| RecoveryError::InvalidWords)?;
    let envelope = control_plane
        .list_recovery_envelopes(&options.workspace_id)?
        .into_iter()
        .find(|envelope| envelope.envelope_id == options.envelope_id.as_str())
        .ok_or_else(|| RecoveryError::MissingRecoveryEnvelope(options.envelope_id.clone()))?;
    if envelope.state != RecoveryEnvelopeState::Active {
        return Err(RecoveryError::MissingRecoveryEnvelope(options.envelope_id));
    }
    let expected_fingerprint = grants::recovery_fingerprint(&options.words);
    if envelope.fingerprint != expected_fingerprint {
        return Err(RecoveryError::InvalidWords);
    }
    let recovery_payload = grants::decrypt_recovery_envelope(&envelope.ciphertext, &options.words)?;
    if recovery_payload.workspace_key.workspace_id != options.workspace_id {
        return Err(RecoveryError::Grant(grants::GrantError::WorkspaceMismatch));
    }
    let workspace_key = recovery_payload.workspace_key;
    let recovery_device_proof_verifiers = recovery_payload.device_proof_verifiers;

    let request = trust::create_device_request(
        control_plane,
        key_store,
        DeviceRequestOptions {
            workspace_id: options.workspace_id.clone(),
            device_id: options.device_id.clone(),
            device_name: options.device_name,
            platform: options.platform,
            host: None,
            root: Some("~/Code".to_string()),
            runtime: None,
            generated_at: options.generated_at.clone(),
        },
    )?;
    let request_id = request.request_id.clone();
    let control_plane_request = control_plane
        .list_device_trust(&options.workspace_id)?
        .pending_requests
        .into_iter()
        .find(|pending| pending.request_id == request_id.as_str())
        .ok_or_else(|| TrustError::MissingPendingRequest(request_id.as_str().to_string()))?;
    let grant_authorizer = recovery_device_proof_verifiers.first().cloned();
    let ciphertext = grants::encrypt_workspace_key_for_request(
        &workspace_key,
        &control_plane_request,
        grant_authorizer,
    )?;
    let grant_acceptance_proof =
        grants::grant_acceptance_proof(&workspace_key, &request_id, &options.device_id);
    let grant_acceptance_proof_verifier =
        grants::grant_acceptance_proof_verifier(&grant_acceptance_proof);
    // Persist the workspace key before server-side authorization. A local write
    // failure after authorize would leave the device trusted without material,
    // and authorized devices cannot request trust again.
    key_store.store_workspace_key(workspace_key.clone())?;
    cache_recovery_device_proof_verifiers(
        key_store,
        &options.workspace_id,
        recovery_device_proof_verifiers,
    )?;
    let grant = control_plane.authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
        workspace_id: options.workspace_id.clone(),
        envelope_id: envelope.envelope_id.clone(),
        request_id: request_id.clone(),
        encrypted_grant_ciphertext: ciphertext,
        grant_acceptance_proof_verifier,
        key_epoch: workspace_key.key_epoch,
        recovery_proof: grants::recovery_proof(
            &options.words,
            &options.workspace_id,
            envelope.envelope_id.as_str(),
        ),
        expires_in_ticks: 600,
    })?;
    let grant_acceptance_proof =
        grants::grant_acceptance_proof(&workspace_key, &request_id, &options.device_id);
    let accepted = control_plane.confirm_device_grant_accepted(GrantAcceptanceInput {
        request_id: request_id.clone(),
        device_id: options.device_id.clone(),
        grant_acceptance_proof,
    })?;

    Ok(RecoveryCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Recover,
        generated_at: options.generated_at,
        action: RecoveryCommandAction::Use,
        workspace_id: Some(options.workspace_id.clone()),
        recovery_key: recovery_state_from_envelope(&envelope, RecoveryKeyLifecycle::Active),
        device_request: Some(request),
        encrypted_grant: Some(EncryptedDeviceGrant {
            grant_id: EncryptedDeviceGrantId::new(accepted.grant_id),
            request_id: DeviceApprovalRequestId::new(grant.request_id),
            workspace_id: options.workspace_id,
            requester_device_id: DeviceId::new(accepted.device_id),
            requester_device_fingerprint: bowline_core::devices::DeviceFingerprint::new(
                accepted.device_fingerprint,
            ),
            approver_device_id: DeviceId::new(accepted.approved_by_device_id),
            key_epoch: accepted.key_epoch,
            ciphertext: accepted.encrypted_grant_ciphertext,
            created_at: accepted.granted_at.to_string(),
            expires_at: accepted.expires_at.to_string(),
            state: EncryptedDeviceGrantState::Accepted,
            accepted_at: accepted.accepted_at.map(|timestamp| timestamp.to_string()),
        }),
        next_actions: Vec::new(),
    })
}

fn recovery_device_authorizer(
    identity: &crate::device_keys::DeviceIdentity,
    device_id: DeviceId,
) -> Result<grants::DeviceGrantAuthorizer, RecoveryError> {
    Ok(grants::DeviceGrantAuthorizer {
        device_id,
        device_authorization_proof_verifier: grants::device_authorization_proof_verifier(identity)?,
    })
}

fn cache_recovery_device_proof_verifiers<K>(
    key_store: &K,
    workspace_id: &WorkspaceId,
    verifiers: Vec<grants::DeviceGrantAuthorizer>,
) -> Result<(), RecoveryError>
where
    K: DeviceKeyStore + ?Sized,
{
    for verifier in verifiers {
        key_store.store_device_proof_verifier(DeviceProofVerifier {
            workspace_id: workspace_id.clone(),
            device_id: verifier.device_id,
            proof_verifier: verifier.device_authorization_proof_verifier,
        })?;
    }
    Ok(())
}

fn recovery_state_from_envelope(
    envelope: &RecoveryEnvelopeRecord,
    lifecycle: RecoveryKeyLifecycle,
) -> RecoveryKeyState {
    RecoveryKeyState {
        lifecycle,
        envelope_id: Some(RecoveryEnvelopeId::new(envelope.envelope_id.clone())),
        fingerprint: Some(envelope.fingerprint.clone()),
        created_at: Some(envelope.created_at.to_string()),
        verified_at: envelope.verified_at.map(|timestamp| timestamp.to_string()),
        rotated_at: envelope.rotated_at.map(|timestamp| timestamp.to_string()),
        revoked_at: envelope.revoked_at.map(|timestamp| timestamp.to_string()),
    }
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceKeys(error) => error.fmt(formatter),
            Self::Trust(error) => error.fmt(formatter),
            Self::Bip39(error) => write!(formatter, "Recovery Key generation failed: {error}"),
            Self::Grant(error) => error.fmt(formatter),
            Self::MissingWorkspaceKey(workspace_id) => write!(
                formatter,
                "workspace key for `{}` is not available on this device",
                workspace_id.as_str()
            ),
            Self::MissingRecoveryEnvelope(envelope_id) => write!(
                formatter,
                "Recovery Key envelope `{}` is not active or unavailable",
                envelope_id.as_str()
            ),
            Self::InvalidWords => write!(
                formatter,
                "Recovery Key words did not verify this recovery envelope"
            ),
        }
    }
}

impl Error for RecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DeviceKeys(error) => Some(error),
            Self::Trust(error) => Some(error),
            Self::Grant(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DeviceKeyError> for RecoveryError {
    fn from(error: DeviceKeyError) -> Self {
        Self::DeviceKeys(error)
    }
}

impl From<TrustError> for RecoveryError {
    fn from(error: TrustError) -> Self {
        Self::Trust(error)
    }
}

impl From<grants::GrantError> for RecoveryError {
    fn from(error: grants::GrantError) -> Self {
        Self::Grant(error)
    }
}

impl From<bowline_control_plane::ControlPlaneError> for RecoveryError {
    fn from(error: bowline_control_plane::ControlPlaneError) -> Self {
        Self::Trust(TrustError::ControlPlane(error))
    }
}

#[cfg(test)]
mod tests {
    use bowline_control_plane::{
        DeterministicClock, DeterministicIdGenerator, DeviceControlPlaneClient,
        RecoveryControlPlaneClient, RecoveryEnvelopeState,
    };
    use bowline_core::{
        commands::RecoveryCommandAction,
        devices::{DevicePlatform, EncryptedDeviceGrantState},
        ids::{DeviceId, WorkspaceId},
    };

    use super::{
        RecoveryError, UseRecoveryKeyOptions, create_recovery_key, generate_recovery_key,
        use_recovery_key, verify_recovery_key,
    };
    use crate::{
        device_keys::DeviceKeyStore,
        fakes::FakeKeychain,
        trust::{ensure_first_device_trust_root, grants},
    };

    #[test]
    fn generated_recovery_key_has_twenty_four_words_and_redacted_debug() {
        let key = generate_recovery_key().expect("generated key");

        assert_eq!(key.words.split_whitespace().count(), 24);
        assert!(!format!("{key:?}").contains(&key.words));
        assert!(format!("{key:?}").contains("24 recovery words redacted"));
    }

    #[test]
    fn wrong_valid_recovery_words_do_not_verify_envelope() {
        let control_plane = bowline_control_plane::FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("recovery-verify-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-recovery-verify");
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
        let (_recovery_key, created) = create_recovery_key(
            &control_plane,
            &trusted_keychain,
            workspace_id.clone(),
            DeviceId::new("trusted-device"),
            "t000000000002".to_string(),
        )
        .expect("created recovery key");
        let envelope_id = created
            .recovery_key
            .envelope_id
            .clone()
            .expect("envelope id");
        let wrong_key = generate_recovery_key().expect("wrong key");

        let error = verify_recovery_key(
            &control_plane,
            &trusted_keychain,
            workspace_id.clone(),
            envelope_id.clone(),
            DeviceId::new("trusted-device"),
            &wrong_key.words,
            "t000000000003".to_string(),
        )
        .expect_err("wrong words are rejected");

        assert!(matches!(error, RecoveryError::InvalidWords));
        let envelopes = control_plane
            .list_recovery_envelopes(&workspace_id)
            .expect("envelopes");
        let envelope = envelopes
            .iter()
            .find(|envelope| envelope.envelope_id == envelope_id.as_str())
            .expect("created envelope");
        assert_eq!(envelope.state, RecoveryEnvelopeState::GeneratedUnverified);
    }

    #[test]
    fn recovery_key_authorizes_fresh_device_without_server_plaintext() {
        let control_plane = bowline_control_plane::FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new("recovery-test"),
        );
        let workspace_id = WorkspaceId::new("workspace-recovery");
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
        let trusted_verifier = grants::device_authorization_proof_verifier(
            &trusted_keychain
                .load_or_create_device_identity()
                .expect("trusted identity"),
        )
        .expect("trusted verifier");
        let (recovery_key, created) = create_recovery_key(
            &control_plane,
            &trusted_keychain,
            workspace_id.clone(),
            DeviceId::new("trusted-device"),
            "t000000000002".to_string(),
        )
        .expect("created recovery key");
        let envelope_id = created
            .recovery_key
            .envelope_id
            .clone()
            .expect("envelope id");
        verify_recovery_key(
            &control_plane,
            &trusted_keychain,
            workspace_id.clone(),
            envelope_id.clone(),
            DeviceId::new("trusted-device"),
            &recovery_key.words,
            "t000000000003".to_string(),
        )
        .expect("verified recovery key");

        let fresh_keychain = FakeKeychain::default();
        let recovered = use_recovery_key(
            &control_plane,
            &fresh_keychain,
            UseRecoveryKeyOptions {
                workspace_id: workspace_id.clone(),
                envelope_id,
                words: recovery_key.words.clone(),
                device_id: DeviceId::new("fresh-linux"),
                device_name: "Fresh Linux".to_string(),
                platform: DevicePlatform::Linux,
                generated_at: "t000000000004".to_string(),
            },
        )
        .expect("used recovery key");

        assert_eq!(recovered.action, RecoveryCommandAction::Use);
        assert_eq!(
            recovered.encrypted_grant.as_ref().expect("grant").state,
            EncryptedDeviceGrantState::Accepted
        );
        assert!(
            fresh_keychain
                .load_workspace_key(&workspace_id)
                .expect("fresh keychain readable")
                .is_some()
        );
        assert!(
            fresh_keychain
                .load_device_proof_verifiers()
                .expect("fresh verifier cache readable")
                .iter()
                .any(|verifier| {
                    verifier.workspace_id == workspace_id
                        && verifier.device_id.as_str() == "trusted-device"
                        && verifier.proof_verifier == trusted_verifier
                })
        );
        let trust = control_plane
            .list_device_trust(&workspace_id)
            .expect("trust list");
        assert!(
            trust
                .authorized_devices
                .iter()
                .any(|device| device.device_id == "fresh-linux")
        );
    }
}
