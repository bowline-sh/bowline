//! Daemon-side driver for the manifest-sync engine (Plan 111 Step 1b).
//!
//! The engine ([`ManifestEngine`]) is a synchronous state machine driven by an
//! event channel. This module owns the long-lived thread that runs it: it folds
//! [`EngineEvent`]s from the watcher bridge and the ref-change subscription into
//! the engine, runs due work on the debounce/backoff schedule, and publishes an
//! [`EngineSnapshot`] after every transition so the status projection
//! (Plan 111 Step 1c) can read a live view without touching the engine thread.
//!
//! One driver owns one workspace. Production builds the real
//! [`ManifestTransport`] and a [`RefChangeSubscription`] inside the thread from a
//! shared hosted client; tests supply a fake transport through the same
//! [`ManifestDriver::spawn`] seam.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bowline_control_plane::{HostedControlPlaneClient, SignedUrlHttpClient};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::sync::manifest_engine::{
    Clock, Degradation, EngineContext, EngineCounters, EngineEvent, EngineIo, EnginePhase,
    EngineSnapshot, FullScanReason, ManifestEngine, ManifestStore, RemoteObjects, RemoteRef,
    SyncBarrierId, SystemClock,
};

use crate::manifest_transport::{
    ManifestTransport, ReconnectDelay, RefChangeSubscription, RefObserverHealth,
    RefObserverHealthHandle, RefObserverReadiness, SignerTrustRefresh,
};

/// The engine's own database file, kept at the daemon state root (never inside
/// the synced workspace — the engine never writes ordinary workspace files
/// outside user space).
pub const MANIFEST_ENGINE_DB_FILE: &str = "manifest_engine.sqlite3";

/// The live engine's inbox, registered while a driver thread is running.
/// A status reader holds an [`EngineSnapshotHandle`] whether or not an engine
/// exists, so every command it can send goes through this slot and reports
/// `Unavailable` when the slot is empty.
#[derive(Debug)]
struct EngineEndpoint {
    generation: u64,
    events: Sender<EngineEvent>,
    pending: Arc<Mutex<BTreeMap<SyncBarrierId, Sender<EngineSnapshot>>>>,
    observer: ObserverEndpoint,
}

#[derive(Debug)]
enum ObserverEndpoint {
    NotRequired,
    Starting,
    LiveHealth(RefObserverHealthHandle),
}

#[derive(Clone)]
enum ObserverBarrierGuard {
    NotRequired,
    LiveHealth(RefObserverHealthHandle),
}

impl ObserverBarrierGuard {
    fn require_live(&self) -> Result<(), SyncBarrierError> {
        let Self::LiveHealth(health) = self else {
            return Ok(());
        };
        match health.readiness() {
            RefObserverReadiness::Live => Ok(()),
            RefObserverReadiness::Retrying => Err(SyncBarrierError::Unavailable {
                reason: "remote manifest observer has not delivered its initial value",
            }),
            RefObserverReadiness::UntrustedSigner | RefObserverReadiness::Unauthenticated => {
                Err(SyncBarrierError::ObserverUnavailable)
            }
        }
    }
}

#[derive(Debug)]
struct EngineShared {
    snapshot: Mutex<EngineSnapshot>,
    endpoint: Mutex<Option<EngineEndpoint>>,
    next_barrier_id: AtomicU64,
    next_generation: AtomicU64,
}

#[derive(Clone)]
pub struct EngineSnapshotSink(Arc<EngineShared>);

impl EngineSnapshotSink {
    /// Publish the latest snapshot for status readers. Public so the daemon can
    /// publish a host-status snapshot (e.g. `limited` while the driver is waiting
    /// to rebuild) into the same slot the driver will later take over.
    pub fn publish(&self, snapshot: EngineSnapshot) {
        if let Ok(mut current) = self.0.snapshot.lock() {
            *current = snapshot;
        }
    }

