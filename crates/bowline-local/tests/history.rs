use std::{fs, path::PathBuf};

use bowline_core::{
    commands::CommandName,
    events::{EventName, EventSeverity, WorkspaceEvent},
    ids::EventId,
    ids::{DeviceId, ProjectId, SnapshotId, WorkspaceId},
    policy::PathClassification,
    workspace_graph::SnapshotKind,
};
use bowline_local::{
    history::{DEFAULT_HISTORY_LIMIT, HistoryMode, HistoryOptions, compose_history},
    metadata::{
        LocalWriteLogRecord, MetadataStore, SnapshotRecord, SyncOperationKind, SyncOperationRecord,
        SyncOperationState, SyncResourceKey,
    },
    workspace::TempWorkspace,
};

#[test]
fn compose_history_lists_restore_points_with_write_summary() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_sync(
        &store,
        &workspace_id,
        "sync_1",
        Some("snap_base"),
        "snap_after",
        "2026-06-25T00:01:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_1",
        "apps/web/src/index.ts",
    );

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-06-25T00:02:00Z".to_string(),
        limit: DEFAULT_HISTORY_LIMIT,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history output");

    assert_eq!(output.command, CommandName::History);
    assert_eq!(output.scope.project_path, "apps/web");
    assert_eq!(output.restore_points.len(), 1);
    let point = &output.restore_points[0];
    assert_eq!(point.id, "rp_snap_after");
    assert_eq!(point.summary.files_changed, 1);
    assert_eq!(point.summary.files_modified, 1);
    assert_eq!(point.summary.paths_sample, vec!["apps/web/src/index.ts"]);
}

#[test]
fn compose_history_keeps_restore_point_writes_and_events_by_cause() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase84-history-cause-index");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_sync(
        &store,
        &workspace_id,
        "sync_newer",
        Some("snap_base"),
        "snap_newer",
        "2026-06-25T00:03:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_newer",
        "apps/web/src/newer.ts",
    );
    seed_event(
        &store,
        &workspace_id,
        &project_id,
        "evt_newer",
        "sync_newer",
    );
    seed_sync(
        &store,
        &workspace_id,
        "sync_older",
        Some("snap_base"),
        "snap_older",
        "2026-06-25T00:01:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_older",
        "apps/web/src/older.ts",
    );
    seed_event(
        &store,
        &workspace_id,
        &project_id,
        "evt_older",
        "sync_older",
    );

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-06-25T00:04:00Z".to_string(),
        limit: DEFAULT_HISTORY_LIMIT,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history output");

    assert_eq!(
        output
            .restore_points
            .iter()
            .map(|point| (
                point.id.as_str(),
                point.summary.paths_sample.clone(),
                point
                    .event_ids
                    .iter()
                    .map(|id| id.as_str().to_string())
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "rp_snap_newer",
                vec!["apps/web/src/newer.ts".to_string()],
                vec!["evt_newer".to_string()]
            ),
            (
                "rp_snap_older",
                vec!["apps/web/src/older.ts".to_string()],
                vec!["evt_older".to_string()]
            ),
        ]
    );
}

#[test]
fn project_history_excludes_restore_points_from_other_projects() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history-project-scope");
    let api_project_id = ProjectId::new("proj_api");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .insert_project(
            &api_project_id,
            &workspace_id,
            "root_code",
            "apps/api",
            "2026-06-25T00:00:00Z",
        )
        .expect("api project");
    seed_sync(
        &store,
        &workspace_id,
        "sync_api",
        None,
        "snap_api",
        "2026-06-25T00:02:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &api_project_id,
        "sync_api",
        "apps/api/src/index.ts",
    );
    seed_sync(
        &store,
        &workspace_id,
        "sync_web",
        None,
        "snap_web",
        "2026-06-25T00:01:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_web",
        "apps/web/src/index.ts",
    );

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-06-25T00:03:00Z".to_string(),
        limit: DEFAULT_HISTORY_LIMIT,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history output");

    assert_eq!(
        output
            .restore_points
            .iter()
            .map(|point| point.snapshot_id.as_str())
            .collect::<Vec<_>>(),
        vec!["snap_web"]
    );
}

