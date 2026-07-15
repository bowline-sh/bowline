use std::{error::Error, fmt};

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use chacha20poly1305::{
    AeadCore, ChaCha20Poly1305, KeyInit,
    aead::{Aead, OsRng, Payload},
};
use serde::{Deserialize, Serialize};

use super::bundle::HandoffBundle;

const ENVELOPE_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffTransferEnvelope {
    pub version: u16,
    pub target: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Debug)]
pub enum HandoffTransferError {
    Serialize(serde_json::Error),
    Decode(base64::DecodeError),
    Crypto,
    WrongTarget { expected: String, actual: String },
}

impl fmt::Display for HandoffTransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(error) => write!(formatter, "handoff bundle JSON failed: {error}"),
            Self::Decode(error) => write!(formatter, "handoff envelope base64 failed: {error}"),
            Self::Crypto => write!(
                formatter,
                "handoff transfer envelope could not be decrypted"
            ),
            Self::WrongTarget { expected, actual } => {
                write!(
                    formatter,
                    "handoff envelope target `{actual}` does not match `{expected}`"
                )
            }
        }
    }
}

impl Error for HandoffTransferError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Serialize(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Crypto | Self::WrongTarget { .. } => None,
        }
    }
}

pub fn encrypt_bundle(
    bundle: &HandoffBundle,
    target: &str,
    key_material: &[u8],
) -> Result<HandoffTransferEnvelope, HandoffTransferError> {
    let key = derive_key(key_material);
    let cipher = ChaCha20Poly1305::new(&key.into());
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let plaintext = serde_json::to_vec(bundle).map_err(HandoffTransferError::Serialize)?;
    let aad = aad(target);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| HandoffTransferError::Crypto)?;
    Ok(HandoffTransferEnvelope {
        version: ENVELOPE_VERSION,
        target: target.to_string(),
        nonce: STANDARD_NO_PAD.encode(nonce),
        ciphertext: STANDARD_NO_PAD.encode(ciphertext),
    })
}

pub fn generate_transfer_key() -> Result<String, HandoffTransferError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| HandoffTransferError::Crypto)?;
    Ok(STANDARD_NO_PAD.encode(bytes))
}

pub fn decrypt_bundle(
    envelope: &HandoffTransferEnvelope,
    expected_target: &str,
    key_material: &[u8],
) -> Result<HandoffBundle, HandoffTransferError> {
    if envelope.version != ENVELOPE_VERSION {
        return Err(HandoffTransferError::Crypto);
    }
    if envelope.target != expected_target {
        return Err(HandoffTransferError::WrongTarget {
            expected: expected_target.to_string(),
            actual: envelope.target.clone(),
        });
    }
    let key = derive_key(key_material);
    let cipher = ChaCha20Poly1305::new(&key.into());
    let nonce_bytes = STANDARD_NO_PAD
        .decode(&envelope.nonce)
        .map_err(HandoffTransferError::Decode)?;
    if nonce_bytes.len() != 12 {
        return Err(HandoffTransferError::Crypto);
    }
    let ciphertext = STANDARD_NO_PAD
        .decode(&envelope.ciphertext)
        .map_err(HandoffTransferError::Decode)?;
    let aad = aad(expected_target);
    let plaintext = cipher
        .decrypt(
            nonce_bytes.as_slice().into(),
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| HandoffTransferError::Crypto)?;
    serde_json::from_slice(&plaintext).map_err(HandoffTransferError::Serialize)
}

pub fn envelope_json(envelope: &HandoffTransferEnvelope) -> Result<String, HandoffTransferError> {
    serde_json::to_string(envelope).map_err(HandoffTransferError::Serialize)
}

pub fn envelope_from_json(value: &str) -> Result<HandoffTransferEnvelope, HandoffTransferError> {
    serde_json::from_str(value).map_err(HandoffTransferError::Serialize)
}

fn derive_key(key_material: &[u8]) -> [u8; 32] {
    *blake3::hash(key_material).as_bytes()
}

fn aad(target: &str) -> Vec<u8> {
    format!("bowline-handoff-v{ENVELOPE_VERSION}:{target}").into_bytes()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bowline_core::commands::{HandoffAgent, HandoffSessionMode};

    use super::*;
    use crate::agents::handoff::bundle::{HandoffBundleManifest, HandoffPromptPayload};

    #[test]
    fn envelope_round_trips_and_rejects_wrong_target() {
        let bundle = HandoffBundle {
            manifest: HandoffBundleManifest {
                agent: HandoffAgent::Codex,
                session_mode: HandoffSessionMode::FreshPrompt,
                session_id: None,
                remote_project_path: PathBuf::from("~/Code/app"),
                created_for_target: "linux".to_string(),
            },
            files: Vec::new(),
            prompt: Some(HandoffPromptPayload {
                bytes: b"do the thing".to_vec(),
            }),
        };

        let envelope = encrypt_bundle(&bundle, "linux", b"shared-key").expect("encrypt");
        let decrypted = decrypt_bundle(&envelope, "linux", b"shared-key").expect("decrypt");
        assert_eq!(decrypted, bundle);
        assert!(decrypt_bundle(&envelope, "other", b"shared-key").is_err());
        let mut wrong_version = envelope.clone();
        wrong_version.version += 1;
        assert!(decrypt_bundle(&wrong_version, "linux", b"shared-key").is_err());
    }

    #[test]
    fn envelope_rejects_modified_ciphertext() {
        let bundle = HandoffBundle {
            manifest: HandoffBundleManifest {
                agent: HandoffAgent::Claude,
                session_mode: HandoffSessionMode::FreshPrompt,
                session_id: None,
                remote_project_path: PathBuf::from("~/Code/app"),
                created_for_target: "linux".to_string(),
            },
            files: Vec::new(),
            prompt: None,
        };
        let mut envelope = encrypt_bundle(&bundle, "linux", b"shared-key").expect("encrypt");
        envelope.ciphertext.push('A');
        assert!(decrypt_bundle(&envelope, "linux", b"shared-key").is_err());
    }
}
