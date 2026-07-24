//! The apply transaction, crash recovery, and the Git contract for pull (Plan
//! 109 Step 5).
//!
//! Split from `pull_apply.rs` because the merge + apply machinery exceeds the
//! 900-line source gate; the seam is the domain boundary between *deciding* the
//! merge (parent module) and *executing* it against the filesystem (here). The
//! leaf filesystem primitives this transaction composes live in the sibling
//! [`super::materialize`] module. Every mutation is intent-journalled,
//! re-observes its preimage immediately before touching disk, and never
//! overwrites a racing user write.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use super::intents::{
    IntentOpTag, PreimagePayload, TargetRecordPayload, decode, encode, recovery_facts,
    target_payload,
};
use super::materialize::{
    DeleteOutcome, Materialized, TempFile, aside_already_materialized, checked_delete,
    install_entry, materialize_aside, reinstall_from_download, set_mode, stage_write_temp,
};
use super::naming::quarantine_name;
use super::{
    FsOp, FsOpKind, MergePlan, PullDeps, PullError, PullOutcome, entry_mode, record_for_entry,
};
use crate::sync::manifest_engine::fs_guard::{
    FileRead, Observed, ParentChain, ParentChainMode, observe, prepare_parent_chain,
    read_file_bounded,
};
use crate::sync::manifest_engine::manifest::{EntryKind, ManifestKey, WorkspacePath};
use crate::sync::manifest_engine::push::{EngineContext, RemoteObjects, RemoteRef, now_unix_ns};
use crate::sync::manifest_engine::store::{
    AncestorCommit, FileRecord, Intent, IntentOperationKind, ManifestStore,
};

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

    // Deletes run first and bottom-up (children before parents), so each directory
    // is empty by the time its own non-recursive remove is attempted and a tracked
    // child that a replacement install must clear is already gone. Every other op
    // runs after, ranked (within a Git repo objects/** land before refs/HEAD/index
    // so no ref ever points at a missing object — Plan 109 Git contract) and
    // top-down (a parent directory exists before its children install).
    let mut fs_ops = plan.fs_ops;
    fs_ops.sort_by(|left, right| {
        delete_phase(left)
            .cmp(&delete_phase(right))
            .then_with(|| order_within_phase(left, right))
    });

    for op in fs_ops {
        if git_lock_active(&deps.ctx.workspace_root, &op.path) {
            outcome.deferred.insert(op.path.clone());
            continue;
        }
        let applied = apply_op(store, deps, &op, manifest_key)?;
        deps.ctx.counters.record_apply_ops(1);
        intent_ids.push(op.path.clone());
        record_applied(&mut commit, &mut outcome, applied);
    }

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
    store.commit_pull_outcome(
        &commit,
        applied,
        Some((manifest_key, ref_version)),
        &intent_ids,
    )?;
    deps.ctx.counters.record_sqlite_mutation();
    Ok(outcome)
}

pub(crate) enum Applied {
    Upsert(WorkspacePath, FileRecord),
    Remove(WorkspacePath),
    Aside(WorkspacePath),
    KeptLocal(WorkspacePath),
}

