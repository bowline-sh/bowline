use bowline_control_plane::{
    ObjectControlPlaneClient, WorkViewControlPlaneClient, WorkspaceControlPlaneClient,
};
use bowline_core::{
    ids::{ContentId, DeviceId, ProjectId, SnapshotId, WorkViewId, WorkspaceId},
    work_views::OVERLAY_HEAD_EMPTY,
};
use bowline_storage::ByteStore;

use crate::work_views::overlay_wire::OVERLAY_CHUNK_BYTES;
use crate::work_views::{
    WorkViewOverlaySyncError,
    overlay_receive::{overlay_manifest_matches_local, read_overlay_manifest},
    overlay_sync::OverlayUploadPlan,
    overlay_upload::build_overlay_manifest,
    sync_local_work_view_overlays_with_checkpoint,
};

use super::*;

fn committed_overlay_manifest(
    control_plane: &FakeControlPlaneClient,
    byte_store: &LocalByteStore,
    db_path: &Path,
    work_view_id: &WorkViewId,
    storage_key: StorageKey,
) -> crate::work_views::overlay_wire::OverlayManifest {
    let store = MetadataStore::open(db_path).expect("writer metadata");
    let work_view = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), work_view_id)
        .expect("writer view lookup")
        .expect("writer view persists");
    let pointer = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == *work_view_id)
        .and_then(|view| view.overlay_head)
        .expect("remote overlay pointer");
    read_overlay_manifest(
        byte_store,
        &WorkViewOverlaySyncOptions {
            db_path: db_path.to_path_buf(),
            device_id: DeviceId::new("device-writer"),
            workspace_content_key: [7_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &work_view,
        &pointer,
    )
    .expect("remote overlay manifest")
}

#[test]
fn overlay_scan_stops_at_the_next_durable_cancellation_checkpoint() {
    let (temp, db_path) = seeded_store("overlay-cancelled-scan");
    let project = temp.root().join("Code/apps/web");
    fs::create_dir_all(project.join("src")).expect("project src");
    fs::write(project.join("src/base.ts"), "export const base = true;").expect("base file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project.display().to_string(),
        name: "cancelled-scan".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-cancel")),
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/cancelled-scan/src");
    for ordinal in 0..64 {
        fs::write(
            work_root.join(format!("changed-{ordinal:02}.bin")),
            vec![ordinal as u8; 128 * 1024],
        )
        .expect("changed file");
    }
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-cancel"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 244)
        .expect("byte store");
    let mut checkpoints = 0_u32;
    let error = sync_local_work_view_overlays_with_checkpoint(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-cancel"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::deterministic(244),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
        || {
            checkpoints = checkpoints.saturating_add(1);
            if checkpoints >= 12 {
                Err(WorkViewOverlaySyncError::CancellationRequested)
            } else {
                Ok(())
            }
        },
    )
    .expect_err("cancelled scan stops before publication");

    assert!(matches!(
        error,
        WorkViewOverlaySyncError::CancellationRequested
    ));
    assert_eq!(
        control_plane
            .object_pointers("ws_code")
            .into_iter()
            .filter(|pointer| pointer.kind == ControlObjectKind::AgentOverlay)
            .count(),
        0
    );

    let error = sync_local_work_view_overlays_with_checkpoint(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-cancel"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::deterministic(244),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
        || {
            if control_plane
                .object_pointers("ws_code")
                .iter()
                .any(|pointer| pointer.kind == ControlObjectKind::AgentOverlay)
            {
                Err(WorkViewOverlaySyncError::CancellationRequested)
            } else {
                Ok(())
            }
        },
    )
    .expect_err("cancellation after root upload stops before overlay commit");
    assert!(matches!(
        error,
        WorkViewOverlaySyncError::CancellationRequested
    ));
    let uploaded_roots = control_plane
        .object_pointers("ws_code")
        .into_iter()
        .filter(|pointer| pointer.kind == ControlObjectKind::AgentOverlay)
        .collect::<Vec<_>>();
    assert!(!uploaded_roots.is_empty());
    for pointer in uploaded_roots {
        assert_eq!(
            control_plane
                .head_object_metadata(&WorkspaceId::new("ws_code"), &pointer.object_key)
                .expect("uploaded root metadata")
                .retention_state,
            bowline_storage::RetentionState::OrphanCandidate,
            "an uncommitted overlay root must enter orphan recovery"
        );
    }
}

#[test]
fn ten_thousand_small_files_upload_with_changed_scope_counters() {
    const FILE_COUNT: usize = 10_000;
    let (temp, db_path) = seeded_store("overlay-v2-small-file-scale");
    let project = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project.display().to_string(),
        name: "small-file-scale".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-scale")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/small-file-scale");
    fs::create_dir_all(materialized.join("small")).expect("small-file directory");
    for ordinal in 0..FILE_COUNT {
        fs::write(materialized.join(format!("small/{ordinal:05}.txt")), b"x")
            .expect("small fixture file");
    }
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-scale"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 211)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-scale"),
            workspace_content_key: [11_u8; 32],
            storage_key: StorageKey::deterministic(211),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("scale overlay sync");

    assert_eq!(report.entries_total, FILE_COUNT);
    assert_eq!(report.entries_completed, FILE_COUNT);
    assert_eq!(report.content_objects_uploaded, 1);
    assert_eq!(report.content_objects_reused, FILE_COUNT - 1);
    assert_eq!(report.plaintext_bytes, FILE_COUNT as u64);
    assert!(
        byte_store.metrics().peak_object_bytes_in_flight
            < (OVERLAY_CHUNK_BYTES as u64 + 1024 * 1024)
    );
}

