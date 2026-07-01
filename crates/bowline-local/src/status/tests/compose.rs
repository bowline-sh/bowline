use super::*;

#[test]
fn missing_metadata_returns_non_mutating_attention_status() {
    let temp = TempWorkspace::new("status-missing").expect("temp workspace");
    let db_path = temp.root().join("missing").join("local.sqlite3");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some("acme/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(!db_path.exists());
    assert_eq!(output.next_actions[0].label, "Initialize ~/Code when ready");
    assert!(output.next_actions[0].command.is_none());
}

#[test]
fn corrupt_metadata_returns_limited_status() {
    let temp = TempWorkspace::new("status-corrupt").expect("temp workspace");
    let db_path = temp.root().join("local.sqlite3");
    std::fs::write(&db_path, b"not sqlite").expect("corrupt db");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Limited);
    assert_eq!(output.limits[0].capability, "local metadata");
}

#[test]
fn redact_workspace_path_strips_root_and_drops_sensitive_paths() {
    let root = Some("~/Code");
    assert_eq!(
        redact_workspace_path("~/Code/apps/web/src/index.ts", root),
        Some("apps/web/src/index.ts".to_string())
    );
    // Already-relative paths are kept untouched.
    assert_eq!(
        redact_workspace_path("apps/api/main.rs", root),
        Some("apps/api/main.rs".to_string())
    );
    // Absolute paths outside the workspace root are dropped entirely.
    assert_eq!(
        redact_workspace_path("/workspace/user/secret.txt", root),
        None
    );
    assert_eq!(
        redact_workspace_path("~/CodeSecrets/private.txt", root),
        None
    );
    assert_eq!(redact_workspace_path("~/.ssh/id_ed25519", root), None);
    assert_eq!(redact_workspace_path("C:\\Users\\user\\app", root), None);
    // Env files are dropped even when workspace-relative.
    assert_eq!(redact_workspace_path("apps/web/.env.local", root), None);
    assert_eq!(redact_workspace_path("~/Code/api/.env", root), None);
    // Empty / whitespace yields nothing.
    assert_eq!(redact_workspace_path("   ", root), None);
}

#[test]
fn redacted_status_snapshot_maps_states_and_redacts_paths() {
    let temp = TempWorkspace::new("status-redacted").expect("temp workspace");
    let db_path = temp.root().join("local.sqlite3");
    std::fs::write(&db_path, b"not sqlite").expect("corrupt db");

    let mut output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-29T12:00:00Z".to_string(),
    })
    .expect("status composes");
    output.resolved_workspace_root = Some("~/Code".to_string());
    output
        .status
        .attention_items
        .push("1 unresolved conflict needs attention: ~/Code/apps/web/.env.local.".to_string());

    let mut visible_item = base_status_item(StatusItemKind::Source, "edited file");
    visible_item.path = Some("~/Code/apps/web/src/index.ts".to_string());
    let mut secret_item = base_status_item(StatusItemKind::Env, "env file changed");
    secret_item.path = Some("~/Code/apps/web/.env.local".to_string());
    let mut absolute_item = base_status_item(StatusItemKind::Device, "external path");
    absolute_item.summary = "external path: /workspace/user/secret".to_string();
    absolute_item.path = Some("/workspace/user/secret".to_string());
    output.items = vec![visible_item, secret_item, absolute_item];
    output.limits = vec![LimitedCapability {
        capability: "search".to_string(),
        unavailable_because: "index degraded".to_string(),
        still_works: vec!["status".to_string()],
        path: Some("~/Code/apps/api".to_string()),
    }];

    let snapshot = redacted_status_snapshot(&output, "device-daemon");

    assert_eq!(snapshot.status_level, "limited");
    assert_eq!(snapshot.published_by_device_id, "device-daemon");
    assert_eq!(snapshot.generated_at, "2026-06-29T12:00:00Z");
    assert_eq!(
        snapshot.attention_items.last().map(String::as_str),
        Some("Sensitive local path redacted.")
    );
    assert!(snapshot.snapshot_id.starts_with("wss_"));
    // Snapshot id is stable for a given (workspace, generatedAt).
    assert_eq!(
        snapshot.snapshot_id,
        redacted_status_snapshot(&output, "device-daemon").snapshot_id
    );
    assert_eq!(snapshot.items.len(), 3);
    assert_eq!(
        snapshot.items[0].path.as_deref(),
        Some("apps/web/src/index.ts")
    );
    assert_eq!(snapshot.items[0].kind, "source");
    assert!(snapshot.items[1].path.is_none(), "env path must be dropped");
    assert!(
        snapshot.items[2].path.is_none(),
        "absolute path must be dropped"
    );
    assert_eq!(snapshot.items[2].summary, "Sensitive local path redacted.");
    assert_eq!(snapshot.limits.len(), 1);
    assert_eq!(snapshot.limits[0].path.as_deref(), Some("apps/api"));
    assert_eq!(snapshot.limits[0].capability, "search");
}

