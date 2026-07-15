use super::*;
use crate::daemon::sync::SyncScanSummary;
use notify::event::Flag;

#[test]
fn watcher_overflow_drops_backlog_and_schedules_rearm() {
    let fixture = watcher_fixture("bowline-daemon-watch-overflow", "ws_watch_overflow");
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("apps/web/src")).expect("root dirs");
    let (signal_tx, signal_rx) = mpsc::channel();
    for index in 0..=WATCHER_DRAIN_BUDGET {
        let changed_path = root.join(format!("apps/web/src/file-{index}.ts"));
        fs::write(&changed_path, "export const ok = true;\n").expect("file");
        signal_tx
            .send(WatcherSignal::Changed(
                Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
            ))
            .expect("watcher signal sends");
    }
    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime.change_rx = Some(signal_rx);

    let drained = runtime.drain_changes();

    assert!(drained.changed);
    assert!(drained.sync_now);
    assert!(runtime.watcher.is_none());
    assert!(runtime.change_rx.is_none());
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
    assert!(runtime.watcher_recovery.rearm_at.is_some());
    assert!(runtime.watcher_recovery.full_reconcile_required);
    assert_eq!(runtime.watcher_recovery.overflow_total, 1);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_git_index_churn_forces_bounded_overflow_recovery() {
    let fixture = watcher_fixture(
        "bowline-daemon-watch-git-ignored-storm",
        "ws_watch_git_ignored_storm",
    );
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("repo/.git")).expect("git dir");
    let index_path = root.join("repo/.git/index");
    fs::write(&index_path, "local index").expect("index");
    let (signal_tx, signal_rx) = mpsc::channel();
    for _ in 0..=WATCHER_DRAIN_BUDGET {
        signal_tx
            .send(WatcherSignal::Changed(
                Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                    .add_path(index_path.clone()),
            ))
            .expect("watcher signal sends");
    }
    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime.change_rx = Some(signal_rx);

    let drained = runtime.drain_changes();

    assert!(drained.changed);
    assert!(drained.sync_now);
    assert!(runtime.watcher.is_none());
    assert!(runtime.change_rx.is_none());
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
    assert!(runtime.watcher_recovery.full_reconcile_required);
    assert_eq!(runtime.watcher_recovery.overflow_total, 1);
    assert_eq!(
        fixture
            .store
            .local_write_log(&fixture.workspace_id)
            .expect("write log")
            .len(),
        WATCHER_DRAIN_BUDGET
    );

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_git_index_backlog_after_relevant_batch_forces_overflow() {
    let fixture = watcher_fixture(
        "bowline-daemon-watch-git-tail-storm",
        "ws_watch_git_tail_storm",
    );
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("apps/web/src")).expect("source dir");
    fs::create_dir_all(root.join("repo/.git")).expect("git dir");
    let (signal_tx, signal_rx) = mpsc::channel();
    for index in 0..WATCHER_DRAIN_BUDGET {
        let changed_path = root.join(format!("apps/web/src/file-{index}.ts"));
        fs::write(&changed_path, "export const ok = true;\n").expect("file");
        signal_tx
            .send(WatcherSignal::Changed(
                Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
            ))
            .expect("watcher signal sends");
    }
    let index_path = root.join("repo/.git/index");
    fs::write(&index_path, "local index").expect("index");
    signal_tx
        .send(WatcherSignal::Changed(
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(index_path),
        ))
        .expect("watcher signal sends");
    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime.change_rx = Some(signal_rx);

    let drained = runtime.drain_changes();

    assert!(drained.changed);
    assert!(drained.sync_now);
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
    assert!(runtime.change_rx.is_none());
    assert!(runtime.watcher_recovery.full_reconcile_required);
    assert_eq!(runtime.watcher_recovery.overflow_total, 1);
    assert_eq!(
        fixture
            .store
            .local_write_log(&fixture.workspace_id)
            .expect("write log")
            .len(),
        WATCHER_DRAIN_BUDGET
    );

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_relevant_backlog_after_git_index_batch_forces_overflow() {
    let fixture = watcher_fixture(
        "bowline-daemon-watch-git-hidden-relevant",
        "ws_watch_git_hidden_relevant",
    );
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("repo/.git")).expect("git dir");
    fs::create_dir_all(root.join("apps/web/src")).expect("source dir");
    let index_path = root.join("repo/.git/index");
    fs::write(&index_path, "local index").expect("index");
    let (signal_tx, signal_rx) = mpsc::channel();
    for _ in 0..WATCHER_DRAIN_BUDGET {
        signal_tx
            .send(WatcherSignal::Changed(
                Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                    .add_path(index_path.clone()),
            ))
            .expect("watcher signal sends");
    }
    let changed_path = root.join("apps/web/src/real.ts");
    fs::write(&changed_path, "export const ok = true;\n").expect("file");
    signal_tx
        .send(WatcherSignal::Changed(
            Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
        ))
        .expect("watcher signal sends");
    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime.change_rx = Some(signal_rx);

    let drained = runtime.drain_changes();

    assert!(drained.changed);
    assert!(drained.sync_now);
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
    assert!(runtime.change_rx.is_none());
    assert!(runtime.watcher_recovery.full_reconcile_required);
    assert_eq!(runtime.watcher_recovery.overflow_total, 1);
    assert_eq!(
        fixture
            .store
            .local_write_log(&fixture.workspace_id)
            .expect("write log")
            .len(),
        WATCHER_DRAIN_BUDGET
    );

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rescan_event_drops_backlog_and_schedules_rearm() {
    let fixture = watcher_fixture("bowline-daemon-watch-rescan", "ws_watch_rescan");
    let (signal_tx, signal_rx) = mpsc::channel();
    signal_tx
        .send(WatcherSignal::Changed(
            Event::new(EventKind::Other).set_flag(Flag::Rescan),
        ))
        .expect("watcher signal sends");
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.change_rx = Some(signal_rx);

    let drained = runtime.drain_changes();

    assert!(drained.changed);
    assert!(drained.sync_now);
    assert!(runtime.watcher.is_none());
    assert!(runtime.change_rx.is_none());
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
    assert!(runtime.watcher_recovery.rearm_at.is_some());
    assert!(runtime.watcher_recovery.full_reconcile_required);
    assert_eq!(runtime.watcher_recovery.overflow_total, 1);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_overflow_error_drops_backlog_and_schedules_rearm() {
    let fixture = watcher_fixture(
        "bowline-daemon-watch-overflow-error",
        "ws_watch_overflow_error",
    );
    let (signal_tx, signal_rx) = mpsc::sync_channel(1);
    send_watcher_signal(
        &signal_tx,
        Err(notify::Error::generic(
            "watch queue overflow; rescan required",
        )),
    );
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.change_rx = Some(signal_rx);

    let drained = runtime.drain_changes();

    assert!(drained.changed);
    assert!(drained.sync_now);
    assert!(runtime.watcher.is_none());
    assert!(runtime.change_rx.is_none());
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
    assert!(runtime.watcher_recovery.rearm_at.is_some());
    assert!(runtime.watcher_recovery.full_reconcile_required);
    assert_eq!(runtime.watcher_recovery.overflow_total, 1);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_overflow_forces_full_reconcile_enqueue() {
    let fixture = watcher_fixture(
        "bowline-daemon-watch-overflow-reconcile",
        "ws_watch_overflow_reconcile",
    );
    let device_id = DeviceId::new("device-test");
    insert_completed_reconcile(&fixture.store, &fixture.workspace_id, &device_id);
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    assert!(!runtime.should_enqueue_daemon_reconcile(
        &fixture.store,
        &fixture.workspace_id,
        &device_id,
        "2026-07-03T00:00:00Z",
    ));
    runtime.watcher_recovery.full_reconcile_required = true;
    assert!(runtime.should_enqueue_daemon_reconcile(
        &fixture.store,
        &fixture.workspace_id,
        &device_id,
        "2026-07-03T00:00:00Z",
    ));
    runtime.sync_once = Box::new(|_, _| {
        Ok(SyncOnceSummary {
            workspace_id: "ws_watch_overflow_reconcile".to_string(),
            snapshot_id: "snap-overflow".to_string(),
            version: 2,
            outcome: SyncSummaryOutcome::Uploaded { stale: false },
            snapshot_root_manifest_id: Some("manifest-root".to_string()),
            manifest_object_key: Some("manifest-key".to_string()),
            namespace_root_id: Some("nsp-root".to_string()),
            conflict_count: 0,
            conflicts: Vec::new(),
            scan: SyncScanSummary::default(),
            cancelled_late: false,
        })
    });

    runtime.poll();

    assert!(!runtime.watcher_recovery.full_reconcile_required);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_overflow_reconcile_marker_survives_failed_sync() {
    let fixture = watcher_fixture(
        "bowline-daemon-watch-overflow-reconcile-fails",
        "ws_watch_overflow_reconcile_fails",
    );
    let device_id = DeviceId::new("device-test");
    insert_completed_reconcile(&fixture.store, &fixture.workspace_id, &device_id);
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.watcher_recovery.full_reconcile_required = true;
    runtime.sync_once = Box::new(|_, _| {
        Err(SyncOnceError::ControlPlane(ControlPlaneError::Storage(
            "sync failed before scanning root".to_string(),
        )))
    });

    runtime.poll();

    assert!(runtime.watcher_recovery.full_reconcile_required);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rearm_restores_ready_state() {
    let fixture = watcher_fixture("bowline-daemon-watch-rearm-ready", "ws_watch_rearm_ready");
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.watcher_state = WatcherRuntimeState::Rearming;
    let now = Instant::now();
    runtime.watcher_recovery.rearm_at = Some(
        now.checked_sub(Duration::from_millis(1))
            .expect("instant subtraction stays in range"),
    );

    runtime.maybe_rearm_watcher(now);

    assert!(runtime.change_rx.is_some());
    assert!(runtime.watcher.is_some());
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Ready);
    assert!(runtime.watcher_recovery.rearm_at.is_none());
    assert_eq!(runtime.watcher_recovery.rearm_failure_count, 0);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rearm_waits_for_scheduled_backoff() {
    let fixture = watcher_fixture("bowline-daemon-watch-rearm-waits", "ws_watch_rearm_waits");
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.watcher_state = WatcherRuntimeState::Rearming;
    let now = Instant::now();
    let rearm_at = now + Duration::from_secs(60);
    runtime.watcher_recovery.rearm_at = Some(rearm_at);

    assert!(!runtime.maybe_rearm_watcher(now));

    assert!(runtime.change_rx.is_none());
    assert!(runtime.watcher.is_none());
    assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
    assert_eq!(runtime.watcher_recovery.rearm_at, Some(rearm_at));
    assert_eq!(runtime.watcher_recovery.rearm_failure_count, 0);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rearm_preserves_storm_backoff_ladder() {
    let fixture = watcher_fixture("bowline-daemon-watch-rearm-storm", "ws_watch_rearm_storm");
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    let first = Instant::now();
    runtime.begin_watcher_overflow_recovery(first);
    let first_rearm_at = runtime
        .watcher_recovery
        .rearm_at
        .expect("first overflow schedules re-arm");
    assert!(runtime.maybe_rearm_watcher(first_rearm_at));
    assert_eq!(runtime.watcher_recovery.consecutive_overflows, 1);

    let second = first + Duration::from_secs(10);
    runtime.begin_watcher_overflow_recovery(second);

    assert_eq!(runtime.watcher_recovery.consecutive_overflows, 2);
    assert_eq!(
        runtime.watcher_recovery.rearm_at,
        Some(second + watcher_rearm_delay(2))
    );

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rearm_transition_refreshes_status_before_next_sync_tick() {
    let fixture = watcher_fixture("bowline-daemon-watch-rearm-status", "ws_watch_rearm_status");
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.watcher_state = WatcherRuntimeState::Rearming;
    runtime.last_json =
        "{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{\"state\":\"rearming\",\"overflowCount\":1,\"rearmAttempt\":1}}"
            .to_string();
    let now = Instant::now();
    runtime.next_tick = now + Duration::from_secs(60);
    runtime.next_remote_observe = now + Duration::from_secs(60);
    runtime.watcher_recovery.overflow_total = 1;
    runtime.watcher_recovery.consecutive_overflows = 1;
    runtime.watcher_recovery.rearm_at = Some(now);

    runtime.poll();

    let status: serde_json::Value =
        serde_json::from_str(runtime.status_json()).expect("status json parses");
    assert_eq!(status["watcherState"]["state"], "ready");
    assert_eq!(status["watcherState"]["overflowCount"], 1);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rearm_failure_refreshes_status_before_next_sync_tick() {
    let temp = unique_temp_dir("bowline-daemon-watch-rearm-failure-status");
    let missing_root = temp.join("missing-root");
    let mut runtime =
        watcher_test_runtime(missing_root, temp.join(".state"), "ws_rearm_failure_status");
    runtime.watcher_state = WatcherRuntimeState::Rearming;
    runtime.last_json =
        "{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{\"state\":\"rearming\",\"overflowCount\":1,\"rearmAttempt\":1}}"
            .to_string();
    let now = Instant::now();
    runtime.next_tick = now + Duration::from_secs(60);
    runtime.next_remote_observe = now + Duration::from_secs(60);
    runtime.watcher_recovery.overflow_total = 1;
    runtime.watcher_recovery.consecutive_overflows = 1;
    runtime.watcher_recovery.rearm_at = Some(now);
    runtime.watcher_recovery.rearm_failure_count = WATCHER_REARM_FAILURE_LIMIT - 1;

    runtime.poll();

    let status: serde_json::Value =
        serde_json::from_str(runtime.status_json()).expect("status json parses");
    assert_eq!(status["watcherState"]["state"], "limited");
    assert_eq!(status["watcherState"]["overflowCount"], 1);
    assert!(
        status["watcherState"]["unavailableBecause"]
            .as_str()
            .is_some_and(|reason| reason.contains("re-arm failed"))
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_rearm_delay_doubles_to_cap() {
    let delays = [1, 2, 3, 4, 5, 6, 7].map(watcher_rearm_delay);
    assert_eq!(
        delays,
        [
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(32),
            Duration::from_secs(60),
            Duration::from_secs(60),
        ],
    );
}

#[test]
fn watcher_overflow_reset_window_restores_initial_delay() {
    let fixture = watcher_fixture(
        "bowline-daemon-watch-overflow-reset",
        "ws_watch_overflow_reset",
    );
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    let first = Instant::now();
    runtime.begin_watcher_overflow_recovery(first);
    assert_eq!(runtime.watcher_recovery.consecutive_overflows, 1);
    let reset = first + WATCHER_OVERFLOW_RESET_WINDOW + Duration::from_secs(1);
    runtime.begin_watcher_overflow_recovery(reset);
    assert_eq!(runtime.watcher_recovery.consecutive_overflows, 1);
    assert_eq!(runtime.watcher_recovery.overflow_total, 2);
    runtime.begin_watcher_overflow_recovery(reset + Duration::from_secs(1));
    assert_eq!(runtime.watcher_recovery.consecutive_overflows, 2);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rearm_failures_become_limited_after_limit() {
    let temp = unique_temp_dir("bowline-daemon-watch-rearm-failure");
    let missing_root = temp.join("missing-root");
    let mut runtime = watcher_test_runtime(missing_root, temp.join(".state"), "ws_rearm_failure");
    runtime.watcher_state = WatcherRuntimeState::Rearming;
    runtime.watcher_recovery.rearm_at = Some(Instant::now());

    for attempt in 1..=WATCHER_REARM_FAILURE_LIMIT {
        let due = runtime
            .watcher_recovery
            .rearm_at
            .expect("re-arm remains scheduled before limit");
        runtime.maybe_rearm_watcher(due);
        if attempt < WATCHER_REARM_FAILURE_LIMIT {
            assert_eq!(runtime.watcher_state, WatcherRuntimeState::Rearming);
            assert!(
                runtime
                    .watcher_recovery
                    .rearm_at
                    .is_some_and(|next| next > due)
            );
        }
    }

    assert!(matches!(
        runtime.watcher_state,
        WatcherRuntimeState::Limited(ref reason) if reason.contains("re-arm failed")
    ));
    assert!(runtime.watcher_recovery.rearm_at.is_none());
    assert_eq!(runtime.watcher_component_state(), "degraded");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_state_json_reports_rearming_with_overflow_count() {
    let fixture = watcher_fixture("bowline-daemon-watch-json", "ws_watch_json");
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );

    runtime.begin_watcher_overflow_recovery(Instant::now());
    assert_eq!(runtime.watcher_component_state(), "degraded");
    let status: serde_json::Value =
        serde_json::to_value(runtime.watcher_state_json()).expect("watcher json serializes");
    assert_eq!(status["state"], "rearming");
    assert_eq!(status["overflowCount"], 1);
    assert_eq!(status["rearmAttempt"], 1);

    runtime.watcher_state = WatcherRuntimeState::Ready;
    let ready_status: serde_json::Value =
        serde_json::to_value(runtime.watcher_state_json()).expect("watcher json serializes");
    assert_eq!(ready_status["state"], "ready");
    assert_eq!(ready_status["overflowCount"], 1);

    let _ = fs::remove_dir_all(fixture.temp);
}

fn insert_completed_reconcile(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) {
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-completed-overflow".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Completed,
            idempotency_key: "daemon-sync:device-test:completed-overflow".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(device_id.clone()),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: "2999-01-01T00:00:00Z".to_string(),
            updated_at: "2999-01-01T00:00:00Z".to_string(),
        })
        .expect("completed reconcile inserted");
}
