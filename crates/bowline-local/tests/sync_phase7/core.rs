use super::*;

#[test]
fn dirty_untracked_workspace_uploads_packed_snapshot_and_imports_structure_first() {
    let workspace = TempWorkspace::new("phase7-source").expect("source root");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    workspace
        .write_project_file("app", ".env.local", b"SECRET=value\n")
        .expect("env");
    workspace
        .create_generated_folder("app", "node_modules")
        .expect("generated");

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let state = TempWorkspace::new("phase7-state").expect("state root");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 10).expect("byte store");
    let workspace_id = WorkspaceId::new("ws_code");
    let storage_key = StorageKey::deterministic(7);
    let content_key = [7_u8; 32];

    let candidate = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-linux"),
        content_key,
        "2026-06-24T12:00:00Z",
    )
    .expect("candidate");

    assert!(
        snapshot_entries(&candidate.snapshot)
            .iter()
            .any(|entry| entry.path == "app/src/main.ts")
    );
    assert!(
        snapshot_entries(&candidate.snapshot)
            .iter()
            .any(|entry| entry.path == "app/.env.local")
    );
    assert!(
        !snapshot_entries(&candidate.snapshot)
            .iter()
            .any(|entry| entry.path.contains("node_modules"))
    );

    let outcome =
        upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
            .expect("upload");
    let snapshot_root = match outcome {
        bowline_local::sync::UploadOutcome::Advanced {
            workspace_ref,
            snapshot_root,
            bound_snapshot,
            ..
        } => {
            assert_eq!(
                workspace_ref.snapshot_id,
                candidate.snapshot.manifest().snapshot_id.as_str()
            );
            let bound_snapshot = bound_snapshot.expect("bound page snapshot");
            assert_eq!(
                snapshot_root.namespace_root_id,
                bound_snapshot.manifest().namespace_root_id.as_str()
            );
            snapshot_root
        }
        bowline_local::sync::UploadOutcome::Stale { .. } => {
            panic!("first writer should advance")
        }
    };

    assert!(
        snapshot_root
            .manifest_object
            .object_key
            .starts_with("manifests_mf_")
    );
    assert!(
        byte_store
            .list_object_keys()
            .expect("stored object keys")
            .iter()
            .any(|key| key.as_str().starts_with("packs_pk_") && !key.as_str().contains("main"))
    );

    let (_, remote) = open_remote_snapshot_by_id(
        &workspace_id,
        &candidate.snapshot.manifest().snapshot_id,
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&workspace_id, content_key),
    )
    .expect("open remote page authority");
    let reads_before_budget_rejection = byte_store.metrics().full_read_count;
    let mut zero_page_budget = NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(1, 0, 0).with_metadata_limits(0, 0, 0, 0),
    );
    assert!(matches!(
        remote.reader().get(
            &WorkspaceRelativePath::new("app/src/main.ts"),
            &mut zero_page_budget,
        ),
        Err(NamespaceReadError::BudgetExceeded { .. })
    ));
    assert_eq!(
        byte_store.metrics().full_read_count,
        reads_before_budget_rejection,
        "page/byte budget rejection must happen before metadata object I/O"
    );

    let imported = import_snapshot_by_id(
        &workspace_id,
        &candidate.snapshot.manifest().snapshot_id,
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&workspace_id, content_key),
    )
    .expect("import");

    assert_eq!(
        imported.snapshot.manifest().snapshot_id,
        candidate.snapshot.manifest().snapshot_id
    );
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .filter(|entry| entry.kind == NamespaceEntryKind::File)
            .all(|entry| entry.content_layout.is_some())
    );
    assert!(byte_store.metrics().full_read_count > 0);
    assert!(byte_store.metrics().full_read_count <= 16);
    assert_eq!(byte_store.metrics().range_read_count, 0);
    let binding_requests = control_plane.metadata_binding_resolution_requests();
    assert!(
        binding_requests
            .iter()
            .all(|batch| !batch.is_empty() && batch.len() <= 16)
    );
    assert!(
        binding_requests.iter().any(|batch| batch.len() > 1),
        "full import should batch anticipated metadata children"
    );

    let retry = upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
        .expect("committed object retry");
    match retry {
        bowline_local::sync::UploadOutcome::Advanced { .. } => {
            panic!("retrying the same base should observe the advanced workspace ref")
        }
        bowline_local::sync::UploadOutcome::Stale {
            stale,
            snapshot_root: retried_root,
            ..
        } => {
            assert_eq!(
                stale.current.snapshot_id,
                candidate.snapshot.manifest().snapshot_id.as_str()
            );
            assert_eq!(retried_root, snapshot_root);
        }
    }
}

