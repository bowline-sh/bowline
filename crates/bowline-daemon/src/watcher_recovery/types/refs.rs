use bowline_local::sync::manifest_engine::ManifestKey;
use serde::Serialize;

use super::{MAX_SCHEMA_INTEGER, RecoveryTypeError};

/// The exact authoritative ref identity shared by publication, observation,
/// application, and the public workspace barrier.
///
/// `ManifestKey` is already a content-addressed opaque key. Deriving a second
/// keyed diagnostic identity would create a competing equality domain and
/// secret-key lifecycle without adding privacy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthoritativeRefIdentity {
    Genesis,
    Head {
        version: u64,
        #[serde(rename = "manifestKey")]
        manifest_key: ManifestKey,
    },
}

impl AuthoritativeRefIdentity {
    pub const fn genesis() -> Self {
        Self::Genesis
    }

    pub fn head(version: u64, manifest_key: ManifestKey) -> Result<Self, RecoveryTypeError> {
        validate_ref_version(version)?;
        validate_manifest_key(&manifest_key)?;
        Ok(Self::Head {
            version,
            manifest_key,
        })
    }

    pub const fn version(&self) -> Option<u64> {
        match self {
            Self::Genesis => None,
            Self::Head { version, .. } => Some(*version),
        }
    }

    pub const fn manifest_key(&self) -> Option<&ManifestKey> {
        match self {
            Self::Genesis => None,
            Self::Head { manifest_key, .. } => Some(manifest_key),
        }
    }
}

/// One observer or engine statement about the authoritative workspace ref.
/// The wrapper prevents callers from substituting unrelated hash-like values
/// while keeping the exact identity available for equality and receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObservation {
    identity: AuthoritativeRefIdentity,
}

impl RefObservation {
    pub fn new(identity: AuthoritativeRefIdentity) -> Self {
        Self { identity }
    }

    pub fn identity(&self) -> &AuthoritativeRefIdentity {
        &self.identity
    }
}

fn validate_manifest_key(manifest_key: &ManifestKey) -> Result<(), RecoveryTypeError> {
    let Some(digest) = manifest_key.as_str().strip_prefix("m_") else {
        return Err(RecoveryTypeError::InvalidManifestKey);
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RecoveryTypeError::InvalidManifestKey);
    }
    Ok(())
}

fn validate_ref_version(version: u64) -> Result<(), RecoveryTypeError> {
    if version == 0 {
        return Err(RecoveryTypeError::HeadVersionMustBePositive);
    }
    if version > MAX_SCHEMA_INTEGER {
        return Err(RecoveryTypeError::SchemaIntegerOutOfRange {
            field: "authoritativeRef.version",
            value: version,
        });
    }
    Ok(())
}
