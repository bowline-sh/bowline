//! The Recovery Key envelope: workspace key material sealed under a passphrase
//! rather than a device key, plus the proofs that let the control plane check a
//! recovery attempt without ever learning the words.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bowline_core::ids::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::device_keys::WorkspaceKeyMaterial;

use super::grants::{DeviceGrantAuthorizer, GrantError, sha256_proof_fields};

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

pub fn redacted_words_debug(words: &str) -> String {
    let count = words.split_whitespace().count();
    format!("[{count} recovery words redacted]")
}

#[cfg(test)]
mod tests {
    use bowline_core::ids::WorkspaceId;

    use super::{recovery_proof, recovery_proof_verifier, recovery_proof_verifier_from_proof};

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
