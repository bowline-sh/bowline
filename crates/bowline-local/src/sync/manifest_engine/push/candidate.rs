//! Deriving the immutable candidate delta one push will publish, and the
//! refusals that decide what must never reach a manifest.
//!
//! Split from `push.rs` at the domain seam between *deciding* the delta (here)
//! and *publishing* it (uploads, CAS, the ancestor commit). Everything in this
//! module is pure with respect to the hosted transport except the blob uploads a
//! changed file triggers; nothing here mutates the ancestor.

use std::collections::{BTreeMap, BTreeSet};

use bowline_core::workspace_graph::symlink_target_stays_in_workspace;

use super::super::endpoint::{StatTrust, prove_rows};
use super::super::fs_guard::{
    FileRead, FileVisit, ObserveOutcome, Observed, observe_classified, read_file_bounded,
    visit_file_bounded,
};
use super::super::manifest::{
    EntryKind, MAX_WORKSPACE_PATH_LEN, ManifestEntry, WorkspacePath, publishable_workspace_path,
};
use super::super::store::{AncestorCommit, FileRecord, ManifestStore};
use super::super::unsyncable::{UnsyncableReason, UnsyncableRecord, path_scoped_reason};
use super::{
    BlobLedger, DeletionAuthorization, PushDeps, PushError, RemoteObjects, RemoteRef,
    mass_deletion_threshold, now_unix_ns, upload_file_blob, upload_file_blob_segmented,
};

/// The immutable delta a scan produced. Never touches the store until CAS
/// success turns it into an [`AncestorCommit`].
#[derive(Default)]
pub(super) struct Candidate {
    pub(super) upserts: BTreeMap<WorkspacePath, (FileRecord, ManifestEntry)>,
    pub(super) removals: BTreeSet<WorkspacePath>,
    pub(super) local_refreshes: BTreeMap<WorkspacePath, FileRecord>,
    /// Dirty paths that exist but cannot be represented or read. They are NOT
    /// removals: publishing a removal for a path the engine merely failed to open
    /// would delete the user's file on every other device.
    pub(super) unsyncable: BTreeMap<WorkspacePath, UnsyncableRecord>,
    /// Dirty paths a scan could not settle this cycle (twice-diverged: actively
    /// being written). NOT part of the published delta — carried alongside so the
    /// driver retains them in its dirty set and reschedules a rescan. `is_empty`
    /// deliberately ignores this: a batch of only-skipped paths is `NoChange`, no
    /// upload and no CAS, yet the paths are still handed back to the driver.
    pub(super) skipped: BTreeSet<WorkspacePath>,
    /// Exactly the paths this scan examined, in the spelling the manifest
    /// publishes them under. The unsyncable ledger clears against this rather
    /// than the raw dirty set, so a path the endpoint's fold renamed is retired
    /// under the same key `unsyncable` recorded it under.
    pub(super) scanned: BTreeSet<WorkspacePath>,
}

impl Candidate {
    pub(super) fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.removals.is_empty()
    }

    pub(super) fn ancestor_commit(&self) -> AncestorCommit {
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

    /// Paths this scan conclusively settled. Churning and unsyncable paths stay
    /// durable in `pending_push` so a crash before their retry cannot erase the
    /// only reminder that a follow-on publication is still required.
    pub(super) fn settled_paths(&self) -> BTreeSet<WorkspacePath> {
        self.scanned
            .iter()
            .filter(|path| !self.skipped.contains(*path) && !self.unsyncable.contains_key(*path))
            .cloned()
            .collect()
    }
}

