use std::{collections::BTreeMap, path::Path};

use bowline_core::{
    ids::{DeviceId, ProjectId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind,
        SnapshotDraft, SnapshotKind, WorkspaceRef as GraphWorkspaceRef,
    },
};

use crate::{
    metadata::{
        LocalWriteLogRecord, MetadataStore, SyncOperationKind, SyncOperationRecord,
        SyncOperationState,
    },
    sync::{SnapshotContent, rebuild_manifest_identity},
    workspace::TempWorkspace,
};

use super::*;

#[test]
fn project_history_excludes_workspace_snapshot_without_project_evidence() {
    let (temp, db_path) = seeded_history_store("history-project-scope");
    let mut store = MetadataStore::open(&db_path).expect("metadata");
    let snapshot_id = persist_test_snapshot(
        &mut store,
        &temp.root().join(".state/history-pages"),
        None,
        vec![directory_entry("apps/website")],
        "2026-07-02T09:00:00Z",
    );
    enqueue_completed_sync(
        &store,
        "op_workspace_only",
        snapshot_id.as_str(),
        "2026-07-02T09:00:00Z",
    );
    drop(store);

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: temp.root().join("Code/apps/web").display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-07-02T10:00:00Z".to_string(),
        limit: 50,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history");

    assert!(output.restore_points.is_empty());
}

#[test]
fn project_history_includes_workspace_snapshot_when_writes_touch_project() {
    let (temp, db_path) = seeded_history_store("history-project-scope-writes");
    let mut store = MetadataStore::open(&db_path).expect("metadata");
    let snapshot_id = persist_test_snapshot(
        &mut store,
        &temp.root().join(".state/history-pages"),
        None,
        Vec::new(),
        "2026-07-02T09:00:00Z",
    );
    enqueue_completed_sync(
        &store,
        "op_workspace_touching_project",
        snapshot_id.as_str(),
        "2026-07-02T09:00:00Z",
    );
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-apps-web".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: None,
            path: "apps/web/src/index.ts".to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "op_workspace_touching_project".to_string(),
            settled_at: "2026-07-02T09:00:00Z".to_string(),
            created_at: "2026-07-02T09:00:00Z".to_string(),
        })
        .expect("write log");
    drop(store);

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: temp.root().join("Code/apps/web").display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-07-02T10:00:00Z".to_string(),
        limit: 50,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history");

    assert_eq!(output.restore_points.len(), 1);
    assert_eq!(output.restore_points[0].snapshot_id, snapshot_id);
    assert_eq!(output.restore_points[0].summary.files_changed, 1);
}

#[test]
fn project_history_uses_page_namespace_membership_for_workspace_snapshot() {
    let (temp, db_path) = seeded_history_store("history-page-project-scope");
    let mut store = MetadataStore::open(&db_path).expect("metadata");
    let snapshot_id = persist_test_snapshot(
        &mut store,
        &temp.root().join(".state/history-pages"),
        None,
        vec![directory_entry("apps/web")],
        "2026-07-02T09:00:00Z",
    );
    enqueue_completed_sync(
        &store,
        "op_workspace_page_membership",
        snapshot_id.as_str(),
        "2026-07-02T09:00:00Z",
    );
    drop(store);

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: temp.root().join("Code/apps/web").display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-07-02T10:00:00Z".to_string(),
        limit: 50,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history");

    assert_eq!(output.restore_points.len(), 1);
    assert_eq!(output.restore_points[0].snapshot_id, snapshot_id);
}

