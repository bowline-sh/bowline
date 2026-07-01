use super::*;
use std::os::unix::fs::PermissionsExt;

use bowline_core::{
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{ContentLocator, ContentStorage, HydrationState, NamespaceEntry},
};

use crate::{
    metadata::{MetadataStore, WorkspaceSyncHeadRecord},
    workspace::TempWorkspace,
};

#[test]
fn materialize_snapshot_replaces_symlink_parents_without_following_them() {
    let workspace = TempWorkspace::new("sync-materialize-symlink-parent").expect("workspace");
    let outside = TempWorkspace::new("sync-materialize-outside").expect("outside");
    std::os::unix::fs::symlink(outside.root(), workspace.root().join("app"))
        .expect("symlink parent");
    let snapshot = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "app/src/main.ts",
        b"export const value = 1;\n",
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert!(
        outside
            .root()
            .join("src")
            .join("main.ts")
            .metadata()
            .is_err(),
        "materialization must not write through a local symlink parent"
    );
    assert_eq!(
        fs::read(workspace.root().join("app").join("src").join("main.ts")).expect("workspace file"),
        b"export const value = 1;\n"
    );
    assert!(
        !fs::symlink_metadata(workspace.root().join("app"))
            .expect("app metadata")
            .file_type()
            .is_symlink(),
        "symlink parent should be replaced with a real directory"
    );
}

#[test]
fn materialize_snapshot_rejects_symlink_targets_outside_workspace() {
    let workspace = TempWorkspace::new("sync-materialize-bad-symlink").expect("workspace");
    let snapshot = snapshot_with_symlink(
        WorkspaceId::new("ws_code"),
        "app/config",
        "/workspace/user/.ssh/config",
    );

    let error =
        materialize_snapshot(workspace.root(), None, &snapshot).expect_err("unsafe symlink");

    assert!(matches!(
        error,
        SyncRunnerError::UnsafeMaterializationPath(_)
    ));
    assert!(
        fs::symlink_metadata(workspace.root().join("app").join("config")).is_err(),
        "unsafe symlink target must not be materialized"
    );
}

#[test]
fn materialize_snapshot_writes_secret_bearing_files_owner_only() {
    let workspace = TempWorkspace::new("sync-materialize-env-permissions").expect("workspace");
    let snapshot = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "app/.env.local",
        b"SECRET=value\n",
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    let mode = fs::metadata(workspace.root().join("app").join(".env.local"))
        .expect("env metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn materialize_snapshot_replaces_files_with_atomic_temp_rename() {
    let workspace = TempWorkspace::new("sync-materialize-atomic-file").expect("workspace");
    let destination = workspace.root().join("app/src/index.ts");
    fs::create_dir_all(destination.parent().expect("destination parent")).expect("parent");
    fs::write(&destination, b"old bytes stay until rename\n").expect("old file");
    let stale_temp = materialization_temp_path(&destination).expect("temp path");
    fs::write(&stale_temp, b"crashed temp bytes\n").expect("stale temp");
    let snapshot = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "app/src/index.ts",
        b"new materialized bytes\n",
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        fs::read(&destination).expect("destination bytes"),
        b"new materialized bytes\n"
    );
    assert!(
        fs::symlink_metadata(&stale_temp).is_err(),
        "stale materialization temp file should be removed"
    );
}

#[test]
fn materialize_snapshot_replaces_symlinks_with_atomic_temp_rename() {
    let workspace = TempWorkspace::new("sync-materialize-atomic-symlink").expect("workspace");
    let destination = workspace.root().join("app/current");
    fs::create_dir_all(destination.parent().expect("destination parent")).expect("parent");
    std::os::unix::fs::symlink("old-target", &destination).expect("old symlink");
    let stale_temp = materialization_temp_path(&destination).expect("temp path");
    std::os::unix::fs::symlink("crashed-target", &stale_temp).expect("stale temp symlink");
    let snapshot = snapshot_with_symlink(WorkspaceId::new("ws_code"), "app/current", "src");

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        fs::read_link(&destination).expect("destination symlink"),
        PathBuf::from("src")
    );
    assert!(
        fs::symlink_metadata(&stale_temp).is_err(),
        "stale materialization temp symlink should be removed"
    );
}

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

    let candidate = super::super::coalescer::coalesce_workspace_scan(
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
            sync_operation_id: None,
        },
    );

    runner
        .persist_scan_metadata(&candidate)
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

    let candidate = super::super::coalescer::coalesce_workspace_scan(
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
            sync_operation_id: None,
        },
    );
    let accepted_remote = WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 7,
        snapshot_id: "snap_remote_committed".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 7 },
        updated_by_device_id: Some("device_remote".to_string()),
    };

    runner
        .persist_scan_metadata_if_committed(&candidate, &accepted_remote)
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
fn sync_runner_tolerates_stale_env_file_during_scan_metadata_persistence() {
    let workspace = TempWorkspace::new("sync-stale-env-metadata").expect("workspace");
    let state = TempWorkspace::new("sync-stale-env-state").expect("state");
    let project = workspace.root().join("app");
    fs::create_dir_all(project.join(".git")).expect("git marker");
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

    let candidate = super::super::coalescer::coalesce_workspace_scan(
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
            sync_operation_id: None,
        },
    );
    let accepted = WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device_local".to_string()),
    };

    runner
        .persist_scan_metadata_if_committed(&candidate, &accepted)
        .expect("stale env metadata import does not fail committed scan persistence");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    assert!(
        store
            .observed_summary(&workspace_id)
            .expect("summary lookup")
            .is_some()
    );
    assert!(
        store
            .env_records(&workspace_id)
            .expect("env records")
            .is_empty()
    );
}

fn snapshot_with_file(workspace_id: WorkspaceId, path: &str, bytes: &[u8]) -> SnapshotContent {
    let content_id = bowline_core::workspace_graph::workspace_content_id([3_u8; 32], bytes);
    SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: SnapshotId::new("snap_remote"),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: vec![NamespaceEntry {
                path: path.to_string(),
                kind: NamespaceEntryKind::File,
                classification: PathClassification::WorkspaceSync,
                mode: MaterializationMode::WorkspaceSync,
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                content_id: Some(content_id.clone()),
                locator: Some(ContentLocator {
                    content_id: content_id.clone(),
                    storage: ContentStorage::Packed,
                    raw_size: bytes.len() as u64,
                    pack_id: None,
                    offset: None,
                    length: None,
                    chunk_ids: Vec::new(),
                }),
                symlink_target: None,
                byte_len: Some(bytes.len() as u64),
                hydration_state: HydrationState::Local,
            }],
            refs: Vec::new(),
        },
        [(content_id, bytes.to_vec())].into_iter().collect(),
    )
}

fn snapshot_with_symlink(workspace_id: WorkspaceId, path: &str, target: &str) -> SnapshotContent {
    SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: SnapshotId::new("snap_remote"),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: vec![NamespaceEntry {
                path: path.to_string(),
                kind: NamespaceEntryKind::Symlink,
                classification: PathClassification::WorkspaceSync,
                mode: MaterializationMode::WorkspaceSync,
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                content_id: None,
                locator: None,
                symlink_target: Some(target.to_string()),
                byte_len: None,
                hydration_state: HydrationState::Local,
            }],
            refs: Vec::new(),
        },
        Default::default(),
    )
}
