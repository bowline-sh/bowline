use super::*;

#[test]
fn sync_runner_persists_fresh_scan_metadata_for_status_and_work_views() {
    let workspace = TempWorkspace::new("sync-persists-scan-metadata").expect("workspace");
    let state = TempWorkspace::new("sync-persists-scan-state").expect("state");
    let project = workspace.root().join("app");
    fs::create_dir_all(project.join(".git")).expect("git marker");
    fs::write(project.join("README.md"), b"hello\n").expect("readme");
    fs::write(project.join(".env.local"), b"SECRET=value\n").expect("env");

    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-29T04:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &workspace.root().display().to_string(),
            "2026-06-29T04:00:00Z",
        )
        .expect("root");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: empty_workspace_ref(workspace_id.clone()),
            observed_at: "2026-06-29T04:00:00Z".to_string(),
        })
        .expect("head");
    drop(store);

    let candidate = crate::sync::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [7_u8; 32],
        "2026-06-29T04:01:00Z",
    )
    .expect("candidate");
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T04:01:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    runner
        .persist_scan_metadata(&candidate, Some(&candidate.snapshot))
        .expect("scan metadata persisted");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let summary = store
        .observed_summary(&workspace_id)
        .expect("summary")
        .expect("summary present");
    assert_eq!(summary.repo_count, 1);
    assert_eq!(summary.env_file_count, 1);
    assert_eq!(
        store
            .current_project_by_path(&project.display().to_string())
            .expect("project lookup")
            .expect("project")
            .path,
        "app"
    );
    let project = store
        .current_project_by_path(&project.display().to_string())
        .expect("project lookup")
        .expect("project");
    assert!(project.id.as_str().contains(workspace_id.as_str()));
    assert_eq!(
        store
            .project_latest_snapshot_id(&workspace_id, &project.id)
            .expect("latest snapshot"),
        Some(candidate.snapshot.manifest.snapshot_id.clone())
    );
    let retained_snapshot = store
        .snapshot(&workspace_id, &candidate.snapshot.manifest.snapshot_id)
        .expect("snapshot lookup")
        .expect("retained snapshot");
    assert_eq!(
        retained_snapshot.project_id,
        candidate.snapshot.manifest.project_id
    );
    assert_eq!(
        retained_snapshot.root_id,
        candidate.snapshot.manifest.namespace_root_id
    );
    assert_eq!(
        retained_snapshot.entry_count,
        candidate.snapshot.manifest.entry_count
    );
    assert_eq!(
        store
            .env_records(&workspace_id)
            .expect("env records")
            .into_iter()
            .map(|record| record.key_name)
            .collect::<Vec<_>>(),
        vec!["SECRET".to_string()]
    );
}

#[test]
fn partial_root_shallow_persist_refreshes_root_env_and_preserves_deep_status() {
    let workspace = TempWorkspace::new("sync-partial-persist-ws").expect("workspace");
    let state = TempWorkspace::new("sync-partial-persist-state").expect("state");
    let project = workspace.root().join("app");
    fs::create_dir_all(project.join(".git")).expect("git marker");
    fs::write(project.join("README.md"), b"hello\n").expect("readme");
    fs::write(project.join(".env.local"), b"DEEP=value\n").expect("deep env");
    fs::write(workspace.root().join(".env"), b"ROOT=value\n").expect("root env");

    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-06T04:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &workspace.root().display().to_string(),
            "2026-07-06T04:00:00Z",
        )
        .expect("root");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: empty_workspace_ref(workspace_id.clone()),
            observed_at: "2026-07-06T04:00:00Z".to_string(),
        })
        .expect("head");
    drop(store);

    let runner = test_runner(&workspace, &state, workspace_id.clone());
    let full = crate::sync::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [7_u8; 32],
        "2026-07-06T04:01:00Z",
    )
    .expect("full candidate");
    runner
        .persist_scan_metadata(&full, Some(&full.snapshot))
        .expect("full persist");

    // Baseline: a full scan recorded both env sources and the deep repo summary.
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let baseline_summary = store
        .observed_summary(&workspace_id)
        .expect("summary")
        .expect("summary present");
    assert_eq!(baseline_summary.repo_count, 1);
    assert_eq!(baseline_summary.env_file_count, 2);
    drop(store);

    // A root-shallow tick observes only root children; it must refresh the root
    // `.env` without erasing the deep `app/.env.local` env records or the deep
    // repo/status summary it never looked at.
    fs::write(workspace.root().join(".env"), b"ROOT=updated\n").expect("root env update");
    let mut session = StatCacheSession::empty_for_scan(1, &[7_u8; 32]);
    let shallow = crate::sync::coalescer::coalesce_workspace_scan_cached(
        crate::sync::coalescer::CoalesceScanRequest {
            root: workspace.root(),
            workspace_id: workspace_id.clone(),
            base_ref: &empty_workspace_ref(workspace_id.clone()),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            created_at: "2026-07-06T04:02:00Z".to_string(),
            context: crate::sync::coalescer::CoalesceContext::empty(),
            stat_cache: Some(&mut session),
            scan_scope: ScanScope::RootShallow,
        },
    )
    .expect("shallow candidate");
    runner
        .persist_scan_metadata(&shallow, Some(&shallow.snapshot))
        .expect("shallow persist");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    // Deep status facts survive the partial pass untouched.
    let summary = store
        .observed_summary(&workspace_id)
        .expect("summary")
        .expect("summary present");
    assert_eq!(summary.repo_count, 1, "deep repo status preserved");
    assert_eq!(summary.env_file_count, 2, "deep env status preserved");
    // Both env sources remain; the deep one is not blanked out by the root pass.
    let sources = store
        .env_records(&workspace_id)
        .expect("env records")
        .into_iter()
        .map(|record| record.source_path)
        .collect::<BTreeSet<_>>();
    assert!(
        sources.contains(".env"),
        "root env refreshed, got {sources:?}"
    );
    assert!(
        sources.contains("app/.env.local"),
        "deep env preserved, got {sources:?}"
    );
}