pub(super) fn build_candidate<O: RemoteObjects, R: RemoteRef>(
    deps: &PushDeps<'_, O, R>,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    dirty_paths: &BTreeSet<WorkspacePath>,
    trust: StatTrust,
    ledger: &mut BlobLedger<'_>,
) -> Result<Candidate, PushError> {
    let mut candidate = Candidate::default();

    // Every path this push touches is first put in the spelling the manifest
    // publishes it under. A dirty set can hold both the NFD name macOS handed
    // the watcher and the NFC name a peer's manifest installed, which on a
    // normalization-insensitive volume are ONE file: scanning both would publish
    // one file as two entries, and apply on the next pull would ping-pong
    // between them. Collapsing here also dedupes them into a single scan.
    let publish: BTreeSet<WorkspacePath> = dirty_paths
        .iter()
        .map(|path| deps.ctx.names.canonical_spelling(path))
        .collect();

    for path in &publish {
        // A dirty path the writer must never publish (private engine state, an
        // unnormalizable name) is rejected here rather than sealed: the reader's
        // decode would reject it and take that peer's engine down with it.
        if let Err(rejection) =
            publishable_workspace_path(path.as_str(), MAX_WORKSPACE_PATH_LEN, deps.ctx.project_view)
        {
            candidate.unsyncable.insert(
                path.clone(),
                UnsyncableRecord::new(rejection.into(), None, now_unix_ns()),
            );
            continue;
        }
        match scan_path(deps, ancestor, path, ledger, trust)? {
            PathScan::Upsert(entry) => {
                candidate.upserts.insert(path.clone(), *entry);
            }
            PathScan::Remove => {
                // A pull conflict can intentionally remove the local ancestor
                // row while the applied remote root still contains the path.
                // Carry the tombstone; the copy-on-write patcher proves whether
                // it changes that root and makes an already-absent path a no-op.
                candidate.removals.insert(path.clone());
            }
            PathScan::Unsyncable(record) => {
                candidate.unsyncable.insert(path.clone(), *record);
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
    expand_subtree_removals(&mut candidate, ancestor);
    candidate.scanned = publish;
    // The last step of building the candidate, so what is handed back is
    // complete and frozen: every row it carries is stamped with the endpoint
    // instant that proves it, or with none. A row builder cannot do this for
    // itself — the proof is a fact about the volume's clock AFTER the
    // observation, and a row that stamps its own `SystemTime::now()` claims a
    // proof it never had (see [`super::super::endpoint`]).
    prove_rows(
        &deps.ctx.workspace_root,
        deps.ctx.endpoint_probe_root(),
        deps.ctx.timestamps,
        &deps.ctx.crypto,
        deps.ctx.config.max_seal_bytes,
        candidate
            .upserts
            .iter_mut()
            .map(|(path, (record, _))| (path, record))
            .chain(candidate.local_refreshes.iter_mut()),
    );
    Ok(candidate)
}

/// A manifest has no implicit directory membership: replacing or deleting one
/// directory entry must explicitly remove every tracked descendant. The scoped
/// ancestor includes those prefix rows, so expanding here makes the SQLite
/// ancestor commit, mass-deletion guard, and remote tree patch operate on the
/// same complete deletion set.
fn expand_subtree_removals(
    candidate: &mut Candidate,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
) {
    let destructive_roots = candidate
        .removals
        .iter()
        .cloned()
        .chain(candidate.upserts.iter().filter_map(|(path, (_, entry))| {
            let replaces_directory = ancestor
                .get(path)
                .is_some_and(|record| record.kind == EntryKind::Directory)
                && !matches!(entry, ManifestEntry::Directory { .. });
            replaces_directory.then(|| path.clone())
        }))
        .collect::<BTreeSet<_>>();

    if destructive_roots.is_empty() {
        return;
    }

    let is_below_destructive_root = |path: &WorkspacePath| {
        destructive_roots
            .iter()
            .any(|root| path != root && is_descendant(path, root))
    };
    candidate.removals.extend(
        ancestor
            .keys()
            .filter(|path| is_below_destructive_root(path))
            .cloned(),
    );
    candidate
        .upserts
        .retain(|path, _| !is_below_destructive_root(path));
    candidate
        .local_refreshes
        .retain(|path, _| !is_below_destructive_root(path));
}

fn is_descendant(path: &WorkspacePath, root: &WorkspacePath) -> bool {
    path.as_str()
        .strip_prefix(root.as_str())
        .is_some_and(|suffix| suffix.starts_with('/'))
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
    /// The path exists but cannot be represented or read. Distinct from `Remove`
    /// (which publishes a deletion) and from an error (which used to kill the
    /// engine): the ancestor row, if any, is left exactly as it is.
    Unsyncable(Box<UnsyncableRecord>),
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
    Unsyncable(Box<UnsyncableRecord>),
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
    ledger: &mut BlobLedger<'_>,
    trust: StatTrust,
) -> Result<PathScan, PushError> {
    for _ in 0..2 {
        let observed = match observe_classified(&deps.ctx.workspace_root, path) {
            ObserveOutcome::Present(observed) => observed,
            ObserveOutcome::Absent => return Ok(PathScan::Remove),
            // Present but unreadable/unrepresentable. NEVER a removal: the file is
            // still there, and publishing its absence would delete it everywhere.
            ObserveOutcome::Unsyncable(reason) => {
                return Ok(PathScan::Unsyncable(Box::new(UnsyncableRecord::new(
                    reason,
                    None,
                    now_unix_ns(),
                ))));
            }
        };
        let ancestor_row = ancestor.get(path);
        match scan_observed(deps, path, &observed, ancestor_row, ledger, trust)? {
            ScanResult::Entry(entry) => return Ok(PathScan::Upsert(entry)),
            ScanResult::LocalRefresh(record) => return Ok(PathScan::LocalRefresh(record)),
            ScanResult::Unchanged => return Ok(PathScan::Settled),
            ScanResult::Unsyncable(record) => return Ok(PathScan::Unsyncable(record)),
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
    ledger: &mut BlobLedger<'_>,
    trust: StatTrust,
) -> Result<ScanResult, PushError> {
    match observed.kind {
        EntryKind::Directory => Ok(directory_scan(observed, ancestor_row)),
        EntryKind::Symlink => Ok(symlink_scan(path, observed, ancestor_row)),
        EntryKind::File => file_candidate(deps, path, observed, ancestor_row, ledger, trust),
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
///
/// A link that resolves outside the workspace is refused before the unchanged
/// comparison, not after: publishing one hands every peer a link their own tools
/// would follow out of the workspace, and checking first also catches a row an
/// older binary already wrote into the ancestor.
fn symlink_scan(
    path: &WorkspacePath,
    observed: &Observed,
    ancestor_row: Option<&FileRecord>,
) -> ScanResult {
    let observed_target = observed.symlink_target.clone().unwrap_or_default();
    if !symlink_target_stays_in_workspace(path.as_str(), &observed_target) {
        return ScanResult::Unsyncable(Box::new(UnsyncableRecord::new(
            UnsyncableReason::EscapingSymlinkTarget,
            None,
            now_unix_ns(),
        )));
    }
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
            // Stamped by `endpoint::prove_rows` once the scan is complete; a row
            // may never assert its own observation instant.
            verified_at: None,
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
            verified_at: None,
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
    ledger: &mut BlobLedger<'_>,
    trust: StatTrust,
) -> Result<ScanResult, PushError> {
    // Stat-clean and same kind: nothing changed. Never open the file. `trust`
    // owns the whole stat argument — the fingerprint comparison at the
    // resolution this endpoint actually records, AND whether that match proves
    // anything. A stat inside the racily-clean window, or one taken after an
    // unbounded unobserved gap, is read instead (see
    // [`super::super::endpoint`]).
    if let Some(row) = ancestor_row
        && row.kind == EntryKind::File
        && row.size == observed.size
        && row.mode == observed.mode
        && trust.settles(row, observed)
    {
        return Ok(ScanResult::Unchanged);
    }

    let is_segmented = observed.size >= deps.ctx.config.large_file_threshold;
    let (content_id, plaintext) = if is_segmented {
        let visited = visit_file_bounded(
            &deps.ctx.workspace_root,
            path,
            deps.ctx.config.max_seal_bytes,
            &observed.expected_file(),
            |file, _| {
                deps.ctx
                    .crypto
                    .content_id_reader(file)
                    .map_err(PushError::Io)
            },
        );
        match visited {
            Ok(FileVisit::Value((content_id, byte_len))) if byte_len == observed.size => {
                deps.ctx.counters.record_content_open(byte_len);
                (content_id, None)
            }
            Ok(FileVisit::Value(_)) | Ok(FileVisit::Diverged) => {
                return Ok(ScanResult::Diverged);
            }
            Err(error) => {
                return Ok(ScanResult::Unsyncable(Box::new(read_rejection(error)?)));
            }
        }
    } else {
        let plaintext = match read_file_bounded(
            &deps.ctx.workspace_root,
            path,
            deps.ctx.config.max_seal_bytes,
            &observed.expected_file(),
        ) {
            Ok(FileRead::Bytes(plaintext)) => plaintext,
            // The leaf was not the regular file we observed (symlink swap,
            // replaced inode, symlinked parent): re-observe rather than seal
            // foreign bytes.
            Ok(FileRead::Diverged) => return Ok(ScanResult::Diverged),
            // One unreadable or oversize file is that file's problem. Reporting
            // it as a path-scoped divergence keeps the workspace syncing.
            Err(error) => {
                return Ok(ScanResult::Unsyncable(Box::new(read_rejection(error)?)));
            }
        };
        deps.ctx
            .counters
            .record_content_open(plaintext.len() as u64);
        (deps.ctx.crypto.content_id(&plaintext), Some(plaintext))
    };
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
        refreshed.verified_at = None;
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
        _ if is_segmented => {
            let Some(blob_key) = upload_file_blob_segmented(
                deps,
                path,
                &observed.expected_file(),
                &content_id,
                ledger,
            )?
            else {
                return Ok(ScanResult::Diverged);
            };
            blob_key
        }
        _ => upload_file_blob(
            deps,
            &content_id,
            plaintext.as_deref().ok_or(PushError::Manifest(
                super::super::manifest::ManifestError::Internal {
                    reason: "buffered file lost plaintext",
                },
            ))?,
            ledger,
        )?,
    };

    let size = observed.size;
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
            verified_at: None,
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

/// Classify a failed content read as a path-scoped unsyncable condition.
///
/// A read that named exactly one file and failed is a fact about that file, so
/// every errno lands here — including the ones the engine does not model by name
/// ([`path_scoped_reason`]). The previous `None => Err(...)` arm meant an EIO on
/// one file classified as `CycleError::Fatal` and killed sync for the whole
/// workspace, which is precisely what this module's `Unsyncable` channel exists
/// to prevent. Only a non-read failure is given back.
fn read_rejection(error: PushError) -> Result<UnsyncableRecord, PushError> {
    match error {
        PushError::StreamSealUnsupported { .. } => Ok(UnsyncableRecord::new(
            UnsyncableReason::AboveSealCeiling,
            None,
            now_unix_ns(),
        )),
        PushError::Io(error) => Ok(UnsyncableRecord::new(
            path_scoped_reason(&error),
            error.raw_os_error(),
            now_unix_ns(),
        )),
        other => Err(other),
    }
}

/// Persist the unsyncable verdict for exactly the paths this push examined:
/// record the ones that failed and clear the ones that now succeed, so a user who
/// fixes a permission sees the attention item disappear on the next cycle.
pub(super) fn record_unsyncable_outcome(
    store: &mut ManifestStore,
    candidate: &Candidate,
) -> Result<(), PushError> {
    let resolved: BTreeSet<WorkspacePath> = candidate
        .scanned
        .iter()
        .filter(|path| !candidate.unsyncable.contains_key(*path))
        .cloned()
        .collect();
    store.record_unsyncable(&candidate.unsyncable, &resolved)?;
    Ok(())
}

/// Refuse to publish a manifest that deletes an implausible share of the
/// workspace. Every guard upstream (the root sentinel, the unsyncable
/// classification) exists to stop a removal batch being manufactured in the first
/// place; this is the last one, and it is the one that holds when a future bug
/// invents a new way to produce the same batch.
pub(super) fn guard_mass_deletion(
    ancestor_count: usize,
    scoped_ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    durable_pending: &BTreeSet<WorkspacePath>,
    candidate: &Candidate,
    deletions: DeletionAuthorization,
) -> Result<(), PushError> {
    let removals = candidate
        .removals
        .iter()
        .filter(|path| scoped_ancestor.contains_key(*path) || durable_pending.contains(*path))
        .cloned()
        .collect::<BTreeSet<_>>();
    match deletions {
        DeletionAuthorization::ExplicitOperation => return Ok(()),
        DeletionAuthorization::ConfirmedPaths(confirmed)
            if removals.is_subset(confirmed.as_ref()) =>
        {
            return Ok(());
        }
        DeletionAuthorization::Enforce | DeletionAuthorization::ConfirmedPaths(_) => {}
    }
    let threshold = mass_deletion_threshold(ancestor_count);
    if removals.len() > threshold {
        return Err(PushError::MassDeletionRefused {
            removals,
            entries: ancestor_count,
            threshold,
        });
    }
    Ok(())
}
