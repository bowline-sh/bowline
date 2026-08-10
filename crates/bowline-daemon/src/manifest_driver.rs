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

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender as EventSender};

use bowline_control_plane::{HostedControlPlaneClient, SignedUrlHttpClient};
use bowline_local::sync::manifest_engine::{
    AuthoritativeScanPlan, AuthoritativeScanResult, Clock, EngineContext,
    EngineConvergenceBarrierId, EngineConvergenceReceipt, EngineCounters, EngineEndpointGeneration,
    EngineEvent, EngineIo, EngineSnapshot, ManifestEngine, ManifestStore, RemoteObjects, RemoteRef,
    SystemClock,
};

use crate::manifest_transport::{
    ManifestTransport, ReconnectDelay, RefChangeSubscription, RefObserverEndpointGeneration,
    RefObserverHealth, RefObserverReadiness, RefObserverSnapshotHandle, SignerTrustRefresh,
};

#[path = "manifest_driver/barrier_wait.rs"]
mod barrier_wait;
#[path = "manifest_driver/control_registry.rs"]
mod control_registry;
#[path = "manifest_driver/exact_observer.rs"]
mod exact_observer;
#[path = "manifest_driver/identity.rs"]
mod identity;
#[path = "manifest_driver/snapshot_status.rs"]
mod snapshot_status;
#[path = "manifest_driver/watcher_ingress.rs"]
mod watcher_ingress;
pub use barrier_wait::{
    EngineConvergenceBarrierError, EngineConvergenceBarrierWaiter,
    EngineObserverConvergenceReceipt, ExactBarrierTimestamp, SyncBarrierError, SyncBarrierWaiter,
};
pub use control_registry::{
    CoverageScanError, CoverageScanFailure, CoverageScanLease, CoverageScanWaiter, WalkStartHook,
};
use control_registry::{RecoveryControlAction, WorkspaceControlRegistry};
use exact_observer::{ObserverBarrierGuard, ObserverEndpoint};
use identity::validate_engine_context_identity;
use snapshot_status::starting_snapshot;
pub use snapshot_status::{EngineCommandError, host_status_snapshot};
#[cfg(test)]
pub(super) const HOST_STATUS_REVISION: u64 = snapshot_status::HOST_STATUS_REVISION;
#[doc(hidden)]
pub use watcher_ingress::{WatcherIngressAccumulator, WatcherIngressSnapshot};
pub use watcher_ingress::{WatcherIngressHandle, WatcherIngressLoss, WatcherIngressObservation};

/// The engine's own database file, kept at the daemon state root (never inside
/// the synced workspace — the engine never writes ordinary workspace files
/// outside user space).
pub const MANIFEST_ENGINE_DB_FILE: &str = "manifest_engine.sqlite3";

/// Bound cross-thread wakeups so native watcher pressure remains in the
/// callback lane, where saturation has an authoritative full-scan recovery
/// path. The engine already coalesces one dirty set; an unbounded second queue
/// only delays newer edits behind stale observations.
const MANIFEST_ENGINE_INBOX_CAPACITY: usize = 1;

/// The live engine's inbox, registered while a driver thread is running.
/// A status reader holds an [`EngineSnapshotHandle`] whether or not an engine
/// exists, so every command it can send goes through this slot and reports
/// `Unavailable` when the slot is empty.
#[derive(Debug)]
struct EngineEndpoint {
    generation: EngineEndpointGeneration,
    events: EventSender<EngineEvent>,
    controls: Arc<WorkspaceControlRegistry>,
    watcher_ingress: Arc<watcher_ingress::WatcherIngressAccumulator>,
    pending: Arc<Mutex<BTreeMap<EngineConvergenceBarrierId, Sender<EngineBarrierCompletion>>>>,
    observer: ObserverEndpoint,
}

#[derive(Debug, Clone)]
struct EngineBarrierCompletion {
    receipt: EngineConvergenceReceipt,
}

#[derive(Debug)]
struct EngineShared {
    snapshot: Mutex<EngineSnapshot>,
    endpoint: Mutex<Option<EngineEndpoint>>,
    next_barrier_id: AtomicU64,
    next_generation: AtomicU64,
}

