//! The encrypted per-workspace auxiliary index (Plan 112 Step 2).
//!
//! Product features attach metadata to a workspace here and nowhere else: the
//! aux index is an ordinary sealed blob (kind `blob`, sealed exactly like a
//! workspace file) that *rides a reserved manifest entry* at [`AUX_INDEX_PATH`].
//! It therefore syncs, versions, and CASes through the same push/pull loop as
//! every other file, and the hosted service still sees only opaque objects and
//! the CAS head — never the work-view metadata inside.
//!
//! Identity and sealing follow the same two-identity rule as [`super::manifest`]
//! (Plan 108 "Object identity"): the *logical* [`ContentId`] is a
//! workspace-keyed BLAKE3 of the canonical plaintext (stable across reseals);
//! the *physical* [`BlobKey`] is `blake3(sealed_bytes)`. The index is the sole
//! sanctioned attachment point — new features extend [`AuxIndex`], never the
//! hosted schema (Plan 112 maintenance note).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use bowline_core::ids::{ContentId, DeviceId, ProjectId};
use serde::{Deserialize, Serialize};

use super::manifest::{
    BlobKey, EntryKind, FileMode, KeyEpoch, Manifest, ManifestEntry, ManifestError,
    WorkspaceCrypto, WorkspacePath, open_file, physical_blob_key, seal_file,
};
use super::push::{BlobUpload, RemoteObjects, TransportError};

/// The reserved, bowline-owned workspace path the aux index rides on. It is a
/// syncable manifest entry (deliberately NOT under `.bowline/`, which
/// [`super::super::is_private_workspace_state_path`] treats as private engine
/// state) so it flows through the ordinary manifest exactly like a user file.
pub const AUX_INDEX_PATH: &str = ".bowline-meta/aux-index";

/// Mode the aux-index entry materializes with: 0600, owner-only. The metadata is
/// not secret (opaque ids + manifest keys), but there is no reason to widen it.
const AUX_INDEX_MODE: u32 = 0o600;

/// Current aux-index plaintext format version, inside the sealed plaintext.
pub const AUX_INDEX_FORMAT_VERSION: u32 = 2;

// ---- model ------------------------------------------------------------------

/// An opaque, workspace-scoped work-view identifier. Never derived from user
/// content; assigned once at creation and stable for the view's life.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkViewId(String);

impl WorkViewId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkViewId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A work view's lifecycle, a typed enum serialized at the edge — never a string
/// literal in engine code (AGENTS code-quality rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewLifecycle {
    /// Materializable and editable; its overlay may still advance.
    Active,
    /// The overlay was three-way merged into the workspace and the view retired.
    Accepted,
    /// The overlay was dropped without merging.
    Discarded,
}

/// One work view: the base it forked from, the overlay manifest holding the
/// view's current truth, and its lifecycle state. Both keys are physical
/// manifest keys (`m_<64 hex>`); an overlay is just another manifest whose
/// entries override the base (Plan 112 model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewRecord {
    pub project_id: ProjectId,
    pub project_path: String,
    pub name: String,
    pub owner_device_id: DeviceId,
    pub created_at: String,
    pub updated_at: String,
    pub base_manifest_key: super::manifest::ManifestKey,
    pub overlay_manifest_key: super::manifest::ManifestKey,
    pub lifecycle: WorkViewLifecycle,
}

/// The whole auxiliary index: a sorted map of work views plus its format
/// version. Sorted by id so the canonical plaintext is deterministic regardless
/// of insertion order (asserted by the determinism test).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxIndex {
    pub format_version: u32,
    pub work_views: BTreeMap<WorkViewId, WorkViewRecord>,
}

impl Default for AuxIndex {
    fn default() -> Self {
        Self::empty()
    }
}

impl AuxIndex {
    pub fn empty() -> Self {
        Self {
            format_version: AUX_INDEX_FORMAT_VERSION,
            work_views: BTreeMap::new(),
        }
    }

    /// Insert or replace a work-view record.
    pub fn upsert(&mut self, id: WorkViewId, record: WorkViewRecord) {
        self.work_views.insert(id, record);
    }

    pub fn get(&self, id: &WorkViewId) -> Option<&WorkViewRecord> {
        self.work_views.get(id)
    }

