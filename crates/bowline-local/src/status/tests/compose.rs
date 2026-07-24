use super::*;
use std::time::{Duration, Instant};

fn revision_options(db_path: &std::path::Path) -> StatusOptions {
    StatusOptions {
        db_path: Some(db_path.to_path_buf()),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    }
}

#[test]
fn local_fact_collector_preserves_composition_parity_and_typed_revisions() {
    let temp = TempWorkspace::new("status-fact-collector-parity").expect("temp workspace");
    let db_path = temp.root().join("missing/local.sqlite3");
    let options = revision_options(&db_path);
    let direct = compose_status(options.clone()).expect("direct composition");
    let start = Instant::now();
    let mut collector = LocalStatusFactCollector::new(Some(db_path)).expect("collector");

    let LocalStatusCollection::Collected(initial) = collector
        .collect_if_needed(options.clone(), start)
        .expect("initial facts")
    else {
        panic!("initial collection must return facts");
    };
    assert_eq!(initial.output, direct);
    assert_eq!(initial.revision.get(), 1);
    assert_eq!(initial.observed_at, options.generated_at);

    assert!(matches!(
        collector.collect_if_needed(options.clone(), start + Duration::from_secs(1)),
        Ok(LocalStatusCollection::Unchanged)
    ));
    collector.mark_local_dirty();
    let LocalStatusCollection::Collected(changed) = collector
        .collect_if_needed(options, start + Duration::from_secs(2))
        .expect("dirty facts")
    else {
        panic!("dirty collection must return facts");
    };
    assert_eq!(changed.revision.get(), 2);
    assert_eq!(
        collector.metrics(),
        StatusComposerMetrics {
            collector_calls: 3,
            collector_skips: 1,
            full_compositions: 2,
            store_opens: 0,
        }
    );
}

#[test]
fn revisioned_status_skips_sixty_unchanged_ticks() {
    let temp = TempWorkspace::new("status-revision-unchanged").expect("temp workspace");
    let db_path = temp.root().join("missing/local.sqlite3");
    let start = Instant::now();
    let mut composer = RevisionedStatusComposer::new(Some(db_path.clone())).expect("composer");

    assert!(matches!(
        composer.compose_if_needed(revision_options(&db_path), start),
        Ok(RevisionedStatus::Composed(_))
    ));
    for tick in 1..=60 {
        assert!(matches!(
            composer.compose_if_needed(
                revision_options(&db_path),
                start + Duration::from_millis(tick),
            ),
            Ok(RevisionedStatus::Unchanged)
        ));
    }
}

#[test]
fn explicit_dirty_and_safety_deadline_each_recompose_once() {
    let temp = TempWorkspace::new("status-revision-dirty").expect("temp workspace");
    let db_path = temp.root().join("missing/local.sqlite3");
    let start = Instant::now();
    let mut composer = RevisionedStatusComposer::new(Some(db_path.clone())).expect("composer");
    composer
        .compose_if_needed(revision_options(&db_path), start)
        .expect("initial compose");

    composer.mark_local_dirty();
    assert!(matches!(
        composer.compose_if_needed(revision_options(&db_path), start + Duration::from_secs(1)),
        Ok(RevisionedStatus::Composed(_))
    ));
    assert!(matches!(
        composer.compose_if_needed(revision_options(&db_path), start + Duration::from_secs(2)),
        Ok(RevisionedStatus::Unchanged)
    ));
    assert!(matches!(
        composer.compose_if_needed(
            revision_options(&db_path),
            start + Duration::from_secs(1) + STATUS_SAFETY_REFRESH_INTERVAL,
        ),
        Ok(RevisionedStatus::Composed(_))
    ));
}

