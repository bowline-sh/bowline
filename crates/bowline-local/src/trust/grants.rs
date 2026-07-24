use std::{error::Error, fmt, str::FromStr};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64_URL},
};
use bowline_control_plane::DeviceApproval;
pub use bowline_control_plane::{
    device_authorization_message, device_request_proof_subject, device_revocation_proof_subject,
    recovery_envelope_payload_proof_subject_parts as recovery_envelope_payload_proof_subject,
    recovery_envelope_proof_subject,
};
use bowline_core::ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::Signer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::device_keys::{DeviceIdentity, WorkspaceKeyMaterial};

#[derive(Debug)]
pub enum GrantError {
    Age(String),
    Base64(base64::DecodeError),
    Json(serde_json::Error),
    SigningKeyDerivation,
    WorkspaceMismatch,
    AuthorizerMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantPayload {
    workspace_id: WorkspaceId,
    request_id: DeviceApprovalRequestId,
    requester_device_id: DeviceId,
    requester_device_fingerprint: String,
    authorizing_device: Option<DeviceGrantAuthorizer>,
    key_epoch: u32,
    workspace_key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceGrantAuthorizer {
    pub device_id: DeviceId,
    pub device_authorization_proof_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedWorkspaceGrant {
    pub workspace_key: WorkspaceKeyMaterial,
    pub authorizing_device: Option<DeviceGrantAuthorizer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryEnvelopePayload {
    workspace_key: WorkspaceKeyMaterial,
    device_proof_verifiers: Vec<DeviceGrantAuthorizer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedRecoveryEnvelope {
    pub workspace_key: WorkspaceKeyMaterial,
    pub device_proof_verifiers: Vec<DeviceGrantAuthorizer>,
}

pub fn encrypt_workspace_key_for_request(
    key: &WorkspaceKeyMaterial,
    request: &bowline_control_plane::DeviceRequest,
    authorizing_device: Option<DeviceGrantAuthorizer>,
) -> Result<String, GrantError> {
    let recipient = age::x25519::Recipient::from_str(&request.device_public_key)
        .map_err(|error| GrantError::Age(error.to_string()))?;
    let payload = GrantPayload {
        workspace_id: key.workspace_id.clone(),
        request_id: DeviceApprovalRequestId::new(request.request_id.clone()),
        requester_device_id: DeviceId::new(request.device_id.clone()),
        requester_device_fingerprint: request.device_fingerprint.clone(),
        authorizing_device,
        key_epoch: key.key_epoch,
        workspace_key: key.key_bytes.clone(),
    };
    let plaintext = serde_json::to_vec(&payload)?;
    let ciphertext =
        age::encrypt(&recipient, &plaintext).map_err(|error| GrantError::Age(error.to_string()))?;
    Ok(BASE64.encode(ciphertext))
}

pub fn decrypt_workspace_key_from_grant(
    identity: &DeviceIdentity,
    grant: &DeviceApproval,
) -> Result<DecryptedWorkspaceGrant, GrantError> {
    let ciphertext = BASE64.decode(&grant.encrypted_grant_ciphertext)?;
    let age_identity = identity
        .age_identity()
        .map_err(|error| GrantError::Age(error.to_string()))?;
    let plaintext = age::decrypt(&age_identity, &ciphertext)
        .map_err(|error| GrantError::Age(error.to_string()))?;
    let payload: GrantPayload = serde_json::from_slice(&plaintext)?;
    if payload.request_id.as_str() != grant.request_id
        || payload.requester_device_id.as_str() != grant.device_id
        || payload.requester_device_fingerprint != grant.device_fingerprint
    {
        return Err(GrantError::WorkspaceMismatch);
    }
    Ok(DecryptedWorkspaceGrant {
        workspace_key: WorkspaceKeyMaterial {
            workspace_id: payload.workspace_id,
            key_epoch: payload.key_epoch,
            key_bytes: payload.workspace_key,
        },
        authorizing_device: payload.authorizing_device,
    })
}

pub fn encrypted_recovery_envelope(
    key: &WorkspaceKeyMaterial,
    words: &str,
    device_proof_verifiers: Vec<DeviceGrantAuthorizer>,
) -> Result<String, GrantError> {
    let passphrase = age::secrecy::SecretString::from(words.to_string());
    let recipient = age::scrypt::Recipient::new(passphrase.clone());
    let plaintext = serde_json::to_vec(&RecoveryEnvelopePayload {
        workspace_key: key.clone(),
        device_proof_verifiers,
    })?;
    let ciphertext =
        age::encrypt(&recipient, &plaintext).map_err(|error| GrantError::Age(error.to_string()))?;
    Ok(BASE64.encode(ciphertext))
}

pub fn decrypt_recovery_envelope(
    ciphertext: &str,
    words: &str,
) -> Result<DecryptedRecoveryEnvelope, GrantError> {
    let ciphertext = BASE64.decode(ciphertext)?;
    let passphrase = age::secrecy::SecretString::from(words.to_string());
    let identity = age::scrypt::Identity::new(passphrase);
    let plaintext =
        age::decrypt(&identity, &ciphertext).map_err(|error| GrantError::Age(error.to_string()))?;
    let payload: RecoveryEnvelopePayload = serde_json::from_slice(&plaintext)?;
    Ok(DecryptedRecoveryEnvelope {
        workspace_key: payload.workspace_key,
        device_proof_verifiers: payload.device_proof_verifiers,
    })
}

pub fn recovery_fingerprint(words: &str) -> String {
    let hash = blake3::hash(words.as_bytes());
    format!("rk_{}", &hash.to_hex()[..16])
}

pub fn recovery_proof_verifier(
    words: &str,
    workspace_id: &WorkspaceId,
    envelope_id: &str,
) -> String {
    recovery_proof_verifier_from_proof(
        &recovery_proof(words, workspace_id, envelope_id),
        workspace_id,
        envelope_id,
    )
}

pub fn recovery_proof(words: &str, workspace_id: &WorkspaceId, envelope_id: &str) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof v2",
        workspace_id.as_str(),
        envelope_id,
        words,
    ]);
    format!("rkp_{}", &hash[..32])
}

pub fn recovery_proof_verifier_from_proof(
    proof: &str,
    workspace_id: &WorkspaceId,
    envelope_id: &str,
) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof verifier v2",
        workspace_id.as_str(),
        envelope_id,
        proof,
    ]);
    format!("rkpv_{}", &hash[..32])
}

