use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::metadata::{
    MetadataStore, RemoteRefCursorRecord, SyncClaimTransition, SyncCommittedCancelledLateResult,
    SyncOperationCheckpointRecord, SyncOperationEnqueueOutcome, SyncOperationKind,
    SyncOperationRecord, SyncOperationState, SyncResourceKey, WorkspaceSyncHeadRecord,
};

#[test]
fn workspace_head_and_sync_queue_survive_reopen() {
    let temp = unique_temp_dir("bowline-sync-daemon-queue");
    let db_path = temp.join(".state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_queue");
    let store = MetadataStore::open(&db_path).expect("metadata opens");

    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: workspace_ref("ws_queue", 7, "snap-7"),
            observed_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("head stored");
    store
        .enqueue_sync_operation(&operation(
            "op-upload-1",
            SyncOperationState::Queued,
            "idem-upload-1",
            &workspace_id,
        ))
        .expect("operation enqueued");
    store
        .put_remote_ref_cursor(&RemoteRefCursorRecord {
            workspace_id: workspace_id.clone(),
            cursor: Some("cursor-7".to_string()),
            last_observed_version: Some(7),
            last_observed_snapshot_id: Some("snap-7".to_string()),
            updated_at: "2026-06-26T12:00:00Z".to_string(),
        })
        .expect("cursor stored");
    drop(store);

    let reopened = MetadataStore::open(&db_path).expect("metadata reopens");
    let head = reopened
        .workspace_sync_head(&workspace_id)
        .expect("head reads")
        .expect("head exists");
    assert_eq!(head.workspace_ref.snapshot_id, "snap-7");
    assert_eq!(head.workspace_ref.version, 7);

    let cursor = reopened
        .remote_ref_cursor(&workspace_id)
        .expect("cursor reads")
        .expect("cursor exists");
    assert_eq!(cursor.cursor.as_deref(), Some("cursor-7"));

    let claimed = reopened
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-a",
            "2026-06-26T12:00:01Z",
            "2999-06-26T12:00:03Z",
        )
        .expect("claim succeeds")
        .expect("operation claimed");
    assert_eq!(claimed.operation.id, "op-upload-1");
    assert_eq!(claimed.operation.state, SyncOperationState::Claimed);
    assert_eq!(claimed.operation.claimed_by.as_deref(), Some("daemon-a"));
    assert_eq!(claimed.operation.attempt_count, 1);
    assert!(
        reopened
            .claim_next_sync_operation(
                &workspace_id,
                "daemon-b",
                "2026-06-26T12:00:01Z",
                "2999-06-26T12:01:01Z",
            )
            .expect("second claim query succeeds")
            .is_none()
    );

    assert_eq!(
        reopened
            .renew_sync_operation_claim(
                &claimed.claim,
                "2026-06-26T12:00:02Z",
                "2000-06-26T12:00:03Z",
            )
            .expect("heartbeat refreshes"),
        SyncClaimTransition::Applied
    );
    assert_eq!(
        reopened
            .requeue_expired_sync_claims(&workspace_id, "2026-06-26T12:00:04Z")
            .expect("expired claim requeues"),
        1
    );
    let reclaimed = reopened
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-b",
            "2026-06-26T12:00:05Z",
            "2999-06-26T12:01:05Z",
        )
        .expect("reclaim succeeds")
        .expect("operation reclaimed");
    assert_eq!(reclaimed.operation.claimed_by.as_deref(), Some("daemon-b"));
    assert_eq!(reclaimed.operation.attempt_count, 2);

    reopened
        .fail_claimed_sync_operation_for_retry(
            &reclaimed.claim,
            "temporary-network",
            "temporary network failure",
            "2026-06-26T12:00:30Z",
            "2026-06-26T12:00:06Z",
        )
        .expect("retry recorded");
    let counts = reopened
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(counts.waiting_retry, 1);

    assert!(
        reopened
            .claim_next_sync_operation(
                &workspace_id,
                "daemon-c",
                "2026-06-26T12:00:29Z",
                "2999-06-26T12:01:29Z",
            )
            .expect("early retry claim query succeeds")
            .is_none()
    );
    let retry_claim = reopened
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-c",
            "2026-06-26T12:00:30Z",
            "2999-06-26T12:01:30Z",
        )
        .expect("due retry claims")
        .expect("retry operation claimed");
    assert_eq!(retry_claim.operation.attempt_count, 3);

    reopened
        .block_claimed_sync_operation_offline(
            &retry_claim.claim,
            "offline",
            "offline",
            "2026-06-26T12:01:00Z",
            "2026-06-26T12:00:31Z",
        )
        .expect("offline state recorded");
    let offline_counts = reopened
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(offline_counts.blocked_offline, 1);
    let active = reopened
        .active_sync_operation_for_device(
            &workspace_id,
            SyncOperationKind::Reconcile,
            &DeviceId::new("device-a"),
        )
        .expect("active operation lookup")
        .expect("offline operation is active");
    assert_eq!(active.id, "op-upload-1");

    assert!(
        reopened
            .claim_next_sync_operation(
                &workspace_id,
                "daemon-d",
                "2026-06-26T12:00:59Z",
                "2999-06-26T12:01:59Z",
            )
            .expect("early offline claim query succeeds")
            .is_none()
    );
    let offline_claim = reopened
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-d",
            "2026-06-26T12:01:00Z",
            "2999-06-26T12:02:00Z",
        )
        .expect("offline claim query succeeds")
        .expect("offline operation reclaims when scheduler wakes");
    assert_eq!(offline_claim.operation.attempt_count, 4);

    reopened
        .mark_claimed_sync_operation_attention(
            &offline_claim.claim,
            "trusted-device-required",
            "trusted device required",
            "2026-06-26T12:00:33Z",
        )
        .expect("attention state recorded");
    let attention_counts = reopened
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(attention_counts.attention, 1);

    reopened
        .requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            SyncOperationKind::Reconcile,
            &DeviceId::new("device-a"),
            "trusted device required",
            "2026-06-26T12:01:01Z",
        )
        .expect("attention operation requeued");
    let completion_claim = reopened
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-e",
            "2026-06-26T12:01:01Z",
            "2999-06-26T12:02:01Z",
        )
        .expect("completion claim")
        .expect("requeued operation");
    reopened
        .complete_claimed_sync_operation(
            &completion_claim.claim,
            r#"{"completed":true}"#,
            "2026-06-26T12:01:02Z",
        )
        .expect("completion recorded");
    let completed_counts = reopened
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(completed_counts.completed, 1);
    assert!(
        reopened
            .active_sync_operation_for_device(
                &workspace_id,
                SyncOperationKind::Reconcile,
                &DeviceId::new("device-a"),
            )
            .expect("active operation lookup")
            .is_none()
    );

    let _ = fs::remove_dir_all(&temp);
}

