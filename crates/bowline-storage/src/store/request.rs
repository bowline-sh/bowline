use std::io::Read;

use bowline_core::ids::{DeviceId, PackId};
use serde::{Deserialize, Serialize};

use super::{ByteStoreError, ObjectKey, ObjectKind};

pub(super) fn read_verified_source(
    key: &ObjectKey,
    reader: &mut dyn Read,
    expected_len: u64,
    expected_hash: &str,
) -> Result<Vec<u8>, ByteStoreError> {
    let initial_capacity = usize::try_from(expected_len.min(64 * 1024)).unwrap_or(64 * 1024);
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
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
    let actual_hash = format!("b3_{}", hasher.finalize().to_hex());
    if byte_len != expected_len || actual_hash != expected_hash {
        return Err(ByteStoreError::CorruptObject {
            key: key.clone(),
            reason: "streamed object source did not match requested identity",
        });
    }
    Ok(bytes)
}

pub struct PutObjectReaderRequest<'a> {
    pub key: ObjectKey,
    pub kind: ObjectKind,
    pub content_id: ObjectContentId,
    pub source: &'a dyn ReopenableObjectSource,
    pub byte_len: u64,
    pub expected_hash: ObjectHash,
    pub key_epoch: u32,
    pub created_by_device_id: Option<&'a DeviceId>,
}

pub trait ReopenableObjectSource {
    fn open(&self) -> std::io::Result<Box<dyn Read + Send>>;
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectHash(String);

impl ObjectHash {
    pub fn from_stable_hash(hash: String) -> Self {
        Self(hash)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectContentId(String);

impl ObjectContentId {
    pub fn from_pack_id(pack_id: &PackId) -> Self {
        Self(pack_id.as_str().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