#[test]
fn two_devices_round_trip_large_content_addressed_overlay_with_bounded_objects() {
    let (writer_temp, writer_db) = seeded_store("overlay-v2-writer");
    let (reader_temp, reader_db) = seeded_store("overlay-v2-reader");
    let writer_project = writer_temp.root().join("Code/apps/web");
    let reader_project = reader_temp.root().join("Code/apps/web");
    for project in [&writer_project, &reader_project] {
        fs::create_dir_all(project.join("src")).expect("project src");
        fs::write(project.join("src/large.bin"), b"base").expect("base file");
        fs::write(project.join(".env"), b"TOKEN=base\n").expect("base secret");
    }
    let writer_view = create_work_view(WorkCreateOptions {
        db_path: Some(writer_db.clone()),
        project_path: writer_project.display().to_string(),
        name: "large-round-trip".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-writer")),
        generated_at: now(),
    })
    .expect("writer view");
    let reader_view = create_work_view(WorkCreateOptions {
        db_path: Some(reader_db.clone()),
        project_path: reader_project.display().to_string(),
        name: "large-round-trip".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-reader")),
        generated_at: now(),
    })
    .expect("reader view");
    assert_eq!(writer_view.work_view.id, reader_view.work_view.id);
    let reader_local_only = reader_temp
        .root()
        .join("Code/.work/apps/web/large-round-trip/node_modules/local-state");
    fs::create_dir_all(reader_local_only.parent().expect("local-only parent"))
        .expect("local-only directory");
    fs::write(&reader_local_only, b"preserve me").expect("local-only state");
    let reader_empty_local_dir = reader_local_only
        .parent()
        .expect("local-only parent")
        .join("empty-cache");
    fs::create_dir(&reader_empty_local_dir).expect("empty local-only directory");

    let changed = vec![0x5a_u8; OVERLAY_CHUNK_BYTES * 2 + 17];
    let writer_file = writer_temp
        .root()
        .join("Code/.work/apps/web/large-round-trip/src/large.bin");
    let writer_secret = writer_temp
        .root()
        .join("Code/.work/apps/web/large-round-trip/.env");
    let secret_bytes = b"TOKEN=secret\n";
    fs::write(&writer_file, &changed).expect("large overlay");
    fs::write(&writer_secret, secret_bytes).expect("secret overlay");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-writer"),
        )
        .expect("base ref");
    let byte_store =
        LocalByteStore::open_deterministic(writer_temp.root().join(".state/objects"), 191)
            .expect("shared byte store");
    let storage_key = StorageKey::deterministic(191);
    let uploaded = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: writer_db.clone(),
            device_id: DeviceId::new("device-writer"),
            workspace_content_key: [7_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("writer upload");
    assert_eq!(uploaded.uploaded, 1);
    assert_eq!(uploaded.content_objects_uploaded, 4);
    assert_eq!(
        uploaded.plaintext_bytes,
        (changed.len() + secret_bytes.len()) as u64
    );
    assert!(
        byte_store.metrics().peak_object_bytes_in_flight
            < (OVERLAY_CHUNK_BYTES as u64 + 1024 * 1024)
    );
    let receipt = committed_overlay_manifest(
        &control_plane,
        &byte_store,
        &writer_db,
        &writer_view.work_view.id,
        storage_key,
    );
    let initial_chunk_keys =
        crate::work_views::overlay_retention::manifest_chunk_object_keys(&receipt);

    let range_reads_before_restore = byte_store.metrics().range_read_count;
    let cancelled = sync_local_work_view_overlays_with_checkpoint(
        WorkViewOverlaySyncOptions {
            db_path: reader_db.clone(),
            device_id: DeviceId::new("device-reader"),
            workspace_content_key: [7_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
        || {
            if byte_store.metrics().range_read_count >= range_reads_before_restore + 3 {
                Err(WorkViewOverlaySyncError::CancellationRequested)
            } else {
                Ok(())
            }
        },
    )
    .expect_err("reader cancellation stops remote materialization between chunks");
    assert!(matches!(
        cancelled,
        WorkViewOverlaySyncError::CancellationRequested
    ));
    assert_eq!(
        fs::read(
            reader_temp
                .root()
                .join("Code/.work/apps/web/large-round-trip/src/large.bin")
        )
        .expect("pre-commit reader file remains"),
        b"base"
    );

    let restored = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: reader_db.clone(),
            device_id: DeviceId::new("device-reader"),
            workspace_content_key: [7_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("reader restore");
    assert_eq!(
        restored.attention,
        0,
        "report: {restored:?}; reader: {:?}",
        MetadataStore::open(&reader_db)
            .expect("reader metadata")
            .work_view_by_id(&WorkspaceId::new("ws_code"), &reader_view.work_view.id)
            .expect("reader lookup")
    );
    assert_eq!(restored.entries_completed, 2);
    let reader_file = reader_temp
        .root()
        .join("Code/.work/apps/web/large-round-trip/src/large.bin");
    assert_eq!(fs::read(&reader_file).expect("restored bytes"), changed);
    assert_eq!(
        fs::read(&reader_local_only).expect("preserved local-only state"),
        b"preserve me"
    );
    assert!(reader_empty_local_dir.is_dir());
    let reader_secret = reader_temp
        .root()
        .join("Code/.work/apps/web/large-round-trip/.env");
    assert_eq!(
        fs::read(&reader_secret).expect("restored secret"),
        secret_bytes
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&reader_secret)
                .expect("secret metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    fs::write(&writer_file, b"base").expect("writer reverts overlay to exposed base");
    fs::write(&writer_secret, b"TOKEN=base\n").expect("writer reverts secret to exposed base");
    let reverted = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: writer_db,
            device_id: DeviceId::new("device-writer"),
            workspace_content_key: [7_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("writer publishes the successive empty overlay");
    assert_eq!(reverted.uploaded, 1);
    for object_key in initial_chunk_keys {
        assert_eq!(
            control_plane
                .head_object_metadata(&WorkspaceId::new("ws_code"), &object_key)
                .expect("superseded chunk metadata")
                .retention_state,
            StorageRetentionState::OrphanCandidate
        );
    }

    let advanced = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: reader_db,
            device_id: DeviceId::new("device-reader"),
            workspace_content_key: [7_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("reader advances from the prior receipt to the successive overlay");
    assert_eq!(advanced.attention, 0);
    assert_eq!(fs::read(reader_file).expect("restored base bytes"), b"base");
    assert_eq!(
        fs::read(reader_secret).expect("restored base secret"),
        b"TOKEN=base\n"
    );
    assert_eq!(
        fs::read(reader_local_only).expect("preserved local-only state after advance"),
        b"preserve me"
    );
    assert!(reader_empty_local_dir.is_dir());
}

#[test]
fn sync_uploads_changed_work_view_overlay_once() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-sync");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "remote-edit".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/remote-edit");
    fs::write(materialized.join("src/index.ts"), "console.log('overlay')").expect("overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 91)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(91);

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("overlay sync");

    assert_eq!(report.uploaded, 1);
    assert_eq!(report.attention, 0);
    let remote = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("remote work view");
    assert_eq!(remote.name, output.work_view.id.as_str());
    assert_eq!(
        remote.visible_path,
        format!(".work/{}", output.work_view.id.as_str())
    );
    let overlay = remote.overlay_head.expect("overlay head");
    assert_eq!(overlay.kind, ControlObjectKind::AgentOverlay);
    assert!(overlay.object_key.starts_with("packs_pk_"));
    assert!(!overlay.object_key.contains("apps"));
    assert!(!overlay.object_key.contains("remote-edit"));
    assert_eq!(
        control_plane
            .head_object_metadata(&WorkspaceId::new("ws_code"), &overlay.object_key)
            .expect("overlay metadata")
            .kind,
        StorageObjectKind::AgentOverlay
    );

    let store = MetadataStore::open(&db_path).expect("metadata");
    let mut synced = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert!(synced.overlay_head.starts_with("cid_"));
    assert_eq!(synced.overlay_version, 1);
    assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
    synced.overlay_head = OVERLAY_HEAD_EMPTY.to_string();
    synced.overlay_version = 0;
    store
        .upsert_work_view(&synced)
        .expect("simulate crash before local overlay state persisted");
    drop(store);

    let retry = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("retry reconciles already committed overlay object metadata");
    assert_eq!(retry.uploaded, 0);
    assert_eq!(retry.attention, 0);

    let idle = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("second overlay sync");
    assert_eq!(idle.uploaded, 0);
    assert_eq!(idle.attention, 0);

    let store = MetadataStore::open(&db_path).expect("metadata");
    let synced_before_revert = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    let previous_overlay_head = synced_before_revert.overlay_head.clone();
    drop(store);
    fs::write(materialized.join("src/index.ts"), "console.log('base')")
        .expect("revert overlay to base");
    let reverted = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("revert uploads empty overlay");
    assert_eq!(reverted.uploaded, 1);
    assert_eq!(reverted.attention, 0);
    let store = MetadataStore::open(&db_path).expect("metadata");
    let reverted_local = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_ne!(reverted_local.overlay_head, previous_overlay_head);
    assert_eq!(reverted_local.sync_state, WorkViewSyncState::Synced);
}

#[test]
fn sync_publishes_empty_work_view_metadata_without_uploading_bytes() {
    let (temp, db_path) = seeded_store("phase9-empty-work-view-publish");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "empty-remote".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 97)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(97),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("empty work view metadata sync");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 0);
    assert!(
        control_plane.object_pointers("ws_code").is_empty(),
        "metadata-only publication must not upload an overlay pack"
    );
    let remote = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("empty work view should be remotely visible");
    assert!(remote.overlay_head.is_none());
    assert_eq!(remote.name, output.work_view.id.as_str());
    assert_eq!(
        remote.visible_path,
        format!(".work/{}", output.work_view.id.as_str())
    );

    let store = MetadataStore::open(&db_path).expect("metadata");
    let synced = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
    drop(store);

    let idle = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(97),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("idle empty work view stays quiet");
    assert_eq!(idle.uploaded, 0);
    assert_eq!(idle.attention, 0);
}

#[test]
fn sync_ignores_local_regenerate_env_files_in_work_view_overlay() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-policy-ignore");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "deps-only".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let dependency_file = temp
        .root()
        .join("Code/.work/apps/web/deps-only/node_modules/lodash/index.js");
    fs::create_dir_all(dependency_file.parent().expect("dependency parent"))
        .expect("dependency dir");
    fs::write(&dependency_file, "module.exports = {}\n").expect("dependency artifact");
    for path in [
        "node_modules/pkg/.env",
        "target/debug/.ENV.Local",
        "dist/service.env",
        ".cache/tool/.Env",
    ] {
        let path = temp.root().join("Code/.work/apps/web/deps-only").join(path);
        fs::create_dir_all(path.parent().expect("local regenerate parent"))
            .expect("local regenerate directory");
        fs::write(path, "TOKEN=must-not-upload\n").expect("local regenerate env");
    }

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 102)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(102),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("local-regenerate-only work view sync");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 0);
    assert!(
        control_plane.object_pointers("ws_code").is_empty(),
        "local regenerate artifacts must not be packaged into overlay packs"
    );
    let remote = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("local-regenerate-only work view should still publish metadata");
    assert!(remote.overlay_head.is_none());
}

