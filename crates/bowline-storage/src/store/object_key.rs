use std::fmt;
#[cfg(test)]
use std::{error::Error, path::Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::ByteStoreError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey(String);

impl ObjectKey {
    /// Manifest-sync file blob prefix; the 64-hex suffix is `blake3(sealed)`.
    pub const BLOB_PREFIX: &'static str = "b_";
    /// Manifest-sync workspace manifest prefix; the 64-hex suffix is
    /// `blake3(sealed)`. The hosted CAS ref's `snapshotId` is this key.
    pub const MANIFEST_PREFIX: &'static str = "m_";

    pub fn new(value: impl Into<String>) -> Result<Self, ByteStoreError> {
        let value = value.into();
        validate_opaque_object_key(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ObjectKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ObjectKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

fn validate_opaque_object_key(key: &str) -> Result<(), ByteStoreError> {
    if key.is_empty() {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "empty keys are not allowed",
        });
    }
    if key.len() > 180 {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "key is too long for the local storage contract",
        });
    }
    if key.contains('/') || key.contains('\\') || key.contains('.') {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "path separators and dotted names are not allowed",
        });
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "only ASCII alphanumeric, dash, and underscore are allowed",
        });
    }

    // Manifest-sync engine keys (Plan 110): the key is the sealed-bytes hash,
    // so the 64-hex suffix is exact — `b_` file blob, `m_` workspace manifest.
    if !(matches_sealed_hash_key(key, ObjectKey::BLOB_PREFIX)
        || matches_sealed_hash_key(key, ObjectKey::MANIFEST_PREFIX))
    {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "object keys must be sealed-hash b_/m_ keys",
        });
    }

    Ok(())
}

/// A manifest-sync key is exactly `<prefix><64 lowercase hex>`: the suffix is the
/// full BLAKE3 of the sealed bytes, so unlike the former opaque generated keys it
/// has no length tolerance — the key must equal the sealed hash.
fn matches_sealed_hash_key(key: &str, prefix: &str) -> bool {
    let Some(suffix) = key.strip_prefix(prefix) else {
        return false;
    };
    suffix.len() == 64
        && suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
pub(crate) struct ObjectKeyLeak {
    pub object_key: String,
    pub leaked_segment: String,
}

#[cfg(test)]
impl fmt::Display for ObjectKeyLeak {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "object key `{}` leaks path segment `{}`",
            self.object_key, self.leaked_segment
        )
    }
}

#[cfg(test)]
impl Error for ObjectKeyLeak {}

#[cfg(test)]
pub(crate) fn assert_object_key_does_not_leak_path(
    object_key: &str,
    source_path: impl AsRef<Path>,
) -> Result<(), ObjectKeyLeak> {
    for component in source_path.as_ref().components() {
        let segment = component.as_os_str().to_string_lossy();
        if segment.len() >= 3 && object_key.contains(segment.as_ref()) {
            return Err(ObjectKeyLeak {
                object_key: object_key.to_string(),
                leaked_segment: segment.into_owned(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_accepts_b_and_m_prefixes() {
        let blob = format!("{}{}", ObjectKey::BLOB_PREFIX, "a".repeat(64));
        let manifest = format!("{}{}", ObjectKey::MANIFEST_PREFIX, "b".repeat(64));
        assert_eq!(
            ObjectKey::new(blob.clone()).expect("blob key").as_str(),
            blob
        );
        assert_eq!(
            ObjectKey::new(manifest.clone())
                .expect("manifest key")
                .as_str(),
            manifest
        );

        // The suffix is the sealed hash, so it is exact: 63 or 65 hex, a non-hex
        // digit, or a wrong prefix length are all rejected.
        assert!(ObjectKey::new(format!("b_{}", "a".repeat(63))).is_err());
        assert!(ObjectKey::new(format!("m_{}", "a".repeat(65))).is_err());
        assert!(ObjectKey::new(format!("b_{}g", "a".repeat(63))).is_err());
    }

    #[test]
    fn object_key_rejects_retired_pack_engine_prefixes() {
        for key in [
            "legacy_pk_0011223344556677",
            "manifests_mf_0011223344556677",
            "indexes_ix_0011223344556677",
            "conflicts_cb_0011223344556677",
            &format!("metadata_mp_{}", "0".repeat(64)),
        ] {
            assert!(ObjectKey::new(key.to_string()).is_err(), "key: {key}");
        }
    }
}
