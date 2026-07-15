use super::*;
use crate::metadata::{
    SyncOperationRecord, SyncResourceKey, WorkViewAcceptCancellationOutcome,
    WorkViewAcceptCandidateObservation,
};

const NOW: &str = "2026-07-13T12:00:00Z";
const LEASE: &str = "2026-07-13T12:05:00Z";

#[test]
fn enqueue_is_terminally_idempotent_and_suppresses_parallel_active_accepts() {
    let (_temp, store, view) = seeded_accept_store("accept-enqueue");
    let first = operation(&view, "accept-1", "key-1", NOW);
    assert!(
        store
            .enqueue_work_view_accept(&first)
            .expect("enqueue")
            .inserted()
    );
    assert_eq!(
        store.enqueue_work_view_accept(&first).expect("repeat"),
        WorkViewAcceptEnqueueOutcome::Existing(first.clone())
    );

    let competing = operation(&view, "accept-2", "key-2", "2026-07-13T12:00:01Z");
    assert_eq!(
        store
            .enqueue_work_view_accept(&competing)
            .expect("active dedupe"),
        WorkViewAcceptEnqueueOutcome::Existing(first)
    );
}

#[test]
fn enqueue_rejects_a_distinct_request_while_accept_is_active() {
    let (_temp, store, view) = seeded_accept_store("accept-distinct-active");
    let first = operation(&view, "accept-1", "key-1", NOW);
    store.enqueue_work_view_accept(&first).expect("enqueue");
    let mut competing = operation(&view, "accept-2", "key-2", "2026-07-13T12:00:01Z");
    competing.selected_paths = Some(vec!["apps/web/other.rs".to_string()]);

    let error = store
        .enqueue_work_view_accept(&competing)
        .expect_err("distinct active input must not alias");

    assert!(
        matches!(error, MetadataError::InvalidStorageMetadata(message)
        if message.contains("already active with different input"))
    );
    assert_eq!(
        store
            .active_work_view_accept(&view.workspace_id, &view.id)
            .expect("active"),
        Some(first)
    );
}