#[test]
fn persisted_head_manifest_after_upload_contains_locators() {
    let workspace = TempWorkspace::new("sync-bound-manifest-persist").expect("workspace");
    let state = TempWorkspace::new("sync-bound-manifest-state").expect("state");
    fs::write(workspace.root().join("README.md"), b"hello\n").expect("readme");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T10:00:00Z")
        .expect("workspace");
    drop(store);
    let candidate = crate::sync::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [7_u8; 32],
        "2026-07-03T10:01:00Z",
    )
    .expect("candidate");
    let mut bound_snapshot = candidate.snapshot.clone();
    add_bound_locator(&mut bound_snapshot, "README.md", "pk_0011223344556677");
    let runner = test_runner(&workspace, &state, workspace_id.clone());
    let accepted = accepted_ref(
        &workspace_id,
        candidate.snapshot.manifest.snapshot_id.as_str(),
    );

    runner
        .persist_scan_metadata_if_committed(&candidate, &accepted, Some(&bound_snapshot))
        .expect("persist bound snapshot");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let retained_snapshot = store
        .snapshot(&workspace_id, &candidate.snapshot.manifest.snapshot_id)
        .expect("snapshot lookup")
        .expect("retained snapshot");
    assert_eq!(
        retained_snapshot.root_id,
        bound_snapshot.manifest().namespace_root_id
    );
    let retained_entry = store
        .current_namespace_entry(&workspace_id, &WorkspaceRelativePath::new("README.md"))
        .expect("projection lookup")
        .expect("stored entry");
    assert!(retained_entry.content_layout_id.is_some());
    assert_eq!(retained_entry.hydration_state, HydrationState::Local);
}

#[test]
fn local_head_commit_enqueues_one_durable_overlay_operation() {
    let workspace = TempWorkspace::new("sync-post-commit-followup-workspace").expect("workspace");
    let state = TempWorkspace::new("sync-post-commit-followup-state").expect("state");
    let workspace_id = WorkspaceId::new("ws_code");
    let generated_at = "2026-07-05T12:31:00Z";
    let operation_id = "sync_post_commit_followup";
    let mut store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-05T12:30:00Z")
        .expect("workspace");
    let root_path = workspace.root().display().to_string();
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &root_path,
            "2026-07-05T12:30:00Z",
        )
        .expect("root");
    store
        .replace_projects(
            &workspace_id,
            "root_code",
            &[ProjectUpsert {
                id: ProjectId::new("project_web"),
                path: "apps/web".to_string(),
                git_observer_state: bowline_core::status::GitObserverState::Ok,
            }],
            "2026-07-05T12:30:00Z",
        )
        .expect("project");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: crate::metadata::SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: operation_id.to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device_local")),
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
            created_at: generated_at.to_string(),
            updated_at: generated_at.to_string(),
        })
        .expect("operation");
    let sync_claim = store
        .claim_next_sync_operation(
            &workspace_id,
            "test-runner",
            "2026-07-05T12:30:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim operation")
        .expect("queued operation")
        .claim;
    drop(store);
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: generated_at.to_string(),
            sync_claim: Some(sync_claim),
            scan_scope: Default::default(),
        },
    );
    let workspace_ref = accepted_ref(&workspace_id, "snap_followup");

    runner
        .complete_local_head(
            &workspace_ref,
            LocalHeadMetadataUpdate::FreshScan {
                bound_snapshot: None,
            },
        )
        .expect("committed local head enqueues overlay operation");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    assert!(
        store
            .workspace_sync_head(&workspace_id)
            .expect("local head")
            .is_some()
    );
    let operations = store.sync_operations(&workspace_id).expect("operations");
    let overlay = operations
        .iter()
        .find(|operation| operation.kind == SyncOperationKind::WorkViewOverlaySync)
        .expect("overlay operation");
    assert_eq!(overlay.state, SyncOperationState::Queued);
    assert_eq!(
        overlay.resource_key,
        SyncResourceKey::post_commit(workspace_id.clone())
    );
    let input = crate::sync::decode_work_view_overlay_sync_operation(overlay)
        .expect("typed overlay payload");
    assert_eq!(input.workspace_version, workspace_ref.version);
    assert_eq!(input.snapshot_id, workspace_ref.snapshot_id);
    drop(store);

    runner
        .complete_local_head(
            &workspace_ref,
            LocalHeadMetadataUpdate::FreshScan {
                bound_snapshot: None,
            },
        )
        .expect("repeated committed local head deduplicates overlay operation");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    assert_eq!(
        store
            .sync_operations(&workspace_id)
            .expect("operations")
            .iter()
            .filter(|operation| operation.kind == SyncOperationKind::WorkViewOverlaySync)
            .count(),
        1
    );
}

