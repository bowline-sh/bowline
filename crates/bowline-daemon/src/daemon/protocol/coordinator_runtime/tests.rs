use super::*;

#[test]
fn watcher_wake_coalesces_and_upgrades_overflow_before_dirty_take() {
    let wake = WatcherWakeState::default();
    assert!(wake.begin_wake());
    assert!(
        !wake.begin_wake(),
        "only one coordinator wake stays pending"
    );
    wake.record_overflow();

    let scope = DirtyScopeKey::new("workspace-coalescing");
    let metrics = Arc::new(CoordinatorMetrics::default());
    let mut coordinator = CoordinatorState::new(SystemCoordinatorClock::new(), metrics);
    let actions = coordinator.handle_event(CoordinatorEvent::FilesystemDirty(
        FilesystemDirty::one(scope.clone(), DirtyPath::new("first-change")),
    ));
    assert!(matches!(
        actions.as_slice(),
        [CoordinatorAction::DirtyReady(_)]
    ));

    assert!(wake.reset_for_dirty_ready());
    let upgrade = coordinator.handle_event(CoordinatorEvent::WatcherOverflow(scope.clone()));
    assert!(
        upgrade.is_empty(),
        "overflow upgrades the pending dirty wake"
    );
    assert!(matches!(
        coordinator.take_dirty(&scope),
        Some(PendingDirtyBatch::FullScan(_))
    ));
    assert!(wake.begin_wake(), "DirtyReady releases the next wake token");
}

#[test]
fn failed_wake_send_retains_the_coalescing_token_for_coordinator_recovery() {
    let wake = WatcherWakeState::default();
    assert!(wake.begin_wake());
    wake.record_delivery_failure();
    assert!(wake.delivery_failed());
    assert!(!wake.begin_wake());
    assert!(!wake.reset_for_dirty_ready());
    assert!(!wake.delivery_failed());
    assert!(wake.begin_wake());
}

#[test]
fn one_saturated_overflow_wake_is_recovered_without_a_second_watcher_signal() {
    let runtime = DaemonRuntime {
        sync: None,
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = Arc::new(DaemonServerState::new(&runtime).expect("daemon state"));
    let clock = SystemCoordinatorClock::new();
    let metrics = Arc::new(CoordinatorMetrics::default());
    let coordinator_state = CoordinatorState::new(clock.clone(), Arc::clone(&metrics));
    let (handle, events) = coordinator_channel(1);
    let mut driver = CoordinatorDriver::new(coordinator_state, events);
    let executor = CoordinatorExecutor::new(
        CoordinatorExecutorConfig::testing([1, 1, 1, 1, 1], 2),
        handle.clone(),
        Arc::clone(&metrics),
    )
    .expect("executor starts");
    let wake = WatcherWakeState::default();
    assert!(wake.begin_wake());
    wake.record_overflow();
    wake.record_delivery_failure();
    let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        Arc::new(Mutex::new(runtime)),
        state,
        handle,
        clock,
        SchedulerChannels {
            completion_tx,
            completion_rx,
            loss_fallback_tx,
            loss_fallback_rx,
        },
        wake.clone(),
        Some(DirtyScopeKey::new("workspace-overflow")),
    );

    assert!(!scheduler.recover_saturated_watcher_wake(&executor, &mut driver));
    assert_eq!(metrics.snapshot().filesystem_overflows, 1);
    assert!(!wake.delivery_failed());
    assert!(
        wake.begin_wake(),
        "recovery releases exactly one wake token"
    );
    executor.shutdown_and_join().expect("executor joins");
}

