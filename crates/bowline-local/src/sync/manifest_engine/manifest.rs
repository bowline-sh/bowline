//! Canonical workspace manifest, logical/physical identity, and the sealed
//! encode/decode boundary for the manifest-sync engine (Plan 109 Step 3).
//!
//! Two identities live here and are never conflated (Plan 108 "Object
//! identity"): the *logical* [`ContentId`] / manifest content id is a
//! workspace-keyed, domain-separated BLAKE3 of canonical plaintext and is
//! stable across reseals; the *physical* [`BlobKey`] / [`ManifestKey`] is
//! `blake3(sealed_bytes)` and changes every reseal because [`seal`] uses a
//! random nonce. Canonical serialization is asserted deterministic on
//! plaintext, never on ciphertext.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;

use bowline_core::ids::ContentId;
use bowline_core::workspace_graph::normalize_workspace_path;
use bowline_storage::{
    EnvelopeContext, EnvelopeError, ObjectKind, SealedEnvelope, StorageKey, open, seal,
    workspace_id_hash,
};
use serde::{Deserialize, Serialize};

use crate::policy::is_private_workspace_state_path;

/// Envelope AEAD format version for engine objects. Distinct from
/// [`Manifest::format_version`], which lives inside the sealed plaintext; this
/// value binds the wire framing as associated data so a future framing change
/// cannot be opened under the old context.
const ENVELOPE_FORMAT_VERSION: u16 = 1;

/// Domain separators keep a file's logical id disjoint from a manifest's even
/// when their plaintext bytes coincide. Length-prefixed on hash so no content
/// can forge a domain boundary.
const FILE_CONTENT_DOMAIN: &[u8] = b"bowline/workspace-file/v1";
const MANIFEST_CONTENT_DOMAIN: &[u8] = b"bowline/workspace-manifest/v1";

/// A normalized, workspace-relative path. Canonicalized on construction via the
/// shared [`normalize_workspace_path`]; the manifest never carries an
/// absolute, `..`-bearing, or private-state path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Physical key of a sealed file blob (`b_<64 hex>` of the sealed bytes).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobKey(String);

impl BlobKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Physical key of a sealed manifest blob (`m_<64 hex>` of the sealed bytes).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManifestKey(String);

impl ManifestKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Workspace key epoch. A numeric newtype so an epoch cannot be swapped for an
/// arbitrary integer at a call boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyEpoch(u32);

impl KeyEpoch {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// POSIX-style file mode bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileMode(u32);

impl FileMode {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// Typed entry kind, serialized at the wire edge — never a string literal in
/// engine code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// One manifest entry. Empty directories are represented; symlinks carry their
/// target verbatim and are never followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestEntry {
    File {
        size: u64,
        mode: FileMode,
        content_id: ContentId,
        blob_key: BlobKey,
        key_epoch: KeyEpoch,
    },
    Directory {
        mode: FileMode,
    },
    Symlink {
        mode: FileMode,
        target: String,
    },
}

impl ManifestEntry {
    pub fn kind(&self) -> EntryKind {
        match self {
            Self::File { .. } => EntryKind::File,
            Self::Directory { .. } => EntryKind::Directory,
            Self::Symlink { .. } => EntryKind::Symlink,
        }
    }
}

/// The portable truth: a sorted map of paths to entries plus the format and key
/// epoch it was produced under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub format_version: u32,
    pub key_epoch: KeyEpoch,
    pub entries: BTreeMap<WorkspacePath, ManifestEntry>,
}

/// The current canonical manifest format version (inside the sealed plaintext).
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