#[test]
fn project_history_includes_head_only_restore_point_with_retained_project_snapshot() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history-retained-head");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_snapshot(&store, &workspace_id, &project_id, "snap_remote");
    seed_sync(
        &store,
        &workspace_id,
        "sync_remote",
        None,
        "snap_remote",
        "2026-06-25T00:01:00Z",
    );

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-06-25T00:03:00Z".to_string(),
        limit: DEFAULT_HISTORY_LIMIT,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history output");

    assert_eq!(output.restore_points.len(), 1);
    assert_eq!(output.restore_points[0].snapshot_id.as_str(), "snap_remote");
    assert_eq!(output.restore_points[0].summary.files_changed, 0);
}

#[test]
fn project_history_counts_writes_older_than_global_history_window() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history-write-window");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_snapshot(&store, &workspace_id, &project_id, "snap_old");
    seed_sync(
        &store,
        &workspace_id,
        "sync_old",
        None,
        "snap_old",
        "2026-06-25T00:01:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_old",
        "apps/web/src/old.ts",
    );
    for index in 0..500 {
        seed_write(
            &store,
            &workspace_id,
            &project_id,
            &format!("noise_{index:03}"),
            &format!("apps/web/src/noise-{index:03}.ts"),
        );
    }

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-06-25T00:03:00Z".to_string(),
        limit: DEFAULT_HISTORY_LIMIT,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history output");

    let point = output
        .restore_points
        .iter()
        .find(|point| point.snapshot_id.as_str() == "snap_old")
        .expect("old restore point");
    assert_eq!(point.summary.files_changed, 1);
    assert_eq!(point.summary.paths_sample, vec!["apps/web/src/old.ts"]);
}

#[test]
fn project_history_pages_completed_syncs_until_project_restore_point_is_found() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history-project-page");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_snapshot(&store, &workspace_id, &project_id, "snap_web_old");
    seed_sync(
        &store,
        &workspace_id,
        "sync_web_old",
        None,
        "snap_web_old",
        "2026-06-25T00:01:00Z",
    );
    for index in 0..501 {
        let seconds = index + 120;
        seed_sync(
            &store,
            &workspace_id,
            &format!("sync_other_{index:03}"),
            None,
            &format!("snap_other_{index:03}"),
            &format!("2026-06-25T00:{:02}:{:02}Z", seconds / 60, seconds % 60),
        );
    }

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.display().to_string(),
        mode: HistoryMode::Timeline,
        generated_at: "2026-06-25T00:20:00Z".to_string(),
        limit: DEFAULT_HISTORY_LIMIT,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history output");

    assert_eq!(
        output
            .restore_points
            .iter()
            .map(|point| point.snapshot_id.as_str())
            .collect::<Vec<_>>(),
        vec!["snap_web_old"]
    );
}

#[test]
fn history_diff_resolves_restore_points_outside_current_page() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history-diff-page");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_sync(
        &store,
        &workspace_id,
        "sync_old",
        None,
        "snap_old",
        "2026-06-25T00:01:00Z",
    );
    seed_sync(
        &store,
        &workspace_id,
        "sync_new",
        Some("snap_old"),
        "snap_new",
        "2026-06-25T00:02:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_old",
        "apps/web/src/old.ts",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_new",
        "apps/web/src/new.ts",
    );

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.display().to_string(),
        mode: HistoryMode::Diff {
            from: "rp_snap_old".to_string(),
            to: "rp_snap_new".to_string(),
        },
        generated_at: "2026-06-25T00:03:00Z".to_string(),
        limit: 1,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("history diff");

    assert_eq!(output.restore_points.len(), 1);
    assert_eq!(output.from.expect("from").snapshot_id.as_str(), "snap_old");
    assert_eq!(output.to.expect("to").snapshot_id.as_str(), "snap_new");
    let summary = output.diff_summary.expect("diff summary");
    assert_eq!(summary.files_changed, 1);
    assert_eq!(summary.paths_sample, vec!["apps/web/src/new.ts"]);
}

#[test]
fn path_history_uses_off_page_restore_point_for_write_causation() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history-path-page");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_sync(
        &store,
        &workspace_id,
        "sync_old",
        None,
        "snap_old",
        "2026-06-25T00:01:00Z",
    );
    seed_sync(
        &store,
        &workspace_id,
        "sync_new",
        Some("snap_old"),
        "snap_new",
        "2026-06-25T00:02:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_old",
        "apps/web/src/old.ts",
    );

    let output = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.join("src/old.ts").display().to_string(),
        mode: HistoryMode::Path,
        generated_at: "2026-06-25T00:03:00Z".to_string(),
        limit: 1,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("path history");

    assert_eq!(output.restore_points.len(), 1);
    assert_eq!(output.restore_points[0].id, "rp_snap_old");
    assert_eq!(output.path_entries.len(), 1);
    assert_eq!(output.path_entries[0].restore_point_id, "rp_snap_old");
    assert_eq!(output.path_entries[0].snapshot_id.as_str(), "snap_old");
}

