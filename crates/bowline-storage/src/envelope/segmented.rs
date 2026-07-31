//! Bounded-memory envelope framing for large workspace files.
//!
//! Each plaintext segment is compressed, length-padded, and authenticated
//! independently. A final authenticated frame commits the aggregate plaintext
//! length, so truncating a complete prefix cannot look like a valid object.

use std::io::{Cursor, Read, Write};

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use zeroize::Zeroizing;

use super::{EnvelopeError, NONCE_LEN, StorageKey, decode_bounded, nonce, padding};

const MAGIC: &[u8; 8] = b"bowseg1\0";
const VERSION: u16 = 1;
const SEGMENT_PLAINTEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CIPHERTEXT_BYTES: usize = SEGMENT_PLAINTEXT_BYTES * 2;
const DATA_FRAME: u8 = 1;
const END_FRAME: u8 = 2;
const FRAME_HEADER_BYTES: usize = 1 + 8 + 4 + NONCE_LEN;
const FRAME_AAD_DOMAIN: &[u8] = b"bowline-segment-frame-v1";

pub(super) fn has_magic(prefix: &[u8]) -> bool {
    prefix.starts_with(MAGIC)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentedSealStats {
    pub plaintext_bytes: u64,
    pub sealed_bytes: u64,
    pub segments: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentedOpenStats {
    pub plaintext_bytes: u64,
    pub segments: u64,
}

pub(super) fn seal(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    key: &StorageKey,
    key_epoch: u32,
    associated_data: &[u8],
) -> Result<SegmentedSealStats, EnvelopeError> {
    if key_epoch == 0 {
        return Err(EnvelopeError::InvalidContext("key epoch must be non-zero"));
    }
    write_header(writer, key_epoch)?;
    let cipher = XChaCha20Poly1305::new(&key.as_key());
    let mut plaintext_bytes = 0_u64;
    let mut sealed_bytes = header_len() as u64;
    let mut index = 0_u64;
    let mut segment = Zeroizing::new(vec![0_u8; SEGMENT_PLAINTEXT_BYTES]);

    loop {
        let read = read_segment(reader, &mut segment)?;
        if read == 0 {
            break;
        }
        plaintext_bytes = plaintext_bytes
            .checked_add(read as u64)
            .ok_or(EnvelopeError::PlaintextTooLarge)?;
        let plaintext = &segment[..read];
        let compressed = Zeroizing::new(
            zstd::stream::encode_all(Cursor::new(plaintext), 0)
                .map_err(|_| EnvelopeError::CompressionFailed)?,
        );
        let padded =
            Zeroizing::new(padding::pad(&compressed).ok_or(EnvelopeError::PlaintextTooLarge)?);
        let aad = frame_aad(associated_data, DATA_FRAME, index);
        let frame_nonce = nonce::derive_nonce::<NONCE_LEN>(key.as_array(), &aad, plaintext);
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&frame_nonce),
                Payload {
                    msg: &padded,
                    aad: &aad,
                },
            )
            .map_err(|_| EnvelopeError::EncryptionFailed)?;
        write_frame(writer, DATA_FRAME, index, &frame_nonce, &ciphertext)?;
        sealed_bytes = sealed_bytes
            .checked_add(frame_len(ciphertext.len())?)
            .ok_or(EnvelopeError::PlaintextTooLarge)?;
        index = index
            .checked_add(1)
            .ok_or(EnvelopeError::PlaintextTooLarge)?;
    }

    let total_bytes = plaintext_bytes.to_le_bytes();
    let aad = frame_aad(associated_data, END_FRAME, index);
    let frame_nonce = nonce::derive_nonce::<NONCE_LEN>(key.as_array(), &aad, &total_bytes);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&frame_nonce),
            Payload {
                msg: &total_bytes,
                aad: &aad,
            },
        )
        .map_err(|_| EnvelopeError::EncryptionFailed)?;
    write_frame(writer, END_FRAME, index, &frame_nonce, &ciphertext)?;
    sealed_bytes = sealed_bytes
        .checked_add(frame_len(ciphertext.len())?)
        .ok_or(EnvelopeError::PlaintextTooLarge)?;

    Ok(SegmentedSealStats {
        plaintext_bytes,
        sealed_bytes,
        segments: index,
    })
}

