use std::io::{self, Read};

use bowline_core::ids::DeviceId;
use serde::{Deserialize, Serialize};

use super::{ByteStoreError, ObjectKey, ObjectKind};

const OBJECT_READ_BUFFER_BYTES: usize = 64 * 1024;

fn read_verified_source(
    key: &ObjectKey,
    reader: &mut dyn Read,
    expected_len: u64,
    expected_hash: &ObjectHash,
) -> Result<Vec<u8>, ByteStoreError> {
    let initial_capacity = usize::try_from(expected_len.min(OBJECT_READ_BUFFER_BYTES as u64))
        .unwrap_or(OBJECT_READ_BUFFER_BYTES);
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; OBJECT_READ_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        byte_len = byte_len
            .checked_add(read as u64)
            .ok_or(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "streamed object source length overflowed",
            })?;
        if byte_len > expected_len {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "streamed object source exceeded requested length",
            });
        }
        hasher.update(&buffer[..read]);
        bytes.extend_from_slice(&buffer[..read]);
    }
    if byte_len != expected_len || ObjectHash::from_hasher(&hasher) != *expected_hash {
        return Err(ByteStoreError::CorruptObject {
            key: key.clone(),
            reason: "streamed object source did not match requested identity",
        });
    }
    Ok(bytes)
}

/// Everything a backend needs to store one object, with no field a backend can
/// quietly ignore.
///
/// One request type rather than a family of overlapping `put_*` methods: the
/// defaulted variants used to drop the caller's content id, key epoch, or
/// streaming property depending on which entry point was reached, and a backend
/// that forgot an override lost those guarantees with no compile error.
pub struct PutObjectRequest<'a> {
    pub key: ObjectKey,
    pub kind: ObjectKind,
    pub content_id: ObjectContentId,
    pub source: PutObjectSource<'a>,
    pub byte_len: u64,
    pub expected_hash: ObjectHash,
    pub key_epoch: u32,
    pub created_by_device_id: Option<&'a DeviceId>,
}

/// Where an object's bytes come from.
///
/// A reader source must be reopenable because a backend may need a second pass
/// (checksum, then upload) and because a failed conditional write is retried.
#[derive(Clone, Copy)]
pub enum PutObjectSource<'a> {
    Bytes(&'a [u8]),
    Reader(&'a dyn ReopenableObjectSource),
}

impl PutObjectRequest<'_> {
    /// Reads the whole source into memory, verifying it against the requested
    /// byte length and hash first.
    ///
    /// Only for a backend that genuinely cannot stream: it costs `byte_len` of
    /// resident memory, which for this product can be a multi-gigabyte file.
    pub fn read_verified_bytes(&self) -> Result<Vec<u8>, ByteStoreError> {
        match self.source {
            PutObjectSource::Bytes(bytes) => {
                if bytes.len() as u64 != self.byte_len
                    || ObjectHash::of_bytes(bytes) != self.expected_hash
                {
                    return Err(ByteStoreError::CorruptObject {
                        key: self.key.clone(),
                        reason: "object source did not match requested identity",
                    });
                }
                Ok(bytes.to_vec())
            }
            PutObjectSource::Reader(source) => read_verified_source(
                &self.key,
                source.open()?.as_mut(),
                self.byte_len,
                &self.expected_hash,
            ),
        }
    }
}

pub trait ReopenableObjectSource {
    fn open(&self) -> io::Result<Box<dyn Read + Send>>;
}

/// A stored object's content hash, always `b3_<64 lowercase hex>`.
///
/// The prefix is written in exactly one place — `from_hasher` — because every
/// producer's output is compared against `ObjectMetadata::hash` for integrity,
/// and a drift in any one of them would surface to the user as `CorruptObject`
/// on data that is perfectly intact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectHash(String);

impl ObjectHash {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(bytes);
        Self::from_hasher(&hasher)
    }

    pub fn of_reader(reader: &mut dyn Read) -> io::Result<Self> {
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; OBJECT_READ_BUFFER_BYTES];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Self::from_hasher(&hasher))
    }

    pub(super) fn from_hasher(hasher: &blake3::Hasher) -> Self {
        Self(format!("b3_{}", hasher.finalize().to_hex()))
    }

    pub fn from_stable_hash(hash: String) -> Self {
        Self(hash)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectContentId(String);

impl ObjectContentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