#[test]
fn path_history_pages_path_entries() {
    let (_temp, db_path, project_root, workspace_id, project_id) =
        seeded_history_workspace("phase18-history-path-entry-page");
    let store = MetadataStore::open(&db_path).expect("metadata");
    seed_sync(
        &store,
        &workspace_id,
        "sync_old",
        None,
        "snap_old",
        "2026-06-25T00:01:00Z",
    );
    seed_sync(
        &store,
        &workspace_id,
        "sync_new",
        Some("snap_old"),
        "snap_new",
        "2026-06-25T00:02:00Z",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_old",
        "apps/web/src/shared.ts",
    );
    seed_write(
        &store,
        &workspace_id,
        &project_id,
        "sync_new",
        "apps/web/src/shared.ts",
    );

    let first_page = compose_history(HistoryOptions {
        db_path: Some(db_path.clone()),
        target_path: project_root.join("src/shared.ts").display().to_string(),
        mode: HistoryMode::Path,
        generated_at: "2026-06-25T00:03:00Z".to_string(),
        limit: 1,
        cursor: None,
        since: None,
        until: None,
    })
    .expect("first page");
    assert_eq!(first_page.path_entries.len(), 1);
    assert!(first_page.truncated);
    assert_eq!(first_page.next_cursor.as_deref(), Some("1"));

    let second_page = compose_history(HistoryOptions {
        db_path: Some(db_path),
        target_path: project_root.join("src/shared.ts").display().to_string(),
        mode: HistoryMode::Path,
        generated_at: "2026-06-25T00:03:00Z".to_string(),
        limit: 1,
        cursor: Some(1),
        since: None,
        until: None,
    })
    .expect("second page");
    assert_eq!(second_page.path_entries.len(), 1);
    assert!(!second_page.truncated);
    assert_eq!(second_page.next_cursor, None);
}

fn seeded_history_workspace(
    name: &str,
) -> (TempWorkspace, PathBuf, PathBuf, WorkspaceId, ProjectId) {
    let temp = TempWorkspace::new(name).expect("temp workspace");
    let code_root = temp.root().join("Code");
    let project_root = code_root.join("apps/web");
    fs::create_dir_all(project_root.join("src")).expect("project dir");
    let db_path = temp.root().join(".state/local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let store = MetadataStore::open(&db_path).expect("metadata");
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
    (temp, db_path, project_root, workspace_id, project_id)
}

fn seed_sync(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    id: &str,
    base_snapshot_id: Option<&str>,
    target_snapshot_id: &str,
    updated_at: &str,
) {
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Completed,
            idempotency_key: id.to_string(),
            base_version: Some(1),
            base_snapshot_id: base_snapshot_id.map(ToString::to_string),
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

fn seed_write(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    causation_id: &str,
    path: &str,
) {
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: format!("write_{causation_id}"),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-1"),
            project_id: Some(project_id.clone()),
            path: path.to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: causation_id.to_string(),
            settled_at: "2026-06-25T00:02:00Z".to_string(),
            created_at: "2026-06-25T00:02:00Z".to_string(),
        })
        .expect("write log");
}

fn seed_event(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    event_id: &str,
    causation_id: &str,
) {
    let mut event = WorkspaceEvent::new(
        EventId::new(event_id),
        EventName::SourceStale,
        "2026-06-25T00:02:00Z",
        EventSeverity::Attention,
        "Source needs attention.",
        workspace_id.clone(),
    );
    event.project_id = Some(project_id.clone());
    event.causation_id = Some(EventId::new(causation_id));
    store.append_event(event).expect("event");
}

fn seed_snapshot(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    snapshot_id: &str,
) {
    let snapshot_id = SnapshotId::new(snapshot_id);
    store
        .upsert_snapshot(&SnapshotRecord {
            id: snapshot_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            root_id: bowline_core::ids::NamespacePageId::new(format!("nsp_{snapshot_id}")),
            semantic_manifest_digest: bowline_core::ids::ManifestDigest::new(format!(
                "md_{snapshot_id}"
            )),
            entry_count: 0,
            refs: Vec::new(),
            created_at: "2026-06-25T00:00:00Z".to_string(),
        })
        .expect("snapshot");
}