pub fn device_authorization_proof_verifier(
    identity: &DeviceIdentity,
) -> Result<String, GrantError> {
    let signing_key = device_signing_key(identity)?;
    let verifying_key = VerifyingKey::from(&signing_key);
    let public_key = verifying_key.to_encoded_point(false);
    Ok(format!(
        "dapv_p256_v1_{}",
        BASE64_URL.encode(public_key.as_bytes())
    ))
}

pub fn device_authorization_proof(
    identity: &DeviceIdentity,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    action: &str,
    subject: &str,
) -> Result<String, GrantError> {
    let signing_key = device_signing_key(identity)?;
    let signature: Signature = signing_key.sign(&device_authorization_message(&[
        "bowline device authorization proof v2",
        workspace_id.as_str(),
        device_id.as_str(),
        action,
        subject,
    ]));
    Ok(format!(
        "dapp_p256_v1_{}",
        BASE64_URL.encode(signature.to_bytes())
    ))
}

pub fn grant_acceptance_proof(
    key: &WorkspaceKeyMaterial,
    request_id: &DeviceApprovalRequestId,
    requester_device_id: &DeviceId,
) -> String {
    let key_epoch = key.key_epoch.to_string();
    let hash = sha256_proof_parts(&[
        b"bowline grant acceptance proof v1",
        key.workspace_id.as_str().as_bytes(),
        request_id.as_str().as_bytes(),
        requester_device_id.as_str().as_bytes(),
        key_epoch.as_bytes(),
        key.key_bytes.as_slice(),
    ]);
    format!("gap_{}", &hash[..32])
}

