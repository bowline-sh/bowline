use super::*;

fn overlay_workspace_ref(workspace_id: &WorkspaceId, version: u64) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: workspace_id.clone(),
        version,
        snapshot_id: SnapshotId::new(format!("snap-overlay-{version}")),
        updated_at: ControlPlaneTimestamp { tick: version },
        updated_by_device_id: Some(DeviceId::new("device-test")),
    }
}

fn enqueue_overlay_operation(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    version: u64,
    generated_at: &str,
) -> SyncOperationRecord {
    let operation = bowline_local::sync::work_view_overlay_sync_operation(
        &overlay_workspace_ref(workspace_id, version),
        &DeviceId::new("device-test"),
        generated_at,
    )
    .expect("overlay operation");
    store
        .enqueue_sync_operation(&operation)
        .expect("overlay operation queued");
    operation
}

#[test]
fn restart_forces_canonical_reconcile_for_pointerless_conflict_occurrence() {
    let temp = unique_temp_dir("bowline-pointerless-conflict-restart");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let workspace_id = WorkspaceId::new("ws_pointerless_restart");
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "completed-before-pointerless-conflict".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Completed,
            idempotency_key: "completed-before-pointerless-conflict".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            claim_generation: 1,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: Some("{}".to_string()),
            last_error_code: None,
            last_error: None,
            created_at: "2026-07-13T10:00:00Z".to_string(),
            updated_at: "2026-07-13T10:00:00Z".to_string(),
        })
        .expect("completed reconcile");
    let mut conflict = ConflictRecord::same_path("src/restart.rs");
    conflict.base_snapshot_id = Some("snap_base".to_string());
    conflict.remote_snapshot_id = Some("snap_remote".to_string());
    let bundle = create_conflict_bundle(
        &state_root,
        conflict,
        &[ConflictFile {
            relative_path: "src/restart.rs".to_string(),
            base: Some(b"base".to_vec()),
            local: Some(b"local".to_vec()),
            remote: Some(b"remote".to_vec()),
        }],
    )
    .expect("pointerless conflict bundle");
    assert!(bundle.record.bundle_object.is_none());

    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    assert!(runtime.should_enqueue_daemon_reconcile(
        &store,
        &workspace_id,
        &DeviceId::new("device-test"),
        "2026-07-13T10:00:01Z",
    ));
    let claimed = runtime
        .claim_daemon_sync_operation()
        .expect("reconcile forced on restart");
    assert_eq!(claimed.operation.kind, SyncOperationKind::Reconcile);
    assert!(
        store
            .sync_operations(&workspace_id)
            .expect("operations")
            .iter()
            .all(|operation| operation.kind != SyncOperationKind::ConflictOccurrenceReconcile),
        "pointerless preparation stays in the runner's canonical uploader"
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_recovers_local_head_to_overlay_operation_after_enqueue_crash_gap() {
    let temp = unique_temp_dir("bowline-overlay-crash-gap");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let workspace_id = WorkspaceId::new("ws_overlay_crash_gap");
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T10:00:00Z")
        .expect("workspace");
    let workspace_ref = overlay_workspace_ref(&workspace_id, 7);
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: workspace_ref.clone(),
            observed_at: "2026-07-13T10:00:00Z".to_string(),
        })
        .expect("committed local head");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());

    let first = runtime
        .claim_daemon_sync_operation()
        .expect("recovered operation claimed");
    let claimed = if first.operation.kind == SyncOperationKind::WorkViewOverlaySync {
        first
    } else {
        runtime
            .claim_daemon_sync_operation()
            .expect("recovered overlay operation claimed")
    };
    assert_eq!(
        claimed.operation.kind,
        SyncOperationKind::WorkViewOverlaySync
    );
    assert_eq!(
        claimed.operation.target_snapshot_id.as_deref(),
        Some(workspace_ref.snapshot_id.as_str())
    );
    runtime.claim_daemon_sync_operation();
    assert_eq!(
        store
            .sync_operations(&workspace_id)
            .expect("operations")
            .iter()
            .filter(|operation| operation.kind == SyncOperationKind::WorkViewOverlaySync)
            .count(),
        1,
        "repeated crash-gap scans must deduplicate the committed ref"
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn post_commit_resource_claims_overlay_operations_in_workspace_order() {
    let temp = unique_temp_dir("bowline-overlay-order");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_overlay_order");
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    enqueue_overlay_operation(&store, &workspace_id, 1, "2026-07-13T10:00:00Z");
    enqueue_overlay_operation(&store, &workspace_id, 2, "2026-07-13T10:00:01Z");

    let first = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-order",
            "2026-07-13T10:00:02Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("first operation");
    assert_eq!(first.operation.base_version, Some(1));
    assert!(
        store
            .claim_next_sync_operation(
                &workspace_id,
                "daemon-order-2",
                "2026-07-13T10:00:02Z",
                "2999-01-01T00:00:00Z",
            )
            .expect("concurrent claim")
            .is_none(),
        "the PostCommit resource serializes work per workspace"
    );
    store
        .complete_claimed_sync_operation(
            &first.claim,
            r#"{"uploaded":0,"attention":0}"#,
            "2026-07-13T10:00:03Z",
        )
        .expect("first completion");
    let second = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-order",
            "2026-07-13T10:00:04Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("second operation");
    assert_eq!(second.operation.base_version, Some(2));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn overlay_worker_retries_then_completes_and_recovers_component() {
    let temp = unique_temp_dir("bowline-overlay-retry-success");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let workspace_id = WorkspaceId::new("ws_overlay_retry");
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    enqueue_overlay_operation(&store, &workspace_id, 1, "2026-07-13T10:00:00Z");
    let first = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            "2026-07-13T10:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("operation");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    runtime.process_claimed_work_view_overlay_sync(first, |_, _| {
        Err(SyncOnceError::ControlPlane(ControlPlaneError::Storage(
            "transient hosted failure".to_string(),
        )))
    });
    let retrying = store
        .sync_operations(&workspace_id)
        .expect("operations")
        .into_iter()
        .find(|operation| operation.kind == SyncOperationKind::WorkViewOverlaySync)
        .expect("overlay operation");
    assert_eq!(retrying.state, SyncOperationState::WaitingRetry);
    assert_eq!(
        store
            .post_commit_component_state(PostCommitSyncComponent::WorkViewOverlaySync)
            .expect("component"),
        Some(bowline_core::status::ComponentState::Degraded)
    );

    let retry = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            "2999-01-01T00:00:00Z",
            "3999-01-01T00:00:00Z",
        )
        .expect("retry claim")
        .expect("retry operation");
    runtime.process_claimed_work_view_overlay_sync(retry, |_, _| {
        Ok(WorkViewOverlaySyncResult {
            uploaded: 2,
            attention: 0,
            ..WorkViewOverlaySyncResult::default()
        })
    });
    let completed = store
        .sync_operations(&workspace_id)
        .expect("operations")
        .into_iter()
        .find(|operation| operation.kind == SyncOperationKind::WorkViewOverlaySync)
        .expect("overlay operation");
    assert_eq!(completed.state, SyncOperationState::Completed);
    assert_eq!(completed.attempt_count, 2);
    assert_eq!(
        store
            .post_commit_component_state(PostCommitSyncComponent::WorkViewOverlaySync)
            .expect("component"),
        Some(bowline_core::status::ComponentState::Ready)
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn overlay_partial_failure_with_cancellation_requires_reconciliation() {
    let temp = unique_temp_dir("bowline-overlay-partial-cancel");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let workspace_id = WorkspaceId::new("ws_overlay_partial_cancel");
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    let operation = enqueue_overlay_operation(&store, &workspace_id, 1, "2026-07-13T10:00:00Z");
    let first = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            "2026-07-13T10:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("operation");
    let operation_id = operation.id.clone();
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    runtime.process_claimed_work_view_overlay_sync(first, move |args, _| {
        MetadataStore::open(args.state_root.join(DEFAULT_DATABASE_FILE))
            .expect("worker store")
            .request_sync_operation_cancellation(&operation_id, &current_timestamp())
            .expect("cancellation request");
        Err(SyncOnceError::ControlPlane(ControlPlaneError::Storage(
            "later overlay failed after an earlier commit".to_string(),
        )))
    });
    let reconciling = store
        .sync_operation_by_id(&operation.id)
        .expect("operation")
        .expect("stored operation");
    assert_eq!(
        reconciling.state,
        SyncOperationState::ReconciliationRequired
    );

    let recovery = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            "2999-01-01T00:00:00Z",
            "3999-01-01T00:00:00Z",
        )
        .expect("recovery claim")
        .expect("reconciliation operation");
    assert_eq!(
        recovery.claim.claimed_from_state(),
        SyncOperationState::ReconciliationRequired
    );
    runtime.process_claimed_work_view_overlay_sync(recovery, |_, _| {
        Ok(WorkViewOverlaySyncResult {
            uploaded: 1,
            attention: 0,
            ..WorkViewOverlaySyncResult::default()
        })
    });
    let completed = store
        .sync_operation_by_id(&operation.id)
        .expect("operation")
        .expect("stored operation");
    assert_eq!(completed.state, SyncOperationState::Completed);
    assert!(
        completed
            .result_json
            .expect("completion result")
            .contains("committed-cancelled-late")
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn overlay_worker_stops_before_domain_call_on_cancellation_or_lost_claim() {
    let temp = unique_temp_dir("bowline-overlay-boundaries");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("workspace root");
    let workspace_id = WorkspaceId::new("ws_overlay_boundaries");
    let store = open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
    enqueue_overlay_operation(&store, &workspace_id, 1, "2026-07-13T10:00:00Z");
    let cancelled = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-test",
            "2026-07-13T10:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("operation");
    store
        .request_sync_operation_cancellation(cancelled.claim.operation_id(), "2026-07-13T10:00:02Z")
        .expect("cancel request");
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    let cancellation_calls = Arc::new(AtomicUsize::new(0));
    let cancellation_calls_for_worker = Arc::clone(&cancellation_calls);
    runtime.process_claimed_work_view_overlay_sync(cancelled, move |_, _| {
        cancellation_calls_for_worker.fetch_add(1, Ordering::SeqCst);
        Ok(WorkViewOverlaySyncResult {
            uploaded: 0,
            attention: 0,
            ..WorkViewOverlaySyncResult::default()
        })
    });
    assert_eq!(cancellation_calls.load(Ordering::SeqCst), 0);

    enqueue_overlay_operation(&store, &workspace_id, 2, "2026-07-13T10:00:03Z");
    let stale = store
        .claim_next_sync_operation(
            &workspace_id,
            "dead-daemon",
            "1970-01-01T00:00:00Z",
            "1970-01-01T00:00:01Z",
        )
        .expect("claim")
        .expect("operation");
    store
        .requeue_expired_sync_claims(&workspace_id, "2026-07-13T10:00:04Z")
        .expect("expired claim requeued");
    let replacement = store
        .claim_next_sync_operation(
            &workspace_id,
            "replacement-daemon",
            "2026-07-13T10:00:05Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("replacement claim")
        .expect("operation");
    let lost_claim_calls = Arc::new(AtomicUsize::new(0));
    let lost_claim_calls_for_worker = Arc::clone(&lost_claim_calls);
    runtime.process_claimed_work_view_overlay_sync(stale, move |_, _| {
        lost_claim_calls_for_worker.fetch_add(1, Ordering::SeqCst);
        Ok(WorkViewOverlaySyncResult {
            uploaded: 0,
            attention: 0,
            ..WorkViewOverlaySyncResult::default()
        })
    });
    assert_eq!(lost_claim_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .sync_operation_by_id(replacement.claim.operation_id())
            .expect("operation")
            .expect("replacement")
            .claimed_by
            .as_deref(),
        Some("replacement-daemon")
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_requeues_expired_claims_before_next_sync() {
    let temp = unique_temp_dir("bowline-daemon-requeue-expired");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_requeue");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-expired".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "expired".to_string(),
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
            created_at: "1970-01-01T00:00:00Z".to_string(),
            updated_at: "1970-01-01T00:00:00Z".to_string(),
        })
        .expect("operation queued");
    store
        .claim_next_sync_operation(
            &workspace_id,
            "dead-daemon",
            "1970-01-01T00:00:00Z",
            "1970-01-01T00:00:01Z",
        )
        .expect("claim query")
        .expect("expired operation claimed");
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

    runtime.requeue_expired_sync_claims();

    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations[0].state, SyncOperationState::Queued);
    assert_eq!(operations[0].claimed_by, None);
    assert_eq!(operations[0].heartbeat_at, None);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_poll_reconciles_expired_cancelled_claim_before_terminal_completion() {
    let temp = unique_temp_dir("bowline-daemon-reconcile-expired-cancelled");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_reconcile_expired_cancelled");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "op-expired-cancelled".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "expired-cancelled".to_string(),
            base_version: Some(0),
            base_snapshot_id: Some("empty".to_string()),
            target_snapshot_id: Some("snap-committed".to_string()),
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
            created_at: "1970-01-01T00:00:00Z".to_string(),
            updated_at: "1970-01-01T00:00:00Z".to_string(),
        })
        .expect("operation queued");
    let crashed = store
        .claim_next_sync_operation(
            &workspace_id,
            "dead-daemon",
            "1970-01-01T00:00:00Z",
            "1970-01-01T00:00:01Z",
        )
        .expect("claim query")
        .expect("operation claimed");
    store
        .request_sync_operation_cancellation(crashed.claim.operation_id(), &current_timestamp())
        .expect("cancellation request");
    let committed_ref = WorkspaceRef {
        workspace_id: workspace_id.clone(),
        version: 1,
        snapshot_id: SnapshotId::new("snap-committed"),
        updated_at: ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(DeviceId::new("device-a")),
    };
    let committed_ref_for_executor = committed_ref.clone();
    let attempts = Arc::new(Mutex::new(0_u64));
    let attempts_for_executor = Arc::clone(&attempts);
    let mut runtime = watcher_test_runtime(root, state_root.clone(), workspace_id.as_str());
    runtime.latest_observed_ref = Some(committed_ref.clone());
    runtime.next_remote_observe = Instant::now() + Duration::from_secs(60);
    runtime.sync_once = Box::new(move |args, observed_ref| {
        let mut attempts = attempts_for_executor.lock().expect("attempt count lock");
        *attempts += 1;
        if *attempts == 1 {
            return Err(SyncOnceError::ControlPlane(ControlPlaneError::Storage(
                "remote ref not caught up after crash".to_string(),
            )));
        }
        assert_eq!(observed_ref, Some(committed_ref_for_executor.clone()));
        let store = MetadataStore::open(args.state_root.join(DEFAULT_DATABASE_FILE))
            .expect("reconciliation metadata opens");
        store
            .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
                workspace_ref: committed_ref_for_executor.clone(),
                observed_at: current_timestamp(),
            })
            .expect("reconciliation persists local head");
        Ok(SyncOnceSummary {
            workspace_id: committed_ref_for_executor.workspace_id.as_str().to_string(),
            snapshot_id: committed_ref_for_executor.snapshot_id.as_str().to_string(),
            version: committed_ref_for_executor.version,
            outcome: SyncSummaryOutcome::Imported,
            snapshot_root_manifest_id: None,
            manifest_object_key: None,
            namespace_root_id: None,
            conflict_count: 0,
            conflicts: Vec::new(),
            scan: SyncScanSummary::default(),
            cancelled_late: true,
        })
    });

    runtime.poll();

    let deferred = store
        .sync_operation_by_id(crashed.claim.operation_id())
        .expect("deferred operation query")
        .expect("deferred operation remains");
    assert_eq!(deferred.state, SyncOperationState::ReconciliationRequired);
    assert_eq!(deferred.last_error_code, None);
    assert!(
        store
            .workspace_sync_head(&workspace_id)
            .expect("local head before reconciliation")
            .is_none()
    );

    runtime.next_tick = Instant::now();
    runtime.poll();

    let local_head = store
        .workspace_sync_head(&workspace_id)
        .expect("local head query")
        .expect("reconciled local head");
    assert_eq!(local_head.workspace_ref, committed_ref);
    let completed = store
        .sync_operation_by_id(crashed.claim.operation_id())
        .expect("operation query")
        .expect("operation remains");
    assert_eq!(completed.state, SyncOperationState::Completed);
    let result = completed.result_json.expect("completion result");
    assert!(result.contains("committed-cancelled-late"));
    assert!(!result.contains(r#"\"outcome\":\"cancelled\""#));
    assert_eq!(*attempts.lock().expect("attempt count lock"), 2);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_restart_idles_after_recent_completed_tick_operation() {
    let temp = unique_temp_dir("bowline-daemon-restart-operation-id");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_restart_operation_id");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-tick-1".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Completed,
            idempotency_key: "daemon-sync:device-test:1".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
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
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
        })
        .expect("completed operation inserted");
    let mut runtime = watcher_test_runtime(
        temp.join("Code"),
        state_root.clone(),
        "ws_restart_operation_id",
    );

    assert_eq!(runtime.claim_daemon_sync_operation(), None);

    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, SyncOperationState::Completed);

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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-completed".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Completed,
            idempotency_key: "daemon-sync:device-a:completed".to_string(),
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
            last_error: None,
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
        })
        .expect("completed operation inserted");
    let sync_calls = Arc::new(Mutex::new(0_u64));
    let sync_calls_for_executor = Arc::clone(&sync_calls);
    let dispatch_calls = Arc::new(Mutex::new(0_u64));
    let dispatch_calls_for_claimer = Arc::clone(&dispatch_calls);
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
            interval: Duration::from_secs(3600),
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
        sync_once: Box::new(move |_, _| {
            *sync_calls_for_executor
                .lock()
                .expect("sync call count lock") += 1;
            Err(SyncOnceError::ControlPlane(ControlPlaneError::Storage(
                "idle poll must not run sync-once".to_string(),
            )))
        }),
        remote_ref_observer: noop_remote_ref_observer(),
        dispatch_claimer: Box::new(move |_| {
            *dispatch_calls_for_claimer
                .lock()
                .expect("dispatch call count lock") += 1;
            Ok(None)
        }),
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

    assert_eq!(
        *sync_calls.lock().expect("sync call count lock"),
        0,
        "idle daemon poll must not call hosted sync work"
    );
    assert_eq!(
        *dispatch_calls.lock().expect("dispatch call count lock"),
        1,
        "first idle daemon poll should check dispatch leases"
    );
    assert_eq!(runtime.tick_count, 1);
    runtime.next_tick = Instant::now();
    runtime.poll();
    assert_eq!(
        *dispatch_calls.lock().expect("dispatch call count lock"),
        1,
        "dispatch lease checks are throttled across repeated idle polls"
    );
    assert!(
        runtime.status_json().contains("\"state\":\"idle\""),
        "{}",
        runtime.status_json()
    );
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, SyncOperationState::Completed);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_post_sync_skips_dispatch_claim_after_idle_poll_found_none() {
    let temp = unique_temp_dir("bowline-daemon-post-sync-dispatch-throttle");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_post_sync_dispatch_throttle");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");

    let dispatch_calls = Arc::new(Mutex::new(0_u64));
    let dispatch_calls_for_claimer = Arc::clone(&dispatch_calls);
    let mut runtime = watcher_test_runtime(root.clone(), state_root.clone(), workspace_id.as_str());
    runtime.next_tick = Instant::now();
    runtime.next_dispatch_claim = Instant::now() + Duration::from_secs(30);
    runtime.awaiting_handoff = false;
    runtime.dispatch_claimer = Box::new(move |_| {
        *dispatch_calls_for_claimer
            .lock()
            .expect("dispatch call count lock") += 1;
        Ok(None)
    });

    enqueue_status_pin_operation(&store, &workspace_id, "op-sync", "queued");
    runtime.next_tick = Instant::now();
    runtime.sync_once = Box::new(|_, _| {
        Ok(SyncOnceSummary {
            workspace_id: "ws_post_sync_dispatch_throttle".to_string(),
            snapshot_id: "snap-post-sync".to_string(),
            version: 1,
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
        *dispatch_calls.lock().expect("dispatch call count lock"),
        0,
        "post-sync dispatch claim should not force list_leases when no handoff is awaited"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_claims_reconcile_when_local_write_is_newer_than_completed_tick() {
    let temp = unique_temp_dir("bowline-daemon-local-write-reconcile");
    let state_root = temp.join(".state");
    let workspace_id = WorkspaceId::new("ws_local_write_reconcile");
    let device_id = DeviceId::new("device-test");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-completed".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Completed,
            idempotency_key: "daemon-sync:device-test:completed".to_string(),
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
    let mut runtime = watcher_test_runtime(
        temp.join("Code"),
        state_root.clone(),
        "ws_local_write_reconcile",
    );

    let claimed = runtime
        .claim_daemon_sync_operation()
        .expect("local write queues sync");

    assert_ne!(claimed.operation.id, "daemon-sync-completed");
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 2);
    assert!(
        operations
            .iter()
            .any(|operation| operation.state == SyncOperationState::Claimed)
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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-completed".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Completed,
            idempotency_key: "daemon-sync:device-test:completed".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(device_id),
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
        .expect("completed operation inserted");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 1,
                snapshot_id: SnapshotId::new("snap-local"),
                updated_at: ControlPlaneTimestamp { tick: 1 },
                updated_by_device_id: Some(DeviceId::new("device-a")),
            },
            observed_at: "2999-01-01T00:00:00Z".to_string(),
        })
        .expect("local head inserted");
    let mut runtime = watcher_test_runtime(
        temp.join("Code"),
        state_root.clone(),
        "ws_remote_observer_reconcile",
    );
    runtime.remote_ref_observer = test_remote_ref_observer(|_| {
        Ok(Some(WorkspaceRef {
            workspace_id: WorkspaceId::new("ws_remote_observer_reconcile"),
            version: 2,
            snapshot_id: SnapshotId::new("snap-remote"),
            updated_at: ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some(DeviceId::new("device-b")),
        }))
    });

    assert!(runtime.observe_remote_ref_cursor());
    let claimed = runtime
        .claim_daemon_sync_operation()
        .expect("remote cursor advance queues sync");

    assert_ne!(claimed.operation.id, "daemon-sync-completed");
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
        workspace_id: WorkspaceId::new(workspace_id),
        version: 2,
        snapshot_id: SnapshotId::new("snap-stale"),
        updated_at: ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(DeviceId::new("device-b")),
    });
    runtime.remote_ref_observer = test_remote_ref_observer(|_| Ok(None));

    assert!(!runtime.observe_remote_ref_cursor());
    assert_eq!(runtime.latest_observed_ref, None);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_startup_does_not_steal_a_live_claim() {
    let temp = unique_temp_dir("bowline-daemon-startup-requeue-claimed");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_startup_requeue_claimed");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-before-restart".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "daemon-sync:device-test:claimed-before-restart".to_string(),
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
            created_at: "2026-06-26T00:00:00Z".to_string(),
            updated_at: "2026-06-26T00:00:00Z".to_string(),
        })
        .expect("claimed operation inserted");
    store
        .claim_next_sync_operation(
            &workspace_id,
            "old-daemon-process",
            &current_timestamp(),
            "2999-01-01T00:00:00Z",
        )
        .expect("claim query")
        .expect("operation claimed");

    let mut runtime = ContinuousSyncRuntime::new(ContinuousSyncOptions {
        args: SyncOnceArgs {
            root,
            state_root: state_root.clone(),
            workspace_id: workspace_id.as_str().to_string(),
            device_id: "device-test".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    });

    let operation = store
        .sync_operation_by_id("daemon-sync-before-restart")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::Claimed);
    assert_eq!(operation.claimed_by.as_deref(), Some("old-daemon-process"));
    assert!(runtime.claim_daemon_sync_operation().is_none());
    let operations = store
        .sync_operations(&workspace_id)
        .expect("operations read");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, SyncOperationState::Claimed);
    assert_eq!(
        operations[0].claimed_by.as_deref(),
        Some("old-daemon-process")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_startup_preserves_retry_backoff_after_restart() {
    let temp = unique_temp_dir("bowline-daemon-startup-requeue-retry");
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root");
    let workspace_id = WorkspaceId::new("ws_startup_requeue_retry");
    let store =
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-before-repair".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::WaitingRetry,
            idempotency_key: "daemon-sync:device-test:retry-before-repair".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 4,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
            result_json: None,
            last_error_code: None,
            last_error: Some(
                "daemon sync requires account session credentials, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session"
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
            sync_claim: None,
            scan_scope: Default::default(),
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    };
    requeue_startup_sync_claims_with_resolved_attention(&options, true, false);

    let operation = store
        .sync_operation_by_id("daemon-sync-before-repair")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::WaitingRetry);
    assert_eq!(
        operation.next_attempt_at.as_deref(),
        Some("2999-01-01T00:00:00Z")
    );
    assert_eq!(
        operation.last_error.as_deref(),
        Some(
            "daemon sync requires account session credentials, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session"
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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-missing-convex".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Attention,
            idempotency_key: "daemon-sync:device-test:missing-convex".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
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
            sync_claim: None,
            scan_scope: Default::default(),
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    };
    requeue_startup_sync_claims_with_resolved_attention(&options, true, false);

    let operation = store
        .sync_operation_by_id("daemon-sync-missing-convex")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::Queued);
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
        open_store_for_test(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "daemon-sync-missing-workspace-key".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Attention,
            idempotency_key: "daemon-sync:device-test:missing-workspace-key".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-test")),
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
            sync_claim: None,
            scan_scope: Default::default(),
        },
        interval: Duration::from_secs(60),
        max_ticks: None,
    };
    requeue_startup_sync_claims_with_resolved_attention(&options, false, true);

    let operation = store
        .sync_operation_by_id("daemon-sync-missing-workspace-key")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::Queued);
    assert_eq!(operation.last_error, None);
    assert_eq!(operation.next_attempt_at, None);

    let _ = fs::remove_dir_all(temp);
}
