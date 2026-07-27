//! Push: scan dirty paths, upload changed blobs and one manifest, then CAS the
//! workspace ref (Plan 109 Step 4).
//!
//! The one binding contract here (review Change 1): **the committed ancestor is
//! sacred.** A scan/upload builds an IMMUTABLE in-memory candidate map; the
//! `files` table changes ONLY on CAS success, inside
//! [`ManifestStore::commit_push_success`]. A lost CAS leaves the ancestor and
//! the user's local edit exactly as they were, so the driver can pull the winner
//! against an unchanged base and retry.
//!
//! This module also owns the shared engine primitives that pull/apply reuses:
//! the remote dependency traits ([`RemoteObjects`], [`RemoteRef`]) and the
//! [`EngineContext`]. They live here because push is Step 4 (it lands first) and
//! pull builds on them. The no-follow filesystem trust boundary that push's read
//! side and apply's write side share has its own seam in [`super::fs_guard`];
//! deriving the candidate delta — and the refusals that decide what may never
//! reach a manifest — lives in the sibling `candidate` module.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bowline_core::ids::{ContentId, DeviceId};

use super::endpoint::{NameFolding, StatTrust, TimestampGranularity};
use super::fs_guard::write_private_file;
use super::manifest::{
    BlobKey, EntryKind, KeyEpoch, Manifest, ManifestEntry, ManifestError, ManifestKey,
    WorkspaceCrypto, WorkspacePath, physical_blob_key, seal_file,
};
use super::store::{FileRecord, ManifestStore, ManifestStoreError};
use super::tree_transport::{PublishTreeRequest, StoreNodeLedger, TreeError, publish_tree};

#[path = "push/candidate.rs"]
mod candidate;

use candidate::{Candidate, build_candidate, guard_mass_deletion, record_unsyncable_outcome};

/// Private engine subtree under the workspace root. Temp writes, the sealed
/// large-file spool, and quarantined preimages all live here so a crash never
/// strands plaintext or partial files inside the synced tree.
pub const ENGINE_STATE_DIR: &str = ".bowline";

// ---- dependency seams (Plan 111 wires the real transport; tests fake it) ----

/// A sealed blob upload: create-only PUT + hosted metadata commit. The physical
/// `key` is `blake3(sealed)`; a 412 on different bytes is corruption, never a
/// recoverable reseal (Plan 108 object identity).
pub struct BlobUpload<'a> {
    pub key: &'a BlobKey,
    pub content_id: &'a ContentId,
    pub key_epoch: KeyEpoch,
    pub sealed: &'a [u8],
}

/// A large sealed blob streamed from a 0600 on-disk spool, so peak memory during
/// the HTTP send stays bounded rather than holding a second copy of the sealed
/// bytes (Plan 109 review ADD 4).
pub struct BlobReaderUpload<'a> {
    pub key: &'a BlobKey,
    pub content_id: &'a ContentId,
    pub key_epoch: KeyEpoch,
    pub spool_path: &'a Path,
    pub byte_len: u64,
}

/// A sealed manifest upload: same create-only + commit contract as a blob.
pub struct ManifestUpload<'a> {
    pub key: &'a ManifestKey,
    pub content_id: &'a ContentId,
    pub key_epoch: KeyEpoch,
    pub sealed: &'a [u8],
}

/// The object side of the hosted contract the engine consumes. Every `put_*`
/// reserves, PUTs create-only, and commits hosted metadata before returning; the
/// engine reads the object back through `get_*` and re-validates before letting
/// anything reference it.
pub trait RemoteObjects {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError>;
    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError>;
    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError>;
    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError>;
    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError>;
}

/// The current head of the workspace CAS ref, mapped from the hosted
/// `WorkspaceRef` (`snapshot_id <-> manifest object key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObservation {
    pub version: u64,
    pub manifest_key: ManifestKey,
}

/// The three outcomes of a ref CAS the engine must distinguish (Plan 108 loop).
/// `Ambiguous` is a lost/failed ack after the swap may or may not have
/// committed; push resolves it by reading the current ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasOutcome {
    Advanced(RefObservation),
    Lost(RefObservation),
    Ambiguous,
}

/// The ref side of the hosted contract. `read_ref` is the synchronous startup
/// authority and the `Ambiguous`-CAS resolver; the subscription is only a
/// wakeup (Plan 108).
pub trait RemoteRef {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError>;
    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError>;
}

