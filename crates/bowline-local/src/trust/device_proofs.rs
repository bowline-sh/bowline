//! Device authorization proofs: the P-256 signature a device produces over
//! `(workspace, device, action, subject)`, the verifier it publishes, and the
//! matching code a human compares. Every trust decision in the client bottoms
//! out here, so it lives in exactly one place.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_control_plane::device_authorization_message;
use bowline_core::ids::{DeviceId, WorkspaceId};
use p256::ecdsa::{
    Signature, SigningKey, VerifyingKey,
    signature::{Signer, Verifier},
};
use sha2::{Digest, Sha256};

use crate::device_keys::DeviceIdentity;

use super::grants::{GrantError, sha256_proof_fields};

const PROOF_VERIFIER_PREFIX: &str = "dapv_p256_v1_";
const PROOF_SIGNATURE_PREFIX: &str = "dapp_p256_v1_";

pub struct DeviceAuthorizationProofCheck<'a> {
    pub proof_verifier: &'a str,
    pub proof: &'a str,
    pub workspace_id: &'a WorkspaceId,
    pub device_id: &'a DeviceId,
    pub action: &'a str,
    pub subject: &'a str,
}

pub fn verify_device_authorization_proof(
    check: DeviceAuthorizationProofCheck<'_>,
) -> Result<(), GrantError> {
    let encoded_key = check
        .proof_verifier
        .strip_prefix(PROOF_VERIFIER_PREFIX)
        .ok_or(GrantError::MalformedProofMaterial("proof verifier prefix"))?;
    let verifying_key = VerifyingKey::from_sec1_bytes(&BASE64_URL.decode(encoded_key)?)
        .map_err(|_| GrantError::MalformedProofMaterial("proof verifier key"))?;
    let encoded_signature = check
        .proof
        .strip_prefix(PROOF_SIGNATURE_PREFIX)
        .ok_or(GrantError::MalformedProofMaterial("proof signature prefix"))?;
    let signature = Signature::from_slice(&BASE64_URL.decode(encoded_signature)?)
        .map_err(|_| GrantError::MalformedProofMaterial("proof signature"))?;
    verifying_key
        .verify(
            &device_authorization_message(&[
                "bowline device authorization proof v2",
                check.workspace_id.as_str(),
                check.device_id.as_str(),
                check.action,
                check.subject,
            ]),
            &signature,
        )
        .map_err(|_| GrantError::GrantSealInvalid)
}

/// The code a human compares across the requesting and approving screens. It is
/// framed like every other proof subject so no field boundary can be shifted by
/// a `:` inside a caller-settable `device_id`.
pub fn device_matching_code(
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    public_key: &str,
    proof_verifier: &str,
) -> String {
    let hash = sha256_proof_fields(&[
        "bowline device matching code v2",
        workspace_id.as_str(),
        device_id.as_str(),
        public_key,
        proof_verifier,
    ]);
    format!("bowline-{hash}")
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

/// Proof that the recipient really recovered the material it was sent. Derived
/// from every key byte in the payload, so a device cannot claim an epoch it
/// only saw the ciphertext for, and the sealer can publish the verifier before
/// the recipient is ever online.
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
