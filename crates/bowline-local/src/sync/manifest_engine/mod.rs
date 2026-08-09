//! The manifest-sync engine (Plan 109): a flat-table, change-proportional sync
//! loop that replaces the old convergence/namespace engine.
//!
//! Built here beside the old engine (tests only — no daemon wiring) per the
//! Plan 108/109 staged cutover. `store.rs`, `manifest.rs`, `push.rs`, and
//! `pull_apply.rs` land the ancestor store, the canonical manifest + identity,
//! push, and pull/apply. This file lands the autonomous driver ([`ManifestEngine`]):
//! a single in-memory dirty set + one `scan_required` bit (never a durable
//! queue), debounce with a max-latency cap, and a jittered-backoff failure loop
//! that keeps the committed ancestor sacred through every network fault.
//!
//! Two guards sit in front of everything a cycle does, because the engine's
//! failure modes are asymmetric: a missed change costs a delay, while a wrongly
//! published deletion costs the user's files on every device. `workspace_root`
//! proves the directory about to be scanned is still this workspace, and the
//! push-side deletion breaker refuses a removal batch no plausible edit
//! produces. `unsyncable` is the third: a path the engine cannot read is that
//! path's problem, recorded and surfaced, never the engine's death.

pub mod aux_index;
pub mod counters;
pub mod endpoint;
pub mod fs_guard;
pub mod manifest;
pub mod pull_apply;
pub mod push;
pub mod stat_walk;
pub mod store;
pub mod tree_transport;
pub mod unsyncable;
pub mod work_view;
pub mod work_view_cli;
pub(crate) mod work_view_lock;
pub mod workspace_root;

mod cycle_outcome;
mod dirty_set;
use dirty_set::{DirtySeq, PUBLISH_BATCH_MAX};
mod entry_record;
mod events;
mod publish_cycle;
mod ref_observation;
mod remote;
mod scan_cycle;
mod state;

#[cfg(feature = "test-support")]
pub mod empty_genesis;
#[cfg(test)]
mod engine_test_remotes;
#[cfg(test)]
mod engine_test_support;
#[cfg(test)]
mod generative;
#[cfg(test)]
#[path = "invariant_tests.rs"]
mod invariant_tests;
#[cfg(test)]
#[path = "kill_matrix/tests.rs"]
mod kill_matrix;
#[cfg(test)]
#[path = "normalization_tests.rs"]
mod normalization_tests;
#[cfg(test)]
#[path = "safety_tests.rs"]
mod safety_tests;
#[cfg(test)]
#[path = "scale_fixture.rs"]
mod scale_fixture;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
#[cfg(test)]
mod upload_durability_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub use counters::{CountersSnapshot, EngineCounters};

