//! Watcher-to-engine bridge (Plan 111 Step 1b). The daemon's watcher kernel
//! produces [`WatcherSignal`]s; this bridge consumes them on a dedicated thread
//! and forwards read-filtered [`EngineEvent`]s into the manifest engine's inbox.
//! It replaces the old convergence-journal cause recorder: the manifest engine
//! keeps its dirty set in memory, so no durable cause table is written here.

use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{fmt, io};

use bowline_local::policy::UserPolicy;

use crate::daemon::DaemonRuntime;
use crate::daemon::WatcherSignal;
use crate::daemon::watcher::WatcherOverflowLane;
use bowline_local::sync::manifest_engine::{EngineCounters, EngineEvent};

type WatcherBridgeWorker = Box<dyn FnOnce() + Send + 'static>;
const WATCHER_BRIDGE_SOURCE_FIELD: &str = "sync.change_rx";
const WATCHER_BRIDGE_WORKER_FIELD: &str = "worker";
const WATCHER_FORWARD_POLL: Duration = Duration::from_millis(100);
const WATCHER_OVERFLOW_ACTIVITY_POLL: Duration = Duration::from_millis(5);
// macOS can deliver a stopped daemon's finite FSEvents burst in several waves
// separated by more than a second. Closing the lane between those waves turns
// one kernel overflow into several serial full scans and can consume the whole
// edit budget. Wait across that delivery gap while retaining a hard ceiling for
// a genuinely continuous producer.
const WATCHER_OVERFLOW_QUIET_PERIOD: Duration = Duration::from_secs(3);
const WATCHER_OVERFLOW_DRAIN_LIMIT: Duration = Duration::from_secs(5);

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
}

