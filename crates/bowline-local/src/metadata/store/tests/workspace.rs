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
                project_upsert("proj_old", "old"),
                project_upsert("proj_web", "apps/web"),
            ],
            "2026-06-23T12:00:00Z",
        )
        .expect("first project set");
    store
        .replace_projects(
            &workspace_id,
            "root_code",
            &[project_upsert("proj_web", "apps/web")],
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
            &[project_upsert("proj_app", "app")],
            "2026-06-29T04:00:00Z",
        )
        .expect("workspace a projects");

    let error = store
        .replace_projects(
            &workspace_b,
            "root_b",
            &[project_upsert("proj_app", "app")],
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

#[test]
fn blocked_and_local_only_observed_paths_returns_only_visible_policy_rows() {
    let temp = TempWorkspace::new("metadata-blocked-local-only-paths").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let mut store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-07T12:00:00Z")
        .expect("workspace");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-07-07T12:00:00Z")
        .expect("root");
    store
        .replace_projects(
            &workspace_id,
            "root_code",
            &[project_upsert("proj_web", "apps/web")],
            "2026-07-07T12:00:00Z",
        )
        .expect("project");
    store
        .replace_observed_paths(
            &workspace_id,
            &[
                ObservedLocalPath {
                    project_id: Some(project_id.clone()),
                    path: "apps/web/.env".to_string(),
                    classification: PathClassification::LocalOnly,
                    mode: MaterializationMode::LocalOnly,
                    access: Vec::new(),
                },
                ObservedLocalPath {
                    project_id: Some(project_id.clone()),
                    path: "apps/web/blocked.pem".to_string(),
                    classification: PathClassification::Blocked,
                    mode: MaterializationMode::Blocked,
                    access: Vec::new(),
                },
                ObservedLocalPath {
                    project_id: Some(project_id.clone()),
                    path: "apps/web/.git".to_string(),
                    classification: PathClassification::LocalOnly,
                    mode: MaterializationMode::LocalOnly,
                    access: Vec::new(),
                },
                ObservedLocalPath {
                    project_id: Some(project_id.clone()),
                    path: "apps/web/.git/index".to_string(),
                    classification: PathClassification::LocalOnly,
                    mode: MaterializationMode::LocalOnly,
                    access: Vec::new(),
                },
                ObservedLocalPath {
                    project_id: Some(project_id),
                    path: "apps/web/src/main.rs".to_string(),
                    classification: PathClassification::WorkspaceSync,
                    mode: MaterializationMode::WorkspaceSync,
                    access: Vec::new(),
                },
            ],
            "2026-07-07T12:00:00Z",
        )
        .expect("observed paths");

    let paths = store
        .blocked_and_local_only_observed_paths(&workspace_id, None)
        .expect("query");

    assert_eq!(
        paths
            .iter()
            .map(|path| path.path.as_str())
            .collect::<Vec<_>>(),
        vec!["apps/web/blocked.pem", "apps/web/.env"]
    );
}
