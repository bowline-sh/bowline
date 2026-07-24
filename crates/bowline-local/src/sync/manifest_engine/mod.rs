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

pub mod aux_index;
pub mod counters;
pub mod fs_guard;
pub mod manifest;
pub mod pull_apply;
pub mod push;
pub mod stat_walk;
pub mod store;
pub mod work_view;
pub mod work_view_cli;

mod ref_observation;
mod state;

#[cfg(test)]
mod engine_test_support;
#[cfg(test)]
#[path = "invariant_tests.rs"]
mod invariant_tests;
#[cfg(test)]
#[path = "kill_matrix/tests.rs"]
mod kill_matrix;
#[cfg(test)]
#[path = "scale_fixture.rs"]
mod scale_fixture;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub use counters::{CountersSnapshot, EngineCounters};

pub use fs_guard::{
    Observed, ParentChain, ParentChainMode, observe, prepare_parent_chain, read_file_bounded,
};
pub use manifest::{
    BlobKey, DecodeLimits, DecodedManifest, EntryKind, EnvelopePurpose, FileMode, KeyEpoch,
    MANIFEST_FORMAT_VERSION, Manifest, ManifestEntry, ManifestError, ManifestKey, PathCollision,
    WorkspaceCrypto, WorkspacePath, content_id, decode_manifest_plaintext, manifest_content_id,
    open_file, open_manifest, physical_blob_key, physical_manifest_key, seal_file, seal_manifest,
};
pub use pull_apply::{
    PullDeps, PullError, PullOutcome, RecoveryAction, RecoveryBoundary, RecoveryObservation,
    git_apply_rank, git_lock_active, pull, recover_intents, recovery_action, recovery_boundary,
};
pub use push::{
    BlobReaderUpload, BlobUpload, CasOutcome, ENGINE_STATE_DIR, EngineConfig, EngineContext,
    ManifestUpload, PushDeps, PushError, PushOutcome, RefObservation, RemoteObjects, RemoteRef,
    TransportError, push,
};
pub use stat_walk::{
    StatWalk, project_view_verification_paths, stat_walk, stat_walk_project_view,
    stat_walk_subtrees,
};
pub use store::{
    AncestorCommit, EngineState, FileRecord, Intent, IntentOperationKind, ManifestStore,
    ManifestStoreError, StatFingerprint,
};

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

// ---- events + snapshot ------------------------------------------------------

/// Why a full stat walk was demanded. A lost watcher event, an overflow, a
/// disconnect, or a root replacement all reduce to the same cheap recovery: one
/// stat-only pass. The variant is carried so the snapshot can explain the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullScanReason {
    WatcherOverflow,
    WatcherDisconnected,
    RootReplaced,
    PeriodicAudit,
    /// An explicit caller boundary: re-observe disk and the hosted ref before
    /// acknowledging that sync is caught up.
    SyncBarrier,
}

/// Opaque identity for one caller-requested convergence barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SyncBarrierId(pub u64);

/// The events the daemon (Plan 111) feeds the engine. No event carries durable
/// authority: paths are re-derived from disk, while a verified ref observation
/// is only a freshness-checked hint for a scheduled pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// Watcher-reported paths to re-observe (re-observed even on a stat match).
    Paths(BTreeSet<WorkspacePath>),
    /// Watcher-reported directory roots whose current descendants must be
    /// discovered after the normal burst debounce.
    RecursivePaths(BTreeSet<WorkspacePath>),
    /// The watcher lost fidelity; fall back to a full stat walk immediately.
    FullScanRequired(FullScanReason),
    /// The ref subscription fired: pull and reconcile.
    RefChanged,
    /// The ref subscription delivered a signature-verified real head. The
    /// engine may consume this hint instead of repeating the same hosted query.
    RefObserved(RefObservation),
    /// The network came back; retry any pending work now, preempting backoff.
    ConnectivityRestored,
    /// Re-observe both authorities and acknowledge this exact request only after
    /// the resulting work has settled.
    SyncBarrier(SyncBarrierId),
    /// Stop the run loop.
    Shutdown,
}

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
/// are non-fatal and self-clearing (a lost CAS is never here — it is normal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    Nominal,
    FullScanRequired(FullScanReason),
    OfflineRetrying { attempt: u32 },
    IntegrityStalled,
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
/// off; an integrity fault stalls non-destructively until the next ref event; a
/// genuine bug propagates.
enum CycleError {
    Transport,
    Integrity,
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
    dirty_subtrees: Arc<BTreeSet<WorkspacePath>>,
    scan_required: bool,
    pull_needed: bool,
    pending_ref_hint: Option<RefObservation>,
    force_ref_read: bool,
    unattributed_pull_pending: bool,
    cycle_active: bool,

