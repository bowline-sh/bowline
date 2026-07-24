//! Pull, the three-way merge matrix, and startup/freshness (Plan 109 Step 5).
//!
//! This module *decides* the reconciliation; the sibling [`apply`] module
//! *executes* it (the apply transaction, crash recovery, and the Git contract),
//! and [`materialize`] holds the leaf filesystem primitives apply composes — the
//! split is forced by the 900-line source gate at the natural decide/execute
//! (and orchestrate/materialize) domain seams. [`intents`] owns the serde
//! payloads persisted in the intent journal.
//!
//! Binding contract (the merge matrix, `classify`): eleven ancestor×local×remote
//! rows, each a named test. Local bytes are always canonical; a divergent remote
//! never overwrites — it materializes as a deterministic conflict-aside. Remote
//! absence carries no deletion authority.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;

use bowline_core::ids::ContentId;

pub mod apply;
pub(crate) mod intents;
pub mod materialize;
pub(crate) mod naming;

use apply::{apply_plan, is_git_lock_path};
use intents::PreimagePayload;

use super::fs_guard::{FileRead, Observed, observe};
use super::manifest::{
    DecodeLimits, DecodedManifest, EntryKind, FileMode, ManifestEntry, ManifestError, ManifestKey,
    PathCollision, WorkspacePath, open_manifest,
};
use super::push::{
    EngineContext, PushError, RefObservation, RemoteObjects, RemoteRef, TransportError, now_unix_ns,
};
use super::store::{FileRecord, ManifestStore, ManifestStoreError, StatFingerprint};

pub use apply::{
    RecoveryAction, RecoveryBoundary, RecoveryObservation, git_apply_rank, git_lock_active,
    recover_intents, recovery_action, recovery_boundary,
};

/// The dependency bundle a pull receives (mirrors `PushDeps`).
pub struct PullDeps<'a, O: RemoteObjects, R: RemoteRef> {
    pub ctx: &'a EngineContext,
    pub objects: &'a O,
    pub refs: &'a R,
}

/// What a pull achieved. `push_again` are paths the driver must reschedule for
/// push (kept-local divergences and freshly materialized asides); `deferred` are
/// paths skipped because a Git lock was active (auto-rescan after it clears).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullOutcome {
    pub applied_manifest_key: Option<ManifestKey>,
    pub ref_version: Option<u64>,
    pub installed: BTreeSet<WorkspacePath>,
    pub deleted: BTreeSet<WorkspacePath>,
    pub conflict_asides: BTreeSet<WorkspacePath>,
    pub push_again: BTreeSet<WorkspacePath>,
    pub deferred: BTreeSet<WorkspacePath>,
    /// True when the remote ref equals the applied ref: nothing to do.
    pub already_current: bool,
}

// ---- startup / freshness ----------------------------------------------------

/// Read the current ref, enforce freshness (monotonic
/// `highest_verified_ref_version`; same-version-different-key is a typed
/// integrity failure that mutates nothing), and pull if the head differs from
/// the applied ref. Recovery of in-flight intents runs first.
pub fn pull<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
) -> Result<PullOutcome, PullError> {
    recover_intents(store, deps)?;

    let observed = deps.refs.read_ref().map_err(PullError::Transport)?;
    pull_observed(store, deps, observed)
}

/// Pull from a ref observation that the transport has already authenticated.
///
/// This is the steady-state reactive fast path. It deliberately shares the
/// same freshness and apply path as [`pull`]; only the redundant synchronous
/// ref query is skipped. Startup, reconnect recovery, barriers, and ambiguous
/// retries continue to call [`pull`].
pub(crate) fn pull_from_observation<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    observed: RefObservation,
) -> Result<PullOutcome, PullError> {
    recover_intents(store, deps)?;
    pull_observed(store, deps, Some(observed))
}