#[test]
fn cancellation_before_claim_is_terminal_without_execution() {
    let (_temp, store, view) = seeded_accept_store("accept-cancel-before-claim");
    let operation = operation(&view, "accept-cancelled", "key-cancelled", NOW);
    store.enqueue_work_view_accept(&operation).expect("enqueue");

    assert_eq!(
        store
            .request_work_view_accept_cancellation(&operation.id, "2026-07-13T12:00:01Z")
            .expect("cancel"),
        Some(WorkViewAcceptCancellationOutcome::Cancelled)
    );
    assert!(
        store
            .claim_next_work_view_accept(
                &view.workspace_id,
                &DeviceId::new("device"),
                "daemon-a",
                "2026-07-13T12:00:02Z",
                LEASE,
            )
            .expect("claim check")
            .is_none()
    );
    let cancelled = store
        .work_view_accept_operation(&operation.id)
        .expect("read")
        .expect("operation");
    assert_eq!(cancelled.state, WorkViewAcceptOperationState::Cancelled);
    assert_eq!(
        cancelled.result_json.as_deref(),
        Some(r#"{"outcome":"cancelled"}"#)
    );
}

#[test]
fn claimed_cancellation_stops_at_checkpoint_but_late_completion_stays_completed() {
    let (_temp, store, view) = seeded_accept_store("accept-cancel-checkpoint");
    let operation = operation(&view, "accept-cancelled", "key-cancelled", NOW);
    store.enqueue_work_view_accept(&operation).expect("enqueue");
    let claimed = store
        .claim_next_work_view_accept(
            &view.workspace_id,
            &DeviceId::new("device"),
            "daemon-a",
            NOW,
            LEASE,
        )
        .expect("claim")
        .expect("claimed");

    assert_eq!(
        store
            .request_claimed_work_view_accept_cancellations(
                &view.workspace_id,
                &DeviceId::new("device"),
                "daemon-a",
                "2026-07-13T12:00:01Z",
            )
            .expect("cancel active"),
        1
    );
    assert_eq!(
        store
            .check_work_view_accept_claim(&claimed.claim, "2026-07-13T12:00:01Z")
            .expect("checkpoint"),
        WorkViewAcceptClaimCheck::CancellationRequested
    );

    let completed_after_cancel = r#"{"outcome":"completed-after-cancel"}"#;
    assert_eq!(
        store
            .complete_work_view_accept(
                &claimed.claim,
                &SnapshotId::new("target"),
                completed_after_cancel,
                "2026-07-13T12:00:02Z",
            )
            .expect("irreversible completion"),
        WorkViewAcceptClaimTransition::Applied
    );
    let completed = store
        .work_view_accept_operation(&operation.id)
        .expect("read")
        .expect("operation");
    assert_eq!(completed.state, WorkViewAcceptOperationState::Completed);
    assert!(completed.cancellation_requested_at.is_some());
    assert_eq!(
        completed.result_json.as_deref(),
        Some(completed_after_cancel)
    );
}

#[test]
fn claim_checkpoint_and_completion_are_fenced() {
    let (_temp, store, view) = seeded_accept_store("accept-fenced");
    let operation = operation(&view, "accept-1", "key-1", NOW);
    store.enqueue_work_view_accept(&operation).expect("enqueue");
    let claimed = store
        .claim_next_work_view_accept(
            &view.workspace_id,
            &DeviceId::new("device"),
            "daemon-a",
            NOW,
            LEASE,
        )
        .expect("claim")
        .expect("ready operation");
    assert_eq!(claimed.operation.attempt_count, 1);
    assert_eq!(claimed.claim.generation(), 1);
    assert!(
        store
            .claim_next_work_view_accept(
                &view.workspace_id,
                &DeviceId::new("device"),
                "daemon-b",
                NOW,
                LEASE,
            )
            .expect("concurrent claim check")
            .is_none()
    );
    assert_eq!(
        claimed.operation.resource_key.as_string(),
        "work_view_accept:workspace:project:view"
    );
    assert_eq!(
        store
            .check_work_view_accept_claim(&claimed.claim, NOW)
            .expect("owned"),
        WorkViewAcceptClaimCheck::Owned
    );

    let candidate_checkpoint = WorkViewAcceptCheckpointRecord {
        id: "checkpoint-candidate".to_string(),
        workspace_id: view.workspace_id.clone(),
        operation_id: operation.id.clone(),
        claim_generation: claimed.claim.generation(),
        step: WorkViewAcceptCheckpointStep::CandidateBuilt,
        payload_json: r#"{"selectedPathCount":1}"#.to_string(),
        created_at: "2026-07-13T12:00:00Z".to_string(),
    };
    let observation = WorkViewAcceptCandidateObservation {
        observed_main_snapshot_id: SnapshotId::new("main-observed"),
        observed_ref_version: 8,
        observed_ref_snapshot_id: SnapshotId::new("remote-observed"),
        target_snapshot_id: SnapshotId::new("target"),
    };
    assert_eq!(
        store
            .record_work_view_accept_candidate(
                &claimed.claim,
                &candidate_checkpoint,
                &observation,
                "2026-07-13T12:00:00Z",
            )
            .expect("candidate checkpoint"),
        WorkViewAcceptClaimTransition::Applied
    );
    assert_eq!(
        store
            .record_work_view_accept_candidate(
                &claimed.claim,
                &candidate_checkpoint,
                &observation,
                "2026-07-13T12:00:00Z",
            )
            .expect("duplicate checkpoint retry"),
        WorkViewAcceptClaimTransition::Applied
    );

    let fence_checkpoint = WorkViewAcceptCheckpointRecord {
        id: "checkpoint-fence".to_string(),
        workspace_id: view.workspace_id.clone(),
        operation_id: operation.id.clone(),
        claim_generation: claimed.claim.generation(),
        step: WorkViewAcceptCheckpointStep::MainFenceRechecked,
        payload_json: r#"{"mainSnapshotId":"main-observed"}"#.to_string(),
        created_at: "2026-07-13T12:00:00.500Z".to_string(),
    };
    assert_eq!(
        store
            .append_work_view_accept_checkpoint(
                &claimed.claim,
                &fence_checkpoint,
                "2026-07-13T12:00:00.500Z",
            )
            .expect("main fence checkpoint"),
        WorkViewAcceptClaimTransition::Applied
    );

    let checkpoint = WorkViewAcceptCheckpointRecord {
        id: "checkpoint-1".to_string(),
        workspace_id: view.workspace_id.clone(),
        operation_id: operation.id.clone(),
        claim_generation: claimed.claim.generation(),
        step: WorkViewAcceptCheckpointStep::ObjectsUploaded,
        payload_json: r#"{"objectCount":2}"#.to_string(),
        created_at: "2026-07-13T12:00:01Z".to_string(),
    };
    let target = SnapshotId::new("target");
    assert_eq!(
        store
            .mark_work_view_accept_uploaded_or_staged(
                &claimed.claim,
                &checkpoint,
                &target,
                "2026-07-13T12:00:01Z",
            )
            .expect("checkpoint"),
        WorkViewAcceptClaimTransition::Applied
    );
    let main_published = WorkViewAcceptCheckpointRecord {
        id: "checkpoint-main-published".to_string(),
        workspace_id: view.workspace_id.clone(),
        operation_id: operation.id.clone(),
        claim_generation: claimed.claim.generation(),
        step: WorkViewAcceptCheckpointStep::MainPublished,
        payload_json: r#"{"snapshotId":"target"}"#.to_string(),
        created_at: "2026-07-13T12:00:01.500Z".to_string(),
    };
    assert_eq!(
        store
            .append_work_view_accept_checkpoint(
                &claimed.claim,
                &main_published,
                "2026-07-13T12:00:01.500Z",
            )
            .expect("main publication checkpoint"),
        WorkViewAcceptClaimTransition::Applied
    );
    assert_eq!(
        store
            .work_view_accept_checkpoints(&operation.id)
            .expect("checkpoints"),
        vec![
            candidate_checkpoint,
            fence_checkpoint,
            checkpoint,
            main_published,
        ]
    );
    assert_eq!(
        store
            .complete_work_view_accept(
                &claimed.claim,
                &target,
                r#"{"outcome":"accepted"}"#,
                "2026-07-13T12:00:02Z",
            )
            .expect("complete"),
        WorkViewAcceptClaimTransition::Applied
    );
    assert_eq!(
        store
            .complete_work_view_accept(
                &claimed.claim,
                &target,
                r#"{"outcome":"accepted"}"#,
                "2026-07-13T12:00:03Z",
            )
            .expect("stale completion"),
        WorkViewAcceptClaimTransition::OwnershipLost
    );
    let completed = store
        .work_view_accept_operation(&operation.id)
        .expect("read")
        .expect("operation");
    assert_eq!(completed.state, WorkViewAcceptOperationState::Completed);
    assert_eq!(completed.target_snapshot_id, Some(target));
    assert_eq!(
        store
            .enqueue_work_view_accept(&operation)
            .expect("terminal retry"),
        WorkViewAcceptEnqueueOutcome::Existing(completed)
    );
}

#[test]
fn accept_claim_waits_for_claimed_workspace_reconcile() {
    let (_temp, store, view) = seeded_accept_store("accept-waits-for-reconcile");
    let accept = operation(&view, "accept-1", "accept-key-1", NOW);
    store
        .enqueue_work_view_accept(&accept)
        .expect("enqueue accept");
    store
        .enqueue_sync_operation(&reconcile_operation(&view.workspace_id, "reconcile-1"))
        .expect("enqueue reconcile");

    let reconcile = store
        .claim_next_sync_operation(&view.workspace_id, "daemon-sync", NOW, LEASE)
        .expect("claim reconcile")
        .expect("reconcile ready");
    assert!(
        store
            .claim_next_work_view_accept(
                &view.workspace_id,
                &DeviceId::new("device"),
                "daemon-accept",
                NOW,
                LEASE,
            )
            .expect("check accept claim")
            .is_none()
    );

    assert_eq!(
        store
            .requeue_claimed_sync_operation_after_dispatch_failure(
                &reconcile.claim,
                "dispatch-unavailable",
                "release reconcile claim for accept",
                "2026-07-13T12:00:01Z",
            )
            .expect("release reconcile"),
        SyncClaimTransition::Applied
    );
    assert!(
        store
            .claim_next_work_view_accept(
                &view.workspace_id,
                &DeviceId::new("device"),
                "daemon-accept",
                "2026-07-13T12:00:02Z",
                LEASE,
            )
            .expect("claim accept after reconcile release")
            .is_some()
    );
}

#[test]
fn reconcile_claim_waits_for_claimed_work_view_accept() {
    let (_temp, store, view) = seeded_accept_store("reconcile-waits-for-accept");
    let accept = operation(&view, "accept-1", "accept-key-1", NOW);
    store
        .enqueue_work_view_accept(&accept)
        .expect("enqueue accept");
    store
        .enqueue_sync_operation(&reconcile_operation(&view.workspace_id, "reconcile-1"))
        .expect("enqueue reconcile");

    let accept = store
        .claim_next_work_view_accept(
            &view.workspace_id,
            &DeviceId::new("device"),
            "daemon-accept",
            NOW,
            LEASE,
        )
        .expect("claim accept")
        .expect("accept ready");
    assert!(
        store
            .claim_next_sync_operation(&view.workspace_id, "daemon-sync", NOW, LEASE)
            .expect("check reconcile claim")
            .is_none()
    );

    assert_eq!(
        store
            .requeue_claimed_work_view_accept_after_dispatch_failure(
                &accept.claim,
                "release accept claim for reconcile",
                "2026-07-13T12:00:01Z",
            )
            .expect("release accept"),
        WorkViewAcceptClaimTransition::Applied
    );
    assert!(
        store
            .claim_next_sync_operation(
                &view.workspace_id,
                "daemon-sync",
                "2026-07-13T12:00:02Z",
                LEASE,
            )
            .expect("claim reconcile after accept release")
            .is_some()
    );
}

#[test]
fn expired_claim_requeues_and_fences_the_old_worker() {
    let (_temp, store, view) = seeded_accept_store("accept-expiry");
    let operation = operation(&view, "accept-1", "key-1", NOW);
    store.enqueue_work_view_accept(&operation).expect("enqueue");
    let old = store
        .claim_next_work_view_accept(
            &view.workspace_id,
            &DeviceId::new("device"),
            "daemon-a",
            NOW,
            "2026-07-13T12:00:01Z",
        )
        .expect("claim")
        .expect("ready");
    assert_eq!(
        store
            .requeue_expired_work_view_accepts("2026-07-13T12:00:02Z")
            .expect("requeue"),
        1
    );
    let new = store
        .claim_next_work_view_accept(
            &view.workspace_id,
            &DeviceId::new("device"),
            "daemon-b",
            "2026-07-13T12:00:03Z",
            LEASE,
        )
        .expect("reclaim")
        .expect("ready");
    assert_eq!(new.claim.generation(), 2);
    let mut review_ready = view.clone();
    review_ready.lifecycle = WorkViewLifecycle::ReviewReady;
    review_ready.updated_at = "2026-07-13T12:00:04Z".to_string();
    assert_eq!(
        store
            .upsert_work_view_under_accept_claim(&review_ready, &old.claim, "2026-07-13T12:00:04Z",)
            .expect("stale lifecycle fence"),
        WorkViewAcceptClaimTransition::OwnershipLost
    );
    assert_eq!(
        store
            .work_view_by_id(&view.workspace_id, &view.id)
            .expect("work view")
            .expect("record")
            .lifecycle,
        WorkViewLifecycle::Active
    );
    assert_eq!(
        store
            .upsert_work_view_under_accept_claim(&review_ready, &new.claim, "2026-07-13T12:00:04Z",)
            .expect("owned lifecycle fence"),
        WorkViewAcceptClaimTransition::Applied
    );
    assert_eq!(
        store
            .mark_work_view_accept_review(
                &old.claim,
                WorkViewAcceptReviewReason::MergeConflict,
                r#"{"paths":["apps/web/main.rs"]}"#,
                "2026-07-13T12:00:04Z",
            )
            .expect("old fence"),
        WorkViewAcceptClaimTransition::OwnershipLost
    );
    assert_eq!(
        store
            .retry_work_view_accept(
                &new.claim,
                "hosted ref advanced",
                "2026-07-13T12:01:00Z",
                "2026-07-13T12:00:04Z",
            )
            .expect("retry"),
        WorkViewAcceptClaimTransition::Applied
    );
    let waiting = store
        .work_view_accept_operation(&operation.id)
        .expect("read")
        .expect("operation");
    assert_eq!(waiting.state, WorkViewAcceptOperationState::WaitingRetry);
    assert_eq!(
        waiting.failure_reason,
        Some(WorkViewAcceptFailureReason::Transient)
    );
}

#[test]
fn dispatch_failure_requeues_accept_and_worker_failure_keeps_the_claim_durable() {
    let (_temp, store, view) = seeded_accept_store("accept-dispatch-requeue");
    let operation = operation(&view, "accept-1", "key-1", NOW);
    store.enqueue_work_view_accept(&operation).expect("enqueue");
    let first = store
        .claim_next_work_view_accept(
            &view.workspace_id,
            &DeviceId::new("device"),
            "daemon-a",
            NOW,
            LEASE,
        )
        .expect("claim")
        .expect("ready operation");
    assert_eq!(
        store
            .requeue_claimed_work_view_accept_after_dispatch_failure(
                &first.claim,
                "sync lane disconnected before execution",
                "2026-07-13T12:00:01Z",
            )
            .expect("dispatch requeue"),
        WorkViewAcceptClaimTransition::Applied
    );
    assert_eq!(
        store
            .requeue_claimed_work_view_accept_after_dispatch_failure(
                &first.claim,
                "stale dispatch requeue",
                "2026-07-13T12:00:02Z",
            )
            .expect("stale dispatch requeue"),
        WorkViewAcceptClaimTransition::OwnershipLost
    );
    let replacement = store
        .claim_next_work_view_accept(
            &view.workspace_id,
            &DeviceId::new("device"),
            "daemon-b",
            "2026-07-13T12:00:03Z",
            LEASE,
        )
        .expect("replacement claim")
        .expect("replacement ready operation");
    assert_eq!(replacement.claim.generation(), 2);
    assert_eq!(
        store
            .record_claimed_work_view_accept_worker_failure(
                &replacement.claim,
                "work-view accept worker terminated unexpectedly",
                "2026-07-13T12:00:04Z",
            )
            .expect("worker failure recorded"),
        WorkViewAcceptClaimTransition::Applied
    );
    let claimed = store
        .work_view_accept_operation(&operation.id)
        .expect("operation reads")
        .expect("operation exists");
    assert_eq!(claimed.state, WorkViewAcceptOperationState::Claimed);
    assert_eq!(
        claimed.last_error.as_deref(),
        Some("work-view accept worker terminated unexpectedly")
    );
}

#[test]
fn version_26_is_refused_without_migration() {
    let (temp, store, _view) = seeded_accept_store("accept-schema-migration");
    let path = temp.root().join("metadata.sqlite3");
    drop(store);
    let connection = Connection::open(&path).expect("database");
    connection
        .execute_batch(
            "DROP TABLE work_view_accept_checkpoints;
             DROP TABLE work_view_accept_operations;
             PRAGMA user_version = 26;",
        )
        .expect("simulate version 26");
    drop(connection);
    let error = MetadataStore::open(&path).expect_err("schema 26 must be refused");
    assert!(matches!(error, MetadataError::UnsupportedSchema));
    let connection = Connection::open(&path).expect("database remains readable");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .expect("schema version"),
        26
    );
}