#[test]
fn external_sqlite_commit_changes_source_revision() {
    let temp = TempWorkspace::new("status-revision-external").expect("temp workspace");
    let db_path = temp.root().join("state/local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_revision");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    drop(store);
    let start = Instant::now();
    let mut composer = RevisionedStatusComposer::new(Some(db_path.clone())).expect("composer");
    composer
        .compose_if_needed(revision_options(&db_path), start)
        .expect("initial compose");

    let external = MetadataStore::open(&db_path).expect("external metadata opens");
    seed_project(
        &external,
        &ProjectId::new("project_revision"),
        &workspace_id,
        "root_code",
        "revision-project",
    );
    drop(external);

    assert!(matches!(
        composer.compose_if_needed(revision_options(&db_path), start + Duration::from_secs(1)),
        Ok(RevisionedStatus::Composed(_))
    ));
    assert!(matches!(
        composer.compose_if_needed(revision_options(&db_path), start + Duration::from_secs(2)),
        Ok(RevisionedStatus::Unchanged)
    ));
}

#[test]
fn status_collection_reads_while_writer_holds_immediate_transaction() {
    let temp = TempWorkspace::new("status-reader-contention").expect("temp workspace");
    let db_path = temp.root().join("state/local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_status_reader_contention");
    let writer = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&writer, &workspace_id);
    writer
        .connection()
        .execute("BEGIN IMMEDIATE", [])
        .expect("writer reserves database");

    let mut collector = LocalStatusFactCollector::new(Some(db_path.clone())).expect("collector");
    let LocalStatusCollection::Collected(facts) = collector
        .collect_if_needed(revision_options(&db_path), Instant::now())
        .expect("status reads committed metadata during write")
    else {
        panic!("initial status collection must return facts");
    };
    assert_eq!(facts.output.workspace_id, workspace_id);

    writer
        .connection()
        .execute("ROLLBACK", [])
        .expect("writer releases database");
}

#[test]
fn database_removal_and_replacement_invalidate_retained_store() {
    let temp = TempWorkspace::new("status-revision-replacement").expect("temp workspace");
    let db_path = temp.root().join("state/local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_before_replacement");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    drop(store);
    let start = Instant::now();
    let mut composer = RevisionedStatusComposer::new(Some(db_path.clone())).expect("composer");
    composer
        .compose_if_needed(revision_options(&db_path), start)
        .expect("initial compose");

    std::fs::remove_file(&db_path).expect("database removed");
    let removed = composer
        .compose_if_needed(revision_options(&db_path), start + Duration::from_secs(1))
        .expect("removed database composes");
    let RevisionedStatus::Composed(removed) = removed else {
        panic!("database removal must recompose");
    };
    assert_eq!(removed.workspace_id.as_str(), "ws_local_uninitialized");

    let replacement_id = WorkspaceId::new("ws_after_replacement");
    let replacement = MetadataStore::open(&db_path).expect("replacement metadata opens");
    seed_workspace_root(&replacement, &replacement_id);
    drop(replacement);
    let replaced = composer
        .compose_if_needed(revision_options(&db_path), start + Duration::from_secs(2))
        .expect("replacement database composes");
    let RevisionedStatus::Composed(replaced) = replaced else {
        panic!("database replacement must recompose");
    };
    assert_eq!(replaced.workspace_id, replacement_id);
}

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
fn explicit_unknown_root_does_not_fall_back_to_current_workspace() {
    let temp = TempWorkspace::new("status-explicit-root-miss").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    drop(store);

    let requested = temp.root().join("other-code").display().to_string();
    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(requested.clone()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.workspace_id.as_str(), "ws_local_uninitialized");
    assert_eq!(output.requested_path.as_deref(), Some(requested.as_str()));
}

