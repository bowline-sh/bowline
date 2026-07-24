//! Watcher-to-engine bridge (Plan 111 Step 1b). The daemon's watcher kernel
//! produces [`WatcherSignal`]s; this bridge consumes them on a dedicated thread
//! and forwards read-filtered [`EngineEvent`]s into the manifest engine's inbox.
//! It replaces the old convergence-journal cause recorder: the manifest engine
//! keeps its dirty set in memory, so no durable cause table is written here.

use super::*;
use crate::daemon::watcher::WatcherOverflowLane;
use bowline_local::sync::manifest_engine::{EngineCounters, EngineEvent};

type WatcherBridgeWorker = Box<dyn FnOnce() + Send + 'static>;
const WATCHER_BRIDGE_SOURCE_FIELD: &str = "sync.change_rx";
const WATCHER_BRIDGE_WORKER_FIELD: &str = "worker";
const WATCHER_FORWARD_POLL: Duration = Duration::from_millis(100);

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
        sync.watcher.take();
        sync.change_rx.take();
    }
    if let Some(watcher_bridge) = watcher_bridge {
        watcher_bridge.join()?;
    }
    Ok(())
}

pub(in crate::daemon) struct WatcherBridge {
    worker: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl WatcherBridge {
    pub(in crate::daemon) fn start(
        runtime: &mut DaemonRuntime,
    ) -> Result<Option<Self>, WatcherBridgeStartError> {
        Self::start_with_spawner(runtime, |worker| {
            std::thread::Builder::new()
                .name("bowline-watcher-engine-bridge".to_string())
                .spawn(worker)
        })
    }

    pub(super) fn start_with_spawner(
        runtime: &mut DaemonRuntime,
        spawn_worker: impl FnOnce(WatcherBridgeWorker) -> io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Option<Self>, WatcherBridgeStartError> {
        // Only bridge when both the watcher kernel and the manifest engine are
        // live; otherwise there is nothing to forward to.
        let Some(events) = runtime.manifest_event_sender() else {
            return Ok(None);
        };
        let Some(sync) = runtime.sync.as_mut() else {
            return Ok(None);
        };
        if sync.change_rx.is_none() {
            return Ok(None);
        }
        let root = sync.args.root.clone();
        let counters = Arc::clone(&sync.manifest_counters);
        let (source_tx, source_rx) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker = spawn_worker(Box::new(move || {
            let Ok(source) = source_rx.recv() else {
                return;
            };
            forward_watcher_signals(source, events, root, worker_shutdown, counters);
        }))
        .map_err(|source| WatcherBridgeStartError::ThreadSpawn {
            field: WATCHER_BRIDGE_WORKER_FIELD,
            source,
        })?;
        let source = sync
            .change_rx
            .take()
            .expect("watcher signal receiver remains owned until worker spawn succeeds");
        if let Err(error) = source_tx.send(source) {
            sync.change_rx = Some(error.0);
            worker
                .join()
                .map_err(|_| WatcherBridgeStartError::WorkerPanicked {
                    field: WATCHER_BRIDGE_WORKER_FIELD,
                })?;
            return Err(WatcherBridgeStartError::SourceHandoff {
                field: WATCHER_BRIDGE_SOURCE_FIELD,
            });
        }
        Ok(Some(Self {
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
    let mut source_connected = true;
    let initial_signal_count = usize::from(initial_signal.is_some());
    let mut limited_signal = initial_signal.and_then(|signal| match signal {
        WatcherSignal::Limited { reason } => Some(WatcherSignal::Limited { reason }),
        WatcherSignal::OverflowLane(_)
        | WatcherSignal::Changed { .. }
        | WatcherSignal::Recoverable => None,
    });
    // At overflow, at most one full channel capacity can predate the recovery
    // request. Drain that fixed snapshot only. A producer replenishing the
    // channel cannot postpone the fence; remaining events stay FIFO-ordered
    // behind it and are forwarded normally.
    for _ in initial_signal_count..crate::daemon::WATCHER_DRAIN_BUDGET {
        if shutdown.load(Ordering::Acquire) {
            return false;
        }
        match source.try_recv() {
            Ok(WatcherSignal::Limited { reason }) => {
                limited_signal = Some(WatcherSignal::Limited { reason });
            }
            Ok(WatcherSignal::OverflowLane(_))
            | Ok(WatcherSignal::Changed { .. })
            | Ok(WatcherSignal::Recoverable) => {}
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                source_connected = false;
                break;
            }
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
