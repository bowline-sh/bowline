use super::{
    Command, ConflictSummary, ContinuousSyncOptions, ContinuousSyncRuntime, DEFAULT_DATABASE_FILE,
    DeviceId, LocalWriteLogRecord, MetadataStore, RemoteRefObserver, STATUS_PUBLISH_INTERVAL,
    StatusPublisher, SyncExecutor, SyncFailureAction, SyncOnceArgs, SyncOnceSummary,
    SyncOperationRecord, WATCHER_DRAIN_BUDGET, WatcherRuntimeState, WatcherSignal, WorkspaceId,
    current_timestamp, hosted_sync_executor, initial_sync_status_json, parse_args,
    remote_observer_reconnect_delay, requeue_startup_sync_claims_with_resolved_attention,
    retry_delay_seconds, run_sync_once_with, runtime_error, sync_failure_action,
    sync_status_with_hosted_calls, watcher_relative_path,
};
use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneTimestamp, FakeControlPlaneClient, WorkspaceRef,
};
use bowline_core::{
    events::{EventName, EventSubjectKind},
    policy::PathClassification,
};
use bowline_local::metadata::WorkspaceSyncHeadRecord;
use bowline_storage::LocalByteStore;
use notify::{
    Event, EventKind,
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

fn noop_remote_ref_observer() -> RemoteRefObserver {
    Box::new(|_| Ok(None))
}

fn noop_status_publisher() -> StatusPublisher {
    Box::new(|_| Ok(()))
}

#[test]
fn parses_serve_once_socket() {
    let cli = parse_args([
        "serve",
        "--once",
        "--socket",
        "/tmp/bowline-daemon-test.sock",
    ]);

    assert_eq!(cli.socket, PathBuf::from("/tmp/bowline-daemon-test.sock"));
    assert_eq!(cli.command, Command::Serve { once: true });
}

#[test]
fn parses_version_flags() {
    let cli = parse_args(["--version"]);
    assert_eq!(cli.command, Command::Version);

    let cli = parse_args(["-V", "--json"]);
    assert!(cli.json);
    assert_eq!(cli.command, Command::Version);
}

#[test]
fn parses_continuous_sync_for_serve() {
    let cli = parse_args([
        "serve",
        "--sync-root",
        "/tmp/code",
        "--sync-state-root",
        "/tmp/state",
        "--sync-workspace",
        "ws_custom",
        "--sync-device",
        "device_custom",
        "--sync-interval-ms",
        "250",
        "--sync-max-ticks",
        "3",
    ]);
    let sync = cli
        .continuous_sync
        .expect("sync options should be configured");

    assert_eq!(sync.args.root, PathBuf::from("/tmp/code"));
    assert_eq!(sync.args.state_root, PathBuf::from("/tmp/state"));
    assert_eq!(sync.args.workspace_id, "ws_custom");
    assert_eq!(sync.args.device_id, "device_custom");
    assert_eq!(sync.interval, std::time::Duration::from_millis(250));
    assert_eq!(sync.max_ticks, Some(3));
}

#[test]
fn parses_notify_approvals_for_continuous_serve() {
    let cli = parse_args([
        "serve",
        "--sync-root",
        "/tmp/code",
        "--sync-state-root",
        "/tmp/state",
        "--notify-approvals",
    ]);

    assert!(cli.notify_approvals);
    assert_eq!(cli.command, Command::Serve { once: false });
    assert!(cli.continuous_sync.is_some());
}

#[test]
fn watcher_error_wakes_reconciliation_and_marks_watcher_limited() {
    let (signal_tx, signal_rx) = mpsc::channel();
    signal_tx
        .send(WatcherSignal::Limited("watch queue overflow".to_string()))
        .expect("watcher signal sends");
    let mut runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: PathBuf::from("/tmp/bowline-root"),
                state_root: PathBuf::from("/tmp/bowline-state"),
                workspace_id: "ws_code".to_string(),
                device_id: "device-test".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(2),
            max_ticks: None,
        },
        next_tick: Instant::now() + Duration::from_secs(60),
        next_remote_observe: Instant::now() + Duration::from_secs(60),
        tick_count: 0,
        last_json: "{\"state\":\"queued\",\"tickCount\":0}".to_string(),
        watcher: None,
        change_rx: Some(signal_rx),
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    let drained = runtime.drain_changes();
    assert!(drained.changed);
    assert!(drained.sync_now);
    assert!(matches!(
        runtime.watcher_state,
        WatcherRuntimeState::Limited(ref reason) if reason.contains("overflow")
    ));
}

