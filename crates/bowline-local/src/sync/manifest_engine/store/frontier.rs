//! Typed durable engine frontier stored in the singleton state row.

use super::{ManifestKey, ManifestStoreError};
use crate::sync::manifest_engine::{EngineRef, RefObservation};

/// The typed singleton engine-state row. Absent refs denote Genesis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineState {
    pub applied_manifest_key: Option<ManifestKey>,
    pub last_ref_version: Option<u64>,
    pub materialization_revision: MaterializationRevision,
    pub highest_verified_ref_version: Option<u64>,
    pub highest_verified_manifest_key: Option<ManifestKey>,
}

impl EngineState {
    /// Checked structural view of the durable applied frontier.
    pub fn applied_ref(&self) -> Result<EngineRef, ManifestStoreError> {
        match (&self.applied_manifest_key, self.last_ref_version) {
            (None, None) => Ok(EngineRef::Genesis),
            (Some(manifest_key), Some(version)) => Ok(EngineRef::Head(RefObservation {
                version,
                manifest_key: manifest_key.clone(),
            })),
            (None, Some(_)) | (Some(_), None) => Err(ManifestStoreError::Corrupt {
                field: "applied_ref",
            }),
        }
    }
}

/// Durable revision of the local materialized/applied-ref frontier.
///
/// This is distinct from the in-memory engine revision and moves only in the
/// SQLite transaction that commits materialization or an applied ref.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaterializationRevision(u64);

impl MaterializationRevision {
    pub const INITIAL: Self = Self(0);

    pub(crate) const fn from_stored(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
