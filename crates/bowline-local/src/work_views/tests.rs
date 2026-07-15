use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{collections::BTreeMap, fs};

use bowline_control_plane::{
    ControlPlaneTimestamp, FakeControlPlaneClient, MetadataBindingCommit, MetadataBindingInput,
    MetadataRecordKind as ControlMetadataRecordKind, MetadataSidecar, ObjectControlPlaneClient,
    ObjectKind as ControlObjectKind, ObjectMetadataCommit, ObjectPointer, SnapshotRootCommit,
    UploadIntentRequest, WorkViewCreate, WorkViewOverlayCommit, WorkspaceRef,
};
use bowline_core::{
    commands::AgentLeaseBase,
    ids::{ContentId, DeviceId, ManifestId, ProjectId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::StatusLevel,
    work_views::{
        WorkCommandAction, WorkDiffChangeKind, WorkViewLifecycle, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    },
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, HydrationState, NamespaceEntry,
        NamespaceEntryKind, RefKind, SnapshotDraft, SnapshotKind,
        WorkspaceRef as GraphWorkspaceRef, workspace_content_id,
    },
};

use crate::{
    agents::{AgentLeaseCreateOptions, create_agent_lease},
    metadata::{LocalWriteLogRecord, MetadataStore, SnapshotRecord, WorkspaceSyncHeadRecord},
    status::{StatusOptions, compose_status},
    workspace::TempWorkspace,
};

use super::{
    PartialExposedBaseAdvance, WorkCleanupOptions, WorkCreateOptions, WorkListOptions,
    WorkSelectorOptions, WorkViewAcceptReview, WorkViewError, WorkViewOverlaySyncOptions,
    accept_journal::AcceptJournal, accept_work_view, advance_partial_exposed_base,
    cleanup_work_views, create_work_view, diff_work_view, discard_work_view,
    enqueue_work_view_accept, finalize_review_ready, list_work_views, overlay_delta_kind_name,
    overlay_deltas_for_upload, restore_work_view, sync_local_work_view_overlays,
    upload_staged_content, writer_lock::ProjectWriterLock,
};
use bowline_storage::{
    LocalByteStore, LocalContentCache, ObjectKind as StorageObjectKind,
    RetentionState as StorageRetentionState, StorageKey,
};

fn exposed_entries(
    store: &MetadataStore,
    descriptor: &crate::metadata::WorkViewBaseDescriptor,
) -> Vec<NamespaceEntry> {
    let snapshot = super::namespace::load_exposed_snapshot(store, descriptor)
        .expect("page-backed exposed snapshot");
    super::namespace::collect_prefix(
        &snapshot,
        &bowline_core::workspace_graph::WorkspaceRelativePath::new(""),
    )
    .expect("exposed entries")
}

mod accept_diff;
mod atomic_accept;
mod create;
mod hardening;
mod hidden_secret_accept;
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

fn snapshot_content_with_file(
    path: &str,
    content_id: ContentId,
    byte_len: u64,
) -> crate::sync::SnapshotContent {
    let workspace_id = WorkspaceId::new("ws_code");
    let entries = vec![NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: Some(content_id.clone()),
        content_layout: Some(
            ContentLayout::single_segment(ContentLocator {
                content_id,
                storage: ContentStorage::Packed,
                raw_size: byte_len,
                pack_id: Some(bowline_core::ids::PackId::new("pk_retained")),
                offset: Some(0),
                length: Some(byte_len),
            })
            .expect("test layout"),
        ),
        symlink_target: None,
        byte_len: Some(byte_len),
        executability: bowline_core::workspace_graph::FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }];
    let snapshot_id =
        crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    crate::sync::SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id: Some(ProjectId::new("proj_web")),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![GraphWorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id,
                kind: RefKind::Workspace,
            }],
        },
        BTreeMap::new(),
        [7; 32],
    )
    .expect("page-backed test snapshot")
}

