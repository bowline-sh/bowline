use super::*;

#[test]
fn pending_sync_operations_are_visible_in_status() {
    let temp = TempWorkspace::new("status-sync-operations").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .enqueue_sync_operation(&sync_operation_record(
            "op_queued",
            &workspace_id,
            "queued",
            "queued-key",
        ))
        .expect("queued operation");
    store
        .enqueue_sync_operation(&sync_operation_record(
            "op_retry",
            &workspace_id,
            "waiting_retry",
            "retry-key",
        ))
        .expect("retry operation");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Limited);
    let sync_queue = output.sync_queue.expect("sync queue is reported");
    assert_eq!(sync_queue.queued, 1);
    assert_eq!(sync_queue.waiting_retry, 1);
    assert!(
        output
            .status
            .attention_items
            .contains(&"Sync queue is waiting for retry.".to_string())
    );
    assert!(output.items.iter().any(|item| {
        item.kind == StatusItemKind::Materialization
            && item.summary.contains("1 queued")
            && item.summary.contains("1 waiting retry")
    }));
}

#[test]
fn offline_sync_operations_report_recovery_wait_in_status() {
    let temp = TempWorkspace::new("status-sync-offline").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .enqueue_sync_operation(&sync_operation_record(
            "op_offline",
            &workspace_id,
            "blocked_offline",
            "offline-key",
        ))
        .expect("offline operation");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Limited);
    let sync_queue = output.sync_queue.expect("sync queue is reported");
    assert_eq!(sync_queue.blocked_offline, 1);
    assert!(
        output
            .status
            .attention_items
            .contains(&"Sync queue is waiting for offline recovery.".to_string())
    );
    assert!(output.limits.iter().any(|limit| {
        limit.capability == "sync"
            && limit.unavailable_because == "sync queue is waiting for offline recovery"
    }));
}

#[test]
fn attention_sync_operations_report_attention_in_status() {
    let temp = TempWorkspace::new("status-sync-attention").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .enqueue_sync_operation(&sync_operation_record(
            "op_attention",
            &workspace_id,
            "attention",
            "attention-key",
        ))
        .expect("attention operation");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    let sync_queue = output.sync_queue.expect("sync queue is reported");
    assert_eq!(sync_queue.attention, 1);
    assert!(
        output
            .status
            .attention_items
            .contains(&"Sync queue needs attention.".to_string())
    );
    assert!(output.limits.iter().any(|limit| {
        limit.capability == "sync" && limit.unavailable_because == "sync queue needs attention"
    }));
}

#[test]
fn status_scopes_sync_queue_to_recent_daemon_device() {
    let temp = TempWorkspace::new("status-sync-current-device").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);

    let mut stale_other_device = sync_operation_record(
        "op_other_attention",
        &workspace_id,
        "attention",
        "other-key",
    );
    stale_other_device.device_id = Some(DeviceId::new("device_other"));
    store
        .enqueue_sync_operation(&stale_other_device)
        .expect("other device attention operation");

    let mut current_device_completed =
        sync_operation_record("op_current_done", &workspace_id, "completed", "current-key");
    current_device_completed.device_id = Some(DeviceId::new("device_current"));
    store
        .enqueue_sync_operation(&current_device_completed)
        .expect("current device completed operation");

    let mut event = WorkspaceEvent::new(
        EventId::new("evt_current_sync"),
        EventName::SyncCompleted,
        "2026-06-23T12:00:01Z",
        EventSeverity::Info,
        "Current device sync completed.",
        workspace_id.clone(),
    );
    event.device_id = Some(DeviceId::new("device_current"));
    store.append_event(event).expect("sync event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .all(|item| item != "Sync queue needs attention.")
    );
    assert_eq!(output.sync_queue, None);
}

#[test]
fn status_reports_state_root_unresolved_conflict_bundles() {
    let temp = TempWorkspace::new("status-state-root-conflict").expect("temp workspace");
    let state_root = temp.root().join("state");
    let db_path = state_root.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    create_conflict_bundle(
        &state_root,
        ConflictRecord::same_path("apps/web/src/index.ts"),
        &[ConflictFile {
            relative_path: "apps/web/src/index.ts".to_string(),
            base: Some(b"base\n".to_vec()),
            local: Some(b"local\n".to_vec()),
            remote: Some(b"remote\n".to_vec()),
        }],
    )
    .expect("conflict bundle created");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(
        output.status.attention_items.iter().any(|item| {
            item == "1 unresolved conflict needs attention: apps/web/src/index.ts."
        })
    );
    assert!(output.items.iter().any(|item| {
        item.kind == StatusItemKind::Conflict
            && item.path.as_deref() == Some("apps/web/src/index.ts")
    }));
    assert!(output.limits.iter().any(|limit| {
        limit.capability == "sync" && limit.unavailable_because == "unresolved conflict"
    }));
    assert!(
        output
            .next_actions
            .iter()
            .any(|action| { action.command.as_deref() == Some("bowline resolve ~/Code") })
    );
}