pub use cycle_outcome::EngineError;
use cycle_outcome::{pull_cycle_error, push_cycle_error};
pub use endpoint::{
    CaseForm, EndpointCapabilities, EndpointInstant, NameFolding, NormalizationForm, StatTrust,
    TimestampGranularity, nfc_path, prepare_endpoint_probe_root, probe_endpoint_capabilities,
    probe_name_folding, probe_timestamp_granularity, refresh_endpoint_capabilities,
    sample_endpoint_clock,
};
pub use events::{
    DeletionConfirmation, EngineConvergenceBarrierId, EngineEndpointGeneration, EngineEvent,
    FullScanReason,
};
pub use fs_guard::{
    ObserveOutcome, Observed, ParentChain, ParentChainMode, observe_classified,
    prepare_parent_chain, read_file_bounded,
};
pub use manifest::directory_tree::{ChildSpec, DirPath, DirectoryTree, SubtreeHash};
pub use manifest::tree::{TREE_FORMAT_VERSION, TreeEntry, TreeEntryPayload, TreeNode};
pub use manifest::{
    BlobKey, DecodeLimits, DecodedManifest, EntryKind, EnvelopePurpose, FileMode, KeyEpoch,
    MAX_WORKSPACE_PATH_DEPTH, MAX_WORKSPACE_PATH_LEN, Manifest, ManifestEntry, ManifestError,
    ManifestKey, PathCollision, PathRejection, WorkspaceCrypto, WorkspacePath, content_id,
    open_file, open_tree_node, physical_blob_key, physical_manifest_key,
    publishable_workspace_path, seal_file, seal_tree_node, tree_node_content_id,
    validate_manifest_path,
};
pub use pull_apply::naming::{CONFLICT_ASIDE_MARKER, conflict_aside_origin, is_conflict_aside};
pub use pull_apply::{
    PullDeps, PullError, PullOutcome, PullScope, RecoveryAction, RecoveryBoundary,
    RecoveryObservation, git_apply_rank, git_lock_active, pull, recover_intents, recovery_action,
    recovery_boundary,
};
#[cfg(test)]
pub(crate) use push::RECOVERY_STATE_DIR;
pub use push::{
    DeletionPolicy, ENGINE_STATE_DIR, EngineConfig, EngineContext, PushDeps, PushError,
    PushOutcome, WatcherEvidence, mass_deletion_threshold, push,
};
use ref_observation::LocalObservation;
pub use remote::{
    BlobPrefetchRequest, BlobReaderUpload, BlobUpload, CasOutcome, ManifestBatchUpload,
    ManifestUpload, PrefetchedBlobs, RefObservation, RefVersionLookup, RemoteObjects, RemoteRef,
    TransportError, TransportFailureClass,
};
pub use scan_cycle::{AuthoritativeScanError, AuthoritativeScanReceipt, AuthoritativeScanRevision};
pub use stat_walk::{
    StatWalk, project_view_verification_paths, stat_walk, stat_walk_project_view,
    stat_walk_subtrees,
};
use state::StateSig;
pub use state::{
    Degradation, EngineConvergenceReceipt, EnginePhase, EngineProcessIdentity, EngineRef,
    EngineSnapshot,
};
pub use store::{
    AncestorCommit, EngineState, FileRecord, Intent, IntentOperationKind, ManifestStore,
    ManifestStoreError, MaterializationRevision, StatFingerprint,
};
pub use tree_transport::{
    FetchTreeRequest, FetchedTree, PruneBasis, PublishTreeRequest, StoreNodeLedger, TreeError,
    TreeNodeLedger, TreeNodeLookup, UnledgeredNodes, fetch_tree, publish_tree,
};
pub use unsyncable::{UnsyncablePath, UnsyncableReason, UnsyncableRecord};
pub use workspace_root::{RootFault, RootState, verify_root};

// ---- driver timing constants ------------------------------------------------

/// Quiescence window: a burst of edits publishes 250 ms after the last one so a
/// noisy save (editors write-truncate-rename) becomes one push, not many.
const DEBOUNCE_MS: u64 = 250;
/// The max-latency cap: even a continuous edit stream must publish within this
/// bound, so debounce can never starve publication (Plan 109 Step 7).
const MAX_LATENCY_MS: u64 = 2_000;
/// Base retry delay for the jittered exponential backoff.
const BACKOFF_BASE_MS: u64 = 250;
/// Backoff ceiling (Plan 109 Step 7): retries never wait longer than this.
const BACKOFF_CAP_MS: u64 = 5_000;
/// One pull-then-push retry inside a cycle before a lost CAS is rescheduled.
/// More than this is pull-and-reschedule, never an attention state.
const MAX_PUSH_ATTEMPTS: u8 = 2;

/// The periodic audit floor and ceiling. Watchers silently drop events — every
/// platform's does, under load, on network mounts, and across sleep — so a purely
/// reactive engine converges only by luck. One cheap stat-only pass on this
/// interval is what makes "Everything Syncs" a property rather than a hope. The
/// spread is Syncthing's 75–125% jitter, so a fleet does not audit in lockstep.
const AUDIT_INTERVAL_MIN_MS: u64 = 30 * 60 * 1_000;
const AUDIT_INTERVAL_MAX_MS: u64 = 60 * 60 * 1_000;

/// How often a stalled engine re-probes the condition that stalled it. An
/// unmounted volume comes back, a hosted fork is resolved by another device: both
/// must self-heal without the user discovering a dead engine hours later.
const STALL_REPROBE_MS: u64 = 30_000;

// ---- clock seam -------------------------------------------------------------

/// A monotonic millisecond clock. Two real impls justify the seam: the system
/// clock the daemon runs on, and a virtual clock tests advance by hand so the
/// debounce/backoff schedule is exercised deterministically without sleeping.
pub trait Clock {
    fn now_millis(&self) -> u64;
}

/// The production clock: milliseconds since the engine started.
pub struct SystemClock {
    base: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            base: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }
}

// ---- driver dependencies ----------------------------------------------------

/// Everything one driver cycle needs beyond the engine's own store and context:
/// the object/ref transport and the clock. The daemon (Plan 111) supplies the
/// real transport and [`SystemClock`]; tests supply fakes.
pub struct EngineIo<'a, O: RemoteObjects, R: RemoteRef, C: Clock> {
    pub objects: &'a O,
    pub refs: &'a R,
    pub clock: &'a C,
}

