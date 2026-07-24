//! Watcher-to-engine bridge (Plan 111 Step 1b). The daemon's watcher kernel
//! produces [`WatcherSignal`]s; this bridge consumes them on a dedicated thread
//! and forwards read-filtered [`EngineEvent`]s into the manifest engine's inbox.
//! It replaces the old convergence-journal cause recorder: the manifest engine
//! keeps its dirty set in memory, so no durable cause table is written here.

use super::*;
use bowline_local::sync::manifest_engine::EngineEvent;

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
        let (source_tx, source_rx) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker = spawn_worker(Box::new(move || {
            let Ok(source) = source_rx.recv() else {
                return;
            };
            forward_watcher_signals(source, events, root, worker_shutdown);
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
fn forward_watcher_signals(
    source: mpsc::Receiver<WatcherSignal>,
    events: std::sync::mpsc::Sender<EngineEvent>,
    root: PathBuf,
    shutdown: Arc<AtomicBool>,
) {
    let mut policy_cache = HashMap::new();
    while !shutdown.load(Ordering::Acquire) {
        let signal = match source.recv_timeout(WATCHER_FORWARD_POLL) {
            Ok(signal) => signal,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(event) =
            crate::daemon::watcher::watcher_signal_engine_event(&root, &signal, &mut policy_cache)
            && events.send(event).is_err()
        {
            // The engine thread has stopped; nothing left to forward to.
            break;
        }
    }
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