#[test]
fn cancellation_after_materialization_persists_local_head_as_committed_late() {
    let workspace =
        TempWorkspace::new("sync-post-materialization-cancel-workspace").expect("workspace");
    let state = TempWorkspace::new("sync-post-materialization-cancel-state").expect("state");
    let workspace_id = WorkspaceId::new("ws_materialized_cancel");
    let generated_at = "2026-07-13T11:00:00Z";
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "sync_materialized_cancel".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "sync-materialized-cancel".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device_local")),
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
            created_at: generated_at.to_string(),
            updated_at: generated_at.to_string(),
        })
        .expect("operation");
    let claim = store
        .claim_next_sync_operation(
            &workspace_id,
            "test-runner",
            "2026-07-13T11:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("operation")
        .claim;
    store
        .request_sync_operation_cancellation(claim.operation_id(), "2026-07-13T11:00:02Z")
        .expect("cancellation request");
    drop(store);
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: generated_at.to_string(),
            sync_claim: Some(claim),
            scan_scope: Default::default(),
        },
    );
    let workspace_ref = accepted_ref(&workspace_id, "snap_materialized_cancel");
    runner
        .authorize_materialization(&workspace_ref, MaterializationBoundary::AfterMutation)
        .expect("record irreversible materialization effect");
    runner
        .authorize_materialization(&workspace_ref, MaterializationBoundary::BeforeMutation)
        .expect("post-effect cancellation stays on reconciliation path");

    runner
        .complete_local_head(
            &workspace_ref,
            LocalHeadMetadataUpdate::FreshScan {
                bound_snapshot: None,
            },
        )
        .expect("irreversible materialization reconciles local head");

    assert!(runner.cancellation_requested_after_commit());
    assert_eq!(
        MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE))
            .expect("store")
            .workspace_sync_head(&workspace_id)
            .expect("local head")
            .expect("persisted local head")
            .workspace_ref,
        workspace_ref
    );
}

#[test]
fn projected_nodes_remain_local_after_reuse_tick() {
    let workspace = TempWorkspace::new("sync-projected-local-after-reuse").expect("workspace");
    let state = TempWorkspace::new("sync-projected-local-state").expect("state");
    fs::write(workspace.root().join("README.md"), b"hello\n").expect("readme");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T10:00:00Z")
        .expect("workspace");
    drop(store);
    let candidate = crate::sync::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [7_u8; 32],
        "2026-07-03T10:01:00Z",
    )
    .expect("candidate");
    let mut bound_snapshot = candidate.snapshot.clone();
    add_bound_locator(&mut bound_snapshot, "README.md", "pk_8899aabbccddeeff");
    let runner = test_runner(&workspace, &state, workspace_id.clone());
    let accepted = accepted_ref(
        &workspace_id,
        candidate.snapshot.manifest.snapshot_id.as_str(),
    );

    runner
        .persist_scan_metadata_if_committed(&candidate, &accepted, Some(&bound_snapshot))
        .expect("persist metadata");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let readme = store
        .current_namespace_entry(&workspace_id, &WorkspaceRelativePath::new("README.md"))
        .expect("projected readme")
        .expect("projected readme");
    assert_eq!(readme.hydration_state, HydrationState::Local);
}

