//! The engine's in-memory state: the read-only snapshot the daemon publishes,
//! the phase/degradation vocabulary it reports, and the transition signature
//! that decides when a snapshot revision moves.
//!
//! Split from `mod.rs`, which owns the driver's scheduling and cycle logic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::{
    Clock, EngineIo, FullScanReason, ManifestEngine, ManifestKey, RefObservation, RemoteObjects,
    RemoteRef, RootFault, UnsyncableRecord, WorkspacePath,
};

/// Coarse engine phase for the snapshot. Momentary; the durable facts are the
/// ref/manifest/intents fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePhase {
    Starting,
    Idle,
    Syncing,
    BackingOff,
    Stalled,
    Stopped,
}

/// The engine's health, distinct from its phase. Nominal is healthy; the rest
/// are non-fatal (a lost CAS is never here — it is normal). `RootUnavailable`
/// clears only when the root sentinel is satisfied again, and
/// `MassDeletionBlocked` only when a push actually publishes — clearing either
/// one on a timer would mean destroying user data on a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    Nominal,
    FullScanRequired(FullScanReason),
    OfflineRetrying {
        attempt: u32,
    },
    IntegrityStalled,
    /// The workspace root is gone, is not a directory, or is not the workspace
    /// this device committed its ancestor against. NOTHING is published while
    /// this holds.
    RootUnavailable(RootFault),
    /// A push would have removed an implausible share of the workspace and was
    /// refused. Cleared only by [`ManifestEngine::confirm_mass_deletion`]. The
    /// counts are the arithmetic the guard performed; the paths themselves live
    /// in [`EngineSnapshot::refused_removals`], which no `Copy` state may hold.
    /// The ceiling is not stored beside them: it is exactly
    /// [`mass_deletion_threshold`] of `entries`, and a stored copy could drift.
    MassDeletionBlocked {
        removals: usize,
        entries: usize,
    },
    /// The engine's own store could not be read, so the reported engine state is
    /// unknown rather than the last good value.
    StoreUnavailable,
}

/// A read-only snapshot of engine state: in-memory facts only, no JSON method,
/// no queue fiction. `revision` bumps ONLY on a state transition, so a status
/// consumer that polls an idle engine sees a stable revision (Plan 109 Step 7 /
/// review Change 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub revision: u64,
    pub phase: EnginePhase,
    pub observed_ref: Option<RefObservation>,
    pub applied_manifest: Option<ManifestKey>,
    pub pending_intents: usize,
    /// Dirty paths queued for the next push. Exposed so the daemon status
    /// projection can report a truthful outbound queue count (Plan 111 Step 1c)
    /// without a second `dirty_paths()` round-trip.
    pub dirty: usize,
    /// Attributed work retained for project-scoped status. The workspace-wide
    /// counters above remain the canonical global projection.
    pub dirty_paths: Arc<BTreeSet<WorkspacePath>>,
    pub dirty_subtree_paths: Arc<BTreeSet<WorkspacePath>>,
    pub pending_intent_paths: Arc<BTreeSet<WorkspacePath>>,
    /// A pending full scan makes every project scope conservatively
    /// non-ready because the attributed sets may still be incomplete.
    pub scan_required: bool,
    /// A remote wake has scheduled a pull whose paths are not known yet, or a
    /// cycle is currently able to discover such paths.
    pub unattributed_pull_pending: bool,
    pub cycle_active: bool,
    pub last_success_at: Option<u64>,
    pub degradation: Degradation,
    /// Paths this device cannot sync, with the reason and remedy for each. A
    /// non-empty set is an actionable status item, never a silent omission from
    /// a product whose invariant is "Everything Syncs".
    pub unsyncable: Arc<BTreeMap<WorkspacePath, UnsyncableRecord>>,
    /// Exactly the removals the deletion breaker refused, so an operator can
    /// read what a confirmation would publish before authorising it. Non-empty
    /// if and only if `degradation` is [`Degradation::MassDeletionBlocked`].
    pub refused_removals: Arc<BTreeSet<WorkspacePath>>,
}

impl Default for EngineSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            phase: EnginePhase::Starting,
            observed_ref: None,
            applied_manifest: None,
            pending_intents: 0,
            dirty: 0,
            dirty_paths: Arc::new(BTreeSet::new()),
            dirty_subtree_paths: Arc::new(BTreeSet::new()),
            pending_intent_paths: Arc::new(BTreeSet::new()),
            scan_required: false,
            unattributed_pull_pending: false,
            cycle_active: false,
            last_success_at: None,
            degradation: Degradation::Nominal,
            unsyncable: Arc::new(BTreeMap::new()),
            refused_removals: Arc::new(BTreeSet::new()),
        }
    }
}

