//! The apply transaction and crash recovery for pull (Plan 109 Step 5).
//!
//! Split from `pull_apply.rs` because the merge + apply machinery exceeds the
//! 900-line source gate; the seam is the domain boundary between *deciding* the
//! merge (parent module) and *executing* it against the filesystem (here). The
//! leaf filesystem primitives this transaction composes live in the sibling
//! [`super::materialize`] module, and the rules for writing into a Git working
//! copy live in [`super::git_contract`]. Every mutation is intent-journalled,
//! re-observes its preimage immediately before touching disk, and never
//! overwrites a racing user write.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;

use super::git_contract::{GitLockCache, git_apply_rank};
use super::intents::{
    IntentOpTag, PreimagePayload, TargetRecordPayload, decode, encode, recovery_facts,
    target_payload,
};
use super::materialize::{
    DeleteOutcome, Materialized, TempFile, aside_already_materialized, checked_delete,
    install_entry, materialize_aside, quarantine_dir, reinstall_from_download, release_quarantine,
    set_mode, stage_write_temp,
};
use super::naming::{quarantine_leaf, quarantine_name};
mod applied_outcome;

use super::prefetch::{apply_windows, prefetch_objects};
use super::{
    FsOp, FsOpKind, LocalRead, MergePlan, PullDeps, PullError, PullOutcome, entry_mode,
    observe_syncable, read_local_content, record_for_entry,
};
use crate::sync::manifest_engine::aux_index::AUX_INDEX_PATH;
use crate::sync::manifest_engine::fs_guard::{
    AnchoredLeafKind, Observed, ParentChain, ParentChainMode, is_recovery_owner_record_name,
    open_private_root, prepare_parent_chain,
};
use crate::sync::manifest_engine::manifest::{
    EntryKind, ManifestEntry, ManifestKey, WorkspacePath,
};
use crate::sync::manifest_engine::push::{EngineContext, now_unix_ns};
use crate::sync::manifest_engine::remote::{RemoteObjects, RemoteRef};
use crate::sync::manifest_engine::store::{
    AncestorCommit, Intent, IntentOperationKind, ManifestStore,
};
use crate::sync::manifest_engine::unsyncable::{UnsyncableReason, UnsyncableRecord};
use crate::sync::manifest_engine::work_view_lock::acquire_work_view_transition_lock;
use crate::sync::manifest_engine::{EngineRef, RefObservation};
use applied_outcome::prove_commit;
pub(crate) use applied_outcome::{Applied, record_applied};

// ---- apply transaction ------------------------------------------------------

