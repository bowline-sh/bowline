use super::*;

#[test]
fn metadata_lives_outside_workspace_root() {
    let temp = TempWorkspace::new("metadata-outside-root").expect("temp workspace");
    let workspace_root = temp.root().join("Code");
    fs::create_dir_all(&workspace_root).expect("workspace root");
    let db_path = temp.root().join("state").join("local.sqlite3");

    assert!(!is_below(&db_path, &workspace_root));
    MetadataStore::open(&db_path).expect("metadata opens");
    assert!(!workspace_root.join("local.sqlite3").exists());
}

#[test]
fn corrupt_database_can_be_inspected_without_panic() {
    let temp = TempWorkspace::new("metadata-corrupt").expect("temp workspace");
    let db_path = temp.root().join("local.sqlite3");
    fs::write(&db_path, b"not sqlite").expect("write corrupt db");

    let inspection = MetadataStore::inspect(&db_path);
    assert_eq!(inspection.state, DatabaseState::Corrupt);
}

#[test]
fn product_shaped_queries_find_workspace_and_project_by_path() {
    let temp = TempWorkspace::new("metadata-query").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_acme_web");

    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
        .expect("root insert");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "acme/web",
            "2026-06-23T12:00:00Z",
        )
        .expect("project insert");

    assert_eq!(
        store
            .current_workspace()
            .expect("current workspace")
            .unwrap()
            .id,
        workspace_id
    );
    assert_eq!(
        store
            .current_project_by_path("acme/web/src/index.ts")
            .expect("project by path")
            .unwrap()
            .id,
        project_id
    );
    let home = std::env::var("HOME").expect("HOME should be set for tilde root matching");
    assert_eq!(
        store
            .current_project_by_path(&format!("{home}/Code/acme/web/src/index.ts"))
            .expect("project by absolute path under tilde root")
            .unwrap()
            .id,
        project_id
    );
}