#[test]
fn corrupt_metadata_requires_attention_while_reporting_unavailability() {
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

    assert_eq!(output.status.level, StatusLevel::Attention);
    let summary = output.status_summary;
    assert_eq!(summary.availability, StatusAvailability::Unavailable);
    assert_eq!(summary.attention, StatusAttention::Required);
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
        support_capability: None,
        unavailable_because: "index degraded".to_string(),
        still_works: vec!["status".to_string()],
        path: Some("~/Code/apps/api".to_string()),
    }];
    output.status_summary.facts.push(
        StatusFact::new(
            "path-conflict",
            "sync.conflict_unresolved",
            "local-conflict-store",
            StatusFactScope::Path,
            output.generated_at.clone(),
            "path-conflict",
        )
        .with_scope_id("apps/web/src/index.ts"),
    );
    output.status_summary.facts.push(StatusFact::new(
        "client-update",
        "client.update_available",
        "release-manifest",
        StatusFactScope::Device,
        output.generated_at.clone(),
        "client-update",
    ));
    output.status_summary.facts.push(StatusFact::new(
        "network-offline",
        "network.offline",
        "local-network-observer",
        StatusFactScope::Device,
        output.generated_at.clone(),
        "network-offline",
    ));

    let snapshot = redacted_status_snapshot(&output, "device-daemon");

    assert_eq!(snapshot.availability, "degraded");
    assert_eq!(snapshot.attention, "none");
    assert_eq!(snapshot.primary_fact_id.as_deref(), Some("network-offline"));
    assert!(
        snapshot
            .facts
            .iter()
            .any(|fact| fact.kind.as_str() == "client.update_available"),
        "non-workspace facts remain available for hosted presentation"
    );
    assert_eq!(snapshot.published_by_device_id, "device-daemon");
    assert_eq!(snapshot.observed_at, "2026-06-29T12:00:00Z");
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
    assert!(
        snapshot
            .facts
            .iter()
            .all(|fact| fact.scope != StatusFactScope::Path),
        "hosted status must not carry path-scoped fact identifiers"
    );
    assert!(snapshot.items[0].path.is_none());
    assert_eq!(snapshot.items[0].kind, "source");
    assert!(snapshot.items[1].path.is_none(), "env path must be dropped");
    assert!(
        snapshot.items[2].path.is_none(),
        "absolute path must be dropped"
    );
    assert_eq!(snapshot.items[2].summary, "Sensitive local path redacted.");
    assert_eq!(snapshot.limits.len(), 1);
    assert!(snapshot.limits[0].path.is_none());
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
            .any(|item| item.summary.contains("Tracking"))
    );
}

