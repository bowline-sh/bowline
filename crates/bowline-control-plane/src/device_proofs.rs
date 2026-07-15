use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use sha2::{Digest, Sha256};

use crate::RecoveryEnvelopeInput;

const DEVICE_AUTHORIZATION_VERIFIER_PREFIX: &str = "dapv_p256_v1_";
const DEVICE_AUTHORIZATION_PROOF_PREFIX: &str = "dapp_p256_v1_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceAuthorizationProofError {
    InvalidPrefix,
    MalformedVerifier,
    MalformedSignature,
    VerificationFailed,
}

pub fn device_authorization_message(fields: &[&str]) -> Vec<u8> {
    let mut message = Vec::new();
    for field in fields {
        message.extend_from_slice(&(field.len() as u64).to_le_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    message
}

pub(crate) fn verify_device_authorization_proof(
    verifier: &str,
    proof: &str,
    workspace_id: &str,
    device_id: &str,
    action: &str,
    subject: &str,
) -> Result<(), DeviceAuthorizationProofError> {
    let public_key = verifier
        .strip_prefix(DEVICE_AUTHORIZATION_VERIFIER_PREFIX)
        .ok_or(DeviceAuthorizationProofError::InvalidPrefix)?;
    let signature = proof
        .strip_prefix(DEVICE_AUTHORIZATION_PROOF_PREFIX)
        .ok_or(DeviceAuthorizationProofError::InvalidPrefix)?;
    let public_key = BASE64_URL
        .decode(public_key)
        .map_err(|_| DeviceAuthorizationProofError::MalformedVerifier)?;
    let signature = BASE64_URL
        .decode(signature)
        .map_err(|_| DeviceAuthorizationProofError::MalformedSignature)?;
    let verifying_key = VerifyingKey::from_sec1_bytes(&public_key)
        .map_err(|_| DeviceAuthorizationProofError::MalformedVerifier)?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| DeviceAuthorizationProofError::MalformedSignature)?;
    verifying_key
        .verify(
            &device_authorization_message(&[
                "bowline device authorization proof v2",
                workspace_id,
                device_id,
                action,
                subject,
            ]),
            &signature,
        )
        .map_err(|_| DeviceAuthorizationProofError::VerificationFailed)
}

pub fn recovery_envelope_payload_proof_subject(input: &RecoveryEnvelopeInput) -> String {
    recovery_envelope_payload_proof_subject_parts(
        input.envelope_id.as_str(),
        &input.fingerprint,
        &input.recovery_proof_verifier,
        &input.ciphertext,
    )
}

pub fn recovery_envelope_payload_proof_subject_parts(
    envelope_id: &str,
    fingerprint: &str,
    recovery_proof_verifier: &str,
    ciphertext: &str,
) -> String {
    let ciphertext_hash = Sha256::digest(ciphertext.as_bytes());
    format!(
        "envelopeId={envelope_id}\nfingerprint={fingerprint}\nrecoveryProofVerifier={recovery_proof_verifier}\nciphertextHash=sha256:{ciphertext_hash:x}"
    )
}

pub fn recovery_envelope_proof_subject(envelope_id: impl AsRef<str>) -> String {
    format!("envelopeId={}", envelope_id.as_ref())
}

pub fn device_request_proof_subject(request_id: impl AsRef<str>) -> String {
    format!("requestId={}", request_id.as_ref())
}

pub fn device_revocation_proof_subject(device_id: impl AsRef<str>) -> String {
    format!("deviceId={}", device_id.as_ref())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use serde::Deserialize;
    use sha2::{Digest, Sha256};

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
    fn device_authorization_message_matches_shared_vectors() {
        let fixture = load_fixture();
        for vector in fixture.message_vectors {
            let fields = vector.fields.iter().map(String::as_str).collect::<Vec<_>>();
            let digest = Sha256::digest(super::device_authorization_message(&fields));
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
