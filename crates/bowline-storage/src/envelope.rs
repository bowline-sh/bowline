use std::{collections::HashSet, error::Error, fmt, io::Cursor};

use chacha20poly1305::{
    ChaCha20Poly1305, Key, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use serde::{Deserialize, Serialize};

use crate::ObjectKind;

const ENVELOPE_MAGIC: &[u8; 8] = b"bowenv1\0";
const ENVELOPE_VERSION: u16 = 1;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = ENVELOPE_MAGIC.len() + 2 + 4 + NONCE_LEN;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct StorageKey([u8; 32]);

impl StorageKey {
    pub fn deterministic(byte: u8) -> Self {
        Self([byte; 32])
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn as_key(self) -> Key {
        Key::from(self.0)
    }
}

impl fmt::Debug for StorageKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageKey(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeContext {
    pub workspace_id_hash: String,
    pub object_kind: ObjectKind,
    pub object_id: String,
    pub record_id: String,
    pub key_epoch: u32,
    pub format_version: u16,
}

impl EnvelopeContext {
    pub fn associated_data(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("envelope context serializes")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedEnvelope {
    bytes: Vec<u8>,
}

impl SealedEnvelope {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub fn seal(
    plaintext: &[u8],
    key: StorageKey,
    context: &EnvelopeContext,
) -> Result<SealedEnvelope, EnvelopeError> {
    seal_with_nonce(plaintext, key, context, random_nonce()?)
}

#[derive(Debug, Default)]
pub(crate) struct EnvelopeNonceTracker {
    seen: HashSet<(u32, [u8; NONCE_LEN])>,
}

impl EnvelopeNonceTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn reserve(
        &mut self,
        context: &EnvelopeContext,
        nonce: [u8; NONCE_LEN],
    ) -> Result<(), EnvelopeError> {
        if context.key_epoch == 0 {
            return Err(EnvelopeError::InvalidContext("key epoch must be non-zero"));
        }
        if !self.seen.insert((context.key_epoch, nonce)) {
            return Err(EnvelopeError::NonceReuse);
        }
        Ok(())
    }
}

pub(crate) fn seal_tracked(
    plaintext: &[u8],
    key: StorageKey,
    context: &EnvelopeContext,
    nonce_tracker: &mut EnvelopeNonceTracker,
) -> Result<SealedEnvelope, EnvelopeError> {
    let nonce = random_nonce()?;
    seal_with_tracked_nonce(plaintext, key, context, nonce, nonce_tracker)
}

fn seal_with_tracked_nonce(
    plaintext: &[u8],
    key: StorageKey,
    context: &EnvelopeContext,
    nonce: [u8; NONCE_LEN],
    nonce_tracker: &mut EnvelopeNonceTracker,
) -> Result<SealedEnvelope, EnvelopeError> {
    nonce_tracker.reserve(context, nonce)?;
    seal_with_nonce(plaintext, key, context, nonce)
}

fn seal_with_nonce(
    plaintext: &[u8],
    key: StorageKey,
    context: &EnvelopeContext,
    nonce: [u8; NONCE_LEN],
) -> Result<SealedEnvelope, EnvelopeError> {
    if context.key_epoch == 0 {
        return Err(EnvelopeError::InvalidContext("key epoch must be non-zero"));
    }

    let compressed = zstd::stream::encode_all(Cursor::new(plaintext), 0)
        .map_err(|_| EnvelopeError::CompressionFailed)?;
    let associated_data = context.associated_data();
    let cipher = ChaCha20Poly1305::new(&key.as_key());
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &compressed,
                aad: &associated_data,
            },
        )
        .map_err(|_| EnvelopeError::EncryptionFailed)?;

    let mut bytes = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    bytes.extend_from_slice(ENVELOPE_MAGIC);
    bytes.extend_from_slice(&ENVELOPE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&context.key_epoch.to_le_bytes());
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&ciphertext);

    Ok(SealedEnvelope { bytes })
}

pub fn open(
    envelope: &[u8],
    key: StorageKey,
    context: &EnvelopeContext,
) -> Result<Vec<u8>, EnvelopeError> {
    if envelope.len() < HEADER_LEN {
        return Err(EnvelopeError::Truncated);
    }
    if &envelope[..ENVELOPE_MAGIC.len()] != ENVELOPE_MAGIC {
        return Err(EnvelopeError::UnknownFormat);
    }

    let version = u16::from_le_bytes([
        envelope[ENVELOPE_MAGIC.len()],
        envelope[ENVELOPE_MAGIC.len() + 1],
    ]);
    if version != ENVELOPE_VERSION {
        return Err(EnvelopeError::UnsupportedVersion(version));
    }

    let epoch_offset = ENVELOPE_MAGIC.len() + 2;
    let key_epoch = u32::from_le_bytes([
        envelope[epoch_offset],
        envelope[epoch_offset + 1],
        envelope[epoch_offset + 2],
        envelope[epoch_offset + 3],
    ]);
    if key_epoch != context.key_epoch {
        return Err(EnvelopeError::WrongContext);
    }

    let nonce_offset = epoch_offset + 4;
    let nonce = &envelope[nonce_offset..nonce_offset + NONCE_LEN];
    let ciphertext = &envelope[HEADER_LEN..];
    let associated_data = context.associated_data();
    let cipher = ChaCha20Poly1305::new(&key.as_key());
    let compressed = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: &associated_data,
            },
        )
        .map_err(|_| EnvelopeError::VerificationFailed)?;

    zstd::stream::decode_all(Cursor::new(compressed))
        .map_err(|_| EnvelopeError::DecompressionFailed)
}

