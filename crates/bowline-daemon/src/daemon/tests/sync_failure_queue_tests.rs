use super::*;

#[test]
fn missing_remote_bytes_are_reported_as_offline_sync_work() {
    assert_eq!(
        SyncOnceError::Runner(SyncRunnerError::Download(
            DownloadError::SnapshotManifestMissing("snap_missing".to_string()),
        ))
        .disposition(),
        SyncFailureAction::Offline
    );
    assert_eq!(
        SyncOnceError::Runner(SyncRunnerError::Download(DownloadError::ByteStore(
            ByteStoreError::MissingObject {
                key: ObjectKey::new("packs_pk_0011223344556677".to_string())
                    .expect("valid object key"),
                component: "object",
            },
        )))
        .disposition(),
        SyncFailureAction::Offline
    );
    assert_eq!(
        SyncOnceError::Runner(SyncRunnerError::Download(DownloadError::ByteStore(
            ByteStoreError::HttpStatus {
                key: ObjectKey::new("packs_pk_0011223344556677".to_string())
                    .expect("valid object key"),
                operation: TransferOperation::Download,
                status: 404,
            },
        )))
        .disposition(),
        SyncFailureAction::Offline
    );
    assert_eq!(
        SyncOnceError::Runner(SyncRunnerError::Download(DownloadError::ByteStore(
            ByteStoreError::CorruptObject {
                key: ObjectKey::new("packs_pk_8899aabbccddeeff".to_string())
                    .expect("valid object key"),
                reason: "object bytes did not match metadata",
            },
        )))
        .disposition(),
        SyncFailureAction::Retry
    );
    assert_eq!(
        SyncOnceError::HostedConfigUnavailable.disposition(),
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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "missing-remote".to_string(),
            base_version: Some(1),
            base_snapshot_id: Some("snap-1".to_string()),
            target_snapshot_id: Some("snap-2".to_string()),
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
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
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
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    };

    let before = OffsetDateTime::now_utc();
    runtime.fail_daemon_sync_operation(
        &claim,
        &SyncOnceError::Runner(SyncRunnerError::Download(
            DownloadError::SnapshotManifestMissing("snap_missing".to_string()),
        )),
    );

    let counts = store
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(counts.blocked_offline, 1);
    let operation = store
        .sync_operation_by_id(operation_id)
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::BlockedOffline);
    runtime.record_component_states(SyncComponentState::Degraded, "ready", "offline");
    assert_eq!(
        store.event_watermarks().expect("watermarks").network_state,
        Some(bowline_core::status::NetworkState::Offline)
    );
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
fn failed_ref_advance_checkpoint_reconciles_instead_of_cancelling() {
    let temp = unique_temp_dir("bowline-daemon-post-cas-failure");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let workspace_id = WorkspaceId::new("ws_post_cas_failure");
    let operation_id = "op-post-cas-failure";
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: operation_id.to_string(),
            base_version: Some(0),
            base_snapshot_id: Some("empty".to_string()),
            target_snapshot_id: Some("snap-committed".to_string()),
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
            created_at: "2026-07-14T00:00:00Z".to_string(),
            updated_at: "2026-07-14T00:00:00Z".to_string(),
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
        .expect("claimed operation")
        .claim;
    store
        .request_sync_operation_cancellation(operation_id, &current_timestamp())
        .expect("request cancellation");

    let runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    assert!(runtime.fail_daemon_sync_operation(
        &claim,
        &SyncOnceError::Runner(SyncRunnerError::Upload(
            UploadError::RemoteCommitCheckpoint("metadata unavailable".to_string()),
        )),
    ));

    assert_eq!(
        store
            .sync_operation_by_id(operation_id)
            .expect("operation query")
            .expect("operation")
            .state,
        SyncOperationState::ReconciliationRequired
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_does_not_bypass_pending_backoff_with_fresh_reconcile_rows() {
    let temp = unique_temp_dir("bowline-daemon-no-backoff-bypass");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_no_backoff_bypass");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-blocked".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::BlockedOffline,
            idempotency_key: "blocked-reconcile".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
            result_json: None,
            last_error_code: None,
            last_error: Some("offline".to_string()),
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("operation queued");

    let mut runtime = ContinuousSyncRuntime {
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
        tick_count: 42,
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
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    };

    assert_eq!(runtime.claim_daemon_sync_operation(), None);
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].id, "op-blocked");
    assert_eq!(operations[0].state, SyncOperationState::BlockedOffline);

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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-blocked".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::BlockedOffline,
            idempotency_key: "blocked-reconcile".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
            result_json: None,
            last_error_code: None,
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
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
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
    assert_eq!(operations[0].state, SyncOperationState::BlockedOffline);
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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-attention".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Attention,
            idempotency_key: "attention-reconcile".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
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
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "retry-backoff".to_string(),
            base_version: Some(1),
            base_snapshot_id: Some("snap-1".to_string()),
            target_snapshot_id: Some("snap-2".to_string()),
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 2,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: "2026-06-26T12:00:00Z".to_string(),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
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
        hosted_resolver: test_hosted_context_resolver(),
        store_health: StoreHealth::new(),
        claimant_id: "daemon-test".to_string(),
        store: CachedStore::new(state_root.join(DEFAULT_DATABASE_FILE)),
        pending_dirty: Default::default(),
        pending_dirty_roots: Default::default(),
    };

    let before = OffsetDateTime::now_utc();
    runtime.fail_daemon_sync_operation(
        &claim,
        &SyncOnceError::Runner(SyncRunnerError::Download(DownloadError::ByteStore(
            ByteStoreError::CorruptObject {
                key: ObjectKey::new("packs_pk_8899aabbccddeeff".to_string())
                    .expect("valid object key"),
                reason: "object bytes did not match metadata",
            },
        ))),
    );

    let operation = store
        .sync_operation_by_id(operation_id)
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::WaitingRetry);
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