// ---- engine context + config ----------------------------------------------

/// Tunables the driver (Plan 111) sets once. Split out so tests can drive the
/// large-file boundary with small fixtures.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// At or above this plaintext size a blob is sealed to a 0600 spool and
    /// streamed from disk rather than uploaded from a buffer.
    pub large_file_threshold: u64,
    /// Hard ceiling on the plaintext a single seal may buffer. The envelope has
    /// no streaming-seal API (`seal(&[u8])`), so sealing is whole-buffer; above
    /// this we STOP rather than buffer unboundedly (Plan 109 STOP condition,
    /// documented in the module report).
    pub max_seal_bytes: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            large_file_threshold: 8 * 1024 * 1024,
            max_seal_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// Everything push and pull need that is not the store or the transport: crypto
/// for one key epoch, this device's id (for conflict-aside names), the workspace
/// root, and config.
#[derive(Clone)]
pub struct EngineContext {
    pub crypto: WorkspaceCrypto,
    pub device_id: DeviceId,
    pub workspace_root: PathBuf,
    /// Private scratch and intent state. Usually `<workspace>/.bowline`; work
    /// views place it in the daemon state root so it cannot collide with
    /// project-owned content.
    pub engine_state_dir: PathBuf,
    pub config: EngineConfig,
    /// Work views are rooted at a project, where names reserved at the
    /// workspace root are ordinary project content.
    pub project_view: bool,
    /// Which spellings of a name this endpoint filesystem folds together,
    /// probed once when the workspace is accepted rather than per cycle: it is a
    /// property of the mounted volume, and every push, pull, and manifest decode
    /// must answer "same file?" the same way within one cycle. Build it with
    /// [`super::endpoint::probe_name_folding`].
    pub names: NameFolding,
    /// How coarsely this endpoint volume records modification times, probed
    /// alongside `names` and for the same reason: it is a property of the
    /// mounted volume, and every cycle must answer "could this endpoint have
    /// recorded those two timestamps as one?" the same way. Build it with
    /// [`super::endpoint::probe_timestamp_granularity`]. The engine stays
    /// correct whatever the probe answers — see [`super::endpoint`] — so a wrong
    /// answer costs syscalls, never a write.
    pub timestamps: TimestampGranularity,
    /// Shared cost meters (Plan 111 Step 5). The same `Arc` the driver holds, so
    /// push/pull/apply increment the very counters the daemon surfaces. Cloned
    /// cheaply into every `EngineContext`.
    pub counters: Arc<super::counters::EngineCounters>,
}

impl EngineContext {
    /// The private engine subtree (`<root>/.bowline`).
    pub fn engine_dir(&self) -> PathBuf {
        self.engine_state_dir.clone()
    }

    pub fn key_epoch(&self) -> KeyEpoch {
        self.crypto.key_epoch()
    }
}

/// The dependency bundle a single push receives.
pub struct PushDeps<'a, O: RemoteObjects, R: RemoteRef> {
    pub ctx: &'a EngineContext,
    pub objects: &'a O,
    pub refs: &'a R,
}

// ---- push outcome ----------------------------------------------------------

/// What a push attempt achieved. The driver reacts: `Advanced`/`NoChange` are
/// terminal; `RefLost` triggers a pull against the unchanged ancestor then one
/// rescan+retry; `Ambiguous` is resolved inside push and never surfaces.
///
/// `Advanced`/`NoChange` carry `skipped`: dirty paths a scan could not settle
/// this cycle because they were being actively written (two consecutive
/// divergences, see [`scan_path`]). They are NOT part of the published delta;
/// the driver must retain them and rescan, or a change that settles without a
/// further watcher event would stay divergent forever (a silent unsynced-change
/// violation of the change-proportional contract). `RefLost` carries no skipped
/// set: it leaves the whole dirty set in place for the pull-then-retry, so the
/// skipped paths are already retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The CAS advanced the ref and the ancestor was committed to the new head.
    Advanced {
        manifest_key: ManifestKey,
        ref_version: u64,
        skipped: BTreeSet<WorkspacePath>,
    },
    /// Nothing changed versus the ancestor: no upload, no CAS (invariant C1/C2).
    NoChange { skipped: BTreeSet<WorkspacePath> },
    /// The CAS lost. The ancestor and the local edit are untouched.
    RefLost { current: Option<RefObservation> },
}