pub fn workspace_id_hash(value: &str) -> String {
    format!("wsh_{}", blake3::hash(value.as_bytes()).to_hex())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    InvalidContext(&'static str),
    CompressionFailed,
    EncryptionFailed,
    RandomFailed,
    NonceReuse,
    DecompressionFailed,
    Truncated,
    UnknownFormat,
    UnsupportedVersion(u16),
    WrongContext,
    VerificationFailed,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidContext(reason) => {
                write!(formatter, "invalid encryption context: {reason}")
            }
            Self::CompressionFailed => formatter.write_str("envelope compression failed"),
            Self::EncryptionFailed => formatter.write_str("envelope encryption failed"),
            Self::RandomFailed => formatter.write_str("envelope nonce generation failed"),
            Self::NonceReuse => formatter.write_str("envelope nonce reuse was refused"),
            Self::DecompressionFailed => formatter.write_str("envelope decompression failed"),
            Self::Truncated => formatter.write_str("encrypted envelope is truncated"),
            Self::UnknownFormat => formatter.write_str("encrypted envelope has unknown format"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "encrypted envelope version {version} is unsupported"
                )
            }
            Self::WrongContext => {
                formatter.write_str("encrypted envelope was opened with the wrong context")
            }
            Self::VerificationFailed => {
                formatter.write_str("encrypted envelope verification failed")
            }
        }
    }
}

impl Error for EnvelopeError {}

