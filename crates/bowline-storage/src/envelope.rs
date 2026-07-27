use std::{error::Error, fmt, io::Cursor};

use chacha20poly1305::{
    Key, KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{ObjectKind, canonical_framing::CanonicalFrame};

mod nonce;
mod padding;

/// v3 is the convergent layout: a deterministic nonce (see [`nonce`]) over a
/// length-padded plaintext (see [`padding`]). The magic and version both move so
/// a v2 object — random nonce, unpadded — fails as an unknown format rather than
/// as a decryption error that reads like corruption.
const ENVELOPE_MAGIC: &[u8; 8] = b"bowenv3\0";
/// The trailing generation here numbers the *associated-data* encoding, which is
/// versioned independently of the envelope layout: v3 changed how the plaintext
/// is framed and how the nonce is chosen, not which fields are bound. Bumping it
/// in lockstep would invalidate the pinned-bytes tripwire below for a change it
/// is not meant to catch.
const ENVELOPE_AAD_DOMAIN: &str = "bowline-storage-envelope-v2";
const ENVELOPE_VERSION: u16 = 3;
const NONCE_LEN: usize = 24;
const HEADER_LEN: usize = ENVELOPE_MAGIC.len() + 2 + 4 + NONCE_LEN;

/// The key that decrypts every sealed object on this device.
///
/// Deliberately not `Copy`: `Copy` structurally forbids a meaningful `Drop`, so
/// every hop would leave an unscrubbable stack copy of the workspace key behind
/// in freed memory, core dumps, and swap. Moving it means each hop's copy is
/// zeroized when it goes out of scope.
#[derive(Clone, PartialEq, Eq)]
pub struct StorageKey(Zeroizing<[u8; 32]>);

impl StorageKey {
    #[cfg(test)]
    pub fn deterministic(byte: u8) -> Self {
        Self(Zeroizing::new([byte; 32]))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    fn as_key(&self) -> Key {
        Key::from(*self.0)
    }

    fn as_array(&self) -> &[u8; 32] {
        &self.0
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
    /// Canonical AEAD associated data for this context.
    ///
    /// The field order and labels below are the on-disk contract for every
    /// sealed object: change either and nothing sealed by an older binary can
    /// be opened again. `envelope_associated_data_bytes_are_pinned` fails
    /// loudly if this encoding moves.
    pub fn associated_data(&self) -> Vec<u8> {
        CanonicalFrame::new(ENVELOPE_AAD_DOMAIN)
            .str_field("workspace-id-hash", &self.workspace_id_hash)
            .str_field("object-kind", self.object_kind.associated_data_token())
            .str_field("object-id", &self.object_id)
            .str_field("record-id", &self.record_id)
            .u32_field("key-epoch", self.key_epoch)
            .u16_field("format-version", self.format_version)
            .into_bytes()
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

/// Seal `plaintext` convergently: identical plaintext under an identical key and
/// context always produces byte-identical output, so `blake3(sealed)` is a
/// content address rather than a fresh random name on every reseal.
pub fn seal(
    plaintext: &[u8],
    key: StorageKey,
    context: &EnvelopeContext,
) -> Result<SealedEnvelope, EnvelopeError> {
    seal_with_associated_data(
        plaintext,
        &key,
        context.key_epoch,
        &context.associated_data(),
    )
}

pub(crate) fn seal_with_associated_data(
    plaintext: &[u8],
    key: &StorageKey,
    key_epoch: u32,
    associated_data: &[u8],
) -> Result<SealedEnvelope, EnvelopeError> {
    if key_epoch == 0 {
        return Err(EnvelopeError::InvalidContext("key epoch must be non-zero"));
    }

    // Derived from the plaintext, so a nonce collision implies the whole sealed
    // object collides — the one case where XChaCha nonce reuse is harmless. See
    // `envelope::nonce` for the full argument.
    let nonce = nonce::derive_nonce::<NONCE_LEN>(key.as_array(), associated_data, plaintext);

    let compressed = Zeroizing::new(
        zstd::stream::encode_all(Cursor::new(plaintext), 0)
            .map_err(|_| EnvelopeError::CompressionFailed)?,
    );
    let padded = Zeroizing::new(padding::pad(&compressed).ok_or(EnvelopeError::PlaintextTooLarge)?);
    let cipher = XChaCha20Poly1305::new(&key.as_key());
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &padded,
                aad: associated_data,
            },
        )
        .map_err(|_| EnvelopeError::EncryptionFailed)?;

    let mut bytes = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    bytes.extend_from_slice(ENVELOPE_MAGIC);
    bytes.extend_from_slice(&ENVELOPE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&key_epoch.to_le_bytes());
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&ciphertext);

    Ok(SealedEnvelope { bytes })
}

pub fn open(
    envelope: &[u8],
    key: StorageKey,
    context: &EnvelopeContext,
) -> Result<Vec<u8>, EnvelopeError> {
    open_with_associated_data(
        envelope,
        &key,
        context.key_epoch,
        &context.associated_data(),
    )
}

pub(crate) fn open_with_associated_data(
    envelope: &[u8],
    key: &StorageKey,
    key_epoch: u32,
    associated_data: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    let fixed_header_len = ENVELOPE_MAGIC.len() + 2 + 4;
    if envelope.len() < fixed_header_len {
        return Err(EnvelopeError::Truncated);
    }
    if &envelope[..ENVELOPE_MAGIC.len()] != ENVELOPE_MAGIC {
        return Err(EnvelopeError::UnknownFormat);
    }

    let version = u16::from_le_bytes([
        envelope[ENVELOPE_MAGIC.len()],
        envelope[ENVELOPE_MAGIC.len() + 1],
    ]);
    let epoch_offset = ENVELOPE_MAGIC.len() + 2;
    let envelope_key_epoch = u32::from_le_bytes([
        envelope[epoch_offset],
        envelope[epoch_offset + 1],
        envelope[epoch_offset + 2],
        envelope[epoch_offset + 3],
    ]);
    if envelope_key_epoch != key_epoch {
        return Err(EnvelopeError::WrongContext);
    }

    if version != ENVELOPE_VERSION {
        return Err(EnvelopeError::UnsupportedVersion(version));
    }
    let padded = Zeroizing::new(open_sealed_body(envelope, key, associated_data)?);
    let compressed = padding::unpad(&padded).ok_or(EnvelopeError::PaddingCorrupt)?;

    zstd::stream::decode_all(Cursor::new(compressed))
        .map_err(|_| EnvelopeError::DecompressionFailed)
}

fn open_sealed_body(
    envelope: &[u8],
    key: &StorageKey,
    associated_data: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    if envelope.len() < HEADER_LEN {
        return Err(EnvelopeError::Truncated);
    }
    let nonce_offset = ENVELOPE_MAGIC.len() + 2 + 4;
    let nonce = &envelope[nonce_offset..nonce_offset + NONCE_LEN];
    let ciphertext = &envelope[HEADER_LEN..];
    let cipher = XChaCha20Poly1305::new(&key.as_key());
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: associated_data,
            },
        )
        .map_err(|_| EnvelopeError::VerificationFailed)
}

