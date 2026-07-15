use super::*;

#[test]
fn fresh_empty_runner_imports_remote_head_without_overwriting_it() {
    let source = TempWorkspace::new("phase7-fresh-import-source").expect("source workspace");
    let source_state =
        TempWorkspace::new("phase7-fresh-import-source-state").expect("source state");
    let fresh = TempWorkspace::new("phase7-fresh-import-empty").expect("fresh workspace");
    let fresh_state = TempWorkspace::new("phase7-fresh-import-empty-state").expect("fresh state");
    source
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    source
        .write_project_file("app", ".bowlinesetup", b"echo setup\n")
        .expect("setup recipe");
    source
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 13)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(13);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [13_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:08:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let uploaded_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");
    let before_import_metrics = byte_store.metrics();
    {
        let initialized_metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
            .expect("initialized metadata");
        initialized_metadata
            .insert_workspace(&WorkspaceId::new("ws_code"), "Code", "2026-06-24T12:08:30Z")
            .expect("initialized workspace");
        initialized_metadata
            .insert_root(
                "root_code",
                &WorkspaceId::new("ws_code"),
                &fresh.root().display().to_string(),
                "2026-06-24T12:08:30Z",
            )
            .expect("initialized root");
    }

    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [13_u8; 32],
            storage_key,
            key_epoch: 2,
            generated_at: "2026-06-24T12:09:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let outcome = fresh_runner.tick().expect("fresh tick");

    assert!(matches!(outcome, SyncTickOutcome::Imported(_)));
    let after_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");
    assert_eq!(
        after_ref.version, uploaded_ref.version,
        "fresh empty device must not advance remote head with an empty snapshot"
    );
    assert_eq!(after_ref.snapshot_id, uploaded_ref.snapshot_id);
    assert!(
        fresh.root().join("app").join("src").is_dir(),
        "fresh import should materialize directories"
    );
    assert_eq!(
        fs::read(fresh.root().join("app").join("package.json")).expect("package manifest"),
        br#"{"name":"app"}"#,
        "package manifests should materialize as normal source bytes"
    );
    assert_eq!(
        fs::read(fresh.root().join("app").join("src").join("main.ts")).expect("source file"),
        b"export const value = 1;\n",
        "fresh import should materialize canonical source bytes into the real root"
    );
    let after_import_metrics = byte_store.metrics();
    let metadata_reads =
        after_import_metrics.full_read_count - before_import_metrics.full_read_count;
    assert!(
        (2..=32).contains(&metadata_reads),
        "fresh import should read one root plus a bounded metadata graph, got {metadata_reads}"
    );
    assert_eq!(
        after_import_metrics.range_read_count - before_import_metrics.range_read_count,
        3,
        "each required file should hydrate through its claimed task range"
    );
    let metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
        .expect("fresh import metadata");
    let retained_snapshot = metadata
        .snapshot(
            &WorkspaceId::new("ws_code"),
            &bowline_core::ids::SnapshotId::new(uploaded_ref.snapshot_id.clone()),
        )
        .expect("retained snapshot lookup")
        .expect("imported retained snapshot");
    assert_eq!(retained_snapshot.id.as_str(), uploaded_ref.snapshot_id);
    let history = bowline_local::history::compose_history(bowline_local::history::HistoryOptions {
        db_path: Some(fresh_state.root().join("local.sqlite3")),
        target_path: fresh.root().join("app").display().to_string(),
        mode: bowline_local::history::HistoryMode::Timeline,
        generated_at: "2026-06-24T12:09:01Z".to_string(),
        limit: 10,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("fresh-device history");
    assert_eq!(
        history
            .restore_points
            .iter()
            .map(|point| point.snapshot_id.as_str())
            .collect::<Vec<_>>(),
        vec![uploaded_ref.snapshot_id.as_str()]
    );
    let pack_records = metadata
        .pack_records(&WorkspaceId::new("ws_code"))
        .expect("pack records");
    assert_eq!(pack_records.len(), 1);
    assert!(
        !pack_records[0].object_hash.is_empty(),
        "structure import should persist real pack object metadata"
    );
    assert_eq!(
        pack_records[0].key_epoch, 1,
        "imported pack metadata should preserve the remote object epoch"
    );
    assert_eq!(
        metadata
            .current_namespace_entry(
                &WorkspaceId::new("ws_code"),
                &WorkspaceRelativePath::new("app/src/main.ts"),
            )
            .expect("projected node lookup")
            .expect("projected node")
            .hydration_state,
        HydrationState::Local
    );
    assert_eq!(
        metadata
            .current_namespace_entry(
                &WorkspaceId::new("ws_code"),
                &WorkspaceRelativePath::new("app/package.json"),
            )
            .expect("package projected node lookup")
            .expect("package projected node")
            .hydration_state,
        HydrationState::Local,
        "materialized files are real local files, not cold placeholders"
    );
    source
        .write_project_file("app", "src/remote.ts", b"export const remote = 2;\n")
        .expect("remote update");
    assert!(
        matches!(
            source_runner.tick().expect("source remote update tick"),
            SyncTickOutcome::Uploaded(_)
        ),
        "source remote update should advance"
    );
    assert!(
        matches!(
            fresh_runner.tick().expect("fresh remote update tick"),
            SyncTickOutcome::Imported(_)
        ),
        "remote-only updates should import and materialize the new source bytes"
    );
    assert_eq!(
        fs::read(fresh.root().join("app").join("src").join("remote.ts"))
            .expect("remote source materialized"),
        b"export const remote = 2;\n",
        "remote-only ordinary source updates should appear in the real root"
    );

    fs::remove_file(source.root().join("app").join(".bowlinesetup")).expect("remote setup delete");
    assert!(
        matches!(
            source_runner.tick().expect("source remote delete tick"),
            SyncTickOutcome::Uploaded(_)
        ),
        "source remote delete should advance"
    );
    assert!(
        matches!(
            fresh_runner.tick().expect("fresh remote delete tick"),
            SyncTickOutcome::Imported(_)
        ),
        "remote-only deletes should update the real root"
    );
    assert!(
        !fresh.root().join("app").join(".bowlinesetup").exists(),
        "remote deletion of a bootstrap-materialized file should remove the local file"
    );
    assert!(
        matches!(
            fresh_runner.tick().expect("fresh after remote delete tick"),
            SyncTickOutcome::NoChanges
        ),
        "remote-deleted bootstrap files must not be resurrected as local edits"
    );

    fs::remove_file(fresh.root().join("app").join("package.json")).expect("remove package file");
    let deletion_outcome = fresh_runner.tick().expect("package deletion tick");
    let deleted_ref = match deletion_outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("package deletion should advance")
            }
        },
        other => panic!("expected package deletion upload, got {other:?}"),
    };
    let after_package_delete = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(deleted_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [13; 32]),
    )
    .expect("import after package delete");
    let manifest_pack_ids = snapshot_entries(&after_package_delete.snapshot)
        .iter()
        .filter_map(|entry| entry.content_layout.as_ref())
        .flat_map(ContentLayout::segments)
        .map(|segment| bowline_core::ids::ContentId::new(segment.pack_id.as_str()))
        .collect::<BTreeSet<_>>();
    let retained_pack_ids = after_package_delete
        .pack_pointers
        .iter()
        .map(|pointer| pointer.content_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        retained_pack_ids, manifest_pack_ids,
        "uploads that preserve cold locators must retain every source pack pointer"
    );
    assert!(
        !snapshot_entries(&after_package_delete.snapshot)
            .iter()
            .any(|entry| entry.path == "app/package.json"),
        "deleting a bootstrap-materialized file must sync as a deletion"
    );
    assert!(
        metadata
            .current_namespace_entry(
                &WorkspaceId::new("ws_code"),
                &WorkspaceRelativePath::new("app/package.json"),
            )
            .expect("deleted package projected node lookup")
            .is_none(),
        "accepted local deletions should remove projected-node metadata"
    );
    assert!(
        snapshot_entries(&after_package_delete.snapshot)
            .iter()
            .any(|entry| entry.path == "app/src/main.ts"),
        "cold source files must still be preserved while real local deletions sync"
    );

    fs::create_dir_all(fresh.root().join("app").join("src")).expect("recreate src dir");
    fs::write(
        fresh.root().join("app").join("src").join("main.ts"),
        b"console.log('edited locally');\n",
    )
    .expect("edit cold file");
    let edited_ref = match fresh_runner.tick().expect("edited cold file tick") {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => panic!("edit should advance"),
        },
        other => panic!("expected edit upload, got {other:?}"),
    };
    let edited_snapshot = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(edited_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [13; 32]),
    )
    .expect("import after edit");
    assert!(
        snapshot_entries(&edited_snapshot.snapshot)
            .iter()
            .any(|entry| entry.path == "app/src/main.ts"),
        "local edits to formerly cold files should sync as real files"
    );

    fs::remove_file(fresh.root().join("app").join("src").join("main.ts"))
        .expect("delete materialized source file");
    let deleted_cold_ref = match fresh_runner.tick().expect("delete source file tick") {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => panic!("delete should advance"),
        },
        other => panic!("expected delete upload, got {other:?}"),
    };
    let deleted_cold_snapshot = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(deleted_cold_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [13; 32]),
    )
    .expect("import after source delete");
    assert!(
        !snapshot_entries(&deleted_cold_snapshot.snapshot)
            .iter()
            .any(|entry| entry.path == "app/src/main.ts"),
        "deleting a materialized source file must sync as a deletion"
    );
    assert!(
        metadata
            .current_namespace_entry(
                &WorkspaceId::new("ws_code"),
                &WorkspaceRelativePath::new("app/src/main.ts"),
            )
            .expect("deleted source projected node lookup")
            .is_none(),
        "accepted deletion of a materialized source file should remove projected-node metadata"
    );

    fs::remove_file(fresh.root().join("app").join("src").join("remote.ts"))
        .expect("delete remaining source file before replacing directory");
    fs::remove_dir(fresh.root().join("app").join("src")).expect("remove empty src dir");
    fs::remove_dir(fresh.root().join("app")).expect("remove empty app dir");
    fs::write(
        fresh.root().join("app"),
        b"local file replacing directory\n",
    )
    .expect("replace directory with file");
    let replaced_dir_ref = match fresh_runner.tick().expect("directory replacement tick") {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("directory replacement should advance")
            }
        },
        other => panic!("expected directory replacement upload, got {other:?}"),
    };
    let replaced_dir_snapshot = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(replaced_dir_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [13; 32]),
    )
    .expect("import after directory replacement");
    assert!(
        snapshot_entries(&replaced_dir_snapshot.snapshot)
            .iter()
            .any(|entry| entry.path == "app" && entry.kind == NamespaceEntryKind::File),
        "local file replacement should be represented as the path itself"
    );
    assert!(
        !snapshot_entries(&replaced_dir_snapshot.snapshot)
            .iter()
            .any(|entry| entry.path.starts_with("app/")),
        "preserved cold descendants must not survive under a local file replacement"
    );
}