#[test]
fn sync_retries_empty_local_view_when_remote_overlay_is_unavailable() {
    let (temp, db_path) = seeded_store("phase9-empty-local-remote-overlay");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "remote-has-work".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: WorkspaceId::new("ws_code"),
            work_view_id: WorkViewId::new(output.work_view.id.as_str()),
            project_id: ProjectId::new("proj_web"),
            name: output.work_view.id.as_str().to_string(),
            visible_path: format!(".work/{}", output.work_view.id.as_str()),
            base_snapshot_id: SnapshotId::new("snap_project_base"),
            base_workspace_version: workspace_ref.version,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
        })
        .expect("remote work view");
    let remote_overlay = ObjectPointer {
        object_key: "packs_pk_0011223344556677".to_string(),
        content_id: ContentId::new("pack_remote_overlay"),
        byte_len: 16,
        hash: "b3_remote_overlay".to_string(),
        key_epoch: 1,
        kind: ControlObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp { tick: 7 },
    };
    control_plane.put_object_pointer("ws_code", remote_overlay.clone());
    control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: WorkspaceId::new("ws_code"),
            work_view_id: WorkViewId::new(output.work_view.id.as_str()),
            expected_overlay_version: 0,
            overlay_object: remote_overlay,
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("remote overlay commit");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 101)
        .expect("byte store");

    let error = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(101),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect_err("unavailable remote overlay remains retryable");

    assert!(error.to_string().contains("missing metadata"));
    let store = MetadataStore::open(&db_path).expect("metadata");
    let retryable = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_ne!(retryable.sync_state, WorkViewSyncState::Attention);
    assert!(retryable.attention.is_empty());
}