fn pull_observed<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    observed: Option<RefObservation>,
) -> Result<PullOutcome, PullError> {
    let Some(head) = observed else {
        // No ref exists yet: genesis is a push concern; a pull is a no-op.
        return Ok(PullOutcome::default());
    };
    enforce_freshness(store, &head)?;

    let state = store.engine_state()?;
    if state.applied_manifest_key.as_ref() == Some(&head.manifest_key) {
        // An ABA hosted sequence (A -> B -> A while offline) re-presents the
        // applied key at a newer version. Persist that version (applied +
        // ratchet) or every later push CASes against the stale stored version,
        // loses, re-pulls this same already-current key, and livelocks.
        if state.last_ref_version != Some(head.version) {
            store.record_ref_advance(&head.manifest_key, head.version)?;
        }
        return Ok(PullOutcome {
            applied_manifest_key: Some(head.manifest_key),
            ref_version: Some(head.version),
            already_current: true,
            ..PullOutcome::default()
        });
    }
    apply_head(store, deps, &head)
}

fn enforce_freshness(
    store: &mut ManifestStore,
    head: &super::push::RefObservation,
) -> Result<(), PullError> {
    let state = store.engine_state()?;
    if let Some(highest) = state.highest_verified_ref_version {
        if head.version < highest {
            return Err(PullError::RefRegressed {
                observed: head.version,
                highest,
            });
        }
        if head.version == highest
            && state.highest_verified_manifest_key.as_ref() != Some(&head.manifest_key)
        {
            return Err(PullError::RefForked {
                version: head.version,
            });
        }
    }
    // Freshness only REJECTS regressions/forks here; it never advances the ratchet.
    // The advance is persisted in `commit_pull_outcome`, after `apply_head` has
    // fetched, authenticated (sealed crypto), and decoded the head manifest — so a
    // transient missing/corrupt object or a forged high-version ref cannot freeze
    // the ratchet with nothing verified and integrity-stall every legitimate head
    // afterward.
    Ok(())
}

fn apply_head<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    head: &super::push::RefObservation,
) -> Result<PullOutcome, PullError> {
    let plan = decide_head(store, deps, head)?;
    apply_plan(store, deps, plan, &head.manifest_key, head.version)
}

/// Fetch + verify the head manifest and three-way classify it against the
/// ancestor, returning the merge plan *without* touching the filesystem. Split
/// out of [`apply_head`] so the kill-9 matrix (Step 6) can drive the real
/// classification and then execute individual apply stages under barriers,
/// rather than duplicating the merge logic in the test harness.
pub(crate) fn decide_head<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    head: &super::push::RefObservation,
) -> Result<MergePlan, PullError> {
    let sealed = deps
        .objects
        .get_manifest(&head.manifest_key)
        .map_err(PullError::Transport)?;
    if super::manifest::physical_manifest_key(&sealed) != head.manifest_key {
        return Err(PullError::ManifestKeyMismatch);
    }
    // `head_snapshot` is the new engine's flat `Manifest` (distinct from the old
    // `SnapshotManifest` the page-reader gate targets); binding it to a non-
    // `*manifest` name keeps that intent unambiguous.
    let DecodedManifest {
        manifest: head_snapshot,
        collisions,
    } = open_manifest(
        &deps.ctx.crypto,
        &sealed,
        &if deps.ctx.project_view {
            DecodeLimits::project_view()
        } else {
            DecodeLimits::default()
        },
    )
    .map_err(PullError::Manifest)?;

    let ancestor = store.all_files()?;
    classify(deps.ctx, &ancestor, &head_snapshot.entries, &collisions)
}

// ---- three-way classification (the merge matrix) ----------------------------

/// The reconciliation a pull computes before touching disk.
#[derive(Default)]
pub(crate) struct MergePlan {
    pub(crate) fs_ops: Vec<FsOp>,
    pub(crate) ancestor_upserts: BTreeMap<WorkspacePath, FileRecord>,
    pub(crate) ancestor_removals: BTreeSet<WorkspacePath>,
    pub(crate) push_again: BTreeSet<WorkspacePath>,
}