pub fn workspace_id_hash(value: &str) -> String {
    format!("wsh_{}", blake3::hash(value.as_bytes()).to_hex())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    InvalidContext(&'static str),
    CompressionFailed,
    EncryptionFailed,
    PlaintextTooLarge,
    PaddingCorrupt,
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
            Self::PlaintextTooLarge => {
                formatter.write_str("envelope plaintext exceeds the addressable frame")
            }
            Self::PaddingCorrupt => formatter.write_str("envelope padding frame is malformed"),
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn envelope_round_trips_with_associated_data() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let sealed = seal(b"source bytes", key.clone(), &context).expect("sealed");

        assert!(
            !sealed
                .as_bytes()
                .windows("source bytes".len())
                .any(|window| window == b"source bytes")
        );
        assert_eq!(
            open(sealed.as_bytes(), key.clone(), &context).expect("opened"),
            b"source bytes"
        );
    }

    /// The content-addressing contract: `blake3(sealed)` is the object key, so
    /// resealing the same bytes must land on the same key or the whole
    /// content-addressed design collects zero dedup (a rename, an A→B→A edit,
    /// and a branch round trip each re-upload bytes the server already holds).
    #[test]
    fn envelope_is_convergent_for_the_same_plaintext_and_context() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let first = seal(b"source bytes", key.clone(), &context).expect("first seal");
        let second = seal(b"source bytes", key.clone(), &context).expect("second seal");

        assert_eq!(first.as_bytes(), second.as_bytes());
        assert_eq!(
            open(first.as_bytes(), key.clone(), &context).expect("first opens"),
            b"source bytes"
        );
    }

    #[test]
    fn envelope_diverges_across_key_context_and_plaintext() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let baseline = seal(b"source bytes", key.clone(), &context).expect("baseline");

        let other_key = seal(b"source bytes", StorageKey::deterministic(9), &context)
            .expect("seal under other key");
        let other_context =
            seal(b"source bytes", key.clone(), &test_context("record-b")).expect("other context");
        let other_plaintext =
            seal(b"source byteS", key.clone(), &context).expect("other plaintext");

        assert_ne!(baseline.as_bytes(), other_key.as_bytes());
        assert_ne!(baseline.as_bytes(), other_context.as_bytes());
        assert_ne!(baseline.as_bytes(), other_plaintext.as_bytes());
    }

    #[test]
    fn envelope_uses_xchacha_nonce_width() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let sealed = seal(b"source bytes", key.clone(), &context).expect("sealed");
        let header_without_nonce_len = ENVELOPE_MAGIC.len() + 2 + 4;

        assert!(sealed.as_bytes().len() > header_without_nonce_len + 24);
        assert_eq!(NONCE_LEN, 24);
    }

    /// Distinct plaintexts must still get distinct nonces. Convergent sealing
    /// only stays safe while equal nonces imply equal plaintext; a collision
    /// here would be two different messages under one XChaCha nonce.
    #[test]
    fn envelope_nonce_does_not_repeat_across_distinct_plaintexts() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let mut nonces = HashSet::new();

        for index in 0..1024_u32 {
            let plaintext = format!("source bytes {index}");
            let sealed = seal(plaintext.as_bytes(), key.clone(), &context).expect("seal");
            let nonce_start = ENVELOPE_MAGIC.len() + 2 + 4;
            let nonce_end = nonce_start + NONCE_LEN;
            let nonce = sealed.as_bytes()[nonce_start..nonce_end].to_vec();
            assert!(nonces.insert(nonce), "envelope nonce repeated");
            assert_eq!(
                open(sealed.as_bytes(), key.clone(), &context).expect("opens"),
                plaintext.as_bytes()
            );
        }
    }

    /// R12: the sealed length must reveal only a ladder rung, so neighbouring
    /// plaintext sizes are indistinguishable to the server holding the objects.
    #[test]
    fn envelope_length_is_padded_onto_a_shared_ladder_rung() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");

        let short = seal(&vec![0xab_u8; 900], key.clone(), &context).expect("short");
        let longer = seal(&vec![0xab_u8; 940], key.clone(), &context).expect("longer");

        assert_eq!(short.as_bytes().len(), longer.as_bytes().len());
    }

    #[test]
    fn envelope_rejects_tamper_wrong_key_wrong_context_and_truncation() {
        let key = StorageKey::deterministic(7);
        let context = test_context("record-a");
        let sealed = seal(b"very secret env value", key.clone(), &context).expect("sealed");

        let mut tampered = sealed.as_bytes().to_vec();
        let last = tampered.last_mut().expect("ciphertext exists");
        *last ^= 1;
        assert!(matches!(
            open(&tampered, key.clone(), &context),
            Err(EnvelopeError::VerificationFailed)
        ));

        assert!(matches!(
            open(sealed.as_bytes(), StorageKey::deterministic(9), &context),
            Err(EnvelopeError::VerificationFailed)
        ));

        let wrong_context = test_context("record-b");
        assert!(matches!(
            open(sealed.as_bytes(), key.clone(), &wrong_context),
            Err(EnvelopeError::VerificationFailed | EnvelopeError::WrongContext)
        ));

        let mut wrong_epoch = context.clone();
        wrong_epoch.key_epoch = 2;
        assert!(matches!(
            open(sealed.as_bytes(), key.clone(), &wrong_epoch),
            Err(EnvelopeError::WrongContext)
        ));

        let mut unsupported_version = sealed.as_bytes().to_vec();
        unsupported_version[ENVELOPE_MAGIC.len()] = 99;
        assert!(matches!(
            open(&unsupported_version, key.clone(), &context),
            Err(EnvelopeError::UnsupportedVersion(_))
        ));

        assert!(matches!(
            open(&sealed.as_bytes()[..8], key.clone(), &context),
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

    const PINNED_ENVELOPE_ASSOCIATED_DATA: &str = concat!(
        "01000000000000001b626f776c696e652d73746f726167652d656e76656c6f70652d763200000000",
        "00000011776f726b73706163652d69642d68617368000000000000000a7773685f70696e6e656400",
        "0000000000000b6f626a6563742d6b696e640000000000000011776f726b73706163652d66696c65",
        "2d763100000000000000096f626a6563742d696400000000000000036f626a000000000000000972",
        "65636f72642d6964000000000000000372656300000000000000096b65792d65706f636800000000",
        "0000000400000001000000000000000e666f726d61742d76657273696f6e00000000000000020001",
    );

    /// Pins the exact associated-data bytes for a fixed context.
    ///
    /// A sealed object is only openable by reproducing these bytes, so this
    /// test is the tripwire for the one class of change that cannot be
    /// recovered from later: adding, renaming, reordering, or re-encoding an
    /// `EnvelopeContext` field. If it fails, the encoding moved and every
    /// object sealed by an older binary just became unopenable — do not
    /// re-bless the literal, revert the encoding change.
    #[test]
    fn envelope_associated_data_bytes_are_pinned() {
        let context = EnvelopeContext {
            workspace_id_hash: "wsh_pinned".to_string(),
            object_kind: ObjectKind::WorkspaceFileV1,
            object_id: "obj".to_string(),
            record_id: "rec".to_string(),
            key_epoch: 1,
            format_version: 1,
        };

        assert_eq!(
            hex(&context.associated_data()),
            PINNED_ENVELOPE_ASSOCIATED_DATA
        );
    }

    #[test]
    fn envelope_associated_data_separates_object_kinds() {
        let file = EnvelopeContext {
            object_kind: ObjectKind::WorkspaceFileV1,
            ..test_context("record-a")
        };
        let manifest = EnvelopeContext {
            object_kind: ObjectKind::WorkspaceManifestV1,
            ..test_context("record-a")
        };

        assert_ne!(file.associated_data(), manifest.associated_data());
        assert!(
            !String::from_utf8_lossy(&manifest.associated_data()).contains('{'),
            "associated data must not be a JSON rendering"
        );
    }

    /// R12's cost claim, asserted rather than asserted-in-prose: rounding sealed
    /// lengths up a ~1.1x ladder must cost a few percent of storage, not a
    /// visible fraction of the bill. Measured on incompressible payloads with a
    /// source-tree size distribution, so the number is padding and framing only
    /// — zstd cannot flatter it.
    #[test]
    fn envelope_padding_costs_a_few_percent_over_a_source_shaped_tree() {
        let key = StorageKey::deterministic(3);
        let context = test_context("record-a");
        let noise = incompressible_bytes(200_000);
        let sizes: Vec<usize> = (0..200_usize)
            .map(|index| match index % 100 {
                0..=60 => 200 + index * 61 % 9_800,
                61..=90 => 10_000 + index * 37 % 40_000,
                _ => 50_000 + index * 911 % 150_000,
            })
            .collect();

        let mut plaintext_total = 0_usize;
        let mut sealed_total = 0_usize;
        for size in &sizes {
            let plaintext = &noise[..*size];
            let sealed = seal(plaintext, key.clone(), &context).expect("seal");
            plaintext_total += plaintext.len();
            sealed_total += sealed.as_bytes().len();
        }

        assert!(
            sealed_total * 100 < plaintext_total * 108,
            "padding overhead was {:.2}% over {plaintext_total} plaintext bytes",
            (sealed_total as f64 / plaintext_total as f64 - 1.0) * 100.0
        );
    }

    fn incompressible_bytes(length: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(length + 32);
        let mut block = *blake3::hash(b"envelope-measurement-seed").as_bytes();
        while bytes.len() < length {
            bytes.extend_from_slice(&block);
            block = *blake3::hash(&block).as_bytes();
        }
        bytes
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn test_context(record_id: &str) -> EnvelopeContext {
        EnvelopeContext {
            workspace_id_hash: workspace_id_hash("ws_test"),
            object_kind: ObjectKind::WorkspaceFileV1,
            object_id: "pk_0011223344556677".to_string(),
            record_id: record_id.to_string(),
            key_epoch: 1,
            format_version: 1,
        }
    }
}
