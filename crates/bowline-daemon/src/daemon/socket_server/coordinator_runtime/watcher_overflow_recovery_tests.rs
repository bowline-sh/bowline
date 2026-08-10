//! Regression pins for the covered-overflow acknowledgement (release proof:
//! silent post-burst blindness). The native callback asserts the level-
//! triggered overflow request and then withholds ordinary changes because an
//! authoritative rescan covers them; the acknowledgement inside the recovery
//! close fence is the only thing that restores sight. Commit `7f902e1cf`
//! deleted both acknowledgement sites in a rename and every existing test kept
//! passing, because the harness walked the `recovery == None` fallback, which
//! clears the latch itself. These tests run the production `recovery ==
//! Some(..)` path: a real native coverage adapter, a real engine servicing
//! authoritative scans, and the real recovery coordinator.

use bowline_daemon::watcher_recovery::RecoveryMoment;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::Event;
use notify::event::{EventKind, ModifyKind};

use super::{WatcherBridge, WatcherBridgeStart};
use crate::daemon::sync::{ManifestEngineHost, RecoveryClock, recovery_process_identity};
use crate::daemon::watcher::{
    SyncWatcher, SyncWatcherCoverageHandle, WatcherOverflowLane, send_watcher_signal,
    start_sync_watcher,
};
use crate::daemon::{DaemonRuntime, WatcherSignal};
use bowline_core::ids::WorkspaceId;
use bowline_daemon::manifest_driver::{
    EngineSnapshotHandle, EngineSnapshotSink, MANIFEST_ENGINE_DB_FILE, ManifestDriver,
    WatcherIngressHandle, run_engine_loop, shared_engine_snapshot,
};
use bowline_daemon::watcher_coverage::{
    CoverageCancellation, CoverageWait, WatcherCoverageAdapter, WatcherCoverageIds,
};
use bowline_daemon::watcher_recovery::{
    BackoffPolicy, RecoveryLifecycle, RecoveryPhase, RecoverySourceIdentity,
    RecoveryWorkDisposition, WatcherRecoveryCoordinator, WatcherRecoveryCoordinatorError,
    WatcherRecoveryWorker,
};
use bowline_local::notifications::NotificationDedupe;
use bowline_local::sync::manifest_engine::empty_genesis::{
    EmptyGenesisTransport, empty_genesis_engine,
};
use bowline_local::sync::manifest_engine::{
    BlobKey, BlobReaderUpload, BlobUpload, CasOutcome, EngineEvent, ManifestKey, ManifestUpload,
    RefObservation, RemoteObjects, RemoteRef, TransportError,
};
use bowline_local::sync::manifest_engine::{EngineCounters, SystemClock, WorkspacePath};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[path = "watcher_overflow_coverage_fixture.rs"]
mod watcher_overflow_coverage_fixture;
use watcher_overflow_coverage_fixture::{GateSeal, GateSecondPreparation};

static EARLY_RELEASE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn serialize_early_release_test() -> std::sync::MutexGuard<'static, ()> {
    EARLY_RELEASE_TEST_LOCK
        .lock()
        .expect("early-release seam test lock")
}

/// A real manifest engine over `root` whose loop services authoritative
/// coverage scans — the one dependency `recover_once` cannot fake, because the
/// scan lease protocol is owned by the engine loop.
/// A transport that fires a hook the first time the engine uploads a blob.
///
/// Blob upload happens inside the engine's publication pass, which the registry
/// dispatches strictly before the authoritative walk. That ordering is
/// structural, not timed, so a hook fired here is guaranteed pre-walk -- which is
/// exactly the window the recovery fence used to cover and no longer should.
enum ProbeTransport {
    Plain(EmptyGenesisTransport),
    Probing(PreWalkProbeTransport),
    Shared(SharedRemote),
}

