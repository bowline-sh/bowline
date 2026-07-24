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
//! side and apply's write side share ([`observe`], [`read_file_bounded`],
//! [`prepare_parent_chain`]) has earned its own seam in [`super::fs_guard`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bowline_core::ids::{ContentId, DeviceId};

use super::fs_guard::{FileRead, Observed, observe, read_file_bounded, write_private_file};
use super::manifest::{
    BlobKey, EntryKind, KeyEpoch, Manifest, ManifestEntry, ManifestError, ManifestKey,
    WorkspaceCrypto, WorkspacePath, physical_blob_key, physical_manifest_key, seal_file,
    seal_manifest,
};
use super::store::{AncestorCommit, FileRecord, ManifestStore, ManifestStoreError};

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

/// One push attempt over `dirty_paths`. See the module contract: the ancestor is
/// never mutated except on CAS success.
pub fn push<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    dirty_paths: &BTreeSet<WorkspacePath>,
) -> Result<PushOutcome, PushError> {
    push_with_content_verification(store, deps, dirty_paths, false)
}

/// Push paths whose bytes must be verified even when their stat fingerprint is
/// unchanged. Work views use this for explicit review/accept operations because
/// they do not have a continuously running native watcher: a same-size rewrite
/// can otherwise be invisible on filesystems whose timestamp clock has not
/// advanced since materialization.
pub(super) fn push_verifying_dirty_files<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    dirty_paths: &BTreeSet<WorkspacePath>,
) -> Result<PushOutcome, PushError> {
    push_with_content_verification(store, deps, dirty_paths, true)
}

fn push_with_content_verification<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PushDeps<'_, O, R>,
    dirty_paths: &BTreeSet<WorkspacePath>,
    verify_file_content: bool,
) -> Result<PushOutcome, PushError> {
    let ancestor = store.all_files()?;
    let state = store.engine_state()?;

    let candidate = build_candidate(deps, &ancestor, dirty_paths, verify_file_content)?;
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

    let manifest = build_manifest(&ancestor, &candidate, deps.ctx.key_epoch())?;
    let manifest_key = upload_manifest(deps, &manifest)?;

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

// ---- candidate map (immutable in-memory delta) ------------------------------

/// The immutable delta a scan produced. Never touches the store until CAS
/// success turns it into an [`AncestorCommit`].
#[derive(Default)]
struct Candidate {
    upserts: BTreeMap<WorkspacePath, (FileRecord, ManifestEntry)>,
    removals: BTreeSet<WorkspacePath>,
    local_refreshes: BTreeMap<WorkspacePath, FileRecord>,
    /// Dirty paths a scan could not settle this cycle (twice-diverged: actively
    /// being written). NOT part of the published delta — carried alongside so the
    /// driver retains them in its dirty set and reschedules a rescan. `is_empty`
    /// deliberately ignores this: a batch of only-skipped paths is `NoChange`, no
    /// upload and no CAS, yet the paths are still handed back to the driver.
    skipped: BTreeSet<WorkspacePath>,
}

impl Candidate {
    fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.removals.is_empty()
    }

    fn ancestor_commit(&self) -> AncestorCommit {
        let mut upserts = self.local_refreshes.clone();
        upserts.extend(
            self.upserts
                .iter()
                .map(|(path, (record, _))| (path.clone(), record.clone())),
        );
        AncestorCommit {
            upserts,
            removals: self.removals.clone(),
        }
    }
}