pub(crate) struct FsOp {
    pub(crate) path: WorkspacePath,
    pub(crate) kind: FsOpKind,
    pub(crate) expected: PreimagePayload,
}

pub(crate) enum FsOpKind {
    Install(ManifestEntry),
    Delete,
    // Carries the full remote entry (not just the mode) so the ancestor row a
    // mode-only change writes keeps the file's content identity — otherwise the
    // next push of any path fails `file_record_to_entry` (AncestorRowMissing).
    ModeChange(ManifestEntry),
    ConflictAside(ManifestEntry),
}

fn classify(
    ctx: &EngineContext,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    remote: &BTreeMap<WorkspacePath, ManifestEntry>,
    collisions: &[PathCollision],
) -> Result<MergePlan, PullError> {
    let collided = collision_set(collisions);
    let mut plan = MergePlan::default();
    let mut paths: BTreeSet<&WorkspacePath> = ancestor.keys().collect();
    paths.extend(remote.keys());

    for path in paths {
        if is_git_lock_path(path.as_str()) {
            // Git lockfiles are local-only signals, never manifest entries.
            continue;
        }
        let remote_delta = remote_delta(remote.get(path), ancestor.get(path));
        let local = local_delta(
            ctx,
            path,
            ancestor.get(path),
            remote_delta.requires_verified_local_content(),
        )?;
        // A case-fold collision must never silently clobber: force the aside path.
        let force_aside = collided.contains(path.as_str());
        classify_one(&mut plan, path, &local, &remote_delta, force_aside)?;
    }
    Ok(plan)
}

