use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bowline_core::devices::DeviceFingerprint;

use super::DeviceKeyError;

pub(super) fn fingerprint_for_public_key(public_key: &str) -> DeviceFingerprint {
    let hash = blake3::hash(public_key.as_bytes());
    DeviceFingerprint::new(format!("fp_{}", &hash.to_hex()[..16]))
}

pub(super) fn decode_signing_seed(value: &str) -> Result<[u8; 32], DeviceKeyError> {
    BASE64
        .decode(value)
        .map_err(|error| DeviceKeyError::CorruptSecret(error.to_string()))?
        .try_into()
        .map_err(|_| DeviceKeyError::CorruptSecret("signing seed must be 32 bytes".to_string()))
}
