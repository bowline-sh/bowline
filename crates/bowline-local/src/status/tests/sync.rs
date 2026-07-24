use super::*;

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

    let work_view = WorkView {
        id: WorkViewId::new("work_stale_view"),
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        project_path: "apps/web".to_string(),
        name: "stale-work-view".to_string(),
        visible_path: temp
            .root()
            .join(".work/apps/web/stale-work-view")
            .display()
            .to_string(),
        base_snapshot_id: SnapshotId::new("snap_project_old"),
        overlay_head: OVERLAY_HEAD_EMPTY.to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: WorkViewLifecycle::ReviewReady,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: true,
        },
        owner_device_id: Some(DeviceId::new("device_user_mac")),
        followed_by: Vec::new(),
        host_materializations: Vec::new(),
        attention: Vec::new(),
        created_at: "2026-06-23T12:00:01Z".to_string(),
        updated_at: "2026-06-23T12:00:01Z".to_string(),
    };
    store
        .upsert_work_view(&work_view)
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