impl Manifest {
    pub fn new(key_epoch: KeyEpoch, entries: BTreeMap<WorkspacePath, ManifestEntry>) -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            key_epoch,
            entries,
        }
    }

    /// Deterministic canonical plaintext. The [`BTreeMap`] fixes entry order and
    /// serde fixes field order, so equal manifests serialize to equal bytes
    /// regardless of insertion order (asserted by the determinism property
    /// test). This is the pre-seal identity input; ciphertext is never used for
    /// identity.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        let wire = ManifestWire {
            format_version: self.format_version,
            key_epoch: self.key_epoch.get(),
            entries: self
                .entries
                .iter()
                .map(|(path, entry)| ManifestEntryWire::from_entry(path, entry))
                .collect(),
        };
        serde_json::to_vec(&wire)
            .map_err(|_| ManifestError::Serialization("manifest serialization failed"))
    }
}

// ---- wire form -----------------------------------------------------------

// Entries serialize as a sorted array rather than a JSON object so decode can
// bound the record count, reject a non-sorted or duplicated path, and validate
// each path before building the in-memory map — none of which a map decode into
// a `BTreeMap` (which silently dedups) could do.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestWire {
    format_version: u32,
    key_epoch: u32,
    entries: Vec<ManifestEntryWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestEntryWire {
    path: String,
    kind: EntryKind,
    mode: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    content_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    blob_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    key_epoch: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    symlink_target: Option<String>,
}

impl ManifestEntryWire {
    fn from_entry(path: &WorkspacePath, entry: &ManifestEntry) -> Self {
        let mut wire = Self {
            path: path.as_str().to_string(),
            kind: entry.kind(),
            mode: 0,
            size: None,
            content_id: None,
            blob_key: None,
            key_epoch: None,
            symlink_target: None,
        };
        match entry {
            ManifestEntry::File {
                size,
                mode,
                content_id,
                blob_key,
                key_epoch,
            } => {
                wire.mode = mode.get();
                wire.size = Some(*size);
                wire.content_id = Some(content_id.as_str().to_string());
                wire.blob_key = Some(blob_key.as_str().to_string());
                wire.key_epoch = Some(key_epoch.get());
            }
            ManifestEntry::Directory { mode } => {
                wire.mode = mode.get();
            }
            ManifestEntry::Symlink { mode, target } => {
                wire.mode = mode.get();
                wire.symlink_target = Some(target.clone());
            }
        }
        wire
    }

    fn into_entry(self) -> Result<ManifestEntry, ManifestError> {
        let mode = FileMode::new(self.mode);
        match self.kind {
            EntryKind::File => {
                let size = self.size.ok_or(ManifestError::InvalidEntry {
                    reason: "file entry missing size",
                })?;
                let content_id = self.content_id.ok_or(ManifestError::InvalidEntry {
                    reason: "file entry missing content id",
                })?;
                let blob_key = self.blob_key.ok_or(ManifestError::InvalidEntry {
                    reason: "file entry missing blob key",
                })?;
                let key_epoch = self.key_epoch.ok_or(ManifestError::InvalidEntry {
                    reason: "file entry missing key epoch",
                })?;
                Ok(ManifestEntry::File {
                    size,
                    mode,
                    content_id: ContentId::new(content_id),
                    blob_key: BlobKey::new(blob_key),
                    key_epoch: KeyEpoch::new(key_epoch),
                })
            }
            EntryKind::Directory => Ok(ManifestEntry::Directory { mode }),
            EntryKind::Symlink => {
                let target = self.symlink_target.ok_or(ManifestError::InvalidEntry {
                    reason: "symlink entry missing target",
                })?;
                Ok(ManifestEntry::Symlink { mode, target })
            }
        }
    }
}

// ---- logical identity ----------------------------------------------------

