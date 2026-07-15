use super::*;

#[test]
fn materialization_queue_reports_bounded_workspace_progress() {
    let temp = TempWorkspace::new("status-materialization-progress").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let snapshot_id = SnapshotId::new("snap_materializing");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    let tasks = [
        materialization_task(
            "mat_ready",
            &workspace_id,
            &snapshot_id,
            "app/package.json",
            100,
        ),
        materialization_task(
            "mat_pending",
            &workspace_id,
            &snapshot_id,
            "app/src/main.rs",
            200,
        ),
    ];
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            &tasks,
            "2026-06-23T12:00:00Z",
        )
        .expect("tasks reconcile");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:01Z".to_string(),
    })
    .expect("status composes");

    assert!(output.items.iter().any(|item| {
        item.kind == StatusItemKind::Materialization
            && item.summary.contains("0/2 paths ready")
            && item.summary.contains("0/300 bytes")
    }));
    let summary = output.status_summary;
    assert_eq!(summary.attention, StatusAttention::None);
    assert!(
        summary
            .facts
            .iter()
            .any(|fact| fact.kind.as_str() == "project.not_materialized")
    );
}

#[test]
fn blocked_materialization_requires_canonical_attention() {
    for (case, state, failure_kind) in [
        (
            "missing",
            MaterializationTaskState::BlockedMissing,
            MaterializationFailureKind::ContentMissing,
        ),
        (
            "attention",
            MaterializationTaskState::Attention,
            MaterializationFailureKind::HydrationFailed,
        ),
    ] {
        let temp =
            TempWorkspace::new(&format!("status-materialization-{case}")).expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new(format!("ws_{case}"));
        let snapshot_id = SnapshotId::new(format!("snap_{case}"));
        let task_id = MaterializationTaskId::new(format!("mat_{case}"));
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .reconcile_materialization_tasks(
                &workspace_id,
                &snapshot_id,
                &[materialization_task(
                    task_id.as_str(),
                    &workspace_id,
                    &snapshot_id,
                    "app/src/main.rs",
                    200,
                )],
                "2026-06-23T12:00:00Z",
            )
            .expect("task reconciles");
        let claimed = store
            .claim_next_materialization_task(
                &workspace_id,
                "status-test",
                "claim-token",
                "2026-06-23T12:00:01Z",
            )
            .expect("claim succeeds")
            .expect("task is claimable");
        assert_eq!(claimed.id, task_id);
        assert!(
            store
                .finish_materialization_task(&crate::metadata::MaterializationTaskFinish {
                    id: &task_id,
                    claim_token: "claim-token",
                    claim_generation: claimed.claim_generation,
                    state,
                    error_kind: Some(failure_kind),
                    error: Some("materialization test blocker"),
                    not_before: None,
                    now: "2026-06-23T12:00:02Z",
                })
                .expect("task finishes")
        );

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:03Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status_summary.attention, StatusAttention::Required);
        let fact = output
            .status_summary
            .facts
            .iter()
            .find(|fact| fact.kind.as_str() == "project.materialization_blocked")
            .expect("typed materialization blocker fact");
        assert_eq!(fact.attention_impact, StatusAttention::Required);
        assert_eq!(
            fact.action.as_ref().map(|action| action.kind.as_str()),
            Some("materialize-project")
        );
    }
}

fn materialization_task(
    id: &str,
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    path: &str,
    bytes: u64,
) -> MaterializationTaskRecord {
    MaterializationTaskRecord {
        id: MaterializationTaskId::new(id),
        workspace_id: workspace_id.clone(),
        project_id: None,
        snapshot_id: snapshot_id.clone(),
        path: path.to_string(),
        expected_kind: NamespaceEntryKind::File,
        expected_content_id: None,
        expected_byte_len: bytes,
        expected_executable: false,
        priority_class: MaterializationPriorityClass::SmallFile,
        state: MaterializationTaskState::Queued,
        attempt_count: 0,
        claim_generation: 0,
        not_before: None,
        claim_token: None,
        claimed_by: None,
        claimed_at: None,
        lease_expires_at: None,
        last_error_kind: None,
        last_error: None,
        created_at: "2026-06-23T12:00:00Z".to_string(),
        updated_at: "2026-06-23T12:00:00Z".to_string(),
    }
}

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
fn reconciliation_required_operations_report_typed_queue_attention() {
    let temp = TempWorkspace::new("status-sync-reconciliation").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .enqueue_sync_operation(&sync_operation_record(
            "op_reconciliation",
            &workspace_id,
            "reconciliation_required",
            "reconciliation-key",
        ))
        .expect("reconciliation operation");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    let sync_queue = output.sync_queue.expect("sync queue is reported");
    assert_eq!(sync_queue.reconciliation_required, 1);
    assert_eq!(sync_queue.attention, 0);
    assert!(
        output
            .status
            .attention_items
            .contains(&"Sync queue needs attention.".to_string())
    );
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
fn status_reports_agent_lease_on_stale_snapshot_base() {
    let temp = TempWorkspace::new("status-stale-agent-base").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let root_path = temp.root().display().to_string();
    std::fs::create_dir_all(temp.root().join("apps/web")).expect("project directory");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &root_path,
            "2026-06-23T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    drop(store);

    create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: "apps/web".to_string(),
        task: "stale base".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device_user_mac"),
        generated_at: "2026-06-23T12:00:01Z".to_string(),
    })
    .expect("lease created");
    let store = MetadataStore::open(&db_path).expect("metadata reopens");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &project_id,
            &SnapshotId::new("snap_project_new"),
        )
        .expect("new snapshot");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:02Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert_eq!(output.freshness, FreshnessVerdict::Behind);
    assert_eq!(output.stale_bases.len(), 1);
    assert_eq!(output.stale_bases[0].verdict, FreshnessVerdict::Behind);
    assert_eq!(
        output.stale_bases[0].latest_snapshot_id.as_ref(),
        Some(&SnapshotId::new("snap_project_new"))
    );
    assert!(output.items.iter().any(|item| {
        item.kind == StatusItemKind::Source && item.event_name == Some(EventName::SourceStale)
    }));
    assert!(
        output
            .next_actions
            .iter()
            .any(|action| { action.command.as_deref() == Some("bowline status --watch") })
    );
}

