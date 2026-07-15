use super::*;
#[test]
fn runner_masks_unresolved_conflict_paths_from_next_upload() {
    let workspace = TempWorkspace::new("phase7-conflict-mask-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-conflict-mask-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "config.toml", b"value = \"local unresolved\"\n")
        .expect("local unresolved");
    create_conflict_bundle(
        state.root(),
        canonical_conflict_occurrence(
            ConflictRecord::same_path("app/config.toml"),
            "empty",
            "empty",
        ),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"local unresolved\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("conflict bundle");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 12).expect("byte store");
    let storage_key = StorageKey::deterministic(12);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [12_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:07:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let outcome = runner.tick().expect("tick");
    let snapshot_id = match outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => {
                workspace_ref.snapshot_id
            }
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("first upload should advance")
            }
        },
        other => panic!("expected upload, got {other:?}"),
    };
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [12; 32]),
    )
    .expect("import");

    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/package.json")
    );
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .all(|entry| entry.path != "app/config.toml"),
        "unresolved conflict path must not be re-uploaded before accept/reject"
    );
}

#[test]
fn runner_syncs_later_non_overlapping_edits_inside_unresolved_conflict_file() {
    let workspace =
        TempWorkspace::new("phase7-conflict-continuation-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-conflict-continuation-state").expect("state");
    let remote_seed =
        TempWorkspace::new("phase7-conflict-continuation-remote-seed").expect("remote seed");
    let workspace_id = WorkspaceId::new("ws_code");
    let content_key = [21_u8; 32];
    let storage_key = StorageKey::deterministic(21);
    let recorded_local = b"title = \"local\"\nshared = \"same\"\n";
    let recorded_remote = b"title = \"remote\"\nshared = \"same\"\n";
    let live_local = b"title = \"local\"\nshared = \"same\"\nlater = \"kept\"\n";
    let expected_uploaded = b"title = \"remote\"\nshared = \"same\"\nlater = \"kept\"\n";

    remote_seed
        .write_project_file("app", "config.toml", recorded_remote)
        .expect("remote config");
    workspace
        .write_project_file("app", "config.toml", live_local)
        .expect("live config");
    for index in 0..200 {
        let path = format!("src/generated_{index:03}.ts");
        let bytes = format!("export const generated{index} = {index};\n");
        remote_seed
            .write_project_file("app", &path, bytes.as_bytes())
            .expect("remote generated source");
        workspace
            .write_project_file("app", &path, bytes.as_bytes())
            .expect("local generated source");
    }

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 21).expect("byte store");
    let remote_candidate = coalesce_workspace_scan(
        remote_seed.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-b"),
        content_key,
        "2026-06-24T12:08:00Z",
    )
    .expect("remote candidate");
    let remote_ref = match upload_snapshot_candidate(
        &remote_candidate,
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("remote upload")
    {
        bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
        bowline_local::sync::UploadOutcome::Stale { .. } => panic!("seed upload should advance"),
    };

    let store = MetadataStore::open(state.root().join("local.sqlite3")).expect("metadata");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: remote_ref.clone(),
            observed_at: "2026-06-24T12:08:30Z".to_string(),
        })
        .expect("local head");
    drop(store);

    let bundle = create_conflict_bundle(
        state.root(),
        canonical_conflict_occurrence(
            ConflictRecord::same_path_span(
                "app/config.toml",
                ConflictSpan {
                    path: "app/config.toml".to_string(),
                    base_start_line: 1,
                    base_end_line: 1,
                    local_start_line: 1,
                    local_end_line: 1,
                    remote_start_line: 1,
                    remote_end_line: 1,
                    base_context_hash: None,
                    local_context_hash: None,
                    remote_context_hash: None,
                },
            ),
            base_ref.snapshot_id.as_str(),
            remote_ref.snapshot_id.as_str(),
        ),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"title = \"base\"\nshared = \"same\"\n".to_vec()),
            local: Some(recorded_local.to_vec()),
            remote: Some(recorded_remote.to_vec()),
        }],
    )
    .expect("conflict bundle");

    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: content_key,
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let outcome = runner.tick().expect("tick");
    let snapshot_id = match outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => {
                workspace_ref.snapshot_id
            }
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("continuation should advance")
            }
        },
        other => panic!("expected upload, got {other:?}"),
    };
    let imported = import_snapshot_by_id(
        &workspace_id,
        &bowline_core::ids::SnapshotId::new(snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&workspace_id, content_key),
    )
    .expect("import");

    let imported_entries = snapshot_entries(&imported.snapshot);
    let entry = imported_entries
        .iter()
        .find(|entry| entry.path == "app/config.toml")
        .expect("continued config entry");
    let segment = entry
        .content_layout
        .as_ref()
        .and_then(|layout| layout.segments().first())
        .expect("continued config segment");
    let locator = content_locator_for_segment(segment);
    let pack_id = &segment.pack_id;
    let pack_object = imported
        .pack_pointers
        .iter()
        .find(|pointer| pointer.content_id == pack_id.as_str())
        .expect("continued config pack object");
    let hydrated = LocalContentCache::open(state.root().join("cache"))
        .expect("content cache")
        .hydrate_record_from_range(
            &byte_store,
            RangeHydrationRequest {
                object_key: &ObjectKey::new(pack_object.object_key.clone()).expect("object key"),
                workspace_id: &workspace_id,
                locator: &locator,
                content_key,
                content_verification: bowline_storage::ContentVerification::AuthenticatedSegment,
                key: storage_key,
                key_epoch: 1,
            },
        )
        .expect("continued config hydration");

    assert_eq!(
        hydrated, expected_uploaded,
        "safe later edit should sync while the unresolved line stays on the remote side"
    );
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/src/generated_199.ts"),
        "many-file trees must keep syncing around an unresolved conflict span"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.root.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert_eq!(manifest["state"].as_str(), Some("unresolved"));
}