#[test]
fn sync_refuses_to_overwrite_unseen_remote_work_view_overlay() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-stale-local");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale-local".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let local_file = temp
        .root()
        .join("Code/.work/apps/web/stale-local/src/index.ts");
    fs::write(&local_file, "console.log('local edit')").expect("local overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: WorkspaceId::new("ws_code"),
            work_view_id: WorkViewId::new(output.work_view.id.as_str()),
            project_id: ProjectId::new("proj_web"),
            name: output.work_view.id.as_str().to_string(),
            visible_path: format!(".work/{}", output.work_view.id.as_str()),
            base_snapshot_id: SnapshotId::new("snap_project_base"),
            base_workspace_version: workspace_ref.version,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-2"),
        })
        .expect("remote work view");
    let remote_overlay =
        reserve_test_overlay_object(&control_plane, "ws_code", "overlay-remote", 8);
    control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: WorkspaceId::new("ws_code"),
            work_view_id: WorkViewId::new(output.work_view.id.as_str()),
            expected_overlay_version: 0,
            overlay_object: remote_overlay,
            committed_by_device_id: DeviceId::new("device-2"),
        })
        .expect("remote overlay commit");
    let object_count_before = control_plane.object_pointers("ws_code").len();
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 103)
        .expect("byte store");

    let error = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(103),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect_err("unavailable remote overlay should remain retryable");

    assert!(error.to_string().contains("missing metadata"));
    assert_eq!(
        control_plane.object_pointers("ws_code").len(),
        object_count_before,
        "stale local overlay should not upload a replacement object"
    );
    let store = MetadataStore::open(&db_path).expect("metadata");
    let retryable = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_ne!(retryable.sync_state, WorkViewSyncState::Attention);
    assert!(retryable.attention.is_empty());
}