/// The transition signature: the fields whose change is a state transition (and
/// so bumps `revision`). Deliberately excludes wall-clock time and scheduling
/// deadlines, so an idle poll never advances the revision.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct StateSig {
    phase: EnginePhase,
    degradation: Degradation,
    applied_manifest: Option<ManifestKey>,
    observed_version: Option<u64>,
    pending_intents: usize,
    pending_intent_paths: Arc<BTreeSet<WorkspacePath>>,
    dirty_paths: Arc<BTreeSet<WorkspacePath>>,
    dirty_subtree_paths: Arc<BTreeSet<WorkspacePath>>,
    scan_required: bool,
    unattributed_pull_pending: bool,
    cycle_active: bool,
    unsyncable: Arc<BTreeMap<WorkspacePath, UnsyncableRecord>>,
    refused_removals: Arc<BTreeSet<WorkspacePath>>,
}

impl ManifestEngine {
    /// The one writer of `degradation`.
    ///
    /// Every transition out of [`Degradation::MassDeletionBlocked`] must drop the
    /// refused removal set with it: a snapshot that still lists paths nothing is
    /// refusing would offer an operator a confirmation for a push that no longer
    /// exists. Routing every write through here is what makes that invariant a
    /// property of the type rather than of remembering.
    pub(super) fn set_degradation(&mut self, degradation: Degradation) {
        if !matches!(degradation, Degradation::MassDeletionBlocked { .. })
            && !self.refused_removals.is_empty()
        {
            self.refused_removals = Arc::new(BTreeSet::new());
        }
        self.degradation = degradation;
    }

    /// Record a refused removal batch: the counts status reports, and the paths
    /// the operator surface lists.
    pub(super) fn block_mass_deletion(
        &mut self,
        removals: BTreeSet<WorkspacePath>,
        entries: usize,
    ) {
        self.set_degradation(Degradation::MassDeletionBlocked {
            removals: removals.len(),
            entries,
        });
        self.refused_removals = Arc::new(removals);
    }

    pub(super) fn idle(&self) -> bool {
        self.dirty.is_empty()
            && self.dirty_subtrees.is_empty()
            && !self.scan_required
            && !self.pull_needed
    }

    /// Whether a successful cycle clears this degradation. `RootUnavailable` is
    /// absent on purpose: only the root sentinel may declare the root trustworthy
    /// again, and it does so with a full rescan. `StoreUnavailable` is likewise
    /// owned by the store read in `refresh_and_bump`.
    pub(super) fn degradation_is_transient(&self) -> bool {
        matches!(
            self.degradation,
            Degradation::OfflineRetrying { .. }
                | Degradation::FullScanRequired(_)
                // A push that published is proof the batch is no longer refused,
                // whether the user confirmed it or restored the missing files.
                | Degradation::MassDeletionBlocked { .. }
        )
    }

    /// Re-read the durable engine facts the snapshot reports.
    ///
    /// A read failure MUST NOT be swallowed: replaying the last good values makes
    /// status report a healthy, unchanged engine while its database is failing,
    /// and the unchanged signature means the revision does not even move, so no
    /// consumer learns anything is wrong. Record it as a degradation instead.
    pub(super) fn refresh_and_bump<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        _io: &EngineIo<'_, O, R, C>,
    ) {
        let mut store_failed = false;
        match self.store.engine_state() {
            Ok(state) => self.applied_manifest = state.applied_manifest_key,
            Err(_) => store_failed = true,
        }
        match self.store.pending_intents() {
            Ok(intents) => {
                self.pending_intents = intents.len();
                self.pending_intent_paths = Arc::new(
                    intents
                        .into_iter()
                        .map(|intent| intent.path)
                        .collect::<BTreeSet<_>>(),
                );
            }
            Err(_) => store_failed = true,
        }
        match self.store.unsyncable() {
            Ok(entries) => self.unsyncable = Arc::new(entries),
            Err(_) => store_failed = true,
        }
        if store_failed {
            self.set_degradation(Degradation::StoreUnavailable);
        } else if self.degradation == Degradation::StoreUnavailable {
            self.set_degradation(Degradation::Nominal);
        }
        self.bump_revision_if_changed();
    }

    pub(super) fn bump_revision_if_changed(&mut self) {
        let sig = StateSig {
            phase: self.phase,
            degradation: self.degradation,
            applied_manifest: self.applied_manifest.clone(),
            observed_version: self.head_ref.as_ref().map(|observed| observed.version),
            pending_intents: self.pending_intents,
            pending_intent_paths: self.pending_intent_paths.clone(),
            dirty_paths: self.dirty.clone(),
            dirty_subtree_paths: self.dirty_subtrees.clone(),
            scan_required: self.scan_required,
            unattributed_pull_pending: self.unattributed_pull_pending,
            cycle_active: self.cycle_active,
            unsyncable: self.unsyncable.clone(),
            refused_removals: self.refused_removals.clone(),
        };
        if self.last_sig.as_ref() != Some(&sig) {
            self.revision += 1;
            self.last_sig = Some(sig);
        }
    }
}