#[test]
fn runner_does_not_mask_rejected_conflict_paths_after_resolution() {
    let workspace =
        TempWorkspace::new("phase7-rejected-conflict-mask-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-rejected-conflict-mask-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "config.toml", b"value = \"rejected local\"\n")
        .expect("local unresolved");
    let bundle = create_conflict_bundle(
        state.root(),
        canonical_conflict_occurrence(
            ConflictRecord::same_path("app/config.toml"),
            "empty",
            "empty",
        ),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"rejected local\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("conflict bundle");
    let manifest_path = bundle.root.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest")).expect("json");
    manifest["state"] = serde_json::Value::String("rejected".to_string());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .expect("write rejected manifest");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 16).expect("byte store");
    let storage_key = StorageKey::deterministic(16);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [16_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:07:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let outcome = runner.tick().expect("tick");
    let snapshot_id = match outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => {
                workspace_ref.snapshot_id
            }
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("first upload should advance")
            }
        },
        other => panic!("expected upload, got {other:?}"),
    };
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [16; 32]),
    )
    .expect("import");

    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/package.json")
    );
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/config.toml"),
        "rejected conflicts should not silently suppress future uploads after resolve applies a concrete view"
    );
}

#[test]
fn runner_idles_after_recording_unresolved_conflict() {
    let source = TempWorkspace::new("phase7-conflict-stable-source").expect("source workspace");
    let source_state =
        TempWorkspace::new("phase7-conflict-stable-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-conflict-stable-peer").expect("peer workspace");
    let peer_state = TempWorkspace::new("phase7-conflict-stable-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 15)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(15);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [15_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:12:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base tick"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [15_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:13:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer import tick"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    source
        .write_project_file("app", "remote.ts", b"export const remote = true;\n")
        .expect("remote-only file");
    assert!(matches!(
        source_runner.tick().expect("source remote tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");

    let conflict = peer_runner.tick().expect("conflict tick");
    assert!(matches!(conflict, SyncTickOutcome::Conflicted(_)));
    assert_eq!(
        drive_pending_conflict_occurrences(
            peer_state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:13:01Z",
        )
        .expect("durable conflict occurrence worker"),
        1
    );
    let listed_conflicts = control_plane
        .list_workspace_conflicts(
            &bowline_core::ids::WorkspaceId::new("ws_code"),
            &bowline_core::ids::DeviceId::new("device-b"),
        )
        .expect("published conflict metadata");
    assert_eq!(listed_conflicts.len(), 1);
    assert_eq!(listed_conflicts[0].paths, vec!["app/config.toml"]);
    assert_eq!(
        listed_conflicts[0].state,
        bowline_control_plane::ConflictOccurrenceState::Unresolved
    );
    assert_eq!(
        fs::read(peer.root().join("app").join("remote.ts")).expect("remote-only file"),
        b"export const remote = true;\n",
        "non-conflicting remote files must materialize before recording the remote head locally"
    );
    let second = peer_runner.tick().expect("second peer tick");

    assert_eq!(second, SyncTickOutcome::NoChanges);
    let current_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(current_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [15; 32]),
    )
    .expect("import current head");
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/remote.ts"),
        "the conflicted peer must not delete non-conflicting remote files on the next tick"
    );
    assert_eq!(
        fs::read(peer.root().join("app").join("config.toml")).expect("local unresolved file"),
        b"value = \"local\"\n",
        "unresolved local view should remain in place until accept/reject"
    );
}

#[test]
fn unresolved_conflict_metadata_publish_retries_existing_bundle_after_failure() {
    let source = TempWorkspace::new("phase7-conflict-publish-retry-source").expect("source");
    let source_state =
        TempWorkspace::new("phase7-conflict-publish-retry-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-conflict-publish-retry-peer").expect("peer");
    let peer_state =
        TempWorkspace::new("phase7-conflict-publish-retry-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let inner = FakeControlPlaneClient::default();
    inner.create_workspace("ws_code");
    let control_plane = CasFailsOnceControlPlane::new_conflict_publish_fails_once(inner);
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 32)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(32);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [32_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:20:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base tick"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [32_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:21:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer import tick"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    assert!(matches!(
        source_runner.tick().expect("source remote tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");

    let first_outcome = peer_runner
        .tick()
        .expect("conflicted tick commits before failed post-commit publish");
    assert!(matches!(first_outcome, SyncTickOutcome::Conflicted(_)));
    let publish_error = drive_pending_conflict_occurrences(
        peer_state.root(),
        &WorkspaceId::new("ws_code"),
        &control_plane,
        "2026-06-24T12:21:01Z",
    )
    .expect_err("first durable conflict publish fails");
    assert!(publish_error.contains("injected conflict metadata publish failure"));
    assert_eq!(
        control_plane
            .list_workspace_conflicts(
                &bowline_core::ids::WorkspaceId::new("ws_code"),
                &bowline_core::ids::DeviceId::new("device-b")
            )
            .expect("no published conflicts yet")
            .len(),
        0
    );
    assert_eq!(
        MetadataStore::open(peer_state.root().join(DEFAULT_DATABASE_FILE))
            .expect("store")
            .sync_operations(&WorkspaceId::new("ws_code"))
            .expect("operations")
            .into_iter()
            .filter(|operation| {
                operation.kind == SyncOperationKind::ConflictOccurrenceReconcile
                    && operation.state == SyncOperationState::WaitingRetry
            })
            .count(),
        1
    );
    assert_eq!(
        drive_pending_conflict_occurrences(
            peer_state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:21:02Z",
        )
        .expect("retry durable conflict publish"),
        1
    );
    assert_eq!(
        control_plane
            .list_workspace_conflicts(
                &bowline_core::ids::WorkspaceId::new("ws_code"),
                &bowline_core::ids::DeviceId::new("device-b")
            )
            .expect("published conflict metadata after retry")
            .len(),
        1
    );
    let entries = fs::read_dir(peer_state.root().join("conflicts"))
        .expect("conflicts dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("conflicts")
        .into_iter()
        .filter(|entry| {
            entry.file_type().expect("conflict entry type").is_dir()
                && entry.path().join("manifest.json").is_file()
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(entries[0].path().join("manifest.json")).expect("manifest"),
    )
    .expect("manifest json");
    assert!(
        manifest.get("remoteConflictPublishedAt").is_some(),
        "successful retry must locally acknowledge remote conflict metadata publication"
    );
}

#[test]
fn accepted_conflict_resolution_advances_ref_and_imports_on_peer() {
    let source = TempWorkspace::new("phase7-accepted-resolution-source").expect("source");
    let source_state =
        TempWorkspace::new("phase7-accepted-resolution-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-accepted-resolution-peer").expect("peer");
    let peer_state =
        TempWorkspace::new("phase7-accepted-resolution-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 17)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(17);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [17_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:14:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base upload"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [17_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:15:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer base import"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    assert!(matches!(
        source_runner.tick().expect("source remote upload"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");
    assert!(matches!(
        peer_runner.tick().expect("peer conflict"),
        SyncTickOutcome::Conflicted(_)
    ));
    assert_eq!(
        drive_pending_conflict_occurrences(
            peer_state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:15:01Z",
        )
        .expect("publish conflict occurrence"),
        1
    );
    assert_eq!(
        control_plane
            .list_workspace_conflicts(
                &bowline_core::ids::WorkspaceId::new("ws_code"),
                &bowline_core::ids::DeviceId::new("device-b")
            )
            .expect("published conflict metadata")
            .len(),
        1
    );

    peer.write_project_file("app", "config.toml", b"value = \"resolved\"\n")
        .expect("accepted resolution bytes");
    mark_only_conflict_bundle_state(peer_state.root(), "accepted");

    assert!(matches!(
        peer_runner.tick().expect("peer accepted resolution upload"),
        SyncTickOutcome::Uploaded(_)
    ));
    assert_eq!(
        drive_pending_conflict_occurrences(
            peer_state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:15:02Z",
        )
        .expect("publish accepted conflict resolution"),
        1
    );
    assert_eq!(
        control_plane
            .list_workspace_conflicts(
                &bowline_core::ids::WorkspaceId::new("ws_code"),
                &bowline_core::ids::DeviceId::new("device-b")
            )
            .expect("resolved conflict metadata")
            .len(),
        0
    );
    let resolved_event_count = control_plane
        .list_events(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("events")
        .iter()
        .filter(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictResolved)
        .count();
    assert_eq!(
        peer_runner.tick().expect("post-resolution idle"),
        SyncTickOutcome::NoChanges
    );
    assert_eq!(
        control_plane
            .list_events(&bowline_core::ids::WorkspaceId::new("ws_code"))
            .expect("events")
            .iter()
            .filter(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictResolved)
            .count(),
        resolved_event_count,
        "resolved conflicts must not re-publish terminal metadata on idle ticks"
    );
    assert!(matches!(
        source_runner
            .tick()
            .expect("source imports accepted resolution"),
        SyncTickOutcome::Imported(_)
    ));
    assert_eq!(
        fs::read(source.root().join("app").join("config.toml")).expect("source resolved file"),
        b"value = \"resolved\"\n"
    );

    let current_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(current_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [17; 32]),
    )
    .expect("import current head");
    assert_eq!(current_ref.version, 3);
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/config.toml"),
        "accepted resolution must remain in the workspace head"
    );
}

#[test]
fn rejected_conflict_resolution_adopts_remote_head_without_new_ref() {
    let source = TempWorkspace::new("phase7-rejected-resolution-source").expect("source");
    let source_state =
        TempWorkspace::new("phase7-rejected-resolution-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-rejected-resolution-peer").expect("peer");
    let peer_state =
        TempWorkspace::new("phase7-rejected-resolution-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 18)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(18);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [18_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:16:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base upload"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [18_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:17:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer base import"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    assert!(matches!(
        source_runner.tick().expect("source remote upload"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");
    assert!(matches!(
        peer_runner.tick().expect("peer conflict"),
        SyncTickOutcome::Conflicted(_)
    ));
    assert_eq!(
        drive_pending_conflict_occurrences(
            peer_state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:17:01Z",
        )
        .expect("publish conflict occurrence"),
        1
    );

    peer.write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("rejected resolution adopts remote bytes");
    mark_only_conflict_bundle_state(peer_state.root(), "rejected");

    assert_eq!(
        peer_runner.tick().expect("peer rejected resolution tick"),
        SyncTickOutcome::NoChanges
    );
    assert_eq!(
        drive_pending_conflict_occurrences(
            peer_state.root(),
            &WorkspaceId::new("ws_code"),
            &control_plane,
            "2026-06-24T12:17:02Z",
        )
        .expect("publish rejected conflict resolution"),
        1
    );
    let current_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");
    assert_eq!(
        current_ref.version, 2,
        "rejecting current remote side should not create a duplicate workspace ref"
    );
    assert_eq!(
        fs::read(peer.root().join("app").join("config.toml")).expect("peer remote file"),
        b"value = \"remote\"\n"
    );
}