#[test]
fn sync_retries_unavailable_remote_overlay_without_quarantining_view() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-remote-advance");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "remote-advance".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let local_file = temp
        .root()
        .join("Code/.work/apps/web/remote-advance/src/index.ts");
    fs::write(&local_file, "console.log('local edit')").expect("local overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 104)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(104);
    let initial = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("initial overlay upload");
    assert_eq!(initial.uploaded, 1);

    let remote_overlay = reserve_test_overlay_object(&control_plane, "ws_code", "overlay-peer", 9);
    control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: WorkspaceId::new("ws_code"),
            work_view_id: WorkViewId::new(output.work_view.id.as_str()),
            expected_overlay_version: 1,
            overlay_object: remote_overlay,
            committed_by_device_id: DeviceId::new("device-2"),
        })
        .expect("peer overlay commit");

    let error = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect_err("unavailable remote overlay should remain retryable");

    assert!(
        error.to_string().contains("missing metadata"),
        "unexpected retryable error: {error}"
    );
    let store = MetadataStore::open(&db_path).expect("metadata");
    let retryable = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_eq!(retryable.sync_state, WorkViewSyncState::Synced);
    assert_eq!(retryable.overlay_version, 1);
    assert!(retryable.attention.is_empty());
}

#[test]
fn sync_publishes_local_work_view_after_main_head_advances() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-historical-base");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "historical-base".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/historical-base");
    fs::write(materialized.join("src/index.ts"), "console.log('overlay')").expect("overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let base_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    commit_test_snapshot_manifest(&control_plane, "ws_code", "snap_project_base", "device-1");
    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            base_ref.version,
            &SnapshotId::new("snap_project_next"),
            &DeviceId::new("device-1"),
        )
        .expect("advanced ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 99)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(99),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &advanced_ref,
    )
    .expect("historical-base work view should still publish");

    assert_eq!(report.uploaded, 1);
    assert_eq!(report.attention, 0);
    let remote = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("remote work view");
    assert_eq!(remote.base_snapshot_id, "snap_project_base");
    assert_eq!(remote.base_workspace_version, 0);
    assert!(remote.overlay_head.is_some());
}

#[test]
fn sync_does_not_upload_main_head_changes_as_work_view_overlay() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-main-head-noise");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "untouched-work".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    fs::write(
        project_path.join("src/index.ts"),
        "console.log('main advanced')",
    )
    .expect("main project edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let base_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    commit_test_snapshot_manifest(&control_plane, "ws_code", "snap_project_base", "device-1");
    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            base_ref.version,
            &SnapshotId::new("snap_project_next"),
            &DeviceId::new("device-1"),
        )
        .expect("advanced ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 100)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(100),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &advanced_ref,
    )
    .expect("untouched historical work view should publish metadata only");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 0);
    assert_eq!(
        control_plane
            .object_pointers("ws_code")
            .into_iter()
            .filter(|pointer| pointer.kind == ControlObjectKind::AgentOverlay)
            .count(),
        0,
        "main project changes must not become work-view overlay payloads"
    );
    let remote = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("remote work view");
    assert_eq!(remote.base_snapshot_id, "snap_project_base");
    assert!(remote.overlay_head.is_none());
}