#[test]
fn fresh_import_materializes_large_lazy_files_as_background_priority() {
    let source = TempWorkspace::new("phase7-lazy-source").expect("source workspace");
    let source_state = TempWorkspace::new("phase7-lazy-source-state").expect("source state");
    let fresh = TempWorkspace::new("phase7-lazy-fresh").expect("fresh workspace");
    let fresh_state = TempWorkspace::new("phase7-lazy-fresh-state").expect("fresh state");
    source
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let large_bytes = vec![b'x'; 8 * 1024 * 1024 + 1];
    source
        .write_project_file("app", "assets/video.bin", &large_bytes)
        .expect("large file");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 45)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(45);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [45_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:08:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let before_import_metrics = byte_store.metrics();

    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [45_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    assert!(matches!(
        fresh_runner.tick().expect("fresh import tick"),
        SyncTickOutcome::Imported(_)
    ));

    assert_eq!(
        fs::read(fresh.root().join("app/package.json")).expect("package materialized"),
        br#"{"name":"app"}"#,
        "ordinary source/config files still materialize into the real directory"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/assets/video.bin")).expect("large file materialized"),
        large_bytes,
        "lazy is a scheduling priority, not permission to leave an ordinary path absent"
    );
    let after_import_metrics = byte_store.metrics();
    let metadata_reads =
        after_import_metrics.full_read_count - before_import_metrics.full_read_count;
    assert!(
        (2..=32).contains(&metadata_reads),
        "fresh import should read one root plus a bounded metadata graph, got {metadata_reads}"
    );
    assert_eq!(
        after_import_metrics.range_read_count - before_import_metrics.range_read_count,
        2,
        "priority-ordered tasks should range-hydrate the package and large file independently"
    );

    let metadata =
        MetadataStore::open(fresh_state.root().join("local.sqlite3")).expect("fresh metadata");
    let large_node = metadata
        .current_namespace_entry(
            &WorkspaceId::new("ws_code"),
            &WorkspaceRelativePath::new("app/assets/video.bin"),
        )
        .expect("large projected node lookup")
        .expect("large projected node");
    assert_eq!(large_node.hydration_state, HydrationState::Local);
    let locators = metadata
        .content_locators(&WorkspaceId::new("ws_code"))
        .expect("content locators");
    let workspace_ref = control_plane
        .get_workspace_ref(&WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &workspace_ref.snapshot_id,
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [45; 32]),
    )
    .expect("import current snapshot manifest");
    let imported_entries = snapshot_entries(&imported.snapshot);
    let large_layout = imported_entries
        .iter()
        .find(|entry| entry.path == "app/assets/video.bin")
        .and_then(|entry| entry.content_layout.as_ref())
        .expect("large file segmented layout");
    assert!(
        large_layout.segments().iter().all(|segment| {
            locators
                .iter()
                .any(|locator| locator.content_id.as_str() == segment.segment_id.as_str())
        }),
        "materialized large files retain every authenticated segment locator for repair and replay"
    );
    assert_eq!(
        large_layout
            .segments()
            .iter()
            .map(|segment| segment.plaintext_length)
            .sum::<u64>(),
        large_bytes.len() as u64,
        "authenticated segment locators cover the complete logical file"
    );
}