    debounce_deadline: Option<u64>,
    max_latency_deadline: Option<u64>,
    backoff_deadline: Option<u64>,
    backoff_attempt: u32,

    revision: u64,
    phase: EnginePhase,
    // The last observed CAS head. Named distinctly from the public
    // `EngineSnapshot.observed_ref` it feeds, so this internal init is not
    // mistaken for the old convergence engine's single `observed_ref` authority.
    head_ref: Option<RefObservation>,
    applied_manifest: Option<ManifestKey>,
    pending_intents: usize,
    pending_intent_paths: Arc<BTreeSet<WorkspacePath>>,
    last_success_at: Option<u64>,
    degradation: Degradation,
    last_sig: Option<StateSig>,
    pending_barriers: BTreeSet<SyncBarrierId>,
    completed_barriers: BTreeSet<SyncBarrierId>,
}

/// The transition signature: the fields whose change is a state transition (and
/// so bumps `revision`). Deliberately excludes wall-clock time and scheduling
/// deadlines, so an idle poll never advances the revision.
#[derive(Clone, PartialEq, Eq)]
struct StateSig {
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
            dirty_subtrees: Arc::new(BTreeSet::new()),
            scan_required: false,
            pull_needed: false,
            pending_ref_hint: None,
            force_ref_read: false,
            unattributed_pull_pending: false,
            cycle_active: false,
            debounce_deadline: None,
            max_latency_deadline: None,
            backoff_deadline: None,
            backoff_attempt: 0,
            revision: 0,
            phase: EnginePhase::Starting,
            head_ref: None,
            applied_manifest: None,
            pending_intents: 0,
            pending_intent_paths: Arc::new(BTreeSet::new()),
            last_success_at: None,
            degradation: Degradation::Nominal,
            last_sig: None,
            pending_barriers: BTreeSet::new(),
            completed_barriers: BTreeSet::new(),
        }
    }

    /// A read-only view of the current state for a status consumer.
    pub fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            revision: self.revision,
            phase: self.phase,
            observed_ref: self.head_ref.clone(),
            applied_manifest: self.applied_manifest.clone(),
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
        }
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
    pub fn take_completed_barriers(&mut self) -> BTreeSet<SyncBarrierId> {
        std::mem::take(&mut self.completed_barriers)
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
        ]
        .into_iter()
        .flatten()
        .min()
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
        // Recover in-flight intents FIRST so the seeding stat walk observes the
        // post-recovery tree, not a half-applied one.
        let deps = PullDeps {
            ctx: &self.ctx,
            objects: io.objects,
            refs: io.refs,
        };
        if let Err(error) = recover_intents(&mut self.store, &deps) {
            // A transport fault at startup is not fatal: back off and retry (the
            // next pull re-runs recovery). An integrity fault stalls
            // non-destructively. Only a genuine bug propagates.
            let now = io.clock.now_millis();
            match classify_pull_error(&error) {
                CycleError::Transport => {
                    self.pull_needed = true;
                    self.enter_backoff(now);
                }
                CycleError::Integrity => {
                    self.degradation = Degradation::IntegrityStalled;
                    self.phase = EnginePhase::Stalled;
                }
                CycleError::Fatal(_) => {
                    self.refresh_and_bump(io);
                    return Err(EngineError::Pull(error));
                }
            }
            self.refresh_and_bump(io);
            return Ok(());
        }
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
                Arc::make_mut(&mut self.dirty).extend(paths);
                self.arm_debounce(now);
                self.preempt_backoff();
            }
            EngineEvent::RecursivePaths(paths) => {
                Arc::make_mut(&mut self.dirty_subtrees).extend(paths);
                self.arm_debounce(now);
                self.preempt_backoff();
            }
            EngineEvent::FullScanRequired(reason) => {
                self.scan_required = true;
                self.degradation = Degradation::FullScanRequired(reason);
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
            EngineEvent::SyncBarrier(id) => {
                self.pending_ref_hint = None;
                self.force_ref_read = true;
                self.pending_barriers.insert(id);
                self.scan_required = true;
                self.pull_needed = true;
                self.unattributed_pull_pending = true;
                self.degradation = Degradation::FullScanRequired(FullScanReason::SyncBarrier);
                self.debounce_deadline = Some(now);
                self.preempt_backoff();
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
                    self.degradation = Degradation::Nominal;
                }
                self.phase = if self.idle() {
                    EnginePhase::Idle
                } else {
                    EnginePhase::Syncing
                };
                if self.phase == EnginePhase::Idle {
                    self.completed_barriers.append(&mut self.pending_barriers);
                }
            }
            Err(CycleError::Transport) => self.enter_backoff(now),
            Err(CycleError::Integrity) => {
                self.degradation = Degradation::IntegrityStalled;
                self.phase = EnginePhase::Stalled;
            }
            Err(CycleError::Fatal(error)) => {
                self.refresh_and_bump(io);
                return Err(error);
            }
        }
        self.refresh_and_bump(io);
        Ok(())
    }

    fn enter_backoff(&mut self, now: u64) {
        self.counters.record_retry();
        self.backoff_attempt = self.backoff_attempt.saturating_add(1);
        let delay = self.backoff_delay(self.backoff_attempt);
        self.backoff_deadline = Some(now + delay);
        self.degradation = Degradation::OfflineRetrying {
            attempt: self.backoff_attempt,
        };
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
        if self.scan_required {
            self.full_scan(io)?;
            self.scan_required = false;
            Arc::make_mut(&mut self.dirty_subtrees).clear();
        }
        if !self.dirty_subtrees.is_empty() {
            self.scan_dirty_subtrees(io)?;
        }
        if self.pull_needed {
            // Clear BEFORE pulling: do_pull re-arms pull_needed (and a debounce
            // deadline) when paths were deferred by an active Git lock, and a
            // post-call reset would clobber that internally scheduled retry,
            // leaving deferred paths unmaterialized until an external ref event.
            self.pull_needed = false;
            self.do_pull(io)?;
        }

        let mut attempts = 0u8;
        while !self.dirty.is_empty() && attempts < MAX_PUSH_ATTEMPTS {
            attempts += 1;
            // A second pass through this loop is a real retry after a lost CAS.
            if attempts > 1 {
                self.counters.record_retry();
            }
            let deps = PushDeps {
                ctx: &self.ctx,
                objects: io.objects,
                refs: io.refs,
            };
            // `EngineEvent::Paths` is native evidence that a write may have
            // happened even when a coarse filesystem reports the same stat
            // fingerprint. Verify queued file bytes before declaring them
            // unchanged; idle still performs no work because this runs only for
            // an actual dirty batch.
            let outcome = push::push_verifying_dirty_files(&mut self.store, &deps, &self.dirty)
                .map_err(push_cycle_error)?;
            match outcome {
                PushOutcome::Advanced {
                    manifest_key,
                    ref_version,
                    skipped,
                } => {
                    self.head_ref = Some(RefObservation {
                        version: ref_version,
                        manifest_key: manifest_key.clone(),
                    });
                    self.applied_manifest = Some(manifest_key);
                    // Retain exactly the paths the scan could not settle (actively
                    // being written); everything published leaves the dirty set.
                    self.retain_skipped(skipped);
                    break;
                }
                PushOutcome::NoChange { skipped } => {
                    // No delta this cycle. Keep only the churning paths, if any.
                    self.retain_skipped(skipped);
                    break;
                }
                PushOutcome::RefLost { current } => {
                    // The ancestor and the local edit are untouched. Pull the
                    // winner against that unchanged base, then retry once.
                    self.head_ref = current;
                    self.do_pull(io)?;
                }
            }
        }

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

    /// Replace the dirty set with exactly the paths a push could not settle. On a
    /// successful/no-change push, every other dirty path is done, so retaining the
    /// skipped set both clears the completed work and re-arms the churning paths.
    /// The `break` at each call site is deliberate: a skip is NOT a lost CAS, so
    /// it must not consume a `MAX_PUSH_ATTEMPTS` retry by re-running push against a
    /// still-changing file in the same cycle — the later rescheduled cycle handles
    /// it instead.
    fn retain_skipped(&mut self, skipped: BTreeSet<WorkspacePath>) {
        if !skipped.is_empty() {
            self.counters.record_push_skip(skipped.len() as u64);
        }
        self.dirty = Arc::new(skipped);
    }

    fn full_scan<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        _io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), CycleError> {
        let policy = crate::policy::UserPolicy::load(&self.ctx.workspace_root)
            .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
        let ancestor = self
            .store
            .all_files()
            .map_err(|error| CycleError::Fatal(EngineError::Store(error)))?;
        let walk = stat_walk(&self.ctx.workspace_root, &policy, &ancestor)
            .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
        self.counters.record_stat_walk(walk.scanned, walk.hashes);
        Arc::make_mut(&mut self.dirty).extend(walk.dirty);
        Ok(())
    }

    fn scan_dirty_subtrees<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        _io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), CycleError> {
        let scoped_roots = self
            .dirty_subtrees
            .iter()
            .map(|path| path.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let policy =
            crate::policy::UserPolicy::load_scoped(&self.ctx.workspace_root, &scoped_roots)
                .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
        let ancestor = self
            .store
            .all_files()
            .map_err(|error| CycleError::Fatal(EngineError::Store(error)))?;
        let walk = stat_walk_subtrees(
            &self.ctx.workspace_root,
            &policy,
            &ancestor,
            &self.dirty_subtrees,
        )
        .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
        self.counters.record_stat_walk(walk.scanned, walk.hashes);
        Arc::make_mut(&mut self.dirty).extend(walk.dirty);
        Arc::make_mut(&mut self.dirty_subtrees).clear();
        Ok(())
    }
}