#[test]
fn status_reports_review_ready_work_view_on_stale_snapshot_base() {
    let temp = TempWorkspace::new("status-stale-work-view-base").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let root_path = temp.root().display().to_string();
    std::fs::create_dir_all(temp.root().join("apps/web")).expect("project directory");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &root_path,
            "2026-06-23T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    drop(store);

    let work_view = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: "apps/web".to_string(),
        name: "stale-work-view".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device_user_mac")),
        generated_at: "2026-06-23T12:00:01Z".to_string(),
    })
    .expect("work view created")
    .work_view;
    let store = MetadataStore::open(&db_path).expect("metadata reopens");
    let mut review_ready = work_view.clone();
    review_ready.lifecycle = WorkViewLifecycle::ReviewReady;
    store
        .upsert_work_view(&review_ready)
        .expect("review-ready work view");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &project_id,
            &SnapshotId::new("snap_project_new"),
        )
        .expect("new snapshot");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:02Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert_eq!(output.freshness, FreshnessVerdict::Behind);
    assert_eq!(output.stale_bases.len(), 1);
    let stale_base = &output.stale_bases[0];
    assert!(stale_base.summary.contains(work_view.name.as_str()));
    assert_eq!(
        stale_base.base_snapshot_id,
        Some(work_view.base_snapshot_id)
    );
    assert_eq!(
        stale_base.latest_snapshot_id,
        Some(SnapshotId::new("snap_project_new"))
    );
    assert!(output.items.iter().any(|item| {
        item.kind == StatusItemKind::Source && item.event_name == Some(EventName::SourceStale)
    }));
    assert!(
        output
            .next_actions
            .iter()
            .any(|action| { action.command.as_deref() == Some("bowline status --watch") })
    );
}

#[test]
fn scoped_project_observed_git_freshness_is_current() {
    let temp = TempWorkspace::new("status-unknown-git-freshness").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.freshness, FreshnessVerdict::Current);
    assert_eq!(output.stale_bases.len(), 1);
    assert_eq!(output.stale_bases[0].axis, FreshnessAxis::Git);
    assert_eq!(output.stale_bases[0].verdict, FreshnessVerdict::Current);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .all(|item| !item.contains("freshness"))
    );
}

#[test]
fn scoped_project_unavailable_git_observation_is_unknown() {
    let temp = TempWorkspace::new("status-unavailable-git-freshness").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    store
        .connection()
        .execute(
            "UPDATE projects SET git_observer_state = 'unavailable' WHERE id = ?1",
            [project_id.as_str()],
        )
        .expect("unavailable observer state");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Limited);
    assert_eq!(output.freshness, FreshnessVerdict::Unknown);
    assert_eq!(output.stale_bases.len(), 1);
    assert_eq!(output.stale_bases[0].axis, FreshnessAxis::Git);
    assert_eq!(output.stale_bases[0].verdict, FreshnessVerdict::Unknown);
}

#[test]
fn status_reports_state_root_unresolved_conflict_bundles() {
    let temp = TempWorkspace::new("status-state-root-conflict").expect("temp workspace");
    let state_root = temp.root().join("state");
    let db_path = state_root.join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let workspace_root = temp.root().join("Custom Code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "Custom Code", "2026-06-23T11:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_custom",
            &workspace_id,
            &workspace_root.display().to_string(),
            "2026-06-23T11:00:00Z",
        )
        .expect("root");
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
    let expected_command = format!(
        "bowline resolve {}",
        bowline_core::shell::quote_word(&workspace_root.display().to_string())
    );
    assert!(
        output
            .next_actions
            .iter()
            .any(|action| action.command.as_deref() == Some(expected_command.as_str()))
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