#[test]
fn watcher_drain_disables_saturated_queue_to_keep_daemon_responsive() {
    let (signal_tx, signal_rx) = mpsc::channel();
    for index in 0..=WATCHER_DRAIN_BUDGET {
        signal_tx
            .send(WatcherSignal::Limited(format!(
                "watch queue overflow {index}"
            )))
            .expect("watcher signal sends");
    }
    let mut runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: PathBuf::from("/tmp/bowline-root"),
                state_root: PathBuf::from("/tmp/bowline-state"),
                workspace_id: "ws_code".to_string(),
                device_id: "device-test".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(2),
            max_ticks: None,
        },
        next_tick: Instant::now() + Duration::from_secs(60),
        next_remote_observe: Instant::now() + Duration::from_secs(60),
        tick_count: 0,
        last_json: "{\"state\":\"queued\",\"tickCount\":0}".to_string(),
        watcher: None,
        change_rx: Some(signal_rx),
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    let first = runtime.drain_changes();
    let second = runtime.drain_changes();

    assert!(first.changed);
    assert!(first.sync_now);
    assert!(matches!(
        runtime.watcher_state,
        WatcherRuntimeState::Limited(ref reason) if reason.contains("saturated")
    ));
    assert!(runtime.change_rx.is_none());
    assert!(!second.changed);
}

#[test]
fn initial_sync_status_reports_limited_watcher() {
    let status = initial_sync_status_json(&WatcherRuntimeState::Limited(
        "watch backend unavailable".to_string(),
    ));

    assert_eq!(
        status,
        "{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{\"state\":\"limited\",\"unavailableBecause\":\"watch backend unavailable\"}}"
    );
}

#[test]
fn sync_status_includes_hosted_call_budget_snapshot() {
    let status = sync_status_with_hosted_calls(
        "{\"state\":\"idle\",\"tickCount\":1,\"watcherState\":{\"state\":\"ready\"}}",
    );
    let parsed: serde_json::Value = serde_json::from_str(&status).expect("status remains json");

    assert_eq!(parsed["state"], "idle");
    assert!(parsed["hostedCalls"]["total"].is_u64());
    assert!(parsed["hostedCalls"]["functions"].is_array());
}

#[test]
fn watcher_edit_sets_settle_window_without_immediate_sync() {
    let fixture = watcher_fixture("bowline-daemon-watch-settle", "ws_watch_settle");
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("apps/web/src")).expect("root dirs");
    let changed_path = root.join("apps/web/src/auth.ts");
    fs::write(&changed_path, "export const ok = true;\n").expect("file");
    let (signal_tx, signal_rx) = mpsc::channel();
    signal_tx
        .send(WatcherSignal::Changed(
            Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
        ))
        .expect("watcher signal sends");
    let original_tick = Instant::now() + Duration::from_secs(60);
    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime.next_tick = original_tick;
    runtime.change_rx = Some(signal_rx);

    runtime.poll();

    assert_eq!(runtime.tick_count, 0);
    assert!(runtime.next_tick > Instant::now());
    assert!(runtime.next_tick < original_tick);

    let writes = fixture
        .store
        .local_write_log(&fixture.workspace_id)
        .expect("write log");
    assert_eq!(writes.len(), 1);

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_event_records_durable_local_write_observation() {
    let fixture = watcher_fixture("bowline-daemon-watch-write", "ws_watch");
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("apps/web/src")).expect("root dirs");
    let changed_path = root.join("apps/web/src/auth.ts");
    fs::write(&changed_path, "export const ok = true;\n").expect("file");

    let runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime
        .record_watcher_event(
            &Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
        )
        .expect("event records");

    let writes = fixture
        .store
        .local_write_log(&fixture.workspace_id)
        .expect("write log");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].path, "apps/web/src/auth.ts");
    assert_eq!(writes[0].operation, "create");
    assert_eq!(
        writes[0].policy_classification,
        PathClassification::WorkspaceSync
    );

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_event_ignores_private_bowline_state() {
    let fixture = watcher_fixture("bowline-daemon-watch-private", "ws_watch_private");
    let root = fixture.root.clone();
    fs::create_dir_all(root.join(".bowline")).expect("private dir");
    let private_path = root.join(".bowline/local.sqlite3");
    fs::write(&private_path, "state").expect("private file");

    let runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime
        .record_watcher_event(
            &Event::new(EventKind::Remove(RemoveKind::File)).add_path(private_path),
        )
        .expect("private event ignored");

    let writes = fixture
        .store
        .local_write_log(&fixture.workspace_id)
        .expect("write log");
    assert!(writes.is_empty());

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_rename_records_source_and_target_once() {
    let fixture = watcher_fixture("bowline-daemon-watch-rename", "ws_watch_rename");
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("apps/web/src")).expect("root dirs");
    let old_path = root.join("apps/web/src/old.ts");
    let new_path = root.join("apps/web/src/new.ts");
    fs::write(&new_path, "renamed\n").expect("renamed file");

    let runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime
        .record_watcher_event(
            &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(old_path)
                .add_path(new_path),
        )
        .expect("rename records");

    let writes = fixture
        .store
        .local_write_log(&fixture.workspace_id)
        .expect("write log");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].operation, "rename");
    assert_eq!(
        writes[0].source_path.as_deref(),
        Some("apps/web/src/old.ts")
    );
    assert_eq!(writes[0].path, "apps/web/src/new.ts");

    let _ = fs::remove_dir_all(fixture.temp);
}

#[test]
fn watcher_relative_path_rejects_absolute_paths_outside_root() {
    assert_eq!(
        watcher_relative_path(
            PathBuf::from("/tmp/Code").as_path(),
            PathBuf::from("/etc/passwd").as_path()
        ),
        None
    );
}