fn classify_one(
    plan: &mut MergePlan,
    path: &WorkspacePath,
    local: &LocalDelta,
    remote: &RemoteDelta,
    force_aside: bool,
) -> Result<(), PullError> {
    use LocalDelta as L;
    use RemoteDelta as R;
    match (local, remote) {
        // ---- ancestor absent ----
        (L::Absent, R::Created(entry)) => {
            // A colliding create (`Foo` + `foo` on a case-insensitive volume) is
            // NOT silently lost here even though this schedules a plain install for
            // both: the byte-order-least path applies first (ascending apply order)
            // and installs; the later one re-observes the winner present, fails its
            // absent-preimage check, and deflects to a conflict-aside — so only the
            // winner reaches the ancestor, the loser's bytes survive as an aside.
            plan.install(path, entry.clone(), PreimagePayload::absent());
        }
        (L::Untracked { .. }, R::Absent) => {
            plan.push_again.insert(path.clone()); // keep local; next push includes it
        }
        (L::Untracked { content_id, .. }, R::Created(entry)) => {
            if !force_aside && entry_content_id(entry) == content_id.as_ref() {
                plan.adopt(path, local.record_from_observed(entry)?); // identical: adopt, no rewrite
            } else {
                // Aside the remote AND re-push the kept-local original: the aside
                // alone leaves the local path untracked, so if its watcher event was
                // coalesced/lost (restart, ref timing) the original never enters the
                // manifest until an unrelated full scan. Mirror the changed-vs-changed
                // conflict row so local bytes always publish.
                plan.aside(path, entry.clone(), PreimagePayload::absent());
                plan.push_again.insert(path.clone());
            }
        }
        // ---- ancestor present, local unchanged ----
        (L::Unchanged { .. }, R::Absent) => {
            plan.delete(path, local.preimage());
        }
        (L::Unchanged { .. }, R::Changed(entry)) => {
            plan.install(path, entry.clone(), local.preimage());
        }
        (L::Unchanged { record }, R::ModeChanged(entry)) => {
            plan.mode_change(path, entry.clone(), local.preimage());
            let _ = record; // ancestor row rewritten post-mutate
        }
        (L::Unchanged { .. }, R::Unchanged) => {}
        // ---- ancestor present, local deleted ----
        (L::Deleted, R::Absent) => {
            plan.ancestor_removals.insert(path.clone()); // adopt deletion
        }
        (L::Deleted, R::Changed(entry)) => {
            // Keep the deletion; preserve the remote change as an aside.
            plan.aside(path, entry.clone(), PreimagePayload::absent());
            plan.ancestor_removals.insert(path.clone());
        }
        (L::Deleted, R::Unchanged | R::ModeChanged(_)) => {
            plan.push_again.insert(path.clone()); // deletion is local-ahead; push it
        }
        // ---- ancestor present, local changed ----
        (L::Changed { .. } | L::ModeChanged { .. }, R::Absent) => {
            // Remote deleted, local changed: keep local, drop the ancestor row so
            // the path re-pushes as a creation; no aside (remote has no bytes).
            plan.ancestor_removals.insert(path.clone());
            plan.push_again.insert(path.clone());
        }
        (
            L::Changed {
                observed,
                content_id,
            },
            R::Changed(entry),
        ) => {
            if entry_content_id(entry) != content_id.as_ref() {
                // Divergent bytes: keep local canonical, aside the remote.
                plan.aside(path, entry.clone(), local.preimage());
                plan.push_again.insert(path.clone());
            } else if entry_mode(entry) == observed.mode {
                // Identical bytes AND mode: adopt without rewrite (no fs op).
                plan.adopt(path, record_for_entry(entry, observed.fingerprint));
            } else {
                // Identical bytes, divergent mode. The content has converged, so the
                // remote's published mode is authoritative exactly as in the
                // (Unchanged, ModeChanged) row: apply it to disk (the fd-based
                // no-follow `set_mode`) AND record it in the ancestor in one apply
                // pass, via a ModeChange op. Never seal the remote mode into the
                // ancestor without touching disk — that half-adoption leaves
                // ancestor != disk, and the next scan then reads a phantom mode
                // change (or masks a real one). A local chmod that races the apply
                // itself still deflects keep-local through the preimage guard.
                plan.mode_change(path, entry.clone(), local.preimage());
            }
        }
        (L::ModeChanged { observed }, R::ModeChanged(entry)) => {
            if entry_mode(entry) == observed.mode {
                // Both devices settled on the SAME new mode over identical content:
                // converged. Seal the agreed mode into the ancestor (disk already
                // holds it); no fs op and no echo re-seal on the follow-on push.
                plan.adopt(path, record_for_entry(entry, observed.fingerprint));
            } else {
                // Divergent deliberate chmod over identical content. Local's mode is
                // the canonical winner (documented rule, mirroring "local bytes
                // always win"): keep it and leave the ancestor at the true base, so
                // the follow-on push republishes local's mode and ancestor == disk
                // afterwards. The remote's competing mode is dropped; a peer converges
                // when it pulls the republished mode as an (Unchanged, ModeChanged)
                // row. Never adopt the remote mode here — that would seal a mode onto
                // neither disk nor a real base.
                plan.push_again.insert(path.clone());
            }
        }
        (L::Changed { .. } | L::ModeChanged { .. }, R::Unchanged | R::ModeChanged(_)) => {
            plan.push_again.insert(path.clone()); // local ahead; push resolves it
        }
        (L::ModeChanged { .. }, R::Changed(entry)) => {
            plan.aside(path, entry.clone(), local.preimage());
            plan.push_again.insert(path.clone());
        }
        // Remaining pairs are unreachable: an ancestor-absent local (Absent /
        // Untracked) can only meet an ancestor-absent remote (Absent / Created),
        // and an ancestor-present remote delta (Unchanged / Changed / Mode) only
        // arises with an ancestor-present local. The type system cannot express
        // that coupling, so no-op these combinations rather than fabricate work.
        (L::Absent | L::Untracked { .. }, _) | (_, R::Created(_)) => {}
    }
    Ok(())
}

impl MergePlan {
    fn install(&mut self, path: &WorkspacePath, entry: ManifestEntry, expected: PreimagePayload) {
        self.fs_ops.push(FsOp {
            path: path.clone(),
            kind: FsOpKind::Install(entry),
            expected,
        });
    }

