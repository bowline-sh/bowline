use super::*;

#[test]
fn sync_uploads_changed_work_view_overlay_once() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-sync");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "remote-edit".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/remote-edit");
    fs::write(materialized.join("src/index.ts"), "console.log('overlay')").expect("overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 91)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(91);

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
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
        .list_work_views("ws_code", true)
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
            .head_object_metadata("ws_code", &overlay.object_key)
            .expect("overlay metadata")
            .kind,
        StorageObjectKind::AgentOverlay
    );

    let store = MetadataStore::open(&db_path).expect("metadata");
    let mut synced = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert!(synced.overlay_head.starts_with("b3_"));
    assert_eq!(synced.overlay_version, 1);
    assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
    synced.overlay_head = "overlay_empty".to_string();
    synced.overlay_version = 0;
    store
        .upsert_work_view(&synced)
        .expect("simulate crash before local overlay state persisted");
    drop(store);

    let retry = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
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

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "empty-remote".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 97)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
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
        .list_work_views("ws_code", true)
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
fn sync_ignores_dependency_artifacts_in_work_view_overlay() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-policy-ignore");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "deps-only".to_string(),
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

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 102)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            storage_key: StorageKey::deterministic(102),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("dependency-only work view sync");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 0);
    assert!(
        control_plane.object_pointers("ws_code").is_empty(),
        "dependency artifacts must not be packaged into overlay packs"
    );
    let remote = control_plane
        .list_work_views("ws_code", true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("dependency-only work view should still publish metadata");
    assert!(remote.overlay_head.is_none());
}

#[test]
fn sync_marks_empty_local_work_view_attention_when_remote_overlay_exists() {
    let (temp, db_path) = seeded_store("phase9-empty-local-remote-overlay");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "remote-has-work".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "ws_code".to_string(),
            work_view_id: output.work_view.id.as_str().to_string(),
            project_id: "proj_web".to_string(),
            name: output.work_view.id.as_str().to_string(),
            visible_path: format!(".work/{}", output.work_view.id.as_str()),
            base_snapshot_id: "snap_project_base".to_string(),
            base_workspace_version: workspace_ref.version,
            created_by_device_id: "device-1".to_string(),
        })
        .expect("remote work view");
    let remote_overlay = ObjectPointer {
        object_key: "packs_pk_0011223344556677".to_string(),
        content_id: "pack_remote_overlay".to_string(),
        byte_len: 16,
        hash: "b3_remote_overlay".to_string(),
        key_epoch: 1,
        kind: ControlObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp { tick: 7 },
    };
    control_plane.put_object_pointer("ws_code", remote_overlay.clone());
    control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: "ws_code".to_string(),
            work_view_id: output.work_view.id.as_str().to_string(),
            expected_overlay_version: 0,
            overlay_object: remote_overlay,
            committed_by_device_id: "device-1".to_string(),
        })
        .expect("remote overlay commit");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 101)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            storage_key: StorageKey::deterministic(101),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("empty local detects remote overlay");

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
            .any(|item| item.contains("Remote work view overlay changed"))
    );
}

#[test]
fn sync_refuses_to_overwrite_unseen_remote_work_view_overlay() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-stale-local");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale-local".to_string(),
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
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "ws_code".to_string(),
            work_view_id: output.work_view.id.as_str().to_string(),
            project_id: "proj_web".to_string(),
            name: output.work_view.id.as_str().to_string(),
            visible_path: format!(".work/{}", output.work_view.id.as_str()),
            base_snapshot_id: "snap_project_base".to_string(),
            base_workspace_version: workspace_ref.version,
            created_by_device_id: "device-2".to_string(),
        })
        .expect("remote work view");
    let remote_overlay =
        reserve_test_overlay_object(&control_plane, "ws_code", "overlay-remote", 8);
    control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: "ws_code".to_string(),
            work_view_id: output.work_view.id.as_str().to_string(),
            expected_overlay_version: 0,
            overlay_object: remote_overlay,
            committed_by_device_id: "device-2".to_string(),
        })
        .expect("remote overlay commit");
    let object_count_before = control_plane.object_pointers("ws_code").len();
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 103)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            storage_key: StorageKey::deterministic(103),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("stale local overlay should not overwrite remote");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 1);
    assert_eq!(
        control_plane.object_pointers("ws_code").len(),
        object_count_before,
        "stale local overlay should not upload a replacement object"
    );
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
            .any(|item| item.contains("last synced version 0"))
    );
}