// ---- push -------------------------------------------------------------------

/// Whether this push may publish an unusually large number of removals.
///
/// The engine enforces by default. `Confirmed` is reserved for an explicit
/// user-driven operation (a work-view accept, or an operator confirming the
/// blocked push) — never for an autonomous cycle, because the blast radius of a
/// wrong mass deletion is every trusted device's copy of the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionPolicy {
    Enforce,
    Confirmed,
}

/// Removals below this count are always allowed, however small the workspace.
/// Deleting a scratch directory must not need a confirmation.
const MIN_DELETION_ALLOWANCE: usize = 64;

/// Above `entries / DELETION_FRACTION_DENOMINATOR` removals in ONE push, a
/// deletion batch stops looking like editing and starts looking like a vanished
/// root, a bad rename, or a wrong-folder mount. Model: `rsync --max-delete`.
const DELETION_FRACTION_DENOMINATOR: usize = 4;

/// The largest removal batch one push may publish against an ancestor of
/// `entries` rows.
pub fn mass_deletion_threshold(entries: usize) -> usize {
    MIN_DELETION_ALLOWANCE.max(entries / DELETION_FRACTION_DENOMINATOR)
}

/// One push attempt over `dirty_paths`. See the module contract: the ancestor is
/// never mutated except on CAS success.
pub fn push<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    dirty_paths: &BTreeSet<WorkspacePath>,
) -> Result<PushOutcome, PushError> {
    push_dirty_paths(
        store,
        deps,
        dirty_paths,
        DeletionPolicy::Enforce,
        WatcherEvidence::Continuous,
    )
}

/// Push a dirty batch under an explicit statement of how much the engine may
/// trust a matching stat fingerprint (see [`super::endpoint`]) and whether the
/// removal breaker applies.
pub(super) fn push_dirty_paths<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    dirty_paths: &BTreeSet<WorkspacePath>,
    deletions: DeletionPolicy,
    evidence: WatcherEvidence,
) -> Result<PushOutcome, PushError> {
    let trust = match evidence {
        WatcherEvidence::Continuous => StatTrust::OutsideRacyWindow(deps.ctx.timestamps),
        WatcherEvidence::Gapped => StatTrust::Never,
    };
    push_scanned(store, deps, dirty_paths, trust, deletions)
}

/// How the dirty batch was discovered, and therefore whether a stat fingerprint
/// can be trusted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherEvidence {
    /// A watcher has been attached since the ancestor rows were written, so the
    /// only window a stat cannot see through is the racy one.
    Continuous,
    /// The batch came from a stat walk covering an unbounded unobserved gap (a
    /// restart seed, a watcher-overflow recovery) or from a surface that owns no
    /// watcher at all (a work-view capture). Verify every file's bytes.
    Gapped,
}

fn push_scanned<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    dirty_paths: &BTreeSet<WorkspacePath>,
    trust: StatTrust,
    deletions: DeletionPolicy,
) -> Result<PushOutcome, PushError> {
    let ancestor = store.all_files()?;
    let state = store.engine_state()?;

    let mut ledger = BlobLedger::new(store, deps.ctx.key_epoch());
    let candidate = build_candidate(deps, &ancestor, dirty_paths, trust, &mut ledger)?;
    // Every row here names an object whose PUT returned, so it is durable
    // regardless of how the CAS below resolves: a lost CAS does not un-upload a
    // blob, and the retry must not re-seal it.
    let sealed = ledger.into_sealed();
    if !sealed.is_empty() {
        store.record_sealed_blobs(&sealed)?;
        deps.ctx.counters.record_sqlite_mutation();
    }
    record_unsyncable_outcome(store, &candidate)?;
    if candidate.is_empty() {
        if !candidate.local_refreshes.is_empty() {
            store.refresh_local_file_records(&candidate.local_refreshes)?;
            deps.ctx.counters.record_sqlite_mutation();
        }
        // No delta to publish, but any twice-diverged paths must still be handed
        // back so the driver retains and rescans them rather than dropping them.
        return Ok(PushOutcome::NoChange {
            skipped: candidate.skipped,
        });
    }
    guard_mass_deletion(&ancestor, &candidate, deletions)?;

    let manifest = build_manifest(&ancestor, &candidate, deps.ctx.key_epoch())?;
    let manifest_key = upload_manifest(store, deps, &manifest)?;

    let expected = state.last_ref_version;
    deps.ctx.counters.record_cas_attempt();
    let outcome = deps
        .refs
        .compare_and_swap(expected, &manifest_key)
        .map_err(PushError::Transport)?;
    resolve_cas(store, deps, &candidate, &manifest_key, outcome)
}

