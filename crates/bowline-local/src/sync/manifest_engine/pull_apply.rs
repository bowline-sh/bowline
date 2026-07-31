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
use std::path::Path;

pub mod apply;
pub(crate) mod delta;
pub mod git_contract;
pub(crate) mod intents;
pub mod materialize;
pub mod naming;

use apply::apply_plan;
// The entry<->row projection has its own module; re-exported here because every
// child module reaches it as `super::entry_mode` and friends.
pub(crate) use super::entry_record::{
    entry_matches_observed, entry_matches_record, entry_mode, record_for_entry,
};
use delta::{LocalDelta, RemoteDelta, local_delta, remote_delta};
pub(crate) use delta::{LocalRead, read_local_content};
use git_contract::is_git_lock_path;
use intents::PreimagePayload;

use super::fs_guard::{
    ObserveOutcome, Observed, observe_classified, symlink_target_lands_in_workspace,
};
use super::manifest::directory_tree::DirectoryTree;
use super::manifest::{
    DecodeLimits, DecodedManifest, ManifestEntry, ManifestError, ManifestKey, PathCollision,
    WorkspacePath,
};
use super::push::{
    EngineContext, PushError, RefObservation, RemoteObjects, RemoteRef, TransportError,
    file_record_to_entry, now_unix_ns,
};
use super::store::{FileRecord, ManifestStore, ManifestStoreError};
use super::tree_transport::{
    DiffTreeRequest, FetchTreeRequest, PruneBasis, StoreNodeLedger, TreeError, TreeNodeLedger,
    diff_tree, fetch_tree,
};
use super::unsyncable::{UnsyncablePath, UnsyncableReason, UnsyncableRecord};

pub use apply::{
    RecoveryAction, RecoveryBoundary, RecoveryObservation, recover_intents, recovery_action,
    recovery_boundary,
};
pub use git_contract::{git_apply_rank, git_lock_active};

/// The dependency bundle a pull receives (mirrors `PushDeps`).
pub struct PullDeps<'a, O: RemoteObjects, R: RemoteRef> {
    pub ctx: &'a EngineContext,
    pub objects: &'a O,
    pub refs: &'a R,
    pub scope: PullScope<'a>,
}