#[test]
fn coalesced_snapshot_id_is_stable_for_unchanged_workspace_content() {
    let workspace = TempWorkspace::new("phase7-stable-snapshot-id").expect("workspace");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    let workspace_id = WorkspaceId::new("ws_code");
    let first_base = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_base_a"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let first = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &first_base,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:00:00Z",
    )
    .expect("first candidate");
    let second_base = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 2,
        snapshot_id: bowline_core::ids::SnapshotId::new(
            first.snapshot.manifest().snapshot_id.as_str(),
        ),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
    };
    let second = coalesce_workspace_scan(
        workspace.root(),
        workspace_id,
        &second_base,
        DeviceId::new("device-b"),
        [3_u8; 32],
        "2026-06-24T12:01:00Z",
    )
    .expect("second candidate");

    assert_eq!(
        second.snapshot.manifest().snapshot_id,
        first.snapshot.manifest().snapshot_id,
        "unchanged workspace content must not create a new snapshot ID just because the base ref or device changed"
    );
}

#[test]
fn retry_after_manifest_commit_reuses_side_effects_and_advances_ref() {
    let workspace = TempWorkspace::new("phase7-commit-before-cas-workspace").expect("workspace");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");

    let workspace_id = WorkspaceId::new("ws_code");
    let inner = FakeControlPlaneClient::default();
    let base_ref = inner.create_workspace("ws_code");
    let control_plane = CasFailsOnceControlPlane::new(inner);
    let state = TempWorkspace::new("phase7-commit-before-cas-state").expect("state");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 30).expect("byte store");
    let storage_key = StorageKey::deterministic(30);
    let candidate = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-a"),
        [30_u8; 32],
        "2026-06-24T12:09:00Z",
    )
    .expect("candidate");

    let first_error =
        upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
            .expect_err("first CAS fails after snapshot root commit");
    assert!(
        first_error.to_string().contains("injected CAS failure"),
        "unexpected first error: {first_error}"
    );
    assert_eq!(
        control_plane
            .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
            .expect("workspace ref")
            .expect("workspace exists")
            .version,
        base_ref.version,
        "the failed attempt must not claim the workspace advanced"
    );
    let committed_root = control_plane
        .get_snapshot_root(
            &bowline_core::ids::WorkspaceId::new("ws_code"),
            &candidate.snapshot.manifest().snapshot_id,
        )
        .expect("snapshot root lookup")
        .expect("root committed before CAS failure");
    let first_put_count = byte_store.metrics().put_count;
    assert!(first_put_count > 0, "first attempt should upload objects");

    let retry = upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
        .expect("retry succeeds");
    let bowline_local::sync::UploadOutcome::Advanced {
        workspace_ref,
        snapshot_root,
        ..
    } = retry
    else {
        panic!("retry should advance the original local edit after transient CAS failure");
    };
    assert_eq!(
        workspace_ref.snapshot_id,
        candidate.snapshot.manifest().snapshot_id.as_str()
    );
    assert_eq!(workspace_ref.version, base_ref.version + 1);
    assert_eq!(snapshot_root, committed_root);
    assert_eq!(
        byte_store.metrics().put_count,
        first_put_count,
        "retry should reuse committed packs, metadata bindings, and root without re-uploading"
    );
}