#[test]
fn current_workspace_ignores_stale_workspace_without_root() {
    let temp = TempWorkspace::new("metadata-current-workspace").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let stale_workspace_id = WorkspaceId::new("ws_code");
    let active_workspace_id = WorkspaceId::new("ws_code_account");

    store
        .insert_workspace(&stale_workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("stale workspace insert");
    store
        .insert_workspace(&active_workspace_id, "User Code", "2026-06-23T12:01:00Z")
        .expect("active workspace insert");
    store
        .insert_root(
            "root_code",
            &active_workspace_id,
            "~/Code",
            "2026-06-23T12:01:00Z",
        )
        .expect("active root insert");

    assert_eq!(
        store
            .current_workspace()
            .expect("current workspace")
            .unwrap()
            .id,
        active_workspace_id
    );
}

#[test]
fn current_workspace_prefers_newest_accepted_root() {
    let temp =
        TempWorkspace::new("metadata-current-workspace-newest-root").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let old_workspace_id = WorkspaceId::new("ws_code");
    let account_workspace_id = WorkspaceId::new("ws_code_account");

    store
        .insert_workspace(&old_workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("old workspace insert");
    store
        .insert_root(
            "root_ws_code",
            &old_workspace_id,
            "~/Code",
            "2026-06-23T12:00:00Z",
        )
        .expect("old root insert");
    store
        .insert_workspace(&account_workspace_id, "User Code", "2026-06-23T12:01:00Z")
        .expect("account workspace insert");
    store
        .insert_root(
            "root_ws_code_account",
            &account_workspace_id,
            "~/Code",
            "2026-06-23T12:01:00Z",
        )
        .expect("account root insert");

    assert_eq!(
        store
            .current_workspace()
            .expect("current workspace")
            .unwrap()
            .id,
        account_workspace_id
    );
}

#[test]
fn replace_projects_removes_stale_projects_for_workspace() {
    let temp = TempWorkspace::new("metadata-replace-projects").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let mut store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");

    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
        .expect("root insert");
    store
        .replace_projects(
            &workspace_id,
            "root_code",
            &[
                (ProjectId::new("proj_old"), "old".to_string()),
                (ProjectId::new("proj_web"), "apps/web".to_string()),
            ],
            "2026-06-23T12:00:00Z",
        )
        .expect("first project set");
    store
        .connection()
        .execute(
            "INSERT INTO namespace_entries
             (id, workspace_id, project_id, path, kind, classification, mode,
              hydration_state, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'file', 'workspace-sync', 'workspace-sync',
                     'local', ?5)",
            rusqlite::params![
                "entry-web",
                workspace_id.as_str(),
                "proj_web",
                "apps/web/src/index.ts",
                "2026-06-23T12:00:00Z",
            ],
        )
        .expect("namespace insert");
    store
        .connection()
        .execute(
            "INSERT INTO namespace_entries
             (id, workspace_id, project_id, path, kind, classification, mode,
              hydration_state, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'file', 'workspace-sync', 'workspace-sync',
                     'local', ?5)",
            rusqlite::params![
                "entry-old",
                workspace_id.as_str(),
                "proj_old",
                "old/src/lib.rs",
                "2026-06-23T12:00:00Z",
            ],
        )
        .expect("old namespace insert");
    let old_project_id = ProjectId::new("proj_old");
    store
        .upsert_index_document(&IndexDocumentRecord {
            workspace_id: workspace_id.clone(),
            project_id: Some(old_project_id.clone()),
            path: "src/lib.rs".to_string(),
            snapshot_id: Some(SnapshotId::new("snap_old")),
            content_id: Some(ContentId::new("cid_old")),
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            policy_summary: "source".to_string(),
            body_text: "pub fn stale_secret_text() {}".to_string(),
            hydration_state: HydrationState::Cold,
            indexed_bytes: 29,
            source_watermark: 1,
            indexed_watermark: 1,
            state: "ready".to_string(),
            updated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("old index document");
    store
        .upsert_index_pack(&IndexPackRecord {
            workspace_id: workspace_id.clone(),
            project_id: Some(old_project_id.clone()),
            snapshot_id: Some(SnapshotId::new("snap_old")),
            object_key: "indexes/old.bowlinei".to_string(),
            byte_len: 128,
            hash: "hash-old-index-pack".to_string(),
            state: "ready".to_string(),
            updated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("old index pack");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_old:path:lib".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(old_project_id),
            path: Some("src/lib.rs".to_string()),
            kind: "path".to_string(),
            source_watermark: 1,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("projected-node-updated".to_string()),
            updated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("old index work");
    store
        .replace_projects(
            &workspace_id,
            "root_code",
            &[(ProjectId::new("proj_web"), "apps/web".to_string())],
            "2026-06-23T12:01:00Z",
        )
        .expect("second project set");

    assert_eq!(
        store.project_count(&workspace_id).expect("project count"),
        1
    );
    assert!(
        store
            .current_project_by_path("old/src/lib.rs")
            .expect("old project lookup")
            .is_none()
    );
    assert_eq!(
        store
            .current_project_by_path("apps/web/src/index.ts")
            .expect("current project lookup")
            .unwrap()
            .id,
        ProjectId::new("proj_web")
    );
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT project_id FROM namespace_entries WHERE id = 'entry-web'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("namespace project link"),
        Some("proj_web".to_string())
    );
    let old_entry_count: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM namespace_entries WHERE id = 'entry-old'",
            [],
            |row| row.get(0),
        )
        .expect("old namespace count");
    assert_eq!(old_entry_count, 0);
    for (table, label) in [
        ("index_documents", "old index documents"),
        ("index_packs", "old index packs"),
        ("index_work", "old index work"),
    ] {
        let count: i64 = store
            .connection()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE project_id = 'proj_old'"),
                [],
                |row| row.get(0),
            )
            .expect(label);
        assert_eq!(count, 0, "{label} should be removed");
    }
}

#[test]
fn replace_projects_rejects_project_id_owned_by_another_workspace() {
    let temp = TempWorkspace::new("metadata-replace-projects-cross-workspace").expect("workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let mut store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_a = WorkspaceId::new("ws_a");
    let workspace_b = WorkspaceId::new("ws_b");
    store
        .insert_workspace(&workspace_a, "A", "2026-06-29T04:00:00Z")
        .expect("workspace a");
    store
        .insert_workspace(&workspace_b, "B", "2026-06-29T04:00:00Z")
        .expect("workspace b");
    store
        .insert_root("root_a", &workspace_a, "/tmp/a", "2026-06-29T04:00:00Z")
        .expect("root a");
    store
        .insert_root("root_b", &workspace_b, "/tmp/b", "2026-06-29T04:00:00Z")
        .expect("root b");
    store
        .replace_projects(
            &workspace_a,
            "root_a",
            &[(ProjectId::new("proj_app"), "app".to_string())],
            "2026-06-29T04:00:00Z",
        )
        .expect("workspace a projects");

    let error = store
        .replace_projects(
            &workspace_b,
            "root_b",
            &[(ProjectId::new("proj_app"), "app".to_string())],
            "2026-06-29T04:00:01Z",
        )
        .expect_err("cross-workspace project id is rejected");

    assert!(error.to_string().contains("another workspace"));
    assert_eq!(
        store
            .connection()
            .query_row(
                "SELECT workspace_id FROM projects WHERE id = 'proj_app'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("project owner"),
        workspace_a.as_str()
    );
}

#[test]
fn insert_root_rejects_root_id_owned_by_another_workspace() {
    let temp = TempWorkspace::new("metadata-root-cross-workspace").expect("workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_a = WorkspaceId::new("ws_a");
    let workspace_b = WorkspaceId::new("ws_b");
    store
        .insert_workspace(&workspace_a, "A", "2026-06-29T04:00:00Z")
        .expect("workspace a");
    store
        .insert_workspace(&workspace_b, "B", "2026-06-29T04:00:00Z")
        .expect("workspace b");
    store
        .insert_root("root_code", &workspace_a, "/tmp/a", "2026-06-29T04:00:00Z")
        .expect("workspace a root");

    let error = store
        .insert_root("root_code", &workspace_b, "/tmp/b", "2026-06-29T04:00:01Z")
        .expect_err("cross-workspace root id is rejected");

    assert!(error.to_string().contains("another workspace"));
    assert_eq!(
        store
            .accepted_roots(&workspace_a)
            .expect("workspace a roots")
            .len(),
        1
    );
    assert!(
        store
            .accepted_roots(&workspace_b)
            .expect("workspace b roots")
            .is_empty()
    );
}

#[test]
fn root_matching_is_scoped_to_the_current_workspace() {
    let temp = TempWorkspace::new("metadata-root-scope").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let current_root = temp.root().join("Code");
    let other_root = temp.root().join("OtherCode");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let current_workspace_id = WorkspaceId::new("ws_current");
    let other_workspace_id = WorkspaceId::new("ws_other");
    let project_id = ProjectId::new("proj_acme_web");

    store
        .insert_workspace(
            &current_workspace_id,
            "Current Code",
            "2026-06-23T12:01:00Z",
        )
        .expect("current workspace insert");
    store
        .insert_workspace(&other_workspace_id, "Other Code", "2026-06-23T12:00:00Z")
        .expect("other workspace insert");
    store
        .insert_root(
            "root_current",
            &current_workspace_id,
            &current_root.display().to_string(),
            "2026-06-23T12:01:00Z",
        )
        .expect("current root insert");
    store
        .insert_root(
            "root_other",
            &other_workspace_id,
            &other_root.display().to_string(),
            "2026-06-23T12:00:00Z",
        )
        .expect("other root insert");
    store
        .insert_project(
            &project_id,
            &current_workspace_id,
            "root_current",
            "acme/web",
            "2026-06-23T12:00:00Z",
        )
        .expect("project insert");

    assert_eq!(
        store
            .accepted_root_count(&current_workspace_id)
            .expect("current accepted roots"),
        1
    );
    assert_eq!(
        store
            .project_count(&other_workspace_id)
            .expect("other project count"),
        0
    );
    assert_eq!(
        store
            .current_project_by_path(&format!("{}/acme/web/src/index.ts", current_root.display()))
            .expect("project by current root")
            .unwrap()
            .id,
        project_id
    );
    assert!(
        store
            .current_project_by_path(&format!("{}/acme/web/src/index.ts", other_root.display()))
            .expect("project by other root")
            .is_none()
    );
}