pub(crate) fn apply_plan<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    plan: MergePlan,
    manifest_key: &ManifestKey,
    ref_version: u64,
) -> Result<PullOutcome, PullError> {
    let prior = store.engine_state()?;
    let mut outcome = PullOutcome {
        observed_ref: EngineRef::Head(RefObservation {
            version: ref_version,
            manifest_key: manifest_key.clone(),
        }),
        applied_manifest_key: Some(manifest_key.clone()),
        ref_version: Some(ref_version),
        push_again: plan.push_again,
        ..PullOutcome::default()
    };
    let mut commit = AncestorCommit {
        upserts: plan.ancestor_upserts,
        removals: plan.ancestor_removals,
    };
    let mut intent_ids: Vec<WorkspacePath> = Vec::new();

    // Refused remote entries are durable before any mutation runs: a crash mid-apply
    // must not lose the record of an entry this device will never materialize. It
    // records only, never clears — a pull learns nothing about paths it did not
    // refuse, and push owns resolution when the condition actually goes away.
    store.record_unsyncable(&plan.unsyncable, &BTreeSet::new())?;

    // Deletes run first and bottom-up (children before parents), so each directory
    // is empty by the time its own non-recursive remove is attempted and a tracked
    // child that a replacement install must clear is already gone. Every other op
    // runs after, ranked (within a Git repo objects/** land before refs/HEAD/index
    // so no ref ever points at a missing object — Plan 109 Git contract) and
    // top-down (a parent directory exists before its children install).
    let mut fs_ops = plan.fs_ops;
    let _work_view_transition = fs_ops
        .iter()
        .any(|op| op.path.as_str() == AUX_INDEX_PATH)
        .then(|| acquire_work_view_transition_lock(&deps.ctx.workspace_root))
        .transpose()
        .map_err(PullError::engine_scratch)?;
    fs_ops.sort_by(|left, right| {
        delete_phase(left)
            .cmp(&delete_phase(right))
            .then_with(|| order_within_phase(left, right))
    });

    // Probing git lock state means a recursive walk of `<git dir>/refs`, which a
    // first pull — tens of thousands of `.git/objects/**` entries — must not do
    // once per object. `GitLockCache` amortizes content-addressed payload probes
    // but invalidates the recursive verdict before every operation that exposes
    // mutable Git state. The three fixed locks are also stat-ed on every op, so
    // any Git transaction taken mid-apply defers the next exposing operation.
    let mut git_locks = GitLockCache::default();

    // Prefetch follows the sort, one window at a time, so the first file installs
    // after its own window's blobs rather than the whole plan's. Windows never
    // reorder: the delete/parent/Git-rank ordering above still holds across them.
    for window in apply_windows(&fs_ops) {
        let prefetched_objects =
            prefetch_objects(window, deps.objects).map_err(PullError::Transport)?;
        let apply_deps = PullDeps {
            ctx: deps.ctx,
            objects: &prefetched_objects,
            refs: deps.refs,
            scope: deps.scope,
        };
        for op in window {
            if git_locks.is_active(&deps.ctx.workspace_root, &op.path) {
                outcome.deferred.insert(op.path.clone());
                // Deferral means this path has not settled against the incoming
                // head. Keep its three-way base unchanged so the retry derives the
                // same merge row instead of misclassifying the remote entry as a
                // fresh creation.
                commit.upserts.remove(&op.path);
                commit.removals.remove(&op.path);
                continue;
            }
            // The id is listed BEFORE the op runs, so a path-scoped refusal retires
            // its intent in the same outcome transaction. Leaving the intent open
            // would hand the identical failure to crash recovery on every restart —
            // the shape that turns one racing file into a device that never starts.
            intent_ids.push(op.path.clone());
            let applied = match apply_op(store, &apply_deps, op, manifest_key) {
                Ok(applied) => applied,
                Err(PullError::Path(fault)) => Applied::Unsyncable(fault.path, fault.record),
                Err(error) => return Err(error),
            };
            deps.ctx.counters.record_apply_ops(1);
            record_applied(&mut commit, &mut outcome, applied);
        }
    }

    // Paths the apply itself refused. Durable BEFORE the outcome commits, for the
    // same reason `plan.unsyncable` is: a crash must not lose the record of a
    // path this device could not materialize.
    store.record_unsyncable(&outcome.unsyncable, &BTreeSet::new())?;

    // A deferred path (active Git lock) must be retried after the lock clears, so
    // do NOT advance the applied head past content we have not materialized.
    // Advance to the incoming head only when nothing deferred; otherwise hold the
    // PRIOR head — which is `None` on a first pull, so the head is NOT recorded as
    // applied. Recording the incoming head here would let the next pull short-
    // circuit at `already_current` and never materialize the deferred paths.
    let advance = if outcome.deferred.is_empty() {
        Some((manifest_key.clone(), ref_version))
    } else {
        prior
            .applied_manifest_key
            .clone()
            .zip(prior.last_ref_version)
    };
    outcome.applied_manifest_key = advance.as_ref().map(|(key, _)| key.clone());
    outcome.ref_version = advance.as_ref().map(|(_, version)| *version);

    // ONE transaction: ancestor rows + (optional) applied ref + verified ratchet +
    // intent deletions. The ratchet advances to the head `decide_head` just fetched,
    // authenticated, and decoded — always, even when `applied` is held back for a
    // deferred path (the head itself was still verified).
    let applied = advance.as_ref().map(|(key, version)| (key, *version));
    prove_commit(deps.ctx, &mut commit);
    store.commit_pull_outcome(
        &commit,
        applied,
        Some((manifest_key, ref_version)),
        &intent_ids,
        &outcome.push_again,
    )?;
    deps.ctx.counters.record_sqlite_mutation();
    // The intents are cleared, so their preimages are no longer a rollback asset
    // for anything. Drop them in the same step that retired the intents.
    release_quarantine(deps.ctx, &intent_ids);
    Ok(outcome)
}