#[test]
fn status_ignores_stale_git_index_conflict_bundles() {
    let temp = TempWorkspace::new("status-git-index-conflict").expect("temp workspace");
    let state_root = temp.root().join("state");
    let db_path = state_root.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    create_conflict_bundle(
        &state_root,
        ConflictRecord::opaque_git("apps/web/.git/index"),
        &[ConflictFile {
            relative_path: "apps/web/.git/index".to_string(),
            base: Some(b"base-index".to_vec()),
            local: Some(b"local-index".to_vec()),
            remote: Some(b"remote-index".to_vec()),
        }],
    )
    .expect("conflict bundle created");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .next_actions
            .iter()
            .all(|action| action.command.as_deref() != Some("bowline resolve ~/Code"))
    );
}

#[test]
fn stale_conflict_event_without_unresolved_bundle_does_not_keep_project_attention() {
    let temp = TempWorkspace::new("status-stale-conflict-event").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let backend_id = ProjectId::new("proj_backend");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(
        &store,
        &backend_id,
        &workspace_id,
        "root_code",
        "apps/backend",
    );
    let mut conflict = WorkspaceEvent::new(
        EventId::new("evt_backend_conflict"),
        EventName::ConflictCreated,
        "2026-06-23T12:00:00Z",
        EventSeverity::Attention,
        "Continuous sync detected a conflict in 1 path(s).",
        workspace_id.clone(),
    );
    conflict.project_id = Some(backend_id);
    conflict.path = Some("apps/backend/src/index.ts".to_string());
    conflict.subject = Some(EventSubject {
        kind: EventSubjectKind::Conflict,
        id: "conflict_backend".to_string(),
        path: Some("apps/backend/src/index.ts".to_string()),
    });
    store.append_event(conflict).expect("conflict event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .workspace_summary
            .expect("summary")
            .projects_needing_attention
            .is_empty()
    );
}

#[test]
fn event_subjects_map_to_status_domains() {
    let temp = TempWorkspace::new("status-event-domains").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let state_root = temp.root().join("state");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    create_conflict_bundle(
        &state_root,
        ConflictRecord::same_path("apps/web/src/index.ts"),
        &[ConflictFile {
            relative_path: "apps/web/src/index.ts".to_string(),
            base: Some(b"base\n".to_vec()),
            local: Some(b"local\n".to_vec()),
            remote: Some(b"remote\n".to_vec()),
        }],
    )
    .expect("conflict bundle created");
    let mut event = WorkspaceEvent::new(
        EventId::new("evt_conflict"),
        EventName::ConflictCreated,
        "2026-06-23T12:00:00Z",
        EventSeverity::Attention,
        "Merge conflict detected.",
        workspace_id,
    );
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Conflict,
        id: "conflict-1".to_string(),
        path: Some("apps/web/src/index.ts".to_string()),
    });
    store.append_event(event).expect("event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    let conflict_item = output
        .items
        .iter()
        .find(|item| {
            item.event_id
                .as_ref()
                .is_some_and(|id| id.as_str() == "evt_conflict")
        })
        .expect("conflict status item");
    assert_eq!(conflict_item.kind, StatusItemKind::Conflict);
}

#[test]
fn corrupt_metadata_events_return_error_instead_of_empty_history() {
    let temp = TempWorkspace::new("events-corrupt").expect("temp workspace");
    let db_path = temp.root().join("local.sqlite3");
    std::fs::write(&db_path, b"not sqlite").expect("corrupt db");

    let error = compose_events(EventsOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
        limit: 10,
    })
    .expect_err("corrupt events fail");

    assert!(matches!(error, super::LocalStatusError::MetadataState(_)));
}

#[test]
fn watch_frame_starts_with_current_status() {
    let status = super::missing_metadata_status(&StatusOptions {
        db_path: None,
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    });

    match initial_watch_frame(status) {
        bowline_core::commands::WatchFrame::Status { sequence, .. } => {
            assert_eq!(sequence, 1)
        }
        _ => panic!("expected status frame"),
    }
}