#[test]
fn sync_records_delete_when_base_file_becomes_directory() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-file-to-dir");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "file-to-dir".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let replaced = temp
        .root()
        .join("Code/.work/apps/web/file-to-dir/src/index.ts");
    fs::remove_file(&replaced).expect("remove base file in work view");
    fs::create_dir_all(&replaced).expect("replacement dir");
    fs::write(replaced.join("nested.ts"), "console.log('nested')").expect("nested file");

    let store = MetadataStore::open(&db_path).expect("metadata");
    let work_view = store
        .work_views(&WorkspaceId::new("ws_code"), true, None)
        .expect("work views")
        .into_iter()
        .find(|view| view.name == "file-to-dir")
        .expect("work view");
    let upload_plan = overlay_deltas_for_upload(&store, &work_view).expect("overlay deltas");
    let paths = upload_plan
        .deltas
        .iter()
        .map(|delta| {
            (
                delta.path.display().to_string(),
                overlay_delta_kind_name(&delta.kind),
            )
        })
        .collect::<Vec<_>>();

    assert!(paths.contains(&("src/index.ts".to_string(), "delete")));
    assert!(paths.contains(&("src/index.ts/nested.ts".to_string(), "create")));
}

#[test]
fn overlay_upload_prefers_staged_delta_over_filesystem_delta() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-staged-wins");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        base_snapshot_selector: None,
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "staged-wins".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let staged_path = temp
        .root()
        .join("Code/.work/apps/web/staged-wins/src/index.ts");
    fs::write(&staged_path, "console.log('filesystem')").expect("filesystem edit");
    let staged_bytes = b"console.log('staged')";
    let staged_content_id =
        bowline_core::workspace_graph::workspace_content_id([0_u8; 32], staged_bytes);
    let cache = LocalContentCache::open(temp.root().join(".state/cache")).expect("content cache");
    cache
        .put_content(&staged_content_id, staged_bytes)
        .expect("staged content");
    cache
        .get_content(&staged_content_id, [0_u8; 32])
        .expect("verified staged content");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-staged-modify".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&staged_path),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: Some(staged_content_id.clone()),
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2999-01-01T00:00:00Z".to_string(),
            created_at: "2999-01-01T00:00:00Z".to_string(),
        })
        .expect("write log");

    let upload_plan = overlay_deltas_for_upload(&store, &output.work_view).expect("overlay deltas");
    let delta = upload_plan
        .deltas
        .iter()
        .find(|delta| delta.path.as_path() == std::path::Path::new("src/index.ts"))
        .expect("index delta");
    assert_eq!(overlay_delta_kind_name(&delta.kind), "modify");
    assert_eq!(delta.write_id.as_deref(), Some("write-staged-modify"));

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 199)
        .expect("byte store");
    let options = WorkViewOverlaySyncOptions {
        db_path,
        device_id: DeviceId::new("device-1"),
        workspace_content_key: [0_u8; 32],
        storage_key: StorageKey::deterministic(199),
        key_epoch: 1,
        generated_at: now(),
    };
    let uploaded = upload_staged_content(
        &store,
        &control_plane,
        &byte_store,
        &options,
        &output.work_view,
        &staged_content_id,
    )
    .expect("staged upload");
    assert_eq!(uploaded.content.content_id, staged_content_id);
    assert_eq!(uploaded.content.byte_len, staged_bytes.len() as u64);
    let built = build_overlay_manifest(
        &store,
        &control_plane,
        &byte_store,
        &options,
        &output.work_view,
        &upload_plan,
    )
    .expect("staged manifest");
    assert!(
        overlay_manifest_matches_local(&built.manifest, &options, &output.work_view, &upload_plan,)
            .expect("staged reconciliation")
    );
}