    fn complete_barriers(
        &self,
        completed: impl IntoIterator<Item = SyncBarrierId>,
        snapshot: &EngineSnapshot,
    ) {
        let pending = self.0.endpoint.lock().ok().and_then(|endpoint| {
            endpoint
                .as_ref()
                .map(|endpoint| Arc::clone(&endpoint.pending))
        });
        let Some(pending) = pending else {
            return;
        };
        let Ok(mut pending) = pending.lock() else {
            return;
        };
        for id in completed {
            if let Some(waiter) = pending.remove(&id) {
                let _waiter_gone = waiter.send(snapshot.clone());
            }
        }
    }

    /// A read handle onto the same slot this sink publishes into.
    pub fn handle(&self) -> EngineSnapshotHandle {
        EngineSnapshotHandle(Arc::clone(&self.0))
    }

    fn register_engine_endpoint(
        &self,
        events: Sender<EngineEvent>,
        pending: Arc<Mutex<BTreeMap<SyncBarrierId, Sender<EngineSnapshot>>>>,
        observer_required: bool,
    ) -> u64 {
        let generation = self.0.next_generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut endpoint) = self.0.endpoint.lock() {
            *endpoint = Some(EngineEndpoint {
                generation,
                events,
                pending,
                observer: if observer_required {
                    ObserverEndpoint::Starting
                } else {
                    ObserverEndpoint::NotRequired
                },
            });
        }
        generation
    }

    fn attach_ref_observer_health(&self, generation: u64, health: RefObserverHealthHandle) {
        if let Ok(mut endpoint) = self.0.endpoint.lock()
            && let Some(endpoint) = endpoint.as_mut()
            && endpoint.generation == generation
        {
            endpoint.observer = ObserverEndpoint::LiveHealth(health);
        }
    }

    fn unregister_engine_endpoint(&self, generation: u64) {
        if let Ok(mut endpoint) = self.0.endpoint.lock()
            && endpoint
                .as_ref()
                .is_some_and(|endpoint| endpoint.generation == generation)
        {
            *endpoint = None;
        }
    }
}

/// A fresh, externally-owned snapshot slot seeded with the `Starting` snapshot.
/// The daemon owns one of these per engine-managed workspace so a driver that is
/// built late publishes into the *same* slot the status projection already reads —
/// the projection never has to be rebuilt when the driver comes up.
pub fn shared_engine_snapshot() -> (EngineSnapshotSink, EngineSnapshotHandle) {
    let shared = Arc::new(EngineShared {
        snapshot: Mutex::new(starting_snapshot()),
        endpoint: Mutex::new(None),
        next_barrier_id: AtomicU64::new(1),
        next_generation: AtomicU64::new(1),
    });
    (
        EngineSnapshotSink(Arc::clone(&shared)),
        EngineSnapshotHandle(shared),
    )
}

/// A cloneable read handle onto the engine's latest published snapshot.
#[derive(Clone, Debug)]
pub struct EngineSnapshotHandle(Arc<EngineShared>);