/// Apply one filesystem op through the intent-journalled transaction. The
/// re-observation immediately before mutation is the data-loss guard.
pub(crate) fn apply_op<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    op: &FsOp,
    manifest_key: &ManifestKey,
) -> Result<Applied, PullError> {
    let ctx = deps.ctx;
    let temp = stage_write_temp(ctx, deps.objects, op)?;
    store.open_intent(&build_intent(op, temp.as_ref(), manifest_key))?;
    ctx.counters.record_sqlite_mutation();

    // Re-observe the FULL preimage at the mutation boundary. The intent is
    // already durable here, so a target that raced into an object the engine
    // cannot represent — a FIFO, a socket, a device node, a non-UTF-8 symlink —
    // must settle as a path-scoped refusal rather than fail the cycle: the caller
    // records it unsyncable and retires the intent, where propagating would
    // replay this same observation on every restart forever.
    let observed = observe_syncable(&ctx.workspace_root, &op.path)?;
    if !preimage_matches(ctx, &op.path, &op.expected, observed.as_ref())?
        && !installs_the_same_directory(&op.kind, observed.as_ref())
    {
        // Never overwrite/delete a racing user write: keep local, aside remote.
        return apply_keep_local(ctx, deps.objects, op, temp);
    }

    match &op.kind {
        FsOpKind::Install(entry) => {
            match install_entry(ctx, deps.objects, &op.path, entry, temp, observed.as_ref())? {
                Materialized::Done(record) => Ok(Applied::Upsert(op.path.clone(), record)),
                // The install was blocked — a symlinked parent, or a directory the
                // remote would replace that still holds local-only content. Keep
                // local and aside the remote (which itself keeps-local if the aside
                // is also blocked). The temp was already dropped; the aside
                // re-downloads.
                Materialized::ParentBlocked => apply_keep_local(ctx, deps.objects, op, None),
                // A symlink entry whose target now lands outside the root. The
                // merge froze this shape already, so it means the escaping
                // component appeared since: keep local, aside nothing (the aside
                // would be the same escape under another name and refuses too).
                Materialized::EscapingTarget => Ok(Applied::KeptLocal(op.path.clone())),
                // The bytes landed and were then removed under us. Nothing to
                // aside (the remote content IS what installed), and nothing to
                // record; the next scan publishes the deletion.
                // `install_entry` never refuses on aside grounds — that verdict
                // belongs to `materialize_aside` alone — but the shared outcome
                // type carries it, and keep-local is the same safe answer.
                Materialized::Vanished | Materialized::AsideRefused => {
                    Ok(Applied::KeptLocal(op.path.clone()))
                }
            }
        }
        FsOpKind::ConflictAside(_) => apply_keep_local(ctx, deps.objects, op, temp),
        FsOpKind::Delete => match checked_delete(ctx, &op.path)? {
            DeleteOutcome::Deleted => Ok(Applied::Remove(op.path.clone())),
            // A symlinked parent, or a directory still holding local-only content:
            // never unlink through it, never destroy it — keep local.
            DeleteOutcome::KeptLocal => Ok(Applied::KeptLocal(op.path.clone())),
        },
        FsOpKind::ModeChange(entry) => {
            // A chmod resolves symlinks in the path; a symlinked parent would let
            // it re-mode a file outside the root. Verify the chain first.
            if let ParentChain::Blocked = prepare_parent_chain(
                &ctx.workspace_root,
                &op.path,
                ParentChainMode::RequireExisting,
            ) {
                return Ok(Applied::KeptLocal(op.path.clone()));
            }
            set_mode(&ctx.workspace_root, &op.path, entry_mode(entry))?;
            // A delete racing the chmod is an ordinary filesystem state. Keep
            // local with the ancestor untouched so the next scan classifies the
            // deletion; calling it an invariant violation aborts the cycle and,
            // because the intent is durable, every restart that replays it.
            let Some(observed) = observe_syncable(&ctx.workspace_root, &op.path)? else {
                return Ok(Applied::KeptLocal(op.path.clone()));
            };
            // Carry the entry's content identity: a mode change leaves the bytes
            // untouched, so the ancestor row must keep content_id/blob_key/key_epoch.
            Ok(Applied::Upsert(
                op.path.clone(),
                record_for_entry(entry, observed.fingerprint),
            ))
        }
    }
}