    fn aside(&mut self, path: &WorkspacePath, entry: ManifestEntry, expected: PreimagePayload) {
        self.fs_ops.push(FsOp {
            path: path.clone(),
            kind: FsOpKind::ConflictAside(entry),
            expected,
        });
    }

    fn delete(&mut self, path: &WorkspacePath, expected: PreimagePayload) {
        self.fs_ops.push(FsOp {
            path: path.clone(),
            kind: FsOpKind::Delete,
            expected,
        });
    }

    fn mode_change(
        &mut self,
        path: &WorkspacePath,
        entry: ManifestEntry,
        expected: PreimagePayload,
    ) {
        self.fs_ops.push(FsOp {
            path: path.clone(),
            kind: FsOpKind::ModeChange(entry),
            expected,
        });
    }

    fn adopt(&mut self, path: &WorkspacePath, record: FileRecord) {
        self.ancestor_upserts.insert(path.clone(), record);
    }
}

// ---- local / remote deltas --------------------------------------------------

enum LocalDelta {
    Absent,
    Untracked {
        observed: Observed,
        content_id: Option<ContentId>,
    },
    Unchanged {
        record: FileRecord,
    },
    Changed {
        observed: Observed,
        content_id: Option<ContentId>,
    },
    ModeChanged {
        observed: Observed,
    },
    Deleted,
}

impl LocalDelta {
    /// The expected on-disk preimage for the apply-time re-observation.
    fn preimage(&self) -> PreimagePayload {
        match self {
            Self::Absent | Self::Deleted => PreimagePayload::absent(),
            Self::Unchanged { record } => PreimagePayload::from_record(record),
            Self::Untracked {
                observed,
                content_id,
            }
            | Self::Changed {
                observed,
                content_id,
            } => PreimagePayload::from_observed(observed, content_id.clone()),
            Self::ModeChanged { observed } => PreimagePayload::from_observed(observed, None),
        }
    }

    /// Build an ancestor row that adopts the remote identity while carrying the
    /// LOCAL fingerprint (the bytes are already on disk — no rewrite).
    fn record_from_observed(&self, entry: &ManifestEntry) -> Result<FileRecord, PullError> {
        let observed = match self {
            Self::Untracked { observed, .. } | Self::Changed { observed, .. } => observed,
            _ => {
                return Err(PullError::Internal {
                    reason: "adopt without local observation",
                });
            }
        };
        Ok(record_for_entry(entry, observed.fingerprint))
    }
}

fn local_delta(
    ctx: &EngineContext,
    path: &WorkspacePath,
    ancestor: Option<&FileRecord>,
    verify_file_content: bool,
) -> Result<LocalDelta, PullError> {
    let observed = observe(&ctx.workspace_root, path).map_err(PullError::Io)?;
    match (observed, ancestor) {
        (None, None) => Ok(LocalDelta::Absent),
        (None, Some(_)) => Ok(LocalDelta::Deleted),
        (Some(observed), None) => {
            let content_id = maybe_hash(ctx, path, &observed)?;
            Ok(LocalDelta::Untracked {
                observed,
                content_id,
            })
        }
        (Some(observed), Some(record)) => {
            local_vs_record(ctx, path, observed, record, verify_file_content)
        }
    }
}