/// Interpret the CAS outcome, committing the ancestor only on a proven advance.
fn resolve_cas<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    candidate: &Candidate,
    manifest_key: &ManifestKey,
    outcome: CasOutcome,
) -> Result<PushOutcome, PushError> {
    match outcome {
        CasOutcome::Advanced(observed) => {
            let advanced = commit_advance(store, candidate, manifest_key, observed.version)?;
            // commit_advance committed one ancestor write transaction.
            deps.ctx.counters.record_sqlite_mutation();
            Ok(advanced)
        }
        CasOutcome::Lost(current) => {
            deps.ctx.counters.record_cas_loss();
            Ok(PushOutcome::RefLost {
                current: Some(current),
            })
        }
        CasOutcome::Ambiguous => {
            // The swap ack was lost; the ref itself is authoritative. Adopt the
            // candidate ONLY if the current head is exactly our manifest key.
            match deps.refs.read_ref().map_err(PushError::Transport)? {
                Some(current) if &current.manifest_key == manifest_key => {
                    let advanced = commit_advance(store, candidate, manifest_key, current.version)?;
                    deps.ctx.counters.record_sqlite_mutation();
                    Ok(advanced)
                }
                current => {
                    deps.ctx.counters.record_cas_loss();
                    Ok(PushOutcome::RefLost { current })
                }
            }
        }
    }
}

fn commit_advance(
    store: &mut ManifestStore,
    candidate: &Candidate,
    manifest_key: &ManifestKey,
    ref_version: u64,
) -> Result<PushOutcome, PushError> {
    store.commit_push_success(&candidate.ancestor_commit(), manifest_key, ref_version)?;
    Ok(PushOutcome::Advanced {
        manifest_key: manifest_key.clone(),
        ref_version,
        // The advance published the delta; hand back the paths the scan could not
        // settle so the driver retains and rescans them (see `PushOutcome`).
        skipped: candidate.skipped.clone(),
    })
}

// ---- upload ---------------------------------------------------------------

/// The device's memory of content it has already sealed and uploaded.
///
/// Reads are primary-key lookups against the durable `blobs` table plus the
/// in-flight map for content this same push sealed, so the dedup check is
/// O(log rows) per changed file rather than a scan. Writes are accumulated and
/// committed once by the caller: a row may only be recorded for content whose
/// PUT actually returned, so a failed upload never leaves a claim that the
/// object exists.
pub(super) struct BlobLedger<'a> {
    store: &'a ManifestStore,
    key_epoch: KeyEpoch,
    fresh: BTreeMap<ContentId, super::store::SealedBlob>,
}

impl<'a> BlobLedger<'a> {
    pub(super) fn new(store: &'a ManifestStore, key_epoch: KeyEpoch) -> Self {
        Self {
            store,
            key_epoch,
            fresh: BTreeMap::new(),
        }
    }

    fn known(&self, content_id: &ContentId) -> Result<Option<BlobKey>, PushError> {
        if let Some(blob) = self.fresh.get(content_id) {
            return Ok(Some(blob.blob_key.clone()));
        }
        Ok(self
            .store
            .sealed_blob(content_id, self.key_epoch)?
            .map(|blob| blob.blob_key))
    }

    fn record(&mut self, content_id: &ContentId, blob_key: &BlobKey, byte_len: u64) {
        self.fresh.insert(
            content_id.clone(),
            super::store::SealedBlob {
                blob_key: blob_key.clone(),
                key_epoch: self.key_epoch,
                byte_len,
            },
        );
    }

    /// The rows this push earned the right to persist.
    pub(super) fn into_sealed(self) -> BTreeMap<ContentId, super::store::SealedBlob> {
        self.fresh
    }
}