impl RemoteObjects for ProbeTransport {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        match self {
            Self::Plain(inner) => inner.put_blob(upload),
            Self::Probing(inner) => inner.put_blob(upload),
            Self::Shared(inner) => inner.put_blob(upload),
        }
    }
    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        match self {
            Self::Plain(inner) => inner.put_blob_reader(upload),
            Self::Probing(inner) => inner.put_blob_reader(upload),
            Self::Shared(inner) => inner.put_blob_reader(upload),
        }
    }
    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        match self {
            Self::Plain(inner) => inner.put_manifest(upload),
            Self::Probing(inner) => inner.put_manifest(upload),
            Self::Shared(inner) => inner.put_manifest(upload),
        }
    }
    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        match self {
            Self::Plain(inner) => inner.get_blob(key),
            Self::Probing(inner) => inner.get_blob(key),
            Self::Shared(inner) => inner.get_blob(key),
        }
    }
    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        match self {
            Self::Plain(inner) => inner.get_manifest(key),
            Self::Probing(inner) => inner.get_manifest(key),
            Self::Shared(inner) => inner.get_manifest(key),
        }
    }
}

impl RemoteRef for ProbeTransport {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        match self {
            Self::Plain(_) | Self::Probing(_) => Ok(None),
            Self::Shared(inner) => inner.read_ref(),
        }
    }
    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        match self {
            Self::Plain(_) | Self::Probing(_) => Ok(CasOutcome::Ambiguous),
            Self::Shared(inner) => inner.compare_and_swap(expected_version, new_manifest_key),
        }
    }
}

#[derive(Clone, Default)]
struct SharedRemote {
    state: Arc<Mutex<SharedRemoteState>>,
}

#[derive(Default)]
struct SharedRemoteState {
    blobs: BTreeMap<BlobKey, Vec<u8>>,
    manifests: BTreeMap<ManifestKey, Vec<u8>>,
    head: Option<RefObservation>,
    ref_visible: bool,
}

impl SharedRemote {
    fn expose_ref(&self) {
        self.state.lock().expect("remote state lock").ref_visible = true;
    }

    fn head(&self) -> Option<RefObservation> {
        self.state.lock().expect("remote state lock").head.clone()
    }
}

impl RemoteObjects for SharedRemote {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.state
            .lock()
            .map_err(|_| TransportError::new("put-blob", "remote state lock"))?
            .blobs
            .insert(upload.key.clone(), upload.sealed.to_vec());
        Ok(())
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        let sealed = std::fs::read(upload.spool_path)
            .map_err(|error| TransportError::new("put-blob-reader", error.to_string()))?;
        self.state
            .lock()
            .map_err(|_| TransportError::new("put-blob-reader", "remote state lock"))?
            .blobs
            .insert(upload.key.clone(), sealed);
        Ok(())
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.state
            .lock()
            .map_err(|_| TransportError::new("put-manifest", "remote state lock"))?
            .manifests
            .insert(upload.key.clone(), upload.sealed.to_vec());
        Ok(())
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.state
            .lock()
            .map_err(|_| TransportError::new("get-blob", "remote state lock"))?
            .blobs
            .get(key)
            .cloned()
            .ok_or_else(|| TransportError::new("get-blob", "blob absent"))
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.state
            .lock()
            .map_err(|_| TransportError::new("get-manifest", "remote state lock"))?
            .manifests
            .get(key)
            .cloned()
            .ok_or_else(|| TransportError::new("get-manifest", "manifest absent"))
    }
}

impl RemoteRef for SharedRemote {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        let state = self
            .state
            .lock()
            .map_err(|_| TransportError::new("read-ref", "remote state lock"))?;
        Ok(state.ref_visible.then(|| state.head.clone()).flatten())
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| TransportError::new("compare-and-swap", "remote state lock"))?;
        let current_version = state.head.as_ref().map(|head| head.version);
        if current_version != expected_version {
            return state.head.clone().map(CasOutcome::Lost).ok_or_else(|| {
                TransportError::new("compare-and-swap", "genesis expectation mismatch")
            });
        }
        let advanced = RefObservation {
            version: current_version.unwrap_or(0).saturating_add(1),
            manifest_key: new_manifest_key.clone(),
        };
        state.head = Some(advanced.clone());
        Ok(CasOutcome::Advanced(advanced))
    }
}

