use super::*;

// Recovery attempts remain atomically fenced by scan revision, native boundary,
// and loss watermark even though distributed work resumes after each scan.
// Holding the engine across attempts meant a close that kept being invalidated
// by continuing activity withheld publication for the whole chase.
#[test]
fn publication_is_not_withheld_between_recovery_attempts() {
    let _serial = serialize_early_release_test();
    let harness = recover_once_harness("watcher-overflow-publish-gap", "ws_overflow_publish_gap");
    std::fs::write(harness.temp.join("Code").join("pending.txt"), b"pending")
        .expect("workspace write");
    let worker =
        WatcherRecoveryWorker::claim(Arc::clone(&harness.coordinator), harness.clock.now())
            .expect("recovery worker claims ownership");
    let lane = Arc::new(WatcherOverflowLane::default());
    lane.request_recovery();
    let fence_lane = Arc::clone(&lane);
    let fence_coordinator = Arc::clone(&harness.coordinator);
    let fence_clock = Arc::clone(&harness.clock);
    let fence_counters = Arc::clone(&harness.counters);
    let fence_root = harness.temp.join("Code");
    let raced = std::cell::Cell::new(false);
    let observed_between = Arc::new(AtomicBool::new(false));
    let fence_observed = Arc::clone(&observed_between);
    let baseline = std::cell::Cell::new(0u64);
    let mut before_close = move || {
        let _covered = fence_lane.take_recovery_request();
        if !raced.replace(true) {
            baseline.set(fence_counters.content_hashes.load(Ordering::Relaxed));
            std::fs::write(fence_root.join("between.txt"), b"written mid-incident")
                .expect("mid-incident write");
            fence_coordinator.observe_suppressed(fence_clock.now())?;
            return Ok(());
        }
        if fence_counters.content_hashes.load(Ordering::Relaxed) > baseline.get() {
            fence_observed.store(true, Ordering::Relaxed);
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
    .expect("recovery completes");
    assert_eq!(disposition, RecoveryWorkDisposition::Closed);
    assert!(
        observed_between.load(Ordering::Relaxed),
        "the engine published nothing between recovery attempts"
    );
    let snapshot = harness
        .coordinator
        .snapshot()
        .expect("recovery snapshot stays readable");
    let closure = snapshot
        .last_closure()
        .expect("a closed incident records its closure receipt");
    assert!(
        closure.scan_count().get() >= 2,
        "the raced activity must still invalidate the first close"
    );
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}

#[test]
fn publication_is_not_withheld_while_native_coverage_seals() {
    let _serial = serialize_early_release_test();
    let (published_tx, published_rx) = mpsc::sync_channel(1);
    let harness = recover_once_harness_with(
        "watcher-overflow-seal-publication-gap",
        "ws_overflow_seal_publication_gap",
        Some(Box::new(move || {
            let _sent = published_tx.send(());
        })),
    );
    std::fs::write(
        harness.temp.join("Code").join("publish-before-close.txt"),
        b"publish before native close",
    )
    .expect("workspace write");
    let worker =
        WatcherRecoveryWorker::claim(Arc::clone(&harness.coordinator), harness.clock.now())
            .expect("recovery worker claims ownership");
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mut coverage = GateSeal {
        inner: harness.coverage.clone(),
        gated: false,
        fail_after_gate: false,
        entered: entered_tx,
        release: release_rx,
    };
    let engine = harness.engine.clone();
    let clock = Arc::clone(&harness.clock);
    let recovery = std::thread::spawn(move || {
        let moment_clock = Arc::clone(&clock);
        let moment_source: Arc<dyn Fn() -> RecoveryMoment + Send + Sync> =
            Arc::new(move || moment_clock.now());
        loop {
            match worker.recover_once(
                &mut coverage,
                &engine,
                &moment_source,
                &|| false,
                &CoverageCancellation::new(),
                &mut || Ok(()),
            ) {
                Ok(
                    RecoveryWorkDisposition::RetryRequired | RecoveryWorkDisposition::RetryDeferred,
                ) => std::thread::sleep(Duration::from_millis(50)),
                outcome => break outcome,
            }
        }
    });
    if let Err(error) = entered_rx.recv_timeout(Duration::from_secs(10)) {
        drop(release_tx);
        panic!(
            "native seal starts after the authoritative scan: {error}; recovery result: {:?}",
            recovery.join()
        );
    }
    assert_eq!(
        harness
            .coordinator
            .snapshot()
            .expect("recovery snapshot stays readable")
            .lifecycle(),
        RecoveryLifecycle::Recovering
    );
    assert!(harness.coordinator.capture_nominal_frontier().is_err());
    let published_while_sealing = published_rx.recv_timeout(Duration::from_secs(10)).is_ok();
    release_tx.send(()).expect("native seal resumes");
    let disposition = recovery
        .join()
        .expect("recovery worker joins")
        .expect("recovery completes");
    assert_eq!(disposition, RecoveryWorkDisposition::Closed);
    assert!(
        published_while_sealing,
        "the scan-proven edit stayed withheld for the entire native seal"
    );
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}

#[test]
fn peer_materializes_remote_bytes_while_native_coverage_seals() {
    let _serial = serialize_early_release_test();
    let temp = crate::daemon::tests::unique_temp_dir("watcher-overflow-peer-seal-apply");
    let peer_root = temp.join("peer").join("Code");
    let peer_state = temp.join("peer").join("state");
    let source_root = temp.join("source").join("Code");
    let source_state = temp.join("source").join("state");
    for path in [&peer_root, &peer_state, &source_root, &source_state] {
        std::fs::create_dir_all(path).expect("test root exists");
    }
    let workspace_id = "ws_overflow_peer_seal_apply";
    let remote = SharedRemote::default();
    let (peer_driver, _peer_sink, peer_engine, _peer_counters) =
        shared_remote_driver(&peer_root, &peer_state, workspace_id, remote.clone());
    let peer_ready_deadline = Instant::now() + Duration::from_secs(10);
    while !peer_engine.current().is_exactly_converged() {
        assert!(
            Instant::now() < peer_ready_deadline,
            "peer engine never settles before recovery"
        );
        std::thread::yield_now();
    }
    let sentinel = source_root.join("sentinel.txt");
    let sentinel_bytes = b"remote bytes apply before native close";
    std::fs::write(&sentinel, sentinel_bytes).expect("source sentinel exists");
    let (source_driver, _source_sink, _source_engine, _source_counters) =
        shared_remote_driver(&source_root, &source_state, workspace_id, remote.clone());
    let source_deadline = Instant::now() + Duration::from_secs(10);
    while remote.head().is_none() {
        assert!(
            Instant::now() < source_deadline,
            "source never publishes the sentinel manifest"
        );
        std::thread::yield_now();
    }
    drop(source_driver);
    let ids = WatcherCoverageIds::new();
    let (watch, _native_signals) =
        start_sync_watcher(&peer_root, ids).expect("peer native watcher arms");
    let mut settled = watch.coverage_handle();
    settle_native_adapter(&mut settled);
    let clock = Arc::new(RecoveryClock::new());
    let coordinator = Arc::new(WatcherRecoveryCoordinator::startup_reconciliation(
        RecoverySourceIdentity::new(recovery_process_identity(), WorkspaceId::new(workspace_id)),
        clock.now(),
        BackoffPolicy::new(Duration::from_millis(25), Duration::from_millis(250))
            .expect("test recovery backoff is valid"),
    ));
    let worker = WatcherRecoveryWorker::claim(Arc::clone(&coordinator), clock.now())
        .expect("peer recovery worker claims ownership");
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mut coverage = GateSeal {
        inner: watch.coverage_handle(),
        gated: false,
        fail_after_gate: false,
        entered: entered_tx,
        release: release_rx,
    };
    let recovery_engine = peer_engine.clone();
    let recovery_clock = Arc::clone(&clock);
    let recovery = std::thread::spawn(move || {
        let moment_clock = Arc::clone(&recovery_clock);
        let moment_source: Arc<dyn Fn() -> RecoveryMoment + Send + Sync> =
            Arc::new(move || moment_clock.now());
        loop {
            match worker.recover_once(
                &mut coverage,
                &recovery_engine,
                &moment_source,
                &|| false,
                &CoverageCancellation::new(),
                &mut || Ok(()),
            ) {
                Ok(
                    RecoveryWorkDisposition::RetryRequired | RecoveryWorkDisposition::RetryDeferred,
                ) => std::thread::sleep(Duration::from_millis(50)),
                outcome => break outcome,
            }
        }
    });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("peer native seal is gated after its scan");
    remote.expose_ref();
    peer_driver.send(EngineEvent::RefChanged);
    let peer_sentinel = peer_root.join("sentinel.txt");
    let materialize_deadline = Instant::now() + Duration::from_secs(5);
    let materialized_while_sealing = loop {
        if std::fs::read(&peer_sentinel).ok().as_deref() == Some(sentinel_bytes) {
            break true;
        }
        if Instant::now() >= materialize_deadline {
            break false;
        }
        std::thread::yield_now();
    };
    assert_eq!(
        coordinator
            .snapshot()
            .expect("peer recovery snapshot stays readable")
            .lifecycle(),
        RecoveryLifecycle::Recovering
    );
    assert!(coordinator.capture_nominal_frontier().is_err());
    release_tx.send(()).expect("peer native seal resumes");
    let disposition = recovery
        .join()
        .expect("peer recovery worker joins")
        .expect("peer recovery closes");
    assert_eq!(disposition, RecoveryWorkDisposition::Closed);
    assert!(
        materialized_while_sealing,
        "the peer held an available remote head until native recovery closed"
    );
    drop(peer_driver);
    drop(watch);
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn loss_during_early_release_forces_a_fresh_covering_scan() {
    let _serial = serialize_early_release_test();
    let (published_tx, published_rx) = mpsc::sync_channel(1);
    let harness = recover_once_harness_with(
        "watcher-overflow-early-release-loss",
        "ws_overflow_early_release_loss",
        Some(Box::new(move || {
            let _sent = published_tx.send(());
        })),
    );
    std::fs::write(
        harness.temp.join("Code").join("publish-before-loss.txt"),
        b"publish before loss",
    )
    .expect("workspace write");
    let worker =
        WatcherRecoveryWorker::claim(Arc::clone(&harness.coordinator), harness.clock.now())
            .expect("recovery worker claims ownership");
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mut coverage = GateSeal {
        inner: harness.coverage.clone(),
        gated: false,
        fail_after_gate: false,
        entered: entered_tx,
        release: release_rx,
    };
    let engine = harness.engine.clone();
    let clock = Arc::clone(&harness.clock);
    let recovery = std::thread::spawn(move || {
        let moment_clock = Arc::clone(&clock);
        let moment_source: Arc<dyn Fn() -> RecoveryMoment + Send + Sync> =
            Arc::new(move || moment_clock.now());
        loop {
            match worker.recover_once(
                &mut coverage,
                &engine,
                &moment_source,
                &|| false,
                &CoverageCancellation::new(),
                &mut || Ok(()),
            ) {
                Ok(
                    RecoveryWorkDisposition::RetryRequired | RecoveryWorkDisposition::RetryDeferred,
                ) => std::thread::sleep(Duration::from_millis(50)),
                outcome => break outcome,
            }
        }
    });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("first native seal is gated");
    let progressed_before_seal = published_rx.recv_timeout(Duration::from_secs(10)).is_ok();
    harness
        .coordinator
        .observe_suppressed(harness.clock.now())
        .expect("loss is admitted inside the early-release window");
    release_tx.send(()).expect("native seal resumes");
    let disposition = recovery
        .join()
        .expect("recovery worker joins")
        .expect("recovery completes after a covering retry");
    assert_eq!(disposition, RecoveryWorkDisposition::Closed);
    assert!(
        progressed_before_seal,
        "scan-proven work must progress before the gated seal closes"
    );
    let closure = harness
        .coordinator
        .snapshot()
        .expect("recovery snapshot stays readable")
        .last_closure()
        .cloned()
        .expect("eventual closure records a receipt");
    assert!(
        closure.scan_count().get() >= 2,
        "loss after early release must reject the first close and require another scan"
    );
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}

#[test]
fn seal_failure_after_early_release_never_grants_recovery_authority() {
    let _serial = serialize_early_release_test();
    let (published_tx, published_rx) = mpsc::sync_channel(1);
    let harness = recover_once_harness_with(
        "watcher-overflow-early-release-seal-failure",
        "ws_overflow_early_release_seal_failure",
        Some(Box::new(move || {
            let _sent = published_tx.send(());
        })),
    );
    std::fs::write(
        harness
            .temp
            .join("Code")
            .join("publish-before-seal-failure.txt"),
        b"publish before seal failure",
    )
    .expect("workspace write");
    let worker =
        WatcherRecoveryWorker::claim(Arc::clone(&harness.coordinator), harness.clock.now())
            .expect("recovery worker claims ownership");
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mut coverage = GateSeal {
        inner: harness.coverage.clone(),
        gated: false,
        fail_after_gate: true,
        entered: entered_tx,
        release: release_rx,
    };
    let engine = harness.engine.clone();
    let clock = Arc::clone(&harness.clock);
    let recovery = std::thread::spawn(move || {
        let moment_clock = Arc::clone(&clock);
        let moment_source: Arc<dyn Fn() -> RecoveryMoment + Send + Sync> =
            Arc::new(move || moment_clock.now());
        loop {
            let outcome = worker.recover_once(
                &mut coverage,
                &engine,
                &moment_source,
                &|| false,
                &CoverageCancellation::new(),
                &mut || Ok(()),
            );
            if coverage.gated {
                break outcome;
            }
            match outcome {
                Ok(
                    RecoveryWorkDisposition::RetryRequired | RecoveryWorkDisposition::RetryDeferred,
                ) => std::thread::sleep(Duration::from_millis(50)),
                terminal => break terminal,
            }
        }
    });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("native seal is gated after the scan");
    let progressed_before_failure = published_rx.recv_timeout(Duration::from_secs(10)).is_ok();
    release_tx.send(()).expect("gated seal returns its failure");
    let disposition = recovery
        .join()
        .expect("recovery worker joins")
        .expect("seal failure is classified");
    assert_eq!(disposition, RecoveryWorkDisposition::RetryDeferred);
    assert!(
        progressed_before_failure,
        "useful work must resume even though native sealing later fails"
    );
    let snapshot = harness
        .coordinator
        .snapshot()
        .expect("recovery snapshot stays readable");
    assert_eq!(snapshot.lifecycle(), RecoveryLifecycle::Recovering);
    assert!(snapshot.last_closure().is_none());
    assert!(harness.coordinator.capture_nominal_frontier().is_err());
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}

#[test]
fn publication_continues_while_the_next_recovery_attempt_prepares_native_coverage() {
    let _serial = serialize_early_release_test();
    let harness = recover_once_harness(
        "watcher-overflow-native-preparation-gap",
        "ws_overflow_native_preparation_gap",
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
    let raced = Arc::new(AtomicBool::new(false));
    let fence_raced = Arc::clone(&raced);
    let gate_enabled = Arc::new(AtomicBool::new(false));
    let fence_gate_enabled = Arc::clone(&gate_enabled);
    let mut before_close = move || {
        if !fence_raced.swap(true, Ordering::AcqRel) {
            fence_baseline.store(
                fence_counters.content_hashes.load(Ordering::Acquire),
                Ordering::Release,
            );
            std::fs::write(fence_root.join("during-recovery.txt"), b"during recovery")
                .expect("mid-incident write");
            let observation =
                fence_ingress.observe(EngineEvent::Paths(BTreeSet::from([WorkspacePath::new(
                    "during-recovery.txt",
                )])));
            assert_eq!(
                observation,
                bowline_daemon::manifest_driver::WatcherIngressObservation::Accumulated
            );
            fence_gate_enabled.store(true, Ordering::Release);
            fence_coordinator.observe_suppressed(fence_clock.now())?;
        }
        Ok(())
    };
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let mut coverage = GateSecondPreparation {
        inner: harness.coverage.clone(),
        gate_enabled,
        gated: false,
        entered: entered_tx,
        release: release_rx,
    };
    let engine = harness.engine.clone();
    let clock = Arc::clone(&harness.clock);
    let recovery = std::thread::spawn(move || {
        let moment_clock = Arc::clone(&clock);
        let moment_source: Arc<dyn Fn() -> RecoveryMoment + Send + Sync> =
            Arc::new(move || moment_clock.now());
        loop {
            match worker.recover_once(
                &mut coverage,
                &engine,
                &moment_source,
                &|| false,
                &CoverageCancellation::new(),
                &mut before_close,
            ) {
                Ok(
                    RecoveryWorkDisposition::RetryRequired | RecoveryWorkDisposition::RetryDeferred,
                ) => std::thread::sleep(Duration::from_millis(50)),
                outcome => break outcome,
            }
        }
    });
    if let Err(error) = entered_rx.recv_timeout(Duration::from_secs(10)) {
        drop(release_tx);
        panic!(
            "second native preparation starts: {error}; recovery result: {:?}",
            recovery.join()
        );
    }
    let published_while_blocked =
        content_hash_advances(&harness.counters, baseline.load(Ordering::Acquire));
    release_tx
        .send(())
        .expect("second native preparation resumes");
    let disposition = recovery
        .join()
        .expect("recovery worker joins")
        .expect("recovery completes");
    assert!(published_while_blocked);
    assert_eq!(disposition, RecoveryWorkDisposition::Closed);
    let snapshot = harness
        .coordinator
        .snapshot()
        .expect("recovery snapshot stays readable");
    let closure = snapshot
        .last_closure()
        .expect("retry closes with complete evidence");
    assert!(closure.scan_count().get() >= 2);
    let _ = std::fs::remove_dir_all(harness.temp.clone());
}