fn local_vs_record(
    ctx: &EngineContext,
    path: &WorkspacePath,
    observed: Observed,
    record: &FileRecord,
    verify_file_content: bool,
) -> Result<LocalDelta, PullError> {
    if observed.kind != record.kind {
        let content_id = maybe_hash(ctx, path, &observed)?;
        return Ok(LocalDelta::Changed {
            observed,
            content_id,
        });
    }
    match observed.kind {
        EntryKind::Directory if observed.mode == record.mode => {
            return Ok(LocalDelta::Unchanged {
                record: record.clone(),
            });
        }
        EntryKind::Symlink
            if observed.mode == record.mode && observed.symlink_target == record.symlink_target =>
        {
            return Ok(LocalDelta::Unchanged {
                record: record.clone(),
            });
        }
        EntryKind::File
            if !verify_file_content
                && observed.fingerprint == record.fingerprint
                && observed.size == record.size
                && observed.mode == record.mode =>
        {
            return Ok(LocalDelta::Unchanged {
                record: record.clone(),
            });
        }
        EntryKind::Directory | EntryKind::File | EntryKind::Symlink => {}
    }
    // Ambiguous stat: hash to confirm before manufacturing a conflict.
    match observed.kind {
        EntryKind::File => {
            let content_id = match super::fs_guard::read_file_bounded(
                &ctx.workspace_root,
                path,
                ctx.config.max_seal_bytes,
                &observed.expected_file(),
            )
            .map_err(PullError::Push)?
            {
                FileRead::Bytes(bytes) => ctx.crypto.content_id(&bytes),
                // The leaf changed under us (symlink swap / replaced inode): it no
                // longer matches the record, so it is a Changed delta whose content
                // the next scan re-derives against the settled file.
                FileRead::Diverged => {
                    return Ok(LocalDelta::Changed {
                        observed,
                        content_id: None,
                    });
                }
            };
            if Some(&content_id) == record.content_id.as_ref() {
                if observed.mode == record.mode {
                    Ok(LocalDelta::Unchanged {
                        record: record.clone(),
                    })
                } else {
                    Ok(LocalDelta::ModeChanged { observed })
                }
            } else {
                Ok(LocalDelta::Changed {
                    observed,
                    content_id: Some(content_id),
                })
            }
        }
        EntryKind::Symlink => {
            if observed.symlink_target == record.symlink_target && observed.mode == record.mode {
                Ok(LocalDelta::Unchanged {
                    record: record.clone(),
                })
            } else {
                Ok(LocalDelta::Changed {
                    observed,
                    content_id: None,
                })
            }
        }
        EntryKind::Directory => Ok(LocalDelta::ModeChanged { observed }),
    }
}

fn maybe_hash(
    ctx: &EngineContext,
    path: &WorkspacePath,
    observed: &Observed,
) -> Result<Option<ContentId>, PullError> {
    if observed.kind != EntryKind::File {
        return Ok(None);
    }
    match super::fs_guard::read_file_bounded(
        &ctx.workspace_root,
        path,
        ctx.config.max_seal_bytes,
        &observed.expected_file(),
    )
    .map_err(PullError::Push)?
    {
        FileRead::Bytes(bytes) => Ok(Some(ctx.crypto.content_id(&bytes))),
        // The leaf is no longer the regular file we observed: its content id is
        // unknown, so surface None rather than hashing bytes reached no-follow.
        FileRead::Diverged => Ok(None),
    }
}

enum RemoteDelta {
    Absent,
    Created(ManifestEntry),
    Unchanged,
    ModeChanged(ManifestEntry),
    Changed(ManifestEntry),
}

impl RemoteDelta {
    fn requires_verified_local_content(&self) -> bool {
        !matches!(self, Self::Unchanged)
    }
}

fn remote_delta(remote: Option<&ManifestEntry>, ancestor: Option<&FileRecord>) -> RemoteDelta {
    match (remote, ancestor) {
        (None, _) => RemoteDelta::Absent,
        (Some(entry), None) => RemoteDelta::Created(entry.clone()),
        (Some(entry), Some(record)) => {
            if entry_matches_record(entry, record) {
                if entry_mode(entry) == record.mode {
                    RemoteDelta::Unchanged
                } else {
                    RemoteDelta::ModeChanged(entry.clone())
                }
            } else {
                RemoteDelta::Changed(entry.clone())
            }
        }
    }
}

// ---- entry/record helpers (shared with apply + intents) -------------------

