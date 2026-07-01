use super::*;

#[test]
fn phase11_local_source_change_queues_index_work_and_rebuild_clears_it() {
    let workspace = TempWorkspace::new("phase11-index-work-local-change").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("project dirs");
    std::fs::write(
        project.join("src/session.ts"),
        "export function createSession() {\n  return \"ok\";\n}\n",
    )
    .expect("source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &SnapshotId::new("snap_web"))
        .expect("snapshot");
    drop(store);

    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "createSession".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("initial index build");

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write_session".to_string(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-local"),
            project_id: Some(project_id.clone()),
            path: project.join("src/session.ts").display().to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: Some(ContentId::new("cid_session_changed")),
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "evt_write_session".to_string(),
            settled_at: "2026-06-25T13:30:02Z".to_string(),
            created_at: "2026-06-25T13:30:02Z".to_string(),
        })
        .expect("write log");
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(|work| work.path.as_deref() == Some("src/session.ts")
                && work.state == "pending"
                && work.reason.as_deref() == Some("local-write-log"))
    );
    drop(store);

    let rebuilt = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "createSession".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("rebuild clears work");
    assert_eq!(rebuilt.results.len(), 1);
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .all(|work| work.state == "ready")
    );
}

#[test]
fn phase11_durable_index_rows_are_project_scoped_for_same_relative_path() {
    let workspace = TempWorkspace::new("phase11-index-project-scope").expect("workspace");
    let code_root = workspace.root().join("Code");
    let web = code_root.join("apps/web");
    let api = code_root.join("apps/api");
    std::fs::create_dir_all(web.join("src")).expect("web dirs");
    std::fs::create_dir_all(api.join("src")).expect("api dirs");
    std::fs::write(
        web.join("src/index.ts"),
        "export const webOnlyNeedle = true;\n",
    )
    .expect("web source");
    std::fs::write(
        api.join("src/index.ts"),
        "export const apiOnlyNeedle = true;\n",
    )
    .expect("api source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let web_project_id = ProjectId::new("proj_web");
    let api_project_id = ProjectId::new("proj_api");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &web_project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("web project");
    store
        .insert_project(
            &api_project_id,
            &workspace_id,
            "root_code",
            "apps/api",
            "2026-06-25T13:30:00Z",
        )
        .expect("api project");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &web_project_id,
            &SnapshotId::new("snap_web"),
        )
        .expect("web snapshot");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &api_project_id,
            &SnapshotId::new("snap_api"),
        )
        .expect("api snapshot");
    drop(store);

    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "webOnlyNeedle".to_string(),
        requested_path: Some(web.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("web index build");
    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "apiOnlyNeedle".to_string(),
        requested_path: Some(api.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("api index build");

    let web_search = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "webOnlyNeedle".to_string(),
        requested_path: Some(web.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("web durable search");
    assert_eq!(web_search.results.len(), 1);
    assert_eq!(web_search.results[0].path, "src/index.ts");

    let api_search = search_workspace(SearchCommandOptions {
        db_path: Some(db_path),
        query: "apiOnlyNeedle".to_string(),
        requested_path: Some(api.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:04Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("api durable search");
    assert_eq!(api_search.results.len(), 1);
    assert_eq!(api_search.results[0].path, "src/index.ts");
}

#[test]
fn phase11_durable_index_rows_are_scoped_to_current_snapshot() {
    let workspace = TempWorkspace::new("phase11-durable-index-snapshot-scope").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let old_snapshot_id = SnapshotId::new("snap_old");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &old_snapshot_id)
        .expect("old snapshot");
    drop(store);

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "src/removed.ts",
            "body": "export const oldSnapshotNeedle = true;",
            "contentId": "cid_old"
        }],
        "symbols": [{
            "path": "src/removed.ts",
            "name": "oldSnapshotNeedle",
            "kind": "Function",
            "language": "TypeScript"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id: old_snapshot_id,
        object_key: "indexes_ix_old_snapshot".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-old-snapshot".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:01Z",
    })
    .expect("import old snapshot pack");
    assert_eq!(imported, 1);

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &SnapshotId::new("snap_new"))
        .expect("new snapshot");
    drop(store);

    let search = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "oldSnapshotNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search after snapshot advance");
    assert!(search.results.is_empty());

    let symbols = lookup_symbols(SymbolCommandOptions {
        db_path: Some(db_path),
        query: "oldSnapshotNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols after snapshot advance");
    assert!(symbols.symbols.is_empty());
}

#[test]
fn phase11_durable_index_respects_subdirectory_scope() {
    let workspace = TempWorkspace::new("phase11-index-subdir-scope").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("src dir");
    std::fs::create_dir_all(project.join("tests")).expect("tests dir");
    std::fs::write(
        project.join("src/inside.ts"),
        "export const insideNeedle = true;\n",
    )
    .expect("inside source");
    std::fs::write(
        project.join("tests/outside.ts"),
        "export const outsideNeedle = true;\n",
    )
    .expect("outside source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &snapshot_id)
        .expect("snapshot");
    drop(store);

    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "Needle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("full project index build");

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:outside".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("tests/outside.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 10,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("local-write-log".to_string()),
            updated_at: "2026-06-25T13:30:02Z".to_string(),
        })
        .expect("outside pending work");
    drop(store);

    let scoped_inside = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "insideNeedle".to_string(),
        requested_path: Some(project.join("src").display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("scoped durable search with sibling pending work");
    assert_eq!(
        scoped_inside.index.summary,
        "Local index loaded from durable metadata rows."
    );
    assert_eq!(scoped_inside.results.len(), 1);

    let scoped = search_workspace(SearchCommandOptions {
        db_path: Some(db_path),
        query: "outsideNeedle".to_string(),
        requested_path: Some(project.join("src").display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("scoped durable search");
    assert!(
        scoped.results.is_empty(),
        "subdirectory durable search must not return sibling rows"
    );
}

#[test]
fn phase11_scoped_rebuild_keeps_unrelated_durable_rows() {
    let workspace = TempWorkspace::new("phase11-index-scoped-purge").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("src dir");
    std::fs::create_dir_all(project.join("tests")).expect("tests dir");
    std::fs::write(
        project.join("src/inside.ts"),
        "export const insideNeedle = true;\n",
    )
    .expect("inside source");
    std::fs::write(
        project.join("tests/outside.ts"),
        "export const outsideNeedle = true;\n",
    )
    .expect("outside source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &snapshot_id)
        .expect("snapshot");
    drop(store);

    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "Needle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("full project index build");

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:inside".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("src/inside.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 10,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("local-write-log".to_string()),
            updated_at: "2026-06-25T13:30:02Z".to_string(),
        })
        .expect("pending work");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:outside".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("tests/outside.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 11,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("local-write-log".to_string()),
            updated_at: "2026-06-25T13:30:02Z".to_string(),
        })
        .expect("outside pending work");
    drop(store);

    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "insideNeedle".to_string(),
        requested_path: Some(project.join("src").display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("scoped rebuild");

    let store = MetadataStore::open(&db_path).expect("metadata");
    let paths = store
        .index_documents_for_project(&workspace_id, &project_id)
        .expect("documents")
        .into_iter()
        .map(|document| document.path)
        .collect::<Vec<_>>();
    assert!(paths.iter().any(|path| path == "src/inside.ts"));
    assert!(paths.iter().any(|path| path == "tests/outside.ts"));
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(|work| work.path.as_deref() == Some("tests/outside.ts")
                && work.state == "pending")
    );
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(|work| work.path.as_deref() == Some("src/inside.ts") && work.state == "ready")
    );
}

