use std::{error::Error, fmt, str::FromStr};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64_URL},
};
use bowline_control_plane::DeviceApproval;
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
    WorkspaceMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantPayload {
    workspace_id: WorkspaceId,
    request_id: DeviceApprovalRequestId,
    requester_device_id: DeviceId,
    requester_device_fingerprint: String,
    key_epoch: u32,
    workspace_key: Vec<u8>,
}

pub fn encrypt_workspace_key_for_request(
    key: &WorkspaceKeyMaterial,
    request: &bowline_control_plane::DeviceRequest,
) -> Result<String, GrantError> {
    let recipient = age::x25519::Recipient::from_str(&request.device_public_key)
        .map_err(|error| GrantError::Age(error.to_string()))?;
    let payload = GrantPayload {
        workspace_id: key.workspace_id.clone(),
        request_id: DeviceApprovalRequestId::new(request.request_id.clone()),
        requester_device_id: DeviceId::new(request.device_id.clone()),
        requester_device_fingerprint: request.device_fingerprint.clone(),
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
) -> Result<WorkspaceKeyMaterial, GrantError> {
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
    Ok(WorkspaceKeyMaterial {
        workspace_id: payload.workspace_id,
        key_epoch: payload.key_epoch,
        key_bytes: payload.workspace_key,
    })
}

pub fn encrypted_recovery_envelope(
    key: &WorkspaceKeyMaterial,
    words: &str,
) -> Result<String, GrantError> {
    let passphrase = age::secrecy::SecretString::from(words.to_string());
    let recipient = age::scrypt::Recipient::new(passphrase.clone());
    let plaintext = serde_json::to_vec(key)?;
    let ciphertext =
        age::encrypt(&recipient, &plaintext).map_err(|error| GrantError::Age(error.to_string()))?;
    Ok(BASE64.encode(ciphertext))
}

pub fn decrypt_recovery_envelope(
    ciphertext: &str,
    words: &str,
) -> Result<WorkspaceKeyMaterial, GrantError> {
    let ciphertext = BASE64.decode(ciphertext)?;
    let passphrase = age::secrecy::SecretString::from(words.to_string());
    let identity = age::scrypt::Identity::new(passphrase);
    let plaintext =
        age::decrypt(&identity, &ciphertext).map_err(|error| GrantError::Age(error.to_string()))?;
    serde_json::from_slice(&plaintext).map_err(Into::into)
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

pub fn device_authorization_proof_verifier(identity: &DeviceIdentity) -> String {
    let signing_key = device_signing_key(identity);
    let verifying_key = VerifyingKey::from(&signing_key);
    let public_key = verifying_key.to_encoded_point(false);
    format!("dapv_p256_v1_{}", BASE64_URL.encode(public_key.as_bytes()))
}

pub fn device_authorization_proof(
    identity: &DeviceIdentity,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    action: &str,
    subject: &str,
) -> String {
    let signing_key = device_signing_key(identity);
    let signature: Signature = signing_key.sign(&device_authorization_message(&[
        "bowline device authorization proof v2",
        workspace_id.as_str(),
        device_id.as_str(),
        action,
        subject,
    ]));
    format!("dapp_p256_v1_{}", BASE64_URL.encode(signature.to_bytes()))
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

fn device_signing_key(identity: &DeviceIdentity) -> SigningKey {
    for counter in 0_u8..=u8::MAX {
        let digest = Sha256::digest(device_authorization_message(&[
            "bowline device signing key v1",
            identity.secret(),
            &counter.to_string(),
        ]));
        if let Ok(signing_key) = SigningKey::from_slice(&digest) {
            return signing_key;
        }
    }
    unreachable!("sha256-derived P-256 signing key did not produce a valid scalar")
}

fn device_authorization_message(fields: &[&str]) -> Vec<u8> {
    let mut message = Vec::new();
    for field in fields {
        message.extend_from_slice(&(field.len() as u64).to_le_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    message
}

impl fmt::Display for GrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Age(error) => write!(formatter, "grant encryption failed: {error}"),
            Self::Base64(error) => write!(formatter, "grant ciphertext is malformed: {error}"),
            Self::Json(error) => write!(formatter, "grant payload is malformed: {error}"),
            Self::WorkspaceMismatch => {
                write!(formatter, "grant does not match this workspace or device")
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
    use bowline_control_plane::{DeviceApproval, DeviceRequest, DeviceRequestState};
    use bowline_core::ids::WorkspaceId;

    use super::{
        decrypt_workspace_key_from_grant, encrypt_workspace_key_for_request, recovery_proof,
        recovery_proof_verifier, recovery_proof_verifier_from_proof,
    };
    use crate::device_keys::{DeviceIdentity, WorkspaceKeyMaterial};

    #[test]
    fn correct_device_key_decrypts_grant() {
        let identity = DeviceIdentity::generate();
        let request = DeviceRequest {
            request_id: "request-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            device_id: "device-2".to_string(),
            device_name: "linux".to_string(),
            platform: "linux".to_string(),
            device_public_key: identity.public_key.as_str().to_string(),
            device_fingerprint: identity.fingerprint.as_str().to_string(),
            matching_code: "bowline-abc123".to_string(),
            account_id: None,
            host: None,
            root: None,
            requested_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            state: DeviceRequestState::Pending,
        };
        let key = WorkspaceKeyMaterial {
            workspace_id: WorkspaceId::new("workspace-1"),
            key_epoch: 1,
            key_bytes: vec![9; 32],
        };
        let ciphertext = encrypt_workspace_key_for_request(&key, &request).expect("encrypt");
        let grant = DeviceApproval {
            grant_id: "grant-1".to_string(),
            request_id: request.request_id,
            workspace_id: request.workspace_id,
            device_id: request.device_id,
            device_name: request.device_name,
            platform: request.platform,
            device_fingerprint: request.device_fingerprint,
            approved_by_device_id: "device-1".to_string(),
            encrypted_grant_ciphertext: ciphertext,
            key_epoch: 1,
            granted_at: bowline_control_plane::ControlPlaneTimestamp { tick: 3 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 4 },
            accepted_at: None,
            harness_only: false,
        };

        let decrypted = decrypt_workspace_key_from_grant(&identity, &grant).expect("decrypt");

        assert_eq!(decrypted, key);
    }

    #[test]
    fn wrong_device_key_fails_to_decrypt_grant() {
        let identity = DeviceIdentity::generate();
        let wrong_identity = DeviceIdentity::generate();
        let request = DeviceRequest {
            request_id: "request-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            device_id: "device-2".to_string(),
            device_name: "linux".to_string(),
            platform: "linux".to_string(),
            device_public_key: identity.public_key.as_str().to_string(),
            device_fingerprint: identity.fingerprint.as_str().to_string(),
            matching_code: "bowline-abc123".to_string(),
            account_id: None,
            host: None,
            root: None,
            requested_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            state: DeviceRequestState::Pending,
        };
        let key = WorkspaceKeyMaterial {
            workspace_id: WorkspaceId::new("workspace-1"),
            key_epoch: 1,
            key_bytes: vec![9; 32],
        };
        let ciphertext = encrypt_workspace_key_for_request(&key, &request).expect("encrypt");
        let grant = DeviceApproval {
            grant_id: "grant-1".to_string(),
            request_id: request.request_id,
            workspace_id: request.workspace_id,
            device_id: request.device_id,
            device_name: request.device_name,
            platform: request.platform,
            device_fingerprint: request.device_fingerprint,
            approved_by_device_id: "device-1".to_string(),
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
}