fn keyed_content_id(
    workspace_key: [u8; 32],
    domain: &[u8],
    plaintext: &[u8],
    prefix: &str,
) -> ContentId {
    let mut hasher = blake3::Hasher::new_keyed(&workspace_key);
    hasher.update(&(domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update(plaintext);
    ContentId::new(format!("{prefix}_{}", hasher.finalize().to_hex()))
}

/// Workspace-keyed logical identity of a file's plaintext. Precedent:
/// `crates/bowline-core/src/workspace_graph.rs:490`.
pub fn content_id(workspace_key: [u8; 32], plaintext: &[u8]) -> ContentId {
    keyed_content_id(workspace_key, FILE_CONTENT_DOMAIN, plaintext, "cid")
}

/// Workspace-keyed logical identity of a manifest's canonical plaintext.
pub fn manifest_content_id(workspace_key: [u8; 32], plaintext: &[u8]) -> ContentId {
    keyed_content_id(workspace_key, MANIFEST_CONTENT_DOMAIN, plaintext, "mcid")
}

/// Physical key of a sealed file blob. Derives from the sealed bytes, so a
/// create-only PUT that collides on a different byte string is corruption, not
/// a recoverable reseal (Plan 108).
pub fn physical_blob_key(sealed: &[u8]) -> BlobKey {
    BlobKey::new(format!("b_{}", blake3::hash(sealed).to_hex()))
}

/// Physical key of a sealed manifest blob.
pub fn physical_manifest_key(sealed: &[u8]) -> ManifestKey {
    ManifestKey::new(format!("m_{}", blake3::hash(sealed).to_hex()))
}

// ---- sealing boundary ----------------------------------------------------

/// The purpose bound into the envelope AEAD context, keeping a file blob from
/// ever opening as a manifest and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopePurpose {
    WorkspaceFileV1,
    WorkspaceManifestV1,
}

impl EnvelopePurpose {
    fn as_aad(self) -> &'static str {
        match self {
            Self::WorkspaceFileV1 => "WorkspaceFileV1",
            Self::WorkspaceManifestV1 => "WorkspaceManifestV1",
        }
    }

    fn object_kind(self) -> ObjectKind {
        match self {
            Self::WorkspaceFileV1 => ObjectKind::WorkspaceFileV1,
            Self::WorkspaceManifestV1 => ObjectKind::WorkspaceManifestV1,
        }
    }
}

/// Workspace crypto material for a single key epoch: the raw content key (for
/// keyed BLAKE3 identity) plus everything needed to build an envelope context.
#[derive(Clone)]
pub struct WorkspaceCrypto {
    workspace_id_hash: String,
    key_bytes: [u8; 32],
    key_epoch: KeyEpoch,
}

impl WorkspaceCrypto {
    pub fn new(workspace_id: &str, key_bytes: [u8; 32], key_epoch: KeyEpoch) -> Self {
        Self {
            workspace_id_hash: workspace_id_hash(workspace_id),
            key_bytes,
            key_epoch,
        }
    }

    pub fn key_epoch(&self) -> KeyEpoch {
        self.key_epoch
    }

    pub fn content_id(&self, plaintext: &[u8]) -> ContentId {
        content_id(self.key_bytes, plaintext)
    }

    pub fn manifest_content_id(&self, plaintext: &[u8]) -> ContentId {
        manifest_content_id(self.key_bytes, plaintext)
    }

    fn storage_key(&self) -> StorageKey {
        StorageKey::from_bytes(self.key_bytes)
    }

    fn file_context(&self, content_id: &ContentId, format_version: u16) -> EnvelopeContext {
        EnvelopeContext {
            workspace_id_hash: self.workspace_id_hash.clone(),
            object_kind: EnvelopePurpose::WorkspaceFileV1.object_kind(),
            object_id: content_id.as_str().to_string(),
            record_id: EnvelopePurpose::WorkspaceFileV1.as_aad().to_string(),
            key_epoch: self.key_epoch.get(),
            format_version,
        }
    }

    // The manifest content id is unknowable before opening (it is derived from
    // the very plaintext being opened), so unlike a file the manifest context
    // does not bind it. The physical key `m_<blake3(sealed)>` already pins the
    // exact bytes, and the caller re-derives `manifest_content_id` post-open for
    // its records; workspace, purpose, epoch, and format still bind here.
    fn manifest_context(&self, format_version: u16) -> EnvelopeContext {
        EnvelopeContext {
            workspace_id_hash: self.workspace_id_hash.clone(),
            object_kind: EnvelopePurpose::WorkspaceManifestV1.object_kind(),
            object_id: EnvelopePurpose::WorkspaceManifestV1.as_aad().to_string(),
            record_id: EnvelopePurpose::WorkspaceManifestV1.as_aad().to_string(),
            key_epoch: self.key_epoch.get(),
            format_version,
        }
    }
}