    /// Deterministic canonical plaintext. Same discipline as the manifest: a
    /// `BTreeMap` fixes record order and serde fixes field order, so equal
    /// indexes serialize to equal bytes. This is the pre-seal identity input;
    /// ciphertext is never an identity.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, AuxIndexError> {
        let wire = AuxIndexWire {
            format_version: self.format_version,
            work_views: self
                .work_views
                .iter()
                .map(|(id, record)| WorkViewWire::from_record(id, record))
                .collect(),
        };
        serde_json::to_vec(&wire)
            .map_err(|_| AuxIndexError::Serialization("aux index serialization failed"))
    }
}

// ---- wire form --------------------------------------------------------------

// Work views serialize as a sorted array (not a JSON object) so decode can bound
// the record count and reject an unsorted or duplicated id before building the
// map — the same reasoning as the manifest wire form.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuxIndexWire {
    format_version: u32,
    work_views: Vec<WorkViewWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkViewWire {
    id: String,
    project_id: ProjectId,
    project_path: String,
    name: String,
    owner_device_id: DeviceId,
    created_at: String,
    updated_at: String,
    base_manifest_key: String,
    overlay_manifest_key: String,
    lifecycle: WorkViewLifecycle,
}

impl WorkViewWire {
    fn from_record(id: &WorkViewId, record: &WorkViewRecord) -> Self {
        Self {
            id: id.as_str().to_string(),
            project_id: record.project_id.clone(),
            project_path: record.project_path.clone(),
            name: record.name.clone(),
            owner_device_id: record.owner_device_id.clone(),
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            base_manifest_key: record.base_manifest_key.as_str().to_string(),
            overlay_manifest_key: record.overlay_manifest_key.as_str().to_string(),
            lifecycle: record.lifecycle,
        }
    }

    fn into_record(self) -> (WorkViewId, WorkViewRecord) {
        (
            WorkViewId::new(self.id),
            WorkViewRecord {
                project_id: self.project_id,
                project_path: self.project_path,
                name: self.name,
                owner_device_id: self.owner_device_id,
                created_at: self.created_at,
                updated_at: self.updated_at,
                base_manifest_key: super::manifest::ManifestKey::new(self.base_manifest_key),
                overlay_manifest_key: super::manifest::ManifestKey::new(self.overlay_manifest_key),
                lifecycle: self.lifecycle,
            },
        )
    }
}

// ---- bounded decode ---------------------------------------------------------

/// Decode bounds, checked before allocation where possible. Defaults are safety
/// caps far above any realistic work-view count.
#[derive(Debug, Clone, Copy)]
pub struct AuxDecodeLimits {
    pub max_sealed_bytes: u64,
    pub max_records: u64,
    pub max_id_len: u64,
}

impl Default for AuxDecodeLimits {
    fn default() -> Self {
        Self {
            max_sealed_bytes: 16 * 1024 * 1024,
            max_records: 100_000,
            max_id_len: 256,
        }
    }
}

/// Decode canonical aux-index plaintext with the same hygiene as the manifest:
/// bounded record count, strictly-increasing (so unsorted/duplicate ids are
/// rejected rather than silently deduped by a map decode), bounded id length.
pub fn decode_aux_index_plaintext(
    plaintext: &[u8],
    limits: &AuxDecodeLimits,
) -> Result<AuxIndex, AuxIndexError> {
    let wire: AuxIndexWire = serde_json::from_slice(plaintext)
        .map_err(|_| AuxIndexError::Serialization("aux index decode failed"))?;
    if wire.format_version != AUX_INDEX_FORMAT_VERSION {
        return Err(AuxIndexError::InvalidRecord {
            reason: "unsupported aux-index format version",
        });
    }
    if wire.work_views.len() as u64 > limits.max_records {
        return Err(AuxIndexError::BoundExceeded {
            bound: "record-count",
        });
    }

    let mut work_views = BTreeMap::new();
    let mut previous: Option<String> = None;
    for view in wire.work_views {
        if view.id.is_empty() {
            return Err(AuxIndexError::InvalidRecord {
                reason: "work-view id is empty",
            });
        }
        if view.project_id.as_str().is_empty()
            || view.name.is_empty()
            || view.owner_device_id.as_str().is_empty()
            || view.created_at.is_empty()
            || view.updated_at.is_empty()
        {
            return Err(AuxIndexError::InvalidRecord {
                reason: "work-view identity metadata is incomplete",
            });
        }
        if !view.project_path.is_empty()
            && (view.project_path.starts_with('/')
                || view.project_path.contains('\\')
                || view
                    .project_path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == ".."))
        {
            return Err(AuxIndexError::InvalidRecord {
                reason: "work-view project path is not normalized-relative",
            });
        }
        if view.id.len() as u64 > limits.max_id_len {
            return Err(AuxIndexError::BoundExceeded { bound: "id-length" });
        }
        match &previous {
            Some(prev) if prev.as_str() >= view.id.as_str() => {
                return Err(if prev.as_str() == view.id.as_str() {
                    AuxIndexError::DuplicateId
                } else {
                    AuxIndexError::NotSorted
                });
            }
            _ => {}
        }
        previous = Some(view.id.clone());
        let (id, record) = view.into_record();
        work_views.insert(id, record);
    }

    Ok(AuxIndex {
        format_version: wire.format_version,
        work_views,
    })
}