#[test]
fn status_reports_blocked_local_only_and_unavailable_git_facts() {
    let temp = TempWorkspace::new("status-observed-facts").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let mut store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    store
        .replace_observed_paths(
            &workspace_id,
            &[
                ObservedLocalPath {
                    project_id: Some(project_id.clone()),
                    path: "apps/web/blocked.pem".to_string(),
                    classification: PathClassification::Blocked,
                    mode: MaterializationMode::Blocked,
                    access: vec![AccessFlag::AgentHidden],
                },
                ObservedLocalPath {
                    project_id: Some(project_id.clone()),
                    path: "apps/web/.env".to_string(),
                    classification: PathClassification::LocalOnly,
                    mode: MaterializationMode::LocalOnly,
                    access: Vec::new(),
                },
            ],
            "2026-07-07T12:00:00Z",
        )
        .expect("observed paths");
    store
        .set_observed_summary(
            &workspace_id,
            &bowline_core::status::ObservedWorkspaceSummary {
                repo_count: 1,
                blocked_path_count: 1,
                local_only_path_count: 1,
                git_unavailable_project_count: 1,
                workspace_sync_path_count: 3,
                ..Default::default()
            },
            "2026-07-07T12:00:00Z",
        )
        .expect("observed summary");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-07-07T12:00:01Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(output.items.iter().any(|item| {
        item.summary == "1 path blocked by policy; excluded from sync."
            && item.kind == StatusItemKind::Policy
    }));
    assert!(output.items.iter().any(|item| {
        item.summary == "1 path kept local-only; excluded from workspace sync."
            && item.kind == StatusItemKind::Materialization
    }));
    assert!(output.items.iter().any(|item| {
        item.summary == "Blocked by policy; excluded from sync."
            && item.path.as_deref() == Some("apps/web/blocked.pem")
            && item.classification == Some(PathClassification::Blocked)
            && item.mode == Some(MaterializationMode::Blocked)
    }));
    assert!(output.items.iter().any(|item| {
        item.summary == "Kept local-only; excluded from workspace sync."
            && item.path.as_deref() == Some("apps/web/.env")
            && item.classification == Some(PathClassification::LocalOnly)
            && item.mode == Some(MaterializationMode::LocalOnly)
    }));
    assert!(output.items.iter().any(|item| {
        item.summary == "1 project with Git state unavailable; source status is degraded."
            && item.kind == StatusItemKind::Source
    }));
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
fn project_status_reports_needs_setup_for_unrestored_lockfile_project() {
    let temp = TempWorkspace::new("status-setup-needs").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let project_root = temp.root().join("apps/web");
    std::fs::create_dir_all(&project_root).expect("project directory");
    std::fs::write(
        temp.root().join("apps/web/pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n",
    )
    .expect("lockfile");
    std::fs::write(
        temp.root().join("apps/web/package.json"),
        "{\"packageManager\":\"pnpm@10.30.0\"}\n",
    )
    .expect("package");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-07-03T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    drop(store);

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:01Z".to_string(),
    })
    .expect("status composes");

    let readiness = output.setup_readiness.as_ref().expect("setup readiness");
    assert_eq!(readiness.state, ProjectSetupReadinessState::NeedsSetup);
    assert!(readiness.reason.contains("pnpm-lock.yaml"));
    let setup_action = output
        .next_actions
        .iter()
        .find(|action| action.label == "Run setup")
        .expect("setup action");
    assert_eq!(
        setup_action.command.as_deref(),
        Some(format!("bowline setup {}", project_root.display()).as_str())
    );
    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(output.status.attention_items.iter().any(|item| {
        item.contains("Project setup readiness is needs-setup") && item.contains("pnpm-lock.yaml")
    }));
    assert!(output.items.iter().any(|item| {
        item.kind == StatusItemKind::Setup
            && item
                .summary
                .contains("Project setup readiness is needs-setup")
    }));
    assert!(output.next_actions.iter().any(|action| {
        action.command.as_deref()
            == Some(format!("bowline setup {}", project_root.display()).as_str())
    }));
}

#[test]
fn project_status_reports_unknown_setup_when_project_directory_is_missing() {
    let temp = TempWorkspace::new("status-setup-missing-project").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-07-03T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    drop(store);

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:01Z".to_string(),
    })
    .expect("status composes");

    let readiness = output.setup_readiness.as_ref().expect("setup readiness");
    assert_eq!(readiness.state, ProjectSetupReadinessState::Unknown);
    assert!(
        readiness
            .reason
            .contains("Project directory is not materialized locally")
    );
    assert_eq!(output.status.level, StatusLevel::Limited);
    assert!(output.status.attention_items.iter().any(|item| {
        item.contains("Project setup readiness is unknown")
            && item.contains("not materialized locally")
    }));
}

#[test]
fn project_status_reports_runnable_for_matching_setup_receipt() {
    let temp = TempWorkspace::new("status-setup-runnable").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let project_root = temp.root().join("apps/web");
    std::fs::create_dir_all(&project_root).expect("project directory");
    std::fs::write(project_root.join("Cargo.lock"), "# lock\n").expect("lockfile");
    std::fs::write(
        project_root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel='1.88'\n",
    )
    .expect("toolchain");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-07-03T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    let plan = crate::setup::infer_setup_plan(&project_root)
        .expect("setup infers")
        .expect("setup plan");
    let mut first_identity = None;
    for command in &plan.commands {
        let recipe_hash = crate::setup::inferred_recipe_hash(command);
        let receipt_id = inferred_command_receipt_id(&workspace_id, &project_id, command);
        let identity = crate::setup::collect_setup_identity(
            &project_root,
            "default",
            Some(recipe_hash.clone()),
            Some(command.package_manager.clone()),
        )
        .expect("identity");
        first_identity.get_or_insert_with(|| identity.hash.clone());
        store
            .upsert_setup_receipt(&setup_receipt_record(SetupReceiptFixture {
                id: Some(receipt_id.as_str()),
                workspace_id: &workspace_id,
                project_id: &project_id,
                cwd: "apps/web",
                state: "completed",
                recipe_hash: Some(recipe_hash.as_str()),
                setup_identity_hash: &identity.hash,
                readiness_state: "runnable",
                readiness_reason: "Setup command completed for the current setup identity.",
                readiness_remedy: "",
            }))
            .expect("receipt");
    }
    drop(store);

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:01Z".to_string(),
    })
    .expect("status composes");

    let readiness = output.setup_readiness.as_ref().expect("setup readiness");
    assert_eq!(readiness.state, ProjectSetupReadinessState::Runnable);
    assert_eq!(
        readiness.identity_hash.as_deref(),
        first_identity.as_deref()
    );
    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .all(|item| !item.contains("Project setup readiness"))
    );
}