fn build_candidate<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    dirty_paths: &BTreeSet<WorkspacePath>,
    verify_file_content: bool,
) -> Result<Candidate, PushError> {
    let mut candidate = Candidate::default();
    // Dedup identical content within one batch so two paths sharing bytes upload
    // one blob (the create-only PUT would otherwise re-seal to a fresh key).
    let mut uploaded: HashMap<ContentId, BlobKey> = HashMap::new();

    for path in dirty_paths {
        match scan_path(deps, ancestor, path, &mut uploaded, verify_file_content)? {
            PathScan::Upsert(entry) => {
                candidate.upserts.insert(path.clone(), *entry);
            }
            PathScan::Remove => {
                if ancestor.contains_key(path) {
                    candidate.removals.insert(path.clone());
                }
            }
            PathScan::LocalRefresh(record) => {
                candidate.local_refreshes.insert(path.clone(), *record);
            }
            // Fingerprint-clean / nothing to publish: drop from the dirty set.
            PathScan::Settled => {}
            // Twice-diverged (actively being written): retain so the driver
            // rescans it in a later cycle rather than losing the change.
            PathScan::Retry => {
                candidate.skipped.insert(path.clone());
            }
        }
    }
    Ok(candidate)
}

/// What one dirty path contributes to the candidate delta. The upsert payload is
/// boxed because a [`FileRecord`] dwarfs the unit variants. `Settled` and `Retry`
/// are distinct no-delta outcomes: `Settled` means nothing changed and the path
/// leaves the dirty set; `Retry` means the path is churning under us and MUST be
/// rescanned later — conflating them would either lose a real change (dropping a
/// churning path) or spin forever (retaining a clean one).
enum PathScan {
    Upsert(Box<(FileRecord, ManifestEntry)>),
    Remove,
    LocalRefresh(Box<FileRecord>),
    Settled,
    Retry,
}

/// The outcome of scanning one observation. `Diverged` means the observed
/// regular file was not the object we opened (symlink swap, replaced inode,
/// symlinked parent), so the caller must re-observe. The entry payload is boxed
/// for the same size reason as [`PathScan`].
enum ScanResult {
    Entry(Box<(FileRecord, ManifestEntry)>),
    LocalRefresh(Box<FileRecord>),
    Unchanged,
    Diverged,
}

/// Observe a dirty path and derive its candidate contribution, re-observing when
/// a content read finds the leaf is no longer the regular file we stat'd. A
/// content read that diverges (leaf swapped to a symlink, replaced inode, or a
/// parent turned into a symlink) re-observes and re-derives: a fresh symlink is
/// recorded AS a symlink, a vanished file becomes a removal, a settled edit seals
/// its real bytes. A SECOND divergence means the path is churning under us —
/// skip it this round and let the next scan settle it. Bytes reached through a
/// symlink are NEVER sealed.
fn scan_path<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    path: &WorkspacePath,
    uploaded: &mut HashMap<ContentId, BlobKey>,
    verify_file_content: bool,
) -> Result<PathScan, PushError> {
    for _ in 0..2 {
        let Some(observed) = observe(&deps.ctx.workspace_root, path).map_err(PushError::Io)? else {
            return Ok(PathScan::Remove);
        };
        let ancestor_row = ancestor.get(path);
        match scan_observed(
            deps,
            path,
            &observed,
            ancestor_row,
            uploaded,
            verify_file_content,
        )? {
            ScanResult::Entry(entry) => return Ok(PathScan::Upsert(entry)),
            ScanResult::LocalRefresh(record) => return Ok(PathScan::LocalRefresh(record)),
            ScanResult::Unchanged => return Ok(PathScan::Settled),
            ScanResult::Diverged => continue,
        }
    }
    // Two consecutive divergences: the path is being actively written. Ask the
    // driver to retain and rescan it — this is NOT a settled no-op.
    Ok(PathScan::Retry)
}

/// Turn one observed path into a candidate entry, uploading its blob if the
/// content is new. `Unchanged` is the invariant-C1 "unchanged files are never
/// opened" path; `Diverged` asks the caller to re-observe.
fn scan_observed<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    path: &WorkspacePath,
    observed: &Observed,
    ancestor_row: Option<&FileRecord>,
    uploaded: &mut HashMap<ContentId, BlobKey>,
    verify_file_content: bool,
) -> Result<ScanResult, PushError> {
    match observed.kind {
        EntryKind::Directory => Ok(directory_scan(observed, ancestor_row)),
        EntryKind::Symlink => Ok(symlink_scan(observed, ancestor_row)),
        EntryKind::File => file_candidate(
            deps,
            path,
            observed,
            ancestor_row,
            uploaded,
            verify_file_content,
        ),
    }
}