/// Keep the local bytes and materialize the remote as a deterministic aside.
pub(crate) fn apply_keep_local<O: RemoteObjects>(
    ctx: &EngineContext,
    objects: &O,
    op: &FsOp,
    temp: Option<TempFile>,
) -> Result<Applied, PullError> {
    match &op.kind {
        FsOpKind::Install(entry) | FsOpKind::ConflictAside(entry) => {
            match materialize_aside(ctx, objects, &op.path, entry, temp)? {
                Materialized::Done(aside) => Ok(Applied::Aside(aside)),
                // No safe location for the aside (symlinked parent), a target that
                // escapes the root from the aside's own location, a path that may
                // not carry an aside at all (git-internal state), or an aside
                // removed under us: keep local in every case.
                Materialized::ParentBlocked
                | Materialized::EscapingTarget
                | Materialized::Vanished
                | Materialized::AsideRefused => Ok(Applied::KeptLocal(op.path.clone())),
            }
        }
        // A racing write over a delete/mode target: keep local, nothing to aside.
        FsOpKind::Delete | FsOpKind::ModeChange(_) => Ok(Applied::KeptLocal(op.path.clone())),
    }
}

// ---- intent construction ----------------------------------------------------

pub(crate) fn build_intent(
    op: &FsOp,
    temp: Option<&TempFile>,
    manifest_key: &ManifestKey,
) -> Intent {
    let (operation_kind, target) = target_payload(op);
    Intent {
        path: op.path.clone(),
        operation_kind,
        temp_name: temp.map(|temp| temp.name.clone()),
        expected_preimage: Some(encode(&op.expected)),
        target_record: Some(encode(&target)),
        preserved_preimage: Some(quarantine_name(&op.path)),
        target_manifest_key: Some(manifest_key.clone()),
        created_at: now_unix_ns(),
    }
}
// ---- recovery (pure classification + executor) ------------------------------

/// The six crash boundaries a pending intent may sit at (Plan 109 Step 5). Each
/// recovers idempotently; "temp absent → discard" alone is insufficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryBoundary {
    TempOnly,
    IntentOldTarget,
    InstalledIntent,
    PreservedNoTarget,
    DeleteDoneIntent,
    TargetModifiedWhileDown,
}

/// The action recovery takes for one boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    DiscardTemp,
    Reapply,
    FinalizeInstalled,
    RestoreOrComplete,
    FinalizeDeleted,
    KeepLocalAside,
}

/// The filesystem facts recovery observes for one intent.
#[derive(Debug, Clone, Copy)]
pub struct RecoveryObservation {
    pub target_present: bool,
    pub target_matches_target_record: bool,
    pub target_matches_preimage: bool,
    pub temp_exists: bool,
    pub quarantine_exists: bool,
}

/// Pure map from (intent op, observed filesystem) to a boundary. Drives every
/// kill-9 case directly (Step 6 spawns the barriers; this classifies the state).
pub fn recovery_boundary(
    operation: IntentOperationKind,
    observed: &RecoveryObservation,
) -> RecoveryBoundary {
    match operation {
        IntentOperationKind::Delete => {
            if observed.target_present {
                if observed.target_matches_preimage {
                    RecoveryBoundary::IntentOldTarget
                } else {
                    RecoveryBoundary::TargetModifiedWhileDown
                }
            } else {
                RecoveryBoundary::DeleteDoneIntent
            }
        }
        IntentOperationKind::Install
        | IntentOperationKind::ModeChange
        | IntentOperationKind::ConflictAside => {
            if observed.target_matches_target_record {
                RecoveryBoundary::InstalledIntent
            } else if observed.target_matches_preimage {
                // The target is still in its expected pre-state — present-and-old
                // for a replace, or absent-as-expected for a create — and the
                // intent is committed but the mutation did not complete. Reapply
                // it. Checked before `temp_exists` so a committed file create
                // (absent target, temp staged) completes rather than being
                // discarded as orphan scratch.
                RecoveryBoundary::IntentOldTarget
            } else if !observed.target_present && observed.quarantine_exists {
                RecoveryBoundary::PreservedNoTarget
            } else if observed.target_present {
                RecoveryBoundary::TargetModifiedWhileDown
            } else if observed.temp_exists {
                RecoveryBoundary::TempOnly
            } else {
                RecoveryBoundary::PreservedNoTarget
            }
        }
    }
}

/// The action for a boundary.
pub fn recovery_action(boundary: RecoveryBoundary) -> RecoveryAction {
    match boundary {
        RecoveryBoundary::TempOnly => RecoveryAction::DiscardTemp,
        RecoveryBoundary::IntentOldTarget => RecoveryAction::Reapply,
        RecoveryBoundary::InstalledIntent => RecoveryAction::FinalizeInstalled,
        RecoveryBoundary::PreservedNoTarget => RecoveryAction::RestoreOrComplete,
        RecoveryBoundary::DeleteDoneIntent => RecoveryAction::FinalizeDeleted,
        RecoveryBoundary::TargetModifiedWhileDown => RecoveryAction::KeepLocalAside,
    }
}