#[test]
fn project_status_downgrades_when_inferred_setup_outputs_disappear() {
    let temp = TempWorkspace::new("status-setup-output-missing").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let project_root = temp.root().join("apps/web");
    std::fs::create_dir_all(project_root.join("node_modules")).expect("project directory");
    std::fs::write(
        project_root.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n",
    )
    .expect("lockfile");
    std::fs::write(
        project_root.join("package.json"),
        "{\"packageManager\":\"pnpm@10.30.0\"}\n",
    )
    .expect("package");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-07-03T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    let plan = crate::setup::infer_setup_plan(&project_root)
        .expect("setup infers")
        .expect("setup plan");
    let command = plan.commands.first().expect("setup command");
    let recipe_hash = crate::setup::inferred_recipe_hash(command);
    let receipt_id = inferred_command_receipt_id(&workspace_id, &project_id, command);
    let identity = crate::setup::collect_setup_identity(
        &project_root,
        "default",
        Some(recipe_hash.clone()),
        Some(command.package_manager.clone()),
    )
    .expect("identity");
    store
        .upsert_setup_receipt(&setup_receipt_record(SetupReceiptFixture {
            id: Some(receipt_id.as_str()),
            workspace_id: &workspace_id,
            project_id: &project_id,
            cwd: "apps/web",
            state: "completed",
            recipe_hash: Some(recipe_hash.as_str()),
            setup_identity_hash: &identity.hash,
            readiness_state: "runnable",
            readiness_reason: "Setup command completed for the current setup identity.",
            readiness_remedy: "",
        }))
        .expect("receipt");
    drop(store);

    let runnable = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:01Z".to_string(),
    })
    .expect("status composes");
    assert_eq!(
        runnable
            .setup_readiness
            .as_ref()
            .expect("setup readiness")
            .state,
        ProjectSetupReadinessState::Runnable
    );

    std::fs::remove_dir(project_root.join("node_modules")).expect("node_modules removed");
    let missing_output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:02Z".to_string(),
    })
    .expect("status composes");

    let readiness = missing_output
        .setup_readiness
        .as_ref()
        .expect("setup readiness");
    assert_eq!(readiness.state, ProjectSetupReadinessState::NeedsSetup);
    assert!(readiness.reason.contains("node_modules"));
    assert_eq!(
        readiness.latest_receipt_id.as_deref(),
        Some(receipt_id.as_str())
    );
    assert_eq!(missing_output.status.level, StatusLevel::Attention);
}