/// How a cycle failed, so the driver reacts correctly: a transport fault backs
/// off; an integrity fault stalls non-destructively and re-probes; an unavailable
/// root or a refused mass deletion publishes nothing and says why; a per-path
/// condition retries; a genuine invariant violation propagates.
enum CycleError {
    Transport,
    Integrity,
    RootUnavailable(RootFault),
    MassDeletionBlocked {
        removals: BTreeSet<WorkspacePath>,
        entries: usize,
    },
    /// A condition scoped to ONE workspace path reached the driver. Apply and
    /// crash recovery both settle these where they are raised (keep-local, path
    /// recorded unsyncable, intent retired), so this arm is defence in depth —
    /// and it exists precisely so that "a path-scoped condition reached somewhere
    /// unexpected" cannot mean "kill the engine".
    PathScoped,
    Fatal(EngineError),
}

// ---- the engine -------------------------------------------------------------

/// The autonomous manifest-sync driver for one workspace. Owns its ancestor
/// store and context; a single in-memory dirty set and one `scan_required` bit
/// are its whole scheduling state (no durable queue, no lease, no generation
/// counter — those are the old architecture).
pub struct ManifestEngine {
    store: ManifestStore,
    ctx: EngineContext,
    /// The shared cost meters. The engine writes them from its own thread; the
    /// daemon status/metrics surface reads the same `Arc` concurrently.
    counters: Arc<EngineCounters>,

    dirty: Arc<BTreeSet<WorkspacePath>>,
    /// When each dirty path was last observed. A publish that cannot carry the
    /// whole set sends the newest and oldest halves, so a fresh edit is never
    /// stuck behind a backlog and a backlog never starves.
    dirty_seen: BTreeMap<WorkspacePath, DirtySeq>,
    next_dirty_seq: DirtySeq,
    /// What the in-flight publish carried, so the remainder can be retained
    /// without being counted as churn.
    published_batch: BTreeSet<WorkspacePath>,
    dirty_subtrees: Arc<BTreeSet<WorkspacePath>>,
    scan_required: bool,
    pull_needed: bool,
    pending_ref_hint: Option<RefObservation>,
    force_ref_read: bool,
    unattributed_pull_pending: bool,
    startup_reconcile: bool,
    startup_pending: BTreeSet<WorkspacePath>,
    cycle_active: bool,

    debounce_deadline: Option<u64>,
    max_latency_deadline: Option<u64>,
    backoff_deadline: Option<u64>,
    backoff_attempt: u32,
    /// The next periodic safety-net scan (invariant C5), and the counter that
    /// makes its jitter differ between consecutive audits on one device.
    audit_deadline: Option<u64>,
    audit_round: u32,
    /// The re-probe of a stalled condition (missing root, hosted fork).
    stall_deadline: Option<u64>,
    /// One-shot permission to publish exactly the removal paths the operator
    /// reviewed. Consumed before a cycle performs any I/O.
    confirmed_removals: Option<Arc<BTreeSet<WorkspacePath>>>,

    revision: u64,
    phase: EnginePhase,
    // The last observed CAS head. Named distinctly from the public
    // `EngineSnapshot.observed_ref` it feeds, so this internal init is not
    // mistaken for the old convergence engine's single `observed_ref` authority.
    head_ref: Option<RefObservation>,
    applied_ref: EngineRef,
    materialization_revision: MaterializationRevision,
    pending_intents: usize,
    pending_intent_paths: Arc<BTreeSet<WorkspacePath>>,
    last_success_at: Option<u64>,
    degradation: Degradation,
    unsyncable: Arc<BTreeMap<WorkspacePath, UnsyncableRecord>>,
    /// The removals the deletion breaker refused. Written only by
    /// `block_mass_deletion` and cleared only by `set_degradation`.
    pub(super) refused_removals: Arc<BTreeSet<WorkspacePath>>,
    last_sig: Option<StateSig>,
    pending_barriers: BTreeMap<EngineConvergenceBarrierId, EngineEndpointGeneration>,
    completed_barriers: BTreeMap<EngineConvergenceBarrierId, EngineConvergenceReceipt>,
}