#[test]
fn resolved_conflict_metadata_is_not_cleared_before_failed_upload_is_durable() {
    let workspace = TempWorkspace::new("phase7-resolved-clear-after-upload").expect("workspace");
    let state = TempWorkspace::new("phase7-resolved-clear-after-upload-state").expect("state");
    workspace
        .write_project_file("app", "config.toml", b"value = \"resolved\"\n")
        .expect("resolved file");

    let inner = FakeControlPlaneClient::default();
    let base_ref = inner.create_workspace("ws_code");
    let control_plane = CasFailsOnceControlPlane::new(inner);
    let bundle = create_conflict_bundle(
        state.root(),
        canonical_conflict_occurrence(
            ConflictRecord::same_path("app/config.toml"),
            "empty",
            base_ref.snapshot_id.as_str(),
        ),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"local\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("bundle");
    control_plane
        .reconcile_conflict_occurrence(bowline_control_plane::ConflictOccurrenceReconcile {
            workspace_id: bowline_core::ids::WorkspaceId::new("ws_code"),
            conflict_id: bowline_core::ids::ConflictId::new(bundle.record.id.clone()),
            conflict_kind: "text".to_string(),
            paths: bundle.record.paths.clone(),
            contains_secrets: false,
            base_snapshot_id: bowline_core::ids::SnapshotId::new("empty"),
            remote_snapshot_id: base_ref.snapshot_id.clone(),
            occurrence_version: 1,
            desired_state: bowline_control_plane::ConflictOccurrenceState::Unresolved,
            device_id: bowline_core::ids::DeviceId::new("device-a"),
            reason: "same-path-edit".to_string(),
            bundle_object: None,
        })
        .expect("publish conflict metadata");
    assert!(
        mark_conflict_occurrence_reconciled(
            state.root(),
            &bundle.record.id,
            bundle.record.occurrence_version,
            bowline_local::sync::ConflictState::Unresolved,
            "2026-06-24T12:09:29Z",
        )
        .expect("record durable conflict publication")
    );
    mark_only_conflict_bundle_state(state.root(), "accepted");

    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 31).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [31_u8; 32],
            storage_key: StorageKey::deterministic(31),
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:30Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let error = runner
        .tick()
        .expect_err("upload fails before resolution can be durable");
    assert!(error.to_string().contains("injected CAS failure"));
    assert_eq!(
        control_plane
            .list_workspace_conflicts(
                &bowline_core::ids::WorkspaceId::new("ws_code"),
                &bowline_core::ids::DeviceId::new("device-a")
            )
            .expect("conflict metadata remains unresolved")
            .len(),
        1
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.root.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert!(
        manifest.get("remoteResolutionSyncedAt").is_none(),
        "failed durable sync must not locally acknowledge remote resolution"
    );
}

#[test]
fn resolved_conflict_without_prior_publish_publishes_then_clears_after_durable_upload() {
    let workspace = TempWorkspace::new("phase7-resolved-publish-before-clear").expect("workspace");
    let state = TempWorkspace::new("phase7-resolved-publish-before-clear-state").expect("state");
    workspace
        .write_project_file("app", "config.toml", b"value = \"resolved\"\n")
        .expect("resolved file");

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let mut record = ConflictRecord::same_path("app/config.toml");
    record.base_snapshot_id = Some("empty".to_string());
    record.remote_snapshot_id = Some(base_ref.snapshot_id.as_str().to_string());
    let bundle = create_conflict_bundle(
        state.root(),
        record,
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"local\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("bundle");
    mark_only_conflict_bundle_state(state.root(), "accepted");

    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 33).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [33_u8; 32],
            storage_key: StorageKey::deterministic(33),
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:45Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    assert!(matches!(
        runner
            .tick()
            .expect("resolution upload queues conflict publication"),
        SyncTickOutcome::Uploaded(_)
    ));
    assert_eq!(
        drive_pending_conflict_occurrences(
            state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:09:46Z",
        )
        .expect("publish durable conflict occurrence"),
        1
    );
    assert_eq!(
        runner.tick().expect("queue accepted conflict resolution"),
        SyncTickOutcome::NoChanges
    );
    assert_eq!(
        drive_pending_conflict_occurrences(
            state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:09:47Z",
        )
        .expect("publish durable accepted resolution"),
        1
    );
    assert_eq!(
        control_plane
            .list_workspace_conflicts(
                &bowline_core::ids::WorkspaceId::new("ws_code"),
                &bowline_core::ids::DeviceId::new("device-a")
            )
            .expect("conflict metadata cleared")
            .len(),
        0
    );
    let events = control_plane
        .list_events(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("events");
    assert!(
        events
            .iter()
            .any(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictDetected)
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictResolved)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.root.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert!(manifest.get("remoteConflictPublishedAt").is_some());
    assert!(manifest.get("remoteResolutionSyncedAt").is_some());
}

#[test]
fn runner_noops_when_workspace_content_already_matches_head() {
    let workspace = TempWorkspace::new("phase7-runner-noop-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-runner-noop-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 14).expect("byte store");
    let storage_key = StorageKey::deterministic(14);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [14_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:07:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let first = runner.tick().expect("first tick");
    assert!(
        matches!(first, SyncTickOutcome::Uploaded(_)),
        "first dirty workspace tick should upload, got {first:?}"
    );
    let first_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");

    let second = runner.tick().expect("second tick");
    assert_eq!(second, SyncTickOutcome::NoChanges);
    let second_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");
    assert_eq!(second_ref.version, first_ref.version);
    assert_eq!(second_ref.snapshot_id, first_ref.snapshot_id);
    let preparation_root = state.root().join("preparations");
    if preparation_root.exists() {
        assert_eq!(
            std::fs::read_dir(&preparation_root)
                .expect("preparation directory")
                .count(),
            0,
            "terminal preparations must release owned staged files"
        );
    }
    let metadata = MetadataStore::open(state.root().join("local.sqlite3")).expect("metadata");
    for lease in metadata
        .preparation_leases(&WorkspaceId::new("ws_code"))
        .expect("preparation leases")
    {
        assert!(
            metadata
                .prepared_staged_content(&lease.id, &lease.owner_marker)
                .expect("staged content")
                .is_empty(),
            "terminal preparation metadata must be forgotten after cleanup"
        );
    }
}

#[test]
fn runner_records_upload_side_effect_checkpoints_for_claimed_operation() {
    let workspace = TempWorkspace::new("phase7-runner-checkpoint-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-runner-checkpoint-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");

    let workspace_id = WorkspaceId::new("ws_code");
    let operation_id = "op-daemon-checkpoint".to_string();
    let metadata = MetadataStore::open(state.root().join("local.sqlite3")).expect("metadata");
    metadata
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.clone(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "daemon-reconcile:checkpoint".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
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
            created_at: "2026-06-24T12:08:00Z".to_string(),
            updated_at: "2026-06-24T12:08:00Z".to_string(),
        })
        .expect("operation");
    let sync_claim = metadata
        .claim_next_sync_operation(
            &workspace_id,
            "device-a",
            "2026-07-13T12:08:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim operation")
        .expect("queued operation")
        .claim;
    drop(metadata);

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 29).expect("byte store");
    let storage_key = StorageKey::deterministic(29);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [29_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-07-13T12:08:01Z".to_string(),
            sync_claim: Some(sync_claim),
            scan_scope: Default::default(),
        },
    );

    assert!(matches!(
        runner.tick().expect("upload tick"),
        SyncTickOutcome::Uploaded(_)
    ));

    let checkpoints = MetadataStore::open(state.root().join("local.sqlite3"))
        .expect("metadata")
        .sync_operation_checkpoints(&operation_id)
        .expect("checkpoints");
    let steps = checkpoints
        .iter()
        .map(|checkpoint| checkpoint.step.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "remote-ref-observed",
        "snapshot-candidate-built",
        "source-packs-written",
        "source-pack-uploaded",
        "snapshot-root-committed",
        "workspace-ref-cas-authorized",
        "workspace-ref-advanced",
    ] {
        assert!(
            steps.contains(&expected),
            "missing checkpoint {expected}; got {steps:?}"
        );
    }
    assert!(
        checkpoints
            .iter()
            .all(|checkpoint| checkpoint.workspace_id == workspace_id)
    );
}

#[test]
fn stale_same_line_edit_creates_conflict_instead_of_losing_work() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base_bytes = b"name = \"old\"\n";
    let local_bytes = b"name = \"local\"\n";
    let remote_bytes = b"name = \"remote\"\n";
    let base = snapshot_with_file(&workspace_id, "snap_base", "app/config.toml", base_bytes);
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let mut local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(&workspace_id, "snap_local", "app/config.toml", local_bytes),
    );
    local_candidate.base.version = 1;
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_remote",
        "app/config.toml",
        remote_bytes,
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 2,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_remote"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:03:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Clean(_) => panic!("same-line divergent edits must not auto-merge"),
        MergeOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].paths, vec!["app/config.toml"]);
            assert_eq!(
                conflicts[0].conflict_kind,
                bowline_local::sync::ConflictKind::Text
            );
            assert_eq!(conflicts[0].spans.len(), 1);
            assert_eq!(conflicts[0].spans[0].path, "app/config.toml");
            assert_eq!(conflicts[0].spans[0].local_start_line, 1);
            assert_eq!(conflicts[0].spans[0].remote_start_line, 1);
            assert_eq!(
                conflicts[0].state,
                bowline_local::sync::ConflictState::Unresolved
            );
        }
    }
}