impl EngineSnapshotHandle {
    /// The most recently published snapshot, or a synthesized `Starting`
    /// snapshot if the lock is momentarily poisoned (never blocks status).
    pub fn current(&self) -> EngineSnapshot {
        self.0
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| starting_snapshot())
    }

    /// Request an exact convergence boundary from the active engine. The engine
    /// performs an on-demand disk scan and hosted-ref read, then wakes this
    /// waiter only after that specific request has settled.
    pub fn request_sync_barrier(&self) -> Result<SyncBarrierWaiter, SyncBarrierError> {
        let (id, events, pending, observer) = {
            let endpoint = self
                .0
                .endpoint
                .lock()
                .map_err(|_| SyncBarrierError::Unavailable {
                    reason: "sync barrier state is unavailable",
                })?;
            let endpoint = endpoint.as_ref().ok_or(SyncBarrierError::Unavailable {
                reason: "manifest sync engine is unavailable",
            })?;
            let observer = match &endpoint.observer {
                ObserverEndpoint::NotRequired => ObserverBarrierGuard::NotRequired,
                ObserverEndpoint::Starting => {
                    return Err(SyncBarrierError::Unavailable {
                        reason: "remote manifest observer is starting",
                    });
                }
                ObserverEndpoint::LiveHealth(health) => {
                    ObserverBarrierGuard::LiveHealth(health.clone())
                }
            };
            observer.require_live()?;
            (
                SyncBarrierId(self.0.next_barrier_id.fetch_add(1, Ordering::Relaxed)),
                endpoint.events.clone(),
                Arc::clone(&endpoint.pending),
                observer,
            )
        };
        let (completion, receiver) = mpsc::channel();
        pending
            .lock()
            .map_err(|_| SyncBarrierError::Unavailable {
                reason: "sync barrier state is unavailable",
            })?
            .insert(id, completion);
        if events.send(EngineEvent::SyncBarrier(id)).is_err() {
            if let Ok(mut pending) = pending.lock() {
                pending.remove(&id);
            }
            return Err(SyncBarrierError::EngineStopped);
        }
        Ok(SyncBarrierWaiter {
            id,
            receiver,
            pending,
            observer,
        })
    }

    /// Authorise the removal batch the engine is currently refusing.
    ///
    /// Fire-and-forget by design: what gets authorised is whatever the engine is
    /// refusing when it folds the event, so there is nothing for the caller to
    /// pass and nothing it could usefully wait for. The caller reads the
    /// resulting state from the snapshot it already polls.
    pub fn confirm_mass_deletion(&self) -> Result<(), EngineCommandError> {
        let events = {
            let endpoint = self
                .0
                .endpoint
                .lock()
                .map_err(|_| EngineCommandError::Unavailable {
                    reason: "manifest sync engine state is unavailable",
                })?;
            endpoint
                .as_ref()
                .ok_or(EngineCommandError::Unavailable {
                    reason: "manifest sync engine is unavailable",
                })?
                .events
                .clone()
        };
        events
            .send(EngineEvent::ConfirmMassDeletion)
            .map_err(|_| EngineCommandError::EngineStopped)
    }
}

/// Why a command to the live engine could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommandError {
    /// No engine is attached to this snapshot slot.
    Unavailable { reason: &'static str },
    /// The engine thread stopped before the command reached it.
    EngineStopped,
}

impl EngineCommandError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable { reason } => reason,
            Self::EngineStopped => "manifest sync engine stopped before the command was applied",
        }
    }
}

impl std::fmt::Display for EngineCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for EngineCommandError {}

/// Why an exact sync barrier produced no snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncBarrierError {
    /// The engine slot exists but cannot answer right now — nothing has attached
    /// to it yet, or its barrier state is momentarily unusable. Either way a
    /// caller holding a deadline should ask again.
    Unavailable { reason: &'static str },
    /// The remote observer is present but cannot become live without an account
    /// or device-trust change. Retrying the same barrier would hide the action
    /// the user must take, so this remains distinct from startup unavailability.
    ObserverUnavailable,
    /// This daemon serves no sync workspace at all. Waiting cannot change that,
    /// so it must never be reported as a daemon that is still coming up.
    WorkspaceNotServed,
    /// The caller's cancellation predicate fired: the client disconnected, or
    /// the request's deadline passed.
    Cancelled,
    /// The barrier did not converge before the caller's timeout.
    TimedOut,
    /// The engine stopped before completing the barrier.
    EngineStopped,
}

impl SyncBarrierError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable { reason } => reason,
            Self::ObserverUnavailable => {
                "remote manifest observer requires authentication or signer trust"
            }
            Self::WorkspaceNotServed => "daemon is not serving a sync workspace",
            Self::Cancelled => "the sync barrier request was cancelled",
            Self::TimedOut => "sync barrier did not converge before the deadline",
            Self::EngineStopped => "manifest sync engine stopped before the barrier completed",
        }
    }
}

impl std::fmt::Display for SyncBarrierError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for SyncBarrierError {}

/// How often an interruptible barrier wait re-checks its cancellation
/// predicate. Small enough that a disconnected or expired request frees its
/// worker thread promptly, large enough to cost nothing while idle.
const SYNC_BARRIER_POLL: Duration = Duration::from_millis(200);

/// Reactive completion handle for one exact sync barrier.
pub struct SyncBarrierWaiter {
    id: SyncBarrierId,
    receiver: Receiver<EngineSnapshot>,
    pending: Arc<Mutex<BTreeMap<SyncBarrierId, Sender<EngineSnapshot>>>>,
    observer: ObserverBarrierGuard,
}

