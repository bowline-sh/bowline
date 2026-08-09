//! Watcher-to-engine bridge (Plan 111 Step 1b). The daemon's watcher kernel
//! produces [`WatcherSignal`]s; this bridge consumes them on a dedicated thread
//! and forwards read-filtered [`EngineEvent`]s into the manifest engine's inbox.
//! It replaces the old convergence-journal cause recorder: the manifest engine
//! keeps its dirty set in memory, so no durable cause table is written here.

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{fmt, io};

use bowline_local::sync::manifest_engine::{EngineCounters, EngineEvent};
#[cfg(test)]
use crossbeam_channel::{Sender as EngineEventSender, TrySendError};

use crate::daemon::sync::RecoveryClock;
use crate::daemon::watcher::{SyncWatcherCoverageHandle, WatcherOverflowLane};
use crate::daemon::{DaemonRuntime, WatcherSignal};
use bowline_daemon::manifest_driver::{
    EngineSnapshotHandle, WatcherIngressHandle, WatcherIngressObservation,
};
use bowline_daemon::watcher_coverage::{CoverageCancellation, NativeCoverageObservationReceiver};
use bowline_daemon::watcher_recovery::{
    ActivityWatermark, IncidentId, RecoveryCause, RecoveryLifecycle, RecoveryWorkDisposition,
    WatcherRecoveryCoordinator, WatcherRecoveryCoordinatorError, WatcherRecoveryWorker,
};

type WatcherBridgeWorker = Box<dyn FnOnce() + Send + 'static>;
const WATCHER_BRIDGE_SOURCE_FIELD: &str = "sync.change_rx";
const WATCHER_BRIDGE_WORKER_FIELD: &str = "worker";
const WATCHER_FORWARD_POLL: Duration = Duration::from_millis(100);
const WATCHER_FORWARD_BATCH_LIMIT: usize = crate::daemon::WATCHER_DRAIN_BUDGET;
const RECOVERY_ATTEMPT_DEBOUNCE: Duration = Duration::from_millis(100);
// The quiet window above batches a finished burst's tail into one attempt. It
// restarts on every observation, so a producer that never pauses for that long
// can postpone the attempt forever. The ceiling bounds how long batching may
// continue. Starting a scan under load is safe: closure is guarded independently
// by the loss-watermark fence, so new loss still invalidates a close. The bridge
// keeps forwarding ordinary activity durably while it waits, which prevents
// this batching delay from becoming a second source of blindness.
pub(super) const RECOVERY_ATTEMPT_DEBOUNCE_CEILING: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum WatcherBatchForward {
    Forwarded,
    Saturated(RecoveryCause),
    EngineStopped,
}

enum BridgeIngress {
    Production(WatcherIngressHandle),
}

impl BridgeIngress {
    fn observe(&self, event: EngineEvent) -> WatcherBatchForward {
        match self {
            Self::Production(ingress) => match ingress.observe(event) {
                WatcherIngressObservation::Accumulated => WatcherBatchForward::Forwarded,
                WatcherIngressObservation::DetailCollapsed(_) => {
                    WatcherBatchForward::Saturated(RecoveryCause::IngressDetailCollapsed)
                }
                WatcherIngressObservation::EngineStopped => WatcherBatchForward::EngineStopped,
            },
        }
    }
}

struct BridgeRecovery {
    coordinator: Arc<WatcherRecoveryCoordinator>,
    clock: Arc<RecoveryClock>,
    coverage: SyncWatcherCoverageHandle,
    observations: NativeCoverageObservationReceiver,
    engine: EngineSnapshotHandle,
}

#[derive(Debug)]
pub(in crate::daemon) enum WatcherBridgeStartError {
    SourceHandoff {
        field: &'static str,
    },
    ThreadSpawn {
        field: &'static str,
        source: io::Error,
    },
    WorkerPanicked {
        field: &'static str,
    },
}

impl WatcherBridgeStartError {
    pub(super) fn into_io_error(self) -> io::Error {
        let kind = match &self {
            Self::ThreadSpawn { source, .. } => source.kind(),
            Self::SourceHandoff { .. } | Self::WorkerPanicked { .. } => io::ErrorKind::Other,
        };
        io::Error::new(kind, self)
    }
}