/// Recover every pending intent, then clear them in ONE outcome transaction that
/// does NOT advance the head (the follow-on `pull` re-derives against the fresh
/// ref). Idempotent: safe to run at every startup.
pub fn recover_intents<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
) -> Result<(), PullError> {
    let intents = store.pending_intents()?;
    let _work_view_transition = intents
        .iter()
        .any(|intent| intent.path.as_str() == AUX_INDEX_PATH)
        .then(|| acquire_work_view_transition_lock(&deps.ctx.workspace_root))
        .transpose()
        .map_err(PullError::engine_scratch)?;
    if intents.is_empty() {
        sweep_orphan_temps(deps.ctx, &BTreeSet::new())?;
        sweep_orphan_quarantine(deps.ctx, &BTreeSet::new())?;
        return Ok(());
    }

    let mut commit = AncestorCommit::default();
    let mut intent_ids = Vec::new();
    let mut keep_temps = BTreeSet::new();
    let mut abandoned: BTreeMap<WorkspacePath, UnsyncableRecord> = BTreeMap::new();
    for intent in &intents {
        intent_ids.push(intent.path.clone());
        if let Err(error) = recover_one(store, deps, intent, &mut commit, &mut keep_temps) {
            match replay_verdict(&error) {
                ReplayVerdict::Retry => return Err(error),
                ReplayVerdict::Quarantine(reason) => {
                    // Leave the id listed: the commit below retires the intent.
                    abandoned.insert(
                        intent.path.clone(),
                        UnsyncableRecord::new(reason, None, now_unix_ns()),
                    );
                }
            }
        }
    }
    // Durable before the retirement commits, so a crash in between cannot lose
    // the record of a path this device gave up replaying.
    store.record_unsyncable(&abandoned, &BTreeSet::new())?;
    // Clear the intents and commit the ancestor rows recovery rematerialized, but
    // do NOT advance the applied head: the follow-on `pull` re-derives against the
    // fresh ref and commits the TRUE head + version. Advancing here would have to
    // invent a version on a FIRST pull (no prior `last_ref_version`), and a
    // fabricated 0 then freezes forever — the next pull short-circuits at
    // `already_current` without correcting it, so every push CASes against 0,
    // loses, re-pulls the already-current key, and livelocks.
    // No verified head to ratchet: recovery re-derives the true head on the
    // follow-on pull, which authenticates it and advances the ratchet then.
    prove_commit(deps.ctx, &mut commit);
    store.commit_pull_outcome(&commit, None, None, &intent_ids, &BTreeSet::new())?;
    deps.ctx.counters.record_sqlite_mutation();
    sweep_orphan_temps(deps.ctx, &keep_temps)?;
    // Every intent was just cleared, so every quarantine entry is now orphaned
    // scratch — including entries left by a process killed before it could
    // release its own.
    sweep_orphan_quarantine(deps.ctx, &BTreeSet::new())?;
    Ok(())
}

/// What a failed intent replay means for the journal — the backstop that makes
/// startup converge unconditionally.
///
/// An intent is only a way to FINISH an interrupted mutation. The ancestor is
/// untouched by a replay that fails, and the follow-on pull re-derives the truth
/// for that path from the workspace ref, so retiring one can never lose data.
/// Replaying one forever is a device that never starts again. That asymmetry is
/// why the default here is `Quarantine` and only two conditions earn a retry.
enum ReplayVerdict {
    /// The condition is about the network or the store itself, not about this
    /// intent: keep it journalled so the driver's backoff retries it.
    Retry,
    /// The intent cannot be replayed on this device as it stands. Retire it and
    /// record the path with this reason.
    Quarantine(UnsyncableReason),
}

fn replay_verdict(error: &PullError) -> ReplayVerdict {
    match error {
        // Offline, or a broken database. Both are conditions the driver already
        // recovers from without losing the journal, and both clear on their own.
        PullError::Transport(_) | PullError::Store(_) => ReplayVerdict::Retry,
        PullError::Path(fault) => ReplayVerdict::Quarantine(fault.record.reason),
        // Deliberately the catch-all: a corrupt intent payload, a manifest entry
        // that cannot be rebuilt, an object that came back with the wrong bytes,
        // unusable engine scratch. Every one of them would otherwise be replayed
        // identically at every startup, forever.
        _ => ReplayVerdict::Quarantine(UnsyncableReason::RecoveryAbandoned),
    }
}