pub(crate) fn record_applied(
    commit: &mut AncestorCommit,
    outcome: &mut PullOutcome,
    applied: Applied,
) {
    match applied {
        Applied::Upsert(path, record) => {
            outcome.installed.insert(path.clone());
            commit.upserts.insert(path, record);
        }
        Applied::Remove(path) => {
            outcome.deleted.insert(path.clone());
            commit.removals.insert(path);
        }
        Applied::Aside(path) => {
            outcome.conflict_asides.insert(path);
        }
        Applied::KeptLocal(path) => {
            outcome.push_again.insert(path);
        }
    }
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

    // Re-observe the FULL preimage at the mutation boundary.
    let observed = observe(&ctx.workspace_root, &op.path).map_err(PullError::Io)?;
    if !preimage_matches(ctx, &op.path, &op.expected, observed.as_ref())? {
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
            )? {
                return Ok(Applied::KeptLocal(op.path.clone()));
            }
            set_mode(&ctx.workspace_root, &op.path, entry_mode(entry))?;
            let observed = observe(&ctx.workspace_root, &op.path)
                .map_err(PullError::Io)?
                .ok_or(PullError::Internal {
                    reason: "mode target vanished",
                })?;
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
                // No safe location for the aside (symlinked parent): keep local.
                Materialized::ParentBlocked => Ok(Applied::KeptLocal(op.path.clone())),
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
    if intents.is_empty() {
        sweep_orphan_temps(deps.ctx, &BTreeSet::new())?;
        return Ok(());
    }

    let mut commit = AncestorCommit::default();
    let mut intent_ids = Vec::new();
    let mut keep_temps = BTreeSet::new();
    for intent in &intents {
        recover_one(store, deps, intent, &mut commit, &mut keep_temps)?;
        intent_ids.push(intent.path.clone());
    }
    // Clear the intents and commit the ancestor rows recovery rematerialized, but
    // do NOT advance the applied head: the follow-on `pull` re-derives against the
    // fresh ref and commits the TRUE head + version. Advancing here would have to
    // invent a version on a FIRST pull (no prior `last_ref_version`), and a
    // fabricated 0 then freezes forever — the next pull short-circuits at
    // `already_current` without correcting it, so every push CASes against 0,
    // loses, re-pulls the already-current key, and livelocks.
    // No verified head to ratchet: recovery re-derives the true head on the
    // follow-on pull, which authenticates it and advances the ratchet then.
    store.commit_pull_outcome(&commit, None, None, &intent_ids)?;
    deps.ctx.counters.record_sqlite_mutation();
    sweep_orphan_temps(deps.ctx, &keep_temps)?;
    Ok(())
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
    let observed = observe(&ctx.workspace_root, &intent.path).map_err(PullError::Io)?;
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
        IntentOpTag::Delete => commit.removals.insert(path.clone()),
        _ => {
            let observed = observe(&ctx.workspace_root, path)
                .map_err(PullError::Io)?
                .ok_or(PullError::Internal {
                    reason: "finalize without target",
                })?;
            commit
                .upserts
                .insert(path.clone(), target.to_record(observed.fingerprint)?);
            true
        }
    };
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
                    Materialized::Done(_) | Materialized::ParentBlocked => {}
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
            )? {
                return Ok(()); // symlinked parent: keep local, re-derive on next pull
            }
            set_mode(&ctx.workspace_root, &intent.path, entry_mode(&entry))?;
            let observed = observe(&ctx.workspace_root, &intent.path)
                .map_err(PullError::Io)?
                .ok_or(PullError::Internal {
                    reason: "mode-change recovery target vanished",
                })?;
            commit.upserts.insert(
                intent.path.clone(),
                record_for_entry(&entry, observed.fingerprint),
            );
        }
        IntentOpTag::Install => {
            let entry = target.to_entry()?;
            let existing = observe(&ctx.workspace_root, &intent.path).map_err(PullError::Io)?;
            // A blocked parent yields ParentBlocked (kept local); a genuine error
            // falls back to the download-reinstall path as before.
            let installed =
                match install_entry(ctx, objects, &intent.path, &entry, None, existing.as_ref()) {
                    Ok(result) => result,
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
    let dir = ctx.engine_dir().join("tmp");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PullError::Io(error)),
    };
    for entry in entries {
        let entry = entry.map_err(PullError::Io)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            let _ = fs::remove_file(entry.path()); // orphan temp: discard
        }
    }
    Ok(())
}

// ---- git contract -----------------------------------------------------------

/// Whether a Git lock is active for the repo containing `path`. While active,
/// that repo's paths defer (auto-rescan after the lock clears).
pub fn git_lock_active(root: &Path, path: &WorkspacePath) -> bool {
    let Some(git_dir) = git_dir_for(path.as_str()) else {
        return false;
    };
    let absolute = root.join(&git_dir);
    ["index.lock", "HEAD.lock", "packed-refs.lock"]
        .iter()
        .any(|lock| absolute.join(lock).exists())
        || refs_lock_present(&absolute)
}

pub(crate) fn refs_lock_present(git_dir: &Path) -> bool {
    fn any_lock(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if any_lock(&path) {
                    return true;
                }
            } else if path.extension().is_some_and(|ext| ext == "lock") {
                return true;
            }
        }
        false
    }
    any_lock(&git_dir.join("refs"))
}

pub(crate) fn git_dir_for(path: &str) -> Option<String> {
    let marker = "/.git/";
    if let Some(index) = path.find(marker) {
        return Some(format!("{}/.git", &path[..index]));
    }
    if path == ".git" || path.starts_with(".git/") {
        return Some(".git".to_string());
    }
    path.strip_suffix("/.git")
        .map(|prefix| format!("{prefix}/.git"))
}

pub(crate) fn is_git_lock_path(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|leaf| leaf.ends_with(".lock"))
        && path.contains(".git/")
}

/// Apply-order rank: within a Git repo, `objects/**` must land before
/// `refs`/`packed-refs`/`HEAD`/`index` so no ref points at a missing object.
pub fn git_apply_rank(path: &str) -> u8 {
    if !path.contains(".git/") {
        return 1;
    }
    if path.contains(".git/objects/") { 0 } else { 2 }
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
                    match read_file_bounded(
                        &ctx.workspace_root,
                        path,
                        ctx.config.max_seal_bytes,
                        &observed.expected_file(),
                    )
                    .map_err(PullError::Push)?
                    {
                        FileRead::Bytes(bytes) => {
                            Ok(Some(ctx.crypto.content_id(&bytes)) == expected.content_id)
                        }
                        FileRead::Diverged => Ok(false),
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