impl fmt::Display for WatcherBridgeStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceHandoff { field } => {
                write!(formatter, "watcher bridge could not hand off {field}")
            }
            Self::ThreadSpawn { field, source } => {
                write!(
                    formatter,
                    "watcher bridge could not spawn {field}: {source}"
                )
            }
            Self::WorkerPanicked { field } => {
                write!(formatter, "watcher bridge {field} panicked during startup")
            }
        }
    }
}

impl Error for WatcherBridgeStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn { source, .. } => Some(source),
            Self::SourceHandoff { .. } | Self::WorkerPanicked { .. } => None,
        }
    }
}

pub(super) fn stop_and_join_watcher(
    runtime: &Arc<Mutex<DaemonRuntime>>,
    watcher_bridge: Option<WatcherBridge>,
) -> io::Result<()> {
    if let Ok(mut runtime) = runtime.lock()
        && let Some(sync) = runtime.sync.as_mut()
    {
        sync.watcher.disarm(Instant::now());
    }
    if let Some(watcher_bridge) = watcher_bridge {
        watcher_bridge.join()?;
    }
    Ok(())
}

/// Why a bridge start produced no worker. The two reasons are structurally
/// different: one is a normal startup ordering step, the other means the daemon
/// currently cannot see local file changes at all.
pub(in crate::daemon) enum WatcherBridgeStart {
    Started(WatcherBridge),
    /// No workspace or no manifest driver yet — the driver's own retry loop
    /// brings it up and the bridge starts on a later drive.
    EngineUnavailable,
    /// The watcher kernel is not armed, so there is nothing to forward. Status
    /// reports `ServiceRuntime` degraded until the watcher retry succeeds.
    WatcherDown,
}

pub(in crate::daemon) struct WatcherBridge {
    worker: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    coverage_cancellation: CoverageCancellation,
}

impl WatcherBridge {
    #[cfg(test)]
    pub(super) fn from_worker_for_test(worker: impl FnOnce() + Send + 'static) -> Self {
        Self {
            worker: Some(std::thread::spawn(worker)),
            shutdown: Arc::new(AtomicBool::new(false)),
            coverage_cancellation: CoverageCancellation::new(),
        }
    }

    pub(in crate::daemon) fn start(
        runtime: &mut DaemonRuntime,
    ) -> Result<WatcherBridgeStart, WatcherBridgeStartError> {
        Self::start_with_spawner(runtime, |worker| {
            std::thread::Builder::new()
                .name("bowline-watcher-engine-bridge".to_string())
                .spawn(worker)
        })
    }