#[test]
fn diff_summary_handles_restore_points_with_equal_timestamps() {
    let points = vec![
        restore_point("snap_new", "2026-07-02T09:00:00Z"),
        restore_point("snap_old", "2026-07-02T09:00:00Z"),
    ];
    let mut point_id_by_cause = BTreeMap::new();
    point_id_by_cause.insert("op_new".to_string(), "rp_snap_new".to_string());
    point_id_by_cause.insert("op_old".to_string(), "rp_snap_old".to_string());
    let writes = vec![LocalWriteLogRecord {
        id: "write-new".to_string(),
        workspace_id: WorkspaceId::new("ws_code"),
        device_id: DeviceId::new("device-1"),
        project_id: Some(ProjectId::new("proj_web")),
        path: "apps/web/src/index.ts".to_string(),
        source_path: None,
        operation: "modify".to_string(),
        staged_content_id: None,
        policy_classification: PathClassification::WorkspaceSync,
        causation_id: "op_new".to_string(),
        settled_at: "2026-07-02T09:00:00Z".to_string(),
        created_at: "2026-07-02T09:00:00Z".to_string(),
    }];

    let summary = diff_summary_between(
        &writes,
        &points,
        &point_id_by_cause,
        &HistoryEndpoint {
            restore_point_id: Some("rp_snap_old".to_string()),
            snapshot_id: SnapshotId::new("snap_old"),
        },
        &HistoryEndpoint {
            restore_point_id: Some("rp_snap_new".to_string()),
            snapshot_id: SnapshotId::new("snap_new"),
        },
    );

    assert_eq!(summary.files_changed, 1);
}

fn seeded_history_store(name: &str) -> (TempWorkspace, PathBuf) {
    let temp = TempWorkspace::new(name).expect("temp workspace");
    let code_root = temp.root().join("Code");
    std::fs::create_dir_all(code_root.join("apps/web")).expect("web project");
    let db_path = temp.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-02T00:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-07-02T00:00:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &ProjectId::new("proj_web"),
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-07-02T00:00:00Z",
        )
        .expect("web project");
    drop(store);
    (temp, db_path)
}

fn persist_test_snapshot(
    store: &mut MetadataStore,
    cache_root: &Path,
    project_id: Option<ProjectId>,
    entries: Vec<NamespaceEntry>,
    created_at: &str,
) -> SnapshotId {
    let workspace_id = WorkspaceId::new("ws_code");
    let snapshot_id = rebuild_manifest_identity(&workspace_id, &entries, created_at).snapshot_id;
    let snapshot = SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![GraphWorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id.clone(),
                kind: RefKind::Workspace,
            }],
        },
        BTreeMap::new(),
        [7; 32],
    )
    .expect("page-backed history snapshot");
    crate::page_test_support::persist_cached_snapshot(store, &snapshot, cache_root, created_at);
    snapshot_id
}

fn directory_entry(path: &str) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::Directory,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: None,
        content_layout: None,
        symlink_target: None,
        byte_len: None,
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}

fn enqueue_completed_sync(
    store: &MetadataStore,
    id: &str,
    target_snapshot_id: &str,
    updated_at: &str,
) {
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: id.to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            kind: SyncOperationKind::Reconcile,
            resource_key: crate::metadata::SyncResourceKey::workspace_sync(WorkspaceId::new(
                "ws_code",
            )),
            state: SyncOperationState::Completed,
            idempotency_key: id.to_string(),
            base_version: Some(0),
            base_snapshot_id: None,
            target_snapshot_id: Some(target_snapshot_id.to_string()),
            device_id: Some(DeviceId::new("device-1")),
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
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
        })
        .expect("sync operation");
}

fn restore_point(snapshot_id: &str, occurred_at: &str) -> RestorePoint {
    RestorePoint {
        id: restore_point_id(snapshot_id),
        snapshot_id: SnapshotId::new(snapshot_id),
        base_snapshot_id: None,
        occurred_at: occurred_at.to_string(),
        label: "Workspace sync".to_string(),
        cause: HistoryCause::Sync,
        actor: None,
        summary: HistoryChangeSummary {
            files_changed: 0,
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
            files_renamed: 0,
            binary_or_large_files_changed: 0,
            env_keys_changed: 0,
            paths_sample: Vec::new(),
        },
        event_ids: Vec::new(),
    }
}
