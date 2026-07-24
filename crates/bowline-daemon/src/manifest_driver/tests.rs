use std::time::{Duration, Instant};

use bowline_core::ids::DeviceId;
use bowline_local::sync::manifest_engine::{
    BlobKey, BlobReaderUpload, BlobUpload, CasOutcome, EngineConfig, EngineContext, EngineCounters,
    EnginePhase, KeyEpoch, ManifestKey, ManifestStore, ManifestUpload, RefObservation,
    RemoteObjects, RemoteRef, TransportError, WorkspaceCrypto,
};

use super::*;

/// A transport for an empty genesis workspace: the ref is absent, so the engine
/// pulls nothing, has no dirty paths to push, and settles into `Idle`.
struct EmptyGenesisTransport;

impl RemoteObjects for EmptyGenesisTransport {
    fn put_blob(&self, _upload: BlobUpload<'_>) -> Result<(), TransportError> {
        Ok(())
    }
    fn put_blob_reader(&self, _upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        Ok(())
    }
    fn put_manifest(&self, _upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        Ok(())
    }
    fn get_blob(&self, _key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        Err(TransportError::new(
            "get-blob",
            "empty transport".to_string(),
        ))
    }
    fn get_manifest(&self, _key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        Err(TransportError::new(
            "get-manifest",
            "empty transport".to_string(),
        ))
    }
}

impl RemoteRef for EmptyGenesisTransport {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        Ok(None)
    }
    fn compare_and_swap(
        &self,
        _expected_version: Option<u64>,
        _new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        Ok(CasOutcome::Ambiguous)
    }
}

fn test_engine(root: std::path::PathBuf, store_path: std::path::PathBuf) -> ManifestEngine {
    let store = ManifestStore::open(store_path).expect("engine store opens");
    let ctx = EngineContext {
        crypto: WorkspaceCrypto::new("ws_code", [7_u8; 32], KeyEpoch::new(1)),
        device_id: DeviceId::new("device-a"),
        engine_state_dir: root.join(bowline_local::sync::manifest_engine::ENGINE_STATE_DIR),
        workspace_root: root,
        config: EngineConfig::default(),
        project_view: false,
        counters: EngineCounters::shared(),
    };
    ManifestEngine::new(store, ctx)
}

#[test]
fn driver_reaches_idle_on_an_empty_genesis_workspace() {
    let temp = std::env::temp_dir().join(format!(
        "bowline-manifest-driver-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let root = temp.join("Code");
    std::fs::create_dir_all(&root).expect("workspace root");
    let store_path = temp.join(MANIFEST_ENGINE_DB_FILE);
    let engine = test_engine(root, store_path);

    let driver = ManifestDriver::spawn(move |inbox, sink| {
        let transport = EmptyGenesisTransport;
        let clock = SystemClock::default();
        run_engine_loop(engine, &transport, &transport, &clock, &inbox, &sink);
    })
    .expect("driver spawns");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = driver.snapshot();
        if snapshot.phase == EnginePhase::Idle {
            assert_eq!(snapshot.dirty, 0);
            assert_eq!(snapshot.pending_intents, 0);
            assert_eq!(snapshot.degradation, Degradation::Nominal);
            // The engine snapshot maps to a settled v8 status: this is the same
            // ready/reasons/queue shape `bowline sync wait` settles against.
            let facts = crate::status_projection::engine_convergence_facts(&snapshot);
            assert!(facts.ready);
            assert!(facts.summary.reasons.is_empty());
            assert!(!facts.queue.has_pending_work());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "engine never reached Idle; last phase {:?}",
            snapshot.phase
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let barrier = driver
        .snapshot_handle()
        .request_sync_barrier()
        .expect("active driver accepts a barrier");
    let completed = barrier
        .wait(Duration::from_secs(5))
        .expect("engine cycle wakes the exact barrier waiter");
    assert_eq!(completed.phase, EnginePhase::Idle);
    assert_eq!(completed.degradation, Degradation::Nominal);

    drop(driver);
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn timed_out_sync_barrier_unregisters_its_pending_sender() {
    let driver = ManifestDriver::spawn(|inbox, _sink| {
        let _ = inbox.recv();
        std::thread::sleep(Duration::from_millis(50));
    })
    .expect("driver spawns");
    let waiter = driver
        .snapshot_handle()
        .request_sync_barrier()
        .expect("active driver accepts a barrier");

    let error = waiter
        .wait(Duration::from_millis(1))
        .expect_err("barrier times out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(
        driver
            .barrier_pending
            .lock()
            .expect("pending barriers lock")
            .is_empty()
    );
}