/// Seal a file plaintext under its logical content id.
pub fn seal_file(
    crypto: &WorkspaceCrypto,
    content_id: &ContentId,
    plaintext: &[u8],
) -> Result<SealedEnvelope, ManifestError> {
    let context = crypto.file_context(content_id, ENVELOPE_FORMAT_VERSION);
    seal(plaintext, crypto.storage_key(), &context).map_err(ManifestError::Envelope)
}

/// Open a sealed file blob, verifying the recovered plaintext hashes back to the
/// expected content id (defense in depth atop the AEAD binding).
pub fn open_file(
    crypto: &WorkspaceCrypto,
    expected_content_id: &ContentId,
    sealed: &[u8],
) -> Result<Vec<u8>, ManifestError> {
    let context = crypto.file_context(expected_content_id, ENVELOPE_FORMAT_VERSION);
    let plaintext =
        open(sealed, crypto.storage_key(), &context).map_err(ManifestError::Envelope)?;
    if &crypto.content_id(&plaintext) != expected_content_id {
        return Err(ManifestError::ContentIdMismatch);
    }
    Ok(plaintext)
}

/// Seal a manifest's canonical plaintext.
pub fn seal_manifest(
    crypto: &WorkspaceCrypto,
    plaintext: &[u8],
) -> Result<SealedEnvelope, ManifestError> {
    let context = crypto.manifest_context(ENVELOPE_FORMAT_VERSION);
    seal(plaintext, crypto.storage_key(), &context).map_err(ManifestError::Envelope)
}

// ---- bounded decode ------------------------------------------------------

/// Decode bounds checked at two distinct points. `max_sealed_bytes` is the
/// pre-allocation guard: it is checked on the ciphertext BEFORE [`open`], so a
/// hostile blob cannot force a large plaintext allocation. `max_decoded_bytes`
/// is enforced POST-decompress — on the plaintext returned by [`open`] — so it is
/// AEAD-gated: only a blob that already authenticated under the workspace key can
/// reach it, and it never trusts an attacker-declared size. `max_records` /
/// `max_path_len` / `max_aggregate_declared_bytes` bound the structured entry map
/// during decode. Defaults are safety caps far above the certified 10k/100k
/// fixtures — real growth is met by manifest chunking (Plan 108 trigger: sealed
/// manifest > 16 MB) long before these hard caps.
#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    pub max_sealed_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_records: u64,
    pub max_path_len: u64,
    pub max_aggregate_declared_bytes: u64,
    allow_workspace_state_paths: bool,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_sealed_bytes: 128 * 1024 * 1024,
            max_decoded_bytes: 512 * 1024 * 1024,
            max_records: 2_000_000,
            max_path_len: 4096,
            max_aggregate_declared_bytes: 8 * 1024 * 1024 * 1024 * 1024,
            allow_workspace_state_paths: false,
        }
    }
}

impl DecodeLimits {
    /// Decode a project-scoped work-view manifest. Its root is the project, not
    /// the Bowline workspace, so project-owned `.bowline`, `.bowline-meta`, and
    /// `.work` paths are ordinary content rather than private workspace state.
    pub fn project_view() -> Self {
        Self {
            allow_workspace_state_paths: true,
            ..Self::default()
        }
    }
}

/// A decoded manifest plus any case-fold path collisions. Collisions are
/// reported, never silently dropped: the caller conflict-asides them (Plan 108
/// manifest decode hygiene). Both colliding paths remain present in the
/// decoded entry map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedManifest {
    pub manifest: Manifest,
    pub collisions: Vec<PathCollision>,
}