#[test]
fn completed_sync_records_remote_ref_cursor() {
    let temp = unique_temp_dir("bowline-daemon-remote-cursor");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_remote_cursor");
    let runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: temp.join("Code"),
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    let summary = SyncOnceSummary {
        workspace_id: workspace_id.as_str().to_string(),
        snapshot_id: "snap-42".to_string(),
        version: 42,
        object_manifest_id: "none".to_string(),
        manifest_object_key: "none".to_string(),
        pack_object_keys: Vec::new(),
        stale: false,
        merged: false,
        conflict_count: 0,
        conflicts: Vec::new(),
    };
    runtime.record_remote_ref_cursor(&summary);
    runtime.complete_daemon_sync_operation("op-complete-sync-event", &summary);

    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    let cursor = store
        .remote_ref_cursor(&workspace_id)
        .expect("cursor reads")
        .expect("cursor stored");
    assert_eq!(cursor.last_observed_version, Some(42));
    assert_eq!(cursor.last_observed_snapshot_id.as_deref(), Some("snap-42"));
    assert_eq!(
        runtime.remote_head_json(),
        "{\"workspaceId\":\"ws_remote_cursor\",\"snapshotId\":\"snap-42\",\"version\":42}"
    );
    let events = store.list_events(20).expect("events read");
    let event = events
        .iter()
        .find(|event| event.name == EventName::SyncCompleted)
        .expect("sync completed event");
    assert_eq!(event.payload["outcome"], "no-changes");
    assert_eq!(event.payload["version"], 42);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn conflicted_sync_emits_conflict_created_event() {
    let temp = unique_temp_dir("bowline-daemon-conflict-event");
    let state_root = temp.join(".state");
    fs::create_dir_all(&state_root).expect("state root");
    let workspace_id = WorkspaceId::new("ws_conflict_event");
    let runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: temp.join("Code"),
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    let summary = SyncOnceSummary {
        workspace_id: workspace_id.as_str().to_string(),
        snapshot_id: "snap-base".to_string(),
        version: 7,
        object_manifest_id: "none".to_string(),
        manifest_object_key: "none".to_string(),
        pack_object_keys: Vec::new(),
        stale: true,
        merged: false,
        conflict_count: 1,
        conflicts: vec![ConflictSummary {
            id: "conflict_app_src_main".to_string(),
            paths: vec!["app/src/main.ts".to_string()],
        }],
    };

    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    store
        .insert_workspace(&workspace_id, "Code", "2026-06-27T00:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &runtime.options.args.root.display().to_string(),
            "2026-06-27T00:00:00Z",
        )
        .expect("root");
    runtime.append_sync_completed_event(
        &store,
        "op-conflicted-sync-event",
        &summary,
        "2026-06-27T00:00:00Z",
    );

    let events = store.list_events(20).expect("events read");
    assert!(
        events
            .iter()
            .any(|event| event.name == EventName::SyncCompleted
                && event.payload["outcome"] == "conflicted"
                && event.payload["conflictCount"] == 1),
        "{events:?}"
    );
    let conflict = events
        .iter()
        .find(|event| event.name == EventName::ConflictCreated)
        .expect("conflict event");
    assert_eq!(conflict.path.as_deref(), Some("app/src/main.ts"));
    assert_eq!(conflict.payload["conflictId"], "conflict_app_src_main");
    assert!(
        conflict
            .subject
            .as_ref()
            .is_some_and(|subject| subject.kind == EventSubjectKind::Conflict
                && subject.id == "conflict_app_src_main")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_requeues_expired_claims_before_next_sync() {
    let temp = unique_temp_dir("bowline-daemon-requeue-expired");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_requeue");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-expired".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "upload".to_string(),
            state: "claimed".to_string(),
            idempotency_key: "expired".to_string(),
            base_version: Some(1),
            base_snapshot_id: Some("snap-1".to_string()),
            target_snapshot_id: Some("snap-2".to_string()),
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: Some("dead-daemon".to_string()),
            heartbeat_at: Some("1970-01-01T00:00:00Z".to_string()),
            next_attempt_at: None,
            last_error: None,
            created_at: "1970-01-01T00:00:00Z".to_string(),
            updated_at: "1970-01-01T00:00:00Z".to_string(),
        })
        .expect("operation queued");
    let runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: temp.join("Code"),
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    runtime.requeue_expired_sync_claims();

    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations[0].state, "queued");
    assert_eq!(operations[0].claimed_by, None);
    assert_eq!(operations[0].heartbeat_at, None);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_restart_idles_after_recent_completed_tick_operation() {
    let temp = unique_temp_dir("bowline-daemon-restart-operation-id");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_restart_operation_id");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-tick-1".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "completed".to_string(),
            idempotency_key: "daemon-sync:device-test:1".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: None,
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
        })
        .expect("completed operation inserted");
    let runtime = watcher_test_runtime(
        temp.join("Code"),
        state_root.clone(),
        "ws_restart_operation_id",
    );

    assert_eq!(runtime.claim_daemon_sync_operation(), None);

    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, "completed");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_poll_idles_without_running_sync_once_when_no_work_exists() {
    let temp = unique_temp_dir("bowline-daemon-poll-idle-budget");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_poll_idle_budget");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-completed".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "completed".to_string(),
            idempotency_key: "daemon-sync:device-a:completed".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: None,
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
        })
        .expect("completed operation inserted");
    let sync_calls = Arc::new(Mutex::new(0_u64));
    let sync_calls_for_executor = Arc::clone(&sync_calls);
    let mut runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(3600),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: Box::new(move |_, _| {
            *sync_calls_for_executor
                .lock()
                .expect("sync call count lock") += 1;
            Err(runtime_error("idle poll must not run sync-once"))
        }),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    runtime.poll();

    assert_eq!(
        *sync_calls.lock().expect("sync call count lock"),
        0,
        "idle daemon poll must not call hosted sync work"
    );
    assert_eq!(runtime.tick_count, 1);
    assert!(
        runtime.status_json().contains("\"state\":\"idle\""),
        "{}",
        runtime.status_json()
    );
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, "completed");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_claims_reconcile_when_local_write_is_newer_than_completed_tick() {
    let temp = unique_temp_dir("bowline-daemon-local-write-reconcile");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_local_write_reconcile");
    let device_id = DeviceId::new("device-test");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-completed".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "completed".to_string(),
            idempotency_key: "daemon-sync:device-test:completed".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(device_id.clone()),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: None,
            created_at: "2999-01-01T00:00:00Z".to_string(),
            updated_at: "2999-01-01T00:00:00Z".to_string(),
        })
        .expect("completed operation inserted");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-after-completed".to_string(),
            workspace_id: workspace_id.clone(),
            device_id,
            project_id: None,
            path: "apps/web/src/main.ts".to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "watch-test".to_string(),
            settled_at: "2999-01-01T00:00:01Z".to_string(),
            created_at: "2999-01-01T00:00:01Z".to_string(),
        })
        .expect("local write inserted");
    let runtime = watcher_test_runtime(
        temp.join("Code"),
        state_root.clone(),
        "ws_local_write_reconcile",
    );

    let claimed = runtime
        .claim_daemon_sync_operation()
        .expect("local write queues sync");

    assert_ne!(claimed, "daemon-sync-completed");
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 2);
    assert!(
        operations
            .iter()
            .any(|operation| operation.state == "claimed")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_claims_reconcile_when_remote_observer_advances_cursor() {
    let temp = unique_temp_dir("bowline-daemon-remote-observer-reconcile");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_remote_observer_reconcile");
    let device_id = DeviceId::new("device-test");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-completed".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "completed".to_string(),
            idempotency_key: "daemon-sync:device-test:completed".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(device_id),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: None,
            created_at: "2999-01-01T00:00:00Z".to_string(),
            updated_at: "2999-01-01T00:00:00Z".to_string(),
        })
        .expect("completed operation inserted");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: workspace_id.as_str().to_string(),
                version: 1,
                snapshot_id: "snap-local".to_string(),
                updated_at: ControlPlaneTimestamp { tick: 1 },
                updated_by_device_id: Some("device-a".to_string()),
            },
            observed_at: "2999-01-01T00:00:00Z".to_string(),
        })
        .expect("local head inserted");
    let mut runtime = watcher_test_runtime(
        temp.join("Code"),
        state_root.clone(),
        "ws_remote_observer_reconcile",
    );
    runtime.remote_ref_observer = Box::new(|_| {
        Ok(Some(WorkspaceRef {
            workspace_id: "ws_remote_observer_reconcile".to_string(),
            version: 2,
            snapshot_id: "snap-remote".to_string(),
            updated_at: ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some("device-b".to_string()),
        }))
    });

    assert!(runtime.observe_remote_ref_cursor());
    let claimed = runtime
        .claim_daemon_sync_operation()
        .expect("remote cursor advance queues sync");

    assert_ne!(claimed, "daemon-sync-completed");
    let cursor = store
        .remote_ref_cursor(&workspace_id)
        .expect("cursor reads")
        .expect("cursor exists");
    assert_eq!(cursor.last_observed_version, Some(2));
    assert_eq!(
        cursor.last_observed_snapshot_id.as_deref(),
        Some("snap-remote")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_clears_observed_base_ref_when_remote_observer_has_no_ref() {
    let temp = unique_temp_dir("bowline-daemon-clear-observed-ref");
    let state_root = temp.join(".state");
    let workspace_id = "ws_clear_observed_ref";
    let mut runtime = watcher_test_runtime(temp.join("Code"), state_root, workspace_id);
    runtime.latest_observed_ref = Some(WorkspaceRef {
        workspace_id: workspace_id.to_string(),
        version: 2,
        snapshot_id: "snap-stale".to_string(),
        updated_at: ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    });
    runtime.remote_ref_observer = Box::new(|_| Ok(None));

    assert!(!runtime.observe_remote_ref_cursor());
    assert_eq!(runtime.latest_observed_ref, None);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_startup_requeues_own_claimed_tick_without_waiting_for_timeout() {
    let temp = unique_temp_dir("bowline-daemon-startup-requeue-claimed");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_startup_requeue_claimed");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-before-restart".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "claimed".to_string(),
            idempotency_key: "daemon-sync:device-test:claimed-before-restart".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: Some("old-daemon-process".to_string()),
            heartbeat_at: Some("2999-01-01T00:00:00Z".to_string()),
            next_attempt_at: None,
            last_error: None,
            created_at: "2026-06-26T00:00:00Z".to_string(),
            updated_at: "2026-06-26T00:00:00Z".to_string(),
        })
        .expect("claimed operation inserted");

    let runtime = ContinuousSyncRuntime::new(ContinuousSyncOptions {
        args: SyncOnceArgs {
            root,
            state_root: state_root.clone(),
            workspace_id: workspace_id.as_str().to_string(),
            device_id: "device-test".to_string(),
            sync_operation_id: None,
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    });

    let operation = store
        .sync_operation_by_id("daemon-sync-before-restart")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, "queued");
    assert_eq!(operation.claimed_by, None);
    assert_eq!(operation.heartbeat_at, None);

    let claimed = runtime
        .claim_daemon_sync_operation()
        .expect("restarted daemon claims abandoned operation");
    assert_eq!(claimed, "daemon-sync-before-restart");
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, "claimed");
    assert_eq!(operations[0].claimed_by.as_deref(), Some("device-test"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_startup_requeues_own_retry_after_restart() {
    let temp = unique_temp_dir("bowline-daemon-startup-requeue-retry");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_startup_requeue_retry");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-before-repair".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "waiting_retry".to_string(),
            idempotency_key: "daemon-sync:device-test:retry-before-repair".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 4,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
            last_error: Some(
                "daemon sync requires BOWLINE_ACCOUNT_SESSION_ID, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session"
                    .to_string(),
            ),
            created_at: "2026-06-26T00:00:00Z".to_string(),
            updated_at: "2026-06-26T00:00:00Z".to_string(),
        })
        .expect("retry operation inserted");

    let options = ContinuousSyncOptions {
        args: SyncOnceArgs {
            root,
            state_root: state_root.clone(),
            workspace_id: workspace_id.as_str().to_string(),
            device_id: "device-test".to_string(),
            sync_operation_id: None,
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    };
    requeue_startup_sync_claims_with_resolved_attention(&options, true, false);

    let operation = store
        .sync_operation_by_id("daemon-sync-before-repair")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, "queued");
    assert_eq!(operation.next_attempt_at, None);
    assert_eq!(
        operation.last_error.as_deref(),
        Some(
            "daemon sync requires BOWLINE_ACCOUNT_SESSION_ID, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session"
        )
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_startup_requeues_resolved_missing_convex_attention() {
    let temp = unique_temp_dir("bowline-daemon-startup-requeue-attention");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_startup_requeue_attention");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-missing-convex".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "attention".to_string(),
            idempotency_key: "daemon-sync:device-test:missing-convex".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: Some("CONVEX_URL is required for daemon sync".to_string()),
            created_at: "2026-06-26T00:00:00Z".to_string(),
            updated_at: "2026-06-26T00:00:00Z".to_string(),
        })
        .expect("attention operation inserted");

    let options = ContinuousSyncOptions {
        args: SyncOnceArgs {
            root,
            state_root: state_root.clone(),
            workspace_id: workspace_id.as_str().to_string(),
            device_id: "device-test".to_string(),
            sync_operation_id: None,
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    };
    requeue_startup_sync_claims_with_resolved_attention(&options, true, false);

    let operation = store
        .sync_operation_by_id("daemon-sync-missing-convex")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, "queued");
    assert_eq!(operation.last_error, None);
    assert_eq!(operation.next_attempt_at, None);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_startup_requeues_resolved_missing_workspace_key_attention() {
    let temp = unique_temp_dir("bowline-daemon-startup-requeue-workspace-key");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_startup_requeue_workspace_key");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-missing-workspace-key".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "attention".to_string(),
            idempotency_key: "daemon-sync:device-test:missing-workspace-key".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: Some("workspace key is missing; approve this device".to_string()),
            created_at: "2026-06-26T00:00:00Z".to_string(),
            updated_at: "2026-06-26T00:00:00Z".to_string(),
        })
        .expect("attention operation inserted");

    let options = ContinuousSyncOptions {
        args: SyncOnceArgs {
            root,
            state_root: state_root.clone(),
            workspace_id: workspace_id.as_str().to_string(),
            device_id: "device-test".to_string(),
            sync_operation_id: None,
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    };
    requeue_startup_sync_claims_with_resolved_attention(&options, false, true);

    let operation = store
        .sync_operation_by_id("daemon-sync-missing-workspace-key")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, "queued");
    assert_eq!(operation.last_error, None);
    assert_eq!(operation.next_attempt_at, None);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn missing_remote_bytes_are_reported_as_offline_sync_work() {
    assert_eq!(
        sync_failure_action("snapshot manifest `snap_missing` was not found"),
        SyncFailureAction::Offline
    );
    assert_eq!(
        sync_failure_action("missing object for object `packs_pk_missing`"),
        SyncFailureAction::Offline
    );
    assert_eq!(
        sync_failure_action(
            "R2 download for object `packs_pk_missing` returned HTTP 404 Not Found"
        ),
        SyncFailureAction::Offline
    );
    assert_eq!(
        sync_failure_action("corrupt object `packs_pk_bad`: object bytes did not match metadata"),
        SyncFailureAction::Retry
    );
    assert_eq!(
        sync_failure_action("CONVEX_URL is required for daemon sync"),
        SyncFailureAction::Attention
    );
}