#[test]
fn sync_detects_remote_overlay_advance_even_when_local_digest_matches() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-remote-advance");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "remote-advance".to_string(),
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
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 104)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(104);
    let initial = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
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
            workspace_id: "ws_code".to_string(),
            work_view_id: output.work_view.id.as_str().to_string(),
            expected_overlay_version: 1,
            overlay_object: remote_overlay,
            committed_by_device_id: "device-2".to_string(),
        })
        .expect("peer overlay commit");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            storage_key,
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("remote overlay advance should be noticed");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 1);
    let store = MetadataStore::open(&db_path).expect("metadata");
    let blocked = store
        .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("work view lookup")
        .expect("local work view");
    assert_eq!(blocked.sync_state, WorkViewSyncState::Attention);
    assert_eq!(blocked.overlay_version, 1);
    assert!(
        blocked
            .attention
            .iter()
            .any(|item| item.contains("last synced version 1"))
    );
}

#[test]
fn sync_publishes_local_work_view_after_main_head_advances() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-historical-base");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "historical-base".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/historical-base");
    fs::write(materialized.join("src/index.ts"), "console.log('overlay')").expect("overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let base_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    commit_test_snapshot_manifest(&control_plane, "ws_code", "snap_project_base", "device-1");
    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            "ws_code",
            base_ref.version,
            "snap_project_next",
            "device-1",
        )
        .expect("advanced ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 99)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
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
        .list_work_views("ws_code", true)
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

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "untouched-work".to_string(),
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
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    commit_test_snapshot_manifest(&control_plane, "ws_code", "snap_project_base", "device-1");
    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            "ws_code",
            base_ref.version,
            "snap_project_next",
            "device-1",
        )
        .expect("advanced ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 100)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
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
        .list_work_views("ws_code", true)
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

    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "file-to-dir".to_string(),
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
    let deltas = overlay_deltas_for_upload(&store, &work_view).expect("overlay deltas");
    let paths = deltas
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
fn sync_treats_same_object_stale_overlay_as_converged() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-same-object-stale");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "same-object".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/same-object");
    fs::write(materialized.join("src/index.ts"), "console.log('overlay')").expect("overlay edit");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
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
        .list_work_views("ws_code", true)
        .expect("remote work views")
        .into_iter()
        .find(|view| view.work_view_id == output.work_view.id.as_str())
        .expect("remote work view");
    let overlay = remote.overlay_head.expect("overlay head");
    assert_eq!(
        control_plane
            .head_object_metadata("ws_code", &overlay.object_key)
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
    assert!(synced.overlay_head.starts_with("b3_"));
}

#[test]
fn sync_blocks_secret_bearing_work_view_overlay_before_upload() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-secret");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "env-edit".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/env-edit");
    fs::write(materialized.join(".env.local"), "TOKEN=secret").expect("work env");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 92)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
            storage_key: StorageKey::deterministic(92),
            key_epoch: 1,
            generated_at: now(),
        },
        &control_plane,
        &byte_store,
        &workspace_ref,
    )
    .expect("overlay sync");

    assert_eq!(report.uploaded, 0);
    assert_eq!(report.attention, 1);
    assert!(
        control_plane
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .is_empty()
    );

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
            .any(|item| item.contains("review before overlay sync"))
    );
}

#[test]
fn sync_marks_symlink_work_view_overlay_attention_without_aborting() {
    let (temp, db_path) = seeded_store("phase9-work-overlay-symlink");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "link-edit".to_string(),
        owner_device_id: Some(DeviceId::new("device-1")),
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/link-edit");
    symlink("src/index.ts", materialized.join("linked.ts")).expect("work symlink");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let workspace_ref = control_plane
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 93)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path: db_path.clone(),
            device_id: DeviceId::new("device-1"),
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

    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale-log".to_string(),
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
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 94)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
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

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale-remote".to_string(),
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
        .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
        .expect("base ref");
    let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 95)
        .expect("byte store");

    let report = sync_local_work_view_overlays(
        WorkViewOverlaySyncOptions {
            db_path,
            device_id: DeviceId::new("device-1"),
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
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .is_empty()
    );
}