#[test]
fn watcher_bridge_backpressures_real_backlog_without_inventing_overflow() {
    let temp = std::env::temp_dir().join(format!(
        "bowline-watcher-bridge-bounded-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let mut runtime = DaemonRuntime {
        sync: Some(crate::daemon::tests::watcher_test_runtime(
            temp.join("Code"),
            temp.clone(),
            "ws_watcher_bridge_bounded",
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let (signal_tx, signal_rx) = mpsc::channel();
    runtime.sync.as_mut().expect("sync runtime").change_rx = Some(signal_rx);
    let state = Arc::new(DaemonServerState::new(&runtime).expect("daemon state"));
    let clock = SystemCoordinatorClock::new();
    let metrics = Arc::new(CoordinatorMetrics::default());
    let coordinator_state = CoordinatorState::new(clock.clone(), Arc::clone(&metrics));
    let (handle, events) = coordinator_channel(1);
    handle
        .try_send(CoordinatorEvent::StatusInput(
            bowline_daemon::status_projection::StatusInputEvent::RefreshAll,
        ))
        .expect("event queue is deliberately saturated");
    let bridge = WatcherBridge::start(&mut runtime, handle.clone())
        .expect("watcher bridge starts")
        .expect("configured receiver creates a bridge");
    let wake = bridge.wake_state();
    let scope = bridge.scope();
    for index in 0..64 {
        signal_tx
            .send(WatcherSignal::Changed(
                Event::new(EventKind::Modify(ModifyKind::Any))
                    .add_path(temp.join(format!("file-{index}"))),
            ))
            .expect("synthetic watcher signal sends");
    }
    let wait_deadline = Instant::now() + Duration::from_secs(2);
    while !wake.delivery_failed() && Instant::now() < wait_deadline {
        std::thread::yield_now();
    }
    assert!(wake.delivery_failed(), "saturated wake is observed");

    let runtime = Arc::new(Mutex::new(runtime));
    let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        Arc::clone(&runtime),
        state,
        handle.clone(),
        clock,
        SchedulerChannels {
            completion_tx,
            completion_rx,
            loss_fallback_tx,
            loss_fallback_rx,
        },
        wake,
        Some(scope),
    );
    let mut driver = CoordinatorDriver::new(coordinator_state, events);
    let executor = CoordinatorExecutor::new(
        CoordinatorExecutorConfig::testing([1, 1, 1, 1, 1], 2),
        handle,
        metrics,
    )
    .expect("executor starts");
    driver.run_turn().expect("saturating event drains");
    // This test owns watcher recovery only; a real hosted refresh makes executor shutdown depend on the network.
    scheduler.trust_refresh_in_flight = true;
    assert!(!scheduler.recover_saturated_watcher_wake(&executor, &mut driver));
    let runtime_guard = runtime.lock().expect("runtime locks");
    let sync = runtime_guard.sync.as_ref().expect("sync runtime remains");
    assert!(
        sync.change_rx.is_some(),
        "lossless backlog remains available"
    );
    assert!(!sync.watcher_recovery.full_reconcile_required);
    assert_eq!(sync.watcher_recovery.overflow_total, 0);
    drop(runtime_guard);

    drop(signal_tx);
    runtime
        .lock()
        .expect("runtime locks for shutdown")
        .sync
        .as_mut()
        .expect("sync runtime remains for shutdown")
        .change_rx
        .take();
    bridge.join().expect("watcher bridge joins");
    executor.shutdown_and_join().expect("executor joins");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn one_lost_sync_worker_recovers_only_its_exact_active_claim() {
    let mut in_flight = HashMap::from([
        ("workspace-a-claim".to_string(), "recovery-a"),
        ("workspace-b-claim".to_string(), "recovery-b"),
    ]);
    let lost_job = CoordinatorJobId::new("workspace-a-claim");

    let recovered = take_exact_worker_loss(&mut in_flight, Some(&lost_job));

    assert_eq!(
        recovered,
        Some(("workspace-a-claim".to_string(), "recovery-a"))
    );
    assert_eq!(
        in_flight,
        HashMap::from([("workspace-b-claim".to_string(), "recovery-b")])
    );
    assert_eq!(take_exact_worker_loss(&mut in_flight, None), None);
}

#[test]
fn missing_domain_completion_has_no_one_second_wait_fallback() {
    let source = include_str!("../coordinator_runtime.rs");
    let handle_completion = source
        .split_once("fn handle_completion")
        .and_then(|(_, tail)| tail.split_once("fn drain_domain_completions"))
        .map(|(body, _)| body)
        .expect("handle_completion source section");

    assert!(!handle_completion.contains("recv_timeout"));
    assert!(!handle_completion.contains("from_secs(1)"));
    assert!(handle_completion.contains("WorkerCompletion::worker_lost"));
}

#[test]
fn saturated_side_lane_fallbacks_clear_both_in_flight_flags_for_resubmit() {
    let runtime = DaemonRuntime {
        sync: None,
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = Arc::new(DaemonServerState::new(&runtime).expect("daemon state"));
    let (handle, _events) = coordinator_channel(4);
    let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        Arc::new(Mutex::new(runtime)),
        state,
        handle,
        SystemCoordinatorClock::new(),
        SchedulerChannels {
            completion_tx,
            completion_rx,
            loss_fallback_tx: loss_fallback_tx.clone(),
            loss_fallback_rx,
        },
        WatcherWakeState::default(),
        None,
    );
    let notification_job = CoordinatorJobId::new("notification-poll-1");
    scheduler.notification_in_flight = Some(notification_job.clone());
    scheduler.trust_refresh_in_flight = true;
    loss_fallback_tx
        .send(SchedulerFallback::NotificationWorkerLost(notification_job))
        .expect("notification fallback sends");
    loss_fallback_tx
        .send(SchedulerFallback::TrustRefreshCompleted)
        .expect("trust fallback sends");

    assert!(scheduler.drain_loss_fallbacks());
    assert!(scheduler.notification_in_flight.is_none());
    assert!(!scheduler.trust_refresh_in_flight);
    assert!(!scheduler.drain_loss_fallbacks());
}

#[test]
fn stale_side_lane_completion_cannot_clear_a_replacement_invocation() {
    let runtime = DaemonRuntime {
        sync: None,
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = Arc::new(DaemonServerState::new(&runtime).expect("daemon state"));
    let (handle, _events) = coordinator_channel(4);
    let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        Arc::new(Mutex::new(runtime)),
        state,
        handle,
        SystemCoordinatorClock::new(),
        SchedulerChannels {
            completion_tx,
            completion_rx,
            loss_fallback_tx,
            loss_fallback_rx,
        },
        WatcherWakeState::default(),
        None,
    );
    let stale = CoordinatorJobId::new("status-publish-1");
    let replacement = CoordinatorJobId::new("status-publish-2");
    scheduler.status_publish_in_flight = Some(replacement.clone());

    assert!(
        !scheduler.handle_side_lane_worker_completion(&CoordinatorWorkerCompletion {
            job_id: stale,
            lane: CoordinatorLane::ControlPlane,
            resource: None,
            outcome: CoordinatorWorkerOutcome::Succeeded,
        })
    );
    assert_eq!(
        scheduler.status_publish_in_flight,
        Some(replacement.clone())
    );

    assert!(
        scheduler.handle_side_lane_worker_completion(&CoordinatorWorkerCompletion {
            job_id: replacement,
            lane: CoordinatorLane::ControlPlane,
            resource: None,
            outcome: CoordinatorWorkerOutcome::Panicked,
        })
    );
    assert!(scheduler.status_publish_in_flight.is_none());
}

#[test]
fn three_durable_control_jobs_leave_one_worker_for_trust_and_status() {
    assert_eq!(MAX_DURABLE_CONTROL_PLANE_IN_FLIGHT, 3);
    let (handle, _events) = coordinator_channel(16);
    let metrics = Arc::new(CoordinatorMetrics::default());
    let executor = CoordinatorExecutor::new(CoordinatorExecutorConfig::default(), handle, metrics)
        .expect("executor starts");
    let (started_tx, started_rx) = crossbeam_channel::bounded(3);
    let (release_tx, release_rx) = crossbeam_channel::bounded(3);
    for ordinal in 0..MAX_DURABLE_CONTROL_PLANE_IN_FLIGHT {
        let started = started_tx.clone();
        let release = release_rx.clone();
        executor
            .submit(CoordinatorJob::new(
                CoordinatorJobId::new(format!("durable-control-{ordinal}")),
                CoordinatorLane::ControlPlane,
                Some(CoordinatorResourceKey::new(format!("workspace-{ordinal}"))),
                move || {
                    started.send(()).expect("started signal sends");
                    release.recv().expect("release arrives");
                    Ok(())
                },
            ))
            .expect("durable control work submits");
    }
    for _ in 0..MAX_DURABLE_CONTROL_PLANE_IN_FLIGHT {
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("three durable jobs start");
    }
    let (side_tx, side_rx) = crossbeam_channel::bounded(2);
    for name in ["trust-refresh-proof", "status-publish-proof"] {
        let completed = side_tx.clone();
        executor
            .submit(CoordinatorJob::new(
                CoordinatorJobId::new(name),
                CoordinatorLane::ControlPlane,
                None,
                move || {
                    completed.send(()).expect("side completion sends");
                    Ok(())
                },
            ))
            .expect("reserved side work submits");
    }
    for _ in 0..2 {
        side_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("trust and status progress while durable jobs remain blocked");
    }
    for _ in 0..MAX_DURABLE_CONTROL_PLANE_IN_FLIGHT {
        release_tx.send(()).expect("durable job releases");
    }
    executor.shutdown_and_join().expect("executor joins");
}

#[test]
fn coordinator_owned_deadlines_survive_a_saturated_event_channel() {
    let runtime = DaemonRuntime {
        sync: None,
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = Arc::new(DaemonServerState::new(&runtime).expect("daemon state"));
    let clock = SystemCoordinatorClock::new();
    let metrics = Arc::new(CoordinatorMetrics::default());
    let coordinator_state = CoordinatorState::new(clock.clone(), metrics);
    let (handle, events) = coordinator_channel(1);
    handle
        .try_send(CoordinatorEvent::StatusInput(
            bowline_daemon::status_projection::StatusInputEvent::RefreshAll,
        ))
        .expect("synthetic event fills the coordinator queue");
    let mut driver = CoordinatorDriver::new(coordinator_state, events);
    let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let scheduler = SchedulerCoordinator::new(
        Arc::new(Mutex::new(runtime)),
        state,
        handle,
        clock,
        SchedulerChannels {
            completion_tx,
            completion_rx,
            loss_fallback_tx,
            loss_fallback_rx,
        },
        WatcherWakeState::default(),
        None,
    );

    scheduler.schedule_deadline(
        CoordinatorDeadlineKind::LeaseRenewal(CoordinatorJobId::new("claim-1")),
        Duration::from_secs(15),
        &mut driver,
    );

    assert!(
        driver
            .state()
            .next_wait()
            .is_some_and(|wait| { wait > Duration::ZERO && wait <= Duration::from_secs(15) })
    );
    assert!(matches!(
        driver.run_turn(),
        Ok(actions) if matches!(actions.as_slice(), [CoordinatorAction::ForwardStatusInput(_)])
    ));
    assert!(driver.state().next_wait().is_some());
}

#[test]
fn prepared_notification_io_does_not_reacquire_the_coordinator_runtime() {
    let state_root = std::env::temp_dir().join(format!(
        "bowline-notification-runtime-isolation-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let mut runtime = DaemonRuntime {
        sync: Some(crate::daemon::tests::watcher_test_runtime(
            state_root.join("Code"),
            state_root.clone(),
            "ws_notification_isolation",
        )),
        notify_approvals: true,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let projection = DaemonServerState::new(&runtime)
        .expect("daemon state")
        .current_projection();
    runtime.pending_notification_status = Some(projection.status.clone());
    let runtime = Arc::new(Mutex::new(runtime));
    let notification = runtime
        .lock()
        .expect("runtime locks for preparation")
        .prepare_notification_poll()
        .expect("notification poll is due");
    let runtime_guard = runtime.lock().expect("coordinator retains runtime state");
    let (completed, observed) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        notification.execute();
        completed.send(()).expect("completion sends");
    });

    observed
        .recv_timeout(Duration::from_secs(2))
        .expect("notification preparation detached external IO from runtime mutex");
    drop(runtime_guard);
    worker.join().expect("notification worker joins");
    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn prepared_hosted_status_io_runs_without_the_coordinator_runtime_lock() {
    let state_root = std::env::temp_dir().join(format!(
        "bowline-status-runtime-isolation-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let workspace_id = WorkspaceId::new("ws_status_isolation");
    let root = state_root.join("Code");
    fs::create_dir_all(&root).expect("workspace root");
    let store =
        crate::daemon::store_access::open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE))
            .expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T00:00:00Z")
        .expect("workspace inserts");
    store
        .insert_root(
            "root_status_isolation",
            &workspace_id,
            &root.display().to_string(),
            "2026-07-15T00:00:00Z",
        )
        .expect("workspace root inserts");
    let mut runtime = DaemonRuntime {
        sync: Some(crate::daemon::tests::watcher_test_runtime(
            root,
            state_root.clone(),
            workspace_id.as_str(),
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = DaemonServerState::new(&runtime).expect("daemon state");
    let projection = state.current_projection();
    let projection_input = state.test_projection_input();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    runtime
        .sync
        .as_mut()
        .expect("sync runtime")
        .status_publisher = StatusPublisher::new(move |payload| {
        entered_tx.send(()).expect("publish start sends");
        release_rx.recv().expect("publish release arrives");
        Ok(StatusPublishOutcome {
            fingerprint: payload.fingerprint.expect("projection fingerprint"),
        })
    });
    let prepared = runtime
        .prepare_projection_status(&projection, false, Instant::now(), &projection_input)
        .expect("status publish prepares");
    let runtime = Arc::new(Mutex::new(runtime));
    let runtime_guard = runtime.lock().expect("coordinator runtime locks");
    let worker = std::thread::spawn(move || prepared.execute());

    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("hosted publish starts without reacquiring runtime state");
    release_tx.send(()).expect("publish releases");
    let completion = worker.join().expect("publish worker joins");
    drop(runtime_guard);
    runtime
        .lock()
        .expect("runtime relocks for completion")
        .complete_status_publish(completion, &projection_input);
    let _ = fs::remove_dir_all(state_root);
}