pub(crate) fn recover_one<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    intent: &Intent,
    commit: &mut AncestorCommit,
    keep_temps: &mut BTreeSet<String>,
) -> Result<(), PullError> {
    let ctx = deps.ctx;
    let target: TargetRecordPayload = intent
        .target_record
        .as_deref()
        .map(decode)
        .transpose()?
        .ok_or(PullError::Internal {
            reason: "intent missing target record",
        })?;
    let preimage: PreimagePayload = intent
        .expected_preimage
        .as_deref()
        .map(decode)
        .transpose()?
        .unwrap_or_else(PreimagePayload::absent);
    let observed = observe_syncable(&ctx.workspace_root, &intent.path)?;
    let facts = recovery_facts(
        ctx,
        &intent.path,
        &target,
        &preimage,
        observed.as_ref(),
        intent,
    )?;
    let boundary = recovery_boundary(intent.operation_kind, &facts);
    let _ = store; // reserved for future finalize hooks; recovery commits in the batch
    if let Some(temp) = intent.temp_name.as_ref() {
        keep_temps.insert(temp.clone());
    }
    execute_recovery(ctx, deps.objects, intent, &target, boundary, commit)
}

pub(crate) fn execute_recovery<O: RemoteObjects>(
    ctx: &EngineContext,
    objects: &O,
    intent: &Intent,
    target: &TargetRecordPayload,
    boundary: RecoveryBoundary,
    commit: &mut AncestorCommit,
) -> Result<(), PullError> {
    match recovery_action(boundary) {
        RecoveryAction::DiscardTemp | RecoveryAction::KeepLocalAside => {
            // Keep local: nothing installed; the follow-on pull re-asides if the
            // remote still diverges. No ancestor mutation here.
        }
        RecoveryAction::FinalizeDeleted => {
            commit.removals.insert(intent.path.clone());
        }
        RecoveryAction::FinalizeInstalled => {
            finalize_installed(ctx, &intent.path, target, commit)?;
        }
        RecoveryAction::Reapply | RecoveryAction::RestoreOrComplete => {
            reapply_target(ctx, objects, intent, target, commit)?;
        }
    }
    Ok(())
}

pub(crate) fn finalize_installed(
    ctx: &EngineContext,
    path: &WorkspacePath,
    target: &TargetRecordPayload,
    commit: &mut AncestorCommit,
) -> Result<(), PullError> {
    match target.op {
        IntentOpTag::Delete => {
            commit.removals.insert(path.clone());
        }
        _ => {
            // Recovery classified this boundary from an EARLIER observation, so a
            // delete landing in between finds nothing here. That is an ordinary
            // race, not a broken invariant — and treating it as one would be
            // unrecoverable, because the intent is durable and every restart
            // replays it into the same error. Retire it with the ancestor
            // untouched; the next scan classifies the deletion through the merge
            // matrix (the same rule the ModeChange branch of `reapply_target`
            // follows).
            let Some(observed) = observe_syncable(&ctx.workspace_root, path)? else {
                return Ok(());
            };
            commit
                .upserts
                .insert(path.clone(), target.to_record(observed.fingerprint)?);
        }
    }
    Ok(())
}

