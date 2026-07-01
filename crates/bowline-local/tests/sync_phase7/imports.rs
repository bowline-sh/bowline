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
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let uploaded_ref = control_plane
        .get_workspace_ref("ws_code")
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
        initialized_metadata
            .enqueue_hydration(&HydrationQueueRecord {
                id: "hydrate-main".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                project_id: None,
                path: "app/src/main.ts".to_string(),
                content_id: None,
                priority: "hot-project-prefetch".to_string(),
                state: "queued".to_string(),
                cause: "hot-project-prefetch".to_string(),
                updated_at: "2026-06-24T12:08:30Z".to_string(),
            })
            .expect("queued hydration");
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
            sync_operation_id: None,
        },
    );

    let outcome = fresh_runner.tick().expect("fresh tick");

    assert!(matches!(outcome, SyncTickOutcome::Imported(_)));
    let after_ref = control_plane
        .get_workspace_ref("ws_code")
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
    assert_eq!(
        after_import_metrics.full_read_count - before_import_metrics.full_read_count,
        2,
        "fresh import should full-read the encrypted manifest and source pack"
    );
    assert_eq!(
        after_import_metrics.range_read_count - before_import_metrics.range_read_count,
        0,
        "fresh import should hydrate from the prefetched source pack instead of per-file range reads"
    );
    let metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
        .expect("fresh import metadata");
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
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/src/main.ts")
            .expect("projected node lookup")
            .expect("projected node")
            .hydration_state,
        HydrationState::Local
    );
    assert_eq!(
        metadata
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/package.json")
            .expect("package projected node lookup")
            .expect("package projected node")
            .hydration_state,
        HydrationState::Local,
        "materialized files are real local files, not cold placeholders"
    );
    assert!(
        metadata
            .hydration_queue(&WorkspaceId::new("ws_code"))
            .expect("hydration queue")
            .iter()
            .any(|record| record.path == "app/src/main.ts" && record.state == "completed"),
        "remote import should settle queued hot-project hydration after writing real bytes"
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
    let (deleted_ref, deleted_object_manifest) = match deletion_outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced {
                workspace_ref,
                object_manifest,
            } => (workspace_ref, object_manifest),
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("package deletion should advance")
            }
        },
        other => panic!("expected package deletion upload, got {other:?}"),
    };
    assert_eq!(
        deleted_object_manifest.pack_objects.len(),
        1,
        "uploads that preserve cold locators must retain their source pack pointers"
    );
    let after_package_delete = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(deleted_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import after package delete");
    assert!(
        !after_package_delete
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/package.json"),
        "deleting a bootstrap-materialized file must sync as a deletion"
    );
    assert!(
        metadata
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/package.json")
            .expect("deleted package projected node lookup")
            .is_none(),
        "accepted local deletions should remove projected-node metadata"
    );
    assert!(
        after_package_delete
            .manifest
            .entries
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
        1,
    )
    .expect("import after edit");
    assert!(
        edited_snapshot
            .manifest
            .entries
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
        1,
    )
    .expect("import after source delete");
    assert!(
        !deleted_cold_snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/src/main.ts"),
        "deleting a materialized source file must sync as a deletion"
    );
    assert!(
        metadata
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/src/main.ts")
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
        1,
    )
    .expect("import after directory replacement");
    assert!(
        replaced_dir_snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app" && entry.kind == NamespaceEntryKind::File),
        "local file replacement should be represented as the path itself"
    );
    assert!(
        !replaced_dir_snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path.starts_with("app/")),
        "preserved cold descendants must not survive under a local file replacement"
    );
}