    pub(super) fn start_with_spawner(
        runtime: &mut DaemonRuntime,
        spawn_worker: impl FnOnce(WatcherBridgeWorker) -> io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<WatcherBridgeStart, WatcherBridgeStartError> {
        // Only bridge when both the watcher kernel and the manifest engine are
        // live; otherwise there is nothing to forward to.
        let Some(watcher_ingress) = runtime.manifest_watcher_ingress() else {
            return Ok(WatcherBridgeStart::EngineUnavailable);
        };
        let Some(sync) = runtime.sync.as_mut() else {
            return Ok(WatcherBridgeStart::EngineUnavailable);
        };
        let recovery = sync
            .watcher
            .recovery_inputs()
            .map(|(coverage, observations)| BridgeRecovery {
                coordinator: Arc::clone(&sync.recovery_coordinator),
                clock: Arc::clone(&sync.recovery_clock),
                coverage,
                observations,
                engine: sync.manifest_snapshot.1.clone(),
            });
        // Take the receiver before spawning: a worker whose sender is dropped
        // without a hand-off parks forever on `recv`, so there must be a signal
        // source in hand before a thread exists to wait for it. `None` means the
        // watcher is down or a previous bridge still owns its receiver.
        let Some(source) = sync.watcher.take_signals() else {
            return Ok(WatcherBridgeStart::WatcherDown);
        };
        let root = sync.args.root.clone();
        let counters = Arc::clone(&sync.manifest_counters);
        let (source_tx, source_rx) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let coverage_cancellation = CoverageCancellation::new();
        let worker_coverage_cancellation = coverage_cancellation.clone();
        let worker = match spawn_worker(Box::new(move || {
            let Ok(source) = source_rx.recv() else {
                return;
            };
            forward_watcher_signals_with_recovery(
                source,
                BridgeIngress::Production(watcher_ingress),
                root,
                worker_shutdown,
                counters,
                recovery,
                worker_coverage_cancellation,
            );
        })) {
            Ok(worker) => worker,
            Err(spawn_error) => {
                sync.watcher.restore_signals(source);
                return Err(WatcherBridgeStartError::ThreadSpawn {
                    field: WATCHER_BRIDGE_WORKER_FIELD,
                    source: spawn_error,
                });
            }
        };
        if let Err(error) = source_tx.send(source) {
            sync.watcher.restore_signals(error.0);
            worker
                .join()
                .map_err(|_| WatcherBridgeStartError::WorkerPanicked {
                    field: WATCHER_BRIDGE_WORKER_FIELD,
                })?;
            return Err(WatcherBridgeStartError::SourceHandoff {
                field: WATCHER_BRIDGE_SOURCE_FIELD,
            });
        }
        Ok(WatcherBridgeStart::Started(Self {
            worker: Some(worker),
            shutdown,
            coverage_cancellation,
        }))
    }

    pub(in crate::daemon) fn join(mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        self.coverage_cancellation.cancel();
        self.worker
            .take()
            .expect("watcher bridge remains owned until strict join")
            .join()
            .map_err(|_| io::Error::other("watcher engine bridge panicked"))
    }

    /// True when the bridge worker has exited (engine death, channel close, or
    /// panic). Used by the scheduler to drop a stale bridge before rebuild.
    pub(in crate::daemon) fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

/// Consume watcher signals and forward read-filtered engine events until the
/// producer disconnects or shutdown is requested.
#[cfg(test)]
pub(super) fn forward_watcher_signals(
    source: mpsc::Receiver<WatcherSignal>,
    events: EngineEventSender<EngineEvent>,
    root: PathBuf,
    shutdown: Arc<AtomicBool>,
    counters: Arc<EngineCounters>,
) {
    let accumulator = bowline_daemon::manifest_driver::WatcherIngressAccumulator::new();
    let relay_accumulator = Arc::clone(&accumulator);
    let relay_events = events.clone();
    let relay_done = Arc::new(AtomicBool::new(false));
    let worker_done = Arc::clone(&relay_done);
    let relay = std::thread::spawn(move || {
        let wake = relay_accumulator.wake_receiver();
        loop {
            let _wake = wake.recv_timeout(WATCHER_FORWARD_POLL);
            for event in relay_accumulator.drain().into_events() {
                let _outcome = try_forward_engine_event(&relay_events, event);
            }
            if worker_done.load(Ordering::Acquire) {
                break;
            }
        }
    });
    forward_watcher_signals_with_recovery(
        source,
        BridgeIngress::Production(accumulator.handle()),
        root,
        shutdown,
        counters,
        None,
        CoverageCancellation::new(),
    );
    relay_done.store(true, Ordering::Release);
    accumulator.disconnect();
    let _joined = relay.join();
}

fn forward_watcher_signals_with_recovery(
    source: mpsc::Receiver<WatcherSignal>,
    ingress: BridgeIngress,
    root: PathBuf,
    shutdown: Arc<AtomicBool>,
    _counters: Arc<EngineCounters>,
    recovery: Option<BridgeRecovery>,
    coverage_cancellation: CoverageCancellation,
) {
    let mut policy_cache = HashMap::new();
    let mut overflow_lane: Option<Arc<WatcherOverflowLane>> = None;
    let mut recovery = match recovery {
        Some(recovery) => match LiveBridgeRecovery::claim(recovery, coverage_cancellation) {
            Ok(recovery) => Some(recovery),
            Err(error) => {
                eprintln!("bowline-daemon watcher recovery worker could not start: {error}");
                return;
            }
        },
        None => None,
    };
    while !shutdown.load(Ordering::Acquire) {
        if let Some(recovery) = recovery.as_mut() {
            recovery.drain_native_observations();
            let recovery_open = match recovery.is_open() {
                Ok(is_open) => is_open,
                Err(error) => {
                    eprintln!("bowline-daemon watcher recovery state is unavailable: {error}");
                    break;
                }
            };
            // A nominal coordinator and an asserted overflow request cannot both
            // be true: the callback is suppressing changes for a recovery that no
            // longer exists, so nothing observes the workspace and nothing says
            // so. That pairing has escaped once already, when a rewrite dropped
            // the acknowledgement below, so reopen recovery rather than trusting
            // every future edit of this file to remember.
            if !recovery_open
                && overflow_lane
                    .as_ref()
                    .is_some_and(|lane| lane.recovery_requested())
            {
                if let Err(error) =
                    recovery.observe_ingress_loss(RecoveryCause::NativeCallbackLaneSaturated)
                {
                    eprintln!("bowline-daemon watcher overflow could not reopen recovery: {error}");
                    break;
                }
                continue;
            }
            if recovery_open {
                let attempt_is_debounced = match recovery.attempt_is_debounced() {
                    Ok(debounced) => debounced,
                    Err(error) => {
                        eprintln!("bowline-daemon watcher recovery debounce failed: {error}");
                        break;
                    }
                };
                preserve_watcher_visibility_while_debounced(attempt_is_debounced, &overflow_lane);
                if !attempt_is_debounced {
                    match recovery.recover_once(
                        &source,
                        &ingress,
                        &root,
                        &mut policy_cache,
                        &mut overflow_lane,
                        &shutdown,
                    ) {
                        Ok(RecoveryWorkDisposition::Nominal | RecoveryWorkDisposition::Closed) => {}
                        // A rejected attempt has released publication. Fall through
                        // to the ordinary forwarding tail before the next attempt,
                        // so a queued watcher batch cannot repeatedly collapse into
                        // lost fidelity inside the next close fence.
                        Ok(RecoveryWorkDisposition::RetryRequired) => {}
                        Ok(RecoveryWorkDisposition::RetryDeferred) => {
                            // Retry deferral is coordinator policy, not useful work.
                            // Yield the bridge thread so a blocked dependency cannot
                            // turn a recovery incident into a CPU hot loop.
                            std::thread::sleep(WATCHER_FORWARD_POLL);
                            continue;
                        }
                        Ok(RecoveryWorkDisposition::Blocked) => {
                            std::thread::sleep(WATCHER_FORWARD_POLL);
                            continue;
                        }
                        Ok(RecoveryWorkDisposition::Cancelled) => break,
                        Err(error) => {
                            eprintln!("bowline-daemon watcher recovery worker failed: {error}");
                            break;
                        }
                    }
                }
            }
        } else if overflow_lane
            .as_ref()
            .is_some_and(|lane| lane.take_recovery_request())
            && ingress.observe(EngineEvent::FullScanRequired(
                bowline_local::sync::manifest_engine::FullScanReason::WatcherOverflow,
            )) == WatcherBatchForward::EngineStopped
        {
            break;
        }
        let signal = match source.recv_timeout(WATCHER_FORWARD_POLL) {
            Ok(signal) => signal,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match accumulate_watcher_signals(
            signal,
            &source,
            &ingress,
            &root,
            &mut policy_cache,
            &mut overflow_lane,
        ) {
            WatcherBatchForward::Forwarded => {}
            WatcherBatchForward::Saturated(cause) => {
                if let Some(recovery) = recovery.as_ref() {
                    if let Err(error) = recovery.observe_ingress_loss(cause) {
                        eprintln!(
                            "bowline-daemon watcher ingress loss could not open recovery: {error}"
                        );
                        break;
                    }
                } else if let Some(lane) = overflow_lane.as_ref() {
                    lane.request_recovery();
                }
            }
            WatcherBatchForward::EngineStopped => {
                // The engine thread has stopped; nothing left to forward to.
                break;
            }
        }
    }
    if let Some(recovery) = recovery
        && let Err(error) = recovery.worker_exited()
    {
        eprintln!("bowline-daemon watcher recovery worker exit was not recorded: {error}");
    }
}

fn accumulate_watcher_signals(
    first: WatcherSignal,
    source: &mpsc::Receiver<WatcherSignal>,
    ingress: &BridgeIngress,
    root: &std::path::Path,
    policy_cache: &mut HashMap<String, bowline_local::policy::UserPolicy>,
    overflow_lane: &mut Option<Arc<WatcherOverflowLane>>,
) -> WatcherBatchForward {
    let mut next = Some(first);
    let mut drained = 0_usize;
    for _ in 0..WATCHER_FORWARD_BATCH_LIMIT {
        let signal = match next.take() {
            Some(signal) => signal,
            None => match source.try_recv() {
                Ok(signal) => signal,
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                    break;
                }
            },
        };
        drained += 1;
        if let WatcherSignal::OverflowLane(lane) = signal {
            *overflow_lane = Some(lane);
            continue;
        }
        if let Some(event) =
            crate::daemon::watcher::watcher_signal_engine_event(root, &signal, policy_cache)
        {
            let outcome = ingress.observe(event);
            if outcome != WatcherBatchForward::Forwarded {
                return outcome;
            }
        }
    }
    if drained == WATCHER_FORWARD_BATCH_LIMIT {
        return WatcherBatchForward::Saturated(RecoveryCause::NativeEventBatchSaturated);
    }
    // A saturated callback lane means the queued path sample is incomplete.
    // Let the coordinator (or the legacy fallback) replace it with one covering
    // scan instead of transferring an unbounded stale-event backlog downstream.
    if overflow_lane
        .as_ref()
        .is_some_and(|lane| lane.recovery_requested())
    {
        return WatcherBatchForward::Forwarded;
    }
    WatcherBatchForward::Forwarded
}

struct LiveBridgeRecovery {
    coordinator: Arc<WatcherRecoveryCoordinator>,
    clock: Arc<RecoveryClock>,
    coverage: SyncWatcherCoverageHandle,
    observations: NativeCoverageObservationReceiver,
    engine: EngineSnapshotHandle,
    worker: WatcherRecoveryWorker,
    coverage_cancellation: CoverageCancellation,
    attempt_debounce: RecoveryAttemptDebounce,
}

pub(super) struct RecoveryAttemptDebounce {
    watermark: ActivityWatermark,
    stable_since: Instant,
    deferring_since: Instant,
}

impl RecoveryAttemptDebounce {
    pub(super) fn new(watermark: ActivityWatermark, now: Instant) -> Self {
        Self {
            watermark,
            stable_since: now,
            deferring_since: now,
        }
    }

    pub(super) fn defers_attempt(&mut self, watermark: ActivityWatermark, now: Instant) -> bool {
        if now.saturating_duration_since(self.deferring_since) >= RECOVERY_ATTEMPT_DEBOUNCE_CEILING
        {
            self.authorize(watermark, now);
            return false;
        }
        if watermark != self.watermark {
            self.watermark = watermark;
            self.stable_since = now;
            return true;
        }
        if now.saturating_duration_since(self.stable_since) < RECOVERY_ATTEMPT_DEBOUNCE {
            return true;
        }
        self.authorize(watermark, now);
        false
    }

    // Both ways out of deferral start an attempt, so both restart the ceiling:
    // it bounds the wait before one attempt, not the lifetime of the bridge.
    fn authorize(&mut self, watermark: ActivityWatermark, now: Instant) {
        self.watermark = watermark;
        self.stable_since = now;
        self.deferring_since = now;
    }
}

impl LiveBridgeRecovery {
    fn claim(
        recovery: BridgeRecovery,
        coverage_cancellation: CoverageCancellation,
    ) -> Result<Self, WatcherRecoveryCoordinatorError> {
        let snapshot = recovery.coordinator.snapshot()?;
        let worker =
            WatcherRecoveryWorker::claim(Arc::clone(&recovery.coordinator), recovery.clock.now())?;
        Ok(Self {
            coordinator: recovery.coordinator,
            clock: recovery.clock,
            coverage: recovery.coverage,
            observations: recovery.observations,
            engine: recovery.engine,
            worker,
            coverage_cancellation,
            attempt_debounce: RecoveryAttemptDebounce::new(
                snapshot.activity_watermark(),
                Instant::now(),
            ),
        })
    }

    fn drain_native_observations(&self) {
        while let Ok(observation) = self.observations.try_recv() {
            if let Err(error) = self
                .coordinator
                .observe_native_coverage(observation, self.clock.now())
            {
                eprintln!("bowline-daemon native watcher loss could not be admitted: {error}");
            }
        }
    }

    fn is_open(&self) -> Result<bool, WatcherRecoveryCoordinatorError> {
        Ok(self.coordinator.snapshot()?.lifecycle() != RecoveryLifecycle::Nominal)
    }

    fn attempt_is_debounced(&mut self) -> Result<bool, WatcherRecoveryCoordinatorError> {
        let snapshot = self.coordinator.snapshot()?;
        if snapshot.primary_cause() == Some(RecoveryCause::StartupReconciliation) {
            return Ok(false);
        }
        Ok(self
            .attempt_debounce
            .defers_attempt(snapshot.activity_watermark(), Instant::now()))
    }

    fn observe_ingress_loss(
        &self,
        cause: RecoveryCause,
    ) -> Result<(), WatcherRecoveryCoordinatorError> {
        self.coordinator.observe_loss(cause, self.clock.now())?;
        Ok(())
    }

    fn recover_once(
        &mut self,
        source: &mpsc::Receiver<WatcherSignal>,
        ingress: &BridgeIngress,
        root: &std::path::Path,
        policy_cache: &mut HashMap<String, bowline_local::policy::UserPolicy>,
        overflow_lane: &mut Option<Arc<WatcherOverflowLane>>,
        shutdown: &AtomicBool,
    ) -> Result<RecoveryWorkDisposition, WatcherRecoveryCoordinatorError> {
        let coordinator = Arc::clone(&self.coordinator);
        let clock = Arc::clone(&self.clock);
        let mut before_close = || {
            drain_pending_signals(
                source,
                ingress,
                root,
                policy_cache,
                overflow_lane,
                &coordinator,
                &clock,
            )
        };
        let moment_clock = Arc::clone(&self.clock);
        let moment_source: Arc<
            dyn Fn() -> bowline_daemon::watcher_recovery::RecoveryMoment + Send + Sync,
        > = Arc::new(move || moment_clock.now());
        self.worker.recover_once(
            &mut self.coverage,
            &self.engine,
            &moment_source,
            &|| shutdown.load(Ordering::Acquire),
            &self.coverage_cancellation,
            &mut before_close,
        )
    }

    fn worker_exited(self) -> Result<IncidentId, WatcherRecoveryCoordinatorError> {
        self.worker.worker_exited(self.clock.now())
    }
}

// The callback asserts the overflow request and then stops forwarding ordinary
// changes, because an authoritative scan is going to cover them instead. That
// request is what makes the watcher deliberately blind, so acknowledging it is
// what restores sight. Once recovery owns the fidelity gap, a callback racing
// an acknowledgement either advances the activity watermark and invalidates an
// eventual close, or reasserts loss and keeps recovery open. Waiting until after
// closure has neither property, and a write suppressed in that window is
// observed by nobody.
fn acknowledge_recovery_owned_overflow(overflow_lane: &Option<Arc<WatcherOverflowLane>>) {
    if let Some(lane) = overflow_lane.as_ref() {
        let _acknowledged = lane.take_recovery_request();
    }
}

pub(super) fn preserve_watcher_visibility_while_debounced(
    attempt_is_debounced: bool,
    overflow_lane: &Option<Arc<WatcherOverflowLane>>,
) {
    if attempt_is_debounced {
        // Recovery now owns the fidelity gap, so the callback does not need to
        // stay blind while the quiet-window batching policy waits. Reopen its
        // lane; the caller then drains a normal batch. New activity is either
        // forwarded durably or a new drop reasserts loss and invalidates the
        // eventual close.
        acknowledge_recovery_owned_overflow(overflow_lane);
    }
}

fn drain_pending_signals(
    source: &mpsc::Receiver<WatcherSignal>,
    ingress: &BridgeIngress,
    root: &std::path::Path,
    policy_cache: &mut HashMap<String, bowline_local::policy::UserPolicy>,
    overflow_lane: &mut Option<Arc<WatcherOverflowLane>>,
    coordinator: &WatcherRecoveryCoordinator,
    clock: &RecoveryClock,
) -> Result<(), WatcherRecoveryCoordinatorError> {
    loop {
        acknowledge_recovery_owned_overflow(overflow_lane);
        match source.try_recv() {
            Ok(signal) => {
                let forward = accumulate_watcher_signals(
                    signal,
                    source,
                    ingress,
                    root,
                    policy_cache,
                    overflow_lane,
                );
                // The batch may have carried the first OverflowLane signal, so the
                // lane this loop can reach is only known after accumulating.
                acknowledge_recovery_owned_overflow(overflow_lane);
                match forward {
                    WatcherBatchForward::Forwarded => {}
                    WatcherBatchForward::Saturated(cause) => {
                        coordinator.observe_loss(cause, clock.now())?;
                    }
                    WatcherBatchForward::EngineStopped => return Ok(()),
                }
            }
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                coordinator.observe_loss(RecoveryCause::WatcherDisconnected, clock.now())?;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
fn try_forward_engine_event(
    events: &EngineEventSender<EngineEvent>,
    event: EngineEvent,
) -> WatcherBatchForward {
    match events.try_send(event) {
        Ok(()) => WatcherBatchForward::Forwarded,
        Err(TrySendError::Full(_)) => {
            WatcherBatchForward::Saturated(RecoveryCause::IngressDetailCollapsed)
        }
        Err(TrySendError::Disconnected(_)) => WatcherBatchForward::EngineStopped,
    }
}

impl Drop for WatcherBridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.coverage_cancellation.cancel();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("bowline-daemon watcher engine bridge panicked during ownership drop");
        }
    }
}