#[test]
fn phase11_truncated_rebuild_keeps_unscanned_durable_rows() {
    let workspace = TempWorkspace::new("phase11-index-truncated-purge").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("src dir");
    std::fs::write(
        project.join("src/a.ts"),
        "export const alphaNeedle = true;\n",
    )
    .expect("alpha source");
    std::fs::write(
        project.join("src/b.ts"),
        "export const betaNeedle = true;\n",
    )
    .expect("beta source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &snapshot_id)
        .expect("snapshot");
    drop(store);

    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "Needle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("full project index build");

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:alpha".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("src/a.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 10,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("local-write-log".to_string()),
            updated_at: "2026-06-25T13:30:02Z".to_string(),
        })
        .expect("pending work");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:beta".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("src/b.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 11,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("local-write-log".to_string()),
            updated_at: "2026-06-25T13:30:02Z".to_string(),
        })
        .expect("beta pending work");
    drop(store);

    search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "Needle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: Some(IndexedProjectIdentity {
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            snapshot_id: Some(snapshot_id),
            policy_path_prefix: None,
            max_scan_files: Some(1),
        }),
    })
    .expect("truncated rebuild");

    let store = MetadataStore::open(&db_path).expect("metadata");
    let paths = store
        .index_documents_for_project(&workspace_id, &project_id)
        .expect("documents")
        .into_iter()
        .map(|document| document.path)
        .collect::<Vec<_>>();
    assert!(paths.iter().any(|path| path == "src/a.ts"));
    assert!(paths.iter().any(|path| path == "src/b.ts"));
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(|work| work.path.as_deref() == Some("src/b.ts") && work.state == "pending")
    );
}

