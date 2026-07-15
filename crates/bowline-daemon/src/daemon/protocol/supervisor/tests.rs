use super::*;

#[test]
fn scheduler_grace_expiry_records_forced_recovery_and_still_joins_every_thread() {
    let (done_tx, done) = crossbeam_channel::bounded(1);
    let handle = std::thread::Builder::new()
        .name("bowline-test-delayed-scheduler".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            done_tx
                .send(Ok(ThreadJoinReport {
                    expected: 3,
                    joined: 3,
                    forced_recovery: false,
                }))
                .expect("supervisor receives scheduler completion");
        })
        .expect("scheduler test thread starts");

    let report = join_scheduler(SchedulerThread { handle, done }, Duration::from_millis(1))
        .expect("strict join succeeds after recording grace expiry");

    assert!(report.forced_recovery);
    assert_eq!(report.expected, 4);
    assert_eq!(report.joined, report.expected);
}

#[test]
fn join_report_merge_preserves_complete_accounting_and_forced_state() {
    let mut report = ThreadJoinReport::default();
    report.record_joined(1);
    report.merge(ThreadJoinReport {
        expected: 19,
        joined: 19,
        forced_recovery: true,
    });

    assert_eq!(report.expected, 20);
    assert_eq!(report.joined, report.expected);
    assert!(report.forced_recovery);
}

#[test]
fn live_supervisor_cleanly_joins_every_fixed_worker_and_cleans_socket_last() {
    let socket_dir = unique_supervisor_socket_dir("clean-accounting");
    fs::create_dir_all(&socket_dir).expect("socket directory");
    let socket = socket_dir.join("daemon.sock");
    prepare_socket(&socket).expect("socket path prepared");
    let runtime = runtime_without_sync();
    let state = Arc::new(DaemonServerState::new(&runtime).expect("daemon state"));
    let threads = DaemonThreads::start(&socket, true, runtime, Arc::clone(&state))
        .expect("daemon threads start");
    let live_metrics = state.runtime_metrics();
    assert_eq!(
        live_metrics["coordinator"]["configuredWorkers"],
        serde_json::json!(19)
    );
    assert_eq!(
        live_metrics["rpc"]["configuredQueryWorkers"],
        serde_json::json!(8)
    );
    assert_eq!(live_metrics["shutdown"]["phase"], "running");
    let socket_guard = SocketGuard {
        path: Some(socket.clone()),
    };

    let report = threads
        .shutdown_with_grace(ShutdownReason::ServeOnceComplete, Duration::from_secs(1))
        .expect("strict shutdown joins");

    let expected = 1 // acceptor
        + 1 // one-shot connection executor
        + 1 // scheduler owner
        + 1 // status projection worker
        + 1 // shutdown watchdog
        + 19 // mutation/query/sync/control/notification lane workers
        + 12; // query/status and mutation RPC workers
    assert_eq!(report.outcome, ShutdownOutcome::Clean);
    assert_eq!(report.expected_threads, expected);
    assert_eq!(report.joined_threads, expected);
    assert_eq!(report.coordinator_metrics.configured_workers, 19);
    assert_eq!(report.coordinator_metrics.joined_workers, 19);
    assert_eq!(report.coordinator_metrics.active_resources, 0);
    assert_eq!(state.shutdown_phase(), ShutdownPhase::JoinThreads);
    assert!(socket.exists(), "socket remains until all threads join");

    state.advance_shutdown(ShutdownPhase::RemoveSocketState);
    socket_guard.cleanup().expect("socket cleanup succeeds");
    state.advance_shutdown(ShutdownPhase::Complete);
    assert_eq!(state.shutdown_phase(), ShutdownPhase::Complete);
    assert!(!socket.exists());
    let _cleanup = fs::remove_dir_all(socket_dir);
}

#[test]
fn shutdown_phases_are_monotonic_and_mutation_admission_closes_first() {
    let state = DaemonServerState::new(&runtime_without_sync()).expect("daemon state");
    assert_eq!(state.shutdown_phase(), ShutdownPhase::Running);
    assert!(state.accepts_mutations());

    state.begin_shutdown(ShutdownReason::ClientRequest);
    assert_eq!(state.shutdown_phase(), ShutdownPhase::StopAccepting);
    assert!(!state.accepts_mutations());
    state.cancel_rpc_work();
    assert_eq!(state.shutdown_phase(), ShutdownPhase::CancelRpcWork);
    assert!(state.should_stop_connections());
    state.stop_durable_claims();
    assert_eq!(state.shutdown_phase(), ShutdownPhase::StopDurableClaims);
    assert!(state.should_stop_durable_claims());
    state.advance_shutdown(ShutdownPhase::FlushBookkeeping);
    state.advance_shutdown(ShutdownPhase::JoinThreads);
    state.advance_shutdown(ShutdownPhase::RemoveSocketState);
    state.advance_shutdown(ShutdownPhase::Complete);
    state.advance_shutdown(ShutdownPhase::StopAccepting);
    assert_eq!(state.shutdown_phase(), ShutdownPhase::Complete);
}

fn runtime_without_sync() -> DaemonRuntime {
    DaemonRuntime {
        sync: None,
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    }
}

fn unique_supervisor_socket_dir(label: &str) -> PathBuf {
    PathBuf::from("/tmp").join(format!(
        "bowline-supervisor-{label}-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ))
}