#[test]
fn structured_json_merge_that_fails_validation_conflicts_without_markers() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_json_base",
        "app/config.json",
        b"{\n  \"a\": 1,\n  \"b\": 1\n}\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_json_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_json_local",
            "app/config.json",
            b"{\n  \"a\": 2,\n  \"b\": 1\n}\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_json_remote",
        "app/config.json",
        b"{\n  \"a\": 1,\n  \"b\": }\n}\n",
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 2,
        snapshot_id: bowline_core::ids::SnapshotId::new(remote.manifest().snapshot_id.as_str()),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-26T12:02:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Clean(candidate) => panic!(
            "invalid structured merge must not advance: {:?}",
            candidate.snapshot.read_file_for_path("app/config.json")
        ),
        MergeOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].paths, vec!["app/config.json"]);
            assert_eq!(
                conflicts[0].conflict_kind,
                bowline_local::sync::ConflictKind::StructuredText
            );
            assert_eq!(
                conflicts[0].reason,
                "structured text merge did not validate"
            );
        }
    }
}

#[test]
fn structured_config_merge_that_fails_validation_conflicts_without_advancing() {
    for (path, base_bytes, local_bytes, remote_bytes) in [
        (
            "app/config.toml",
            b"a = 1\nb = 1\n".as_slice(),
            b"a = 2\nb = 1\n".as_slice(),
            b"a = 1\nb = \n".as_slice(),
        ),
        (
            "app/pnpm-lock.yaml",
            b"a: 1\nb: 1\n".as_slice(),
            b"a: 2\nb: 1\n".as_slice(),
            b"a: 1\nb: [\n".as_slice(),
        ),
        (
            "app/config.xml",
            b"<root>\n<a>1</a>\n<b>1</b>\n</root>\n".as_slice(),
            b"<root>\n<a>2</a>\n<b>1</b>\n</root>\n".as_slice(),
            b"<root>\n<a>1</a>\n<b>\n</root>\n".as_slice(),
        ),
    ] {
        let workspace_id = WorkspaceId::new(format!(
            "ws_structured_{}",
            path.replace(['/', '.', '-'], "_")
        ));
        let base = snapshot_with_file(&workspace_id, "snap_structured_base", path, base_bytes);
        let local_ref = bowline_control_plane::WorkspaceRef {
            workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
            version: 1,
            snapshot_id: bowline_core::ids::SnapshotId::new("snap_structured_base"),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
        };
        let local_candidate = coalesced_candidate_from_snapshot(
            &workspace_id,
            &local_ref,
            "device-a",
            snapshot_with_file(&workspace_id, "snap_structured_local", path, local_bytes),
        );
        let remote =
            snapshot_with_file(&workspace_id, "snap_structured_remote", path, remote_bytes);
        let remote_ref = bowline_control_plane::WorkspaceRef {
            workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
            version: 2,
            snapshot_id: bowline_core::ids::SnapshotId::new(remote.manifest().snapshot_id.as_str()),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
        };

        let outcome = merge_snapshots(
            &base,
            &local_candidate,
            &remote,
            bowline_local::sync::CandidateBase::from_remote(&remote_ref),
            [9_u8; 32],
            "2026-06-26T12:07:00Z",
        )
        .expect("merge");

        match outcome {
            MergeOutcome::Clean(candidate) => panic!(
                "invalid structured merge for {path} must not advance: {:?}",
                candidate.snapshot.read_file_for_path(path)
            ),
            MergeOutcome::Conflicted(conflicts) => {
                assert_eq!(conflicts.len(), 1, "{path}");
                assert_eq!(conflicts[0].paths, vec![path], "{path}");
                assert_eq!(
                    conflicts[0].conflict_kind,
                    bowline_local::sync::ConflictKind::StructuredText,
                    "{path}"
                );
                assert_eq!(
                    conflicts[0].reason, "structured text merge did not validate",
                    "{path}"
                );
            }
        }
    }
}

