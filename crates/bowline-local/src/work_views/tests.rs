use std::fs;
use std::os::unix::fs::symlink;

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneTimestamp, FakeControlPlaneClient,
    ObjectKind as ControlObjectKind, ObjectManifestCommit, ObjectPointer, UploadIntentRequest,
    WorkViewCreate, WorkViewOverlayCommit, WorkspaceRef,
};
use bowline_core::{
    commands::{AgentLeaseBase, AgentLeaseOutputState},
    ids::{DeviceId, ProjectId, SnapshotId, WorkspaceId},
    policy::PathClassification,
    status::StatusLevel,
    work_views::{WorkDiffChangeKind, WorkViewLifecycle, WorkViewSyncState, WorkViewVisibility},
};

use crate::{
    agents::{AgentLeaseCreateOptions, create_agent_lease},
    metadata::{LocalWriteLogRecord, MetadataStore, WorkspaceSyncHeadRecord},
    status::{StatusOptions, compose_status},
    workspace::TempWorkspace,
};

use super::{
    WorkCleanupOptions, WorkListOptions, WorkSelectorOptions, WorkViewError,
    WorkViewOverlaySyncOptions, WorkonOptions, accept_work_view, cleanup_work_views,
    create_work_view, diff_work_view, discard_work_view, list_work_views, overlay_delta_kind_name,
    overlay_deltas_for_upload, restore_work_view, sync_local_work_view_overlays,
};
use bowline_storage::{
    LocalByteStore, ObjectKind as StorageObjectKind, RetentionState as StorageRetentionState,
    StorageKey,
};

mod accept_diff;
mod create;
mod hardening;
mod lifecycle;
mod sync;

fn seeded_store(name: &str) -> (TempWorkspace, std::path::PathBuf) {
    seeded_store_with_snapshot(name, true)
}

fn seeded_store_without_snapshot(name: &str) -> (TempWorkspace, std::path::PathBuf) {
    seeded_store_with_snapshot(name, false)
}

fn seeded_store_with_snapshot(
    name: &str,
    project_has_snapshot: bool,
) -> (TempWorkspace, std::path::PathBuf) {
    let temp = TempWorkspace::new(name).expect("temp workspace");
    let code_root = temp.root().join("Code");
    fs::create_dir_all(code_root.join("apps/web")).expect("project dir");
    let db_path = temp.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T00:00:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T00:00:00Z",
        )
        .expect("project");
    if project_has_snapshot {
        store
            .set_project_latest_snapshot_id(
                &workspace_id,
                &project_id,
                &SnapshotId::new("snap_project_base"),
            )
            .expect("project latest snapshot");
    }
    drop(store);
    (temp, db_path)
}

fn commit_test_snapshot_manifest(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    snapshot_id: &str,
    device_id: &str,
) {
    let manifest_content_id = format!("content_manifest_{snapshot_id}");
    let pack_content_id = format!("content_pack_{snapshot_id}");
    let manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ControlObjectKind::SnapshotManifest, 64)
                .with_content_id(&manifest_content_id),
        )
        .expect("snapshot manifest upload intent");
    let pack_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ControlObjectKind::SourcePack, 256)
                .with_content_id(&pack_content_id),
        )
        .expect("source pack upload intent");

    control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: workspace_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
            manifest_id: format!("manifest_{snapshot_id}"),
            manifest_object: ObjectPointer {
                object_key: manifest_upload.object_key,
                content_id: manifest_content_id,
                byte_len: 64,
                hash: format!("b3_manifest_{snapshot_id}"),
                key_epoch: 1,
                kind: ControlObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 90 },
            },
            pack_objects: vec![ObjectPointer {
                object_key: pack_upload.object_key,
                content_id: pack_content_id,
                byte_len: 256,
                hash: format!("b3_pack_{snapshot_id}"),
                key_epoch: 1,
                kind: ControlObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 91 },
            }],
            committed_by_device_id: device_id.to_string(),
        })
        .expect("snapshot manifest commit");
}

fn reserve_test_overlay_object(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    content_id: &str,
    created_at_tick: u64,
) -> ObjectPointer {
    let upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ControlObjectKind::AgentOverlay, 512)
                .with_content_id(content_id),
        )
        .expect("overlay upload intent");
    ObjectPointer {
        object_key: upload.object_key,
        content_id: content_id.to_string(),
        byte_len: 512,
        hash: format!("b3_{content_id}"),
        key_epoch: 1,
        kind: ControlObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp {
            tick: created_at_tick,
        },
    }
}

fn now() -> String {
    "2026-06-25T12:00:00Z".to_string()
}

fn display(path: &std::path::Path) -> String {
    path.display().to_string()
}