#[test]
fn phase11_rebuild_purges_deleted_durable_index_rows() {
    let workspace = TempWorkspace::new("phase11-index-stale-purge").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("project dirs");
    let deleted_path = project.join("src/deleted.ts");
    std::fs::write(
        &deleted_path,
        "export function staleSecretNeedle() { return \"gone\"; }\n",
    )
    .expect("source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &SnapshotId::new("snap_web"))
        .expect("snapshot");
    store
        .upsert_projected_node(&ProjectedNodeRecord {
            workspace_id: workspace_id.clone(),
            node_id: "node_deleted".to_string(),
            project_id: None,
            parent_node_id: None,
            path: code_root
                .join("apps/web/src/deleted.ts")
                .display()
                .to_string(),
            kind: NamespaceEntryKind::File,
            content_id: Some(ContentId::new("cid_deleted")),
            hydration_state: HydrationState::Cold,
            updated_at: "2026-06-25T13:30:00Z".to_string(),
        })
        .expect("projected deleted path");
    drop(store);

    let first = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "staleSecretNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("initial index build");
    assert_eq!(first.results.len(), 1);

    std::fs::remove_file(&deleted_path).expect("delete source");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write_deleted".to_string(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-local"),
            project_id: Some(project_id.clone()),
            path: deleted_path.display().to_string(),
            source_path: None,
            operation: "delete".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "evt_write_deleted".to_string(),
            settled_at: "2026-06-25T13:30:02Z".to_string(),
            created_at: "2026-06-25T13:30:02Z".to_string(),
        })
        .expect("write log");
    drop(store);

    let after_delete = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "staleSecretNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("rebuild after delete");
    assert!(after_delete.results.is_empty());
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .index_documents_for_project(&workspace_id, &project_id)
            .expect("documents")
            .iter()
            .all(|document| document.path != "src/deleted.ts")
    );
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .all(|work| work.path.as_deref() != Some("src/deleted.ts") || work.state == "ready")
    );
}

#[test]
fn phase11_durable_index_revalidates_policy_before_returning_rows() {
    let workspace = TempWorkspace::new("phase11-index-policy-revalidate").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("project dirs");
    std::fs::write(
        project.join("src/secret.ts"),
        "export const newlyHiddenNeedle = true;\n",
    )
    .expect("source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T13:30:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T13:30:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T13:30:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &SnapshotId::new("snap_web"))
        .expect("snapshot");
    drop(store);

    let first = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "newlyHiddenNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("initial search");
    assert_eq!(first.results.len(), 1);

    std::fs::write(project.join(".bowlineignore"), b"src/secret.ts\n").expect("policy");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:secret".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("src/secret.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 10,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("projected-node-updated".to_string()),
            updated_at: "2026-06-25T13:30:01Z".to_string(),
        })
        .expect("pending hidden path work");
    drop(store);
    let after_policy = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "newlyHiddenNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("policy revalidated search");
    assert!(after_policy.results.is_empty());
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .index_documents_for_project(&workspace_id, &project_id)
            .expect("documents")
            .iter()
            .all(|document| document.path != "src/secret.ts")
    );
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .all(|work| work.path.as_deref() != Some("src/secret.ts") || work.state == "ready")
    );
}
