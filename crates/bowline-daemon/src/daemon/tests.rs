use super::protocol::{SocketGuard, prepare_socket};
use super::store_health::StoreHealth;
use super::sync::{
    ClaimLeasePolicy, ClaimLeaseSupervisor, ClaimOwnership, LocalWorkspaceKey, OwnedThreadMetrics,
    SyncComponentState, SyncScanSummary, SyncSummaryOutcome,
};
use super::{
    CachedStore, Command, ConflictSummary, ContinuousSyncOptions, ContinuousSyncRuntime,
    DEFAULT_DATABASE_FILE, DaemonRuntime, DaemonServerState, DeviceId, DispatchClaimer,
    LocalWriteLogRecord, MetadataStore, NotificationDedupe, RemoteObserverState, RemoteRefObserver,
    STATUS_PUBLISH_INTERVAL, StatusPublishOutcome, StatusPublishPayload, StatusPublishRequest,
    StatusPublisher, SyncExecutor, SyncFailureAction, SyncOnceArgs, SyncOnceError, SyncOnceSummary,
    SyncOperationKind, SyncOperationRecord, SyncOperationState, SyncResourceKey,
    WATCHER_DRAIN_BUDGET, WATCHER_OVERFLOW_RESET_WINDOW, WATCHER_REARM_FAILURE_LIMIT,
    WatcherRecovery, WatcherRuntimeState, WatcherSignal, WorkViewOverlaySyncResult, WorkspaceId,
    claim_pending_dispatched_lease_with, current_timestamp, daemon_env_var, drain_policy,
    handshake, hosted_sync_executor, initial_sync_status_json, invalidate_policy_cache_for_path,
    load_persisted_daemon_env, local_metadata_sweep_due, noop_dispatch_claimer,
    open_store_for_test, parse_args, remote_observer_reconnect_delay,
    remote_ref_observer_with_stream_starter, request_shutdown,
    requeue_startup_sync_claims_with_resolved_attention, retry_delay_seconds, run_sync_once_with,
    runtime_error, send_watcher_signal, serve, sync_operation_counts_json,
    sync_status_with_hosted_calls, test_hosted_context_resolver, watcher_rearm_delay,
    watcher_relative_path,
};
use bowline_control_plane::{
    ConflictMetadataRecord, ConflictOccurrenceReconcile, ConflictReconcileOutcome,
    ConflictReconcileResult, ControlPlaneError, ControlPlaneTimestamp, FakeControlPlaneClient,
    LeaseControlPlaneClient as _, ObjectKind, ObjectPointer, RejectionCode,
    WorkspaceControlPlaneClient, WorkspaceRef,
};
use bowline_core::{
    events::{EventName, EventSubjectKind},
    ids::SnapshotId,
    policy::PathClassification,
};
use bowline_local::{
    metadata::{
        PostCommitSyncComponent, RemoteRefCursorRecord, SyncOperationCounts,
        WorkspaceSyncHeadRecord,
    },
    policy::{PathFacts, UserPolicy as DirectPolicy, classify_path},
    sync::{
        ConflictFile, ConflictRecord, DownloadError, SyncRunnerError, UploadError,
        conflict_bundle_object_id, create_conflict_bundle, pending_conflict_occurrence_operations,
        set_conflict_bundle_object,
    },
};
use bowline_storage::{ByteStoreError, LocalByteStore, ObjectKey, TransferOperation};
use bowline_testkit::FakeHostedByteStore;
use notify::{
    Event, EventKind,
    event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode},
};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

fn noop_remote_ref_observer() -> RemoteRefObserver {
    test_remote_ref_observer(|_| Ok(None))
}

fn test_remote_ref_observer(
    observe: impl FnMut(SyncOnceArgs) -> Result<Option<WorkspaceRef>, Box<dyn std::error::Error>>
    + Send
    + 'static,
) -> RemoteRefObserver {
    RemoteRefObserver::new(Box::new(observe), Arc::new(OwnedThreadMetrics::default()))
}