struct PreWalkProbeTransport {
    inner: EmptyGenesisTransport,
    on_first_blob: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PreWalkProbeTransport {
    fn new(on_first_blob: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            inner: EmptyGenesisTransport,
            on_first_blob: Mutex::new(Some(on_first_blob)),
        }
    }

    fn fire(&self) {
        let hook = self
            .on_first_blob
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(hook) = hook {
            hook();
        }
    }
}

impl RemoteObjects for PreWalkProbeTransport {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.fire();
        self.inner.put_blob(upload)
    }
    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        self.fire();
        self.inner.put_blob_reader(upload)
    }
    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.inner.put_manifest(upload)
    }
    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.inner.get_blob(key)
    }
    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.inner.get_manifest(key)
    }
}

fn empty_genesis_driver(
    root: &Path,
    state_root: &Path,
    workspace_id: &str,
    on_first_blob: Option<Box<dyn FnOnce() + Send>>,
) -> (
    ManifestDriver,
    EngineSnapshotSink,
    EngineSnapshotHandle,
    Arc<EngineCounters>,
) {
    let engine = empty_genesis_engine(
        root.to_path_buf(),
        state_root.join(MANIFEST_ENGINE_DB_FILE),
        workspace_id,
    );
    // Taken before the engine moves into the driver thread: the counters are the
    // only way to observe from outside that a cycle actually ran.
    let counters = engine.counters();
    let (sink, handle) = shared_engine_snapshot();
    let driver =
        ManifestDriver::spawn_with_sink(sink.clone(), handle.clone(), move |inbox, sink| {
            let transport = match on_first_blob {
                Some(hook) => ProbeTransport::Probing(PreWalkProbeTransport::new(hook)),
                None => ProbeTransport::Plain(EmptyGenesisTransport),
            };
            let clock = SystemClock::default();
            run_engine_loop(engine, &transport, &transport, &clock, &inbox, &sink);
        })
        .expect("empty genesis engine driver spawns");
    (driver, sink, handle, counters)
}

fn shared_remote_driver(
    root: &Path,
    state_root: &Path,
    workspace_id: &str,
    remote: SharedRemote,
) -> (
    ManifestDriver,
    EngineSnapshotSink,
    EngineSnapshotHandle,
    Arc<EngineCounters>,
) {
    let engine = empty_genesis_engine(
        root.to_path_buf(),
        state_root.join(MANIFEST_ENGINE_DB_FILE),
        workspace_id,
    );
    let counters = engine.counters();
    let (sink, handle) = shared_engine_snapshot();
    let driver =
        ManifestDriver::spawn_with_sink(sink.clone(), handle.clone(), move |inbox, sink| {
            let transport = ProbeTransport::Shared(remote);
            let clock = SystemClock::default();
            run_engine_loop(engine, &transport, &transport, &clock, &inbox, &sink);
        })
        .expect("shared remote engine driver spawns");
    (driver, sink, handle, counters)
}