#[test]
fn overlay_upload_keeps_newer_filesystem_delta_over_staged_delta() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-filesystem-wins");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        base_snapshot_selector: None,
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "filesystem-wins".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let staged_path = temp
        .root()
        .join("Code/.work/apps/web/filesystem-wins/src/index.ts");
    fs::write(&staged_path, "console.log('filesystem')").expect("filesystem edit");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-old-staged-modify".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&staged_path),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: Some(ContentId::new("cid_old_staged")),
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2000-01-01T00:00:00Z".to_string(),
            created_at: "2000-01-01T00:00:00Z".to_string(),
        })
        .expect("write log");

    let upload_plan = overlay_deltas_for_upload(&store, &output.work_view).expect("overlay deltas");
    let delta = upload_plan
        .deltas
        .iter()
        .find(|delta| delta.path.as_path() == std::path::Path::new("src/index.ts"))
        .expect("index delta");
    assert_eq!(overlay_delta_kind_name(&delta.kind), "modify");
    assert_eq!(delta.write_id, None);
}

#[test]
fn overlay_upload_collapses_rename_create_and_source_delete_to_one_entry() {
    let (temp, db_path) = seeded_store("overlay-v2-rename-collapse");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/old.ts"), "export const value = 1;").expect("base file");
    let output = create_work_view(WorkCreateOptions {
        base_snapshot_selector: None,
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "rename-collapse".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/rename-collapse/src");
    let old_path = work_root.join("old.ts");
    let new_path = work_root.join("new.ts");
    fs::rename(&old_path, &new_path).expect("rename file");
    let bytes = fs::read(&new_path).expect("renamed bytes");
    let content_id = bowline_core::workspace_graph::workspace_content_id([0_u8; 32], &bytes);
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-rename".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&new_path),
            source_path: Some(display(&old_path)),
            operation: "rename".to_string(),
            staged_content_id: Some(content_id),
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2000-01-01T00:00:00Z".to_string(),
            created_at: "2000-01-01T00:00:00Z".to_string(),
        })
        .expect("rename log");

    let plan = overlay_deltas_for_upload(&store, &output.work_view).expect("overlay plan");
    assert_eq!(plan.deltas.len(), 1);
    assert_eq!(plan.deltas[0].path, std::path::Path::new("src/new.ts"));
    assert!(matches!(
        plan.deltas[0].kind,
        crate::work_views::overlay::OverlayDeltaKind::Rename { ref from }
            if from == std::path::Path::new("src/old.ts")
    ));
    assert_eq!(plan.deltas[0].write_id, None);
}

#[test]
fn overlay_upload_encodes_rename_of_overlay_created_file_as_create() {
    let (temp, db_path) = seeded_store("overlay-v2-created-rename");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/base.ts"), "export const base = 1;").expect("base file");
    let output = create_work_view(WorkCreateOptions {
        base_snapshot_selector: None,
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "created-rename".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/created-rename/src");
    let old_path = work_root.join("overlay-old.ts");
    let new_path = work_root.join("overlay-new.ts");
    fs::write(&old_path, "export const overlay = 1;").expect("overlay-created source");
    fs::rename(&old_path, &new_path).expect("rename overlay-created file");
    let bytes = fs::read(&new_path).expect("renamed bytes");
    let content_id = bowline_core::workspace_graph::workspace_content_id([0_u8; 32], &bytes);
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-created-rename".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&new_path),
            source_path: Some(display(&old_path)),
            operation: "rename".to_string(),
            staged_content_id: Some(content_id),
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2999-01-01T00:00:00Z".to_string(),
            created_at: "2999-01-01T00:00:00Z".to_string(),
        })
        .expect("rename log");

    let plan = overlay_deltas_for_upload(&store, &output.work_view).expect("overlay plan");
    assert_eq!(plan.deltas.len(), 1);
    assert_eq!(
        plan.deltas[0].path,
        std::path::Path::new("src/overlay-new.ts")
    );
    assert!(matches!(
        plan.deltas[0].kind,
        crate::work_views::overlay::OverlayDeltaKind::Create
    ));
}