fn push_cycle_error(error: PushError) -> CycleError {
    match error {
        PushError::Transport(_) => CycleError::Transport,
        other => CycleError::Fatal(EngineError::Push(other)),
    }
}

fn pull_cycle_error(error: PullError) -> CycleError {
    classify_pull_error(&error).map_fatal(|| EngineError::Pull(error))
}

fn classify_pull_error(error: &PullError) -> CycleError {
    match error {
        PullError::Transport(_) => CycleError::Transport,
        PullError::RefRegressed { .. } | PullError::RefForked { .. } => CycleError::Integrity,
        _ => CycleError::Fatal(EngineError::Internal),
    }
}

impl CycleError {
    /// Replace a placeholder `Fatal` with the caller's real error, so the error
    /// value is built once at the call site that owns it.
    fn map_fatal(self, build: impl FnOnce() -> EngineError) -> Self {
        match self {
            CycleError::Fatal(_) => CycleError::Fatal(build()),
            other => other,
        }
    }
}

// ---- errors -----------------------------------------------------------------

/// A driver-level failure that is neither a retryable transport fault nor a
/// non-destructive integrity stall — i.e. a genuine bug the daemon must surface.
#[derive(Debug)]
pub enum EngineError {
    Io(std::io::Error),
    Store(ManifestStoreError),
    Push(PushError),
    Pull(PullError),
    Internal,
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "engine io failed: {error}"),
            Self::Store(error) => write!(formatter, "engine store failed: {error}"),
            Self::Push(error) => write!(formatter, "engine push failed: {error}"),
            Self::Pull(error) => write!(formatter, "engine pull failed: {error}"),
            Self::Internal => formatter.write_str("engine internal invariant violated"),
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Push(error) => Some(error),
            Self::Pull(error) => Some(error),
            Self::Internal => None,
        }
    }
}