#[test]
fn stale_clean_merge_is_rebased_on_remote_head_for_retry() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_base",
        "app/config.toml",
        b"a = 1\nb = 1\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_local",
            "app/config.toml",
            b"a = 2\nb = 1\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_remote",
        "app/config.toml",
        b"a = 1\nb = 2\n",
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 2,
        snapshot_id: bowline_core::ids::SnapshotId::new(remote.manifest().snapshot_id.as_str()),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:04:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Conflicted(conflicts) => panic!("expected clean merge, got {conflicts:?}"),
        MergeOutcome::Clean(candidate) => {
            assert_eq!(candidate.base.version, 2);
            assert_eq!(candidate.base.snapshot_id, remote.manifest().snapshot_id);
            assert_eq!(
                candidate
                    .snapshot
                    .read_file_for_path("app/config.toml")
                    .expect("read merged bytes")
                    .expect("merged bytes"),
                b"a = 2\nb = 2\n"
            );
        }
    }
}

#[test]
fn stale_non_overlapping_insert_and_edit_auto_merge() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_base",
        "app/config.toml",
        b"a = 1\nb = 1\nc = 1\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_local",
            "app/config.toml",
            b"a = 1\ninserted = true\nb = 1\nc = 1\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_remote",
        "app/config.toml",
        b"a = 1\nb = 1\nc = 2\n",
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 2,
        snapshot_id: bowline_core::ids::SnapshotId::new(remote.manifest().snapshot_id.as_str()),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:05:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Conflicted(conflicts) => {
            panic!("expected independent insert/edit to merge cleanly, got {conflicts:?}")
        }
        MergeOutcome::Clean(candidate) => {
            assert_eq!(
                candidate
                    .snapshot
                    .read_file_for_path("app/config.toml")
                    .expect("read merged bytes")
                    .expect("merged bytes"),
                b"a = 1\ninserted = true\nb = 1\nc = 2\n"
            );
        }
    }
}