/// A directory observation is unchanged when the ancestor already records a
/// directory with the same mode. Watchers routinely re-report a parent dir while
/// a child is edited, and applying a remote dir generates a local event — so
/// without this the echo builds and seals a fresh manifest and advances the ref
/// even though canonical state is identical (violating invariants C1/C2). Mirror
/// [`file_candidate`]'s ancestor-comparison discipline.
fn directory_scan(observed: &Observed, ancestor_row: Option<&FileRecord>) -> ScanResult {
    if let Some(row) = ancestor_row
        && row.kind == EntryKind::Directory
        && row.mode == observed.mode
    {
        return ScanResult::Unchanged;
    }
    ScanResult::Entry(Box::new(directory_candidate(observed)))
}

/// A symlink observation is unchanged when the ancestor records a symlink with
/// the same mode AND target; a retargeted or chmod'ed link still pushes. The
/// target is normalized the way [`symlink_candidate`] stores it (a missing target
/// round-trips to the empty string) so an echoed link never re-seals a manifest.
fn symlink_scan(observed: &Observed, ancestor_row: Option<&FileRecord>) -> ScanResult {
    let observed_target = observed.symlink_target.clone().unwrap_or_default();
    if let Some(row) = ancestor_row
        && row.kind == EntryKind::Symlink
        && row.mode == observed.mode
        && row.symlink_target.as_deref() == Some(observed_target.as_str())
    {
        return ScanResult::Unchanged;
    }
    ScanResult::Entry(Box::new(symlink_candidate(observed)))
}

fn directory_candidate(observed: &Observed) -> (FileRecord, ManifestEntry) {
    (
        FileRecord {
            kind: EntryKind::Directory,
            size: 0,
            mode: observed.mode,
            symlink_target: None,
            content_id: None,
            blob_key: None,
            key_epoch: None,
            fingerprint: observed.fingerprint,
            hashed_at: None,
            verified_at: Some(now_unix_ns()),
        },
        ManifestEntry::Directory {
            mode: observed.mode,
        },
    )
}

fn symlink_candidate(observed: &Observed) -> (FileRecord, ManifestEntry) {
    let target = observed.symlink_target.clone().unwrap_or_default();
    (
        FileRecord {
            kind: EntryKind::Symlink,
            size: 0,
            mode: observed.mode,
            symlink_target: Some(target.clone()),
            content_id: None,
            blob_key: None,
            key_epoch: None,
            fingerprint: observed.fingerprint,
            hashed_at: None,
            verified_at: Some(now_unix_ns()),
        },
        ManifestEntry::Symlink {
            mode: observed.mode,
            target,
        },
    )
}