/// A group of manifest paths that fold to the same case-insensitive key and so
/// would collide when materialized on a case-insensitive filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCollision {
    pub folded: String,
    pub paths: Vec<WorkspacePath>,
}

/// Open and bounds-check a sealed manifest.
pub fn open_manifest(
    crypto: &WorkspaceCrypto,
    sealed: &[u8],
    limits: &DecodeLimits,
) -> Result<DecodedManifest, ManifestError> {
    // Pre-decompression guard: reject before `open` allocates any plaintext.
    if sealed.len() as u64 > limits.max_sealed_bytes {
        return Err(ManifestError::BoundExceeded {
            bound: "sealed-size",
        });
    }
    let context = crypto.manifest_context(ENVELOPE_FORMAT_VERSION);
    let plaintext =
        open(sealed, crypto.storage_key(), &context).map_err(ManifestError::Envelope)?;
    if plaintext.len() as u64 > limits.max_decoded_bytes {
        return Err(ManifestError::BoundExceeded {
            bound: "decoded-size",
        });
    }
    decode_manifest_plaintext(&plaintext, crypto.key_epoch(), limits)
}

/// Decode canonical manifest plaintext with full hygiene. Separated from the
/// sealing boundary so tests can exercise the bounds directly.
pub fn decode_manifest_plaintext(
    plaintext: &[u8],
    expected_epoch: KeyEpoch,
    limits: &DecodeLimits,
) -> Result<DecodedManifest, ManifestError> {
    let wire: ManifestWire = serde_json::from_slice(plaintext)
        .map_err(|_| ManifestError::Serialization("manifest decode failed"))?;
    if wire.key_epoch != expected_epoch.get() {
        return Err(ManifestError::KeyEpochMismatch);
    }
    // An authenticated manifest from a future/experimental writer must fail
    // closed rather than be applied with v1 semantics just because its JSON
    // happens to fit the current struct.
    if wire.format_version != MANIFEST_FORMAT_VERSION {
        return Err(ManifestError::UnsupportedFormatVersion {
            found: wire.format_version,
        });
    }
    if wire.entries.len() as u64 > limits.max_records {
        return Err(ManifestError::BoundExceeded {
            bound: "record-count",
        });
    }

    let mut entries = BTreeMap::new();
    let mut folded: HashMap<String, Vec<WorkspacePath>> = HashMap::new();
    let mut previous: Option<String> = None;
    let mut aggregate: u64 = 0;

    for wire_entry in wire.entries {
        let path = wire_entry.path.clone();
        validate_path(&path, limits)?;
        // The array is the canonical sorted form; a decode that is not strictly
        // increasing is either reordered or duplicated and must be rejected
        // rather than silently accepted (a `BTreeMap` decode would hide both).
        match &previous {
            Some(prev) if prev.as_str() >= path.as_str() => {
                return Err(if prev.as_str() == path.as_str() {
                    ManifestError::DuplicatePath
                } else {
                    ManifestError::NotSorted
                });
            }
            _ => {}
        }
        previous = Some(path.clone());

        let workspace_path = WorkspacePath::new(path);
        let entry = wire_entry.into_entry()?;
        validate_symlink_target(&entry, limits)?;
        if let ManifestEntry::File { size, .. } = &entry {
            aggregate = aggregate
                .checked_add(*size)
                .filter(|total| *total <= limits.max_aggregate_declared_bytes)
                .ok_or(ManifestError::BoundExceeded {
                    bound: "aggregate-declared-size",
                })?;
        }
        folded
            .entry(workspace_path.as_str().to_lowercase())
            .or_default()
            .push(workspace_path.clone());
        entries.insert(workspace_path, entry);
    }

    // Case-fold is Unicode-aware lowercasing; precomposed/decomposed (NFC/NFD)
    // equivalence is a follow-up (needs a normalization crate) — see report.
    let mut collisions: Vec<PathCollision> = folded
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(folded_key, mut paths)| {
            paths.sort();
            PathCollision {
                folded: folded_key,
                paths,
            }
        })
        .collect();
    collisions.sort_by(|left, right| left.folded.cmp(&right.folded));

    Ok(DecodedManifest {
        manifest: Manifest {
            format_version: wire.format_version,
            key_epoch: KeyEpoch::new(wire.key_epoch),
            entries,
        },
        collisions,
    })
}