impl SyncBarrierWaiter {
    /// Wait for the barrier, re-checking `cancelled` on a short poll interval.
    ///
    /// A barrier occupies a bounded RPC worker for its whole duration, so the
    /// wait must never be an uninterruptible park: without this the executor's
    /// cancellation machinery writes a `DeadlineExceeded` response to the client
    /// while the worker stays blocked, and enough concurrent barriers starve the
    /// lane that serves `daemon.info`.
    pub fn wait(
        self,
        timeout: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<EngineSnapshot, SyncBarrierError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.observer.require_live()?;
            if cancelled() {
                return Err(SyncBarrierError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(SyncBarrierError::TimedOut);
            }
            match self.receiver.recv_timeout(remaining.min(SYNC_BARRIER_POLL)) {
                Ok(snapshot) => {
                    self.observer.require_live()?;
                    return Ok(snapshot);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(SyncBarrierError::EngineStopped);
                }
            }
        }
    }
}

impl Drop for SyncBarrierWaiter {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

/// Disjoint high revision band for the daemon's synthetic host-status snapshot.
/// The live engine's revision grows from 0 and stays small, so a value this large
/// never aliases a real engine revision in the status projection's equality-based
/// change detection — the projection republishes the moment a driver takes over.
///
/// Caveat: host-status revisions are NOT monotonic with engine revisions across a
/// pending->active (or active->pending) transition — the number jumps between the
/// `1 << 60` band and the small live-engine band in both directions. Consumers
/// must treat a host-status revision as an opaque change token (equality only),
/// never compare it ordinally against an engine revision.
const HOST_STATUS_REVISION: u64 = 1 << 60;

/// The `limited` host-status snapshot the daemon publishes while a driver is
/// waiting to rebuild (lazy-rebuild path, Plan 111 Step 1b). `Stopped` + `Nominal`
/// maps to `limited` in the v8 adapter; the disjoint revision keeps the status
/// projection from aliasing it with a live engine revision.
pub fn host_status_snapshot() -> EngineSnapshot {
    EngineSnapshot {
        revision: HOST_STATUS_REVISION,
        phase: EnginePhase::Stopped,
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

/// The initial snapshot before the engine thread has run its startup cycle.
fn starting_snapshot() -> EngineSnapshot {
    EngineSnapshot {
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
        unattributed_pull_pending: true,
        cycle_active: false,
        last_success_at: None,
        degradation: Degradation::Nominal,
        unsyncable: Arc::new(BTreeMap::new()),
        refused_removals: Arc::new(BTreeSet::new()),
    }
}

/// The long-lived driver for one workspace's engine.
pub struct ManifestDriver {
    events: Sender<EngineEvent>,
    snapshot: EngineSnapshotHandle,
    endpoint_sink: EngineSnapshotSink,
    endpoint_generation: u64,
    barrier_pending: Arc<Mutex<BTreeMap<SyncBarrierId, Sender<EngineSnapshot>>>>,
    // The engine's shared cost meters (Plan 111 Step 5). The same `Arc` the
    // engine thread writes; the daemon metrics RPC reads it lock-free.
    counters: Arc<EngineCounters>,
    thread: Option<JoinHandle<()>>,
    // The ref-change subscription's worker is stopped when this driver drops.
    ref_subscription: Option<RefChangeSubscription>,
    ref_observer_health: Option<RefObserverHealthHandle>,
}

impl ManifestDriver {
    /// Spawn a driver whose thread body runs `run`. `run` receives the engine's
    /// inbox and the snapshot sink; production and tests differ only in the
    /// transport `run` constructs. Use [`run_engine_loop`] to drive the engine.
    pub fn spawn<F>(run: F) -> io::Result<Self>
    where
        F: FnOnce(Receiver<EngineEvent>, EngineSnapshotSink) + Send + 'static,
    {
        let (sink, handle) = shared_engine_snapshot();
        Self::spawn_with_sink(sink, handle, run)
    }

    /// Like [`spawn`], but publishing into a caller-owned snapshot slot. The daemon
    /// uses this so a driver built after startup publishes into the slot the status
    /// projection already reads (see [`shared_engine_snapshot`]).
    pub fn spawn_with_sink<F>(
        sink: EngineSnapshotSink,
        handle: EngineSnapshotHandle,
        run: F,
    ) -> io::Result<Self>
    where
        F: FnOnce(Receiver<EngineEvent>, EngineSnapshotSink) + Send + 'static,
    {
        Self::spawn_with_sink_observer_requirement(sink, handle, false, run)
    }

    fn spawn_with_sink_observer_requirement<F>(
        sink: EngineSnapshotSink,
        handle: EngineSnapshotHandle,
        observer_required: bool,
        run: F,
    ) -> io::Result<Self>
    where
        F: FnOnce(Receiver<EngineEvent>, EngineSnapshotSink) + Send + 'static,
    {
        let (events, inbox) = mpsc::channel();
        let barrier_pending = Arc::new(Mutex::new(BTreeMap::new()));
        let endpoint_generation = sink.register_engine_endpoint(
            events.clone(),
            Arc::clone(&barrier_pending),
            observer_required,
        );
        let thread_sink = sink.clone();
        let thread = match thread::Builder::new()
            .name("bowline-manifest-engine".to_string())
            .spawn(move || run(inbox, thread_sink))
        {
            Ok(thread) => thread,
            Err(error) => {
                sink.unregister_engine_endpoint(endpoint_generation);
                return Err(error);
            }
        };
        Ok(Self {
            events,
            snapshot: handle,
            endpoint_sink: sink,
            endpoint_generation,
            barrier_pending,
            // Replaced with the engine's own counters by the production path; a
            // generic `run` (tests) keeps this fresh set, harmlessly unread.
            counters: EngineCounters::shared(),
            thread: Some(thread),
            ref_subscription: None,
            ref_observer_health: None,
        })
    }

    /// Spawn the production driver: open the engine store, build the engine over
    /// the resolved crypto/context, and run the real hosted transport. Attaches
    /// a ref-change subscription that wakes the engine on remote head changes.
    pub fn spawn_production(config: ManifestDriverConfig) -> io::Result<Self> {
        let (sink, handle) = shared_engine_snapshot();
        Self::spawn_production_with_sink(config, sink, handle)
    }

    /// Like [`spawn_production`], but publishing into a caller-owned snapshot slot
    /// so the daemon's status projection sees this driver without being rebuilt
    /// (used by the lazy-rebuild path — Plan 111 Step 1b).
    pub fn spawn_production_with_sink(
        config: ManifestDriverConfig,
        sink: EngineSnapshotSink,
        handle: EngineSnapshotHandle,
    ) -> io::Result<Self> {
        let store = ManifestStore::open(&config.store_path)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let engine = ManifestEngine::new(store, config.context);
        // Capture the engine's counters before it moves into the thread body, so
        // the daemon metrics RPC can read the live tally.
        let counters = engine.counters();
        let transport_client = Arc::clone(&config.client);
        let workspace_id = config.workspace_id.clone();
        let device_id = config.device_id.clone();
        let http = config.http;
        let mut driver =
            Self::spawn_with_sink_observer_requirement(sink, handle, true, move |inbox, sink| {
                let transport = ManifestTransport::with_http_client(
                    &*transport_client,
                    workspace_id,
                    device_id,
                    http,
                );
                let clock = SystemClock::default();
                run_engine_loop(engine, &transport, &transport, &clock, &inbox, &sink);
            })?;
        let subscription = RefChangeSubscription::spawn(
            config.client,
            config.workspace_id.as_str().to_string(),
            driver.events.clone(),
            config.reconnect_delay,
            config.trust_refresh,
        );
        let observer_health = subscription.health_handle();
        driver
            .endpoint_sink
            .attach_ref_observer_health(driver.endpoint_generation, observer_health.clone());
        driver.ref_observer_health = Some(observer_health);
        driver.ref_subscription = Some(subscription);
        driver.counters = counters;
        Ok(driver)
    }

    /// A cloneable handle to the engine's latest snapshot for status.
    pub fn snapshot_handle(&self) -> EngineSnapshotHandle {
        self.snapshot.clone()
    }

    /// A shared handle to the engine's cost meters for the daemon metrics RPC.
    pub fn counters(&self) -> Arc<EngineCounters> {
        Arc::clone(&self.counters)
    }

    /// The current engine snapshot.
    pub fn snapshot(&self) -> EngineSnapshot {
        self.snapshot.current()
    }

    /// A sender for feeding watcher-derived events into the engine.
    pub fn event_sender(&self) -> Sender<EngineEvent> {
        self.events.clone()
    }

    /// Current remote observer health. Production drivers return `Some`; test
    /// drivers created without a hosted subscription return `None`.
    pub fn ref_observer_health(&self) -> Option<RefObserverHealth> {
        self.ref_observer_health
            .as_ref()
            .map(RefObserverHealthHandle::current)
    }

    /// What the remote observer currently contributes to status. `Live` requires
    /// Convex to have delivered the initial reactive value over a worker that is
    /// still running; a credential the control plane refuses is reported apart
    /// from ordinary retrying because no reconnect can clear it.
    pub fn ref_observer_readiness(&self) -> RefObserverReadiness {
        let Some(readiness) = self
            .ref_observer_health
            .as_ref()
            .map(RefObserverHealthHandle::readiness)
        else {
            return RefObserverReadiness::Retrying;
        };
        let worker_running = self
            .ref_subscription
            .as_ref()
            .is_some_and(|subscription| !subscription.is_finished());
        if readiness == RefObserverReadiness::Live && !worker_running {
            return RefObserverReadiness::Retrying;
        }
        readiness
    }

    /// Whether either indispensable production worker has exited. The daemon
    /// rebuilds the whole driver so a dead observer can never leave a healthy
    /// engine thread paired with permanently stale remote state.
    pub fn has_finished_required_worker(&self) -> bool {
        self.is_thread_finished()
            || self
                .ref_subscription
                .as_ref()
                .is_some_and(RefChangeSubscription::is_finished)
    }

    /// Whether the engine thread has exited (a panic or an unexpected loop
    /// return). An `Active` host observing `true` must rebuild — a dead thread
    /// still holds a live event sender and a stale snapshot, so nothing else
    /// signals the failure.
    pub fn is_thread_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
    }

    /// Send one event to the engine, ignoring a dropped receiver (the thread has
    /// already stopped, which the caller observes at shutdown).
    pub fn send(&self, event: EngineEvent) {
        let _engine_stopped = self.events.send(event);
    }
}

impl Drop for ManifestDriver {
    fn drop(&mut self) {
        // Stop the ref subscription first so it stops feeding a dead channel,
        // then signal the engine and join its thread.
        self.ref_subscription = None;
        self.endpoint_sink
            .unregister_engine_endpoint(self.endpoint_generation);
        if let Ok(mut pending) = self.barrier_pending.lock() {
            pending.clear();
        }
        let _engine_stopped = self.events.send(EngineEvent::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _joined = thread.join();
        }
    }
}

/// Everything [`ManifestDriver::spawn_production`] needs to build and run the
/// real engine for one workspace.
pub struct ManifestDriverConfig {
    pub store_path: std::path::PathBuf,
    pub context: EngineContext,
    pub client: Arc<HostedControlPlaneClient>,
    /// The workspace's shared signed-URL HTTP client (one per workspace).
    pub http: SignedUrlHttpClient,
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub reconnect_delay: ReconnectDelay,
    /// How the observer learns a device whose signed head it cannot verify —
    /// the workspace's shared device trust, refreshed between stream attempts.
    pub trust_refresh: SignerTrustRefresh,
}

#[derive(Default)]
struct WatcherOverflowPriority {
    applied_generation: u64,
    queued_fences: u64,
}

const WATCHER_OVERFLOW_HOLD_POLL: Duration = Duration::from_millis(5);

impl WatcherOverflowPriority {
    fn refresh<C: Clock>(
        &mut self,
        engine: &mut ManifestEngine,
        counters: &EngineCounters,
        clock: &C,
    ) {
        let latest = counters.watcher_overflow_recovery_generation();
        let new_fences = latest.saturating_sub(self.applied_generation);
        if new_fences == 0 {
            return;
        }
        self.applied_generation = latest;
        self.queued_fences = self.queued_fences.saturating_add(new_fences);
        engine.on_event(
            EngineEvent::FullScanRequired(FullScanReason::WatcherOverflow),
            clock,
        );
    }

    fn subsumes(&mut self, event: &EngineEvent) -> bool {
        if self.queued_fences == 0 {
            return false;
        }
        match event {
            // These watcher events precede the queued fence that asserted this
            // generation. The priority full scan re-observes all of them.
            EngineEvent::Paths(_) | EngineEvent::RecursivePaths(_) => true,
            EngineEvent::FullScanRequired(FullScanReason::WatcherOverflow) => {
                self.queued_fences -= 1;
                true
            }
            EngineEvent::FullScanRequired(_)
            | EngineEvent::RefChanged
            | EngineEvent::RefObserved(_)
            | EngineEvent::ConnectivityRestored
            | EngineEvent::SyncBarrier(_)
            | EngineEvent::ConfirmMassDeletion
            | EngineEvent::Shutdown => false,
        }
    }
}

/// Run the engine loop, publishing a snapshot after startup and after every
/// due-work cycle. This is the daemon's snapshot-observing composition of the
/// engine's finer `start`/`on_event`/`run_due_work` API; it differs from
/// [`ManifestEngine::run`] only by publishing to `sink`, which the status
/// projection reads. A fatal engine error stops the loop after publishing the
/// terminal snapshot; transport and integrity faults are handled inside the
/// engine and never reach here.
pub fn run_engine_loop<O, R, C>(
    mut engine: ManifestEngine,
    objects: &O,
    refs: &R,
    clock: &C,
    inbox: &Receiver<EngineEvent>,
    sink: &EngineSnapshotSink,
) where
    O: RemoteObjects,
    R: RemoteRef,
    C: Clock,
{
    let io = EngineIo {
        objects,
        refs,
        clock,
    };
    if let Err(error) = engine.start(&io) {
        eprintln!("bowline-daemon manifest engine startup failed: {error}");
        sink.publish(engine.snapshot());
        return;
    }
    let snapshot = engine.snapshot();
    sink.publish(snapshot.clone());
    sink.complete_barriers(engine.take_completed_barriers(), &snapshot);
    let counters = engine.counters();
    let mut watcher_overflow_priority = WatcherOverflowPriority::default();
    loop {
        let received = if counters.watcher_overflow_recovery_pending() {
            inbox.recv_timeout(WATCHER_OVERFLOW_HOLD_POLL)
        } else {
            match engine.next_timeout(clock.now_millis()) {
                Some(timeout) => inbox.recv_timeout(timeout),
                None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
            }
        };
        match received {
            Ok(EngineEvent::Shutdown) => {
                engine.on_event(EngineEvent::Shutdown, clock);
                sink.publish(engine.snapshot());
                return;
            }
            Ok(event) => {
                watcher_overflow_priority.refresh(&mut engine, &counters, clock);
                if !watcher_overflow_priority.subsumes(&event) {
                    engine.on_event(event, clock);
                }
                sink.publish(engine.snapshot());
            }
            Err(RecvTimeoutError::Timeout) => {
                watcher_overflow_priority.refresh(&mut engine, &counters, clock);
            }
            Err(RecvTimeoutError::Disconnected) => {
                engine.on_event(EngineEvent::Shutdown, clock);
                sink.publish(engine.snapshot());
                return;
            }
        }
        if counters.watcher_overflow_recovery_pending() {
            continue;
        }
        if engine.announce_due_work(clock) {
            sink.publish(engine.snapshot());
        }
        if let Err(error) = engine.run_due_work(&io) {
            eprintln!("bowline-daemon manifest engine cycle failed: {error}");
            sink.publish(engine.snapshot());
            return;
        }
        let snapshot = engine.snapshot();
        sink.publish(snapshot.clone());
        sink.complete_barriers(engine.take_completed_barriers(), &snapshot);
    }
}

#[cfg(test)]
#[path = "manifest_driver/tests.rs"]
mod tests;
