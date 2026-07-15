use std::fmt;
#[cfg(test)]
use std::{error::Error, path::Path};

use bowline_core::ids::{ManifestId, PackId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::ByteStoreError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey(String);

impl ObjectKey {
    pub const SNAPSHOT_METADATA_PAGE_PREFIX: &'static str = "metadata_mp_";

    pub fn new(value: impl Into<String>) -> Result<Self, ByteStoreError> {
        let value = value.into();
        validate_opaque_object_key(&value)?;
        Ok(Self(value))
    }

    pub fn from_pack_id(pack_id: &PackId) -> Result<Self, ByteStoreError> {
        Self::new(format!("packs_{}", pack_id.as_str()))
    }

    pub fn from_manifest_id(manifest_id: &ManifestId) -> Result<Self, ByteStoreError> {
        Self::new(format!("manifests_{}", manifest_id.as_str()))
    }

    pub fn from_opaque_index_id(index_id: &str) -> Result<Self, ByteStoreError> {
        Self::new(format!("indexes_{index_id}"))
    }

    pub fn from_conflict_bundle_id(conflict_id: &str) -> Result<Self, ByteStoreError> {
        let Some(suffix) = conflict_id.strip_prefix("conflict_") else {
            return Err(ByteStoreError::InvalidObjectKey {
                key: conflict_id.to_string(),
                reason: "conflict bundle IDs must use the generated conflict prefix",
            });
        };
        Self::new(format!("conflicts_cb_{suffix}"))
    }

    pub fn new_snapshot_metadata_page() -> Result<Self, ByteStoreError> {
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).map_err(|_| ByteStoreError::InvalidObjectKey {
            key: Self::SNAPSHOT_METADATA_PAGE_PREFIX.to_string(),
            reason: "snapshot metadata page key generation failed",
        })?;
        let mut suffix = String::with_capacity(random.len() * 2);
        for byte in random {
            use fmt::Write as _;
            write!(&mut suffix, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self::new(format!("{}{suffix}", Self::SNAPSHOT_METADATA_PAGE_PREFIX))
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

    if !(matches_opaque_storage_key(key, "packs_pk_", 16)
        || matches_opaque_storage_key(key, "manifests_mf_", 16)
        || matches_opaque_storage_key(key, "indexes_ix_", 16)
        || matches_opaque_storage_key(key, "conflicts_cb_", 16)
        || matches_opaque_storage_key(key, ObjectKey::SNAPSHOT_METADATA_PAGE_PREFIX, 64))
    {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "object keys must be generated opaque pack, manifest, locator-index, overlay, or conflict-bundle keys",
        });
    }

    Ok(())
}

fn matches_opaque_storage_key(key: &str, prefix: &str, min_suffix_len: usize) -> bool {
    let Some(suffix) = key.strip_prefix(prefix) else {
        return false;
    };
    suffix.len() >= min_suffix_len
        && suffix.len() <= 80
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
    fn conflict_bundle_key_is_opaque_and_deterministic() {
        let key = ObjectKey::from_conflict_bundle_id("conflict_00112233445566778899aabb")
            .expect("conflict key");
        assert_eq!(key.as_str(), "conflicts_cb_00112233445566778899aabb");
        assert!(ObjectKey::new(key.as_str()).is_ok());
    }

    #[test]
    fn conflict_bundle_key_rejects_bad_suffixes() {
        assert!(ObjectKey::from_conflict_bundle_id("not_conflict_0011223344556677").is_err());
        assert!(ObjectKey::new("conflicts_cb_short").is_err());
        assert!(ObjectKey::new("conflicts_cb_001122334455667g").is_err());
    }

    #[test]
    fn snapshot_metadata_page_keys_are_random_opaque_physical_keys() {
        let first = ObjectKey::new_snapshot_metadata_page().expect("first metadata key");
        let second = ObjectKey::new_snapshot_metadata_page().expect("second metadata key");

        assert_ne!(first, second);
        assert!(
            first
                .as_str()
                .starts_with(ObjectKey::SNAPSHOT_METADATA_PAGE_PREFIX)
        );
        assert_eq!(
            first.as_str().len(),
            ObjectKey::SNAPSHOT_METADATA_PAGE_PREFIX.len() + 64
        );
        assert!(ObjectKey::new(first.as_str()).is_ok());
        for canary in ["src", "secret", ".env", "workspace"] {
            assert!(!first.as_str().contains(canary));
        }
    }

    #[test]
    fn snapshot_metadata_page_keys_require_full_random_suffix() {
        assert!(ObjectKey::new("metadata_mp_0011223344556677").is_err());
        assert!(ObjectKey::new(format!("metadata_mp_{}", "0".repeat(63))).is_err());
        assert!(ObjectKey::new(format!("metadata_mp_{}g", "0".repeat(63))).is_err());
    }
}