// ---- sealing boundary + manifest ride ---------------------------------------

/// A sealed aux index ready to reference from a manifest: the sealed bytes plus
/// the identities the manifest entry needs.
pub struct SealedAuxIndex {
    pub content_id: ContentId,
    pub blob_key: BlobKey,
    pub key_epoch: KeyEpoch,
    pub size: u64,
    pub sealed: Vec<u8>,
}

impl SealedAuxIndex {
    /// The manifest entry that references this sealed index. Inserting it at
    /// [`AUX_INDEX_PATH`] is how the index "rides a manifest entry" and thereby
    /// syncs with the workspace (Plan 112 Step 2 verify).
    pub fn manifest_entry(&self) -> ManifestEntry {
        ManifestEntry::File {
            size: self.size,
            mode: FileMode::new(AUX_INDEX_MODE),
            content_id: self.content_id.clone(),
            blob_key: self.blob_key.clone(),
            key_epoch: self.key_epoch,
        }
    }

    /// The reserved workspace path the entry lives at.
    pub fn manifest_path() -> WorkspacePath {
        WorkspacePath::new(AUX_INDEX_PATH)
    }
}

/// Seal an aux index into a blob-identity bundle. The content id is the logical
/// workspace-keyed identity (same domain as a file); the physical key derives
/// from the sealed bytes.
pub fn seal_aux_index(
    crypto: &WorkspaceCrypto,
    aux: &AuxIndex,
) -> Result<SealedAuxIndex, AuxIndexError> {
    let plaintext = aux.to_canonical_bytes()?;
    let content_id = crypto.content_id(&plaintext);
    let sealed = seal_file(crypto, &content_id, &plaintext).map_err(AuxIndexError::Seal)?;
    let blob_key = physical_blob_key(sealed.as_bytes());
    Ok(SealedAuxIndex {
        content_id,
        blob_key,
        key_epoch: crypto.key_epoch(),
        size: plaintext.len() as u64,
        sealed: sealed.into_bytes(),
    })
}

/// Open + decode a sealed aux index, verifying its logical content id
/// (defense in depth atop the AEAD binding, via [`open_file`]).
pub fn open_aux_index(
    crypto: &WorkspaceCrypto,
    expected_content_id: &ContentId,
    sealed: &[u8],
    limits: &AuxDecodeLimits,
) -> Result<AuxIndex, AuxIndexError> {
    if sealed.len() as u64 > limits.max_sealed_bytes {
        return Err(AuxIndexError::BoundExceeded {
            bound: "sealed-size",
        });
    }
    let plaintext = open_file(crypto, expected_content_id, sealed).map_err(AuxIndexError::Seal)?;
    decode_aux_index_plaintext(&plaintext, limits)
}