impl WatcherBridge {
    #[cfg(test)]
    pub(super) fn from_worker_for_test(worker: impl FnOnce() + Send + 'static) -> Self {
        Self {
            worker: Some(std::thread::spawn(worker)),
            shutdown: Arc::new(AtomicBool::new(false)),
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
        let Some(events) = runtime.manifest_event_sender() else {
            return Ok(WatcherBridgeStart::EngineUnavailable);
        };
        let Some(sync) = runtime.sync.as_mut() else {
            return Ok(WatcherBridgeStart::EngineUnavailable);
        };
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
        let worker = match spawn_worker(Box::new(move || {
            let Ok(source) = source_rx.recv() else {
                return;
            };
            forward_watcher_signals(source, events, root, worker_shutdown, counters);
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
        }))
    }

    pub(in crate::daemon) fn join(mut self) -> io::Result<()> {
        // The caller disconnects the native producer first; the forwarding loop
        // then drains any queued signals and exits when the channel closes.
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
pub(super) fn forward_watcher_signals(
    source: mpsc::Receiver<WatcherSignal>,
    events: std::sync::mpsc::Sender<EngineEvent>,
    root: PathBuf,
    shutdown: Arc<AtomicBool>,
    counters: Arc<EngineCounters>,
) {
    let mut policy_cache = HashMap::new();
    let mut overflow_lane: Option<Arc<WatcherOverflowLane>> = None;
    while !shutdown.load(Ordering::Acquire) {
        if overflow_lane
            .as_ref()
            .is_some_and(|lane| lane.recovery_requested())
        {
            if !forward_overflow_recovery(OverflowRecoveryRequest {
                source: &source,
                events: &events,
                root: &root,
                policy_cache: &mut policy_cache,
                overflow_lane: overflow_lane.as_deref().expect("checked overflow lane"),
                initial_signal: None,
                shutdown: &shutdown,
                counters: &counters,
            }) {
                break;
            }
            continue;
        }
        let signal = match source.recv_timeout(WATCHER_FORWARD_POLL) {
            Ok(signal) => signal,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let WatcherSignal::OverflowLane(lane) = signal {
            overflow_lane = Some(lane);
            continue;
        }
        // An overflow may race the receive above. Do not forward one obsolete
        // backlog item ahead of recovery: collapse it with the rest, then emit
        // the fence after every discarded filesystem event was observable.
        if overflow_lane
            .as_ref()
            .is_some_and(|lane| lane.recovery_requested())
        {
            if !forward_overflow_recovery(OverflowRecoveryRequest {
                source: &source,
                events: &events,
                root: &root,
                policy_cache: &mut policy_cache,
                overflow_lane: overflow_lane.as_deref().expect("checked overflow lane"),
                initial_signal: Some(signal),
                shutdown: &shutdown,
                counters: &counters,
            }) {
                break;
            }
            continue;
        }
        if let Some(event) =
            crate::daemon::watcher::watcher_signal_engine_event(&root, &signal, &mut policy_cache)
            && !forward_engine_event(&events, &counters, event)
        {
            // The engine thread has stopped; nothing left to forward to.
            break;
        }
    }
}

/// Collapse a saturated native backlog into one full-scan fence. The lane stays
/// asserted while draining, so every dropped event is covered by the scan sent
/// afterward. It is cleared immediately before that send: an event lost after
/// the clear re-arms the lane and therefore receives a second fence, while a
/// successfully queued follow-up remains ordered after the first fence.
struct OverflowRecoveryRequest<'a> {
    source: &'a mpsc::Receiver<WatcherSignal>,
    events: &'a std::sync::mpsc::Sender<EngineEvent>,
    root: &'a Path,
    policy_cache: &'a mut HashMap<String, UserPolicy>,
    overflow_lane: &'a WatcherOverflowLane,
    initial_signal: Option<WatcherSignal>,
    shutdown: &'a AtomicBool,
    counters: &'a EngineCounters,
}

fn forward_overflow_recovery(request: OverflowRecoveryRequest<'_>) -> bool {
    let OverflowRecoveryRequest {
        source,
        events,
        root,
        policy_cache,
        overflow_lane,
        initial_signal,
        shutdown,
        counters,
    } = request;
    // Hold the engine before draining: the first few native events can reach
    // its FIFO before saturation becomes visible. Without this early phase,
    // their normal debounce can start an obsolete sync cycle while the bridge
    // is still collapsing the burst, delaying the covering scan behind IO.
    counters.begin_watcher_overflow_recovery();
    let mut source_connected = true;
    let mut limited_signal = initial_signal.and_then(|signal| match signal {
        WatcherSignal::Limited { reason } => Some(WatcherSignal::Limited { reason }),
        WatcherSignal::OverflowLane(_)
        | WatcherSignal::Changed { .. }
        | WatcherSignal::Recoverable => None,
    });
    // Dropped callback events bypass this channel by design, so channel
    // emptiness alone cannot identify the end of a native burst. The shared
    // generation advances for both queued and coalesced overflow activity;
    // close the lane only after that generation and the channel stay quiet.
    // The absolute limit keeps a continuously active producer bounded.
    let started_at = Instant::now();
    let drain_deadline = started_at + WATCHER_OVERFLOW_DRAIN_LIMIT;
    let mut quiet_since = started_at;
    let mut observed_generation = overflow_lane.activity_generation();
    loop {
        if shutdown.load(Ordering::Acquire) {
            return false;
        }
        let now = Instant::now();
        if now >= drain_deadline {
            break;
        }
        let quiet_wait = WATCHER_OVERFLOW_ACTIVITY_POLL.min(drain_deadline - now);
        match source.recv_timeout(quiet_wait) {
            Ok(WatcherSignal::Limited { reason }) => {
                limited_signal = Some(WatcherSignal::Limited { reason });
                quiet_since = Instant::now();
            }
            Ok(WatcherSignal::OverflowLane(_))
            | Ok(WatcherSignal::Changed { .. })
            | Ok(WatcherSignal::Recoverable) => {
                quiet_since = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                source_connected = false;
                break;
            }
        }
        let activity_generation = overflow_lane.activity_generation();
        if activity_generation != observed_generation {
            observed_generation = activity_generation;
            quiet_since = Instant::now();
        }
        if Instant::now().duration_since(quiet_since) >= WATCHER_OVERFLOW_QUIET_PERIOD {
            break;
        }
    }

    if overflow_lane.take_recovery_request()
        && !forward_engine_event(
            events,
            counters,
            EngineEvent::FullScanRequired(
                bowline_local::sync::manifest_engine::FullScanReason::WatcherOverflow,
            ),
        )
    {
        return false;
    }
    if let Some(signal) = limited_signal
        && let Some(event) =
            crate::daemon::watcher::watcher_signal_engine_event(root, &signal, policy_cache)
        && !forward_engine_event(events, counters, event)
    {
        return false;
    }
    source_connected
}

fn forward_engine_event(
    events: &std::sync::mpsc::Sender<EngineEvent>,
    counters: &EngineCounters,
    event: EngineEvent,
) -> bool {
    let recovered_watcher_overflow = matches!(
        &event,
        EngineEvent::FullScanRequired(
            bowline_local::sync::manifest_engine::FullScanReason::WatcherOverflow
        )
    );
    if recovered_watcher_overflow {
        // The ordinary engine channel is shared with watcher paths already in
        // flight. Assert the level-triggered recovery first so the engine folds
        // the full scan ahead of that stale FIFO tail when this send wakes it.
        counters.request_watcher_overflow_recovery();
    }
    if events.send(event).is_err() {
        return false;
    }
    if recovered_watcher_overflow {
        counters.record_watcher_overflow_recovery();
    }
    true
}

impl Drop for WatcherBridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("bowline-daemon watcher engine bridge panicked during ownership drop");
        }
    }
}
