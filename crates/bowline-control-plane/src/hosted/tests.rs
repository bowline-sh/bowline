use super::*;

use super::*;

#[test]
fn generated_object_keys_preserve_shape_and_change_with_seed() {
    let first = generated_object_key(ObjectKind::SourcePack, "workspace:device:1");
    let second = generated_object_key(ObjectKind::SourcePack, "workspace:device:2");
    let manifest = generated_object_key(ObjectKind::SnapshotManifest, "workspace:device:1");
    let overlay = generated_object_key(ObjectKind::AgentOverlay, "workspace:device:1");

    assert_ne!(first, second);
    assert!(first.starts_with("packs_pk_"));
    assert!(manifest.starts_with("manifests_mf_"));
    assert!(overlay.starts_with("packs_pk_"));
    assert!(StorageObjectKey::new(first).is_ok());
    assert!(StorageObjectKey::new(second).is_ok());
    assert!(StorageObjectKey::new(manifest).is_ok());
    assert!(StorageObjectKey::new(overlay).is_ok());
}

#[test]
fn status_snapshot_value_builders_use_convex_arg_names() {
    let watermarks = StatusEventWatermarks {
        last_event_id: Some("evt_42".to_string()),
        last_scan_at: Some("2026-06-29T12:00:00Z".to_string()),
        sync_state: Some("ready".to_string()),
        watcher_state: Some("degraded".to_string()),
        network_state: None,
    };
    let Value::Object(watermark_object) = status_event_watermarks_value(&watermarks) else {
        panic!("watermarks must serialize to a Convex object");
    };
    assert_eq!(
        watermark_object.get("lastEventId"),
        Some(&Value::from("evt_42"))
    );
    assert_eq!(
        watermark_object.get("syncState"),
        Some(&Value::from("ready"))
    );
    assert_eq!(
        watermark_object.get("watcherState"),
        Some(&Value::from("degraded"))
    );
    // Absent optional fields must be omitted entirely (Convex v.optional).
    assert!(!watermark_object.contains_key("networkState"));

    let queue = StatusSyncQueueSnapshot {
        queued: 1,
        claimed: 2,
        waiting_retry: 3,
        blocked_offline: 4,
        attention: 5,
        completed: 6,
    };
    let Value::Object(queue_object) = status_sync_queue_value(&queue) else {
        panic!("sync queue must serialize to a Convex object");
    };
    assert_eq!(queue_object.get("queued"), Some(&number_value(1)));
    assert_eq!(queue_object.get("waitingRetry"), Some(&number_value(3)));
    assert_eq!(queue_object.get("blockedOffline"), Some(&number_value(4)));

    let limit = StatusLimitSnapshot {
        capability: "search".to_string(),
        unavailable_because: "index degraded".to_string(),
        path: None,
        still_works: vec!["status".to_string()],
    };
    let Value::Object(limit_object) = status_limit_value(&limit) else {
        panic!("limit must serialize to a Convex object");
    };
    assert_eq!(
        limit_object.get("unavailableBecause"),
        Some(&Value::from("index degraded"))
    );
    assert!(!limit_object.contains_key("path"));
}

#[test]
fn hosted_parser_accepts_phase_10_lease_event_kinds() {
    for kind in [
        CompactEventKind::LeaseBlocked,
        CompactEventKind::LeaseCleanupCompleted,
        CompactEventKind::LeaseCompleted,
        CompactEventKind::LeaseCreated,
        CompactEventKind::LeaseExpired,
        CompactEventKind::LeaseHydrationRequested,
        CompactEventKind::LeaseRevoked,
        CompactEventKind::LeaseReviewReady,
        CompactEventKind::LeaseToolDenied,
        CompactEventKind::LeaseToolInvoked,
        CompactEventKind::LeaseUpdated,
        CompactEventKind::OverlayChanged,
        CompactEventKind::PublishRequested,
    ] {
        assert_eq!(parse_event_kind(kind.as_str()).expect("event kind"), kind);
    }
}

#[test]
fn bootstrap_session_proof_subject_binds_bootstrap_token_hash() {
    let input = BootstrapSessionInput {
        workspace_id: "workspace_1".to_string(),
        host: Some("mac-mini".to_string()),
        root: Some("/workspace/Code".to_string()),
        expires_in_ticks: 900,
    };

    assert_eq!(
        bootstrap_session_proof_subject(&input, "sha256:token_hash_1"),
        [
            "workspaceId=workspace_1",
            "host=mac-mini",
            "root=/workspace/Code",
            "expiresInTicks=900",
            "bootstrapTokenHash=sha256:token_hash_1",
        ]
        .join("\n")
    );
}

#[test]
fn hosted_lease_parser_preserves_returned_timestamps() {
    let lease = parse_lease(&Value::Object(args([
        ("baseSnapshotId", Value::from("snap_1")),
        ("createdAt", Value::from("2026-06-25T12:00:00Z")),
        ("deviceId", Value::from("device_1")),
        ("executionState", Value::from("active")),
        ("expiresAt", Value::from("t000000003600")),
        ("leaseId", Value::from("lease_1")),
        ("outputState", Value::from("empty")),
        ("projectId", Value::from("project_1")),
        ("statusCode", Value::from("active")),
        ("updatedAt", Value::from("2026-06-25T12:00:01Z")),
        ("version", number_value(2)),
        ("writeTargetMode", Value::from("work-view")),
        ("workViewId", Value::from("work_1")),
        ("workspaceId", Value::from("workspace_1")),
    ])))
    .expect("lease parses");

    assert_eq!(lease.created_at.tick, 1_782_388_800_000);
    assert_eq!(lease.updated_at.tick, 1_782_388_801_000);
    assert_eq!(lease.expires_at.tick, 3_600);
}

#[test]
fn account_session_cache_reuses_unexpired_session() {
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client");
    client.account_session_cache.lock().expect("cache").insert(
        account_session_cache_key(Some("workspace_1")),
        CachedAccountSession {
            session_id: "session_cached".to_string(),
            expires_at_unix: OffsetDateTime::now_utc().unix_timestamp() + 600,
        },
    );

    assert_eq!(
        client.cached_account_session_id(&account_session_cache_key(Some("workspace_1"))),
        Some("session_cached".to_string())
    );
}

#[test]
fn account_session_cache_ignores_expired_session() {
    let client = HostedControlPlaneClient::try_new_with_token(
        "https://example.convex.cloud",
        "test-control-plane-token",
    )
    .expect("client");
    client.account_session_cache.lock().expect("cache").insert(
        account_session_cache_key(Some("workspace_1")),
        CachedAccountSession {
            session_id: "session_expired".to_string(),
            expires_at_unix: OffsetDateTime::now_utc().unix_timestamp() + 10,
        },
    );

    assert_eq!(
        client.cached_account_session_id(&account_session_cache_key(Some("workspace_1"))),
        None
    );
}

#[test]
fn hosted_function_call_counts_are_process_local_and_low_cardinality() {
    reset_hosted_function_call_counts();

    record_hosted_function_call("refs:getWorkspaceRef");
    record_hosted_function_call("refs:getWorkspaceRef");
    record_hosted_function_call("objects:createDownloadIntent");

    assert_eq!(
        hosted_function_call_counts(),
        vec![
            HostedFunctionCallCount {
                function_name: "objects:createDownloadIntent".to_string(),
                call_count: 1,
            },
            HostedFunctionCallCount {
                function_name: "refs:getWorkspaceRef".to_string(),
                call_count: 2,
            },
        ]
    );
}