#[test]
fn overlay_manifest_failure_retires_chunks_uploaded_before_the_error() {
    let (temp, db_path) = seeded_store("overlay-v2-partial-build-cleanup");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/base.ts"), "export const base = 1;").expect("base file");
    let output = create_work_view(WorkCreateOptions {
        base_snapshot_selector: None,
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "partial-build".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/partial-build");
    fs::write(work_root.join("src/a.ts"), "export const a = 1;").expect("first overlay file");
    let plan = OverlayUploadPlan {
        deltas: vec![
            crate::work_views::overlay::OverlayDelta {
                path: "src/a.ts".into(),
                kind: crate::work_views::overlay::OverlayDeltaKind::Create,
                contains_secrets: false,
                write_id: None,
            },
            crate::work_views::overlay::OverlayDelta {
                path: "src/z-missing.ts".into(),
                kind: crate::work_views::overlay::OverlayDeltaKind::Create,
                contains_secrets: false,
                write_id: None,
            },
        ],
        staged_content_by_write_id: std::collections::BTreeMap::new(),
    };
    let store = MetadataStore::open(&db_path).expect("metadata");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 211)
        .expect("byte store");
    let error = match build_overlay_manifest(
        &store,
        &control_plane,
        &byte_store,
        &WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(211),
            key_epoch: 1,
            generated_at: now(),
        },
        &output.work_view,
        &plan,
    ) {
        Ok(_) => panic!("second missing file should abort manifest construction"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("No such file") || error.to_string().contains("not found"));
    let pointers = control_plane.object_pointers("ws_code");
    assert_eq!(pointers.len(), 1);
    assert_eq!(
        control_plane
            .head_object_metadata(&WorkspaceId::new("ws_code"), &pointers[0].object_key)
            .expect("partial chunk metadata")
            .retention_state,
        StorageRetentionState::OrphanCandidate
    );
}

#[test]
fn sync_treats_same_object_stale_overlay_as_converged() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-same-object-stale");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "same-object".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/same-object");
    fs::write(materialized.join("src/index.ts"), "console.log('overlay')").expect("overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    control_plane.make_next_overlay_commit_stale_with_same_object_for_harness(
        "ws_code",
        output.work_view.id.as_str(),
    );
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 98)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(98);

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("same-object stale overlay converges");

    assert_eq!(report.uploaded, 1);
    assert_eq!(report.attention, 0);
    let remote = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("remote work view");
    let overlay = remote.overlay_head.expect("overlay head");
    assert_eq!(
        control_plane
            .head_object_metadata(&WorkspaceId::new("ws_code"), &overlay.object_key)
            .expect("overlay metadata")
            .retention_state,
        StorageRetentionState::Current
    );

    let store = MetadataStore::open(&db_path).expect("metadata");
    let synced = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
    assert!(synced.overlay_head.starts_with("cid_"));
}

#[test]
fn sync_encrypts_case_variant_env_work_view_overlay() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-secret");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");
    fs::write(project_path.join(".ENV.Local"), "TOKEN=base").expect("base env");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "env-edit".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/env-edit");
    assert_eq!(
        fs::read_to_string(materialized.join(".ENV.Local")).expect("materialized base env"),
        "TOKEN=base"
    );
    fs::write(materialized.join(".ENV.Local"), "TOKEN=secret").expect("work env");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 92)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(92),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("overlay sync");

    assert_eq!(report.uploaded, 1);
    assert_eq!(report.attention, 0);
    let remote = control_plane
        .list_work_views(&WorkspaceId::new("ws_code"), true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("remote work view");
    let overlay = remote.overlay_head.expect("encrypted overlay");
    assert!(overlay.object_key.starts_with("packs_pk_"));

    let store = MetadataStore::open(&db_path).expect("metadata");
    let synced = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
    assert!(synced.attention.is_empty());
}

#[test]
fn sync_marks_symlink_work_view_overlay_attention_without_aborting() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-symlink");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "link-edit".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/link-edit");
    symlink("src/index.ts", materialized.join("linked.ts")).expect("work symlink");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 93)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(93),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("symlink should not abort workspace sync");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 1);
    let store = MetadataStore::open(&db_path).expect("metadata");
    let blocked = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_eq!(blocked.sync_state, WorkViewSyncState::Attention);
    assert!(
        blocked
            .attention
            .iter()
            .any(|item| item.contains("needs review before sync"))
    );
}

#[test]
fn sync_ignores_stale_create_log_when_file_no_longer_exists() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-stale-log");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale-log".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-stale-create".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&temp.root().join("Code/.work/apps/web/stale-log/ghost.ts")),
            source_path: None,
            operation: "create".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: now(),
            created_at: now(),
        })
        .expect("write log");
    drop(store);

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 94)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(94),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("stale log should not abort sync");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 0);
}

#[test]
fn sync_skips_attention_work_view_overlay_until_user_review() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-attention-skip");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale-remote".to_string(),
        base_snapshot_selector: None,
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    fs::write(
        temp.root()
            .join("Code/.work/apps/web/stale-remote/src/index.ts"),
        "console.log('local')",
    )
    .expect("overlay edit");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let mut blocked = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("work view");
    blocked.sync_state = WorkViewSyncState::Attention;
    blocked.attention = vec!["Remote overlay changed; review required.".to_string()];
    store.upsert_work_view(&blocked).expect("attention view");
    drop(store);

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("ws_code"),
            0,
            &SnapshotId::new("snap_project_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 95)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
            workspace_content_key: [0_u8; 32],
            storage_key: StorageKey::deterministic(95),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("attention view should not retry");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 0);
    assert!(
        control_plane
            .list_work_views(&WorkspaceId::new("ws_code"), true)
            .expect("remote work views")
            .is_empty()
    );
}
