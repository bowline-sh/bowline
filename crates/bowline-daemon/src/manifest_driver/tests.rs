use crate::manifest_driver::{
    EngineConvergenceBarrierError, EngineSnapshotHandle, MANIFEST_ENGINE_DB_FILE,
    MANIFEST_ENGINE_INBOX_CAPACITY, ManifestDriver, SyncBarrierError, run_engine_loop,
    run_engine_loop_with_scan_executor, shared_engine_snapshot,
};
use crate::manifest_transport::{
    RefObserverAuthoritySource, RefObserverEndpointGeneration, RefObserverFailure,
    RefObserverFailureCode, RefObserverFailureStage, RefObserverSnapshotHandle, RefObserverState,
    VerifiedWorkspaceRef, VerifiedWorkspaceRefView,
};
use bowline_control_plane::DependencyFailureClass;
use bowline_local::sync::manifest_engine::Degradation;
use bowline_local::sync::manifest_engine::SystemClock;
use std::time::{Duration, Instant};

use bowline_core::ids::WorkspaceId;
use bowline_local::sync::manifest_engine::{
    EngineEvent, EngineIo, EnginePhase, EngineProcessIdentity, ManifestKey, RefObservation,
};
use std::sync::{Arc, Mutex, mpsc};

use bowline_local::sync::manifest_engine::empty_genesis::{
    EmptyGenesisTransport, empty_genesis_engine, empty_genesis_engine_context,
};

/// The fixed workspace identity every empty-genesis fixture in this file uses.
const GENESIS_WORKSPACE_ID: &str = "ws_code";