pub fn grant_acceptance_proof_verifier(proof: &str) -> String {
    let hash = sha256_proof_fields(&["bowline grant acceptance proof verifier v1", proof]);
    format!("gapv_{}", &hash[..32])
}

fn sha256_proof_fields(fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn sha256_proof_parts(fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn device_signing_key(identity: &DeviceIdentity) -> Result<SigningKey, GrantError> {
    for counter in 0_u8..=u8::MAX {
        let mut hasher = Sha256::new();
        for field in [
            b"bowline device signing key v2".as_slice(),
            identity.signing_seed(),
            &[counter],
        ] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field);
        }
        let digest = hasher.finalize();
        if let Ok(signing_key) = SigningKey::from_slice(&digest) {
            return Ok(signing_key);
        }
    }
    Err(GrantError::SigningKeyDerivation)
}

impl fmt::Display for GrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Age(error) => write!(formatter, "grant encryption failed: {error}"),
            Self::Base64(error) => write!(formatter, "grant ciphertext is malformed: {error}"),
            Self::Json(error) => write!(formatter, "grant payload is malformed: {error}"),
            Self::SigningKeyDerivation => {
                write!(formatter, "device signing key derivation failed")
            }
            Self::WorkspaceMismatch => {
                write!(formatter, "grant does not match this workspace or device")
            }
            Self::AuthorizerMismatch => {
                write!(
                    formatter,
                    "grant authorizer does not match the approved device"
                )
            }
        }
    }
}