/// A freshly armed native adapter can report a startup cursor loss on its very
/// first boundary attempt (observed on darwin as `Loss(NonMonotonicCursor)`);
/// production absorbs that through the coordinator's retry loop. Settle the
/// adapter with one throwaway boundary so the dispositions and counts the seam
/// tests assert belong to the seam under test, not to platform startup noise.
fn settle_native_adapter(coverage: &mut SyncWatcherCoverageHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let wait = CoverageWait::new(
            Instant::now() + Duration::from_secs(10),
            CoverageCancellation::new(),
        );
        let sealed = coverage
            .begin_recovery(&wait)
            .and_then(|preparation| coverage.seal_after_scan(preparation, &wait));
        match sealed {
            Ok(_handoff) => return,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "native adapter never produced a settled boundary: {error}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Drive one incident to a terminal disposition through the production retry
/// policy: retryable dependency failures defer and re-run, exactly as the
/// bridge loop does around `recover_once`.
fn recover_to_completion(
    worker: &WatcherRecoveryWorker,
    coverage: &mut SyncWatcherCoverageHandle,
    engine: &EngineSnapshotHandle,
    clock: &Arc<RecoveryClock>,
    before_close: &mut impl FnMut() -> Result<(), WatcherRecoveryCoordinatorError>,
) -> Result<RecoveryWorkDisposition, WatcherRecoveryCoordinatorError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let hook_clock = Arc::clone(clock);
    let moment_source: Arc<dyn Fn() -> RecoveryMoment + Send + Sync> =
        Arc::new(move || hook_clock.now());
    loop {
        match worker.recover_once(
            coverage,
            engine,
            &moment_source,
            &|| false,
            &CoverageCancellation::new(),
            before_close,
        ) {
            Ok(RecoveryWorkDisposition::RetryRequired | RecoveryWorkDisposition::RetryDeferred) => {
                assert!(
                    Instant::now() < deadline,
                    "deferred recovery attempt never came due"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// Everything a direct `recover_once` test needs: a real native watch for
/// authentic coverage seals, a scan-servicing engine, and a startup-incident
/// coordinator whose worker the test claims itself.
struct RecoverOnceHarness {
    counters: Arc<EngineCounters>,
    coordinator: Arc<WatcherRecoveryCoordinator>,
    clock: Arc<RecoveryClock>,
    coverage: SyncWatcherCoverageHandle,
    engine: EngineSnapshotHandle,
    ingress: WatcherIngressHandle,
    temp: PathBuf,
    _watch: SyncWatcher,
    _native_signals: mpsc::Receiver<WatcherSignal>,
    _driver: ManifestDriver,
}

fn recover_once_harness(label: &str, workspace_id: &str) -> RecoverOnceHarness {
    recover_once_harness_with(label, workspace_id, None)
}

/// A harness whose engine can fire a hook the first time it uploads a blob --
/// inside a publication pass, and therefore strictly before the walk that the
/// registry dispatches after it.
fn recover_once_harness_with(
    label: &str,
    workspace_id: &str,
    on_first_blob: Option<Box<dyn FnOnce() + Send>>,
) -> RecoverOnceHarness {
    let temp = crate::daemon::tests::unique_temp_dir(label);
    let root = temp.join("Code");
    let state_root = temp.join("state");
    std::fs::create_dir_all(&root).expect("workspace root exists");
    std::fs::create_dir_all(&state_root).expect("state root exists");
    let ids = WatcherCoverageIds::new();
    let (watch, native_signals) =
        start_sync_watcher(&root, ids).expect("native watcher arms on a real root");
    let mut coverage = watch.coverage_handle();
    settle_native_adapter(&mut coverage);
    let clock = Arc::new(RecoveryClock::new());
    // These seam tests assert recovery ordering and closure fences, while the
    // backoff curve itself is covered by reducer tests. A real native adapter
    // can transiently reject preparation under a loaded macOS test run; using
    // the production five-second ceiling here turned that platform noise into
    // a near-thirty-second race against the helper deadline.
    let test_backoff = BackoffPolicy::new(Duration::from_millis(25), Duration::from_millis(250))
        .expect("test recovery backoff is valid");
    let coordinator = Arc::new(WatcherRecoveryCoordinator::startup_reconciliation(
        RecoverySourceIdentity::new(recovery_process_identity(), WorkspaceId::new(workspace_id)),
        clock.now(),
        test_backoff,
    ));
    let (driver, _sink, engine, counters) =
        empty_genesis_driver(&root, &state_root, workspace_id, on_first_blob);
    let ingress = driver.watcher_ingress();
    RecoverOnceHarness {
        counters,
        coordinator,
        clock,
        coverage,
        engine,
        ingress,
        temp,
        _watch: watch,
        _native_signals: native_signals,
        _driver: driver,
    }
}

fn content_hash_advances(counters: &EngineCounters, baseline: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if counters.content_hashes.load(Ordering::Acquire) > baseline {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::yield_now();
    }
}

// The release proof: after a burst latched the overflow request, recovery
// scanned, sealed and closed, but nothing cleared the latch, so the callback
// withheld every later write forever while status reported ready. This drives
// the production bridge — `recovery_inputs()` is `Some`, so `recover_once`
// installs `drain_pending_signals` as the close fence — and pins that the fence
// acknowledges the covered request. With the acknowledgement emptied out, the
// latch never clears and this fails at the first assert.
#[test]
fn production_recovery_acknowledges_covered_overflow_and_restores_sight() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-overflow-production-ack");
    let root = temp.join("Code");
    let state_root = temp.join("state");
    std::fs::create_dir_all(&root).expect("workspace root exists");
    std::fs::create_dir_all(&state_root).expect("state root exists");
    let workspace_id = "ws_overflow_production_ack";
    let mut sync =
        crate::daemon::tests::watcher_test_runtime(root.clone(), state_root.clone(), workspace_id);
    let coordinator = Arc::clone(&sync.recovery_coordinator);
    let ids = WatcherCoverageIds::new();
    let (watch, _native_signals) =
        start_sync_watcher(&root, ids.clone()).expect("native watcher arms on a real root");
    let (signal_tx, signal_rx) = mpsc::sync_channel::<WatcherSignal>(64);
    sync.watcher = crate::daemon::sync::WatcherHost::armed_with_native_watch(watch, signal_rx, ids);
    let (driver, sink, engine, _counters) =
        empty_genesis_driver(&root, &state_root, workspace_id, None);
    sync.manifest_snapshot = (sink, engine.clone());
    sync.manifest_engine = ManifestEngineHost::Active(driver);
    let mut runtime = DaemonRuntime {
        sync: Some(sync),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };

    // The burst: the callback latched the overflow request and the kernel's
    // lane signal is queued ahead of it, exactly the state the startup
    // incident's covering recovery must both cover and acknowledge.
    let lane = Arc::new(WatcherOverflowLane::default());
    signal_tx
        .send(WatcherSignal::OverflowLane(Arc::clone(&lane)))
        .expect("overflow lane signal queues");
    lane.request_recovery();

    let WatcherBridgeStart::Started(bridge) =
        WatcherBridge::start(&mut runtime).expect("bridge starts")
    else {
        panic!("bridge must take the production recovery path");
    };

    // Recovery must close AND clear the latch. A latch that outlives its
    // covering recovery is the release-proof blindness: the coordinator goes
    // nominal (or churns through backstop reopens) while the callback keeps
    // withholding every later write.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = coordinator
            .snapshot()
            .expect("recovery snapshot stays readable");
        if snapshot.lifecycle() == RecoveryLifecycle::Nominal && !lane.recovery_requested() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "covered overflow request was never acknowledged \
             (lifecycle {:?}, latch asserted: {}); the callback stays blind",
            snapshot.lifecycle(),
            lane.recovery_requested(),
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The write a user makes after the burst, delivered through the production
    // callback filter: while the latch is asserted this very call withholds
    // the change, so its arrival proves sight is restored end to end.
    let follow_up = root.join("after-burst.txt");
    std::fs::write(&follow_up, b"written after the burst").expect("follow-up edit lands");
    send_watcher_signal(
        &signal_tx,
        &lane,
        Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(follow_up)),
    );
    let follow_up_path = WorkspacePath::new("after-burst.txt");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if engine.current().dirty_paths.contains(&follow_up_path) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the post-burst write never reached the engine"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        coordinator
            .snapshot()
            .expect("recovery snapshot stays readable")
            .lifecycle(),
        RecoveryLifecycle::Nominal,
        "an acknowledged overflow must not leave recovery churning"
    );

    drop(signal_tx);
    drop(bridge);
    drop(runtime);
    let _ = std::fs::remove_dir_all(temp);
}

// Why the acknowledgement belongs inside the close fence rather than after
// closure: a write racing the fence advances the activity watermark, which must
// invalidate the first close offer so the covering scan is repeated. Acknowledging
// after closure has neither property — the raced write would be suppressed and
// observed by nobody. The closure receipt's attempt count is the observable
// evidence that the first `offer_close` returned `RetryRequired`.
#[test]
fn activity_racing_the_fence_invalidates_the_first_close_and_a_retry_closes() {
    let harness = recover_once_harness("watcher-overflow-close-race", "ws_overflow_close_race");
    let worker =
        WatcherRecoveryWorker::claim(Arc::clone(&harness.coordinator), harness.clock.now())
            .expect("recovery worker claims ownership");
    let lane = Arc::new(WatcherOverflowLane::default());
    lane.request_recovery();
    let fence_lane = Arc::clone(&lane);
    let fence_coordinator = Arc::clone(&harness.coordinator);
    let fence_clock = Arc::clone(&harness.clock);
    let raced = std::cell::Cell::new(false);
    let mut before_close = move || {
        let _covered = fence_lane.take_recovery_request();
        // The racing write: a suppressed write admitted while the fence is
        // still open. Forwarded activity deliberately no longer invalidates a
        // close -- only writes the callback dropped do.
        // Only the first attempt races; the retry must then close cleanly.
        if !raced.replace(true) {
            fence_coordinator.observe_suppressed(fence_clock.now())?;
        }
        Ok(())
    };
    let mut coverage = harness.coverage.clone();

    let disposition = recover_to_completion(
        &worker,
        &mut coverage,
        &harness.engine,
        &harness.clock,
        &mut before_close,
    )
    .expect("recovery completes despite the raced first close");

    assert_eq!(disposition, RecoveryWorkDisposition::Closed);
    assert!(
        !lane.recovery_requested(),
        "the fence acknowledged the latch"
    );
    let snapshot = harness
        .coordinator
        .snapshot()
        .expect("recovery snapshot stays readable");
    assert_eq!(snapshot.lifecycle(), RecoveryLifecycle::Nominal);
    let closure = snapshot
        .last_closure()
        .expect("a closed incident records its closure receipt");
    // Two authoritative scans, not one: the raced activity turned the first
    // close offer into RetryRequired, so the covering scan had to be repeated
    // before closure. Attempts are asserted as a floor because a retryable
    // pre-scan dependency failure may add attempts without adding scans.
    assert_eq!(
        closure.scan_count().get(),
        2,
        "the raced activity must invalidate the first close and force a rescan"
    );
    assert!(
        closure.attempt_count().get() >= 2,
        "closure must land on a later attempt than the raced one"
    );
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}

// A rejected close must return ownership to the bridge instead of chasing the
// next attempt inline. The bridge's forwarding tail is the only place queued
// native detail can leave the callback channel between attempts; without this
// return, a dense batch repeatedly collapses inside the next close fence and
// publication misses the recovery-edit SLO even though each scan is sound.
#[test]
fn a_rejected_close_returns_after_releasing_publication() {
    let harness = recover_once_harness(
        "watcher-overflow-single-attempt",
        "ws_overflow_single_attempt",
    );
    let worker =
        WatcherRecoveryWorker::claim(Arc::clone(&harness.coordinator), harness.clock.now())
            .expect("recovery worker claims ownership");
    let baseline = Arc::new(AtomicU64::new(0));
    let fence_baseline = Arc::clone(&baseline);
    let fence_counters = Arc::clone(&harness.counters);
    let fence_coordinator = Arc::clone(&harness.coordinator);
    let fence_clock = Arc::clone(&harness.clock);
    let fence_ingress = harness.ingress.clone();
    let fence_root = harness.temp.join("Code");
    let rejected = std::cell::Cell::new(false);
    let mut before_close = move || {
        if !rejected.replace(true) {
            fence_baseline.store(
                fence_counters.content_hashes.load(Ordering::Acquire),
                Ordering::Release,
            );
            std::fs::write(fence_root.join("between.txt"), b"between attempts")
                .expect("mid-incident write");
            let observation =
                fence_ingress.observe(EngineEvent::Paths(BTreeSet::from([WorkspacePath::new(
                    "between.txt",
                )])));
            assert_eq!(
                observation,
                bowline_daemon::manifest_driver::WatcherIngressObservation::Accumulated,
                "the inter-attempt publication probe enters the engine ingress"
            );
            fence_coordinator.observe_suppressed(fence_clock.now())?;
        }
        Ok(())
    };
    let hook_clock = Arc::clone(&harness.clock);
    let moment_source: Arc<dyn Fn() -> RecoveryMoment + Send + Sync> =
        Arc::new(move || hook_clock.now());
    let deadline = Instant::now() + Duration::from_secs(30);
    let first_disposition = loop {
        match worker
            .recover_once(
                &mut harness.coverage.clone(),
                &harness.engine,
                &moment_source,
                &|| false,
                &CoverageCancellation::new(),
                &mut before_close,
            )
            .expect("first recovery attempt is retryable")
        {
            RecoveryWorkDisposition::RetryDeferred => {
                assert!(
                    Instant::now() < deadline,
                    "first recovery attempt never reached its close fence"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            disposition => break disposition,
        }
    };

    assert_eq!(
        first_disposition,
        RecoveryWorkDisposition::RetryRequired,
        "a rejected close must return before starting the next attempt"
    );
    let publication_deadline = Instant::now() + Duration::from_secs(5);
    while harness.counters.content_hashes.load(Ordering::Acquire)
        <= baseline.load(Ordering::Acquire)
    {
        assert!(
            Instant::now() < publication_deadline,
            "the rejected attempt returned without releasing publication"
        );
        std::thread::yield_now();
    }

    let mut coverage = harness.coverage.clone();
    let disposition = recover_to_completion(
        &worker,
        &mut coverage,
        &harness.engine,
        &harness.clock,
        &mut before_close,
    )
    .expect("a fresh attempt closes after the forwarding opportunity");
    assert_eq!(disposition, RecoveryWorkDisposition::Closed);
    let snapshot = harness
        .coordinator
        .snapshot()
        .expect("recovery snapshot stays readable");
    let closure = snapshot
        .last_closure()
        .expect("the retry records its closure");
    assert!(
        closure.scan_count().get() >= 2,
        "the rejected close must still require a fresh covering scan"
    );
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}

#[path = "watcher_recovery_early_release_tests.rs"]
mod early_release_tests;

// The fence fails closed: a `before_close` error means the incident never
// accounted for what the fence was supposed to admit, so it must not authorise
// closure. Before the fix the fence's two loss admissions were discarded with
// `let _ =`, which would have closed an incident that never recorded its loss.
#[test]
fn a_failing_close_fence_returns_the_error_and_leaves_the_incident_open() {
    let harness = recover_once_harness("watcher-overflow-fence-error", "ws_overflow_fence_error");
    let worker =
        WatcherRecoveryWorker::claim(Arc::clone(&harness.coordinator), harness.clock.now())
            .expect("recovery worker claims ownership");
    let mut before_close = || Err(WatcherRecoveryCoordinatorError::StateUnavailable);
    let mut coverage = harness.coverage.clone();

    let result = recover_to_completion(
        &worker,
        &mut coverage,
        &harness.engine,
        &harness.clock,
        &mut before_close,
    );

    assert!(
        matches!(
            result,
            Err(WatcherRecoveryCoordinatorError::StateUnavailable)
        ),
        "a fence failure must propagate, not authorise closure: {result:?}"
    );
    let snapshot = harness
        .coordinator
        .snapshot()
        .expect("recovery snapshot stays readable");
    assert_eq!(
        snapshot.lifecycle(),
        RecoveryLifecycle::Recovering,
        "a failed fence must not leave the incident nominal"
    );
    assert!(
        snapshot.last_closure().is_none(),
        "no closure receipt may exist when the fence failed"
    );
    assert_eq!(
        snapshot.phase(),
        Some(RecoveryPhase::Closing),
        "the attempt stays where it was fenced, ready for a later retry"
    );
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}