pub(super) fn upload_file_blob<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    content_id: &ContentId,
    plaintext: &[u8],
    ledger: &mut BlobLedger<'_>,
) -> Result<BlobKey, PushError> {
    // Content this device has ever sealed under this epoch is already in the
    // object store under a key that is a function of the plaintext, so both the
    // seal (zstd + AEAD over every byte) and the create-only PUT are pure waste.
    if let Some(existing) = ledger.known(content_id)? {
        return Ok(existing);
    }
    let sealed = seal_file(&deps.ctx.crypto, content_id, plaintext).map_err(PushError::Manifest)?;
    let key = physical_blob_key(sealed.as_bytes());
    let key_epoch = deps.ctx.key_epoch();

    if plaintext.len() as u64 >= deps.ctx.config.large_file_threshold {
        upload_blob_streaming(deps, &key, content_id, key_epoch, sealed.as_bytes())?;
    } else {
        deps.objects
            .put_blob(BlobUpload {
                key: &key,
                content_id,
                key_epoch,
                sealed: sealed.as_bytes(),
            })
            .map_err(PushError::Transport)?;
    }
    // A real blob PUT happened (the dedup short-circuit above returned early).
    deps.ctx.counters.record_blob_upload();
    ledger.record(content_id, &key, plaintext.len() as u64);
    Ok(key)
}

/// Seal-to-spool then stream the upload from disk. The seal itself is still
/// whole-buffer (bounded by `max_seal_bytes`); only the HTTP send is streamed.
fn upload_blob_streaming<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    key: &BlobKey,
    content_id: &ContentId,
    key_epoch: KeyEpoch,
    sealed: &[u8],
) -> Result<(), PushError> {
    let spool = write_private_spool(deps.ctx, key, sealed)?;
    let result = deps.objects.put_blob_reader(BlobReaderUpload {
        key,
        content_id,
        key_epoch,
        spool_path: &spool,
        byte_len: sealed.len() as u64,
    });
    // The spool is engine-private scratch; remove it whether the upload
    // succeeded or failed so a retry re-seals cleanly.
    let _ = fs::remove_file(&spool);
    result.map_err(PushError::Transport)
}

fn write_private_spool(
    ctx: &EngineContext,
    key: &BlobKey,
    sealed: &[u8],
) -> Result<PathBuf, PushError> {
    let dir = ctx.engine_dir().join("spool");
    fs::create_dir_all(&dir).map_err(PushError::Io)?;
    let spool = dir.join(key.as_str());
    write_private_file(&spool, sealed).map_err(PushError::Io)?;
    Ok(spool)
}

/// Publish the candidate manifest as a Merkle tree and return its ROOT node key
/// — the value the CAS below swaps onto the ref.
///
/// Only the nodes on the path from a changed entry to the root are sealed and
/// PUT; every subtree whose content this device has published before is reused
/// by key. The rows the publish earned are committed BEFORE the CAS, because a
/// node's existence in the object store is durable regardless of how the CAS
/// resolves: a lost CAS does not un-upload a node, and the retry must not reseal
/// it.
fn upload_manifest<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    manifest: &Manifest,
) -> Result<ManifestKey, PushError> {
    let key_epoch = deps.ctx.key_epoch();
    let mut ledger = StoreNodeLedger::new(store, key_epoch);
    let key = publish_tree(PublishTreeRequest {
        objects: deps.objects,
        crypto: &deps.ctx.crypto,
        counters: &deps.ctx.counters,
        manifest,
        ledger: &mut ledger,
    })?;
    let recorded = ledger.into_recorded();
    if !recorded.is_empty() {
        store.record_tree_nodes(&recorded, key_epoch)?;
        deps.ctx.counters.record_sqlite_mutation();
    }
    Ok(key)
}

// ---- manifest assembly ------------------------------------------------------

/// The full candidate manifest is the ancestor as manifest entries with the
/// candidate delta applied — NEVER the remote applied manifest. `files` is the
/// single source of truth for what this device has materialized.
fn build_manifest(
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    candidate: &Candidate,
    key_epoch: KeyEpoch,
) -> Result<Manifest, PushError> {
    let mut entries = BTreeMap::new();
    for (path, record) in ancestor {
        entries.insert(path.clone(), file_record_to_entry(record)?);
    }
    for (path, (_, entry)) in &candidate.upserts {
        entries.insert(path.clone(), entry.clone());
    }
    for path in &candidate.removals {
        entries.remove(path);
    }
    Ok(Manifest::new(key_epoch, entries))
}