impl Error for GrantError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Base64(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<base64::DecodeError> for GrantError {
    fn from(error: base64::DecodeError) -> Self {
        Self::Base64(error)
    }
}

impl From<serde_json::Error> for GrantError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn redacted_words_debug(words: &str) -> String {
    let count = words.split_whitespace().count();
    format!("[{count} recovery words redacted]")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use bowline_control_plane::{DeviceApproval, DeviceRequest, DeviceRequestState};
    use bowline_core::ids::{
        DeviceApprovalRequestId, DeviceId, EncryptedDeviceGrantId, WorkspaceId,
    };
    use serde::Deserialize;
    use sha2::Digest;

    use super::{
        DeviceGrantAuthorizer, decrypt_workspace_key_from_grant,
        device_authorization_proof_verifier, encrypt_workspace_key_for_request, recovery_proof,
        recovery_proof_verifier, recovery_proof_verifier_from_proof,
    };
    use crate::device_keys::{DeviceIdentity, WorkspaceKeyMaterial};

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureFile {
        message_vectors: Vec<MessageVector>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct MessageVector {
        fields: Vec<String>,
        name: String,
        sha256_hex: String,
    }

    #[test]
    fn correct_device_key_decrypts_grant() {
        let identity = DeviceIdentity::generate();
        let request = DeviceRequest {
            request_id: DeviceApprovalRequestId::new("request-1"),
            workspace_id: WorkspaceId::new("workspace-1"),
            device_id: DeviceId::new("device-2"),
            device_name: "linux".to_string(),
            platform: "linux".to_string(),
            device_public_key: identity.public_key.as_str().to_string(),
            device_fingerprint: identity.fingerprint.as_str().to_string(),
            device_authorization_proof_verifier: device_authorization_proof_verifier(&identity)
                .expect("verifier"),
            matching_code: "bowline-abc123".to_string(),
            account_id: None,
            host: None,
            root: None,
            runtime: None,
            setup_receipts_digest: None,
            requested_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            state: DeviceRequestState::Pending,
        };
        let key = WorkspaceKeyMaterial {
            workspace_id: WorkspaceId::new("workspace-1"),
            key_epoch: 1,
            key_bytes: vec![9; 32],
        };
        let authorizer = DeviceGrantAuthorizer {
            device_id: DeviceId::new("device-1"),
            device_authorization_proof_verifier: "dapv_authorizer".to_string(),
        };
        let ciphertext =
            encrypt_workspace_key_for_request(&key, &request, Some(authorizer.clone()))
                .expect("encrypt");
        let grant = DeviceApproval {
            grant_id: EncryptedDeviceGrantId::new("grant-1"),
            request_id: request.request_id,
            workspace_id: request.workspace_id,
            device_id: request.device_id,
            device_name: request.device_name,
            platform: request.platform,
            device_fingerprint: request.device_fingerprint,
            approved_by_device_id: DeviceId::new("device-1"),
            encrypted_grant_ciphertext: ciphertext,
            key_epoch: 1,
            granted_at: bowline_control_plane::ControlPlaneTimestamp { tick: 3 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 4 },
            accepted_at: None,
            harness_only: false,
        };

        let decrypted = decrypt_workspace_key_from_grant(&identity, &grant).expect("decrypt");

        assert_eq!(decrypted.workspace_key, key);
        assert_eq!(decrypted.authorizing_device, Some(authorizer));
    }

    #[test]
    fn wrong_device_key_fails_to_decrypt_grant() {
        let identity = DeviceIdentity::generate();
        let wrong_identity = DeviceIdentity::generate();
        let request = DeviceRequest {
            request_id: DeviceApprovalRequestId::new("request-1"),
            workspace_id: WorkspaceId::new("workspace-1"),
            device_id: DeviceId::new("device-2"),
            device_name: "linux".to_string(),
            platform: "linux".to_string(),
            device_public_key: identity.public_key.as_str().to_string(),
            device_fingerprint: identity.fingerprint.as_str().to_string(),
            device_authorization_proof_verifier: device_authorization_proof_verifier(&identity)
                .expect("verifier"),
            matching_code: "bowline-abc123".to_string(),
            account_id: None,
            host: None,
            root: None,
            runtime: None,
            setup_receipts_digest: None,
            requested_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            state: DeviceRequestState::Pending,
        };
        let key = WorkspaceKeyMaterial {
            workspace_id: WorkspaceId::new("workspace-1"),
            key_epoch: 1,
            key_bytes: vec![9; 32],
        };
        let ciphertext = encrypt_workspace_key_for_request(&key, &request, None).expect("encrypt");
        let grant = DeviceApproval {
            grant_id: EncryptedDeviceGrantId::new("grant-1"),
            request_id: request.request_id,
            workspace_id: request.workspace_id,
            device_id: request.device_id,
            device_name: request.device_name,
            platform: request.platform,
            device_fingerprint: request.device_fingerprint,
            approved_by_device_id: DeviceId::new("device-1"),
            encrypted_grant_ciphertext: ciphertext,
            key_epoch: 1,
            granted_at: bowline_control_plane::ControlPlaneTimestamp { tick: 3 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 4 },
            accepted_at: None,
            harness_only: false,
        };

        assert!(decrypt_workspace_key_from_grant(&wrong_identity, &grant).is_err());
    }

    #[test]
    fn recovery_verifier_is_not_replayable_as_the_recovery_proof() {
        let workspace_id = WorkspaceId::new("workspace-recovery-proof");
        let proof = recovery_proof("correct horse battery staple", &workspace_id, "rk_1");
        let verifier =
            recovery_proof_verifier("correct horse battery staple", &workspace_id, "rk_1");

        assert_ne!(proof, verifier);
        assert_eq!(
            recovery_proof_verifier_from_proof(&proof, &workspace_id, "rk_1"),
            verifier
        );
        assert_ne!(
            recovery_proof_verifier_from_proof(&verifier, &workspace_id, "rk_1"),
            verifier
        );
    }

    #[test]
    fn device_authorization_message_matches_shared_vectors() {
        let fixture = load_fixture();
        for vector in fixture.message_vectors {
            let fields = vector.fields.iter().map(String::as_str).collect::<Vec<_>>();
            let digest = sha2::Sha256::digest(super::device_authorization_message(&fields));
            assert_eq!(format!("{digest:x}"), vector.sha256_hex, "{}", vector.name);
        }
    }

    fn load_fixture() -> FixtureFile {
        let text = fs::read_to_string(fixture_path()).expect("proof fixture is readable");
        serde_json::from_str(&text).expect("proof fixture parses")
    }

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/contracts/proofs/device-proof-subjects.json")
    }
}