pub(crate) fn record_for_entry(entry: &ManifestEntry, fingerprint: StatFingerprint) -> FileRecord {
    match entry {
        ManifestEntry::File {
            size,
            mode,
            content_id,
            blob_key,
            key_epoch,
        } => FileRecord {
            kind: EntryKind::File,
            size: *size,
            mode: *mode,
            symlink_target: None,
            content_id: Some(content_id.clone()),
            blob_key: Some(blob_key.clone()),
            key_epoch: Some(*key_epoch),
            fingerprint,
            hashed_at: Some(now_unix_ns()),
            verified_at: Some(now_unix_ns()),
        },
        ManifestEntry::Directory { mode } => FileRecord {
            kind: EntryKind::Directory,
            size: 0,
            mode: *mode,
            symlink_target: None,
            content_id: None,
            blob_key: None,
            key_epoch: None,
            fingerprint,
            hashed_at: None,
            verified_at: Some(now_unix_ns()),
        },
        ManifestEntry::Symlink { mode, target } => FileRecord {
            kind: EntryKind::Symlink,
            size: 0,
            mode: *mode,
            symlink_target: Some(target.clone()),
            content_id: None,
            blob_key: None,
            key_epoch: None,
            fingerprint,
            hashed_at: None,
            verified_at: Some(now_unix_ns()),
        },
    }
}

pub(crate) fn collision_set(collisions: &[PathCollision]) -> BTreeSet<String> {
    collisions
        .iter()
        .flat_map(|collision| collision.paths.iter())
        .map(|path| path.as_str().to_string())
        .collect()
}

pub(crate) fn entry_mode(entry: &ManifestEntry) -> FileMode {
    match entry {
        ManifestEntry::File { mode, .. }
        | ManifestEntry::Directory { mode }
        | ManifestEntry::Symlink { mode, .. } => *mode,
    }
}

pub(crate) fn entry_content_id(entry: &ManifestEntry) -> Option<&ContentId> {
    match entry {
        ManifestEntry::File { content_id, .. } => Some(content_id),
        _ => None,
    }
}

pub(crate) fn entry_matches_record(entry: &ManifestEntry, record: &FileRecord) -> bool {
    match entry {
        ManifestEntry::File { content_id, .. } => {
            record.kind == EntryKind::File && record.content_id.as_ref() == Some(content_id)
        }
        ManifestEntry::Directory { .. } => record.kind == EntryKind::Directory,
        ManifestEntry::Symlink { target, .. } => {
            record.kind == EntryKind::Symlink && record.symlink_target.as_deref() == Some(target)
        }
    }
}

// ---- errors -----------------------------------------------------------------

#[derive(Debug)]
pub enum PullError {
    Io(io::Error),
    Store(ManifestStoreError),
    Manifest(ManifestError),
    Push(PushError),
    Transport(TransportError),
    ManifestKeyMismatch,
    BlobKeyMismatch,
    RefRegressed { observed: u64, highest: u64 },
    RefForked { version: u64 },
    Internal { reason: &'static str },
}

impl fmt::Display for PullError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "pull io failed: {error}"),
            Self::Store(error) => write!(formatter, "pull store failed: {error}"),
            Self::Manifest(error) => write!(formatter, "pull manifest failed: {error}"),
            Self::Push(error) => write!(formatter, "pull scan failed: {error}"),
            Self::Transport(error) => write!(formatter, "pull {error}"),
            Self::ManifestKeyMismatch => {
                formatter.write_str("pulled manifest key does not match ref")
            }
            Self::BlobKeyMismatch => formatter.write_str("pulled blob key does not match manifest"),
            Self::RefRegressed { observed, highest } => write!(
                formatter,
                "ref regressed: observed {observed} below verified {highest}"
            ),
            Self::RefForked { version } => {
                write!(
                    formatter,
                    "ref forked at version {version} with a different key"
                )
            }
            Self::Internal { reason } => write!(formatter, "pull internal invariant: {reason}"),
        }
    }
}

impl Error for PullError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Push(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ManifestStoreError> for PullError {
    fn from(error: ManifestStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<PushError> for PullError {
    fn from(error: PushError) -> Self {
        Self::Push(error)
    }
}

#[cfg(test)]
#[path = "pull_apply/tests.rs"]
mod tests;
