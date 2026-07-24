use super::*;

const NOW: &str = "2026-07-13T12:00:00Z";

#[test]
fn version_25_active_views_are_refused_without_migration() {
    let (temp, store, record) = seeded_store("metadata-legacy-work-view");
    store.upsert_work_view(&record).expect("legacy work view");
    let db_path = temp.root().join("metadata.sqlite3");
    drop(store);

    let connection = Connection::open(&db_path).expect("legacy database");
    connection
        .execute_batch(
            "ALTER TABLE work_views DROP COLUMN base_descriptor_version;
             ALTER TABLE work_views DROP COLUMN exposed_snapshot_id;
             ALTER TABLE work_views DROP COLUMN policy_fingerprint;
             PRAGMA user_version = 25;",
        )
        .expect("simulate canonical version 25");
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&db_path).expect_err("version 25 is destructive-cutover state"),
        MetadataError::UnsupportedSchema
    ));
}

#[test]
fn version_27_is_refused_without_migration() {
    let (temp, store, _record) = seeded_store("metadata-overlay-cutover");
    let db_path = temp.root().join("metadata.sqlite3");
    drop(store);
    let connection = Connection::open(&db_path).expect("version 27 database");
    connection
        .execute_batch(
            "CREATE TABLE work_view_base_files (
               workspace_id TEXT NOT NULL,
               work_view_id TEXT NOT NULL,
               path TEXT NOT NULL,
               hash TEXT NOT NULL,
               captured_at TEXT NOT NULL,
               PRIMARY KEY (workspace_id, work_view_id, path)
             );
             PRAGMA user_version = 27;",
        )
        .expect("simulate version 27 authority");
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&db_path).expect_err("version 27 is destructive-cutover state"),
        MetadataError::UnsupportedSchema
    ));
    let connection = Connection::open(&db_path).expect("version 27 database remains readable");
    let table_exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'work_view_base_files')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .expect("table inspection");
    assert!(table_exists);
}

pub(super) fn seeded_store(name: &str) -> (TempWorkspace, MetadataStore, WorkView) {
    let temp = TempWorkspace::new(name).expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("metadata.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("workspace");
    let project_id = ProjectId::new("project");
    store
        .insert_workspace(&workspace_id, "Workspace", NOW)
        .expect("workspace");
    store
        .insert_root("root", &workspace_id, &temp.root().to_string_lossy(), NOW)
        .expect("root");
    store
        .insert_project(&project_id, &workspace_id, "root", "apps/web", NOW)
        .expect("project");
    let record = WorkView {
        id: WorkViewId::new("view"),
        workspace_id,
        project_id,
        project_path: "apps/web".to_string(),
        name: "agent".to_string(),
        visible_path: temp
            .root()
            .join(".work/agent")
            .to_string_lossy()
            .into_owned(),
        base_snapshot_id: SnapshotId::new("snapshot"),
        overlay_head: OVERLAY_HEAD_EMPTY.to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: WorkViewLifecycle::Active,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: true,
        },
        owner_device_id: None,
        followed_by: Vec::new(),
        host_materializations: Vec::new(),
        attention: Vec::new(),
        created_at: NOW.to_string(),
        updated_at: NOW.to_string(),
    };
    (temp, store, record)
}