#[test]
fn fresh_head_metadata_scan_can_store_bound_manifest() {
    let workspace = TempWorkspace::new("sync-fresh-bound-manifest").expect("workspace");
    let state = TempWorkspace::new("sync-fresh-bound-state").expect("state");
    fs::write(workspace.root().join("README.md"), b"hello\n").expect("readme");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-03T10:00:00Z")
        .expect("workspace");
    drop(store);
    let candidate = crate::sync::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [7_u8; 32],
        "2026-07-03T10:01:00Z",
    )
    .expect("candidate");
    let mut bound_snapshot = candidate.snapshot.clone();
    add_bound_locator(&mut bound_snapshot, "README.md", "pk_1020304050607080");
    let runner = test_runner(&workspace, &state, workspace_id.clone());
    let accepted = accepted_ref(
        &workspace_id,
        candidate.snapshot.manifest.snapshot_id.as_str(),
    );

    runner
        .persist_fresh_scan_metadata_for_head(&accepted, Some(&bound_snapshot))
        .expect("fresh metadata persisted with bound snapshot");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let retained_snapshot = store
        .snapshot(&workspace_id, &candidate.snapshot.manifest.snapshot_id)
        .expect("snapshot lookup")
        .expect("retained snapshot");
    assert_eq!(
        retained_snapshot.root_id,
        bound_snapshot.manifest().namespace_root_id
    );
    let readme = store
        .current_namespace_entry(&workspace_id, &WorkspaceRelativePath::new("README.md"))
        .expect("projected readme")
        .expect("projected readme");
    assert_eq!(readme.hydration_state, HydrationState::Local);
}

#[test]
fn sync_runner_skips_scan_metadata_for_uncommitted_candidate() {
    let workspace = TempWorkspace::new("sync-skips-uncommitted-scan-metadata").expect("workspace");
    let state = TempWorkspace::new("sync-skips-uncommitted-scan-state").expect("state");
    let project = workspace.root().join("app");
    fs::create_dir_all(project.join(".git")).expect("git marker");
    fs::write(project.join("README.md"), b"local-only\n").expect("readme");

    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-29T04:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &workspace.root().display().to_string(),
            "2026-06-29T04:00:00Z",
        )
        .expect("root");
    drop(store);

    let candidate = crate::sync::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [7_u8; 32],
        "2026-06-29T04:01:00Z",
    )
    .expect("candidate");
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T04:01:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    let accepted_remote = WorkspaceRef {
        workspace_id: WorkspaceId::new(workspace_id.as_str()),
        version: 7,
        snapshot_id: SnapshotId::new("snap_remote_committed"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 7 },
        updated_by_device_id: Some(DeviceId::new("device_remote")),
    };

    runner
        .persist_scan_metadata_if_committed(&candidate, &accepted_remote, None)
        .expect("mismatched scan metadata is skipped");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    assert!(
        store
            .observed_summary(&workspace_id)
            .expect("summary lookup")
            .is_none()
    );
    assert!(
        store
            .current_project_by_path(&project.display().to_string())
            .expect("project lookup")
            .is_none()
    );
}

#[test]
fn sync_runner_rejects_stale_env_file_before_committing_scan_metadata() {
    let workspace = TempWorkspace::new("sync-stale-env-metadata").expect("workspace");
    let state = TempWorkspace::new("sync-stale-env-state").expect("state");
    let project = workspace.root().join("app");
    fs::create_dir_all(project.join(".git")).expect("git marker");
    fs::write(project.join(".env"), b"SHARED=value\n").expect("env");
    fs::write(project.join(".env.local"), b"SECRET=value\n").expect("env");

    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-29T04:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &workspace.root().display().to_string(),
            "2026-06-29T04:00:00Z",
        )
        .expect("root");
    drop(store);

    let candidate = crate::sync::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [7_u8; 32],
        "2026-06-29T04:01:00Z",
    )
    .expect("candidate");
    fs::remove_file(project.join(".env.local")).expect("remove stale env");
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T04:01:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    let accepted = WorkspaceRef {
        workspace_id: WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: SnapshotId::new(candidate.snapshot.manifest.snapshot_id.as_str()),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(DeviceId::new("device_local")),
    };
    let bound_snapshot = candidate.snapshot.clone();

    let prepared = runner
        .prepare_local_head_metadata_update(
            &accepted,
            LocalHeadMetadataUpdate::CommittedScan {
                candidate: &candidate,
                bound_snapshot: Some(&bound_snapshot),
            },
        )
        .expect("prepare committed scan metadata");
    let error = runner
        .commit_local_head_metadata(&accepted, prepared)
        .expect_err("stale env metadata import rejects committed local-head persistence");
    assert!(error.to_string().contains(".env.local"));

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    assert!(
        store
            .workspace_sync_head(&workspace_id)
            .expect("local head")
            .is_none()
    );
    assert!(
        store
            .observed_summary(&workspace_id)
            .expect("summary lookup")
            .is_none()
    );
    assert!(
        store
            .env_records(&workspace_id)
            .expect("env records")
            .is_empty()
    );
}