#[derive(Clone)]
pub struct EngineSnapshotSink {
    shared: Arc<EngineShared>,
    generation: Option<EngineEndpointGeneration>,
}

impl EngineSnapshotSink {
    /// Publish the latest snapshot for status readers. Public so the daemon can
    /// publish a host-status snapshot (e.g. `limited` while the driver is waiting
    /// to rebuild) into the same slot the driver will later take over.
    pub fn publish(&self, snapshot: EngineSnapshot) {
        let Ok(endpoint) = self.shared.endpoint.lock() else {
            return;
        };
        if !self.owns_endpoint(&endpoint) {
            return;
        }
        if let Ok(mut current) = self.shared.snapshot.lock() {
            *current = snapshot;
        }
    }

    fn owns_endpoint(&self, endpoint: &Option<EngineEndpoint>) -> bool {
        let current = endpoint.as_ref().map(|endpoint| endpoint.generation);
        match self.generation {
            Some(expected) => current == Some(expected),
            None => current.is_none(),
        }
    }

    /// Atomically replace any live engine endpoint with a non-ready host state.
    ///
    /// Demotion cannot be expressed as `publish` followed by dropping a driver:
    /// that ordering either lets a stale endpoint overwrite the host snapshot or
    /// leaves its last Ready state visible. Taking endpoint and snapshot locks in
    /// the established order makes the takeover one state transition.
    pub fn take_over_with_host_status(&self, snapshot: EngineSnapshot) {
        if self.generation.is_some() {
            return;
        }
        let Ok(mut endpoint) = self.shared.endpoint.lock() else {
            return;
        };
        let Ok(mut current) = self.shared.snapshot.lock() else {
            return;
        };
        let previous = endpoint.take();
        *current = snapshot;
        drop(current);
        if let Some(previous) = previous
            && let Ok(mut pending) = previous.pending.lock()
        {
            pending.clear();
            previous.controls.disconnect();
            previous.watcher_ingress.disconnect();
        }
    }

    fn complete_barriers(&self, completed: impl IntoIterator<Item = EngineConvergenceReceipt>) {
        let Some(expected_generation) = self.generation else {
            return;
        };
        let Ok(endpoint) = self.shared.endpoint.lock() else {
            return;
        };
        let Some(endpoint) = endpoint
            .as_ref()
            .filter(|endpoint| endpoint.generation == expected_generation)
        else {
            return;
        };
        let pending = Arc::clone(&endpoint.pending);
        let Ok(mut pending) = pending.lock() else {
            return;
        };
        for receipt in completed {
            if receipt.endpoint_generation() != expected_generation {
                continue;
            }
            let id = receipt.barrier_id();
            if let Some(waiter) = pending.remove(&id) {
                let _waiter_gone = waiter.send(EngineBarrierCompletion { receipt });
            }
        }
    }

    /// A read handle onto the same slot this sink publishes into.
    pub fn handle(&self) -> EngineSnapshotHandle {
        EngineSnapshotHandle(Arc::clone(&self.shared))
    }

    fn register_engine_endpoint(
        &self,
        generation: EngineEndpointGeneration,
        events: EventSender<EngineEvent>,
        controls: Arc<WorkspaceControlRegistry>,
        watcher_ingress: Arc<watcher_ingress::WatcherIngressAccumulator>,
        pending: Arc<Mutex<BTreeMap<EngineConvergenceBarrierId, Sender<EngineBarrierCompletion>>>>,
        observer_required: bool,
    ) -> io::Result<()> {
        let mut endpoint = self
            .shared
            .endpoint
            .lock()
            .map_err(|_| io::Error::other("engine endpoint state is unavailable"))?;
        let mut snapshot = self
            .shared
            .snapshot
            .lock()
            .map_err(|_| io::Error::other("engine snapshot state is unavailable"))?;
        let previous = endpoint.replace(EngineEndpoint {
            generation,
            events,
            controls,
            watcher_ingress,
            pending,
            observer: if observer_required {
                ObserverEndpoint::Starting
            } else {
                ObserverEndpoint::NotRequired
            },
        });
        *snapshot = starting_snapshot();
        drop(snapshot);
        if let Some(previous) = previous
            && let Ok(mut pending) = previous.pending.lock()
        {
            pending.clear();
        }
        Ok(())
    }