#[test]
fn fresh_non_empty_runner_merges_with_remote_head_without_overwriting_it() {
    let source = TempWorkspace::new("phase7-fresh-merge-source").expect("source workspace");
    let source_state = TempWorkspace::new("phase7-fresh-merge-source-state").expect("source state");
    let fresh = TempWorkspace::new("phase7-fresh-merge-local").expect("fresh workspace");
    let fresh_state = TempWorkspace::new("phase7-fresh-merge-local-state").expect("fresh state");
    source
        .write_project_file("app", "remote.ts", b"export const remote = true;\n")
        .expect("remote file");
    source
        .write_project_file("app", "remote-copy.ts", b"export const remote = true;\n")
        .expect("duplicate remote file");
    fresh
        .write_project_file("app", "local.ts", b"export const local = true;\n")
        .expect("local file");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 14)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(14);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [14_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:10:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let uploaded_ref = control_plane
        .get_workspace_ref(&bowline_core::ids::WorkspaceId::new("ws_code"))
        .expect("workspace ref")
        .expect("workspace exists");

    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [14_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:11:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let outcome = fresh_runner.tick().expect("fresh tick");

    let merged_ref = match outcome {
        SyncTickOutcome::Merged(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => panic!("merge should advance"),
        },
        other => panic!("expected merge, got {other:?}"),
    };
    assert!(
        merged_ref.version > uploaded_ref.version,
        "fresh local additions should publish a merge, not overwrite the current head directly"
    );
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(merged_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_code"), [14; 32]),
    )
    .expect("import merged");
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/remote.ts")
    );
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/remote-copy.ts")
    );
    assert!(
        snapshot_entries(&imported.snapshot)
            .iter()
            .any(|entry| entry.path == "app/local.ts")
    );
    let import_preparations = fresh_state.root().join("preparations/import");
    if import_preparations.exists() {
        assert_eq!(
            fs::read_dir(import_preparations)
                .expect("import preparation directory")
                .count(),
            0,
            "stale merge imports must release transient staged files"
        );
    }
}