impl ManifestEngine {
    /// Construct the driver over an already-open store and context. Startup work
    /// (recovery, the seeding stat walk, the ref read) runs in [`Self::start`].
    pub fn new(store: ManifestStore, ctx: EngineContext) -> Self {
        let counters = ctx.counters.clone();
        Self {
            store,
            ctx,
            counters,
            dirty: Arc::new(BTreeSet::new()),
            dirty_seen: BTreeMap::new(),
            next_dirty_seq: DirtySeq::INITIAL,
            published_batch: BTreeSet::new(),
            dirty_subtrees: Arc::new(BTreeSet::new()),
            scan_required: false,
            pull_needed: false,
            pending_ref_hint: None,
            force_ref_read: false,
            unattributed_pull_pending: false,
            startup_reconcile: true,
            startup_pending: BTreeSet::new(),
            cycle_active: false,
            debounce_deadline: None,
            max_latency_deadline: None,
            backoff_deadline: None,
            backoff_attempt: 0,
            audit_deadline: None,
            audit_round: 0,
            stall_deadline: None,
            confirmed_removals: None,
            revision: 0,
            phase: EnginePhase::Starting,
            head_ref: None,
            applied_ref: EngineRef::Genesis,
            materialization_revision: MaterializationRevision::INITIAL,
            pending_intents: 0,
            pending_intent_paths: Arc::new(BTreeSet::new()),
            last_success_at: None,
            degradation: Degradation::Nominal,
            unsyncable: Arc::new(BTreeMap::new()),
            refused_removals: Arc::new(BTreeSet::new()),
            last_sig: None,
            pending_barriers: BTreeMap::new(),
            completed_barriers: BTreeMap::new(),
        }
    }