/// Which local paths a pull re-observes on disk.
///
/// The remote delta is a pure function of the ancestor map and the decoded
/// manifest — zero filesystem access — and only a non-`Unchanged` remote delta,
/// or a path the driver already knows is locally dirty, can produce merge work.
/// Every other path provably lands on the `(Unchanged, Unchanged)` no-op row, so
/// statting it spends a syscall to learn nothing. Narrowing the observed set is
/// what makes one remote change cost `|Δ|` syscalls instead of one per workspace
/// entry.
#[derive(Clone, Copy)]
pub enum PullScope<'a> {
    /// Steady-state watcher and stat-walk divergences.
    ChangedAndDirty(&'a BTreeSet<WorkspacePath>),
    /// A complete same-cycle stat walk, enabling debug narrowing proofs.
    ChangedAndWalked(&'a BTreeSet<WorkspacePath>),
    /// A work-view materialize with no watcher observes its whole ancestor.
    WholeAncestor,
    /// Startup crash repair: reclassify even when the ref still names the
    /// recorded applied root, because the local ancestor can commit before its
    /// follow-on copy-on-write push reaches the ref.
    ReconcileAncestor(&'a BTreeSet<WorkspacePath>),
}

impl PullScope<'_> {
    fn dirty_paths(&self) -> Option<&BTreeSet<WorkspacePath>> {
        match self {
            Self::ChangedAndDirty(dirty)
            | Self::ChangedAndWalked(dirty)
            | Self::ReconcileAncestor(dirty) => Some(dirty),
            Self::WholeAncestor => None,
        }
    }

    fn observes(&self, path: &WorkspacePath, remote: &RemoteDelta) -> bool {
        match self {
            Self::WholeAncestor | Self::ReconcileAncestor(_) => true,
            Self::ChangedAndDirty(dirty) | Self::ChangedAndWalked(dirty) => {
                !matches!(remote, RemoteDelta::Unchanged) || dirty.contains(path)
            }
        }
    }

    fn reconciles_pending(&self, path: &WorkspacePath) -> bool {
        matches!(self, Self::ReconcileAncestor(pending) if pending.contains(path))
    }
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
    /// Paths this pull refused at the mutation boundary rather than at merge
    /// time: the target raced into an object the engine cannot represent, or a
    /// single-path filesystem operation was refused. They are recorded durably
    /// before the outcome commits, exactly like `MergePlan::unsyncable`.
    pub unsyncable: BTreeMap<WorkspacePath, UnsyncableRecord>,
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
    if state.applied_manifest_key.as_ref() == Some(&head.manifest_key)
        && !matches!(deps.scope, PullScope::ReconcileAncestor(_))
    {
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
    let state = store.engine_state()?;
    if let (Some(applied), Some(dirty)) = (
        state.applied_manifest_key.as_ref(),
        deps.scope.dirty_paths(),
    ) {
        let limits = if deps.ctx.project_view {
            DecodeLimits::project_view()
        } else {
            DecodeLimits::default()
        };
        let delta = diff_tree(DiffTreeRequest {
            objects: deps.objects,
            crypto: &deps.ctx.crypto,
            counters: &deps.ctx.counters,
            old_root: applied,
            new_root: &head.manifest_key,
            dirty,
            names: deps.ctx.names,
            limits: &limits,
        })?;
        let mut scopes = delta.touched;
        scopes.extend(dirty.iter().cloned());
        for collision in &delta.collisions {
            scopes.extend(collision.paths.iter().cloned());
        }
        let ancestor = store.files_at_paths(&scopes)?;
        deps.ctx.counters.record_ancestor_rows_read(ancestor.len());
        return classify(
            deps.ctx,
            &ancestor,
            &delta.entries,
            &delta.collisions,
            deps.scope,
        );
    }
    let ancestor = store.all_files()?;
    deps.ctx.counters.record_ancestor_rows_read(ancestor.len());
    // `head_snapshot` is the new engine's flat `Manifest` (distinct from the old
    // `SnapshotManifest` the page-reader gate targets); binding it to a non-
    // `*manifest` name keeps that intent unambiguous.
    let (head_snapshot, collisions) = fetch_head(
        store,
        deps,
        head,
        &ancestor,
        !matches!(deps.scope, PullScope::ReconcileAncestor(_)),
    )?;
    classify(
        deps.ctx,
        &ancestor,
        &head_snapshot.entries,
        &collisions,
        deps.scope,
    )
}

/// Fetch and flatten the head tree, skipping every subtree whose node key this
/// device's own ancestor already hashes to.
///
/// The prune is byte-exact, not approximate: a node key is a collision-resistant
/// function of its whole subtree, so a match means the remote subtree and the
/// local ancestor subtree are the same content and the local copy may stand in
/// for bytes never fetched. Anything that does not match is downloaded, so the
/// merge matrix still sees a complete remote manifest and its eleven rows are
/// unchanged in meaning.
fn fetch_head<O: RemoteObjects, R: RemoteRef>(
    store: &mut ManifestStore,
    deps: &PullDeps<'_, O, R>,
    head: &super::push::RefObservation,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    prune_unchanged: bool,
) -> Result<(super::manifest::Manifest, Vec<PathCollision>), PullError> {
    let key_epoch = deps.ctx.key_epoch();
    let limits = if deps.ctx.project_view {
        DecodeLimits::project_view()
    } else {
        DecodeLimits::default()
    };
    let ancestor_entries = ancestor_manifest_entries(ancestor)?;
    let ancestor_tree = DirectoryTree::decompose(&ancestor_entries).map_err(PullError::Manifest)?;
    let ancestor_hashes = ancestor_tree
        .subtree_hashes(key_epoch)
        .map_err(PullError::Manifest)?;
    let mut ledger = StoreNodeLedger::new(store, key_epoch);
    let prune = prune_unchanged.then_some(PruneBasis {
        entries: &ancestor_entries,
        hashes: &ancestor_hashes,
        ledger: &ledger,
    });
    let fetched = fetch_tree(FetchTreeRequest {
        objects: deps.objects,
        crypto: &deps.ctx.crypto,
        counters: &deps.ctx.counters,
        root: &head.manifest_key,
        limits: &limits,
        names: deps.ctx.names,
        prune,
    })?;
    // `head_snapshot` is deliberately not named `*manifest`: the flat-manifest
    // cutover gate treats a manifest-named binding's entry-map access as the
    // retired `SnapshotManifest` reader, which this is not.
    let DecodedManifest {
        manifest: head_snapshot,
        collisions,
    } = fetched.decoded;
    // Every node the fetch touched is now known to exist remotely, so remember
    // it: without this a device that only ever pulls would re-download the whole
    // tree on every head, having proven nothing it could reuse.
    let head_tree =
        DirectoryTree::decompose(&head_snapshot.entries).map_err(PullError::Manifest)?;
    for (dir, hash) in head_tree
        .subtree_hashes(key_epoch)
        .map_err(PullError::Manifest)?
    {
        if let Some(node_key) = fetched.node_keys.get(&dir) {
            ledger.record(hash, node_key.clone());
        }
    }
    let recorded = ledger.into_recorded();
    if !recorded.is_empty() {
        store.record_tree_nodes(&recorded, key_epoch)?;
        deps.ctx.counters.record_sqlite_mutation();
    }
    Ok((head_snapshot, collisions))
}

/// The ancestor rows read as manifest entries — the local side of the prune
/// comparison, and the content a pruned subtree is filled from.
fn ancestor_manifest_entries(
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
) -> Result<BTreeMap<WorkspacePath, ManifestEntry>, PullError> {
    ancestor
        .iter()
        .map(|(path, record)| Ok((path.clone(), file_record_to_entry(record)?)))
        .collect()
}

// ---- three-way classification (the merge matrix) ----------------------------

/// The reconciliation a pull computes before touching disk.
#[derive(Default)]
pub(crate) struct MergePlan {
    pub(crate) fs_ops: Vec<FsOp>,
    pub(crate) ancestor_upserts: BTreeMap<WorkspacePath, FileRecord>,
    pub(crate) ancestor_removals: BTreeSet<WorkspacePath>,
    pub(crate) push_again: BTreeSet<WorkspacePath>,
    /// Remote entries this device refuses to materialize. They produce no fs op,
    /// no ancestor row, and no deferral: the rest of the manifest still applies
    /// and the head still advances, so one hostile entry cannot stall sync.
    pub(crate) unsyncable: BTreeMap<WorkspacePath, UnsyncableRecord>,
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
    scope: PullScope<'_>,
) -> Result<MergePlan, PullError> {
    let collided = collision_set(collisions);
    let mut plan = MergePlan::default();
    let mut paths: BTreeSet<&WorkspacePath> = ancestor.keys().collect();
    paths.extend(remote.keys());

    let mut observations: u64 = 0;
    let mut excluded: Vec<&WorkspacePath> = Vec::new();
    for path in paths {
        if is_git_lock_path(path.as_str()) {
            // Git lockfiles are local-only signals, never manifest entries.
            continue;
        }
        // Derived from the ancestor map and the decoded manifest alone: this
        // branch must stay free of filesystem access, because it is what decides
        // whether the path is worth a syscall at all.
        let remote_delta = remote_delta(remote.get(path), ancestor.get(path));
        if !scope.observes(path, &remote_delta) {
            excluded.push(path);
            continue;
        }
        observations += 1;
        let local = local_delta(
            ctx,
            path,
            ancestor.get(path),
            remote_delta.requires_verified_local_content(),
        )?;
        if scope.reconciles_pending(path)
            && ancestor.get(path).is_none()
            && matches!(local, LocalDelta::Absent)
            && remote.get(path).is_some()
        {
            plan.push_again.insert(path.clone());
            continue;
        }
        // A case-fold collision must never silently clobber: force the aside path.
        let force_aside = collided.contains(path.as_str());
        classify_one(
            &mut plan,
            &ctx.workspace_root,
            path,
            &local,
            &remote_delta,
            force_aside,
        )?;
    }
    ctx.counters.record_merge_observations(observations);
    prove_narrowing(ctx, ancestor, scope, &excluded)?;
    Ok(plan)
}

/// Debug-only proof that the narrowed observation set loses nothing.
///
/// Every excluded path has an `Unchanged` remote delta, so it can only reach the
/// `(L::Unchanged, R::Unchanged) => {}` row — unless the driver's dirty set
/// missed a local divergence, which is precisely the failure the narrowing would
/// otherwise hide. The check runs only on a cycle whose stat walk just refreshed
/// the dirty set, where completeness is guaranteed rather than assumed, and it is
/// compiled out of release builds entirely.
#[cfg(debug_assertions)]
fn prove_narrowing(
    ctx: &EngineContext,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    scope: PullScope<'_>,
    excluded: &[&WorkspacePath],
) -> Result<(), PullError> {
    if !matches!(scope, PullScope::ChangedAndWalked(_)) {
        return Ok(());
    }
    for path in excluded {
        match local_delta(ctx, path, ancestor.get(*path), false)? {
            LocalDelta::Unchanged { .. } | LocalDelta::Unreadable => {}
            _ => {
                return Err(PullError::NarrowingMissedChange {
                    path: path.as_str().to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn prove_narrowing(
    _ctx: &EngineContext,
    _ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    _scope: PullScope<'_>,
    _excluded: &[&WorkspacePath],
) -> Result<(), PullError> {
    Ok(())
}

fn classify_one(
    plan: &mut MergePlan,
    root: &Path,
    path: &WorkspacePath,
    local: &LocalDelta,
    remote: &RemoteDelta,
    force_aside: bool,
) -> Result<(), PullError> {
    use LocalDelta as L;
    use RemoteDelta as R;
    // A symlink whose target resolves outside the workspace is dropped from the
    // matrix entirely rather than routed to any row: an install would create the
    // escape, an aside would create it under another name, and adopting the
    // remote's absence would publish a deletion a hostile peer chose. Freezing
    // the path leaves local exactly as it is, mirroring the `L::Unreadable` row.
    //
    // "Outside" is decided by the filesystem-aware gate, which begins with the
    // lexical resolution and then walks it against the local tree. The lexical
    // answer alone is not enough here: a target that is contained on its face can
    // still route through a symlink the user already has (`escape -> /etc`, which
    // Bowline refuses to sync but cannot remove from their disk).
    if let Some(entry) = remote.entry()
        && let ManifestEntry::Symlink { target, .. } = entry
        && !symlink_target_lands_in_workspace(root, path, target)
    {
        plan.unsyncable.insert(
            path.clone(),
            UnsyncableRecord::new(UnsyncableReason::EscapingSymlinkTarget, None, now_unix_ns()),
        );
        return Ok(());
    }
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
        (
            L::Untracked {
                observed,
                content_id,
            },
            R::Created(entry),
        ) => {
            if !force_aside && entry_matches_observed(entry, observed, content_id.as_ref()) {
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
            // The applied remote root still contains this path. Carry the local
            // deletion into the next push explicitly so copy-on-write
            // publication removes it instead of relying on a flat rebuild to
            // omit every row absent from the local ancestor.
            plan.push_again.insert(path.clone());
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
            if !entry_matches_observed(entry, observed, content_id.as_ref()) {
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
        // An unreadable local path freezes every row: no install (it would have
        // to displace bytes we could not verify), no delete (it exists), no
        // ancestor change (we know nothing new about it).
        (L::Unreadable, _) => {}
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
    fn op(&mut self, path: &WorkspacePath, kind: FsOpKind, expected: PreimagePayload) {
        self.fs_ops.push(FsOp {
            path: path.clone(),
            kind,
            expected,
        });
    }

    fn install(&mut self, path: &WorkspacePath, entry: ManifestEntry, expected: PreimagePayload) {
        self.op(path, FsOpKind::Install(entry), expected);
    }

    fn aside(&mut self, path: &WorkspacePath, entry: ManifestEntry, expected: PreimagePayload) {
        self.op(path, FsOpKind::ConflictAside(entry), expected);
    }

    fn delete(&mut self, path: &WorkspacePath, expected: PreimagePayload) {
        self.op(path, FsOpKind::Delete, expected);
    }

    fn mode_change(
        &mut self,
        path: &WorkspacePath,
        entry: ManifestEntry,
        expected: PreimagePayload,
    ) {
        self.op(path, FsOpKind::ModeChange(entry), expected);
    }

    fn adopt(&mut self, path: &WorkspacePath, record: FileRecord) {
        self.ancestor_upserts.insert(path.clone(), record);
    }
}

// ---- observation + collision helpers (shared with apply + intents) --------

/// Observe a path the engine is about to mutate or has just mutated, turning an
/// unsyncable object into the path-scoped refusal the apply/recovery boundary
/// settles.
///
/// `None` means absent, and ONLY absent. The strict `observe` adapter this
/// replaces manufactured an `io::Error` for the unsyncable case, which every
/// caller then swept into `PullError::Io` and thence into `CycleError::Fatal` —
/// so a target that raced into a FIFO, a socket, or a non-UTF-8 symlink between
/// journalling the intent and applying it killed the engine, on every restart.
pub(crate) fn observe_syncable(
    root: &Path,
    path: &WorkspacePath,
) -> Result<Option<Observed>, PullError> {
    match observe_classified(root, path) {
        ObserveOutcome::Present(observed) => Ok(Some(observed)),
        ObserveOutcome::Absent => Ok(None),
        ObserveOutcome::Unsyncable(reason) => Err(PullError::path_refused(path, reason)),
    }
}

pub(crate) fn collision_set(collisions: &[PathCollision]) -> BTreeSet<String> {
    collisions
        .iter()
        .flat_map(|collision| collision.paths.iter())
        .map(|path| path.as_str().to_string())
        .collect()
}

// ---- errors -----------------------------------------------------------------

/// How a pull failed.
///
/// The binding split this type exists to make is [`PullError::Path`] versus
/// everything else. A `Path` value is a fact about ONE workspace path — it is
/// settled where it is raised (keep-local, the path recorded unsyncable, the
/// intent retired) and can never classify as `CycleError::Fatal`. Every other
/// variant is a fault of the engine, the store, or the network.
///
/// There is deliberately no general `Io(io::Error)` arm. That arm was the defect
/// generator: `.map_err(PullError::Io)` was the shortest thing to write at a new
/// call site, it swept a per-path condition into the fatal bucket by omission,
/// and — behind a durable intent that crash recovery replays — turned one racing
/// file into a device that could never start again. Constructing a `Path` value
/// requires naming the path, and [`PullError::engine_scratch`] names the engine's
/// own state directory, so a new call site has to say which it is.
#[derive(Debug)]
pub enum PullError {
    /// A condition scoped to one workspace path. Never fatal.
    Path(UnsyncablePath),
    /// I/O against the engine's OWN private state (`.bowline/tmp`, the
    /// quarantine subtree) — no workspace path is involved, so there is no path
    /// to freeze and this really is an engine fault.
    EngineScratchIo(io::Error),
    Store(ManifestStoreError),
    Manifest(ManifestError),
    Push(PushError),
    Transport(TransportError),
    ManifestKeyMismatch,
    BlobKeyMismatch,
    RefRegressed {
        observed: u64,
        highest: u64,
    },
    RefForked {
        version: u64,
    },
    /// Debug-only: the narrowed observation set excluded a path that turned out
    /// to be locally divergent, so the driver's dirty set was incomplete.
    NarrowingMissedChange {
        path: String,
    },
    Internal {
        reason: &'static str,
    },
}

impl PullError {
    /// A single-path filesystem operation that failed. The reason comes from
    /// [`unsyncable::path_scoped_reason`], which has no "not path-scoped" answer,
    /// so an unmodelled errno cannot become an engine fault here.
    pub(crate) fn path(path: &WorkspacePath, error: &io::Error) -> Self {
        Self::Path(UnsyncablePath::from_io(path, error, now_unix_ns()))
    }

    /// A single-path refusal the engine decided for itself rather than reading
    /// off an errno (an unsyncable object kind, an abandoned replay).
    pub(crate) fn path_refused(path: &WorkspacePath, reason: UnsyncableReason) -> Self {
        Self::Path(UnsyncablePath::new(path, reason, now_unix_ns()))
    }

    /// I/O against the engine's own scratch state, which names no workspace path.
    pub(crate) fn engine_scratch(error: io::Error) -> Self {
        Self::EngineScratchIo(error)
    }
}

impl fmt::Display for PullError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(fault) => write!(
                formatter,
                "pull refused one path {}: {}",
                fault.path.as_str(),
                fault.record.reason
            ),
            Self::EngineScratchIo(error) => {
                write!(formatter, "pull engine scratch io failed: {error}")
            }
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
            Self::NarrowingMissedChange { path } => write!(
                formatter,
                "pull narrowed past a locally divergent path: {path}"
            ),
            Self::Internal { reason } => write!(formatter, "pull internal invariant: {reason}"),
        }
    }
}

impl Error for PullError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EngineScratchIo(error) => Some(error),
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

impl From<TreeError> for PullError {
    fn from(error: TreeError) -> Self {
        match error {
            TreeError::Manifest(error) => Self::Manifest(error),
            TreeError::Store(error) => Self::Store(error),
            TreeError::Transport(error) => Self::Transport(error),
            TreeError::NodeKeyMismatch => Self::ManifestKeyMismatch,
        }
    }
}

#[cfg(test)]
#[path = "pull_apply/directory_tests.rs"]
mod directory_tests;
#[cfg(test)]
#[path = "pull_apply/recovery_tests.rs"]
mod recovery_tests;
#[cfg(test)]
#[path = "pull_apply/tests.rs"]
mod tests;