fn operation(
    id: &str,
    state: SyncOperationState,
    idempotency_key: &str,
    workspace_id: &WorkspaceId,
) -> SyncOperationRecord {
    SyncOperationRecord {
        id: id.to_string(),
        workspace_id: workspace_id.clone(),
        kind: SyncOperationKind::Reconcile,
        resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
        state,
        idempotency_key: idempotency_key.to_string(),
        base_version: Some(7),
        base_snapshot_id: Some("snap-7".to_string()),
        target_snapshot_id: Some("snap-8".to_string()),
        device_id: Some(DeviceId::new("device-a")),
        payload_json: r#"{"candidate":"snap-8"}"#.to_string(),
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
    }
}

#[test]
fn concurrent_connections_cannot_claim_the_same_operation() {
    let temp = unique_temp_dir("bowline-sync-claim-race");
    let db_path = temp.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_claim_race");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .enqueue_sync_operation(&operation(
            "op-race",
            SyncOperationState::Queued,
            "idem-race",
            &workspace_id,
        ))
        .expect("operation enqueued");
    drop(store);

    let barrier = Arc::new(Barrier::new(3));
    let workers = ["daemon-a", "daemon-b"].map(|owner| {
        let barrier = Arc::clone(&barrier);
        let db_path = db_path.clone();
        let workspace_id = workspace_id.clone();
        std::thread::spawn(move || {
            let store = MetadataStore::open(db_path).expect("worker metadata opens");
            barrier.wait();
            store
                .claim_next_sync_operation(
                    &workspace_id,
                    owner,
                    "2026-07-13T12:00:00Z",
                    "2999-07-13T12:01:00Z",
                )
                .expect("claim query succeeds")
        })
    });
    barrier.wait();
    let claims = workers
        .into_iter()
        .filter_map(|worker| worker.join().expect("claim worker joins"))
        .collect::<Vec<_>>();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].claim.generation(), 1);

    let stored = MetadataStore::open(&db_path)
        .expect("metadata reopens")
        .sync_operation_by_id("op-race")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(stored.state, SyncOperationState::Claimed);
    assert_eq!(stored.claim_generation, 1);
    assert_eq!(stored.claimed_by.as_deref(), Some(claims[0].claim.owner()));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn dispatch_failure_requeues_under_the_exact_claim_fence() {
    let temp = unique_temp_dir("bowline-sync-dispatch-requeue");
    let db_path = temp.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_dispatch_requeue");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .enqueue_sync_operation(&operation(
            "op-dispatch-requeue",
            SyncOperationState::Queued,
            "idem-dispatch-requeue",
            &workspace_id,
        ))
        .expect("operation enqueued");
    let first = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-a",
            "2026-07-13T12:00:00Z",
            "2999-07-13T12:01:00Z",
        )
        .expect("claim query")
        .expect("claim");

    assert_eq!(
        store
            .requeue_claimed_sync_operation_after_dispatch_failure(
                &first.claim,
                "dispatch-unavailable",
                "sync lane disconnected before execution",
                "2026-07-13T12:00:01Z",
            )
            .expect("dispatch failure requeue"),
        SyncClaimTransition::Applied
    );
    assert_eq!(
        store
            .requeue_claimed_sync_operation_after_dispatch_failure(
                &first.claim,
                "dispatch-unavailable",
                "stale worker retry",
                "2026-07-13T12:00:02Z",
            )
            .expect("stale requeue checked"),
        SyncClaimTransition::OwnershipLost
    );
    let queued = store
        .sync_operation_by_id("op-dispatch-requeue")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(queued.state, SyncOperationState::Queued);
    assert_eq!(queued.claimed_by, None);
    assert_eq!(
        queued.last_error_code.as_deref(),
        Some("dispatch-unavailable")
    );

    let replacement = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-b",
            "2026-07-13T12:00:03Z",
            "2999-07-13T12:01:03Z",
        )
        .expect("replacement claim query")
        .expect("replacement claim");
    assert_eq!(replacement.claim.generation(), 2);
    assert_eq!(
        store
            .record_claimed_sync_operation_worker_failure(
                &replacement.claim,
                "worker-panicked",
                "sync worker terminated unexpectedly",
                "2026-07-13T12:00:04Z",
            )
            .expect("worker failure recorded"),
        SyncClaimTransition::Applied
    );
    assert_eq!(
        store
            .record_claimed_sync_operation_worker_failure(
                &first.claim,
                "worker-panicked",
                "stale worker report",
                "2026-07-13T12:00:04Z",
            )
            .expect("stale worker failure checked"),
        SyncClaimTransition::OwnershipLost
    );
    let claimed = store
        .sync_operation_by_id("op-dispatch-requeue")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(claimed.state, SyncOperationState::Claimed);
    assert_eq!(claimed.last_error_code.as_deref(), Some("worker-panicked"));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn concurrent_same_key_enqueues_report_one_insert_and_one_existing_row() {
    let temp = unique_temp_dir("bowline-sync-enqueue-race");
    let db_path = temp.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_enqueue_race");
    MetadataStore::open(&db_path).expect("metadata initializes");
    let barrier = Arc::new(Barrier::new(3));
    let workers = [0, 1].map(|_| {
        let barrier = Arc::clone(&barrier);
        let db_path = db_path.clone();
        let workspace_id = workspace_id.clone();
        std::thread::spawn(move || {
            let store = MetadataStore::open(db_path).expect("worker metadata opens");
            let input = operation(
                "op-same-key",
                SyncOperationState::Queued,
                "idem-same-key",
                &workspace_id,
            );
            barrier.wait();
            store
                .enqueue_sync_operation(&input)
                .expect("same-key enqueue succeeds")
        })
    });
    barrier.wait();
    let outcomes = workers.map(|worker| worker.join().expect("enqueue worker joins"));
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SyncOperationEnqueueOutcome::Inserted(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SyncOperationEnqueueOutcome::Existing(_)))
            .count(),
        1
    );
    assert!(outcomes.iter().all(|outcome| {
        outcome.operation().id == "op-same-key"
            && outcome.operation().idempotency_key == "idem-same-key"
    }));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn expired_claim_reclaims_from_checkpoint_and_fences_the_paused_worker() {
    let temp = unique_temp_dir("bowline-sync-reclaim-resume");
    let db_path = temp.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_reclaim_resume");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .enqueue_sync_operation(&operation(
            "op-reclaim",
            SyncOperationState::Queued,
            "idem-reclaim",
            &workspace_id,
        ))
        .expect("operation enqueued");
    let first = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-paused",
            "2026-07-13T12:00:00Z",
            "2999-07-13T12:01:00Z",
        )
        .expect("first claim query")
        .expect("first claim");
    assert_eq!(
        store
            .append_claimed_sync_operation_checkpoint(
                &first.claim,
                &SyncOperationCheckpointRecord {
                    id: "checkpoint-uploaded".to_string(),
                    workspace_id: workspace_id.clone(),
                    operation_id: "op-reclaim".to_string(),
                    step: "objects-uploaded".to_string(),
                    state: "completed".to_string(),
                    payload_json: r#"{"count":2}"#.to_string(),
                    created_at: "2026-07-13T12:00:01Z".to_string(),
                    updated_at: "2026-07-13T12:00:01Z".to_string(),
                },
            )
            .expect("checkpoint append"),
        SyncClaimTransition::Applied
    );
    assert_eq!(
        store
            .renew_sync_operation_claim(
                &first.claim,
                "2026-07-13T12:00:02Z",
                "2000-01-01T00:00:00Z",
            )
            .expect("lease is forced expired"),
        SyncClaimTransition::Applied
    );
    assert_eq!(
        store
            .requeue_expired_sync_claims(&workspace_id, "2026-07-13T12:00:03Z")
            .expect("expired claim requeued"),
        1
    );

    let second = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-resumed",
            "2026-07-13T12:00:04Z",
            "2999-07-13T12:01:04Z",
        )
        .expect("second claim query")
        .expect("second claim");
    assert_eq!(second.claim.generation(), 2);
    assert_eq!(
        store
            .complete_claimed_sync_operation(
                &first.claim,
                r#"{"worker":"paused"}"#,
                "2026-07-13T12:00:05Z",
            )
            .expect("stale completion checked"),
        SyncClaimTransition::OwnershipLost
    );
    assert_eq!(
        store
            .sync_operation_checkpoints("op-reclaim")
            .expect("checkpoints")
            .len(),
        1
    );
    assert_eq!(
        store
            .complete_claimed_sync_operation(
                &second.claim,
                r#"{"worker":"resumed"}"#,
                "2026-07-13T12:00:06Z",
            )
            .expect("resumed completion"),
        SyncClaimTransition::Applied
    );
    let operation = store
        .sync_operation_by_id("op-reclaim")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::Completed);
    assert_eq!(
        operation.result_json.as_deref(),
        Some(r#"{"worker":"resumed"}"#)
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn cancellation_fences_generic_completion_and_allows_typed_committed_late_settlement() {
    let temp = unique_temp_dir("bowline-sync-cancel-fence");
    let db_path = temp.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_cancel_fence");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .enqueue_sync_operation(&operation(
            "op-cancel-fence",
            SyncOperationState::Queued,
            "idem-cancel-fence",
            &workspace_id,
        ))
        .expect("operation enqueued");
    let claimed = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-owner",
            "2026-07-13T12:00:00Z",
            "2999-07-13T12:01:00Z",
        )
        .expect("claim query")
        .expect("claim");
    assert_eq!(
        store
            .request_sync_operation_cancellation("op-cancel-fence", "2026-07-13T12:00:01Z",)
            .expect("cancellation requested"),
        Some(bowline_local::metadata::SyncCancellationOutcome::Requested)
    );
    assert_eq!(
        store
            .complete_claimed_sync_operation(
                &claimed.claim,
                r#"{"outcome":"advanced"}"#,
                "2026-07-13T12:00:02Z",
            )
            .expect("generic completion checked"),
        SyncClaimTransition::OwnershipLost
    );
    assert_eq!(
        store
            .complete_committed_cancelled_late_sync_operation(
                &claimed.claim,
                &SyncCommittedCancelledLateResult::new(
                    SyncOperationKind::Reconcile,
                    serde_json::json!({"outcome": "advanced"}),
                ),
                "2026-07-13T12:00:03Z",
            )
            .expect("typed late completion"),
        SyncClaimTransition::Applied
    );
    let operation = store
        .sync_operation_by_id("op-cancel-fence")
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(operation.state, SyncOperationState::Completed);
    assert!(
        operation
            .result_json
            .as_deref()
            .is_some_and(|result| result.contains("committed-cancelled-late"))
    );
    let _ = fs::remove_dir_all(temp);
}

fn workspace_ref(workspace_id: &str, version: u64, snapshot_id: &str) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id),
        version,
        snapshot_id: bowline_core::ids::SnapshotId::new(snapshot_id),
        updated_at: ControlPlaneTimestamp { tick: version },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
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