fn frame_len(ciphertext_len: usize) -> Result<u64, EnvelopeError> {
    (FRAME_HEADER_BYTES as u64)
        .checked_add(ciphertext_len as u64)
        .ok_or(EnvelopeError::PlaintextTooLarge)
}

pub(super) fn open(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    key: &StorageKey,
    key_epoch: u32,
    associated_data: &[u8],
    max_decoded_bytes: u64,
) -> Result<SegmentedOpenStats, EnvelopeError> {
    read_header(reader, key_epoch)?;
    let cipher = XChaCha20Poly1305::new(&key.as_key());
    let mut expected_index = 0_u64;
    let mut plaintext_bytes = 0_u64;

    loop {
        let frame = read_frame(reader)?;
        if frame.index != expected_index {
            return Err(EnvelopeError::SegmentOutOfOrder);
        }
        let aad = frame_aad(associated_data, frame.kind, frame.index);
        let authenticated = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&frame.nonce),
                    Payload {
                        msg: &frame.ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| EnvelopeError::VerificationFailed)?,
        );

        match frame.kind {
            DATA_FRAME => {
                let compressed =
                    padding::unpad(&authenticated).ok_or(EnvelopeError::PaddingCorrupt)?;
                let remaining = max_decoded_bytes.saturating_sub(plaintext_bytes);
                let decoded =
                    decode_bounded(compressed, remaining.min(SEGMENT_PLAINTEXT_BYTES as u64))?;
                if decoded.is_empty() {
                    return Err(EnvelopeError::InvalidSegment);
                }
                plaintext_bytes = plaintext_bytes.checked_add(decoded.len() as u64).ok_or(
                    EnvelopeError::DecodedSizeExceeded {
                        maximum: max_decoded_bytes,
                        decoded: u64::MAX,
                    },
                )?;
                if plaintext_bytes > max_decoded_bytes {
                    return Err(EnvelopeError::DecodedSizeExceeded {
                        maximum: max_decoded_bytes,
                        decoded: plaintext_bytes,
                    });
                }
                writer
                    .write_all(&decoded)
                    .map_err(|_| EnvelopeError::WriteFailed)?;
                expected_index = expected_index
                    .checked_add(1)
                    .ok_or(EnvelopeError::InvalidSegment)?;
            }
            END_FRAME => {
                let declared = parse_total_bytes(&authenticated)?;
                if declared != plaintext_bytes {
                    return Err(EnvelopeError::InvalidSegment);
                }
                let mut trailing = [0_u8; 1];
                match reader.read(&mut trailing) {
                    Ok(0) => {
                        return Ok(SegmentedOpenStats {
                            plaintext_bytes,
                            segments: expected_index,
                        });
                    }
                    Ok(_) => return Err(EnvelopeError::TrailingData),
                    Err(_) => return Err(EnvelopeError::ReadFailed),
                }
            }
            _ => return Err(EnvelopeError::InvalidSegment),
        }
    }
}

fn header_len() -> usize {
    MAGIC.len() + 2 + 4 + 4
}

fn write_header(writer: &mut dyn Write, key_epoch: u32) -> Result<(), EnvelopeError> {
    writer
        .write_all(MAGIC)
        .and_then(|()| writer.write_all(&VERSION.to_le_bytes()))
        .and_then(|()| writer.write_all(&key_epoch.to_le_bytes()))
        .and_then(|()| writer.write_all(&(SEGMENT_PLAINTEXT_BYTES as u32).to_le_bytes()))
        .map_err(|_| EnvelopeError::WriteFailed)
}