#[test]
fn fresh_import_keeps_large_lazy_files_cold_without_pack_prefetch() {
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
            sync_operation_id: None,
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
            sync_operation_id: None,
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
    assert!(
        !fresh.root().join("app/assets/video.bin").exists(),
        "large lazy files should stay locator-backed instead of materializing during import"
    );
    let after_import_metrics = byte_store.metrics();
    assert_eq!(
        after_import_metrics.full_read_count - before_import_metrics.full_read_count,
        1,
        "fresh import should full-read only the encrypted manifest when a pack also contains cold lazy records"
    );
    assert_eq!(
        after_import_metrics.range_read_count - before_import_metrics.range_read_count,
        1,
        "fresh import should range-read only the eager source record from a mixed pack"
    );

    let metadata =
        MetadataStore::open(fresh_state.root().join("local.sqlite3")).expect("fresh metadata");
    let large_node = metadata
        .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/assets/video.bin")
        .expect("large projected node lookup")
        .expect("large projected node");
    assert_eq!(large_node.hydration_state, HydrationState::Cold);
    let locators = metadata
        .content_locators(&WorkspaceId::new("ws_code"))
        .expect("content locators");
    assert!(
        locators
            .iter()
            .any(|locator| large_node.content_id.as_ref() == Some(&locator.content_id)),
        "cold large file should keep a persisted locator for later active-read hydration"
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
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let uploaded_ref = control_plane
        .get_workspace_ref("ws_code")
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
            sync_operation_id: None,
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
        1,
    )
    .expect("import merged");
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/remote.ts")
    );
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/local.ts")
    );
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
    assert!(candidate.snapshot.manifest.entries.iter().any(|entry| {
        entry.path == "app/.git/refs/heads/main" && entry.mode == MaterializationMode::EncryptedSync
    }));
    assert!(candidate.snapshot.manifest.entries.iter().any(|entry| {
        entry.path == "app/.git/objects/ab/cdef" && entry.mode == MaterializationMode::EncryptedSync
    }));
    assert!(candidate.snapshot.manifest.entries.iter().any(|entry| {
        entry.path == "app/.git/objects/pack/pack-main-001.pack"
            && entry.mode == MaterializationMode::EncryptedSync
    }));
    for path in [
        "app/.git/packed-refs",
        "app/.git/refs/stash",
        "app/.git/hooks/pre-commit",
        "app/.git/modules/lib/HEAD",
        "app/.git/lfs/objects/aa/bb/oid",
    ] {
        assert!(
            candidate
                .snapshot
                .manifest
                .entries
                .iter()
                .any(|entry| entry.path == path && entry.mode == MaterializationMode::EncryptedSync),
            "{path} should sync as opaque Git state"
        );
    }
    assert!(
        !candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/.git/objects/pack/tmp_pack_001"),
        "bounded Git transients must stay local-only and out of workspace head"
    );

    let outcome =
        upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
            .expect("upload");
    let object_manifest = match outcome {
        bowline_local::sync::UploadOutcome::Advanced {
            object_manifest, ..
        } => object_manifest,
        bowline_local::sync::UploadOutcome::Stale { .. } => {
            panic!("first writer should advance")
        }
    };
    assert_eq!(
        object_manifest.pack_objects.len(),
        1,
        ".git entries should pack with ordinary workspace bytes, not one remote object per Git file"
    );
    for pointer in
        std::iter::once(&object_manifest.manifest_object).chain(object_manifest.pack_objects.iter())
    {
        for forbidden in [".git", "refs", "heads", "main", "pack-main"] {
            assert!(
                !pointer.object_key.contains(forbidden),
                "object key leaked Git path detail `{forbidden}`"
            );
        }
    }

    let imported = import_snapshot_by_id(
        &workspace_id,
        &candidate.snapshot.manifest.snapshot_id,
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import");
    let pack_entry = imported
        .manifest
        .entries
        .iter()
        .find(|entry| entry.path == "app/.git/objects/pack/pack-main-001.pack")
        .expect("imported pack entry");
    let locator = pack_entry.locator.as_ref().expect("packed locator");
    let pack_id = locator.pack_id.as_ref().expect("pack id");
    let pack_object = imported
        .pack_objects
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
                locator,
                content_key,
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
            sync_operation_id: None,
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
            .projected_node_by_path(&workspace_id, "app/.git/objects/pack/pack-main-001.pack")
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
    let status = compose_status(StatusOptions {
        db_path: Some(fresh_state.root().join("local.sqlite3")),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-26T12:01:01Z".to_string(),
    })
    .expect("status composes");
    assert!(
        status.hydration_progress.iter().any(|progress| {
            progress.cause == "remote-import"
                && progress.bytes_done > 0
                && progress.bytes_remaining == 0
        }),
        "hydration progress: {:#?}; events: {events:#?}",
        status.hydration_progress
    );
}

#[test]
fn missing_remote_materialization_records_blocked_hydration_event() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    control_plane
        .compare_and_swap_workspace_ref(
            workspace_id.as_str(),
            base_ref.version,
            "snap_missing_manifest",
            "device-a",
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
            sync_operation_id: None,
        },
    );

    let error = runner.tick().expect_err("missing manifest blocks import");
    assert!(error.to_string().contains("snapshot manifest"));

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
                    .is_some_and(|reason| reason.contains("snapshot manifest"))
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
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_git_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
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
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: remote.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
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
            candidate.snapshot.manifest.entries
        ),
        MergeOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].paths, vec!["app/.git/config"]);
            assert_eq!(conflicts[0].reason, "opaque Git state conflict");
        }
    }
}
