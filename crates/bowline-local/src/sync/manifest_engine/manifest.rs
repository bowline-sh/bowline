//! Workspace manifest identity and the sealed encode/decode boundary for the
//! manifest-sync engine (Plan 109 Step 3).
//!
//! Two identities live here and are never conflated (Plan 108 "Object
//! identity"): the *logical* [`ContentId`] is a workspace-keyed,
//! domain-separated BLAKE3 of canonical plaintext; the *physical* [`BlobKey`] /
//! [`ManifestKey`] is `blake3(sealed_bytes)`. Sealing is convergent (the
//! envelope derives its nonce from the workspace key and the plaintext), so both
//! are stable functions of the plaintext and a reseal reproduces the same object
//! byte for byte. Canonical serialization is asserted deterministic on
//! plaintext, never on ciphertext — the plaintext is what identity is defined
//! over, and the envelope layout is free to change.
//!
//! [`Manifest`] is the engine's IN-MEMORY truth: a flat sorted map, which is
//! what the three-way merge matrix reconciles against. It is not the wire form.
//! On the wire a manifest is a Merkle tree of per-directory nodes ([`tree`]),
//! and a manifest key names that tree's ROOT node. [`directory_tree`] is the
//! bridge between the two.

use std::collections::BTreeMap;
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

#[path = "manifest/directory_tree.rs"]
pub mod directory_tree;
#[path = "manifest/tree.rs"]
pub mod tree;

/// Envelope AEAD format version for engine objects. Distinct from
/// [`tree::TREE_FORMAT_VERSION`], which lives inside the sealed plaintext; this
/// value binds the wire framing as associated data so a future framing change
/// cannot be opened under the old context.
const ENVELOPE_FORMAT_VERSION: u16 = 1;

/// Domain separators keep a file's logical id disjoint from a manifest's even
/// when their plaintext bytes coincide. Length-prefixed on hash so no content
/// can forge a domain boundary.
const FILE_CONTENT_DOMAIN: &[u8] = b"bowline/workspace-file/v1";
const TREE_NODE_CONTENT_DOMAIN: &[u8] = b"bowline/workspace-tree-node/v1";

/// A workspace-relative path as some producer observed it. Construction is
/// deliberately infallible — a `readdir` name, a watcher event, and a decoded
/// manifest row all become one of these — so the type carries NO validity
/// promise. [`publishable_workspace_path`] is the single predicate that decides
/// whether a path may cross the wire, and both the writer (push) and the reader
/// ([`validate_manifest_path`], called on every path a tree walk reconstructs)
/// call it.
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

/// The longest workspace path the engine will publish or accept. Owned here so
/// the writer's pre-publish check and [`DecodeLimits`] cannot drift apart.
pub const MAX_WORKSPACE_PATH_LEN: u64 = 4096;

/// The deepest path the engine will publish or accept, in components.
///
/// The flat manifest had no reason to care how deep a path was; the Merkle form
/// does, because depth is recursion and an attacker-shaped tree is otherwise a
/// stack-exhaustion vector. The cap is owned here rather than in the decoder so
/// the writer refuses a too-deep path as an unsyncable entry — one path's
/// problem — instead of publishing something every peer must reject.
pub const MAX_WORKSPACE_PATH_DEPTH: u64 = 256;

/// Why a path may not be published into (or accepted from) a manifest. Carried
/// as a typed reason so push can report the offending path to the user instead
/// of failing the whole cycle, and decode can name the violated rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PathRejection {
    Empty,
    Absolute,
    NotNormalized,
    Traversal,
    PrivateEngineState,
    TooLong,
    TooDeep,
}

impl PathRejection {
    /// The `&'static str` tag carried by [`ManifestError::InvalidPath`].
    pub fn reason(self) -> &'static str {
        match self {
            Self::Empty => "path is empty",
            Self::Absolute => "path is absolute",
            Self::NotNormalized => "path is not normalized-relative",
            Self::Traversal => "path escapes the workspace",
            Self::PrivateEngineState => "path is private engine state",
            Self::TooLong => "path exceeds the maximum length",
            Self::TooDeep => "path exceeds the maximum depth",
        }
    }
}

