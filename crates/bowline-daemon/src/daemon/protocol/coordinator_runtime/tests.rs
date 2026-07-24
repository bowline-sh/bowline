use super::watcher_bridge::{WatcherBridgeStartError, forward_watcher_signals};
use super::*;
use bowline_local::metadata::MetadataStore;

use crate::daemon::watcher::WatcherOverflowLane;
use bowline_local::sync::manifest_engine::{EngineCounters, EngineEvent, WorkspacePath};

fn watcher_changed(event: Event) -> WatcherSignal {
    WatcherSignal::Changed { event }
}

/// A manifest driver whose thread records forwarded engine events instead of
/// running the real engine, so the watcher bridge's output is observable.
fn recording_driver() -> (
    bowline_daemon::manifest_driver::ManifestDriver,
    Arc<Mutex<Vec<EngineEvent>>>,
) {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&recorded);
    let driver = bowline_daemon::manifest_driver::ManifestDriver::spawn(move |inbox, _snapshot| {
        while let Ok(event) = inbox.recv() {
            if matches!(event, EngineEvent::Shutdown) {
                break;
            }
            if let Ok(mut recorded) = sink.lock() {
                recorded.push(event);
            }
        }
    })
    .expect("recording driver spawns");
    (driver, recorded)
}

/// Install a recording driver on a watcher test runtime and wire a manual
/// watcher signal channel into it.
fn runtime_with_recording_driver(
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: &str,
) -> (
    DaemonRuntime,
    mpsc::Sender<WatcherSignal>,
    Arc<Mutex<Vec<EngineEvent>>>,
) {
    let mut sync = crate::daemon::tests::watcher_test_runtime(root, state_root, workspace_id);
    let (signal_tx, signal_rx) = mpsc::channel();
    sync.change_rx = Some(signal_rx);
    let (driver, recorded) = recording_driver();
    sync.manifest_engine = crate::daemon::sync::ManifestEngineHost::Active(driver);
    let runtime = DaemonRuntime {
        sync: Some(sync),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    (runtime, signal_tx, recorded)
}

/// Poll the recorded engine events until `predicate` matches one, or fail.
fn await_recorded_event(
    recorded: &Arc<Mutex<Vec<EngineEvent>>>,
    predicate: impl Fn(&EngineEvent) -> bool,
) -> EngineEvent {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(events) = recorded.lock()
            && let Some(event) = events.iter().find(|event| predicate(event))
        {
            return event.clone();
        }
        assert!(
            Instant::now() < deadline,
            "expected engine event never arrived"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn watcher_bridge_forwards_rename_as_engine_paths() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-bridge-engine-rename");
    let root = temp.join("Code");
    let state_root = temp.join("state");
    fs::create_dir_all(root.join("src")).expect("workspace root");
    fs::create_dir_all(&state_root).expect("state root");
    let source_path = root.join("src/old.rs");
    let destination_path = root.join("src/new.rs");
    fs::write(&destination_path, "fn renamed() {}\n").expect("destination");
    let (mut runtime, signal_tx, recorded) =
        runtime_with_recording_driver(root, state_root, "workspace-engine-rename");
    let bridge = WatcherBridge::start(&mut runtime)
        .expect("bridge starts")
        .expect("watcher receiver configured");

    signal_tx
        .send(watcher_changed(
            Event::new(EventKind::Modify(ModifyKind::Name(
                notify::event::RenameMode::Both,
            )))
            .add_path(source_path)
            .add_path(destination_path),
        ))
        .expect("rename sends");
    let event = await_recorded_event(&recorded, |event| matches!(event, EngineEvent::Paths(_)));
    let EngineEvent::Paths(paths) = event else {
        panic!("expected Paths event");
    };
    assert!(paths.contains(&WorkspacePath::new("src/old.rs")));
    assert!(paths.contains(&WorkspacePath::new("src/new.rs")));

    drop(signal_tx);
    drop(bridge);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_bridge_forwards_overflow_as_full_scan() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-bridge-engine-overflow");
    let root = temp.join("Code");
    let state_root = temp.join("state");
    fs::create_dir_all(&root).expect("workspace root");
    fs::create_dir_all(&state_root).expect("state root");
    let (mut runtime, signal_tx, recorded) =
        runtime_with_recording_driver(root, state_root, "workspace-engine-overflow");
    let counters = Arc::clone(
        &runtime
            .sync
            .as_ref()
            .expect("sync runtime")
            .manifest_counters,
    );
    let bridge = WatcherBridge::start(&mut runtime)
        .expect("bridge starts")
        .expect("watcher receiver configured");

    signal_tx
        .send(WatcherSignal::Recoverable)
        .expect("overflow sends");
    let event = await_recorded_event(&recorded, |event| {
        matches!(event, EngineEvent::FullScanRequired(_))
    });
    assert!(matches!(event, EngineEvent::FullScanRequired(_)));
    assert_eq!(
        counters.snapshot().watcher_overflow_recoveries,
        1,
        "native recoverable signals count successful watcher-overflow fences"
    );

    drop(signal_tx);
    drop(bridge);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_overflow_collapses_backlog_and_preserves_follow_up_edit() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-overflow-collapse");
    let root = temp.join("Code");
    fs::create_dir_all(&root).expect("workspace root");
    let overflow_lane = Arc::new(WatcherOverflowLane::default());
    let (signal_tx, signal_rx) = mpsc::sync_channel(64);
    signal_tx
        .send(WatcherSignal::OverflowLane(Arc::clone(&overflow_lane)))
        .expect("overflow lane");
    for index in 0..10_000 {
        crate::daemon::watcher::send_watcher_signal(
            &signal_tx,
            &overflow_lane,
            Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(root.join(format!("backlog-{index}.txt")))),
        );
    }
    assert!(overflow_lane.recovery_requested());

    let (engine_tx, engine_rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_root = root.clone();
    let counters = EngineCounters::shared();
    let worker_counters = Arc::clone(&counters);
    let worker = std::thread::spawn(move || {
        forward_watcher_signals(
            signal_rx,
            engine_tx,
            worker_root,
            worker_shutdown,
            worker_counters,
        );
    });

    assert!(matches!(
        engine_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::FullScanRequired(
            bowline_local::sync::manifest_engine::FullScanReason::WatcherOverflow
        ))
    ));
    assert!(
        engine_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "the saturated pre-overflow backlog collapses into one recovery fence"
    );
    assert_eq!(counters.snapshot().watcher_overflow_recoveries, 1);

    // Model another native loss while the engine may still be executing the
    // first scan. A level-triggered lane must re-arm a second fence; clearing
    // the first request cannot erase this later loss.
    overflow_lane.request_recovery();
    assert!(matches!(
        engine_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::FullScanRequired(
            bowline_local::sync::manifest_engine::FullScanReason::WatcherOverflow
        ))
    ));
    assert!(
        engine_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "one re-armed loss produces exactly one additional fence"
    );
    assert_eq!(counters.snapshot().watcher_overflow_recoveries, 2);

    let follow_up = root.join("follow-up.txt");
    fs::write(&follow_up, b"after overflow").expect("follow-up edit");
    signal_tx
        .send(watcher_changed(
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(follow_up),
        ))
        .expect("follow-up signal");
    let event = engine_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("follow-up survives recovery fence");
    let EngineEvent::Paths(paths) = event else {
        panic!("expected follow-up Paths event, got {event:?}");
    };
    assert!(paths.contains(&WorkspacePath::new("follow-up.txt")));

    drop(signal_tx);
    worker.join().expect("bridge worker");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_overflow_latch_wakes_bridge_after_source_channel_drains() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-overflow-latched-wake");
    let root = temp.join("Code");
    fs::create_dir_all(&root).expect("workspace root");
    let overflow_lane = Arc::new(WatcherOverflowLane::default());
    let (signal_tx, signal_rx) = mpsc::sync_channel(4);
    signal_tx
        .send(WatcherSignal::OverflowLane(Arc::clone(&overflow_lane)))
        .expect("overflow lane");
    let observed_path = root.join("observed.txt");
    fs::write(&observed_path, b"observed").expect("observed file");
    signal_tx
        .send(watcher_changed(
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(observed_path),
        ))
        .expect("ordinary signal");
    let (engine_tx, engine_rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_root = root.clone();
    let counters = EngineCounters::shared();
    let worker_counters = Arc::clone(&counters);
    let worker = std::thread::spawn(move || {
        forward_watcher_signals(
            signal_rx,
            engine_tx,
            worker_root,
            worker_shutdown,
            worker_counters,
        );
    });
    assert!(matches!(
        engine_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::Paths(_))
    ));
    assert_eq!(
        counters.snapshot().watcher_overflow_recoveries,
        0,
        "ordinary watcher traffic does not increment overflow recovery"
    );

    // No channel send accompanies this latch. The bridge must discover it from
    // the timeout path after the ordinary source queue is already empty.
    overflow_lane.request_recovery();
    assert!(matches!(
        engine_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::FullScanRequired(
            bowline_local::sync::manifest_engine::FullScanReason::WatcherOverflow
        ))
    ));
    assert_eq!(counters.snapshot().watcher_overflow_recoveries, 1);

    drop(signal_tx);
    worker.join().expect("bridge worker");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn continuously_replenished_overflow_emits_fence_and_stops_promptly() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-overflow-live-producer");
    let root = temp.join("Code");
    fs::create_dir_all(&root).expect("workspace root");
    let overflow_lane = Arc::new(WatcherOverflowLane::default());
    let (signal_tx, signal_rx) = mpsc::sync_channel(8);
    signal_tx
        .send(WatcherSignal::OverflowLane(Arc::clone(&overflow_lane)))
        .expect("overflow lane");
    for index in 0..7 {
        signal_tx
            .send(watcher_changed(
                Event::new(EventKind::Modify(ModifyKind::Any))
                    .add_path(root.join(format!("stale-{index}.txt"))),
            ))
            .expect("initial stale backlog");
    }
    overflow_lane.request_recovery();
    let producer_stop = Arc::new(AtomicBool::new(false));
    let producer_stopped = Arc::clone(&producer_stop);
    let producer_lane = Arc::clone(&overflow_lane);
    let producer_tx = signal_tx.clone();
    let producer_root = root.clone();
    let producer = std::thread::spawn(move || {
        let mut index = 0u64;
        while !producer_stopped.load(Ordering::Acquire) {
            let signal = watcher_changed(
                Event::new(EventKind::Modify(ModifyKind::Any))
                    .add_path(producer_root.join(format!("live-{index}.txt"))),
            );
            match producer_tx.try_send(signal) {
                Ok(()) => index = index.saturating_add(1),
                Err(mpsc::TrySendError::Full(_)) => producer_lane.request_recovery(),
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    });

    let (engine_tx, engine_rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_root = root.clone();
    let counters = EngineCounters::shared();
    let worker_counters = Arc::clone(&counters);
    let worker = std::thread::spawn(move || {
        forward_watcher_signals(
            signal_rx,
            engine_tx,
            worker_root,
            worker_shutdown,
            worker_counters,
        );
    });
    assert!(matches!(
        engine_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::FullScanRequired(
            bowline_local::sync::manifest_engine::FullScanReason::WatcherOverflow
        ))
    ));

    shutdown.store(true, Ordering::Release);
    let (joined_tx, joined_rx) = mpsc::channel();
    std::thread::spawn(move || {
        worker.join().expect("bridge worker");
        joined_tx.send(()).expect("join completion");
    });
    assert!(
        joined_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "shutdown joins while the source is continuously replenished"
    );
    producer_stop.store(true, Ordering::Release);
    producer.join().expect("producer");
    drop(signal_tx);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_bridge_drop_joins_under_live_production() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-overflow-drop-live");
    let root = temp.join("Code");
    let state_root = temp.join("state");
    fs::create_dir_all(&root).expect("workspace root");
    fs::create_dir_all(&state_root).expect("state root");
    let (mut runtime, signal_tx, _recorded) =
        runtime_with_recording_driver(root.clone(), state_root, "workspace-drop-live");
    let bridge = WatcherBridge::start(&mut runtime)
        .expect("bridge starts")
        .expect("watcher receiver configured");
    let overflow_lane = Arc::new(WatcherOverflowLane::default());
    signal_tx
        .send(WatcherSignal::OverflowLane(Arc::clone(&overflow_lane)))
        .expect("overflow lane");
    let producer_stop = Arc::new(AtomicBool::new(false));
    let producer_stopped = Arc::clone(&producer_stop);
    let producer_tx = signal_tx.clone();
    let producer_root = root.clone();
    let producer_lane = Arc::clone(&overflow_lane);
    let (started_tx, started_rx) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        started_tx.send(()).expect("producer started");
        let mut index = 0u64;
        while !producer_stopped.load(Ordering::Acquire) {
            producer_lane.request_recovery();
            if producer_tx
                .send(watcher_changed(
                    Event::new(EventKind::Modify(ModifyKind::Any))
                        .add_path(producer_root.join(format!("live-{index}.txt"))),
                ))
                .is_err()
            {
                break;
            }
            index = index.saturating_add(1);
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("live producer active");

    let (dropped_tx, dropped_rx) = mpsc::channel();
    std::thread::spawn(move || {
        drop(bridge);
        dropped_tx.send(()).expect("drop completion");
    });
    assert!(
        dropped_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "WatcherBridge::drop joins under live production"
    );

    producer_stop.store(true, Ordering::Release);
    producer.join().expect("producer");
    drop(signal_tx);
    drop(runtime);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_bridge_without_engine_does_not_start() {
    let temp = crate::daemon::tests::unique_temp_dir("watcher-bridge-no-engine");
    let root = temp.join("Code");
    let state_root = temp.join("state");
    fs::create_dir_all(&root).expect("workspace root");
    fs::create_dir_all(&state_root).expect("state root");
    let mut sync =
        crate::daemon::tests::watcher_test_runtime(root, state_root, "workspace-no-engine");
    let (_signal_tx, signal_rx) = mpsc::channel();
    sync.change_rx = Some(signal_rx);
    let mut runtime = DaemonRuntime {
        sync: Some(sync),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    // No manifest driver means no engine to forward to, so the bridge is absent.
    assert!(
        WatcherBridge::start(&mut runtime)
            .expect("start ok")
            .is_none()
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_bridge_spawn_failure_does_not_strand_receiver() {
    let temp = std::env::temp_dir().join(format!(
        "bowline-watcher-bridge-rearm-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let watched_root = temp.join("Code");
    fs::create_dir_all(watched_root.join("nested")).expect("watched root");
    let (mut runtime, _signal_tx, _recorded) = runtime_with_recording_driver(
        watched_root.clone(),
        temp.clone(),
        "ws_watcher_bridge_rearm",
    );
    let start_result = WatcherBridge::start_with_spawner(&mut runtime, |_worker| {
        Err(io::Error::other("injected watcher bridge spawn failure"))
    });
    assert!(matches!(
        start_result,
        Err(WatcherBridgeStartError::ThreadSpawn { .. })
    ));
    assert!(
        runtime
            .sync
            .as_ref()
            .expect("sync runtime")
            .change_rx
            .is_some(),
        "failed bridge spawn leaves the real receiver retryable"
    );
    let _ = fs::remove_dir_all(temp);
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
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        Arc::new(Mutex::new(runtime)),
        state,
        handle,
        SystemCoordinatorClock::new(),
        loss_fallback_tx.clone(),
        loss_fallback_rx,
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
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        Arc::new(Mutex::new(runtime)),
        state,
        handle,
        SystemCoordinatorClock::new(),
        loss_fallback_tx,
        loss_fallback_rx,
    );
    let stale = CoordinatorJobId::new("status-publish-1");
    let replacement = CoordinatorJobId::new("status-publish-2");
    scheduler.status_publish_in_flight = Some(replacement.clone());

    assert!(
        !scheduler.handle_side_lane_worker_completion(&CoordinatorWorkerCompletion {
            job_id: stale,
            lane: CoordinatorLane::ControlPlane,
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
            outcome: CoordinatorWorkerOutcome::Panicked,
        })
    );
    assert!(scheduler.status_publish_in_flight.is_none());
}

#[test]
fn three_blocked_control_jobs_leave_one_worker_for_trust_and_status() {
    let blocked_jobs = 3;
    let (handle, _events) = coordinator_channel(16);
    let metrics = Arc::new(CoordinatorMetrics::default());
    let executor = CoordinatorExecutor::new(CoordinatorExecutorConfig::default(), handle, metrics)
        .expect("executor starts");
    let (started_tx, started_rx) = crossbeam_channel::bounded(3);
    let (release_tx, release_rx) = crossbeam_channel::bounded(3);
    for ordinal in 0..blocked_jobs {
        let started = started_tx.clone();
        let release = release_rx.clone();
        executor
            .submit(CoordinatorJob::new(
                CoordinatorJobId::new(format!("blocked-control-{ordinal}")),
                CoordinatorLane::ControlPlane,
                move || {
                    started.send(()).expect("started signal sends");
                    release.recv().expect("release arrives");
                    Ok(())
                },
            ))
            .expect("blocked control work submits");
    }
    for _ in 0..blocked_jobs {
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("three blocked jobs start");
    }
    let (side_tx, side_rx) = crossbeam_channel::bounded(2);
    for name in ["trust-refresh-proof", "status-publish-proof"] {
        let completed = side_tx.clone();
        executor
            .submit(CoordinatorJob::new(
                CoordinatorJobId::new(name),
                CoordinatorLane::ControlPlane,
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
            .expect("trust and status progress while blocked jobs remain");
    }
    for _ in 0..blocked_jobs {
        release_tx.send(()).expect("blocked job releases");
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
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let scheduler = SchedulerCoordinator::new(
        Arc::new(Mutex::new(runtime)),
        state,
        handle,
        clock,
        loss_fallback_tx,
        loss_fallback_rx,
    );

    scheduler.schedule_deadline(
        CoordinatorDeadlineKind::EngineRetry(CoordinatorJobId::new("engine-retry-1")),
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
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
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