#[test]
fn stale_remote_delete_auto_merges_when_local_only_differs_by_transport_metadata() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_base",
        "app/config.toml",
        b"value = 1\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let local_snapshot = snapshot_with_file(
        &workspace_id,
        "snap_local",
        "app/config.toml",
        b"value = 1\n",
    );
    let mut local_entries = snapshot_entries(&local_snapshot);
    local_entries[0].content_layout = None;
    local_entries[0].hydration_state = HydrationState::Local;
    let local_snapshot_id =
        bowline_local::sync::rebuild_manifest_identity(&workspace_id, &local_entries, "test")
            .snapshot_id()
            .clone();
    let local_snapshot = SnapshotContent::from_prepared(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: local_snapshot_id,
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: local_entries,
            refs: Vec::new(),
        },
        local_snapshot.prepared_content().clone(),
        [9; 32],
    )
    .expect("page-backed local snapshot");
    let local_candidate =
        coalesced_candidate_from_snapshot(&workspace_id, &local_ref, "device-a", local_snapshot);
    let remote = empty_snapshot(&workspace_id, "snap_remote_deleted");
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 2,
        snapshot_id: bowline_core::ids::SnapshotId::new(remote.manifest().snapshot_id.as_str()),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:09:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Conflicted(conflicts) => {
            panic!("unchanged local file should accept remote delete, got {conflicts:?}")
        }
        MergeOutcome::Clean(candidate) => {
            assert!(
                snapshot_entries(&candidate.snapshot).is_empty(),
                "remote delete should win when local is semantically unchanged"
            );
        }
    }
}

#[test]
fn clean_merge_snapshot_id_includes_symlink_target_metadata() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_symlink(&workspace_id, "snap_base", "app/link", "target-a");
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_symlink(&workspace_id, "snap_local", "app/link", "target-a"),
    );

    let first = merge_snapshots(
        &base,
        &local_candidate,
        &snapshot_with_symlink(&workspace_id, "snap_remote_b", "app/link", "target-b"),
        bowline_local::sync::CandidateBase::from_remote(&bowline_control_plane::WorkspaceRef {
            workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
            version: 2,
            snapshot_id: bowline_core::ids::SnapshotId::new("snap_remote_b"),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
        }),
        [9_u8; 32],
        "2026-06-24T12:08:00Z",
    )
    .expect("first merge");
    let second = merge_snapshots(
        &base,
        &local_candidate,
        &snapshot_with_symlink(&workspace_id, "snap_remote_c", "app/link", "target-c"),
        bowline_local::sync::CandidateBase::from_remote(&bowline_control_plane::WorkspaceRef {
            workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
            version: 2,
            snapshot_id: bowline_core::ids::SnapshotId::new("snap_remote_c"),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-b")),
        }),
        [9_u8; 32],
        "2026-06-24T12:08:00Z",
    )
    .expect("second merge");

    let first_id = match first {
        MergeOutcome::Clean(candidate) => candidate.snapshot.manifest().snapshot_id.clone(),
        MergeOutcome::Conflicted(conflicts) => panic!("expected clean merge: {conflicts:?}"),
    };
    let second_id = match second {
        MergeOutcome::Clean(candidate) => candidate.snapshot.manifest().snapshot_id.clone(),
        MergeOutcome::Conflicted(conflicts) => panic!("expected clean merge: {conflicts:?}"),
    };

    assert_ne!(
        first_id, second_id,
        "clean non-file metadata changes must produce distinct merged snapshots"
    );
}