fn read_header(reader: &mut dyn Read, key_epoch: u32) -> Result<(), EnvelopeError> {
    let mut header = [0_u8; 18];
    reader
        .read_exact(&mut header)
        .map_err(|_| EnvelopeError::Truncated)?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(EnvelopeError::UnknownFormat);
    }
    let version = u16::from_le_bytes([header[8], header[9]]);
    if version != VERSION {
        return Err(EnvelopeError::UnsupportedVersion(version));
    }
    let envelope_epoch = u32::from_le_bytes([header[10], header[11], header[12], header[13]]);
    if envelope_epoch != key_epoch {
        return Err(EnvelopeError::WrongContext);
    }
    let segment_bytes = u32::from_le_bytes([header[14], header[15], header[16], header[17]]);
    if segment_bytes as usize != SEGMENT_PLAINTEXT_BYTES {
        return Err(EnvelopeError::InvalidSegment);
    }
    Ok(())
}

fn read_segment(reader: &mut dyn Read, buffer: &mut [u8]) -> Result<usize, EnvelopeError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(EnvelopeError::ReadFailed),
        }
    }
    Ok(filled)
}

fn frame_aad(associated_data: &[u8], kind: u8, index: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(associated_data.len() + FRAME_AAD_DOMAIN.len() + 1 + 8);
    aad.extend_from_slice(associated_data);
    aad.extend_from_slice(FRAME_AAD_DOMAIN);
    aad.push(kind);
    aad.extend_from_slice(&index.to_le_bytes());
    aad
}