/// Find the aux-index entry in a manifest, returning its identity if present and
/// a valid file entry. A non-file entry at the reserved path is corruption.
/// (The parameter binds to a non-`*manifest` name for the same reason as
/// `decide_head`'s `head_snapshot`: the architecture gate reserves that
/// spelling of entry access for the deleted old-engine authority.)
pub fn aux_index_pointer(
    snapshot: &Manifest,
) -> Result<Option<(ContentId, BlobKey)>, AuxIndexError> {
    let path = WorkspacePath::new(AUX_INDEX_PATH);
    match snapshot.entries.get(&path) {
        None => Ok(None),
        Some(ManifestEntry::File {
            content_id,
            blob_key,
            ..
        }) => Ok(Some((content_id.clone(), blob_key.clone()))),
        Some(other) => Err(AuxIndexError::WrongEntryKind {
            found: other.kind(),
        }),
    }
}

// ---- transport helpers (upload / load through the object store) -------------

/// Seal + upload the aux index and return the manifest entry that references it.
/// The caller inserts that entry at [`AUX_INDEX_PATH`] into the candidate
/// manifest and pushes it — an ordinary create-only blob PUT, no bespoke path.
pub fn upload_aux_index<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    aux: &AuxIndex,
) -> Result<(WorkspacePath, ManifestEntry), AuxIndexError> {
    let sealed = seal_aux_index(crypto, aux)?;
    objects
        .put_blob(BlobUpload {
            key: &sealed.blob_key,
            content_id: &sealed.content_id,
            key_epoch: sealed.key_epoch,
            sealed: &sealed.sealed,
        })
        .map_err(AuxIndexError::Transport)?;
    Ok((SealedAuxIndex::manifest_path(), sealed.manifest_entry()))
}

/// Load the aux index referenced by a manifest, if any. Fetches the sealed blob,
/// re-verifies its physical key, then opens + decodes it.
pub fn load_aux_index<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    snapshot: &Manifest,
    limits: &AuxDecodeLimits,
) -> Result<Option<AuxIndex>, AuxIndexError> {
    let Some((content_id, blob_key)) = aux_index_pointer(snapshot)? else {
        return Ok(None);
    };
    let sealed = objects
        .get_blob(&blob_key)
        .map_err(AuxIndexError::Transport)?;
    if physical_blob_key(&sealed) != blob_key {
        return Err(AuxIndexError::BlobKeyMismatch);
    }
    let aux = open_aux_index(crypto, &content_id, &sealed, limits)?;
    Ok(Some(aux))
}

/// The set of blob/manifest keys the live (non-discarded) work views reference,
/// so a future liveness sweep (Plan 108 deferred GC) can prove them reachable.
/// Kept here because the aux index is the single owner of that knowledge.
pub fn live_manifest_keys(aux: &AuxIndex) -> BTreeSet<super::manifest::ManifestKey> {
    let mut keys = BTreeSet::new();
    for record in aux.work_views.values() {
        if record.lifecycle == WorkViewLifecycle::Discarded {
            continue;
        }
        keys.insert(record.base_manifest_key.clone());
        keys.insert(record.overlay_manifest_key.clone());
    }
    keys
}

// ---- errors -----------------------------------------------------------------

#[derive(Debug)]
pub enum AuxIndexError {
    Serialization(&'static str),
    Seal(ManifestError),
    Transport(TransportError),
    BoundExceeded { bound: &'static str },
    InvalidRecord { reason: &'static str },
    WrongEntryKind { found: EntryKind },
    BlobKeyMismatch,
    NotSorted,
    DuplicateId,
}

impl fmt::Display for AuxIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialization(reason) => write!(formatter, "aux index serialization: {reason}"),
            Self::Seal(error) => write!(formatter, "aux index seal/open failed: {error}"),
            Self::Transport(error) => write!(formatter, "aux index {error}"),
            Self::BoundExceeded { bound } => {
                write!(formatter, "aux index decode bound exceeded: {bound}")
            }
            Self::InvalidRecord { reason } => {
                write!(formatter, "invalid aux index record: {reason}")
            }
            Self::WrongEntryKind { found } => {
                write!(formatter, "aux index entry is not a file: {found:?}")
            }
            Self::BlobKeyMismatch => {
                formatter.write_str("aux index blob key does not match its manifest entry")
            }
            Self::NotSorted => formatter.write_str("aux index records are not canonically sorted"),
            Self::DuplicateId => formatter.write_str("aux index contains a duplicate work-view id"),
        }
    }
}

impl Error for AuxIndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Seal(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "aux_index/tests.rs"]
mod tests;