#[test]
fn recipe_status_requires_every_command_receipt_before_runnable() {
    let temp = TempWorkspace::new("status-setup-recipe-partial").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let project_root = temp.root().join("apps/web");
    std::fs::create_dir_all(&project_root).expect("project directory");
    std::fs::write(project_root.join(".bowlinesetup"), "true\necho done\n").expect("recipe");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-07-03T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    let recipe = crate::setup::load_setup_recipe(&project_root, project_root.join(".bowlinesetup"))
        .expect("recipe loads");
    let identity = crate::setup::collect_setup_identity(
        &project_root,
        "default",
        Some(recipe.recipe_hash.clone()),
        None,
    )
    .expect("identity");
    let first_receipt_id = recipe_command_receipt_id(&workspace_id, &project_id, &recipe, 0);
    store
        .upsert_setup_receipt(&setup_receipt_record(SetupReceiptFixture {
            id: Some(first_receipt_id.as_str()),
            workspace_id: &workspace_id,
            project_id: &project_id,
            cwd: "apps/web",
            state: "completed",
            recipe_hash: Some(recipe.recipe_hash.as_str()),
            setup_identity_hash: &identity.hash,
            readiness_state: "runnable",
            readiness_reason: "Setup command completed for the current setup identity.",
            readiness_remedy: "",
        }))
        .expect("partial receipt");
    drop(store);

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:01Z".to_string(),
    })
    .expect("status composes");

    let readiness = output.setup_readiness.as_ref().expect("setup readiness");
    assert_eq!(readiness.state, ProjectSetupReadinessState::NeedsSetup);
    assert!(readiness.reason.contains("line 2"));
    let setup_action = output
        .next_actions
        .iter()
        .find(|action| action.label == "Approve and run setup")
        .expect("setup action");
    assert_eq!(
        setup_action.command.as_deref(),
        Some(format!("bowline setup {} --yes", project_root.display()).as_str())
    );
    assert_eq!(output.status.level, StatusLevel::Attention);
}

#[test]
fn recipe_status_is_runnable_when_every_command_receipt_matches() {
    let temp = TempWorkspace::new("status-setup-recipe-runnable").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let project_root = temp.root().join("apps/web");
    std::fs::create_dir_all(&project_root).expect("project directory");
    std::fs::write(project_root.join(".bowlinesetup"), "true\necho done\n").expect("recipe");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-07-03T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    let recipe = crate::setup::load_setup_recipe(&project_root, project_root.join(".bowlinesetup"))
        .expect("recipe loads");
    let identity = crate::setup::collect_setup_identity(
        &project_root,
        "default",
        Some(recipe.recipe_hash.clone()),
        None,
    )
    .expect("identity");
    for index in 0..recipe.commands.len() {
        let receipt_id = recipe_command_receipt_id(&workspace_id, &project_id, &recipe, index);
        store
            .upsert_setup_receipt(&setup_receipt_record(SetupReceiptFixture {
                id: Some(receipt_id.as_str()),
                workspace_id: &workspace_id,
                project_id: &project_id,
                cwd: "apps/web",
                state: "completed",
                recipe_hash: Some(recipe.recipe_hash.as_str()),
                setup_identity_hash: &identity.hash,
                readiness_state: "runnable",
                readiness_reason: "Setup command completed for the current setup identity.",
                readiness_remedy: "",
            }))
            .expect("receipt");
    }
    drop(store);

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:01Z".to_string(),
    })
    .expect("status composes");

    let readiness = output.setup_readiness.as_ref().expect("setup readiness");
    assert_eq!(readiness.state, ProjectSetupReadinessState::Runnable);
    assert!(
        output.next_actions.iter().all(|action| {
            action.label != "Run setup" && action.label != "Approve and run setup"
        })
    );
    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .all(|item| !item.contains("Project setup readiness"))
    );
}