#[test]
fn conflict_bundle_rejects_paths_that_escape_bundle_roots() {
    let state = TempWorkspace::new("phase7-conflict-path").expect("state");
    let result = create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path("../escape.txt"),
        &[ConflictFile {
            relative_path: "../escape.txt".to_string(),
            base: Some(b"base".to_vec()),
            local: Some(b"local".to_vec()),
            remote: Some(b"remote".to_vec()),
        }],
    );

    assert!(matches!(result, Err(ConflictBundleError::UnsafePath(_))));
}

#[test]
fn conflict_bundle_marks_env_paths_as_secret_bearing() {
    let state = TempWorkspace::new("phase7-conflict-secret").expect("state");
    let bundle = create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path("app/.env.local"),
        &[ConflictFile {
            relative_path: "app/.env.local".to_string(),
            base: Some(b"SECRET=base\n".to_vec()),
            local: Some(b"SECRET=local\n".to_vec()),
            remote: Some(b"SECRET=remote\n".to_vec()),
        }],
    )
    .expect("conflict bundle");
    let manifest = fs::read_to_string(bundle.root.join("manifest.json")).expect("manifest");
    let manifest: serde_json::Value = serde_json::from_str(&manifest).expect("json");

    assert_eq!(manifest["containsSecrets"], true);
}