fn file_candidate<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    path: &WorkspacePath,
    observed: &Observed,
    ancestor_row: Option<&FileRecord>,
    uploaded: &mut HashMap<ContentId, BlobKey>,
    verify_file_content: bool,
) -> Result<ScanResult, PushError> {
    // Fingerprint-clean and same kind: nothing changed. Never open the file.
    if !verify_file_content
        && let Some(row) = ancestor_row
        && row.kind == EntryKind::File
        && row.fingerprint == observed.fingerprint
        && row.size == observed.size
        && row.mode == observed.mode
    {
        return Ok(ScanResult::Unchanged);
    }

    let plaintext = match read_file_bounded(
        &deps.ctx.workspace_root,
        path,
        deps.ctx.config.max_seal_bytes,
        &observed.expected_file(),
    )? {
        FileRead::Bytes(plaintext) => plaintext,
        // The leaf was not the regular file we observed (symlink swap, replaced
        // inode, symlinked parent): re-observe rather than seal foreign bytes.
        FileRead::Diverged => return Ok(ScanResult::Diverged),
    };
    // One real content open + hash of a changed file (invariant C2: an edit
    // costs the edit; an unchanged file never reaches here).
    deps.ctx
        .counters
        .record_content_open(plaintext.len() as u64);
    let content_id = deps.ctx.crypto.content_id(&plaintext);
    deps.ctx.counters.record_content_hash();

    if let Some(row) = ancestor_row
        && row.kind == EntryKind::File
        && row.content_id.as_ref() == Some(&content_id)
        && row.mode == observed.mode
        && row.key_epoch == Some(deps.ctx.key_epoch())
    {
        let mut refreshed = row.clone();
        refreshed.size = observed.size;
        refreshed.fingerprint = observed.fingerprint;
        refreshed.hashed_at = Some(now_unix_ns());
        refreshed.verified_at = Some(now_unix_ns());
        return Ok(ScanResult::LocalRefresh(Box::new(refreshed)));
    }

    let blob_key = match ancestor_row {
        // Content unchanged (mode-only edit): reference the ancestor blob, no
        // upload or content re-seal moves (matrix row 11 on the push side).
        Some(row)
            if row.content_id.as_ref() == Some(&content_id)
                && row.key_epoch == Some(deps.ctx.key_epoch()) =>
        {
            row.blob_key
                .clone()
                .ok_or(PushError::AncestorRowMissing { field: "blob_key" })?
        }
        _ => upload_file_blob(deps, &content_id, &plaintext, uploaded)?,
    };

    let size = plaintext.len() as u64;
    let key_epoch = deps.ctx.key_epoch();
    Ok(ScanResult::Entry(Box::new((
        FileRecord {
            kind: EntryKind::File,
            size,
            mode: observed.mode,
            symlink_target: None,
            content_id: Some(content_id.clone()),
            blob_key: Some(blob_key.clone()),
            key_epoch: Some(key_epoch),
            fingerprint: observed.fingerprint,
            hashed_at: Some(now_unix_ns()),
            verified_at: Some(now_unix_ns()),
        },
        ManifestEntry::File {
            size,
            mode: observed.mode,
            content_id,
            blob_key,
            key_epoch,
        },
    ))))
}

// ---- upload ---------------------------------------------------------------

fn upload_file_blob<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    content_id: &ContentId,
    plaintext: &[u8],
    uploaded: &mut HashMap<ContentId, BlobKey>,
) -> Result<BlobKey, PushError> {
    if let Some(existing) = uploaded.get(content_id) {
        return Ok(existing.clone());
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
    uploaded.insert(content_id.clone(), key.clone());
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

fn upload_manifest<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    manifest: &Manifest,
) -> Result<ManifestKey, PushError> {
    let plaintext = manifest.to_canonical_bytes().map_err(PushError::Manifest)?;
    let content_id = deps.ctx.crypto.manifest_content_id(&plaintext);
    let sealed = seal_manifest(&deps.ctx.crypto, &plaintext).map_err(PushError::Manifest)?;
    let key = physical_manifest_key(sealed.as_bytes());
    deps.objects
        .put_manifest(ManifestUpload {
            key: &key,
            content_id: &content_id,
            key_epoch: deps.ctx.key_epoch(),
            sealed: sealed.as_bytes(),
        })
        .map_err(PushError::Transport)?;
    deps.ctx.counters.record_manifest_upload();
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

/// Unix nanoseconds since the epoch, for the `hashed_at`/`verified_at` audit
/// columns. Never orders conflicts (Plan 108: no clock ordering).
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
    AncestorRowMissing { field: &'static str },
    StreamSealUnsupported { byte_len: u64, ceiling: u64 },
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

#[cfg(test)]
#[path = "push/tests.rs"]
mod tests;