#[test]
fn zero_byte_metadata_is_observational_attention_without_mutation() {
    let temp = TempWorkspace::new("status-empty-file").expect("temp workspace");
    let db_path = temp.root().join("local.sqlite3");
    std::fs::write(&db_path, []).expect("empty db");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert_eq!(std::fs::metadata(&db_path).expect("metadata").len(), 0);
    assert!(!db_path.with_extension("sqlite3-wal").exists());
}

#[test]
fn empty_accepted_workspace_is_healthy() {
    let temp = TempWorkspace::new("status-empty").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
        .expect("root insert");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(
        output.status.level,
        StatusLevel::Healthy,
        "{:?}",
        output.status.attention_items
    );
    assert!(render_status_human(&output).contains("Status: healthy"));
}

#[test]
fn observed_workspace_with_ready_sync_is_healthy() {
    let temp = TempWorkspace::new("status-observed-sync-ready").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    store
        .set_observed_summary(
            &workspace_id,
            &bowline_core::status::ObservedWorkspaceSummary {
                repo_count: 1,
                no_remote_repo_count: 1,
                workspace_sync_path_count: 12,
                env_file_count: 1,
                ..Default::default()
            },
            "2026-06-23T12:00:00Z",
        )
        .expect("observed summary");
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_sync_ready"),
            EventName::SyncCompleted,
            "2026-06-23T12:00:01Z",
            EventSeverity::Info,
            "Sync completed.",
            workspace_id.clone(),
        ))
        .expect("sync event append");
    store
        .set_component_state("sync", "ready", "2026-06-23T12:00:01Z")
        .expect("sync component");
    store
        .set_component_state("watcher", "ready", "2026-06-23T12:00:01Z")
        .expect("watcher component");
    store
        .set_component_state("network", "online", "2026-06-23T12:00:01Z")
        .expect("network component");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:02Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(
        output.status.level,
        StatusLevel::Healthy,
        "{:?}",
        output.status.attention_items
    );
    assert!(output.status.attention_items.is_empty());
    assert!(
        output
            .items
            .iter()
            .any(|item| item.summary.contains("sync is active"))
    );
}

#[test]
fn status_reports_accepted_workspace_root_from_metadata() {
    let temp = TempWorkspace::new("status-root").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let root_path = temp.root().join("CustomCode").display().to_string();
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_custom",
            &workspace_id,
            &root_path,
            "2026-06-23T12:00:00Z",
        )
        .expect("root insert");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(
        output.resolved_workspace_root.as_deref(),
        Some(root_path.as_str())
    );
}

#[test]
fn status_reports_durable_index_state_without_project_scan() {
    let temp = TempWorkspace::new("status-index-state").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    store
        .connection()
        .execute(
            "INSERT INTO index_work
             (id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark, state, reason, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                "ix_proj_web",
                workspace_id.as_str(),
                project_id.as_str(),
                "apps/web",
                "project",
                42_i64,
                42_i64,
                "ready",
                Option::<&str>::None,
                "2026-06-23T12:00:00Z",
            ],
        )
        .expect("index work insert");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:01Z".to_string(),
    })
    .expect("status composes");
    let index = output.index.expect("index status");
    assert_eq!(index.state, IndexState::Ready);
    assert_eq!(index.indexed_at.as_deref(), Some("2026-06-23T12:00:00Z"));
    assert_eq!(index.pending_path_count, Some(0));
}

#[test]
fn stale_index_metadata_promotes_top_level_status_attention() {
    let temp = TempWorkspace::new("status-index-stale").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    store
        .connection()
        .execute(
            "INSERT INTO index_work
             (id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark, state, reason, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                "ix_proj_web_stale",
                workspace_id.as_str(),
                project_id.as_str(),
                "apps/web",
                "project",
                42_i64,
                1_i64,
                "ready",
                Option::<&str>::None,
                "2026-06-23T12:00:00Z",
            ],
        )
        .expect("index work insert");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:01Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.index.expect("index").state, IndexState::Stale);
    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(
        output
            .items
            .iter()
            .any(|item| item.kind == StatusItemKind::Index)
    );
}

#[test]
fn status_reports_single_active_lease_hydration_budget() {
    let temp = TempWorkspace::new("status-hydration-budget").expect("temp workspace");
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

    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: "apps/web".to_string(),
        task: "hydrate cold files".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 2048,
        work_view: true,
        device_id: DeviceId::new("device_user_mac"),
        generated_at: "2026-06-23T12:00:01Z".to_string(),
    })
    .expect("lease created")
    .lease;

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:02Z".to_string(),
    })
    .expect("status composes");
    let budget = output.hydration_budget.expect("hydration budget");
    assert_eq!(budget.state, HydrationBudgetState::Available);
    assert_eq!(budget.limit_bytes, 2048);
    assert_eq!(budget.lease_id.as_ref(), Some(&lease.id));
}