#[test]
fn project_status_reports_blocked_for_missing_setup_executable_receipt() {
    let temp = TempWorkspace::new("status-setup-blocked").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let project_root = temp.root().join("apps/web");
    std::fs::create_dir_all(&project_root).expect("project directory");
    std::fs::write(project_root.join("uv.lock"), "# lock\n").expect("lockfile");
    std::fs::write(
        project_root.join("pyproject.toml"),
        "[project]\nname='web'\n",
    )
    .expect("pyproject");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-07-03T12:00:00Z",
        )
        .expect("root insert");
    seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
    let plan = crate::setup::infer_setup_plan(&project_root)
        .expect("setup infers")
        .expect("setup plan");
    let command = plan.commands.first().expect("setup command");
    let recipe_hash = crate::setup::inferred_recipe_hash(command);
    let receipt_id = inferred_command_receipt_id(&workspace_id, &project_id, command);
    let identity = crate::setup::collect_setup_identity(
        &project_root,
        "default",
        Some(recipe_hash.clone()),
        Some(command.package_manager.clone()),
    )
    .expect("identity");
    store
        .upsert_setup_receipt(&setup_receipt_record(SetupReceiptFixture {
            id: Some(receipt_id.as_str()),
            workspace_id: &workspace_id,
            project_id: &project_id,
            cwd: "apps/web",
            state: "failed",
            recipe_hash: Some(recipe_hash.as_str()),
            setup_identity_hash: &identity.hash,
            readiness_state: "blocked",
            readiness_reason: "Required setup executable `uv` is not available.",
            readiness_remedy: "Install `uv` on this machine, then rerun setup for the hot project.",
        }))
        .expect("receipt");
    drop(store);

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-03T12:00:01Z".to_string(),
    })
    .expect("status composes");

    let readiness = output.setup_readiness.as_ref().expect("setup readiness");
    assert_eq!(readiness.state, ProjectSetupReadinessState::Blocked);
    assert!(readiness.reason.contains("`uv`"));
    assert!(
        readiness
            .remedy
            .as_deref()
            .is_some_and(|remedy| remedy.contains("Install `uv`"))
    );
    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(output.status.attention_items.iter().any(|item| {
        item.contains("Project setup readiness is blocked") && item.contains("`uv`")
    }));
}

struct SetupReceiptFixture<'a> {
    id: Option<&'a str>,
    workspace_id: &'a WorkspaceId,
    project_id: &'a ProjectId,
    cwd: &'a str,
    state: &'a str,
    recipe_hash: Option<&'a str>,
    setup_identity_hash: &'a str,
    readiness_state: &'a str,
    readiness_reason: &'a str,
    readiness_remedy: &'a str,
}

fn setup_receipt_record(fixture: SetupReceiptFixture<'_>) -> SetupReceiptRecord {
    SetupReceiptRecord {
        id: fixture
            .id
            .map(str::to_string)
            .unwrap_or_else(|| format!("setup_{}_{}", fixture.state, fixture.setup_identity_hash)),
        workspace_id: fixture.workspace_id.clone(),
        project_id: Some(fixture.project_id.clone()),
        command: "setup command".to_string(),
        state: fixture.state.to_string(),
        recipe_hash: fixture.recipe_hash.unwrap_or("inferred").to_string(),
        approval_state: "not-required".to_string(),
        trigger: "test".to_string(),
        cwd: fixture.cwd.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        env_profile: "default".to_string(),
        output_path: None,
        redacted_summary: fixture.readiness_reason.to_string(),
        setup_identity_hash: fixture.setup_identity_hash.to_string(),
        readiness_state: fixture.readiness_state.to_string(),
        readiness_reason: fixture.readiness_reason.to_string(),
        readiness_remedy: fixture.readiness_remedy.to_string(),
        receipt_json: "{}".to_string(),
        updated_at: "2026-07-03T12:00:00Z".to_string(),
    }
}

fn recipe_command_receipt_id(
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    recipe: &crate::setup::SetupRecipe,
    command_index: usize,
) -> String {
    let command = &recipe.commands[command_index];
    let receipt_key =
        crate::setup::recipe_receipt_key(command, &recipe.recipe_hash).expect("receipt key");
    crate::setup::setup_receipt_id(workspace_id, project_id, &recipe.recipe_hash, &receipt_key)
}

fn inferred_command_receipt_id(
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    command: &crate::setup::SetupCommandPlan,
) -> String {
    let command_text = command.command.join(" ");
    let receipt_key =
        crate::setup::inferred_receipt_key(command, &command_text).expect("receipt key");
    let recipe_hash = crate::setup::inferred_recipe_hash(command);
    crate::setup::setup_receipt_id(workspace_id, project_id, &recipe_hash, &receipt_key)
}