fn validate_path(path: &str, limits: &DecodeLimits) -> Result<(), ManifestError> {
    if path.len() as u64 > limits.max_path_len {
        return Err(ManifestError::BoundExceeded {
            bound: "path-length",
        });
    }
    if path.is_empty() || normalize_workspace_path(path) != path {
        return Err(ManifestError::InvalidPath {
            reason: "path is not normalized-relative",
        });
    }
    if path.starts_with('/') {
        return Err(ManifestError::InvalidPath {
            reason: "path is absolute",
        });
    }
    if path.split('/').any(|part| part == ".." || part == ".") {
        return Err(ManifestError::InvalidPath {
            reason: "path escapes the workspace",
        });
    }
    if !limits.allow_workspace_state_paths && is_private_workspace_state_path(path) {
        return Err(ManifestError::InvalidPath {
            reason: "path is private engine state",
        });
    }
    Ok(())
}

fn validate_symlink_target(
    entry: &ManifestEntry,
    limits: &DecodeLimits,
) -> Result<(), ManifestError> {
    if let ManifestEntry::Symlink { target, .. } = entry {
        if target.is_empty() {
            return Err(ManifestError::InvalidEntry {
                reason: "symlink target is empty",
            });
        }
        if target.len() as u64 > limits.max_path_len {
            return Err(ManifestError::BoundExceeded {
                bound: "symlink-target-length",
            });
        }
    }
    Ok(())
}

// ---- errors --------------------------------------------------------------

#[derive(Debug)]
pub enum ManifestError {
    Envelope(EnvelopeError),
    Serialization(&'static str),
    BoundExceeded { bound: &'static str },
    InvalidPath { reason: &'static str },
    InvalidEntry { reason: &'static str },
    KeyEpochMismatch,
    UnsupportedFormatVersion { found: u32 },
    ContentIdMismatch,
    NotSorted,
    DuplicatePath,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(error) => write!(formatter, "manifest envelope failed: {error}"),
            Self::Serialization(reason) => write!(formatter, "manifest serialization: {reason}"),
            Self::BoundExceeded { bound } => {
                write!(formatter, "manifest decode bound exceeded: {bound}")
            }
            Self::InvalidPath { reason } => write!(formatter, "invalid manifest path: {reason}"),
            Self::InvalidEntry { reason } => write!(formatter, "invalid manifest entry: {reason}"),
            Self::KeyEpochMismatch => formatter.write_str("manifest key epoch does not match"),
            Self::UnsupportedFormatVersion { found } => {
                write!(formatter, "unsupported manifest format version {found}")
            }
            Self::ContentIdMismatch => {
                formatter.write_str("recovered plaintext does not match its content id")
            }
            Self::NotSorted => formatter.write_str("manifest entries are not canonically sorted"),
            Self::DuplicatePath => formatter.write_str("manifest contains a duplicate path"),
        }
    }
}

impl Error for ManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
            _ => None,
        }
    }
}

impl From<EnvelopeError> for ManifestError {
    fn from(error: EnvelopeError) -> Self {
        Self::Envelope(error)
    }
}

#[cfg(test)]
impl WorkspaceCrypto {
    /// Builds a file context with an overridable framing version so the
    /// substitution suite can prove a format mismatch fails `open`.
    pub(crate) fn file_context_for_test(
        &self,
        content_id: &ContentId,
        format_version: u16,
    ) -> EnvelopeContext {
        self.file_context(content_id, format_version)
    }

    pub(crate) fn storage_key_for_test(&self) -> StorageKey {
        self.storage_key()
    }
}

#[cfg(test)]
#[path = "manifest/tests.rs"]
mod tests;