/// THE predicate for "may this path exist in a manifest?". One owner, called by
/// the writer before it seals and by the reader after it decodes — the asymmetry
/// this replaces let a device publish a path (e.g. a POSIX filename containing a
/// backslash, which `normalize_workspace_path` rewrites) that every peer then
/// rejected as `InvalidPath`, killing their engines.
pub fn publishable_workspace_path(
    path: &str,
    max_path_len: u64,
    allow_workspace_state_paths: bool,
) -> Result<(), PathRejection> {
    if path.is_empty() {
        return Err(PathRejection::Empty);
    }
    if path.len() as u64 > max_path_len {
        return Err(PathRejection::TooLong);
    }
    if path.starts_with('/') {
        return Err(PathRejection::Absolute);
    }
    if path.split('/').count() as u64 > MAX_WORKSPACE_PATH_DEPTH {
        return Err(PathRejection::TooDeep);
    }
    if path.split('/').any(|part| part == ".." || part == ".") {
        return Err(PathRejection::Traversal);
    }
    if normalize_workspace_path(path) != path {
        return Err(PathRejection::NotNormalized);
    }
    if !allow_workspace_state_paths && is_private_workspace_state_path(path) {
        return Err(PathRejection::PrivateEngineState);
    }
    Ok(())
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

    /// The one mode the engine ever records for a symlink.
    ///
    /// A symlink's permission bits are neither portable nor settable: `symlink(2)`
    /// takes no mode, `set_mode` skips links (macOS `lchmod` semantics vary), and
    /// the kernel picks the value — Linux reports `0o120777`, macOS `0o120755` for
    /// the very same link. Recording what `lstat` happened to say therefore made
    /// one link two different entries: the Linux peer read every macOS-published
    /// link as locally *changed* forever, and `(Changed, Absent)` — keep local,
    /// re-push as a creation — resurrected links the other device had deleted,
    /// along with the parent directories an install recreates. Pinning one value
    /// makes a link's identity exactly what the engine can actually carry: its
    /// kind and its target.
    pub const fn symlink() -> Self {
        Self(0o120_777)
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

/// The portable truth as the engine holds it in memory: a sorted map of paths to
/// entries plus the key epoch it was produced under.
///
/// This is a VIEW, not a wire object. It is what the merge matrix reconciles
/// against and what a work-view diff compares; the bytes that travel are the
/// per-directory nodes [`directory_tree`] decomposes it into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub key_epoch: KeyEpoch,
    pub entries: BTreeMap<WorkspacePath, ManifestEntry>,
}

impl Manifest {
    pub fn new(key_epoch: KeyEpoch, entries: BTreeMap<WorkspacePath, ManifestEntry>) -> Self {
        Self { key_epoch, entries }
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

/// Workspace-keyed logical identity of one tree node's canonical plaintext.
pub fn tree_node_content_id(workspace_key: [u8; 32], plaintext: &[u8]) -> ContentId {
    keyed_content_id(workspace_key, TREE_NODE_CONTENT_DOMAIN, plaintext, "mcid")
}

/// Physical key of a sealed file blob. Derives from the sealed bytes, so a
/// create-only PUT that collides on a different byte string is corruption, not
/// a recoverable reseal (Plan 108).
///
/// Because the envelope's nonce is derived from the workspace key and the
/// plaintext, this is a genuine content address: the same bytes always seal to
/// the same key, on this device and on every other device holding the same
/// workspace key. That is what lets a rename, an A→B→A edit, a `git checkout`
/// round trip, and a second device's first push all collapse onto one stored
/// object instead of uploading the same content again under a fresh key.
///
/// The security trade is deliberate and is standard single-tenant convergent
/// encryption: the server can tell which objects **within one workspace** are
/// byte-identical. It cannot tell what they contain, cannot compare across
/// workspaces (the key is per workspace), and already saw each object's padded
/// size, so the marginal leak is a within-workspace equality relation over
/// objects whose sizes it could already bucket.
pub fn physical_blob_key(sealed: &[u8]) -> BlobKey {
    BlobKey::new(format!("b_{}", blake3::hash(sealed).to_hex()))
}

/// Physical key of a sealed manifest tree node. The ROOT node's key is the
/// manifest key the workspace ref points at, so this one function names both a
/// whole snapshot and each of its shared subtrees.
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

/// Workspace crypto material for held key epochs. New objects seal at
/// `write_epoch`; readers may open older epochs still present in the keyring.
#[derive(Clone)]
pub struct WorkspaceCrypto {
    workspace_id_hash: String,
    write_epoch: KeyEpoch,
    keys: BTreeMap<KeyEpoch, [u8; 32]>,
}

impl WorkspaceCrypto {
    pub fn new(workspace_id: &str, key_bytes: [u8; 32], key_epoch: KeyEpoch) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(key_epoch, key_bytes);
        Self {
            workspace_id_hash: workspace_id_hash(workspace_id),
            write_epoch: key_epoch,
            keys,
        }
    }

    pub fn with_key_epoch(mut self, key_epoch: KeyEpoch, key_bytes: [u8; 32]) -> Self {
        self.keys.insert(key_epoch, key_bytes);
        self
    }

    /// Held key epochs in ascending order, matching [`BTreeMap`] iteration.
    pub fn key_epochs(&self) -> Vec<KeyEpoch> {
        self.keys.keys().copied().collect()
    }

    pub fn key_epoch(&self) -> KeyEpoch {
        self.write_epoch
    }

    /// The non-secret workspace identity bound into every envelope context. The
    /// workspace-root sentinel writes it to disk so a remounted or renamed root
    /// can be proven to be the same workspace the ancestor was committed against.
    pub fn workspace_id_hash(&self) -> &str {
        &self.workspace_id_hash
    }

    pub fn content_id(&self, plaintext: &[u8]) -> ContentId {
        let key_bytes = self
            .key_bytes(self.write_epoch)
            .expect("write epoch key must exist in workspace crypto keyring");
        content_id(key_bytes, plaintext)
    }

    pub fn content_id_at(&self, key_epoch: KeyEpoch, plaintext: &[u8]) -> Option<ContentId> {
        self.key_bytes(key_epoch)
            .map(|key_bytes| content_id(key_bytes, plaintext))
    }

    pub fn tree_node_content_id(&self, plaintext: &[u8]) -> ContentId {
        let key_bytes = self
            .key_bytes(self.write_epoch)
            .expect("write epoch key must exist in workspace crypto keyring");
        tree_node_content_id(key_bytes, plaintext)
    }

    fn key_bytes(&self, key_epoch: KeyEpoch) -> Option<[u8; 32]> {
        self.keys.get(&key_epoch).copied()
    }

    fn storage_key(&self) -> StorageKey {
        self.storage_key_at(self.write_epoch)
            .expect("write epoch key must exist in workspace crypto keyring")
    }

    fn storage_key_at(&self, key_epoch: KeyEpoch) -> Option<StorageKey> {
        self.key_bytes(key_epoch).map(StorageKey::from_bytes)
    }

    fn file_context(
        &self,
        content_id: &ContentId,
        key_epoch: KeyEpoch,
        format_version: u16,
    ) -> EnvelopeContext {
        EnvelopeContext {
            workspace_id_hash: self.workspace_id_hash.clone(),
            object_kind: EnvelopePurpose::WorkspaceFileV1.object_kind(),
            object_id: content_id.as_str().to_string(),
            record_id: EnvelopePurpose::WorkspaceFileV1.as_aad().to_string(),
            key_epoch: key_epoch.get(),
            format_version,
        }
    }

    // A node's content id is unknowable before opening (it is derived from the
    // very plaintext being opened), so unlike a file the node context does not
    // bind it. The physical key `m_<blake3(sealed)>` already pins the exact
    // bytes, and the caller re-derives `tree_node_content_id` post-open for its
    // records; workspace, purpose, epoch, and format still bind here.
    fn tree_node_context(&self, key_epoch: KeyEpoch, format_version: u16) -> EnvelopeContext {
        EnvelopeContext {
            workspace_id_hash: self.workspace_id_hash.clone(),
            object_kind: EnvelopePurpose::WorkspaceManifestV1.object_kind(),
            object_id: EnvelopePurpose::WorkspaceManifestV1.as_aad().to_string(),
            record_id: EnvelopePurpose::WorkspaceManifestV1.as_aad().to_string(),
            key_epoch: key_epoch.get(),
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
    let context = crypto.file_context(content_id, crypto.key_epoch(), ENVELOPE_FORMAT_VERSION);
    seal(plaintext, crypto.storage_key(), &context).map_err(ManifestError::Envelope)
}

/// Open a sealed file blob, verifying the recovered plaintext hashes back to the
/// expected content id (defense in depth atop the AEAD binding).
pub fn open_file(
    crypto: &WorkspaceCrypto,
    key_epoch: KeyEpoch,
    expected_content_id: &ContentId,
    sealed: &[u8],
) -> Result<Vec<u8>, ManifestError> {
    let Some(storage_key) = crypto.storage_key_at(key_epoch) else {
        return Err(ManifestError::UnknownKeyEpoch { key_epoch });
    };
    let context = crypto.file_context(expected_content_id, key_epoch, ENVELOPE_FORMAT_VERSION);
    let plaintext = open(sealed, storage_key, &context).map_err(ManifestError::Envelope)?;
    if crypto.content_id_at(key_epoch, &plaintext).as_ref() != Some(expected_content_id) {
        return Err(ManifestError::ContentIdMismatch);
    }
    Ok(plaintext)
}

/// Seal one tree node's canonical plaintext.
pub fn seal_tree_node(
    crypto: &WorkspaceCrypto,
    plaintext: &[u8],
) -> Result<SealedEnvelope, ManifestError> {
    let context = crypto.tree_node_context(crypto.key_epoch(), ENVELOPE_FORMAT_VERSION);
    seal(plaintext, crypto.storage_key(), &context).map_err(ManifestError::Envelope)
}

/// Open one sealed tree node to bounds-checked plaintext.
///
/// `max_sealed_bytes` is the pre-allocation guard: it is checked on the
/// ciphertext BEFORE [`open`], so a hostile object cannot force a large
/// plaintext allocation. `max_decoded_bytes` is enforced POST-decompress, on the
/// plaintext [`open`] returns, so it is AEAD-gated — only an object that already
/// authenticated under the workspace key can reach it — and it never trusts an
/// attacker-declared size.
pub fn open_tree_node(
    crypto: &WorkspaceCrypto,
    sealed: &[u8],
    limits: &DecodeLimits,
) -> Result<(Vec<u8>, KeyEpoch), ManifestError> {
    if sealed.len() as u64 > limits.max_sealed_bytes {
        return Err(ManifestError::BoundExceeded {
            bound: "sealed-size",
        });
    }
    let mut last_error = None;
    for (key_epoch, key_bytes) in crypto.keys.iter().rev() {
        let context = crypto.tree_node_context(*key_epoch, ENVELOPE_FORMAT_VERSION);
        let storage_key = StorageKey::from_bytes(*key_bytes);
        // AEAD authentication makes a wrong-key attempt indistinguishable from any
        // other open failure, so trying the handful of held epochs leaks no oracle.
        match open(sealed, storage_key, &context) {
            Ok(plaintext) => {
                if plaintext.len() as u64 > limits.max_decoded_bytes {
                    return Err(ManifestError::BoundExceeded {
                        bound: "decoded-size",
                    });
                }
                return Ok((plaintext, *key_epoch));
            }
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(ManifestError::Envelope(error)),
        None => Err(ManifestError::UnknownKeyEpoch {
            key_epoch: crypto.key_epoch(),
        }),
    }
}

// ---- bounded decode ------------------------------------------------------

/// The bounds a hostile peer's manifest tree is decoded under.
///
/// `max_sealed_bytes` / `max_decoded_bytes` bound ONE node (see
/// [`open_tree_node`]). `max_node_entries` bounds one node's fan-out;
/// `max_records`, `max_path_len`, `max_depth`,
/// `max_aggregate_declared_bytes`, and `max_aggregate_decoded_bytes` bound the
/// tree as a whole and are enforced by the walk that flattens it.
///
/// `max_aggregate_declared_bytes` bounds the file content a tree *claims* to
/// hold; `max_aggregate_decoded_bytes` bounds what flattening actually
/// allocates. They are separate caps because the second is the memory a hostile
/// peer can make this device hold before a single byte of content is fetched:
/// per-node caps alone leave `max_records * max_path_len` of retained paths and
/// symlink targets, which is gigabytes at these values while every individual
/// node validates. Every directory a walk descends into arrived as an
/// entry that already counted against `max_records`, so the node count is
/// bounded by the same cap and a tree cannot fan out without limit.
///
/// The per-node sealed cap is what retires the flat form's "sealed manifest >
/// 16 MB → chunk it" escape hatch: no single object grows with the workspace any
/// more, only the node count does.
#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    pub max_sealed_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_records: u64,
    pub max_node_entries: u64,
    pub max_path_len: u64,
    pub max_name_len: u64,
    pub max_depth: u64,
    pub max_aggregate_declared_bytes: u64,
    pub max_aggregate_decoded_bytes: u64,
    allow_workspace_state_paths: bool,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_sealed_bytes: 64 * 1024 * 1024,
            max_decoded_bytes: 256 * 1024 * 1024,
            max_records: 2_000_000,
            max_node_entries: 1_000_000,
            max_path_len: MAX_WORKSPACE_PATH_LEN,
            max_name_len: tree::MAX_NAME_LEN,
            max_depth: MAX_WORKSPACE_PATH_DEPTH,
            max_aggregate_declared_bytes: 8 * 1024 * 1024 * 1024 * 1024,
            // Comfortably above a real workspace's flattened manifest (a 100k-entry
            // tree decodes to tens of MB) and far below what exhausts a daemon.
            max_aggregate_decoded_bytes: 2 * 1024 * 1024 * 1024,
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

    /// Whether `.bowline`-style names are ordinary content under these limits.
    /// Push consults it so the writer and the reader share one path predicate.
    pub fn allows_workspace_state_paths(&self) -> bool {
        self.allow_workspace_state_paths
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

/// A group of manifest paths the endpoint filesystem cannot tell apart, and so
/// would collide when materialized here. What counts as indistinguishable is the
/// probed [`NameFolding`], not a fixed rule: `README.md`/`readme.md` collide only
/// on a case-insensitive volume, and the NFC/NFD spellings of one name only on a
/// normalization-insensitive one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCollision {
    pub folded: String,
    pub paths: Vec<WorkspacePath>,
}

/// THE reader-side path check, applied to every full path a tree walk
/// reconstructs. It is the same predicate the writer calls before publishing, so
/// a path one device will emit is exactly the set every device will accept.
pub fn validate_manifest_path(path: &str, limits: &DecodeLimits) -> Result<(), ManifestError> {
    match publishable_workspace_path(
        path,
        limits.max_path_len,
        limits.allow_workspace_state_paths,
    ) {
        Ok(()) => Ok(()),
        Err(PathRejection::TooLong) => Err(ManifestError::BoundExceeded {
            bound: "path-length",
        }),
        Err(PathRejection::TooDeep) => Err(ManifestError::BoundExceeded {
            bound: "path-depth",
        }),
        Err(rejection) => Err(ManifestError::InvalidPath {
            reason: rejection.reason(),
        }),
    }
}

// ---- errors --------------------------------------------------------------

#[derive(Debug)]
pub enum ManifestError {
    Envelope(EnvelopeError),
    Serialization(&'static str),
    BoundExceeded {
        bound: &'static str,
    },
    InvalidPath {
        reason: &'static str,
    },
    InvalidEntry {
        reason: &'static str,
    },
    KeyEpochMismatch,
    UnknownKeyEpoch {
        key_epoch: KeyEpoch,
    },
    UnsupportedFormatVersion {
        found: u32,
    },
    ContentIdMismatch,
    NotSorted,
    DuplicatePath,
    /// An engine invariant the wire cannot express was violated locally.
    Internal {
        reason: &'static str,
    },
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
            Self::UnknownKeyEpoch { key_epoch } => {
                write!(
                    formatter,
                    "manifest key epoch {} is not held",
                    key_epoch.get()
                )
            }
            Self::UnsupportedFormatVersion { found } => {
                write!(formatter, "unsupported manifest format version {found}")
            }
            Self::ContentIdMismatch => {
                formatter.write_str("recovered plaintext does not match its content id")
            }
            Self::NotSorted => formatter.write_str("manifest entries are not canonically sorted"),
            Self::DuplicatePath => formatter.write_str("manifest contains a duplicate path"),
            Self::Internal { reason } => write!(formatter, "manifest internal invariant: {reason}"),
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
        self.file_context(content_id, self.key_epoch(), format_version)
    }

    pub(crate) fn storage_key_for_test(&self) -> StorageKey {
        self.storage_key()
    }
}

#[cfg(test)]
#[path = "manifest/tests.rs"]
mod tests;