fn write_frame(
    writer: &mut dyn Write,
    kind: u8,
    index: u64,
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<(), EnvelopeError> {
    let ciphertext_len =
        u32::try_from(ciphertext.len()).map_err(|_| EnvelopeError::PlaintextTooLarge)?;
    writer
        .write_all(&[kind])
        .and_then(|()| writer.write_all(&index.to_le_bytes()))
        .and_then(|()| writer.write_all(&ciphertext_len.to_le_bytes()))
        .and_then(|()| writer.write_all(nonce))
        .and_then(|()| writer.write_all(ciphertext))
        .map_err(|_| EnvelopeError::WriteFailed)
}

struct Frame {
    kind: u8,
    index: u64,
    nonce: [u8; NONCE_LEN],
    ciphertext: Vec<u8>,
}

fn read_frame(reader: &mut dyn Read) -> Result<Frame, EnvelopeError> {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .map_err(|_| EnvelopeError::Truncated)?;
    let kind = header[0];
    let index = u64::from_le_bytes(
        header[1..9]
            .try_into()
            .map_err(|_| EnvelopeError::InvalidSegment)?,
    );
    let ciphertext_len = u32::from_le_bytes(
        header[9..13]
            .try_into()
            .map_err(|_| EnvelopeError::InvalidSegment)?,
    ) as usize;
    if ciphertext_len == 0 || ciphertext_len > MAX_CIPHERTEXT_BYTES {
        return Err(EnvelopeError::InvalidSegment);
    }
    let mut nonce = [0_u8; NONCE_LEN];
    nonce.copy_from_slice(&header[13..]);
    let mut ciphertext = vec![0_u8; ciphertext_len];
    reader
        .read_exact(&mut ciphertext)
        .map_err(|_| EnvelopeError::Truncated)?;
    Ok(Frame {
        kind,
        index,
        nonce,
        ciphertext,
    })
}

fn parse_total_bytes(bytes: &[u8]) -> Result<u64, EnvelopeError> {
    let encoded: [u8; 8] = bytes
        .try_into()
        .map_err(|_| EnvelopeError::InvalidSegment)?;
    Ok(u64::from_le_bytes(encoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EnvelopeContext, ObjectKind};

    struct MeasuringReader {
        inner: Cursor<Vec<u8>>,
        largest_request: usize,
    }

    impl Read for MeasuringReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.largest_request = self.largest_request.max(buffer.len());
            self.inner.read(buffer)
        }
    }

    #[derive(Default)]
    struct MeasuringWriter {
        bytes: Vec<u8>,
        largest_write: usize,
    }

    impl Write for MeasuringWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.largest_write = self.largest_write.max(buffer.len());
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn context() -> EnvelopeContext {
        EnvelopeContext {
            workspace_id_hash: "wsh_test".to_string(),
            object_kind: ObjectKind::WorkspaceFileV1,
            object_id: "cid_test".to_string(),
            record_id: "WorkspaceFileV1".to_string(),
            key_epoch: 1,
            format_version: 1,
        }
    }

    #[test]
    fn segmented_round_trip_crosses_chunk_boundaries() {
        let plaintext = (0..SEGMENT_PLAINTEXT_BYTES + 79)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut sealed = Vec::new();
        let stats = seal(
            &mut Cursor::new(&plaintext),
            &mut sealed,
            &StorageKey::deterministic(7),
            1,
            &context().associated_data(),
        )
        .expect("seal");
        let mut opened = Vec::new();
        let opened_stats = open(
            &mut Cursor::new(&sealed),
            &mut opened,
            &StorageKey::deterministic(7),
            1,
            &context().associated_data(),
            plaintext.len() as u64,
        )
        .expect("open");

        assert_eq!(opened, plaintext);
        assert_eq!(stats.plaintext_bytes, plaintext.len() as u64);
        assert_eq!(stats.segments, 2);
        assert_eq!(opened_stats.plaintext_bytes, plaintext.len() as u64);
    }

    #[test]
    fn segmented_seal_is_convergent() {
        let plaintext = vec![0x5a; SEGMENT_PLAINTEXT_BYTES + 1];
        let seal_once = || {
            let mut sealed = Vec::new();
            seal(
                &mut Cursor::new(&plaintext),
                &mut sealed,
                &StorageKey::deterministic(7),
                1,
                &context().associated_data(),
            )
            .expect("seal");
            sealed
        };
        assert_eq!(seal_once(), seal_once());
    }

    #[test]
    fn segmented_open_rejects_truncation_reordering_and_tampering() {
        let plaintext = vec![0x5a; SEGMENT_PLAINTEXT_BYTES + 1];
        let mut sealed = Vec::new();
        seal(
            &mut Cursor::new(&plaintext),
            &mut sealed,
            &StorageKey::deterministic(7),
            1,
            &context().associated_data(),
        )
        .expect("seal");

        for corrupt in [
            sealed[..sealed.len() - 1].to_vec(),
            {
                let mut bytes = sealed.clone();
                bytes[19] = 1;
                bytes
            },
            {
                let mut bytes = sealed.clone();
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
                bytes
            },
        ] {
            assert!(
                open(
                    &mut Cursor::new(corrupt),
                    &mut Vec::new(),
                    &StorageKey::deterministic(7),
                    1,
                    &context().associated_data(),
                    plaintext.len() as u64,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn segmented_memory_windows_do_not_grow_with_incompressible_input() {
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        let plaintext = (0..SEGMENT_PLAINTEXT_BYTES * 3 + 17)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        let mut source = MeasuringReader {
            inner: Cursor::new(plaintext.clone()),
            largest_request: 0,
        };
        let mut sealed = MeasuringWriter::default();
        seal(
            &mut source,
            &mut sealed,
            &StorageKey::deterministic(7),
            1,
            &context().associated_data(),
        )
        .expect("seal");

        let mut opened = MeasuringWriter::default();
        open(
            &mut Cursor::new(&sealed.bytes),
            &mut opened,
            &StorageKey::deterministic(7),
            1,
            &context().associated_data(),
            plaintext.len() as u64,
        )
        .expect("open");

        assert_eq!(opened.bytes, plaintext);
        assert!(source.largest_request <= SEGMENT_PLAINTEXT_BYTES);
        assert!(opened.largest_write <= SEGMENT_PLAINTEXT_BYTES);
    }
}