#[test]
fn retry_backoff_is_bounded_and_increases() {
    let first = retry_delay_seconds("op-retry", 1);
    let second = retry_delay_seconds("op-retry", 2);
    let late = retry_delay_seconds("op-retry", 99);

    assert!((2..=5).contains(&first));
    assert!(second >= first);
    assert_eq!(late, 60);
}

#[test]
fn remote_observer_reconnect_backoff_is_bounded() {
    assert_eq!(remote_observer_reconnect_delay(1), Duration::from_secs(30));
    assert_eq!(remote_observer_reconnect_delay(2), Duration::from_secs(60));
    assert_eq!(
        remote_observer_reconnect_delay(99),
        Duration::from_secs(900)
    );
}

#[test]
fn daemon_routes_missing_remote_bytes_to_offline_queue_state() {
    let temp = unique_temp_dir("bowline-daemon-missing-remote");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_missing_remote");
    let operation_id = "op-missing-remote";
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: "download".to_string(),
            state: "claimed".to_string(),
            idempotency_key: "missing-remote".to_string(),
            base_version: Some(1),
            base_snapshot_id: Some("snap-1".to_string()),
            target_snapshot_id: Some("snap-2".to_string()),
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: Some("daemon-test".to_string()),
            heartbeat_at: Some("2026-06-26T12:00:00Z".to_string()),
            next_attempt_at: None,
            last_error: None,
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("operation queued");

    let runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: temp.join("Code"),
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    let before = OffsetDateTime::now_utc();
    runtime.fail_daemon_sync_operation(
        operation_id,
        "snapshot manifest `snap_missing` was not found",
    );

    let counts = store
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(counts.blocked_offline, 1);
    let operation = store
        .sync_operation_by_id(operation_id)
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, "blocked_offline");
    let next_attempt = OffsetDateTime::parse(
        operation
            .next_attempt_at
            .as_deref()
            .expect("offline retry time is set"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("offline retry time parses");
    assert!(next_attempt > before);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_does_not_bypass_pending_backoff_with_fresh_reconcile_rows() {
    let temp = unique_temp_dir("bowline-daemon-no-backoff-bypass");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_no_backoff_bypass");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-blocked".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "blocked_offline".to_string(),
            idempotency_key: "blocked-reconcile".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
            last_error: Some("offline".to_string()),
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("operation queued");

    let runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: temp.join("Code"),
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 42,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    assert_eq!(runtime.claim_daemon_sync_operation(), None);
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].id, "op-blocked");
    assert_eq!(operations[0].state, "blocked_offline");

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_poll_waits_for_backoff_instead_of_running_sync_once() {
    let temp = unique_temp_dir("bowline-daemon-poll-backoff");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_poll_backoff");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-blocked".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "blocked_offline".to_string(),
            idempotency_key: "blocked-reconcile".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
            last_error: Some("offline".to_string()),
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("operation queued");

    let mut runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    runtime.poll();

    assert!(runtime.status_json().contains("\"state\":\"limited\""));
    assert!(
        runtime
            .status_json()
            .contains("sync queue is waiting for offline recovery")
    );
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, "blocked_offline");
    assert_eq!(operations[0].last_error.as_deref(), Some("offline"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_poll_reports_attention_queue_truthfully() {
    let temp = unique_temp_dir("bowline-daemon-poll-attention");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_poll_attention");
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-attention".to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "attention".to_string(),
            idempotency_key: "attention-reconcile".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: Some("trusted device required".to_string()),
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("operation queued");

    let mut runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    runtime.poll();

    assert!(runtime.status_json().contains("\"state\":\"attention\""));
    assert!(runtime.status_json().contains("sync queue needs attention"));
    assert!(
        runtime
            .status_json()
            .contains("\"blockedAction\":\"resolve sync queue attention\"")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_retry_failures_wait_before_next_attempt() {
    let temp = unique_temp_dir("bowline-daemon-retry-backoff");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_retry_backoff");
    let operation_id = "op-retry-backoff";
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: "upload".to_string(),
            state: "claimed".to_string(),
            idempotency_key: "retry-backoff".to_string(),
            base_version: Some(1),
            base_snapshot_id: Some("snap-1".to_string()),
            target_snapshot_id: Some("snap-2".to_string()),
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 3,
            claimed_by: Some("daemon-test".to_string()),
            heartbeat_at: Some("2026-06-26T12:00:00Z".to_string()),
            next_attempt_at: None,
            last_error: None,
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("operation queued");

    let runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: temp.join("Code"),
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-a".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    };

    let before = OffsetDateTime::now_utc();
    runtime.fail_daemon_sync_operation(
        operation_id,
        "corrupt object `packs_pk_bad`: object bytes did not match metadata",
    );

    let operation = store
        .sync_operation_by_id(operation_id)
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, "waiting_retry");
    let events = store.list_events(20).expect("events read");
    let event = events
        .iter()
        .find(|event| event.name == EventName::SyncLimited)
        .expect("sync limited event");
    assert_eq!(event.payload["outcome"], "retry");
    assert!(
        !serde_json::to_string(event)
            .expect("event json")
            .contains("corrupt object"),
        "sync event must not include raw error text"
    );
    let next_attempt = OffsetDateTime::parse(
        operation
            .next_attempt_at
            .as_deref()
            .expect("retry time is set"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("retry time parses");
    assert!(next_attempt > before + time::Duration::seconds(7));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn two_fake_daemon_loops_sync_edit_without_manual_sync_once() {
    let temp = unique_temp_dir("bowline-daemon-two-loop");
    let workspace_id = "ws_two_daemon_loop";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/loop.txt");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "initial daemon loop\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store =
        Arc::new(LocalByteStore::open_deterministic(temp.join("objects"), 41).expect("byte store"));
    let workspace_key = [41_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state.clone(),
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state.clone(),
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "initial daemon loop"),
        "device B initial materialization",
    );

    fs::write(
        a_root.join(&note_path),
        "initial daemon loop\nlive edit from daemon A\n",
    )
    .expect("edit file");

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 2,
        "device A edit upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "live edit from daemon A"),
        "device B edit materialization",
    );

    assert!(
        daemon_a.status_json().contains("\"state\":\"idle\""),
        "{}",
        daemon_a.status_json()
    );
    assert!(
        daemon_b.status_json().contains("\"state\":\"idle\""),
        "{}",
        daemon_b.status_json()
    );
    let a_checkpoints = checkpoint_steps(&a_state);
    for expected in [
        "snapshot-candidate-built",
        "source-pack-uploaded",
        "snapshot-manifest-uploaded",
        "object-manifest-committed",
        "workspace-ref-advanced",
    ] {
        assert!(
            a_checkpoints.iter().any(|step| step == expected),
            "missing device A checkpoint {expected}; got {a_checkpoints:?}"
        );
    }
    let b_checkpoints = checkpoint_steps(&b_state);
    for expected in ["remote-import-started", "remote-materialized"] {
        assert!(
            b_checkpoints.iter().any(|step| step == expected),
            "missing device B checkpoint {expected}; got {b_checkpoints:?}"
        );
    }

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn restarted_daemon_reconciles_real_directory_edit_without_data_loss() {
    let temp = unique_temp_dir("bowline-daemon-restart-real-root-edit");
    let workspace_id = "ws_two_daemon_loop_restart";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/restart.txt");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "initial before restart\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store =
        Arc::new(LocalByteStore::open_deterministic(temp.join("objects"), 43).expect("byte store"));
    let workspace_key = [43_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state.clone(),
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state,
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "initial before restart"),
        "device B initial materialization",
    );

    drop(daemon_a);
    fs::write(
        a_root.join(&note_path),
        "initial before restart\nedit while daemon was down\n",
    )
    .expect("edit real file while daemon is down");
    let mut restarted_daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state,
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut restarted_daemon_a,
        |runtime| sync_status_version(runtime) >= 2,
        "restarted device A upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "edit while daemon was down"),
        "device B materializes edit from restarted daemon",
    );

    assert!(
        file_contains(&a_root.join(&note_path), "edit while daemon was down"),
        "restarted sync must never roll back the local real-directory edit"
    );
    assert!(
        restarted_daemon_a
            .status_json()
            .contains("\"state\":\"idle\""),
        "{}",
        restarted_daemon_a.status_json()
    );
    assert!(
        daemon_b.status_json().contains("\"state\":\"idle\""),
        "{}",
        daemon_b.status_json()
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn restarted_daemon_adopts_materialized_remote_head_without_reupload() {
    let temp = unique_temp_dir("bowline-daemon-restart-adopt-materialized");
    let workspace_id = "ws_two_daemon_loop_adopt";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/adopt.txt");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "remote materialized bytes\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store =
        Arc::new(LocalByteStore::open_deterministic(temp.join("objects"), 44).expect("byte store"));
    let workspace_key = [44_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state,
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state.clone(),
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "remote materialized bytes"),
        "device B initial materialization",
    );
    let remote_before = control_plane
        .lock()
        .expect("fake control plane lock")
        .get_workspace_ref(workspace_id)
        .expect("remote ref reads")
        .expect("remote ref exists");
    assert_eq!(remote_before.version, 1);

    drop(daemon_b);
    fs::remove_file(b_state.join(DEFAULT_DATABASE_FILE)).expect("remove local metadata db");
    let mut restarted_daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state.clone(),
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut restarted_daemon_b,
        |runtime| sync_status_version(runtime) >= 1,
        "restarted device B adopts materialized remote head",
    );

    let remote_after = control_plane
        .lock()
        .expect("fake control plane lock")
        .get_workspace_ref(workspace_id)
        .expect("remote ref reads")
        .expect("remote ref exists");
    assert_eq!(
        remote_after.version, remote_before.version,
        "materialized remote bytes must not be uploaded as a new workspace version"
    );
    assert_eq!(remote_after.snapshot_id, remote_before.snapshot_id);
    let recovered_store =
        MetadataStore::open(b_state.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    let recovered_head = recovered_store
        .workspace_sync_head(&WorkspaceId::new(workspace_id))
        .expect("head reads")
        .expect("head restored");
    assert_eq!(
        recovered_head.workspace_ref.snapshot_id,
        remote_before.snapshot_id
    );
    assert_eq!(recovered_head.workspace_ref.version, remote_before.version);
    assert!(
        file_contains(&b_root.join(&note_path), "remote materialized bytes"),
        "restart must preserve the real-directory bytes"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn two_fake_daemon_loops_sync_safe_save_without_temp_churn() {
    let temp = unique_temp_dir("bowline-daemon-two-loop-safe-save");
    let workspace_id = "ws_two_daemon_loop_safe_save";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/safe-save.txt");
    let temp_path = PathBuf::from("project/notes/.safe-save.txt.tmp");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "initial safe save\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store =
        Arc::new(LocalByteStore::open_deterministic(temp.join("objects"), 42).expect("byte store"));
    let workspace_key = [42_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state,
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state,
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "initial safe save"),
        "device B initial materialization",
    );

    fs::write(a_root.join(&temp_path), "safe-save final bytes\n").expect("temp write");
    fs::rename(a_root.join(&temp_path), a_root.join(&note_path)).expect("safe-save rename");

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 2,
        "device A safe-save upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "safe-save final bytes"),
        "device B safe-save materialization",
    );

    assert!(
        !b_root.join(&temp_path).exists(),
        "safe-save temp path should not materialize remotely"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn parses_status_json() {
    let cli = parse_args(["status", "--json"]);

    assert!(cli.json);
    assert_eq!(cli.command, Command::Status);
}

fn watcher_test_runtime(
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: &str,
) -> ContinuousSyncRuntime {
    ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root,
                workspace_id: workspace_id.to_string(),
                device_id: "device-test".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    }
}

fn fake_daemon_runtime(
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: &str,
    device_id: &str,
    control_plane: Arc<Mutex<FakeControlPlaneClient>>,
    byte_store: Arc<LocalByteStore>,
    workspace_key: [u8; 32],
) -> ContinuousSyncRuntime {
    ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root,
                workspace_id: workspace_id.to_string(),
                device_id: device_id.to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_millis(0),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        sync_once: fake_sync_executor(control_plane, byte_store, workspace_key),
        remote_ref_observer: noop_remote_ref_observer(),
        latest_observed_ref: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
    }
}

fn fake_sync_executor(
    control_plane: Arc<Mutex<FakeControlPlaneClient>>,
    byte_store: Arc<LocalByteStore>,
    workspace_key: [u8; 32],
) -> SyncExecutor {
    Box::new(move |args, observed_base_ref| {
        let workspace_id = WorkspaceId::new(args.workspace_id.clone());
        let device_id = DeviceId::new(args.device_id.clone());
        let control_plane = control_plane
            .lock()
            .map_err(|_| runtime_error("fake control plane lock poisoned"))?;
        let base_ref = match observed_base_ref {
            Some(workspace_ref) => workspace_ref,
            None => match control_plane.get_workspace_ref(workspace_id.as_str())? {
                Some(workspace_ref) => workspace_ref,
                None => control_plane.create_workspace_ref(workspace_id.as_str())?,
            },
        };
        run_sync_once_with(
            args,
            &*control_plane,
            &*byte_store,
            base_ref,
            workspace_id,
            device_id,
            workspace_key,
        )
    })
}

fn poll_until(
    runtime: &mut ContinuousSyncRuntime,
    condition: impl Fn(&ContinuousSyncRuntime) -> bool,
    label: &str,
) {
    for _ in 0..20 {
        runtime.next_tick = Instant::now();
        runtime.poll();
        if condition(runtime) {
            return;
        }
    }
    panic!(
        "{label} did not complete; last status {}",
        runtime.status_json()
    );
}

fn sync_status_version(runtime: &ContinuousSyncRuntime) -> u64 {
    serde_json::from_str::<serde_json::Value>(runtime.status_json())
        .ok()
        .and_then(|value| value["version"].as_u64())
        .unwrap_or_default()
}

fn file_contains(path: &std::path::Path, needle: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|content| content.contains(needle))
}

fn checkpoint_steps(state_root: &Path) -> Vec<String> {
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("daemon metadata opens");
    store
        .sync_operations(&WorkspaceId::new("ws_two_daemon_loop"))
        .expect("sync operations")
        .into_iter()
        .flat_map(|operation| {
            store
                .sync_operation_checkpoints(&operation.id)
                .expect("checkpoints")
                .into_iter()
                .map(|checkpoint| checkpoint.step)
                .collect::<Vec<_>>()
        })
        .collect()
}

struct WatcherFixture {
    temp: PathBuf,
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: WorkspaceId,
    store: MetadataStore,
}

fn watcher_fixture(label: &str, workspace_id: &str) -> WatcherFixture {
    let temp = unique_temp_dir(label);
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root dir");
    let workspace_id = WorkspaceId::new(workspace_id);
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "Code", "2026-06-26T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root-code",
            &workspace_id,
            &root.display().to_string(),
            "2026-06-26T12:00:00Z",
        )
        .expect("root");
    WatcherFixture {
        temp,
        root,
        state_root,
        workspace_id,
        store,
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("{label}-{suffix}"));
    fs::create_dir_all(&path).expect("temp dir");
    path
}