pub(crate) fn reapply_target<O: RemoteObjects>(
    ctx: &EngineContext,
    objects: &O,
    intent: &Intent,
    target: &TargetRecordPayload,
    commit: &mut AncestorCommit,
) -> Result<(), PullError> {
    match target.op {
        IntentOpTag::Delete => match checked_delete(ctx, &intent.path)? {
            DeleteOutcome::Deleted => {
                commit.removals.insert(intent.path.clone());
            }
            // A symlinked parent, or a directory holding local-only content: keep
            // local; the follow-on pull re-derives against the fresh ref.
            DeleteOutcome::KeptLocal => {}
        },
        IntentOpTag::ConflictAside => {
            let entry = target.to_entry()?;
            // A crash between materialize and outcome-commit re-enters recovery.
            // The aside name is content-derived, so a prior attempt already left
            // the exact bytes on disk; re-materializing would append a duplicate
            // (1), (2) copy. No-op when an aside carrying this content exists.
            if !aside_already_materialized(ctx, &intent.path, &entry)? {
                // Done records the placed path (unused in recovery); ParentBlocked
                // keeps local — both leave no ancestor mutation here.
                match materialize_aside(ctx, objects, &intent.path, &entry, None)? {
                    Materialized::Done(_)
                    | Materialized::ParentBlocked
                    | Materialized::EscapingTarget
                    | Materialized::Vanished
                    | Materialized::AsideRefused => {}
                }
            }
        }
        IntentOpTag::ModeChange => {
            // A mode change moved no content; the target carries the full entry
            // (content identity included) so the recovered ancestor row is complete.
            let entry = target.to_entry()?;
            if let ParentChain::Blocked = prepare_parent_chain(
                &ctx.workspace_root,
                &intent.path,
                ParentChainMode::RequireExisting,
            ) {
                return Ok(()); // symlinked parent: keep local, re-derive on next pull
            }
            // A path that no longer exists is a race, not a broken invariant: the
            // file was deleted after the intent was journalled. Treating it as
            // fatal bricked the device permanently, because start() replays the
            // same intent on every restart and can never get past it. Retire the
            // intent with the ancestor untouched and let the next scan classify
            // the deletion through the ordinary merge matrix.
            set_mode(&ctx.workspace_root, &intent.path, entry_mode(&entry))?;
            let Some(observed) = observe_syncable(&ctx.workspace_root, &intent.path)? else {
                return Ok(());
            };
            commit.upserts.insert(
                intent.path.clone(),
                record_for_entry(&entry, observed.fingerprint),
            );
        }
        IntentOpTag::Install => {
            let entry = target.to_entry()?;
            let existing = observe_syncable(&ctx.workspace_root, &intent.path)?;
            // A blocked parent yields ParentBlocked (kept local); a genuine error
            // falls back to the download-reinstall path as before. A path-scoped
            // refusal is NOT retried by re-downloading: the reinstall writes to
            // the same refused path and would only fail the same way.
            let installed =
                match install_entry(ctx, objects, &intent.path, &entry, None, existing.as_ref()) {
                    Ok(result) => result,
                    Err(error @ PullError::Path(_)) => return Err(error),
                    Err(_) => reinstall_from_download(ctx, objects, &intent.path, &entry)?,
                };
            if let Materialized::Done(record) = installed {
                commit.upserts.insert(intent.path.clone(), record);
            }
        }
    }
    Ok(())
}

pub(crate) fn sweep_orphan_temps(
    ctx: &EngineContext,
    keep: &BTreeSet<String>,
) -> Result<(), PullError> {
    let directory = match open_private_root(&ctx.recovery_dir()) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PullError::engine_scratch(error)),
    };
    for entry in directory.entries().map_err(PullError::engine_scratch)? {
        let Some(name) = entry.name.as_str() else {
            continue;
        };
        if is_recovery_owner_record_name(name) {
            continue;
        }
        if !keep.contains(name) {
            match entry.kind {
                AnchoredLeafKind::NonDirectory => {
                    let _ = directory.unlink(&entry.name);
                }
                AnchoredLeafKind::Directory => {
                    let _ = directory.remove_tree(&entry.name);
                }
                AnchoredLeafKind::Absent => {}
            }
        }
    }
    Ok(())
}

/// Remove every quarantined preimage that no pending intent still needs — the
/// exact mirror of [`sweep_orphan_temps`], for the resource that had no sweep at
/// all and therefore grew without bound.
pub(crate) fn sweep_orphan_quarantine(
    ctx: &EngineContext,
    keep_paths: &BTreeSet<WorkspacePath>,
) -> Result<(), PullError> {
    let keep: BTreeSet<String> = keep_paths.iter().map(quarantine_leaf).collect();
    let dir = quarantine_dir(ctx);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PullError::engine_scratch(error)),
    };
    for entry in entries {
        let entry = entry.map_err(PullError::engine_scratch)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            let _ = fs::remove_file(entry.path()); // orphan preimage: discard
        }
    }
    Ok(())
}

/// Phase key for the apply sort: deletes (0) run before every other op (1). A
/// directory delete is non-recursive, so its children must already be gone; a
/// replacement install over a directory needs that directory emptied first.
fn delete_phase(op: &FsOp) -> u8 {
    match op.kind {
        FsOpKind::Delete => 0,
        _ => 1,
    }
}