/// Ancestor row -> manifest entry. Shared with pull's ancestor projection.
pub(super) fn file_record_to_entry(record: &FileRecord) -> Result<ManifestEntry, PushError> {
    match record.kind {
        EntryKind::File => Ok(ManifestEntry::File {
            size: record.size,
            mode: record.mode,
            content_id: record
                .content_id
                .clone()
                .ok_or(PushError::AncestorRowMissing {
                    field: "content_id",
                })?,
            blob_key: record
                .blob_key
                .clone()
                .ok_or(PushError::AncestorRowMissing { field: "blob_key" })?,
            key_epoch: record
                .key_epoch
                .ok_or(PushError::AncestorRowMissing { field: "key_epoch" })?,
        }),
        EntryKind::Directory => Ok(ManifestEntry::Directory { mode: record.mode }),
        EntryKind::Symlink => Ok(ManifestEntry::Symlink {
            mode: record.mode,
            target: record
                .symlink_target
                .clone()
                .ok_or(PushError::AncestorRowMissing {
                    field: "symlink_target",
                })?,
        }),
    }
}

// ---- timestamps -------------------------------------------------------------

/// Unix nanoseconds since the epoch, for the `hashed_at` audit column and the
/// unsyncable ledger. Never orders conflicts (Plan 108: no clock ordering), and
/// never `verified_at`: that column is a reading of the ENDPOINT volume's clock,
/// which runs behind this one by up to one of its ticks (see
/// [`super::endpoint`]).
pub fn now_unix_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as i64)
        .unwrap_or_default()
}

// ---- errors -----------------------------------------------------------------

/// A transport error surfaced by the [`RemoteObjects`]/[`RemoteRef`] seams. The
/// real daemon maps `ByteStoreError`/`ControlPlaneError` into this; the detail
/// is a `String` because the underlying errors already carry structured tags.
#[derive(Debug)]
pub struct TransportError {
    pub operation: &'static str,
    pub detail: String,
}

impl TransportError {
    pub fn new(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "transport {}: {}", self.operation, self.detail)
    }
}

impl Error for TransportError {}

#[derive(Debug)]
pub enum PushError {
    Io(io::Error),
    Store(ManifestStoreError),
    Manifest(ManifestError),
    Transport(TransportError),
    AncestorRowMissing {
        field: &'static str,
    },
    StreamSealUnsupported {
        byte_len: u64,
        ceiling: u64,
    },
    /// The circuit breaker: this push would remove more of the workspace than any
    /// plausible edit does. Carries the refused paths themselves, not just their
    /// count: a confirmation the user cannot inspect first is not a decision.
    MassDeletionRefused {
        removals: BTreeSet<WorkspacePath>,
        entries: usize,
        threshold: usize,
    },
}

impl fmt::Display for PushError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "push io failed: {error}"),
            Self::Store(error) => write!(formatter, "push store failed: {error}"),
            Self::Manifest(error) => write!(formatter, "push manifest failed: {error}"),
            Self::Transport(error) => write!(formatter, "push {error}"),
            Self::AncestorRowMissing { field } => {
                write!(formatter, "push ancestor row missing {field}")
            }
            Self::StreamSealUnsupported { byte_len, ceiling } => write!(
                formatter,
                "push cannot seal a {byte_len}-byte file: envelope has no streaming seal and the \
                 {ceiling}-byte ceiling would be exceeded"
            ),
            Self::MassDeletionRefused {
                removals,
                entries,
                threshold,
            } => write!(
                formatter,
                "push refused: it would remove {} of {entries} synced entries, above the \
                 {threshold} allowed without confirmation",
                removals.len()
            ),
        }
    }
}

impl Error for PushError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ManifestStoreError> for PushError {
    fn from(error: ManifestStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<TreeError> for PushError {
    fn from(error: TreeError) -> Self {
        match error {
            TreeError::Manifest(error) => Self::Manifest(error),
            TreeError::Store(error) => Self::Store(error),
            TreeError::Transport(error) => Self::Transport(error),
            TreeError::NodeKeyMismatch => Self::Manifest(ManifestError::Internal {
                reason: "published tree node does not match its key",
            }),
        }
    }
}

#[cfg(test)]
#[path = "push/tests.rs"]
mod tests;