#[test]
fn socket_handshake_remains_responsive_while_sync_worker_is_blocked() {
    let temp = unique_temp_dir("daemon-responsive-during-sync");
    let root = temp.join("Code");
    let state_root = temp.join("state");
    fs::create_dir_all(&root).expect("root");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    enqueue_status_pin_operation(
        &store,
        &WorkspaceId::new("ws_responsive"),
        "operation-blocked-sync",
        "queued",
    );

    let mut sync = watcher_test_runtime(root, state_root, "ws_responsive");
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let blocked = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
    let worker_blocked = Arc::clone(&blocked);
    sync.sync_once = Box::new(move |_, _| {
        started_tx.send(()).expect("signal blocked sync");
        let (lock, changed) = &*worker_blocked;
        let mut waiting = lock.lock().expect("blocked sync lock");
        while *waiting {
            waiting = changed.wait(waiting).expect("blocked sync wait");
        }
        Err(SyncOnceError::CredentialsMissing)
    });
    let socket_dir = PathBuf::from("/tmp").join(format!(
        "bld-responsive-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    fs::create_dir_all(&socket_dir).expect("socket dir");
    let socket = socket_dir.join("s.sock");
    let server_socket = socket.clone();
    let (server_done_tx, server_done_rx) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let result = serve(
            &server_socket,
            false,
            DaemonRuntime {
                sync: Some(sync),
                notify_approvals: false,
                notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
                next_notification_poll: Instant::now(),
                pending_notification_status: None,
            },
        );
        let _ = server_done_tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("sync worker entered blocked executor");

    let started = Instant::now();
    let response = handshake(&socket).expect("handshake remains responsive");
    assert_eq!(response.daemon_version, env!("CARGO_PKG_VERSION"));
    assert!(started.elapsed() < Duration::from_millis(500));

    request_shutdown(&socket).expect("request shutdown");
    let cancellation_deadline = Instant::now() + Duration::from_secs(2);
    let operation = loop {
        let operation = store
            .sync_operation_by_id("operation-blocked-sync")
            .expect("operation reads after shutdown")
            .expect("blocked operation exists");
        if operation.cancellation_requested_at.is_some() || Instant::now() >= cancellation_deadline
        {
            break operation;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        operation.cancellation_requested_at.is_some(),
        "strict shutdown must cooperatively cancel the in-flight claim before joining"
    );
    let (lock, changed) = &*blocked;
    *lock.lock().expect("release blocked sync") = false;
    changed.notify_all();
    server_done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("daemon joins after blocked sync reaches its checkpoint")
        .expect("daemon serves");
    server.join().expect("server exits");
    let _ = fs::remove_dir_all(temp);
    let _ = fs::remove_dir_all(socket_dir);
}

#[test]
fn claim_lease_renews_while_sync_work_is_blocked() {
    let temp = unique_temp_dir("daemon-claim-heartbeat");
    let state_root = temp.join("state");
    let workspace_id = WorkspaceId::new("ws_claim_heartbeat");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    enqueue_status_pin_operation(&store, &workspace_id, "operation-claim-heartbeat", "queued");
    let claim = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-heartbeat-test",
            "2000-01-01T00:00:00Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim query")
        .expect("operation claimed");
    let supervisor = ClaimLeaseSupervisor::start(
        state_root.clone(),
        claim.claim,
        ClaimLeasePolicy {
            heartbeat_interval: Duration::from_millis(5),
            lease_duration: Duration::from_millis(250),
        },
    )
    .expect("lease supervisor starts");
    let initial_heartbeat = store
        .sync_operation_by_id("operation-claim-heartbeat")
        .expect("initial heartbeat read")
        .expect("operation exists")
        .heartbeat_at;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let heartbeat = store
            .sync_operation_by_id("operation-claim-heartbeat")
            .expect("renewed heartbeat read")
            .expect("operation exists")
            .heartbeat_at;
        if heartbeat != initial_heartbeat {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "lease supervisor did not publish a heartbeat before the test deadline"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let requeued = store
        .requeue_expired_sync_claims(&workspace_id, &current_timestamp())
        .expect("expiry scan");

    assert_eq!(requeued, 0);
    assert_eq!(supervisor.stop(), ClaimOwnership::Owned);
    assert_eq!(
        store
            .sync_operation_by_id("operation-claim-heartbeat")
            .expect("operation read")
            .expect("operation exists")
            .state,
        SyncOperationState::Claimed
    );
    let _ = fs::remove_dir_all(temp);
}

fn noop_status_publisher() -> StatusPublisher {
    StatusPublisher::new(|payload| {
        Ok(StatusPublishOutcome {
            fingerprint: payload.fingerprint.unwrap_or_else(|| "noop".to_string()),
        })
    })
}

#[test]
fn sync_failure_classifier_routes_setup_trust_and_transport_failures() {
    assert_eq!(
        SyncOnceError::HostedConfigUnavailable.disposition(),
        SyncFailureAction::Attention
    );
    assert_eq!(
        SyncOnceError::CredentialsMissing.disposition(),
        SyncFailureAction::Attention
    );
    assert_eq!(
        SyncOnceError::ControlPlane(ControlPlaneError::Rejected {
            code: RejectionCode::DeviceNotTrusted,
            message: "device is not trusted".to_string(),
        })
        .disposition(),
        SyncFailureAction::Attention
    );
    assert_eq!(
        SyncOnceError::ControlPlane(ControlPlaneError::Transport {
            detail: "connection refused".to_string(),
        })
        .disposition(),
        SyncFailureAction::Offline
    );
}

#[test]
fn drain_policy_cache_matches_direct_load_and_refreshes_after_ignore_edit() {
    let root = unique_temp_dir("bowline-drain-policy-refresh");
    fs::create_dir_all(root.join("ignored")).expect("dir");
    fs::write(root.join(".bowlineignore"), "").expect("ignore");
    let path = "ignored/note.txt";
    let mut cache = HashMap::new();
    let facts = PathFacts {
        relative_path: path.into(),
        is_dir: false,
        byte_len: Some(4),
    };
    let cached = classify_path(&facts, drain_policy(&root, path, &mut cache));
    let direct = classify_path(&facts, &DirectPolicy::load_for_path(&root, path).unwrap());
    assert_eq!(cached, direct);
    assert_eq!(cached.classification, PathClassification::WorkspaceSync);
    fs::write(root.join(".bowlineignore"), "ignored/**\n").expect("ignore update");
    invalidate_policy_cache_for_path(".bowlineignore", &mut cache);
    let cached = classify_path(&facts, drain_policy(&root, path, &mut cache));
    let direct = classify_path(&facts, &DirectPolicy::load_for_path(&root, path).unwrap());
    assert_eq!(cached, direct);
    assert_eq!(cached.classification, PathClassification::LocalOnly);
    let _ = fs::remove_dir_all(root);
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
fn default_socket_path_uses_per_user_runtime_dir() {
    let cli = parse_args(["status"]);
    let socket = cli.socket.display().to_string();

    assert!(socket.ends_with("/bowline/runtime/bowline-daemon.sock"));
    assert_ne!(socket, "/tmp/bowline-daemon.sock");
}

#[test]
fn prepare_socket_creates_owner_only_runtime_dir() {
    let temp = unique_temp_dir("bowline-daemon-socket-perms");
    let socket = temp.join("runtime").join("bowline-daemon.sock");

    prepare_socket(&socket).expect("prepare socket");

    let mode = fs::metadata(socket.parent().expect("socket parent"))
        .expect("runtime dir metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    assert!(!socket.exists());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn socket_owner_removes_control_socket_when_shutdown_scope_exits() {
    let temp = PathBuf::from("/tmp").join(format!(
        "blds-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let socket = temp.join("d.sock");
    prepare_socket(&socket).expect("prepare socket");
    let listener = super::UnixListener::bind(&socket).expect("bind socket");
    assert!(socket.exists());

    let owner = SocketGuard {
        path: Some(socket.clone()),
    };
    drop(listener);
    drop(owner);

    assert!(!socket.exists(), "shutdown scope removes socket state");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn persisted_daemon_env_loads_allowlisted_values_without_shell() {
    let temp = unique_temp_dir("bowline-daemon-env-overlay");
    fs::write(
        temp.join("daemon.env"),
        "BOWLINE_DEVICE_NAME=mac mini\nBOWLINE_WORKOS_REFRESH_TOKEN=secret-refresh\n",
    )
    .expect("daemon env");

    load_persisted_daemon_env(&temp);

    assert_eq!(
        daemon_env_var("BOWLINE_DEVICE_NAME").as_deref(),
        Some("mac mini")
    );
    assert_eq!(daemon_env_var("BOWLINE_WORKOS_REFRESH_TOKEN"), None);
    let _ = fs::remove_dir_all(temp);
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
fn watcher_fatal_error_wakes_reconciliation_and_marks_watcher_limited() {
    let (signal_tx, signal_rx) = mpsc::channel();
    signal_tx
        .send(WatcherSignal::Limited(
            "watch backend unavailable".to_string(),
        ))
        .expect("watcher signal sends");
    let mut runtime = ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root: PathBuf::from("/tmp/bowline-root"),
                state_root: PathBuf::from("/tmp/bowline-state"),
                workspace_id: "ws_code".to_string(),
                device_id: "device-test".to_string(),
                sync_claim: None,
                scan_scope: Default::default(),
            },
            interval: Duration::from_secs(2),
            max_ticks: None,
        },
        next_tick: Instant::now() + Duration::from_secs(60),
        next_remote_observe: Instant::now() + Duration::from_secs(60),
        next_dispatch_claim: Instant::now() + Duration::from_secs(60),
        awaiting_handoff: false,
        tick_count: 0,
        last_json: "{\"state\":\"queued\",\"tickCount\":0}".to_string(),
        watcher: None,
        change_rx: Some(signal_rx),
        watcher_state: WatcherRuntimeState::Ready,
        watcher_recovery: WatcherRecovery::default(),
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        dispatch_claimer: noop_dispatch_claimer(),
        latest_observed_ref: None,
        remote_observer_state: RemoteObserverState::Ready,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        last_status_publish_fingerprint: None,
        last_status_publish_at: None,
        last_status_publish_failed_at: None,
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(PathBuf::from("/tmp/bowline-state").join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    };

    let drained = runtime.drain_changes();
    assert!(drained.changed);
    assert!(drained.sync_now);
    assert!(matches!(
        runtime.watcher_state,
        WatcherRuntimeState::Limited(ref reason) if reason.contains("backend unavailable")
    ));
}
fn assert_local_metadata_sweep_cadence_tracks_sync_interval() {
    assert!(!local_metadata_sweep_due(0, Duration::from_secs(600)));
    assert!(!local_metadata_sweep_due(5, Duration::from_secs(600)));
    assert!(local_metadata_sweep_due(6, Duration::from_secs(600)));
    assert!(local_metadata_sweep_due(1, Duration::from_secs(7200)));
}
#[test]
fn initial_sync_status_reports_limited_watcher() {
    assert_local_metadata_sweep_cadence_tracks_sync_interval();
    let status = initial_sync_status_json(
        &WatcherRuntimeState::Limited("watch backend unavailable".to_string()),
        &WatcherRecovery::default(),
    );

    assert_eq!(
        status,
        "{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{\"state\":\"limited\",\"unavailableBecause\":\"watch backend unavailable\",\"overflowCount\":0}}"
    );
}

#[test]
fn daemon_status_json_pins_ready_and_limited_watcher_shapes() {
    assert_eq!(
        initial_sync_status_json(&WatcherRuntimeState::Ready, &WatcherRecovery::default()),
        "{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{\"state\":\"ready\",\"overflowCount\":0}}"
    );
    assert_eq!(
        initial_sync_status_json(
            &WatcherRuntimeState::Limited("watch \"backend\" unavailable".to_string()),
            &WatcherRecovery::default()
        ),
        "{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{\"state\":\"limited\",\"unavailableBecause\":\"watch \\\"backend\\\" unavailable\",\"overflowCount\":0}}"
    );
}

#[test]
fn daemon_status_json_pins_waiting_queue_variants_and_counts() {
    let temp = unique_temp_dir("bowline-daemon-status-pins-queue");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_status_queue");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    let mut runtime = watcher_test_runtime(temp.join("Code"), state_root, workspace_id.as_str());

    runtime.tick_count = 7;
    assert_eq!(
        runtime.waiting_for_sync_queue_json(),
        "{\"state\":\"idle\",\"tickCount\":7,\"watcherState\":{\"state\":\"ready\",\"overflowCount\":0},\"limitedCapability\":\"continuous sync\",\"unavailableBecause\":\"no sync work is queued\",\"blockedAction\":\"wait for local or remote changes\",\"stillWorks\":[\"local edits\",\"status\"],\"queueCounts\":{\"queued\":0,\"claimed\":0,\"waitingRetry\":0,\"blockedOffline\":0,\"reconciliationRequired\":0,\"attention\":0,\"completed\":0,\"cancelled\":0},\"localHead\":null,\"remoteHead\":null}"
    );

    enqueue_status_pin_operation(&store, &workspace_id, "op-attention", "attention");
    assert_eq!(
        runtime.waiting_for_sync_queue_json(),
        "{\"state\":\"attention\",\"tickCount\":7,\"watcherState\":{\"state\":\"ready\",\"overflowCount\":0},\"limitedCapability\":\"continuous sync\",\"unavailableBecause\":\"sync queue needs attention\",\"blockedAction\":\"resolve sync queue attention\",\"stillWorks\":[\"local edits\",\"status\"],\"queueCounts\":{\"queued\":0,\"claimed\":0,\"waitingRetry\":0,\"blockedOffline\":0,\"reconciliationRequired\":0,\"attention\":1,\"completed\":0,\"cancelled\":0},\"localHead\":null,\"remoteHead\":null}"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_status_json_pins_head_and_cursor_payloads() {
    let temp = unique_temp_dir("bowline-daemon-status-pins-heads");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_status_heads");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    let runtime = watcher_test_runtime(temp.join("Code"), state_root, workspace_id.as_str());

    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 12,
                snapshot_id: SnapshotId::new("snap-local"),
                updated_at: ControlPlaneTimestamp { tick: 34 },
                updated_by_device_id: Some(DeviceId::new("device-a")),
            },
            observed_at: "2026-06-27T00:00:00Z".to_string(),
        })
        .expect("local head");
    store
        .put_remote_ref_cursor(&RemoteRefCursorRecord {
            workspace_id: workspace_id.clone(),
            cursor: None,
            last_observed_version: Some(13),
            last_observed_snapshot_id: Some("snap-remote".to_string()),
            updated_at: "2026-06-27T00:00:01Z".to_string(),
        })
        .expect("remote cursor");

    assert_eq!(
        runtime.local_head_json(),
        "{\"workspaceId\":\"ws_status_heads\",\"snapshotId\":\"snap-local\",\"version\":12,\"updatedAtTick\":34}"
    );
    assert_eq!(
        runtime.remote_head_json(),
        "{\"workspaceId\":\"ws_status_heads\",\"snapshotId\":\"snap-remote\",\"version\":13}"
    );
    assert_eq!(
        sync_operation_counts_json(&SyncOperationCounts {
            queued: 1,
            claimed: 2,
            waiting_retry: 3,
            blocked_offline: 4,
            reconciliation_required: 5,
            attention: 5,
            completed: 6,
            cancelled: 7,
        }),
        "{\"queued\":1,\"claimed\":2,\"waitingRetry\":3,\"blockedOffline\":4,\"reconciliationRequired\":5,\"attention\":5,\"completed\":6,\"cancelled\":7}"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_status_json_pins_remote_observer_error_shape() {
    let temp = unique_temp_dir("bowline-daemon-status-pins-observer");
    let state_root = temp.join(".state");
    let mut runtime = watcher_test_runtime(temp.join("Code"), state_root, "ws_status_observer");
    runtime.tick_count = 9;
    runtime.remote_ref_observer =
        test_remote_ref_observer(|_| Err(runtime_error("network \"offline\"")));

    assert!(!runtime.observe_remote_ref_cursor());
    assert_eq!(
        runtime.status_json(),
        "{\"state\":\"limited\",\"tickCount\":9,\"unavailableBecause\":\"control-plane-unavailable\",\"nextAction\":\"check network or hosted auth\",\"queue\":{\"queued\":0,\"claimed\":0,\"waitingRetry\":0,\"blockedOffline\":0,\"reconciliationRequired\":0,\"attention\":0,\"completed\":0,\"cancelled\":0},\"localHead\":null,\"remoteHead\":null}"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn remote_observer_stays_unavailable_until_stream_delivers_initial_state() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut receiver = Some(receiver);
    let mut observer = remote_ref_observer_with_stream_starter(Box::new(move |_| {
        Ok(receiver
            .take()
            .expect("observer stream starts exactly once")
            .into())
    }));
    let temp = unique_temp_dir("bowline-daemon-observer-connect-readiness");
    let runtime = watcher_test_runtime(temp.join("Code"), temp.join(".state"), "ws_connecting");

    let first = observer.observe(runtime.options.args.clone());
    assert!(first.is_err(), "an empty new stream is still connecting");

    sender.send(Ok(None)).expect("initial observer state");
    assert_eq!(
        observer
            .observe(runtime.options.args.clone())
            .expect("observer ready"),
        None
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn repeated_remote_observer_failure_records_only_semantic_state_transitions() {
    let temp = unique_temp_dir("bowline-daemon-observer-publish-transitions");
    let mut runtime = watcher_test_runtime(
        temp.join("Code"),
        temp.join(".state"),
        "ws_observer_publish_transitions",
    );
    runtime.remote_ref_observer =
        test_remote_ref_observer(|_| Err(runtime_error("observer unavailable")));

    assert!(!runtime.observe_remote_ref_cursor());
    assert!(!runtime.observe_remote_ref_cursor());
    assert_eq!(
        runtime.remote_observer_state,
        RemoteObserverState::Unavailable
    );
    let degraded = MetadataStore::open(runtime.options.args.state_root.join(DEFAULT_DATABASE_FILE))
        .expect("metadata store")
        .event_watermarks()
        .expect("event watermarks");
    assert_eq!(
        degraded.sync_state,
        Some(bowline_core::status::ComponentState::Degraded)
    );

    runtime.remote_ref_observer = test_remote_ref_observer(|_| Ok(None));
    assert!(!runtime.observe_remote_ref_cursor());
    assert_eq!(runtime.remote_observer_state, RemoteObserverState::Ready);
    let recovered =
        MetadataStore::open(runtime.options.args.state_root.join(DEFAULT_DATABASE_FILE))
            .expect("metadata store")
            .event_watermarks()
            .expect("event watermarks");
    assert_eq!(
        recovered.sync_state,
        Some(bowline_core::status::ComponentState::Ready)
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_poll_cannot_project_remote_observer_failure_as_healthy() {
    let temp = unique_temp_dir("bowline-daemon-observer-failure-health");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), "ws_observer_failure");
    runtime.remote_ref_observer =
        test_remote_ref_observer(|_| Err(runtime_error("observer unavailable")));
    runtime.sync_once = Box::new(|_, _| {
        Ok(SyncOnceSummary {
            workspace_id: "ws_observer_failure".to_string(),
            snapshot_id: "snap-local".to_string(),
            version: 1,
            outcome: SyncSummaryOutcome::Uploaded { stale: false },
            snapshot_root_manifest_id: None,
            manifest_object_key: None,
            namespace_root_id: None,
            conflict_count: 0,
            conflicts: Vec::new(),
            scan: SyncScanSummary::default(),
            cancelled_late: false,
        })
    });

    runtime.poll();

    let status: serde_json::Value =
        serde_json::from_str(runtime.status_json()).expect("daemon status json");
    assert_eq!(status["state"], "limited");
    assert_eq!(status["unavailableBecause"], "control-plane-unavailable");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    let watermarks = store.event_watermarks().expect("watermarks");
    assert_eq!(
        watermarks.sync_state,
        Some(bowline_core::status::ComponentState::Degraded)
    );
    assert_eq!(
        watermarks.network_state,
        Some(bowline_core::status::NetworkState::Degraded)
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_status_json_pins_success_and_failure_poll_shapes() {
    let temp = unique_temp_dir("bowline-daemon-status-pins-poll");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_status_poll");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    enqueue_status_pin_operation(&store, &workspace_id, "op-success", "queued");

    let mut runtime = watcher_test_runtime(root.clone(), state_root.clone(), workspace_id.as_str());
    runtime.sync_once = Box::new(|_, _| {
        Ok(SyncOnceSummary {
            workspace_id: "ws_status_poll".to_string(),
            snapshot_id: "snap-advanced".to_string(),
            version: 4,
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
    assert_eq!(
        runtime.status_json(),
        "{\"state\":\"idle\",\"tickCount\":1,\"watcherState\":{\"state\":\"ready\",\"overflowCount\":0},\"lastOutcome\":\"advanced\",\"workspaceId\":\"ws_status_poll\",\"snapshotId\":\"snap-advanced\",\"version\":4,\"conflictCount\":0,\"scan\":{\"mode\":\"full\",\"fullReason\":\"cli-requested\",\"filesHashed\":0,\"statHits\":0,\"futureMtimePaths\":0,\"divergenceCount\":0,\"rehashReasons\":[]},\"queueCounts\":{\"queued\":0,\"claimed\":0,\"waitingRetry\":0,\"blockedOffline\":0,\"reconciliationRequired\":0,\"attention\":0,\"completed\":1,\"cancelled\":0},\"localHead\":null,\"remoteHead\":{\"workspaceId\":\"ws_status_poll\",\"snapshotId\":\"snap-advanced\",\"version\":4}}"
    );

    let failure_state_root = temp.join(".failure-state");
    let failure_workspace_id = WorkspaceId::new("ws_status_failure");
    let failure_store = open_store_for_test(failure_state_root.join(DEFAULT_DATABASE_FILE))
        .expect("metadata opens");
    enqueue_status_pin_operation(
        &failure_store,
        &failure_workspace_id,
        "op-failure",
        "queued",
    );
    let mut failure_runtime = watcher_test_runtime(root, failure_state_root, "ws_status_failure");
    failure_runtime.sync_once = Box::new(|_, _| {
        Err(SyncOnceError::ControlPlane(ControlPlaneError::Storage(
            "sync json failed".to_string(),
        )))
    });
    failure_runtime.poll();
    assert_eq!(
        failure_runtime.status_json(),
        "{\"state\":\"limited\",\"tickCount\":1,\"watcherState\":{\"state\":\"ready\",\"overflowCount\":0},\"limitedCapability\":\"continuous sync\",\"unavailableBecause\":\"control-plane-unavailable\",\"blockedAction\":\"sync ~/Code\",\"stillWorks\":[\"local edits\",\"status\",\"manual sync-once diagnostics\"],\"queueCounts\":{\"queued\":0,\"claimed\":0,\"waitingRetry\":1,\"blockedOffline\":0,\"reconciliationRequired\":0,\"attention\":0,\"completed\":0,\"cancelled\":0},\"localHead\":null,\"remoteHead\":null}"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn sync_status_includes_hosted_call_budget_snapshot() {
    let status = sync_status_with_hosted_calls(
        "{\"state\":\"idle\",\"tickCount\":1,\"watcherState\":{\"state\":\"ready\",\"overflowCount\":0}}",
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

    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    let mut policy_cache = HashMap::new();
    runtime
        .record_watcher_event(
            &Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
            &mut policy_cache,
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

    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    let mut policy_cache = HashMap::new();
    runtime
        .record_watcher_event(
            &Event::new(EventKind::Remove(RemoveKind::File)).add_path(private_path),
            &mut policy_cache,
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
fn watcher_event_records_git_index_and_pointer_state() {
    let fixture = watcher_fixture("bowline-daemon-watch-git-shape", "ws_watch_git_shape");
    let root = fixture.root.clone();
    fs::create_dir_all(root.join("repo/.git/refs/heads")).expect("git dirs");
    let index_path = root.join("repo/.git/index");
    let head_path = root.join("repo/.git/HEAD");
    fs::write(&index_path, "local index").expect("index");
    fs::write(&head_path, "ref: refs/heads/main\n").expect("head");

    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    let mut policy_cache = HashMap::new();
    assert!(
        runtime
            .record_watcher_event(
                &Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                    .add_path(index_path),
                &mut policy_cache,
            )
            .expect("git index event records")
    );
    assert!(
        runtime
            .record_watcher_event(
                &Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                    .add_path(head_path),
                &mut policy_cache,
            )
            .expect("git head event records")
    );

    let writes = fixture
        .store
        .local_write_log(&fixture.workspace_id)
        .expect("write log");
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].path, "repo/.git/index");
    assert_eq!(writes[1].path, "repo/.git/HEAD");

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

    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    let mut policy_cache = HashMap::new();
    runtime
        .record_watcher_event(
            &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(old_path)
                .add_path(new_path),
            &mut policy_cache,
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
                sync_claim: None,
                scan_scope: Default::default(),
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        next_dispatch_claim: Instant::now(),
        awaiting_handoff: false,
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        watcher_recovery: WatcherRecovery::default(),
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        dispatch_claimer: noop_dispatch_claimer(),
        latest_observed_ref: None,
        remote_observer_state: RemoteObserverState::Ready,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        last_status_publish_fingerprint: None,
        last_status_publish_at: None,
        last_status_publish_failed_at: None,
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    };

    let summary = SyncOnceSummary {
        workspace_id: workspace_id.as_str().to_string(),
        snapshot_id: "snap-42".to_string(),
        version: 42,
        outcome: SyncSummaryOutcome::NoChanges,
        snapshot_root_manifest_id: None,
        manifest_object_key: None,
        namespace_root_id: None,
        conflict_count: 0,
        conflicts: Vec::new(),
        scan: SyncScanSummary::default(),
        cancelled_late: false,
    };
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-complete-sync-event".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "complete-sync-event".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: Some(summary.snapshot_id.clone()),
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 0,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
        })
        .expect("operation queued");
    let claim = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            &current_timestamp(),
            "2999-01-01T00:00:00Z",
        )
        .expect("claim query")
        .expect("operation claimed")
        .claim;
    runtime.record_remote_ref_cursor(&summary);
    assert!(runtime.complete_daemon_sync_operation(&claim, &summary));

    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
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
                sync_claim: None,
                scan_scope: Default::default(),
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        next_dispatch_claim: Instant::now(),
        awaiting_handoff: false,
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        watcher_recovery: WatcherRecovery::default(),
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        dispatch_claimer: noop_dispatch_claimer(),
        latest_observed_ref: None,
        remote_observer_state: RemoteObserverState::Ready,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        last_status_publish_fingerprint: None,
        last_status_publish_at: None,
        last_status_publish_failed_at: None,
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    };

    let summary = SyncOnceSummary {
        workspace_id: workspace_id.as_str().to_string(),
        snapshot_id: "snap-base".to_string(),
        version: 7,
        outcome: SyncSummaryOutcome::Conflicted,
        snapshot_root_manifest_id: None,
        manifest_object_key: None,
        namespace_root_id: None,
        conflict_count: 1,
        conflicts: vec![ConflictSummary {
            id: "conflict_app_src_main".to_string(),
            paths: vec!["app/src/main.ts".to_string()],
        }],
        scan: SyncScanSummary::default(),
        cancelled_late: false,
    };

    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
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

mod fake_daemon_loop_tests;
mod scheduler_recovery_tests;
mod sync_failure_queue_tests;
mod watcher_overflow_tests;
mod watcher_root_routing_tests;

#[test]
fn parses_status_json() {
    let cli = parse_args(["status", "--json"]);

    assert!(cli.json);
    assert_eq!(cli.command, Command::Status);
}

#[test]
fn sync_component_ready_preserves_degraded_post_commit_lane() {
    let temp = unique_temp_dir("bowline-daemon-post-commit-lane");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_post_commit_lane");
    let runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-05T12:31:00Z")
        .expect("workspace");
    store
        .set_component_state(
            "sync.post_commit.overlays",
            "degraded",
            "2026-07-05T12:31:00Z",
        )
        .expect("lane state");

    runtime.record_component_states(SyncComponentState::Ready, "ready", "online");

    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    assert_eq!(
        store.event_watermarks().expect("watermarks").sync_state,
        Some(bowline_core::status::ComponentState::Degraded)
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn projection_input_preserves_degraded_post_commit_lane() {
    let temp = unique_temp_dir("bowline-daemon-publish-post-commit-lane");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_publish_post_commit_lane");
    let runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .set_component_state(
            "sync.post_commit.overlays",
            "degraded",
            "2026-07-05T12:31:00Z",
        )
        .expect("lane state");
    drop(store);
    runtime.record_component_states(SyncComponentState::Ready, "ready", "online");
    let watermarks = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE))
        .expect("store")
        .event_watermarks()
        .expect("watermarks");
    assert_eq!(
        watermarks.sync_state,
        Some(bowline_core::status::ComponentState::Degraded)
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn sync_component_ready_recovers_after_post_commit_lane_recovers() {
    let temp = unique_temp_dir("bowline-daemon-post-commit-lane-recovery");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_post_commit_lane_recovery");
    let runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .set_component_state("sync", "degraded", "2026-07-05T12:31:00Z")
        .expect("sync state");
    store
        .set_component_state("sync.post_commit.overlays", "ready", "2026-07-05T12:32:00Z")
        .expect("overlay lane state");
    store
        .set_component_state(
            "sync.post_commit.conflicts",
            "ready",
            "2026-07-05T12:32:00Z",
        )
        .expect("conflict lane state");
    drop(store);

    runtime.record_component_states(SyncComponentState::Ready, "ready", "online");

    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    assert_eq!(
        store.event_watermarks().expect("watermarks").sync_state,
        Some(bowline_core::status::ComponentState::Ready)
    );
    let _ = fs::remove_dir_all(temp);
}

fn conflict_reconcile_result(
    input: &ConflictOccurrenceReconcile,
    outcome: ConflictReconcileOutcome,
) -> ConflictReconcileResult {
    let resolved =
        input.desired_state != bowline_control_plane::ConflictOccurrenceState::Unresolved;
    ConflictReconcileResult {
        conflict: ConflictMetadataRecord {
            workspace_id: input.workspace_id.clone(),
            conflict_id: input.conflict_id.clone(),
            conflict_kind: input.conflict_kind.clone(),
            paths: input.paths.clone(),
            contains_secrets: input.contains_secrets,
            state: input.desired_state,
            base_snapshot_id: input.base_snapshot_id.clone(),
            remote_snapshot_id: input.remote_snapshot_id.clone(),
            occurrence_version: input.occurrence_version,
            reason: input.reason.clone(),
            detected_by_device_id: input.device_id.clone(),
            bundle_object: input.bundle_object.clone(),
            detected_at: ControlPlaneTimestamp { tick: 1 },
            resolved_by_device_id: resolved.then(|| input.device_id.clone()),
            resolved_at: resolved.then_some(ControlPlaneTimestamp { tick: 2 }),
        },
        outcome,
    }
}

fn create_test_conflict(state_root: &Path, remote_snapshot_id: &str) -> ConflictRecord {
    let mut record = ConflictRecord::same_path("src/main.rs");
    record.base_snapshot_id = Some("snap_base".to_string());
    record.remote_snapshot_id = Some(remote_snapshot_id.to_string());
    let bundle = create_conflict_bundle(
        state_root,
        record,
        &[ConflictFile {
            relative_path: "src/main.rs".to_string(),
            base: Some(b"base".to_vec()),
            local: Some(b"local".to_vec()),
            remote: Some(b"remote".to_vec()),
        }],
    )
    .expect("conflict bundle");
    let object_id = conflict_bundle_object_id(&bundle.record);
    let pointer = ObjectPointer {
        object_key: ObjectKey::from_conflict_bundle_id(object_id.as_str())
            .expect("canonical conflict bundle object key")
            .as_str()
            .to_string(),
        content_id: object_id,
        byte_len: 128,
        hash: "b3_0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        key_epoch: 1,
        kind: ObjectKind::ConflictBundle,
        created_at: ControlPlaneTimestamp { tick: 42 },
    };
    assert!(
        set_conflict_bundle_object(&bundle.record, pointer.clone())
            .expect("persist conflict bundle pointer")
    );
    let mut record = bundle.record;
    record.bundle_object = Some(pointer);
    record
}

#[test]
fn conflict_worker_marks_only_the_exact_applied_occurrence() {
    let temp = unique_temp_dir("bowline-daemon-conflict-worker-applied");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let record = create_test_conflict(&state_root, "snap_remote_1");
    let workspace_id = WorkspaceId::new("ws_conflict_worker");
    let device_id = DeviceId::new("device-test");
    let operation = pending_conflict_occurrence_operations(
        &state_root,
        &workspace_id,
        &device_id,
        &current_timestamp(),
    )
    .expect("scan")
    .pop()
    .expect("operation");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    store.enqueue_sync_operation(&operation).expect("enqueue");
    let claimed = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            &current_timestamp(),
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("claimed");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());

    runtime.process_claimed_conflict_occurrence(claimed, |input| {
        Ok(conflict_reconcile_result(
            &input,
            ConflictReconcileOutcome::Applied,
        ))
    });

    let stored = store
        .sync_operation_by_id(&operation.id)
        .expect("operation")
        .expect("stored");
    assert_eq!(stored.state, SyncOperationState::Completed);
    let manifest: ConflictRecord = serde_json::from_slice(
        &fs::read(
            state_root
                .join("conflicts")
                .join(&record.id)
                .join("manifest.json"),
        )
        .expect("manifest"),
    )
    .expect("record");
    assert!(manifest.remote_conflict_published_at.is_some());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn hosted_superseded_conflict_job_does_not_advance_the_local_marker() {
    let temp = unique_temp_dir("bowline-daemon-conflict-worker-hosted-superseded");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let record = create_test_conflict(&state_root, "snap_remote_1");
    let workspace_id = WorkspaceId::new("ws_conflict_worker_hosted_superseded");
    let device_id = DeviceId::new("device-test");
    let operation = pending_conflict_occurrence_operations(
        &state_root,
        &workspace_id,
        &device_id,
        &current_timestamp(),
    )
    .expect("scan")
    .pop()
    .expect("operation");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    store.enqueue_sync_operation(&operation).expect("enqueue");
    let claimed = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            &current_timestamp(),
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("claimed");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());

    runtime.process_claimed_conflict_occurrence(claimed, |input| {
        Ok(conflict_reconcile_result(
            &input,
            ConflictReconcileOutcome::Superseded,
        ))
    });

    let stored = store
        .sync_operation_by_id(&operation.id)
        .expect("operation")
        .expect("stored");
    assert_eq!(stored.state, SyncOperationState::Completed);
    assert!(
        stored
            .result_json
            .as_deref()
            .is_some_and(|result| result.contains("superseded"))
    );
    let manifest: ConflictRecord = serde_json::from_slice(
        &fs::read(
            state_root
                .join("conflicts")
                .join(&record.id)
                .join("manifest.json"),
        )
        .expect("manifest"),
    )
    .expect("record");
    assert_eq!(manifest.remote_conflict_published_at, None);
    assert_eq!(manifest.remote_resolution_synced_at, None);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn stale_conflict_job_completes_superseded_without_remote_call_or_newer_marker() {
    let temp = unique_temp_dir("bowline-daemon-conflict-worker-stale");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    create_test_conflict(&state_root, "snap_remote_1");
    let workspace_id = WorkspaceId::new("ws_conflict_worker_stale");
    let device_id = DeviceId::new("device-test");
    let operation = pending_conflict_occurrence_operations(
        &state_root,
        &workspace_id,
        &device_id,
        &current_timestamp(),
    )
    .expect("scan")
    .pop()
    .expect("operation");
    let newer = create_test_conflict(&state_root, "snap_remote_2");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    store.enqueue_sync_operation(&operation).expect("enqueue");
    let claimed = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            &current_timestamp(),
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("claimed");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    let remote_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remote_called_in_worker = Arc::clone(&remote_called);

    runtime.process_claimed_conflict_occurrence(claimed, move |input| {
        remote_called_in_worker.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(conflict_reconcile_result(
            &input,
            ConflictReconcileOutcome::Applied,
        ))
    });

    assert!(!remote_called.load(std::sync::atomic::Ordering::SeqCst));
    let stored = store
        .sync_operation_by_id(&operation.id)
        .expect("operation")
        .expect("stored");
    assert_eq!(stored.state, SyncOperationState::Completed);
    let manifest: ConflictRecord = serde_json::from_slice(
        &fs::read(newer.bundle_path.expect("bundle").join("manifest.json")).expect("manifest"),
    )
    .expect("record");
    assert_eq!(manifest.occurrence_version, 2);
    assert_eq!(manifest.remote_conflict_published_at, None);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn lost_claim_cannot_mark_or_complete_a_conflict_occurrence() {
    let temp = unique_temp_dir("bowline-daemon-conflict-worker-lost-claim");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let record = create_test_conflict(&state_root, "snap_remote_1");
    let workspace_id = WorkspaceId::new("ws_conflict_worker_lost_claim");
    let device_id = DeviceId::new("device-test");
    let operation = pending_conflict_occurrence_operations(
        &state_root,
        &workspace_id,
        &device_id,
        &current_timestamp(),
    )
    .expect("scan")
    .pop()
    .expect("operation");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
    store.enqueue_sync_operation(&operation).expect("enqueue");
    let claimed = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            &current_timestamp(),
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("claimed");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    let db_path = state_root.join(DEFAULT_DATABASE_FILE);
    let competing_claim = claimed.claim.clone();

    runtime.process_claimed_conflict_occurrence(claimed, move |input| {
        MetadataStore::open(&db_path)
            .expect("competing store")
            .mark_claimed_sync_operation_attention(
                &competing_claim,
                "competing-worker",
                "competing worker took terminal ownership",
                &current_timestamp(),
            )
            .expect("steal claim");
        Ok(conflict_reconcile_result(
            &input,
            ConflictReconcileOutcome::Applied,
        ))
    });

    let stored = store
        .sync_operation_by_id(&operation.id)
        .expect("operation")
        .expect("stored");
    assert_eq!(stored.state, SyncOperationState::Attention);
    let manifest: ConflictRecord = serde_json::from_slice(
        &fs::read(
            state_root
                .join("conflicts")
                .join(&record.id)
                .join("manifest.json"),
        )
        .expect("manifest"),
    )
    .expect("record");
    assert_eq!(manifest.remote_conflict_published_at, None);
    let _ = fs::remove_dir_all(temp);
}

pub(super) fn watcher_test_runtime(
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: &str,
) -> ContinuousSyncRuntime {
    ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.to_string(),
                device_id: "device-test".to_string(),
                sync_claim: None,
                scan_scope: Default::default(),
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        next_dispatch_claim: Instant::now(),
        awaiting_handoff: false,
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        watcher_recovery: WatcherRecovery::default(),
        sync_once: hosted_sync_executor(),
        remote_ref_observer: noop_remote_ref_observer(),
        dispatch_claimer: noop_dispatch_claimer(),
        latest_observed_ref: None,
        remote_observer_state: RemoteObserverState::Ready,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        last_status_publish_fingerprint: None,
        last_status_publish_at: None,
        last_status_publish_failed_at: None,
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    }
}

fn enqueue_status_pin_operation(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    operation_id: &str,
    state: &str,
) {
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: match state {
                "queued" => SyncOperationState::Queued,
                "claimed" => SyncOperationState::Claimed,
                "waiting_retry" => SyncOperationState::WaitingRetry,
                "blocked_offline" => SyncOperationState::BlockedOffline,
                "reconciliation_required" => SyncOperationState::ReconciliationRequired,
                "attention" => SyncOperationState::Attention,
                "completed" => SyncOperationState::Completed,
                other => panic!("unsupported sync operation state in daemon test helper: {other}"),
            },
            idempotency_key: format!("status-pin:{operation_id}"),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 0,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: "2026-06-27T00:00:00Z".to_string(),
            updated_at: "2026-06-27T00:00:00Z".to_string(),
        })
        .expect("status pin operation inserted");
}

fn fake_daemon_runtime(
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: &str,
    device_id: &str,
    control_plane: Arc<Mutex<FakeControlPlaneClient>>,
    byte_store: Arc<Mutex<LocalByteStore>>,
    workspace_key: [u8; 32],
) -> ContinuousSyncRuntime {
    ContinuousSyncRuntime {
        options: ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.to_string(),
                device_id: device_id.to_string(),
                sync_claim: None,
                scan_scope: Default::default(),
            },
            interval: Duration::from_millis(0),
            max_ticks: None,
        },
        next_tick: Instant::now(),
        next_remote_observe: Instant::now(),
        next_dispatch_claim: Instant::now(),
        awaiting_handoff: false,
        tick_count: 0,
        last_json: String::new(),
        watcher: None,
        change_rx: None,
        watcher_state: WatcherRuntimeState::Ready,
        watcher_recovery: WatcherRecovery::default(),
        sync_once: fake_sync_executor(Arc::clone(&control_plane), byte_store, workspace_key),
        remote_ref_observer: noop_remote_ref_observer(),
        dispatch_claimer: fake_dispatch_claimer(control_plane, workspace_key),
        latest_observed_ref: None,
        remote_observer_state: RemoteObserverState::Ready,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        last_status_publish_fingerprint: None,
        last_status_publish_at: None,
        last_status_publish_failed_at: None,
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    }
}

fn fake_dispatch_claimer(
    control_plane: Arc<Mutex<FakeControlPlaneClient>>,
    workspace_key: [u8; 32],
) -> DispatchClaimer {
    Box::new(move |args| {
        let workspace_id = WorkspaceId::new(args.workspace_id.clone());
        let device_id = DeviceId::new(args.device_id.clone());
        let control_plane = control_plane
            .lock()
            .expect("fake control plane lock for dispatch claim");
        claim_pending_dispatched_lease_with(
            &*control_plane,
            args,
            &workspace_id,
            &device_id,
            workspace_key,
        )
    })
}

fn fake_sync_executor(
    control_plane: Arc<Mutex<FakeControlPlaneClient>>,
    byte_store: Arc<Mutex<LocalByteStore>>,
    workspace_key: [u8; 32],
) -> SyncExecutor {
    Box::new(move |args, observed_base_ref| {
        let workspace_id = WorkspaceId::new(args.workspace_id.clone());
        let device_id = DeviceId::new(args.device_id.clone());
        let control_plane = control_plane.lock().map_err(|_| {
            SyncOnceError::ControlPlane(ControlPlaneError::Storage(
                "fake control plane lock poisoned".to_string(),
            ))
        })?;
        let byte_store = byte_store.lock().map_err(|_| {
            SyncOnceError::ControlPlane(ControlPlaneError::Storage(
                "fake byte store lock poisoned".to_string(),
            ))
        })?;
        let base_ref = match observed_base_ref {
            Some(workspace_ref) => workspace_ref,
            None => match control_plane.get_workspace_ref(&workspace_id)? {
                Some(workspace_ref) => workspace_ref,
                None => control_plane.create_workspace_ref(&workspace_id)?,
            },
        };
        let hosted_byte_store =
            FakeHostedByteStore::new(&control_plane, &byte_store, &workspace_id);
        run_sync_once_with(
            args,
            &*control_plane,
            &hosted_byte_store,
            base_ref,
            workspace_id.clone(),
            device_id,
            LocalWorkspaceKey {
                bytes: workspace_key,
                key_epoch: 1,
            },
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

#[test]
fn unchanged_status_heartbeats_reuse_composition_until_independent_safety_deadline() {
    let fixture = watcher_fixture(
        "bowline-status-heartbeat-suppression",
        "ws_status_heartbeat",
    );
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    let projection_runtime = DaemonRuntime {
        sync: Some(watcher_test_runtime(
            fixture.root.clone(),
            fixture.state_root.clone(),
            fixture.workspace_id.as_str(),
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let projection_state =
        DaemonServerState::new(&projection_runtime).expect("projection daemon state");
    let projection = projection_state.current_projection();
    let projection_input = projection_state.test_projection_input();
    let publishes = Arc::new(Mutex::new(Vec::<StatusPublishPayload>::new()));
    let captured = Arc::clone(&publishes);
    runtime.status_publisher = StatusPublisher::new(move |payload| {
        let fingerprint = payload
            .fingerprint
            .clone()
            .unwrap_or_else(|| "direct-status-publish".to_string());
        captured.lock().expect("publish capture lock").push(payload);
        Ok(StatusPublishOutcome { fingerprint })
    });

    let start = Instant::now();
    runtime.next_status_publish = start;
    runtime.publish_projection_status(&projection, false, start, &projection_input);
    assert_eq!(publishes.lock().expect("published").len(), 1);
    assert!(
        publishes.lock().expect("published")[0].snapshot.is_some(),
        "heartbeat publish should reuse the precomposed status snapshot"
    );

    for tick in 1..60 {
        runtime.publish_projection_status(
            &projection,
            true,
            start + STATUS_PUBLISH_INTERVAL * tick,
            &projection_input,
        );
        assert_eq!(
            runtime.next_status_publish,
            start + STATUS_PUBLISH_INTERVAL * (tick + 1),
            "a deduplicated publish must advance the coordinator deadline"
        );
    }
    assert_eq!(publishes.lock().expect("published").len(), 12);

    runtime.publish_projection_status(
        &projection,
        true,
        start + STATUS_PUBLISH_INTERVAL * 60,
        &projection_input,
    );
    assert_eq!(publishes.lock().expect("published").len(), 13);
    assert_eq!(projection.sequence.get(), 1);
}

#[test]
fn failed_hosted_projection_publish_retries_without_changing_local_sequence() {
    let fixture = watcher_fixture("bowline-status-publish-retry", "ws_status_publish_retry");
    let projection_runtime = DaemonRuntime {
        sync: Some(watcher_test_runtime(
            fixture.root.clone(),
            fixture.state_root.clone(),
            fixture.workspace_id.as_str(),
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let projection_state =
        DaemonServerState::new(&projection_runtime).expect("projection daemon state");
    let projection = projection_state.current_projection();
    let projection_input = projection_state.test_projection_input();
    let initial_sequence = projection.sequence;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.status_publisher = StatusPublisher::new(move |payload| {
        if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(runtime_error("injected hosted publish failure"));
        }
        Ok(StatusPublishOutcome {
            fingerprint: payload.fingerprint.expect("projection fingerprint"),
        })
    });

    let start = Instant::now();
    runtime.publish_projection_status(&projection, false, start, &projection_input);
    assert_eq!(runtime.last_status_publish_fingerprint, None);
    runtime.publish_projection_status(
        &projection,
        false,
        start + Duration::from_secs(1),
        &projection_input,
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "projection churn must not bypass failed-publish backoff"
    );
    runtime.retry_projection_status_if_due(
        &projection,
        start + STATUS_PUBLISH_INTERVAL,
        &projection_input,
    );

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(runtime.last_status_publish_fingerprint.is_some());
    assert_eq!(projection.sequence, initial_sequence);
    let metrics = projection_state.test_projection_metrics();
    assert_eq!(metrics.hosted_serializations, 3);
    assert_eq!(metrics.hosted_publish_attempts, 2);
    assert_eq!(metrics.hosted_publish_failures, 1);
    assert_eq!(metrics.hosted_publish_successes, 1);
}

#[test]
fn status_publish_rejects_projection_for_a_different_workspace() {
    let fixture = watcher_fixture(
        "bowline-status-workspace-mismatch",
        "ws_status_projection_source",
    );
    let projection_runtime = DaemonRuntime {
        sync: Some(watcher_test_runtime(
            fixture.root.clone(),
            fixture.state_root.clone(),
            fixture.workspace_id.as_str(),
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let projection_state =
        DaemonServerState::new(&projection_runtime).expect("projection daemon state");
    let projection = projection_state.current_projection();
    let mut args = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    )
    .options
    .args;
    args.workspace_id = "ws_different_configured".to_string();

    let error = StatusPublishPayload::from_projection(StatusPublishRequest { args }, &projection)
        .expect_err("workspace mismatch must be rejected");

    assert!(
        error
            .to_string()
            .contains("status projection workspace does not match configured daemon workspace")
    );
}

fn file_contains(path: &std::path::Path, needle: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|content| content.contains(needle))
}

fn checkpoint_steps(state_root: &Path) -> Vec<String> {
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("daemon metadata opens");
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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
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

pub(super) fn unique_temp_dir(label: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("{label}-{suffix}"));
    fs::create_dir_all(&path).expect("temp dir");
    path
}