/// Order two ops within the same phase. Deletes sort bottom-up (a child path
/// sorts before its parent) so each directory is empty before its own remove;
/// every other op sorts by Git rank then top-down (parents before children).
fn order_within_phase(left: &FsOp, right: &FsOp) -> std::cmp::Ordering {
    if matches!(left.kind, FsOpKind::Delete) {
        // Same phase means `right` is a delete too; reverse the path order.
        right.path.cmp(&left.path)
    } else {
        git_apply_rank(left.path.as_str())
            .cmp(&git_apply_rank(right.path.as_str()))
            .then_with(|| left.path.cmp(&right.path))
    }
}

// ---- shared helpers ---------------------------------------------------------

/// Whether this op would put a directory exactly where a directory already sits.
///
/// A directory is a container, not content: its identity is that it exists, and
/// its children are reconciled one row at a time by the merge matrix. So two
/// directories meeting at one path never disagree, even though the preimage the
/// plan snapshotted no longer describes the target — and the install is
/// idempotent (`create_dir` accepts the directory that is already there, then
/// stamps the published mode). Deflecting instead materializes an EMPTY
/// directory beside the real one and pins the workspace at `attention` on a
/// conflict with no two sides to choose between.
///
/// Two ordinary things reach it, neither of them a divergence. Apply order ranks
/// `.git/objects/**` ahead of the `.git` entry that contains it, so the parent
/// chain creates `.git` before `.git`'s own op runs; and two devices can each
/// `mkdir` the same path between one device's plan and its apply. Neither
/// carries any intent about the directory's mode — nobody chmod'd anything — so
/// the published mode wins exactly as it would have on an empty slot.
///
/// Strictly directory-vs-directory. A kind clash (a file or a symlink where the
/// entry is a directory, or the reverse) is a real conflict and still asides,
/// and a file whose bytes diverged is never in scope here at all.
fn installs_the_same_directory(kind: &FsOpKind, observed: Option<&Observed>) -> bool {
    matches!(kind, FsOpKind::Install(ManifestEntry::Directory { .. }))
        && observed.is_some_and(|observed| observed.kind == EntryKind::Directory)
}

pub(crate) fn preimage_matches(
    ctx: &EngineContext,
    path: &WorkspacePath,
    expected: &PreimagePayload,
    observed: Option<&Observed>,
) -> Result<bool, PullError> {
    match (expected.present, observed) {
        (false, None) => Ok(true),
        (false, Some(_)) => Ok(false),
        (true, None) => Ok(false),
        (true, Some(observed)) => {
            if Some(observed.kind) != expected.kind {
                return Ok(false);
            }
            match observed.kind {
                EntryKind::File => {
                    // A local chmod between merge planning and this re-observation
                    // changes the mode without touching the bytes; the remote op
                    // must deflect (keep-local) rather than silently discard the
                    // concurrent permission change. Mode is the full st_mode that
                    // `observe` reads and `push` records into the entry, so the
                    // snapshot and the re-observation compare directly.
                    if expected.mode.is_some_and(|mode| observed.mode != mode) {
                        return Ok(false);
                    }
                    // Ambiguity: hash to confirm the bytes really match. Read
                    // no-follow against the observed fingerprint — a leaf raced
                    // into a symlink diverges and can never satisfy the preimage.
                    match read_local_content(ctx, path, &observed.expected_file())? {
                        LocalRead::Bytes(bytes) => match expected.key_epoch {
                            Some(key_epoch) => {
                                // If the historical preimage key is missing, the
                                // preimage cannot be authenticated as matching, so
                                // the op deflects instead of mutating uncertain bytes.
                                Ok(ctx.crypto.content_id_at(key_epoch, &bytes).as_ref()
                                    == expected.content_id.as_ref())
                            }
                            None => Ok(Some(ctx.crypto.content_id(&bytes)) == expected.content_id),
                        },
                        // Unverifiable bytes can never satisfy a preimage, so the
                        // op deflects to keep-local — the safe answer.
                        LocalRead::Unverifiable => Ok(false),
                    }
                }
                // Symlink modes are deliberately excluded: they are not portably
                // settable (`set_mode` skips them; macOS `lchmod` semantics vary),
                // so `observe` records a platform-dependent link mode the engine
                // never treats as authoritative. Match on the target alone, exactly
                // as the mutation side does.
                EntryKind::Symlink => Ok(observed.symlink_target == expected.symlink_target),
                // A raced directory chmod deflects like a file's: compare the mode
                // the plan snapshotted against what is on disk now.
                EntryKind::Directory => Ok(expected.mode.is_none_or(|mode| observed.mode == mode)),
            }
        }
    }
}
