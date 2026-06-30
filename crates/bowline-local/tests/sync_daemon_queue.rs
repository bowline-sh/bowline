use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::metadata::{
    MetadataStore, RemoteRefCursorRecord, SyncOperationRecord, WorkspaceSyncHeadRecord,
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
            "upload",
            "queued",
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
        .claim_next_sync_operation(&workspace_id, "daemon-a", "2026-06-26T12:00:01Z")
        .expect("claim succeeds")
        .expect("operation claimed");
    assert_eq!(claimed.id, "op-upload-1");
    assert_eq!(claimed.state, "claimed");
    assert_eq!(claimed.claimed_by.as_deref(), Some("daemon-a"));
    assert_eq!(claimed.attempt_count, 1);
    assert!(
        reopened
            .claim_next_sync_operation(&workspace_id, "daemon-b", "2026-06-26T12:00:01Z")
            .expect("second claim query succeeds")
            .is_none()
    );

    assert!(
        reopened
            .refresh_sync_operation_heartbeat("op-upload-1", "daemon-a", "2026-06-26T12:00:02Z",)
            .expect("heartbeat refreshes")
    );
    assert_eq!(
        reopened
            .requeue_expired_sync_claims(
                &workspace_id,
                "2026-06-26T12:00:03Z",
                "2026-06-26T12:00:04Z",
            )
            .expect("expired claim requeues"),
        1
    );
    let reclaimed = reopened
        .claim_next_sync_operation(&workspace_id, "daemon-b", "2026-06-26T12:00:05Z")
        .expect("reclaim succeeds")
        .expect("operation reclaimed");
    assert_eq!(reclaimed.claimed_by.as_deref(), Some("daemon-b"));
    assert_eq!(reclaimed.attempt_count, 2);

    reopened
        .fail_sync_operation_for_retry(
            "op-upload-1",
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
            .claim_next_sync_operation(&workspace_id, "daemon-c", "2026-06-26T12:00:29Z")
            .expect("early retry claim query succeeds")
            .is_none()
    );
    let retry_claim = reopened
        .claim_next_sync_operation(&workspace_id, "daemon-c", "2026-06-26T12:00:30Z")
        .expect("due retry claims")
        .expect("retry operation claimed");
    assert_eq!(retry_claim.attempt_count, 3);

    reopened
        .block_sync_operation_offline(
            "op-upload-1",
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
        .active_sync_operation_for_device(&workspace_id, "upload", &DeviceId::new("device-a"))
        .expect("active operation lookup")
        .expect("offline operation is active");
    assert_eq!(active.id, "op-upload-1");

    assert!(
        reopened
            .claim_next_sync_operation(&workspace_id, "daemon-d", "2026-06-26T12:00:59Z")
            .expect("early offline claim query succeeds")
            .is_none()
    );
    let offline_claim = reopened
        .claim_next_sync_operation(&workspace_id, "daemon-d", "2026-06-26T12:01:00Z")
        .expect("offline claim query succeeds")
        .expect("offline operation reclaims when scheduler wakes");
    assert_eq!(offline_claim.attempt_count, 4);

    reopened
        .mark_sync_operation_attention(
            "op-upload-1",
            "trusted device required",
            "2026-06-26T12:00:33Z",
        )
        .expect("attention state recorded");
    let attention_counts = reopened
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(attention_counts.attention, 1);

    reopened
        .complete_sync_operation(
            "op-upload-1",
            r#"{"completed":true}"#,
            "2026-06-26T12:00:34Z",
        )
        .expect("completion recorded");
    let completed_counts = reopened
        .sync_operation_counts(&workspace_id)
        .expect("counts read");
    assert_eq!(completed_counts.completed, 1);
    assert!(
        reopened
            .active_sync_operation_for_device(&workspace_id, "upload", &DeviceId::new("device-a"),)
            .expect("active operation lookup")
            .is_none()
    );

    let _ = fs::remove_dir_all(&temp);
}

fn operation(
    id: &str,
    kind: &str,
    state: &str,
    idempotency_key: &str,
    workspace_id: &WorkspaceId,
) -> SyncOperationRecord {
    SyncOperationRecord {
        id: id.to_string(),
        workspace_id: workspace_id.clone(),
        kind: kind.to_string(),
        state: state.to_string(),
        idempotency_key: idempotency_key.to_string(),
        base_version: Some(7),
        base_snapshot_id: Some("snap-7".to_string()),
        target_snapshot_id: Some("snap-8".to_string()),
        device_id: Some(DeviceId::new("device-a")),
        payload_json: r#"{"candidate":"snap-8"}"#.to_string(),
        attempt_count: 0,
        claimed_by: None,
        heartbeat_at: None,
        next_attempt_at: None,
        last_error: None,
        created_at: "2026-06-26T12:00:00Z".to_string(),
        updated_at: "2026-06-26T12:00:00Z".to_string(),
    }
}

fn workspace_ref(workspace_id: &str, version: u64, snapshot_id: &str) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: workspace_id.to_string(),
        version,
        snapshot_id: snapshot_id.to_string(),
        updated_at: ControlPlaneTimestamp { tick: version },
        updated_by_device_id: Some("device-a".to_string()),
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