fn seeded_accept_store(name: &str) -> (TempWorkspace, MetadataStore, WorkView) {
    let (temp, store, view) = super::work_views::seeded_store(name);
    store.upsert_work_view(&view).expect("work view");
    (temp, store, view)
}

fn operation(
    view: &WorkView,
    id: &str,
    idempotency_key: &str,
    created_at: &str,
) -> WorkViewAcceptOperationRecord {
    WorkViewAcceptOperationRecord {
        id: id.to_string(),
        workspace_id: view.workspace_id.clone(),
        project_id: view.project_id.clone(),
        work_view_id: view.id.clone(),
        device_id: DeviceId::new("device"),
        resource_key: WorkViewAcceptResourceKey::new(
            view.workspace_id.clone(),
            view.project_id.clone(),
            view.id.clone(),
        ),
        idempotency_key: idempotency_key.to_string(),
        state: WorkViewAcceptOperationState::Queued,
        selected_paths: Some(vec!["apps/web/main.rs".to_string()]),
        input_json: r#"{"baseSnapshotId":"snapshot","exposedManifestId":"manifest"}"#.to_string(),
        observed_main_snapshot_id: None,
        observed_ref_version: None,
        observed_ref_snapshot_id: None,
        target_snapshot_id: None,
        result_json: None,
        review_reason: None,
        failure_reason: None,
        cancellation_requested_at: None,
        last_error: None,
        claimed_by: None,
        claim_token: None,
        claim_generation: 0,
        heartbeat_at: None,
        lease_expires_at: None,
        attempt_count: 0,
        next_attempt_at: None,
        created_at: created_at.to_string(),
        updated_at: created_at.to_string(),
    }
}

fn reconcile_operation(workspace_id: &WorkspaceId, id: &str) -> SyncOperationRecord {
    SyncOperationRecord {
        id: id.to_string(),
        workspace_id: workspace_id.clone(),
        kind: SyncOperationKind::Reconcile,
        resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
        state: SyncOperationState::Queued,
        idempotency_key: format!("{id}-key"),
        base_version: None,
        base_snapshot_id: None,
        target_snapshot_id: None,
        device_id: Some(DeviceId::new("device")),
        payload_json: r#"{"scanScope":{"kind":"full","reason":"explicit"}}"#.to_string(),
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
        created_at: NOW.to_string(),
        updated_at: NOW.to_string(),
    }
}