fn random_nonce() -> Result<[u8; NONCE_LEN], EnvelopeError> {
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| EnvelopeError::RandomFailed)?;
    Ok(nonce)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn envelope_round_trips_with_associated_data() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let sealed = seal(b"source bytes", key, &context).expect("sealed");

        assert!(
            !sealed
                .as_bytes()
                .windows("source bytes".len())
                .any(|window| window == b"source bytes")
        );
        assert_eq!(
            open(sealed.as_bytes(), key, &context).expect("opened"),
            b"source bytes"
        );
    }

    #[test]
    fn envelope_uses_unique_nonce_for_same_plaintext_and_context() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let first = seal(b"source bytes", key, &context).expect("first seal");
        let second = seal(b"source bytes", key, &context).expect("second seal");

        assert_ne!(first.as_bytes(), second.as_bytes());
        assert_eq!(
            open(first.as_bytes(), key, &context).expect("first opens"),
            b"source bytes"
        );
        assert_eq!(
            open(second.as_bytes(), key, &context).expect("second opens"),
            b"source bytes"
        );
    }

    #[test]
    fn envelope_nonce_does_not_repeat_across_batch_for_same_epoch() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let mut nonces = HashSet::new();

        for _ in 0..1024 {
            let sealed = seal(b"source bytes", key, &context).expect("seal");
            let nonce_start = ENVELOPE_MAGIC.len() + 2 + 4;
            let nonce_end = nonce_start + NONCE_LEN;
            let nonce = sealed.as_bytes()[nonce_start..nonce_end].to_vec();
            assert!(nonces.insert(nonce), "envelope nonce repeated");
            assert_eq!(
                open(sealed.as_bytes(), key, &context).expect("opens"),
                b"source bytes"
            );
        }
    }

    #[test]
    fn envelope_tracker_refuses_forced_nonce_reuse_for_same_epoch() {
        let key = StorageKey::deterministic(7);
        let first_context = test_context("record-a");
        let second_context = test_context("record-b");
        let nonce = [42_u8; NONCE_LEN];
        let mut tracker = EnvelopeNonceTracker::new();

        let first = seal_with_tracked_nonce(
            b"first source bytes",
            key,
            &first_context,
            nonce,
            &mut tracker,
        )
        .expect("first seal succeeds");
        assert_eq!(
            open(first.as_bytes(), key, &first_context).expect("first opens"),
            b"first source bytes"
        );

        assert!(matches!(
            seal_with_tracked_nonce(
                b"second source bytes",
                key,
                &second_context,
                nonce,
                &mut tracker,
            ),
            Err(EnvelopeError::NonceReuse)
        ));
    }

    #[test]
    fn envelope_rejects_tamper_wrong_key_wrong_context_and_truncation() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let sealed = seal(b"very secret env value", key, &context).expect("sealed");

        let mut tampered = sealed.as_bytes().to_vec();
        let last = tampered.last_mut().expect("ciphertext exists");
        *last ^= 1;
        assert!(matches!(
            open(&tampered, key, &context),
            Err(EnvelopeError::VerificationFailed)
        ));

        assert!(matches!(
            open(sealed.as_bytes(), StorageKey::deterministic(9), &context),
            Err(EnvelopeError::VerificationFailed)
        ));

        let wrong_context = test_context("record-b");
        assert!(matches!(
            open(sealed.as_bytes(), key, &wrong_context),
            Err(EnvelopeError::VerificationFailed | EnvelopeError::WrongContext)
        ));

        let mut wrong_epoch = context.clone();
        wrong_epoch.key_epoch = 2;
        assert!(matches!(
            open(sealed.as_bytes(), key, &wrong_epoch),
            Err(EnvelopeError::WrongContext)
        ));

        let mut unsupported_version = sealed.as_bytes().to_vec();
        unsupported_version[ENVELOPE_MAGIC.len()] = 99;
        assert!(matches!(
            open(&unsupported_version, key, &context),
            Err(EnvelopeError::UnsupportedVersion(_))
        ));

        assert!(matches!(
            open(&sealed.as_bytes()[..8], key, &context),
            Err(EnvelopeError::Truncated)
        ));
    }

    #[test]
    fn envelope_errors_do_not_expose_key_or_plaintext() {
        let error = EnvelopeError::VerificationFailed.to_string();
        assert!(!error.contains("very secret env value"));
        assert!(!error.contains("070707"));
        assert!(!format!("{:?}", StorageKey::deterministic(7)).contains("7, 7, 7"));
    }

    fn test_context(record_id: &str) -> EnvelopeContext {
        EnvelopeContext {
            workspace_id_hash: workspace_id_hash("ws_test"),
            object_kind: ObjectKind::SourcePack,
            object_id: "pk_0011223344556677".to_string(),
            record_id: record_id.to_string(),
            key_epoch: 1,
            format_version: 1,
        }
    }
}