    fn allocate_engine_endpoint_generation(&self) -> io::Result<EngineEndpointGeneration> {
        allocate_identity(&self.shared.next_generation)
            .map(EngineEndpointGeneration)
            .ok_or_else(|| io::Error::other("engine endpoint generation exhausted"))
    }

    fn attach_ref_observer_snapshot(
        &self,
        generation: EngineEndpointGeneration,
        snapshot: RefObserverSnapshotHandle,
    ) {
        let observer_generation = snapshot
            .current()
            .authority_source
            .endpoint_generation()
            .get();
        if let Ok(mut endpoint) = self.shared.endpoint.lock()
            && let Some(endpoint) = endpoint.as_mut()
            && endpoint.generation == generation
            && observer_generation == generation.0
        {
            endpoint.observer = ObserverEndpoint::Snapshot(snapshot);
        }
    }

    fn unregister_engine_endpoint(&self, generation: EngineEndpointGeneration) {
        let Ok(mut endpoint) = self.shared.endpoint.lock() else {
            return;
        };
        if !endpoint
            .as_ref()
            .is_some_and(|endpoint| endpoint.generation == generation)
        {
            return;
        }
        let Ok(mut snapshot) = self.shared.snapshot.lock() else {
            return;
        };
        if let Some(previous) = endpoint.take() {
            previous.controls.disconnect();
            previous.watcher_ingress.disconnect();
        }
        *snapshot = starting_snapshot();
    }

    fn control_registry(&self) -> Option<Arc<WorkspaceControlRegistry>> {
        let endpoint = self.shared.endpoint.lock().ok()?;
        let current = endpoint.as_ref()?;
        self.owns_endpoint(&endpoint)
            .then(|| Arc::clone(&current.controls))
    }

    #[doc(hidden)]
    pub fn watcher_ingress_endpoint(&self) -> Option<Arc<WatcherIngressAccumulator>> {
        let endpoint = self.shared.endpoint.lock().ok()?;
        let current = endpoint.as_ref()?;
        self.owns_endpoint(&endpoint)
            .then(|| Arc::clone(&current.watcher_ingress))
    }

    fn request_engine_barrier(
        &self,
        endpoint: &EngineEndpoint,
    ) -> Result<EngineConvergenceBarrierWaiter, EngineConvergenceBarrierError> {
        let id = EngineConvergenceBarrierId(
            allocate_identity(&self.shared.next_barrier_id)
                .ok_or(EngineConvergenceBarrierError::IdentityExhausted)?,
        );
        let (completion, receiver) = mpsc::channel();
        let pending = Arc::clone(&endpoint.pending);
        let mut registered =
            pending
                .lock()
                .map_err(|_| EngineConvergenceBarrierError::Unavailable {
                    reason: "sync barrier state is unavailable",
                })?;
        registered.insert(id, completion);
        if let Err(error) = endpoint.controls.register_barrier(id, endpoint.generation) {
            registered.remove(&id);
            return Err(error);
        }
        drop(registered);
        Ok(EngineConvergenceBarrierWaiter {
            id,
            generation: endpoint.generation,
            receiver,
            pending,
            controls: Arc::clone(&endpoint.controls),
        })
    }

    fn for_generation(&self, generation: EngineEndpointGeneration) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            generation: Some(generation),
        }
    }
}

fn allocate_identity(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()
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
        EngineSnapshotSink {
            shared: Arc::clone(&shared),
            generation: None,
        },
        EngineSnapshotHandle(shared),
    )
}

/// A cloneable read handle onto the engine's latest published snapshot.
#[derive(Clone, Debug)]
pub struct EngineSnapshotHandle(Arc<EngineShared>);