#[test]
fn opaque_git_state_uploads_and_imports_as_encrypted_workspace_bytes() {
    let workspace = TempWorkspace::new("phase7-git-opaque-source").expect("source root");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let git = workspace.create_git_repo("app").expect("git repo");
    fs::write(git.join("index"), b"opaque index bytes").expect("git index");
    fs::write(git.join("refs").join("heads").join("main"), b"abc123\n").expect("branch ref");
    fs::write(git.join("packed-refs"), b"dddd refs/tags/v1\n").expect("packed refs");
    fs::write(git.join("refs").join("stash"), b"stash123\n").expect("stash ref");
    fs::create_dir_all(git.join("hooks")).expect("hooks dir");
    fs::write(git.join("hooks").join("pre-commit"), b"#!/bin/sh\n").expect("hook");
    fs::create_dir_all(git.join("modules").join("lib")).expect("submodule git dir");
    fs::write(
        git.join("modules").join("lib").join("HEAD"),
        b"ref: refs/heads/main\n",
    )
    .expect("submodule git head");
    fs::create_dir_all(git.join("lfs").join("objects").join("aa").join("bb")).expect("lfs dir");
    fs::write(
        git.join("lfs")
            .join("objects")
            .join("aa")
            .join("bb")
            .join("oid"),
        b"lfs object bytes",
    )
    .expect("lfs object");
    fs::create_dir_all(git.join("objects").join("ab")).expect("object dir");
    fs::write(git.join("objects").join("ab").join("cdef"), b"loose-object").expect("loose object");
    fs::create_dir_all(git.join("objects").join("pack")).expect("pack dir");
    fs::write(
        git.join("objects").join("pack").join("pack-main-001.pack"),
        b"git pack bytes",
    )
    .expect("pack file");
    fs::write(
        git.join("objects").join("pack").join("tmp_pack_001"),
        b"tmp",
    )
    .expect("transient pack temp");
    let detector = workspace.mutation_detector().expect("mutation detector");

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let state = TempWorkspace::new("phase7-git-opaque-state").expect("state root");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 21).expect("byte store");
    let workspace_id = WorkspaceId::new("ws_code");
    let storage_key = StorageKey::deterministic(21);
    let content_key = [21_u8; 32];
    let candidate = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-a"),
        content_key,
        "2026-06-26T12:00:00Z",
    )
    .expect("candidate");

    detector
        .assert_unchanged()
        .expect("sync should not mutate git");
    let candidate_entries = snapshot_entries(&candidate.snapshot);
    assert!(candidate_entries.iter().any(|entry| {
        entry.path == "app/.git/refs/heads/main" && entry.mode == MaterializationMode::EncryptedSync
    }));
    assert!(candidate_entries.iter().any(|entry| {
        entry.path == "app/.git/objects/ab/cdef" && entry.mode == MaterializationMode::EncryptedSync
    }));
    assert!(candidate_entries.iter().any(|entry| {
        entry.path == "app/.git/objects/pack/pack-main-001.pack"
            && entry.mode == MaterializationMode::EncryptedSync
    }));
    for path in [
        "app/.git/index",
        "app/.git/packed-refs",
        "app/.git/refs/stash",
        "app/.git/hooks/pre-commit",
        "app/.git/modules/lib/HEAD",
        "app/.git/lfs/objects/aa/bb/oid",
    ] {
        assert!(
            snapshot_entries(&candidate.snapshot)
                .iter()
                .any(|entry| entry.path == path && entry.mode == MaterializationMode::EncryptedSync),
            "{path} should sync as opaque Git state"
        );
    }
    assert!(
        !snapshot_entries(&candidate.snapshot)
            .iter()
            .any(|entry| entry.path == "app/.git/objects/pack/tmp_pack_001"),
        "bounded Git transients must stay local-only and out of workspace head"
    );

    let outcome =
        upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
            .expect("upload");
    let snapshot_root = match outcome {
        bowline_local::sync::UploadOutcome::Advanced { snapshot_root, .. } => snapshot_root,
        bowline_local::sync::UploadOutcome::Stale { .. } => {
            panic!("first writer should advance")
        }
    };
    assert_eq!(
        snapshot_root.extra_root_logical_ids.len(),
        0,
        "the namespace root owns Git metadata reachability without snapshot-wide pack arrays"
    );
    for pointer in std::iter::once(&snapshot_root.manifest_object) {
        for forbidden in [".git", "refs", "heads", "main", "pack-main"] {
            assert!(
                !pointer.object_key.contains(forbidden),
                "object key leaked Git path detail `{forbidden}`"
            );
        }
    }

    let imported = import_snapshot_by_id(
        &workspace_id,
        &candidate.snapshot.manifest().snapshot_id,
        &control_plane,
        &byte_store,
        storage_key,
        MetadataIdentityKey::derive(&workspace_id, content_key),
    )
    .expect("import");
    let imported_entries = snapshot_entries(&imported.snapshot);
    let pack_entry = imported_entries
        .iter()
        .find(|entry| entry.path == "app/.git/objects/pack/pack-main-001.pack")
        .expect("imported pack entry");
    let segment = pack_entry
        .content_layout
        .as_ref()
        .and_then(|layout| layout.segments().first())
        .expect("packed segment");
    let locator = content_locator_for_segment(segment);
    let pack_id = &segment.pack_id;
    let pack_object = imported
        .pack_pointers
        .iter()
        .find(|pointer| pointer.content_id == pack_id.as_str())
        .expect("pack object pointer");
    let object_key = ObjectKey::new(pack_object.object_key.clone()).expect("object key");
    let cache = LocalContentCache::open(state.root().join("cache")).expect("content cache");
    let hydrated = cache
        .hydrate_record_from_range(
            &byte_store,
            RangeHydrationRequest {
                object_key: &object_key,
                workspace_id: &workspace_id,
                locator: &locator,
                content_key,
                content_verification: bowline_storage::ContentVerification::AuthenticatedSegment,
                key: storage_key,
                key_epoch: 1,
            },
        )
        .expect("range hydrate git pack entry");

    assert_eq!(hydrated, b"git pack bytes");

    let fresh = TempWorkspace::new("phase7-git-opaque-fresh").expect("fresh root");
    let fresh_state = TempWorkspace::new("phase7-git-opaque-fresh-state").expect("fresh state");
    {
        let initialized_metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
            .expect("initialized metadata");
        initialized_metadata
            .insert_workspace(&workspace_id, "Code", "2026-06-26T12:00:30Z")
            .expect("initialized workspace");
        initialized_metadata
            .insert_root(
                "root_code",
                &workspace_id,
                &fresh.root().display().to_string(),
                "2026-06-26T12:00:30Z",
            )
            .expect("initialized root");
    }
    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: content_key,
            storage_key,
            key_epoch: 2,
            generated_at: "2026-06-26T12:01:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    assert!(
        matches!(
            fresh_runner.tick().expect("fresh git import tick"),
            SyncTickOutcome::Imported(_)
        ),
        "fresh devices should import opaque Git continuity into the real directory"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/index")).expect("git index materialized"),
        b"opaque index bytes"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/refs/heads/main")).expect("branch materialized"),
        b"abc123\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/objects/ab/cdef")).expect("object materialized"),
        b"loose-object"
    );
    assert_eq!(
        fs::read(
            fresh
                .root()
                .join("app/.git/objects/pack/pack-main-001.pack")
        )
        .expect("pack materialized"),
        b"git pack bytes"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/packed-refs")).expect("packed refs"),
        b"dddd refs/tags/v1\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/refs/stash")).expect("stash ref"),
        b"stash123\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/hooks/pre-commit")).expect("hook"),
        b"#!/bin/sh\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/modules/lib/HEAD")).expect("submodule git head"),
        b"ref: refs/heads/main\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/lfs/objects/aa/bb/oid")).expect("lfs object"),
        b"lfs object bytes"
    );
    assert!(
        !fresh
            .root()
            .join("app/.git/objects/pack/tmp_pack_001")
            .exists(),
        "bounded Git transients must not rematerialize on fresh devices"
    );
    let fresh_metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
        .expect("fresh import metadata");
    assert_eq!(
        fresh_metadata
            .current_namespace_entry(
                &workspace_id,
                &WorkspaceRelativePath::new("app/.git/objects/pack/pack-main-001.pack"),
            )
            .expect("projected node lookup")
            .expect("git pack projected node")
            .hydration_state,
        HydrationState::Local
    );
    let events = fresh_metadata.list_events(20).expect("fresh import events");
    assert!(events.iter().any(|event| {
        event.name == EventName::HydrationStarted
            && event
                .summary
                .contains("Remote snapshot materialization started")
            && event.summary.contains("byte(s)")
    }));
    assert!(events.iter().any(|event| {
        event.name == EventName::HydrationCompleted
            && event
                .summary
                .contains("Remote snapshot materialization completed")
            && event.payload.get("cause").and_then(|value| value.as_str()) == Some("remote-import")
    }));
}