    /// A read-only view of the current state for a status consumer.
    pub fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            revision: self.revision,
            phase: self.phase,
            observed_ref: self.head_ref.clone(),
            applied_ref: self.applied_ref.clone(),
            materialization_revision: self.materialization_revision,
            pending_intents: self.pending_intents,
            dirty: self.dirty.len().saturating_add(self.dirty_subtrees.len()),
            dirty_paths: Arc::clone(&self.dirty),
            dirty_subtree_paths: Arc::clone(&self.dirty_subtrees),
            pending_intent_paths: Arc::clone(&self.pending_intent_paths),
            scan_required: self.scan_required,
            unattributed_pull_pending: self.unattributed_pull_pending,
            cycle_active: self.cycle_active,
            last_success_at: self.last_success_at,
            degradation: self.degradation,
            unsyncable: Arc::clone(&self.unsyncable),
            refused_removals: Arc::clone(&self.refused_removals),
        }
    }

    /// Allow ONE push to publish a removal batch above the safety threshold.
    ///
    /// The operator surface for [`Degradation::MassDeletionBlocked`]: the user has
    /// seen the paths, agrees the deletions are real, and is telling the engine to
    /// proceed. Re-arms the schedule so the confirmed push runs immediately.
    ///
    /// Authorises nothing unless a batch is actually blocked right now. An
    /// unconditional arm would let a confirmation issued against yesterday's
    /// refusal wave through tomorrow's unrelated one, which is the guard's whole
    /// blast radius handed away by a race.
    pub fn confirm_mass_deletion<C: Clock>(&mut self, clock: &C) -> DeletionConfirmation {
        let Degradation::MassDeletionBlocked { removals, entries } = self.degradation else {
            return DeletionConfirmation::NotBlocked;
        };
        self.confirmed_removals = Some(Arc::clone(&self.refused_removals));
        self.set_degradation(Degradation::Nominal);
        self.debounce_deadline = Some(clock.now_millis());
        self.preempt_backoff();
        self.bump_revision_if_changed();
        DeletionConfirmation::Authorized { removals, entries }
    }

    /// Test/introspection accessor: the paths currently queued for the next push.
    pub fn dirty_paths(&self) -> &BTreeSet<WorkspacePath> {
        self.dirty.as_ref()
    }

    /// A shared handle to the engine cost meters, for the daemon status/metrics
    /// surface. Reading it never blocks the engine thread.
    pub fn counters(&self) -> Arc<EngineCounters> {
        self.counters.clone()
    }

    /// Drain barriers completed by the most recent successful convergence
    /// cycle. The daemon driver uses these acknowledgements to wake exact RPC
    /// waiters; they are deliberately not part of public status state.
    pub fn take_completed_barriers(&mut self) -> Vec<EngineConvergenceReceipt> {
        std::mem::take(&mut self.completed_barriers)
            .into_values()
            .collect()
    }

    /// The next scheduled wakeup as a timeout from `now`, or `None` when the
    /// engine is idle (block on the next event — idle costs nothing, C1).
    pub fn next_timeout(&self, now: u64) -> Option<Duration> {
        self.next_due()
            .map(|due| Duration::from_millis(due.saturating_sub(now)))
    }

    fn next_due(&self) -> Option<u64> {
        [
            self.debounce_deadline,
            self.max_latency_deadline,
            self.backoff_deadline,
            self.audit_deadline,
            self.stall_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Schedule the next periodic audit at a jittered 75–125% of the nominal
    /// interval. The jitter is deterministic (device id + audit round) so tests
    /// are stable and a fleet's audits stay spread out.
    fn arm_audit(&mut self, now: u64) {
        self.audit_round = self.audit_round.wrapping_add(1);
        let span = AUDIT_INTERVAL_MAX_MS - AUDIT_INTERVAL_MIN_MS;
        let mut seed = u64::from(self.audit_round);
        for byte in self.ctx.device_id.as_str().bytes() {
            seed = seed.wrapping_mul(31).wrapping_add(u64::from(byte));
        }
        self.audit_deadline = Some(now + AUDIT_INTERVAL_MIN_MS + seed % (span + 1));
    }

    // ---- startup rule -------------------------------------------------------

    /// Startup (Plan 108 RESTART, binding): recover intents → one stat walk →
    /// synchronous ref read + verify → genesis publish or pull-first. The
    /// subscription is a wakeup, never startup authority.
    pub fn start<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), EngineError> {
        self.phase = EnginePhase::Starting;
        self.unattributed_pull_pending = true;
        self.arm_audit(io.clock.now_millis());
        // The root sentinel gates startup too: recovery rematerializes files, and
        // rematerializing into a wrong or unmounted root is exactly the damage the
        // sentinel exists to prevent.
        if let Err(error) = self.guard_root() {
            let now = io.clock.now_millis();
            let absorbed = self.absorb_cycle_error(error, now);
            self.refresh_and_bump(io);
            return absorbed;
        }
        // Recover in-flight intents FIRST so the seeding stat walk observes the
        // post-recovery tree, not a half-applied one.
        let deps = PullDeps {
            ctx: &self.ctx,
            objects: io.objects,
            refs: io.refs,
            // Recovery replays journalled intents by path; it never classifies a
            // merge, so the scope it carries is inert here.
            scope: pull_apply::PullScope::ChangedAndDirty(&self.dirty),
        };
        if let Err(error) = recover_intents(&mut self.store, &deps) {
            // A transport fault at startup is not fatal: back off and retry (the
            // next pull re-runs recovery). An integrity fault stalls
            // non-destructively. Only a genuine bug propagates.
            let now = io.clock.now_millis();
            let classified = pull_cycle_error(error);
            if matches!(classified, CycleError::Transport) {
                self.pull_needed = true;
            }
            let absorbed = self.absorb_cycle_error(classified, now);
            self.refresh_and_bump(io);
            return absorbed;
        }
        let pending_push = self
            .store
            .pending_push_paths()
            .map_err(EngineError::Store)?;
        self.startup_pending = pending_push.clone();
        self.absorb_dirty(pending_push);
        // Seed the dirty set from one stat walk, then pull-first before any push.
        // Schedule the startup cycle immediately so `run_due_work` runs it now
        // (the deadline gate would otherwise skip flag-only work).
        self.scan_required = true;
        self.pull_needed = true;
        self.debounce_deadline = Some(io.clock.now_millis());
        self.run_due_work(io)
    }

    // ---- event folding ------------------------------------------------------

    /// Fold one event into the in-memory schedule. Pure state mutation — the IO
    /// happens in [`Self::run_due_work`] when a deadline elapses.
    pub fn on_event<C: Clock>(&mut self, event: EngineEvent, clock: &C) {
        let now = clock.now_millis();
        match event {
            EngineEvent::Paths(paths) => {
                self.absorb_dirty(paths);
                self.arm_debounce(now);
                self.preempt_backoff();
            }
            EngineEvent::RecursivePaths(paths) => {
                let folded = self.canonical_paths(paths);
                Arc::make_mut(&mut self.dirty_subtrees).extend(folded);
                self.arm_debounce(now);
                self.preempt_backoff();
            }
            EngineEvent::FullScanRequired(reason) => {
                self.scan_required = true;
                self.set_degradation(Degradation::FullScanRequired(reason));
                // Overflow/disconnect/root-replacement recover immediately.
                self.debounce_deadline = Some(now);
                self.preempt_backoff();
            }
            EngineEvent::RefChanged => {
                self.pending_ref_hint = None;
                self.force_ref_read = true;
                self.pull_needed = true;
                self.unattributed_pull_pending = true;
                // A ref wakeup may be the echo of our preceding partial
                // directory publish. Do not let it collapse an active native
                // watcher burst: a recursive root must be observed only after
                // its normal debounce, when already-created descendants are
                // visible. Pulling the remote head is bounded by that same
                // short window.
                if self.dirty_subtrees.is_empty() || self.debounce_deadline.is_none() {
                    self.debounce_deadline = Some(now);
                }
                self.preempt_backoff();
            }
            EngineEvent::RefObserved(observed) => {
                if self.coalesce_ref_hint(observed) {
                    self.pull_needed = true;
                    self.unattributed_pull_pending = true;
                    if self.dirty_subtrees.is_empty() || self.debounce_deadline.is_none() {
                        self.debounce_deadline = Some(now);
                    }
                    self.preempt_backoff();
                }
            }
            EngineEvent::ConnectivityRestored => {
                self.pending_ref_hint = None;
                self.force_ref_read = true;
                self.pull_needed = true;
                self.unattributed_pull_pending = true;
                self.debounce_deadline = Some(now);
                self.preempt_backoff();
            }
            EngineEvent::EngineConvergenceBarrier {
                id,
                endpoint_generation,
            } => {
                self.pending_ref_hint = None;
                self.force_ref_read = true;
                self.pending_barriers.insert(id, endpoint_generation);
                self.scan_required = true;
                self.pull_needed = true;
                self.unattributed_pull_pending = true;
                self.set_degradation(Degradation::FullScanRequired(
                    FullScanReason::EngineConvergenceBarrier,
                ));
                self.debounce_deadline = Some(now);
                self.preempt_backoff();
            }
            EngineEvent::CancelEngineConvergenceBarrier {
                id,
                endpoint_generation,
            } => {
                if self.pending_barriers.get(&id) == Some(&endpoint_generation) {
                    self.pending_barriers.remove(&id);
                }
                if self
                    .completed_barriers
                    .get(&id)
                    .is_some_and(|receipt| receipt.endpoint_generation() == endpoint_generation)
                {
                    self.completed_barriers.remove(&id);
                }
            }
            EngineEvent::ConfirmMassDeletion => {
                // `confirm_mass_deletion` owns the whole transition (arm, clear,
                // reschedule) and bumps the revision itself; folding it here would
                // duplicate that decision in a second place.
                let _authorized = self.confirm_mass_deletion(clock);
            }
            EngineEvent::Shutdown => {
                self.phase = EnginePhase::Stopped;
            }
        }
        self.bump_revision_if_changed();
    }

    /// Announce the pre-I/O transition that the daemon publishes before a due
    /// cycle. This closes the status race while a pull discovers its paths.
    pub fn announce_due_work<C: Clock>(&mut self, clock: &C) -> bool {
        let now = clock.now_millis();
        if !self.next_due().is_some_and(|due| due <= now) {
            return false;
        }
        self.phase = EnginePhase::Syncing;
        self.cycle_active = true;
        self.unattributed_pull_pending = false;
        self.bump_revision_if_changed();
        true
    }

    fn arm_debounce(&mut self, now: u64) {
        self.debounce_deadline = Some(now + DEBOUNCE_MS);
        // The cap is armed once per burst and never pushed forward, so a
        // continuous stream still publishes within MAX_LATENCY_MS.
        if self.max_latency_deadline.is_none() {
            self.max_latency_deadline = Some(now + MAX_LATENCY_MS);
        }
    }

    /// An actionable event preempts a pending backoff: clear the delay and reset
    /// the attempt count so the next cycle runs promptly (Plan 109 Step 7).
    fn preempt_backoff(&mut self) {
        self.backoff_deadline = None;
        self.backoff_attempt = 0;
    }

    // ---- the run loop -------------------------------------------------------

    /// Drive the engine from an event channel using the real clock. The daemon
    /// owns the producing side (Plan 111); this is the thin system glue over the
    /// same `on_event`/`run_due_work` the tests drive directly.
    pub fn run<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        inbox: &Receiver<EngineEvent>,
        io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), EngineError> {
        self.start(io)?;
        loop {
            let received = match self.next_timeout(io.clock.now_millis()) {
                Some(timeout) => inbox.recv_timeout(timeout),
                None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(EngineEvent::Shutdown) => {
                    self.phase = EnginePhase::Stopped;
                    break;
                }
                Ok(event) => self.on_event(event, io.clock),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.run_due_work(io)?;
        }
        Ok(())
    }

    /// Run any work whose deadline has elapsed. Called after every event and on
    /// every timeout; a no-op when nothing is due, so an idle engine performs no
    /// store, network, or filesystem work (invariant C1).
    pub fn run_due_work<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), EngineError> {
        let now = io.clock.now_millis();
        match self.next_due() {
            Some(due) if due <= now => {}
            _ => return Ok(()),
        }
        // Consume the debounce/latency window for this run; backoff is cleared
        // only on success.
        self.debounce_deadline = None;
        self.max_latency_deadline = None;
        self.stall_deadline = None;
        if self.audit_deadline.is_some_and(|due| due <= now) {
            self.arm_audit(now);
            self.scan_required = true;
            self.pull_needed = true;
            if self.degradation == Degradation::Nominal {
                self.set_degradation(Degradation::FullScanRequired(FullScanReason::PeriodicAudit));
            }
        }

        self.phase = EnginePhase::Syncing;
        self.cycle_active = true;
        self.unattributed_pull_pending = false;
        let result = self.run_cycle(io);
        self.cycle_active = false;
        match result {
            Ok(()) => {
                self.backoff_deadline = None;
                self.backoff_attempt = 0;
                self.last_success_at = Some(now);
                if self.degradation_is_transient() {
                    self.set_degradation(Degradation::Nominal);
                }
                self.phase = if self.idle() {
                    EnginePhase::Idle
                } else {
                    EnginePhase::Syncing
                };
            }
            Err(error) => {
                if let Err(fatal) = self.absorb_cycle_error(error, now) {
                    self.refresh_and_bump(io);
                    return Err(fatal);
                }
            }
        }
        self.refresh_and_bump(io);
        let snapshot = self.snapshot();
        if snapshot.is_exactly_converged() {
            for (id, endpoint_generation) in std::mem::take(&mut self.pending_barriers) {
                if let Some(receipt) = snapshot.convergence_receipt(
                    self.ctx.process_identity.clone(),
                    self.ctx.workspace_identity.clone(),
                    endpoint_generation,
                    id,
                ) {
                    self.completed_barriers.insert(id, receipt);
                }
            }
        }
        Ok(())
    }

    /// Turn a failed cycle into engine state. `Err` is returned ONLY for a
    /// genuine invariant violation; every other classification is a state the
    /// engine recovers from without losing the workspace.
    fn absorb_cycle_error(&mut self, error: CycleError, now: u64) -> Result<(), EngineError> {
        match error {
            CycleError::Transport => self.enter_backoff(now),
            CycleError::Integrity => {
                self.set_degradation(Degradation::IntegrityStalled);
                self.phase = EnginePhase::Stalled;
                // A fork clears when some other device advances the hosted ref.
                // A stalled device provokes no ref event of its own, so without
                // this re-probe it waits forever for a wake that cannot come.
                self.pull_needed = true;
                self.force_ref_read = true;
                self.stall_deadline = Some(now + STALL_REPROBE_MS);
            }
            CycleError::RootUnavailable(fault) => {
                self.set_degradation(Degradation::RootUnavailable(fault));
                self.phase = EnginePhase::Stalled;
                // Whatever comes back may differ arbitrarily from what this device
                // last saw, so recovery is a full re-observation, not a replay.
                self.scan_required = true;
                self.pull_needed = true;
                self.stall_deadline = Some(now + STALL_REPROBE_MS);
            }
            CycleError::MassDeletionBlocked { removals, entries } => {
                self.block_mass_deletion(removals, entries);
                self.phase = EnginePhase::Stalled;
                // Deliberately NO deadline: a cycle with nothing new to observe
                // would refuse the same batch again forever. What DOES change the
                // answer is the remote, so the next cycle — whenever a ref event,
                // an edit, or the audit brings one — pulls before it pushes. A
                // device parked here must never be a device that stopped
                // receiving; that is the trap this arm used to set.
                self.pull_needed = true;
            }
            CycleError::PathScoped => {
                // Nothing about the workspace is broken and nothing was
                // published; re-observe both authorities on the stall interval so
                // the condition (a raced object kind, a full volume) is retried
                // once it clears. The path itself is already durable in the
                // unsyncable ledger, which status surfaces.
                self.scan_required = true;
                self.pull_needed = true;
                self.force_ref_read = true;
                self.stall_deadline = Some(now + STALL_REPROBE_MS);
            }
            CycleError::Fatal(error) => return Err(error),
        }
        Ok(())
    }

    fn enter_backoff(&mut self, now: u64) {
        self.counters.record_retry();
        self.backoff_attempt = self.backoff_attempt.saturating_add(1);
        let delay = self.backoff_delay(self.backoff_attempt);
        self.backoff_deadline = Some(now + delay);
        self.set_degradation(Degradation::OfflineRetrying {
            attempt: self.backoff_attempt,
        });
        self.phase = EnginePhase::BackingOff;
    }

    /// Jittered exponential backoff, capped at [`BACKOFF_CAP_MS`]. The jitter is
    /// deterministic (seeded by the device id + attempt) so tests are stable and
    /// a fleet does not synchronize its retries.
    fn backoff_delay(&self, attempt: u32) -> u64 {
        let exp = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.min(8));
        let base = exp.min(BACKOFF_CAP_MS);
        let jitter_span = base / 4;
        if jitter_span == 0 {
            return base;
        }
        let mut seed = attempt as u64;
        for byte in self.ctx.device_id.as_str().bytes() {
            seed = seed.wrapping_mul(31).wrapping_add(byte as u64);
        }
        base.saturating_sub(jitter_span) + (seed % (jitter_span + 1))
    }

    // ---- one sync cycle -----------------------------------------------------

    /// The whole loop for one wakeup: optional full scan, pull-first, then a
    /// bounded push-with-one-retry. The committed ancestor is never mutated by a
    /// scan or a failed CAS — only push/pull commit transactions touch it.
    fn run_cycle<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), CycleError> {
        // Spend the capability before any fallible cycle work. A root, scan, or
        // transport failure must not carry yesterday's approval into a later
        // cycle whose watcher events may name additional removals.
        let deletions = self
            .confirmed_removals
            .take()
            .map(push::DeletionAuthorization::ConfirmedPaths)
            .unwrap_or(push::DeletionAuthorization::Enforce);
        // NOTHING in a cycle may touch the workspace before the root is proven.
        self.guard_root()?;
        let mut observation = LocalObservation::Reactive;
        if self.scan_required {
            self.full_scan()?;
            self.scan_required = false;
            Arc::make_mut(&mut self.dirty_subtrees).clear();
            observation = LocalObservation::FreshlyWalked;
        }
        if !self.dirty_subtrees.is_empty() {
            self.scan_dirty_subtrees(io)?;
        }
        let mut pulled = false;
        if self.pull_needed {
            // Clear BEFORE pulling: do_pull re-arms pull_needed (and a debounce
            // deadline) when paths were deferred by an active Git lock, and a
            // post-call reset would clobber that internally scheduled retry,
            // leaving deferred paths unmaterialized until an external ref event.
            self.pull_needed = false;
            self.do_pull(io, observation)?;
            pulled = true;
        }

        self.publish_dirty(io, observation, pulled, deletions)?;

        if !self.dirty.is_empty() {
            // Two ways to land here, both a reschedule and never an attention
            // state: repeated CAS loss (pull the winner and re-push), or paths a
            // scan could not settle because they were being written (retain and
            // rescan). Arming the debounce deadline WITHOUT a new watcher event is
            // what lets a change that settles after racing writes still publish;
            // dropping these would be a silent unsynced-change violation. When
            // nothing skipped and no CAS loss, the dirty set is empty here, so no
            // deadline is armed and the engine stays idle (invariant C1).
            self.pull_needed = true;
            self.debounce_deadline = Some(io.clock.now_millis() + DEBOUNCE_MS);
        }
        Ok(())
    }

    /// Put watcher- and walk-reported paths into the spelling the engine's own
    /// path space uses, so a macOS NFD event and the NFC entry a peer published
    /// name one dirty path rather than two (see [`endpoint::NameFolding`]).
    /// Arm the next cycle for the publish window a recovery attempt just
    /// released.
    ///
    /// Paths absorbed from an authoritative scan never arm a deadline, so the
    /// window between two recovery attempts would otherwise find nothing due
    /// and publish nothing, leaving the release pointless.
    pub fn schedule_recovered_work<C: Clock>(&mut self, clock: &C) {
        if self.dirty.is_empty() {
            return;
        }
        self.debounce_deadline = Some(clock.now_millis());
        self.bump_revision_if_changed();
    }
}
