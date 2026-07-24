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
    EngineSnapshot, ManifestEngine, ManifestStore, RemoteObjects, RemoteRef, SyncBarrierId,
    SystemClock,
};

use crate::manifest_transport::{
    ManifestTransport, ReconnectDelay, RefChangeSubscription, RefObserverHealth,
    RefObserverHealthHandle,
};

/// The engine's own database file, kept at the daemon state root (never inside
/// the synced workspace — the engine never writes ordinary workspace files
/// outside user space).
pub const MANIFEST_ENGINE_DB_FILE: &str = "manifest_engine.sqlite3";

/// A thread-safe sink the engine thread publishes its latest snapshot into. The
/// status projection reads the other end through [`EngineSnapshotHandle`].
#[derive(Debug)]
struct BarrierEndpoint {
    generation: u64,
    events: Sender<EngineEvent>,
    pending: Arc<Mutex<BTreeMap<SyncBarrierId, Sender<EngineSnapshot>>>>,
}

#[derive(Debug)]
struct EngineShared {
    snapshot: Mutex<EngineSnapshot>,
    barrier: Mutex<Option<BarrierEndpoint>>,
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
        let pending = self.0.barrier.lock().ok().and_then(|endpoint| {
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

    fn register_barrier_endpoint(
        &self,
        events: Sender<EngineEvent>,
        pending: Arc<Mutex<BTreeMap<SyncBarrierId, Sender<EngineSnapshot>>>>,
    ) -> u64 {
        let generation = self.0.next_generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut endpoint) = self.0.barrier.lock() {
            *endpoint = Some(BarrierEndpoint {
                generation,
                events,
                pending,
            });
        }
        generation
    }

    fn unregister_barrier_endpoint(&self, generation: u64) {
        if let Ok(mut endpoint) = self.0.barrier.lock()
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
        barrier: Mutex::new(None),
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
    pub fn request_sync_barrier(&self) -> io::Result<SyncBarrierWaiter> {
        let (id, events, pending) = {
            let endpoint = self
                .0
                .barrier
                .lock()
                .map_err(|_| io::Error::other("sync barrier state is unavailable"))?;
            let endpoint = endpoint
                .as_ref()
                .ok_or_else(|| io::Error::other("manifest sync engine is unavailable"))?;
            (
                SyncBarrierId(self.0.next_barrier_id.fetch_add(1, Ordering::Relaxed)),
                endpoint.events.clone(),
                Arc::clone(&endpoint.pending),
            )
        };
        let (completion, receiver) = mpsc::channel();
        pending
            .lock()
            .map_err(|_| io::Error::other("sync barrier state is unavailable"))?
            .insert(id, completion);
        if events.send(EngineEvent::SyncBarrier(id)).is_err() {
            if let Ok(mut pending) = pending.lock() {
                pending.remove(&id);
            }
            return Err(io::Error::other("manifest sync engine stopped"));
        }
        Ok(SyncBarrierWaiter {
            id,
            receiver,
            pending,
        })
    }
}

/// Reactive completion handle for one exact sync barrier.
pub struct SyncBarrierWaiter {
    id: SyncBarrierId,
    receiver: Receiver<EngineSnapshot>,
    pending: Arc<Mutex<BTreeMap<SyncBarrierId, Sender<EngineSnapshot>>>>,
}

impl SyncBarrierWaiter {
    pub fn wait(self, timeout: Duration) -> io::Result<EngineSnapshot> {
        self.receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => io::Error::new(
                    io::ErrorKind::TimedOut,
                    "sync barrier did not converge before the deadline",
                ),
                mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::other("manifest sync engine stopped before the barrier completed")
                }
            })
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
    }
}

/// The long-lived driver for one workspace's engine.
pub struct ManifestDriver {
    events: Sender<EngineEvent>,
    snapshot: EngineSnapshotHandle,
    barrier_sink: EngineSnapshotSink,
    barrier_generation: u64,
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
        let (events, inbox) = mpsc::channel();
        let barrier_pending = Arc::new(Mutex::new(BTreeMap::new()));
        let barrier_generation =
            sink.register_barrier_endpoint(events.clone(), Arc::clone(&barrier_pending));
        let thread_sink = sink.clone();
        let thread = match thread::Builder::new()
            .name("bowline-manifest-engine".to_string())
            .spawn(move || run(inbox, thread_sink))
        {
            Ok(thread) => thread,
            Err(error) => {
                sink.unregister_barrier_endpoint(barrier_generation);
                return Err(error);
            }
        };
        Ok(Self {
            events,
            snapshot: handle,
            barrier_sink: sink,
            barrier_generation,
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
        let mut driver = Self::spawn_with_sink(sink, handle, move |inbox, sink| {
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
        );
        driver.ref_observer_health = Some(subscription.health_handle());
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

    /// Readiness requires Convex to have delivered the initial reactive value.
    pub fn ref_observer_is_live(&self) -> bool {
        self.ref_subscription
            .as_ref()
            .is_some_and(|subscription| !subscription.is_finished())
            && self
                .ref_observer_health
                .as_ref()
                .is_some_and(RefObserverHealthHandle::is_live)
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
        self.barrier_sink
            .unregister_barrier_endpoint(self.barrier_generation);
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
    loop {
        let received = match engine.next_timeout(clock.now_millis()) {
            Some(timeout) => inbox.recv_timeout(timeout),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
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
        sink.complete_barriers(engine.take_completed_barriers(), &snapshot);
    }
}

#[cfg(test)]
#[path = "manifest_driver/tests.rs"]
mod tests;