#[test]
fn manifest_driver_bounds_pending_engine_events() {
    let (release_tx, release_rx) = mpsc::channel();
    let driver = ManifestDriver::spawn(move |inbox, _sink| {
        release_rx.recv().expect("release bounded engine inbox");
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("bounded driver spawns");
    let events = driver.event_sender();

    for _ in 0..MANIFEST_ENGINE_INBOX_CAPACITY {
        events
            .try_send(EngineEvent::RefChanged)
            .expect("declared inbox capacity accepts one pending wake");
    }
    assert!(matches!(
        events.try_send(EngineEvent::RefChanged),
        Err(crossbeam_channel::TrySendError::Full(
            EngineEvent::RefChanged
        ))
    ));

    release_tx.send(()).expect("release engine consumer");
    drop(driver);
}

#[test]
fn engine_barrier_admission_cannot_wait_behind_a_stalled_wake() {
    let (release_tx, release_rx) = mpsc::channel();
    let driver = ManifestDriver::spawn(move |inbox, _sink| {
        release_rx.recv().expect("release stalled engine wake");
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("bounded driver spawns");
    driver
        .event_sender()
        .try_send(EngineEvent::RefChanged)
        .expect("one pending engine wake is admitted");

    let started_at = Instant::now();
    let waiter = driver
        .snapshot_handle()
        .request_engine_convergence_barrier()
        .expect("independent control registry admits the barrier");
    drop(waiter);
    assert!(
        started_at.elapsed() < Duration::from_secs(1),
        "barrier admission must not bypass the caller's recovery budget"
    );

    release_tx.send(()).expect("release stalled engine wake");
    drop(driver);
}

#[test]
fn sync_barrier_reads_live_observer_health_before_and_during_convergence() {
    let (sink, handle) = shared_engine_snapshot();
    let driver = ManifestDriver::spawn_with_sink_observer_requirement(
        sink.clone(),
        handle.clone(),
        true,
        |inbox, _sink| {
            while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
        },
    )
    .expect("observer-gated driver spawns");

    assert!(matches!(
        handle.request_sync_barrier(),
        Err(SyncBarrierError::Unavailable { .. })
    ));

    let health = RefObserverSnapshotHandle::for_endpoint(RefObserverEndpointGeneration::new(
        driver.endpoint_generation.0,
    ));
    health.transition(RefObserverState::Live, 0, false, None);
    sink.attach_ref_observer_snapshot(driver.endpoint_generation, health.clone());
    let waiter = handle
        .request_sync_barrier()
        .expect("a live observer admits an exact barrier");

    health.transition(RefObserverState::Retrying, 1, false, None);
    assert!(matches!(
        waiter.wait(Duration::from_secs(1), || false),
        Err(SyncBarrierError::Unavailable { .. })
    ));

    health.transition(
        RefObserverState::Blocked,
        1,
        false,
        Some(RefObserverFailure {
            stage: RefObserverFailureStage::Authentication,
            class: DependencyFailureClass::AuthenticationRequired,
            code: RefObserverFailureCode::AuthenticationRequired,
        }),
    );
    assert!(matches!(
        handle.request_sync_barrier(),
        Err(SyncBarrierError::ObserverBlocked {
            class: DependencyFailureClass::AuthenticationRequired,
            code: RefObserverFailureCode::AuthenticationRequired,
        })
    ));

    drop(driver);
}

fn observer_gated_genesis_driver() -> (
    ManifestDriver,
    EngineSnapshotHandle,
    RefObserverSnapshotHandle,
    mpsc::Sender<()>,
    std::path::PathBuf,
) {
    static NEXT_OBSERVER_TEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let temp = std::env::temp_dir().join(format!(
        "bowline-observer-barrier-{}-{}",
        std::process::id(),
        NEXT_OBSERVER_TEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let root = temp.join("Code");
    std::fs::create_dir_all(&root).expect("observer barrier workspace");
    let mut engine = empty_genesis_engine(
        root,
        temp.join(MANIFEST_ENGINE_DB_FILE),
        GENESIS_WORKSPACE_ID,
    );
    let (sink, handle) = shared_engine_snapshot();
    let (release_tx, release_rx) = mpsc::channel();
    let driver = ManifestDriver::spawn_with_sink_observer_requirement(
        sink.clone(),
        handle.clone(),
        true,
        move |inbox, sink| {
            let transport = EmptyGenesisTransport;
            let clock = SystemClock::default();
            let io = EngineIo {
                objects: &transport,
                refs: &transport,
                clock: &clock,
            };
            engine.start(&io).expect("test engine starts");
            sink.publish(engine.snapshot());
            let Some(controls) = sink.control_registry() else {
                return;
            };
            let control_wake = controls.wake_receiver();
            loop {
                let event = crossbeam_channel::select! {
                    recv(control_wake) -> _ => {
                        let mut controls = controls.drain_barriers();
                        match controls.pop() {
                            Some(event) => event,
                            None => continue,
                        }
                    }
                    recv(inbox) -> event => match event {
                        Ok(event) => event,
                        Err(_) => break,
                    },
                };
                if matches!(event, EngineEvent::EngineConvergenceBarrier { .. }) {
                    release_rx.recv().expect("release gated barrier");
                }
                if matches!(event, EngineEvent::Shutdown) {
                    break;
                }
                engine.on_event(event, &clock);
                if engine.announce_due_work(&clock) {
                    sink.publish(engine.snapshot());
                }
                engine.run_due_work(&io).expect("test engine cycle");
                sink.publish(engine.snapshot());
                sink.complete_barriers(engine.take_completed_barriers());
            }
        },
    )
    .expect("observer-gated driver starts");
    let authority = RefObserverAuthoritySource::issue(
        EngineProcessIdentity::current(),
        WorkspaceId::new(GENESIS_WORKSPACE_ID),
        RefObserverEndpointGeneration::new(driver.endpoint_generation.0),
    );
    let observer = RefObserverSnapshotHandle::for_source(authority);
    observer.live_with_ref(VerifiedWorkspaceRef::genesis());
    sink.attach_ref_observer_snapshot(driver.endpoint_generation, observer.clone());
    (driver, handle, observer, release_tx, temp)
}

#[test]
fn exact_barrier_rejects_a_live_frontier_that_changed_after_admission() {
    let (driver, handle, observer, release, temp) = observer_gated_genesis_driver();
    let waiter = handle
        .request_sync_barrier()
        .expect("exact frontier admits barrier");

    observer.live_with_ref(VerifiedWorkspaceRef::genesis());
    release.send(()).expect("release barrier completion");
    assert!(matches!(
        waiter.wait(Duration::from_secs(1), || false),
        Err(SyncBarrierError::Unavailable {
            reason: "remote manifest observer frontier changed during convergence"
        })
    ));
    drop(driver);
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn exact_barrier_rejects_engine_state_that_does_not_match_the_observer_ref() {
    let (driver, handle, observer, release, temp) = observer_gated_genesis_driver();
    observer.live_with_ref(VerifiedWorkspaceRef::from_observation(RefObservation {
        version: 7,
        manifest_key: ManifestKey::new("m_different_observer_frontier"),
    }));
    let waiter = handle
        .request_sync_barrier()
        .expect("live observer admits barrier");

    release.send(()).expect("release barrier completion");
    assert!(matches!(
        waiter.wait(Duration::from_secs(1), || false),
        Err(SyncBarrierError::Unavailable {
            reason: "engine convergence does not match the exact observer frontier"
        })
    ));
    drop(driver);
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn exact_barrier_receipt_binds_engine_and_observer_frontiers() {
    let (driver, handle, _observer, release, temp) = observer_gated_genesis_driver();
    let waiter = handle
        .request_sync_barrier()
        .expect("exact frontier admits barrier");

    release.send(()).expect("release barrier completion");
    let receipt = waiter
        .wait(Duration::from_secs(1), || false)
        .expect("matching exact frontiers complete");
    let frontier = receipt.observer_frontier().expect("observer receipt");
    assert_eq!(
        frontier.authority_source.workspace_identity(),
        receipt.engine().workspace_identity()
    );
    assert_eq!(
        frontier.authority_source.endpoint_generation().get(),
        receipt.engine().endpoint_generation().0
    );
    assert_eq!(
        frontier.verified_ref.view(),
        VerifiedWorkspaceRefView::Genesis
    );
    drop(driver);
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn production_context_rejects_crypto_for_another_workspace_identity() {
    let temp = std::env::temp_dir().join(format!(
        "bowline-manifest-driver-identity-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&temp).expect("identity test root");
    let mut context = empty_genesis_engine_context(temp.clone(), GENESIS_WORKSPACE_ID);
    super::validate_engine_context_identity(&context).expect("matching identity");

    context.workspace_identity = WorkspaceId::new("ws_other");
    let error = super::validate_engine_context_identity(&context)
        .expect_err("mismatched crypto and workspace identity must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

    let _ = std::fs::remove_dir_all(temp);
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
    let engine = empty_genesis_engine(root, store_path, GENESIS_WORKSPACE_ID);

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
        .wait(Duration::from_secs(5), || false)
        .expect("engine cycle wakes the exact barrier waiter");
    assert_eq!(
        completed.engine().observed_ref(),
        completed.engine().applied_ref()
    );
    assert!(completed.observer_frontier().is_none());

    let scan_lease = driver
        .snapshot_handle()
        .request_coverage_scan(None)
        .expect("coverage control has its dedicated slot")
        .wait(Duration::from_secs(5), || false)
        .expect("authoritative local scan completes independently");
    assert!(scan_lease.receipt().revision().get() > 0);
    driver
        .snapshot_handle()
        .request_engine_convergence_barrier()
        .expect("convergence remains available while the scan lease is held")
        .wait(Duration::from_secs(5), || false)
        .expect("a scan lease never suspends ordinary convergence");
    scan_lease.release();

    let receipt = driver
        .snapshot_handle()
        .request_engine_convergence_barrier()
        .expect("engine barrier admits independently")
        .wait(Duration::from_secs(5), || false)
        .expect("engine barrier returns its typed receipt");
    assert_eq!(receipt.endpoint_generation(), driver.endpoint_generation);
    assert_eq!(
        receipt.process_identity(),
        &EngineProcessIdentity::current()
    );
    assert_eq!(
        receipt.workspace_identity(),
        &WorkspaceId::new(GENESIS_WORKSPACE_ID)
    );
    assert_eq!(
        receipt.observed_ref(),
        &bowline_local::sync::manifest_engine::EngineRef::Genesis
    );
    assert_eq!(receipt.observed_ref(), receipt.applied_ref());
    assert_eq!(receipt.materialization_revision().get(), 0);

    drop(driver);
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn scan_in_progress_does_not_block_watcher_publication() {
    let temp = std::env::temp_dir().join(format!(
        "bowline-manifest-driver-live-scan-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let root = temp.join("Code");
    std::fs::create_dir_all(&root).expect("workspace root");
    let engine = empty_genesis_engine(
        root.clone(),
        temp.join(MANIFEST_ENGINE_DB_FILE),
        GENESIS_WORKSPACE_ID,
    );
    let counters = engine.counters();
    let (scan_entered_tx, scan_entered_rx) = mpsc::channel();
    let (release_scan_tx, release_scan_rx) = mpsc::channel();
    let release_scan_rx = Arc::new(Mutex::new(release_scan_rx));

    let driver = ManifestDriver::spawn(move |inbox, sink| {
        let transport = EmptyGenesisTransport;
        let clock = SystemClock::default();
        let release_scan_rx = Arc::clone(&release_scan_rx);
        let executor = Arc::new(move |plan: super::AuthoritativeScanPlan| {
            scan_entered_tx.send(()).expect("report scan entry");
            release_scan_rx
                .lock()
                .expect("scan gate lock")
                .recv()
                .expect("release scan walk");
            plan.execute()
        });
        run_engine_loop_with_scan_executor(
            engine, &transport, &transport, &clock, &inbox, &sink, executor,
        );
    })
    .expect("driver spawns");

    let scan_waiter = driver
        .snapshot_handle()
        .request_coverage_scan(None)
        .expect("coverage scan admits");
    scan_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("filesystem walk starts");

    std::fs::write(root.join("sentinel.txt"), b"published during scan").expect("write sentinel");
    assert_eq!(
        driver.watcher_ingress().observe(EngineEvent::Paths(
            [bowline_local::sync::manifest_engine::WorkspacePath::new(
                "sentinel.txt"
            )]
            .into_iter()
            .collect(),
        )),
        super::WatcherIngressObservation::Accumulated
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while counters
        .content_hashes
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        assert!(
            Instant::now() < deadline,
            "watcher edit did not enter a publication pass while the scan was blocked"
        );
        std::thread::yield_now();
    }

    release_scan_tx.send(()).expect("release scan");
    scan_waiter
        .wait(Duration::from_secs(5), || false)
        .expect("scan completes after publication")
        .release();
    drop(driver);
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn internal_barrier_admission_does_not_depend_on_observer_readiness() {
    let (sink, handle) = shared_engine_snapshot();
    let driver = ManifestDriver::spawn_with_sink_observer_requirement(
        sink,
        handle.clone(),
        true,
        |inbox, _sink| while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {},
    )
    .expect("observer-required engine starts");

    let waiter = handle
        .request_engine_convergence_barrier()
        .expect("engine-only barrier ignores observer startup");
    assert_eq!(
        waiter
            .wait(Duration::from_secs(1), || true)
            .expect_err("cancelled test barrier"),
        EngineConvergenceBarrierError::Cancelled
    );
    drop(driver);
}

#[test]
fn cancelled_barrier_waiters_withdraw_their_engine_requests() {
    let (sink, handle) = shared_engine_snapshot();
    let driver = ManifestDriver::spawn_with_sink(sink, handle.clone(), move |inbox, _sink| {
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("engine endpoint starts");

    for _ in 0..64 {
        let waiter = handle
            .request_engine_convergence_barrier()
            .expect("barrier is admitted");
        assert_eq!(
            waiter
                .wait(Duration::ZERO, || false)
                .expect_err("zero-duration barrier times out"),
            EngineConvergenceBarrierError::TimedOut
        );
    }
    let next = handle
        .request_engine_convergence_barrier()
        .expect("cancelled waiters immediately release control capacity");
    drop(next);
    assert!(
        driver
            .barrier_pending
            .lock()
            .expect("barrier registry")
            .is_empty()
    );
}

#[test]
fn endpoint_replacement_fences_old_publication_and_disconnects_its_waiter() {
    let (sink, handle) = shared_engine_snapshot();
    let (release_stale, stale_gate) = std::sync::mpsc::channel();
    let old = ManifestDriver::spawn_with_sink(sink.clone(), handle.clone(), move |inbox, sink| {
        let _event = inbox.recv();
        let _released = stale_gate.recv();
        let mut stale = super::starting_snapshot();
        stale.revision = 777;
        sink.publish(stale);
    })
    .expect("old endpoint");
    let waiter = old
        .snapshot_handle()
        .request_engine_convergence_barrier()
        .expect("old barrier admitted");

    let replacement = ManifestDriver::spawn_with_sink(sink, handle.clone(), |inbox, _sink| {
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("replacement endpoint");
    assert_ne!(old.endpoint_generation, replacement.endpoint_generation);
    release_stale.send(()).expect("release stale publisher");
    assert_eq!(
        waiter
            .wait(Duration::from_secs(1), || false)
            .expect_err("replacement disconnects old completion lane"),
        EngineConvergenceBarrierError::EngineStopped
    );
    std::thread::sleep(Duration::from_millis(20));
    assert_ne!(handle.current().revision, 777, "stale sink cannot publish");

    drop(old);
    drop(replacement);
}

#[test]
fn generation_bound_sink_cannot_take_over_a_replacement_endpoint() {
    let (sink, handle) = shared_engine_snapshot();
    let old = ManifestDriver::spawn_with_sink(sink.clone(), handle.clone(), |inbox, _sink| {
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("old endpoint");
    let stale_sink = sink.for_generation(old.endpoint_generation);
    let replacement = ManifestDriver::spawn_with_sink(sink, handle.clone(), |inbox, _sink| {
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("replacement endpoint");

    stale_sink.take_over_with_host_status(super::host_status_snapshot());
    assert_eq!(handle.current().phase, EnginePhase::Starting);
    let waiter = handle
        .request_engine_convergence_barrier()
        .expect("replacement endpoint remains registered");
    assert_eq!(waiter.generation, replacement.endpoint_generation);

    drop(waiter);
    drop(old);
    drop(replacement);
}

#[test]
fn host_takeover_is_atomic_and_survives_old_driver_drop() {
    let (sink, handle) = shared_engine_snapshot();
    let driver = ManifestDriver::spawn_with_sink(sink.clone(), handle.clone(), |inbox, _sink| {
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("driver");
    let stale_sink = sink.for_generation(driver.endpoint_generation);
    let waiter = handle
        .request_engine_convergence_barrier()
        .expect("old endpoint barrier");

    sink.take_over_with_host_status(super::host_status_snapshot());
    assert_eq!(handle.current().phase, EnginePhase::Stopped);
    assert_eq!(
        waiter
            .wait(Duration::from_secs(1), || false)
            .expect_err("takeover disconnects endpoint waiters"),
        EngineConvergenceBarrierError::EngineStopped
    );

    let mut stale = super::starting_snapshot();
    stale.revision = 991;
    stale_sink.publish(stale);
    assert_ne!(handle.current().revision, 991);
    drop(driver);
    assert_eq!(handle.current().revision, super::HOST_STATUS_REVISION);
}

#[test]
fn barrier_and_endpoint_identity_exhaustion_never_wraps() {
    let (sink, handle) = shared_engine_snapshot();
    let driver = ManifestDriver::spawn_with_sink(sink.clone(), handle.clone(), |inbox, _sink| {
        while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
    })
    .expect("driver");
    handle
        .0
        .next_barrier_id
        .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        handle.request_engine_convergence_barrier(),
        Err(EngineConvergenceBarrierError::IdentityExhausted)
    ));
    drop(driver);

    sink.shared
        .next_generation
        .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    let error = match ManifestDriver::spawn_with_sink(sink, handle, |_inbox, _sink| {}) {
        Ok(_) => panic!("endpoint generation exhaustion must be terminal"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("generation exhausted"));
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
        .wait(Duration::from_millis(1), || false)
        .expect_err("barrier times out");
    assert_eq!(error, SyncBarrierError::TimedOut);
    assert!(
        driver
            .barrier_pending
            .lock()
            .expect("pending barriers lock")
            .is_empty()
    );
}

#[test]
fn a_cancelled_sync_barrier_releases_its_worker_before_the_timeout() {
    let driver =
        ManifestDriver::spawn(
            |inbox, _sink| {
                while !matches!(inbox.recv(), Ok(EngineEvent::Shutdown) | Err(_)) {}
            },
        )
        .expect("driver spawns");
    let waiter = driver
        .snapshot_handle()
        .request_sync_barrier()
        .expect("active driver accepts a barrier");

    // A blocking wait would park this thread for the full hour-scale timeout the
    // RPC surface used to accept; the cancellation predicate must cut it short.
    let started = Instant::now();
    let error = waiter
        .wait(Duration::from_secs(3600), || true)
        .expect_err("a cancelled barrier does not converge");
    assert_eq!(error, SyncBarrierError::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation must release the worker immediately"
    );
}