#[test]
fn missing_remote_materialization_records_blocked_hydration_event() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    control_plane
        .compare_and_swap_workspace_ref(
            &workspace_id,
            base_ref.version,
            &bowline_core::ids::SnapshotId::new("snap_missing_manifest"),
            &bowline_core::ids::DeviceId::new("device-a"),
        )
        .expect("advance fake remote ref");
    let root = TempWorkspace::new("phase7-missing-remote-root").expect("root");
    let state = TempWorkspace::new("phase7-missing-remote-state").expect("state");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 31).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: root.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [31_u8; 32],
            storage_key: StorageKey::deterministic(31),
            key_epoch: 1,
            generated_at: "2026-06-26T12:02:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    let error = runner.tick().expect_err("missing root blocks import");
    assert!(matches!(
        error,
        SyncRunnerError::Download(DownloadError::SnapshotManifestMissing(_))
    ));

    let metadata = MetadataStore::open(state.root().join("local.sqlite3")).expect("metadata opens");
    let events = metadata.list_events(20).expect("events");
    assert!(
        events.iter().any(|event| {
            event.name == EventName::HydrationBlocked
                && event
                    .summary
                    .contains("Remote snapshot materialization blocked")
                && event
                    .payload
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .is_some_and(|reason| reason.contains("snapshot root"))
        }),
        "events: {events:#?}"
    );
}

#[test]
fn divergent_git_files_create_opaque_conflicts_instead_of_text_merges() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_git_base",
        "app/.git/config",
        b"[core]\n\trepositoryformatversion = 0\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: bowline_core::ids::SnapshotId::new("snap_git_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(bowline_core::ids::DeviceId::new("device-a")),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_git_local",
            "app/.git/config",
            b"[core]\n\trepositoryformatversion = 0\n[branch \"local\"]\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_git_remote",
        "app/.git/config",
        b"[core]\n\trepositoryformatversion = 0\n[branch \"remote\"]\n",
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
        "2026-06-26T12:01:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Clean(candidate) => panic!(
            "opaque Git state must not text-merge into {:?}",
            snapshot_entries(&candidate.snapshot)
        ),
        MergeOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].paths, vec!["app/.git/config"]);
            assert_eq!(conflicts[0].reason, "opaque Git state conflict");
        }
    }
}