/// The long-lived driver for one workspace's engine.
pub struct ManifestDriver {
    events: EventSender<EngineEvent>,
    watcher_ingress: WatcherIngressHandle,
    snapshot: EngineSnapshotHandle,
    endpoint_sink: EngineSnapshotSink,
    endpoint_generation: EngineEndpointGeneration,
    barrier_pending:
        Arc<Mutex<BTreeMap<EngineConvergenceBarrierId, Sender<EngineBarrierCompletion>>>>,
    // The engine's shared cost meters (Plan 111 Step 5). The same `Arc` the
    // engine thread writes; the daemon metrics RPC reads it lock-free.
    counters: Arc<EngineCounters>,
    thread: Option<JoinHandle<()>>,
    // The ref-change subscription's worker is stopped when this driver drops.
    ref_subscription: Option<RefChangeSubscription>,
    ref_observer_health: Option<RefObserverSnapshotHandle>,
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
        let (events, inbox) = crossbeam_channel::bounded(MANIFEST_ENGINE_INBOX_CAPACITY);
        let controls = Arc::new(WorkspaceControlRegistry::new());
        let watcher_ingress = watcher_ingress::WatcherIngressAccumulator::new();
        let barrier_pending = Arc::new(Mutex::new(BTreeMap::new()));
        let endpoint_generation = sink.allocate_engine_endpoint_generation()?;
        let thread_sink = sink.for_generation(endpoint_generation);
        let (start, start_gate) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("bowline-manifest-engine".to_string())
            .spawn(move || {
                if start_gate.recv().is_ok() {
                    run(inbox, thread_sink);
                }
            })?;
        if let Err(error) = sink.register_engine_endpoint(
            endpoint_generation,
            events.clone(),
            Arc::clone(&controls),
            Arc::clone(&watcher_ingress),
            Arc::clone(&barrier_pending),
            observer_required,
        ) {
            drop(start);
            let _joined = thread.join();
            return Err(error);
        }
        if start.send(()).is_err() {
            sink.unregister_engine_endpoint(endpoint_generation);
            let _joined = thread.join();
            return Err(io::Error::other("manifest engine start gate closed"));
        }
        Ok(Self {
            events,
            watcher_ingress: watcher_ingress.handle(),
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
        validate_engine_context_identity(&config.context)?;
        let workspace_id = config.context.workspace_identity.clone();
        let device_id = config.context.device_id.clone();
        let process_identity = config.context.process_identity.clone();
        let store = ManifestStore::open(&config.store_path)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let engine = ManifestEngine::new(store, config.context);
        // Capture the engine's counters before it moves into the thread body, so
        // the daemon metrics RPC can read the live tally.
        let counters = engine.counters();
        let transport_client = Arc::clone(&config.client);
        let observer_workspace_id = workspace_id.as_str().to_string();
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
        let subscription = RefChangeSubscription::spawn_for_endpoint(
            config.client,
            observer_workspace_id,
            process_identity,
            driver.events.clone(),
            config.reconnect_delay,
            config.trust_refresh,
            RefObserverEndpointGeneration::new(driver.endpoint_generation.0),
        );
        let observer_health = subscription.snapshot_handle();
        driver
            .endpoint_sink
            .attach_ref_observer_snapshot(driver.endpoint_generation, observer_health.clone());
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
    pub fn event_sender(&self) -> EventSender<EngineEvent> {
        self.events.clone()
    }

    /// A bounded, level-triggered handoff for watcher-derived path detail.
    pub fn watcher_ingress(&self) -> WatcherIngressHandle {
        self.watcher_ingress.clone()
    }

    /// Current remote observer health. Production drivers return `Some`; test
    /// drivers created without a hosted subscription return `None`.
    pub fn ref_observer_health(&self) -> Option<RefObserverHealth> {
        self.ref_observer_health
            .as_ref()
            .map(RefObserverSnapshotHandle::current)
    }

    /// What the remote observer currently contributes to status. `Live` requires
    /// Convex to have delivered the initial reactive value over a worker that is
    /// still running; a credential the control plane refuses is reported apart
    /// from ordinary retrying because no reconnect can clear it.
    pub fn ref_observer_readiness(&self) -> RefObserverReadiness {
        let Some(readiness) = self
            .ref_observer_health
            .as_ref()
            .map(RefObserverSnapshotHandle::readiness)
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
    pub reconnect_delay: ReconnectDelay,
    /// How the observer learns a device whose signed head it cannot verify —
    /// the workspace's shared device trust, refreshed between stream attempts.
    pub trust_refresh: SignerTrustRefresh,
}

/// Run the engine loop, publishing a snapshot after startup and after every
/// due-work cycle. This is the daemon's snapshot-observing composition of the
/// engine's finer `start`/`on_event`/`run_due_work` API; it differs from
/// [`ManifestEngine::run`] only by publishing to `sink`, which the status
/// projection reads. A fatal engine error stops the loop after publishing the
/// terminal snapshot; transport and integrity faults are handled inside the
/// engine and never reach here.
pub fn run_engine_loop<O, R, C>(
    engine: ManifestEngine,
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
    run_engine_loop_with_scan_executor(
        engine,
        objects,
        refs,
        clock,
        inbox,
        sink,
        Arc::new(AuthoritativeScanPlan::execute),
    );
}

fn run_engine_loop_with_scan_executor<O, R, C>(
    mut engine: ManifestEngine,
    objects: &O,
    refs: &R,
    clock: &C,
    inbox: &Receiver<EngineEvent>,
    sink: &EngineSnapshotSink,
    scan_executor: Arc<dyn Fn(AuthoritativeScanPlan) -> AuthoritativeScanResult + Send + Sync>,
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
    sink.complete_barriers(engine.take_completed_barriers());
    let Some(controls) = sink.control_registry() else {
        return;
    };
    let control_wake = controls.wake_receiver();
    let Some(watcher_ingress) = sink.watcher_ingress_endpoint() else {
        return;
    };
    let watcher_wake = watcher_ingress.wake_receiver();
    let (scan_result_tx, scan_result_rx) = crossbeam_channel::bounded(1);
    loop {
        if let Some(action) = controls.take_recovery_action() {
            match action {
                RecoveryControlAction::Scan(id) => match engine.begin_authoritative_local_scan() {
                    Ok(plan) => {
                        let sender = scan_result_tx.clone();
                        let execute = Arc::clone(&scan_executor);
                        thread::spawn(move || {
                            let _sent = sender.send((id, execute(plan)));
                        });
                    }
                    Err(error) => {
                        sink.publish(engine.snapshot());
                        controls.complete_scan(id, Err(error));
                    }
                },
                RecoveryControlAction::Release => {
                    // Release remains an idempotent scheduling edge for callers
                    // using the scan lease, but convergence is never suspended
                    // while that lease is held.
                    engine.schedule_recovered_work(clock);
                }
            }
        }
        for control in controls.drain_barriers() {
            engine.on_event(control, clock);
        }
        for event in watcher_ingress.drain().into_events() {
            engine.on_event(event, clock);
        }
        let timeout = engine
            .next_timeout(clock.now_millis())
            .map(crossbeam_channel::after)
            .unwrap_or_else(crossbeam_channel::never);
        let received = crossbeam_channel::select! {
            recv(scan_result_rx) -> completed => {
                if let Ok((id, result)) = completed {
                    let result = engine.complete_authoritative_local_scan(result);
                    if result.is_ok() {
                        // The complete walk is now ordinary dirty input. Publish
                        // it immediately; do not wait for native watcher sealing.
                        engine.schedule_recovered_work(clock);
                    }
                    sink.publish(engine.snapshot());
                    controls.complete_scan(id, result);
                }
                Err(RecvTimeoutError::Timeout)
            },
            recv(control_wake) -> _ => Err(RecvTimeoutError::Timeout),
            recv(watcher_wake) -> _ => Err(RecvTimeoutError::Timeout),
            recv(inbox) -> event => event.map_err(|_| RecvTimeoutError::Disconnected),
            recv(timeout) -> _ => Err(RecvTimeoutError::Timeout),
        };
        match received {
            Ok(EngineEvent::Shutdown) => {
                engine.on_event(EngineEvent::Shutdown, clock);
                sink.publish(engine.snapshot());
                return;
            }
            Ok(event) => {
                engine.on_event(event, clock);
                sink.publish(engine.snapshot());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                engine.on_event(EngineEvent::Shutdown, clock);
                sink.publish(engine.snapshot());
                return;
            }
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
        sink.complete_barriers(engine.take_completed_barriers());
    }
}

#[cfg(test)]
#[path = "manifest_driver/tests.rs"]
mod tests;
