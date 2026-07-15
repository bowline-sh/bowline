use super::*;
use bowline_core::{ids::NamespacePageId, workspace_graph::SnapshotKind};
use std::collections::BTreeMap;

const NOW: &str = "2026-07-13T12:00:00Z";
const KEY: [u8; 32] = [17; 32];

#[test]
fn exposed_work_view_base_round_trips_immutable_page_root() {
    let (temp, mut store, record) = seeded_store("metadata-exposed-base");
    let snapshot = exposed_snapshot(
        &record,
        vec![namespace_entry("apps/web", NamespaceEntryKind::Directory)],
    );
    crate::page_test_support::persist_cached_snapshot(
        &mut store,
        &snapshot,
        &temp.root().join("metadata-pages"),
        NOW,
    );
    let descriptor = descriptor(&record, &snapshot);

    store
        .insert_work_view_with_exposed_base(&record, &descriptor)
        .expect("authoritative exposed root persists");

    assert_eq!(
        store
            .work_view_exposed_base(&record.workspace_id, &record.id)
            .expect("base reads"),
        Some(descriptor.clone())
    );
    assert_eq!(
        store
            .work_view_base_state(&record.workspace_id, &record.id)
            .expect("base state"),
        WorkViewBaseState::Authoritative {
            descriptor: Box::new(descriptor),
        }
    );
}

#[test]
fn exposed_base_rejects_descriptor_root_mismatch() {
    let (temp, mut store, record) = seeded_store("metadata-mismatched-base");
    let snapshot = exposed_snapshot(
        &record,
        vec![namespace_entry("apps/web", NamespaceEntryKind::Directory)],
    );
    crate::page_test_support::persist_cached_snapshot(
        &mut store,
        &snapshot,
        &temp.root().join("metadata-pages"),
        NOW,
    );
    let mut descriptor = descriptor(&record, &snapshot);
    descriptor.exposed_namespace_root_id = NamespacePageId::new("nsp_wrong");

    let error = store
        .insert_work_view_with_exposed_base(&record, &descriptor)
        .expect_err("descriptor cannot redirect an immutable snapshot");
    assert!(matches!(error, MetadataError::InvalidStorageMetadata(_)));
}

#[test]
fn version_25_active_views_are_refused_without_migration() {
    let (temp, store, record) = seeded_store("metadata-legacy-work-view");
    store.upsert_work_view(&record).expect("legacy work view");
    let db_path = temp.root().join("metadata.sqlite3");
    drop(store);

    let connection = Connection::open(&db_path).expect("legacy database");
    connection
        .execute_batch(
            "DROP TABLE work_view_base_descriptors;
             ALTER TABLE work_views DROP COLUMN base_descriptor_version;
             ALTER TABLE work_views DROP COLUMN exposed_snapshot_id;
             ALTER TABLE work_views DROP COLUMN policy_fingerprint;
             ALTER TABLE work_views DROP COLUMN base_review_reason;
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

fn exposed_snapshot(
    record: &WorkView,
    entries: Vec<NamespaceEntry>,
) -> crate::sync::SnapshotContent {
    let identity = crate::sync::rebuild_manifest_identity(&record.workspace_id, &entries, "test");
    crate::sync::SnapshotContent::new(
        bowline_core::workspace_graph::SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: identity.snapshot_id,
            workspace_id: record.workspace_id.clone(),
            project_id: Some(record.project_id.clone()),
            kind: SnapshotKind::Base,
            base_snapshot_id: Some(record.base_snapshot_id.clone()),
            entries,
            refs: Vec::new(),
        },
        BTreeMap::new(),
        KEY,
    )
    .expect("page-backed exposed snapshot")
}

fn descriptor(
    record: &WorkView,
    snapshot: &crate::sync::SnapshotContent,
) -> WorkViewBaseDescriptor {
    WorkViewBaseDescriptor {
        format_version: WORK_VIEW_BASE_DESCRIPTOR_VERSION,
        workspace_id: record.workspace_id.clone(),
        project_id: record.project_id.clone(),
        work_view_id: record.id.clone(),
        base_snapshot_id: record.base_snapshot_id.clone(),
        project_prefix: record.project_path.clone(),
        policy_fingerprint: "policy-sha256".to_string(),
        exposed_snapshot_id: snapshot.manifest().snapshot_id.clone(),
        exposed_namespace_root_id: snapshot.manifest().namespace_root_id.clone(),
        exposed_semantic_manifest_digest: snapshot.manifest().semantic_manifest_digest.clone(),
        exposed_entry_count: snapshot.manifest().entry_count,
        created_at: NOW.to_string(),
    }
}

fn namespace_entry(path: &str, kind: NamespaceEntryKind) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind,
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