#[cfg(unix)]
#[test]
fn coalescer_syncs_symlink_shape_without_reading_target_bytes() {
    let workspace = TempWorkspace::new("phase7-symlink").expect("workspace");
    let outside = TempWorkspace::new("phase7-symlink-outside").expect("outside");
    workspace
        .write_file("package.json", b"{}")
        .expect("package");
    workspace
        .write_file("src/main.ts", b"export const value = 1;\n")
        .expect("source");
    outside
        .write_file("outside-secret.txt", b"do not upload\n")
        .expect("outside");
    workspace
        .create_symlink(
            "",
            "linked-secret.txt",
            outside.root().join("outside-secret.txt"),
        )
        .expect("symlink");
    workspace
        .create_symlink("", "linked-source.ts", "src/main.ts")
        .expect("relative symlink");
    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");

    let candidate = coalesce_workspace_scan(
        workspace.root(),
        WorkspaceId::new("ws_code"),
        &base_ref,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:05:00Z",
    )
    .expect("candidate");

    let candidate_entries = snapshot_entries(&candidate.snapshot);
    assert!(
        candidate_entries
            .iter()
            .all(|entry| entry.path != "linked-secret.txt"),
        "absolute out-of-workspace symlinks must stay local-only so peers never reject the snapshot"
    );
    let link = candidate_entries
        .iter()
        .find(|entry| entry.path == "linked-source.ts")
        .expect("link entry");
    assert_eq!(link.kind, NamespaceEntryKind::Symlink);
    assert_eq!(link.symlink_target.as_deref(), Some("src/main.ts"));
    assert!(
        !candidate
            .snapshot
            .prepared_content()
            .values()
            .filter_map(PreparedContent::resident_bytes)
            .any(|bytes| bytes == b"do not upload\n")
    );

    let first_snapshot_id = candidate.snapshot.manifest().snapshot_id.clone();
    fs::remove_file(workspace.root().join("linked-source.ts")).expect("remove old symlink");
    workspace
        .write_file("src/other.ts", b"export const other = 2;\n")
        .expect("second source");
    workspace
        .create_symlink("", "linked-source.ts", "src/other.ts")
        .expect("second symlink");

    let retargeted = coalesce_workspace_scan(
        workspace.root(),
        WorkspaceId::new("ws_code"),
        &base_ref,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:06:00Z",
    )
    .expect("retargeted candidate");

    assert_ne!(
        retargeted.snapshot.manifest().snapshot_id,
        first_snapshot_id,
        "changing only a symlink target must publish a distinct snapshot"
    );
}

#[test]
fn coalescer_excludes_bowline_private_state_from_upload() {
    let workspace = TempWorkspace::new("phase7-private-state").expect("workspace");
    workspace
        .write_file("package.json", b"{}")
        .expect("package");
    workspace
        .write_file(".bowline/conflicts/conflict_1/local/app.env", b"SECRET=1\n")
        .expect("private state");
    workspace
        .write_file(".bowline/conflicts/conflict_2/local/app.env", b"SECRET=2\n")
        .expect("private state");
    workspace
        .write_file(".bowline-conflicts/conflict_3/local/app.env", b"SECRET=3\n")
        .expect("ordinary user file");
    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");

    let candidate = coalesce_workspace_scan(
        workspace.root(),
        WorkspaceId::new("ws_code"),
        &base_ref,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:06:00Z",
    )
    .expect("candidate");

    assert!(
        snapshot_entries(&candidate.snapshot)
            .iter()
            .all(|entry| entry.path != ".bowline"
                && !entry.path.starts_with(".bowline/")
                && !entry.path.contains("SECRET"))
    );
    assert!(
        snapshot_entries(&candidate.snapshot)
            .iter()
            .any(|entry| entry.path == ".bowline-conflicts/conflict_3/local/app.env")
    );
}