fn seed_large_canonical_file(
    temp: &TempWorkspace,
    db_path: &Path,
    bytes: &[u8],
    key: [u8; 32],
) -> ContentId {
    let content_id = workspace_content_id(key, bytes);
    let project_path = temp.root().join("Code/apps/web");
    fs::write(project_path.join("large.bin"), bytes).expect("large source");
    let cache = LocalContentCache::open(temp.root().join(".state/cache")).expect("cache");
    cache
        .put_content(&content_id, bytes)
        .expect("cached content");
    cache
        .get_content(&content_id, key)
        .expect("verified content");
    let mut store = MetadataStore::open(db_path).expect("metadata");
    let snapshot =
        snapshot_content_with_file("apps/web/large.bin", content_id.clone(), bytes.len() as u64);
    crate::page_test_support::persist_cached_snapshot(
        &mut store,
        &snapshot,
        &temp.root().join(".state/metadata-pages"),
        &now(),
    );
    store
        .set_project_latest_snapshot_id(
            &WorkspaceId::new("ws_code"),
            &ProjectId::new("proj_web"),
            &snapshot.manifest().snapshot_id,
        )
        .expect("project latest snapshot");
    content_id
}

fn commit_test_snapshot_manifest(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    snapshot_id: &str,
    device_id: &str,
) {
    let metadata_suffix = blake3::hash(snapshot_id.as_bytes()).to_hex().to_string();
    let manifest_content_id = format!("content_manifest_{snapshot_id}");
    let namespace_root_id = format!("nsp_{metadata_suffix}");
    let metadata_content_id = namespace_root_id.clone();
    let manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ControlObjectKind::SnapshotManifest, 64)
                .with_content_id(&manifest_content_id),
        )
        .expect("snapshot manifest upload intent");
    let metadata_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ControlObjectKind::SnapshotMetadataPage, 256)
                .with_content_id(&metadata_content_id),
        )
        .expect("metadata page upload intent");

    let manifest_object = ObjectPointer {
        object_key: manifest_upload.object_key,
        content_id: ContentId::new(manifest_content_id),
        byte_len: 64,
        hash: format!("b3_manifest_{snapshot_id}"),
        key_epoch: 1,
        kind: ControlObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp { tick: 90 },
    };
    let metadata_object = ObjectPointer {
        object_key: metadata_upload.object_key,
        content_id: ContentId::new(metadata_content_id),
        byte_len: 256,
        hash: format!("b3_{metadata_suffix}"),
        key_epoch: 1,
        kind: ControlObjectKind::SnapshotMetadataPage,
        created_at: ControlPlaneTimestamp { tick: 91 },
    };
    control_plane
        .commit_uploaded_object_metadata(ObjectMetadataCommit {
            workspace_id: WorkspaceId::new(workspace_id),
            object: manifest_object.clone(),
            committed_by_device_id: DeviceId::new(device_id),
        })
        .expect("manifest object metadata");
    control_plane
        .commit_uploaded_object_metadata(ObjectMetadataCommit {
            workspace_id: WorkspaceId::new(workspace_id),
            object: metadata_object.clone(),
            committed_by_device_id: DeviceId::new(device_id),
        })
        .expect("metadata page object metadata");
    control_plane
        .commit_metadata_bindings(MetadataBindingCommit {
            workspace_id: WorkspaceId::new(workspace_id),
            bindings: vec![MetadataBindingInput {
                logical_id: namespace_root_id.clone(),
                record_kind: ControlMetadataRecordKind::NamespacePage,
                object: metadata_object,
                sidecar: MetadataSidecar {
                    child_logical_ids: Vec::new(),
                    direct_object_keys: Vec::new(),
                    digest: format!("b3_{metadata_suffix}"),
                },
            }],
            committed_by_device_id: DeviceId::new(device_id),
        })
        .expect("metadata root binding");

    control_plane
        .commit_snapshot_root(SnapshotRootCommit {
            workspace_id: WorkspaceId::new(workspace_id),
            snapshot_id: SnapshotId::new(snapshot_id),
            manifest_id: ManifestId::new(format!("manifest_{snapshot_id}")),
            manifest_object,
            namespace_root_id,
            extra_root_logical_ids: Vec::new(),
            committed_by_device_id: DeviceId::new(device_id),
        })
        .expect("snapshot root commit");
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
        content_id: ContentId::new(content_id),
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

fn accept_journal_dirs(namespace_root: &Path) -> Vec<PathBuf> {
    fs::read_dir(namespace_root)
        .expect("namespace root")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".accept-journal-"))
        })
        .collect()
}
