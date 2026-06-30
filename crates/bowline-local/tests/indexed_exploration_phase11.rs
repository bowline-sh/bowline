use std::collections::BTreeSet;

use bowline_core::{
    commands::{IndexState, SymbolKind, SymbolLanguage},
    ids::{ContentId, DeviceId, ProjectId, SnapshotId, WorkspaceId},
    policy::PathClassification,
    workspace_graph::{HydrationState, NamespaceEntryKind},
};
use bowline_local::{
    indexed::{DecryptedIndexPackImport, IndexedProjectIdentity, import_decrypted_index_pack},
    metadata::{IndexWorkRecord, LocalWriteLogRecord, MetadataStore, ProjectedNodeRecord},
    search::{SearchCommandOptions, search_workspace},
    symbols::{SymbolCommandOptions, lookup_symbols},
    workspace::TempWorkspace,
};
use bowline_storage::{StorageKey, open_index_pack, seal_index_pack};

#[test]
fn phase11_search_returns_redacted_indexed_hits_without_generated_noise() {
    let workspace = TempWorkspace::new("phase11-search").expect("workspace");
    let project = workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "src/auth/callback.ts",
            b"export async function handleAuthCallback(request: Request) {\n  return request.url;\n}\n",
        )
        .expect("source");
    workspace
        .write_project_file("app", ".env.local", b"AUTH_TOKEN=super-secret-value\n")
        .expect("env");
    workspace
        .write_project_file("app", "id_rsa", b"PRIVATE KEY handleAuthCallback\n")
        .expect("secret-looking");
    workspace
        .write_project_file("app", ".bowlineignore", b"ignored.ts\n")
        .expect("ignore");
    workspace
        .write_project_file("app", "ignored.ts", b"handleAuthCallback\n")
        .expect("ignored source");
    workspace
        .write_project_file("app", "node_modules/pkg/index.js", b"handleAuthCallback\n")
        .expect("generated dependency");

    let output = search_workspace(SearchCommandOptions {
        db_path: None,
        query: "handleAuthCallback".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:00Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search works");

    assert_eq!(output.results.len(), 1);
    assert_eq!(output.results[0].path, "src/auth/callback.ts");
    assert_eq!(output.index.file_count, 2);
    let json = serde_json::to_string(&output).expect("search json");
    assert!(!json.contains("super-secret-value"));
    assert!(!json.contains("AUTH_TOKEN"));
    assert!(!json.contains("id_rsa"));
    assert!(!json.contains("ignored.ts"));
    assert!(!json.contains("node_modules"));
}

#[test]
fn phase11_search_truncation_uses_one_extra_result_probe() {
    let workspace = TempWorkspace::new("phase11-search-truncation").expect("workspace");
    let project = workspace.create_project("app").expect("project");
    for index in 0..20 {
        workspace
            .write_project_file(
                "app",
                format!("src/exact-{index:02}.ts"),
                b"export const sharedNeedle = true;\n",
            )
            .expect("source");
    }

    let exact = search_workspace(SearchCommandOptions {
        db_path: None,
        query: "sharedNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search works");
    assert_eq!(exact.results.len(), 20);
    assert!(!exact.truncated);

    workspace
        .write_project_file(
            "app",
            "src/overflow.ts",
            b"export const sharedNeedle = false;\n",
        )
        .expect("overflow source");
    let overflow = search_workspace(SearchCommandOptions {
        db_path: None,
        query: "sharedNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:04Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search works");
    assert_eq!(overflow.results.len(), 20);
    assert!(overflow.truncated);
}

#[test]
fn phase11_search_and_symbols_survive_restart_from_durable_rows() {
    let workspace = TempWorkspace::new("phase11-durable-index-restart").expect("workspace");
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        query: "createSession".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("initial search works");
    assert_eq!(first.results.len(), 1);

    std::fs::write(
        project.join("src/session.ts"),
        "export const renamed = true;\n",
    )
    .expect("source changed after index build");

    let stale = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "createSession".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("stale durable rows are not trusted");
    assert!(stale.results.is_empty());

    let fresh = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "renamed".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("current file content is indexed");
    assert_eq!(fresh.results.len(), 1);
    assert_eq!(fresh.results[0].path, "src/session.ts");

    std::fs::write(project.join("src/large.ts"), vec![b'a'; 1024 * 1024 + 1])
        .expect("large local file");
    let durable_after_large = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "renamed".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:04Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("large skipped file does not force rebuild");
    assert_eq!(durable_after_large.results.len(), 1);
    assert_eq!(
        durable_after_large.index.summary,
        "Local index loaded from durable metadata rows."
    );

    std::fs::write(
        project.join("src/new_file.ts"),
        "export const newFileNeedle = true;\n",
    )
    .expect("new local file");
    let new_file = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "newFileNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("new local file is indexed");
    assert_eq!(new_file.results.len(), 1);
    assert_eq!(new_file.results[0].path, "src/new_file.ts");

    let symbols = lookup_symbols(SymbolCommandOptions {
        db_path: Some(db_path),
        query: "renamed".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:06Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("fresh symbols work");
    assert!(
        symbols
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Variable)
    );
}

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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

#[test]
fn phase11_decrypted_index_pack_covers_cold_search_and_symbols() {
    let workspace = TempWorkspace::new("phase11-index-pack-import").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "src/cold.ts",
            "body": "export function coldNeedle() { return 42; }",
            "contentId": "cid_cold",
            "policySummary": "source from encrypted index pack"
        }],
        "symbols": [{
            "path": "src/cold.ts",
            "name": "coldNeedle",
            "kind": "Function",
            "language": "TypeScript",
            "lineStart": 0,
            "lineEnd": 0,
            "byteStart": 16,
            "byteEnd": 26
        }]
    })
    .to_string();
    let key = StorageKey::deterministic(41);
    let sealed = seal_index_pack(
        workspace_id.clone(),
        snapshot_id.clone(),
        plaintext.as_bytes(),
        key,
        1,
    )
    .expect("sealed index pack");
    let opened = open_index_pack(&sealed, key, &workspace_id).expect("opened index pack");
    assert_ne!(sealed.bytes, plaintext.as_bytes());

    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id: snapshot_id.clone(),
        object_key: sealed.pointer.object_key.as_str().to_string(),
        byte_len: sealed.pointer.byte_len,
        hash: sealed.pointer.hash.clone(),
        plaintext: &opened,
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import index pack");
    assert_eq!(imported, 1);
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert_eq!(
        store
            .index_packs_for_project(&workspace_id, &project_id)
            .expect("index packs")[0]
            .state,
        "ready"
    );
    drop(store);

    let search = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "coldNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search from imported pack");
    assert_eq!(search.results.len(), 1);
    assert_eq!(search.results[0].path, "src/cold.ts");

    let symbols = lookup_symbols(SymbolCommandOptions {
        db_path: Some(db_path),
        query: "coldNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:06Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols from imported pack");
    assert_eq!(symbols.symbols.len(), 1);
    assert_eq!(symbols.symbols[0].kind, SymbolKind::Function);
}

#[test]
fn phase11_partial_index_pack_does_not_hide_local_materialized_files() {
    let workspace = TempWorkspace::new("phase11-index-pack-partial-local").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("project dir");
    std::fs::write(
        project.join("src/local.ts"),
        "export const localNeedle = true;\n",
    )
    .expect("local source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
    store
        .upsert_projected_node(&ProjectedNodeRecord {
            workspace_id: workspace_id.clone(),
            node_id: "node_cold".to_string(),
            project_id: None,
            parent_node_id: None,
            path: code_root.join("apps/web/src/cold.ts").display().to_string(),
            kind: NamespaceEntryKind::File,
            content_id: Some(ContentId::new("cid_cold")),
            hydration_state: HydrationState::Cold,
            updated_at: "2026-06-25T13:30:01Z".to_string(),
        })
        .expect("projected cold node");
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(|work| work.path.as_deref() == Some("src/cold.ts")
                && work.state == "pending"
                && work.reason.as_deref() == Some("projected-node-updated"))
    );
    drop(store);

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "src/cold.ts",
            "body": "export const coldPackNeedle = true;",
            "contentId": "cid_cold"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_partial_local".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-partial-local".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import partial pack");
    assert_eq!(imported, 1);

    let local = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "localNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search scans local files when pack is partial");
    assert_eq!(local.results.len(), 1);
    assert_eq!(local.results[0].path, "src/local.ts");

    let cold = search_workspace(SearchCommandOptions {
        db_path: Some(db_path),
        query: "coldPackNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:06Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("cold pack rows survive local scan");
    assert_eq!(cold.results.len(), 1);
    assert_eq!(cold.results[0].path, "src/cold.ts");
}

#[test]
fn phase11_rebuild_purges_cold_rows_without_projected_namespace_backing() {
    let workspace = TempWorkspace::new("phase11-index-pack-cold-purge").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("project dir");
    std::fs::write(
        project.join("src/local.ts"),
        "export const localNeedle = true;\n",
    )
    .expect("local source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "src/stale_cold.ts",
            "body": "export const staleColdNeedle = true;",
            "contentId": "cid_stale_cold"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_stale_cold".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-stale-cold".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import stale cold pack");
    assert_eq!(imported, 1);

    let local = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "localNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("local rebuild");
    assert_eq!(local.results.len(), 1);

    let stale = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "staleColdNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:06Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("stale cold search");
    assert!(stale.results.is_empty());
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .index_documents_for_project(&workspace_id, &project_id)
            .expect("documents")
            .iter()
            .all(|document| document.path != "src/stale_cold.ts")
    );
}

#[test]
fn phase11_durable_cold_rows_require_current_projected_namespace_backing() {
    let workspace = TempWorkspace::new("phase11-index-pack-cold-namespace").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
    for (node_id, path, content_id) in [
        ("node_removed", "apps/web/src/removed.ts", "cid_removed"),
        ("node_kept", "apps/web/src/kept.ts", "cid_kept"),
    ] {
        store
            .upsert_projected_node(&ProjectedNodeRecord {
                workspace_id: workspace_id.clone(),
                node_id: node_id.to_string(),
                project_id: None,
                parent_node_id: None,
                path: path.to_string(),
                kind: NamespaceEntryKind::File,
                content_id: Some(ContentId::new(content_id)),
                hydration_state: HydrationState::Cold,
                updated_at: "2026-06-25T13:30:01Z".to_string(),
            })
            .expect("projected node");
    }
    drop(store);

    let plaintext = serde_json::json!({
        "documents": [
            {
                "path": "src/removed.ts",
                "body": "export const removedColdNeedle = true;",
                "contentId": "cid_removed"
            },
            {
                "path": "src/kept.ts",
                "body": "export const keptColdNeedle = true;",
                "contentId": "cid_kept"
            }
        ]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_cold_namespace".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-cold-namespace".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:02Z",
    })
    .expect("import cold namespace pack");
    assert_eq!(imported, 2);

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .delete_unlisted_workspace_projected_nodes(
            &workspace_id,
            &BTreeSet::from(["apps/web/src/kept.ts".to_string()]),
        )
        .expect("remove projected deleted path");
    drop(store);

    let removed = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "removedColdNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("removed cold search");
    assert!(removed.results.is_empty());

    let kept = search_workspace(SearchCommandOptions {
        db_path: Some(db_path),
        query: "keptColdNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:04Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("kept cold search");
    assert_eq!(kept.results.len(), 1);
    assert_eq!(kept.results[0].path, "src/kept.ts");
}

#[test]
fn phase11_index_pack_import_clears_stale_symbols_for_replaced_document() {
    let workspace = TempWorkspace::new("phase11-index-pack-replace-symbols").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    let first_pack = serde_json::json!({
        "documents": [{
            "path": "src/cold.ts",
            "body": "export function coldNeedle() { return 42; }",
            "contentId": "cid_cold_1"
        }],
        "symbols": [{
            "path": "src/cold.ts",
            "name": "coldNeedle",
            "kind": "Function",
            "language": "TypeScript"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id: snapshot_id.clone(),
        object_key: "indexes_ix_symbols_first".to_string(),
        byte_len: first_pack.len() as u64,
        hash: "hash-symbols-first".to_string(),
        plaintext: first_pack.as_bytes(),
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import first index pack");
    assert_eq!(imported, 1);

    let first_symbols = lookup_symbols(SymbolCommandOptions {
        db_path: Some(db_path.clone()),
        query: "coldNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols from first pack");
    assert_eq!(first_symbols.symbols.len(), 1);

    let second_pack = serde_json::json!({
        "documents": [{
            "path": "src/cold.ts",
            "body": "export const renamedNeedle = 42;",
            "contentId": "cid_cold_2"
        }],
        "symbols": []
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_symbols_second".to_string(),
        byte_len: second_pack.len() as u64,
        hash: "hash-symbols-second".to_string(),
        plaintext: second_pack.as_bytes(),
        now: "2026-06-25T13:30:06Z",
    })
    .expect("import replacement index pack");
    assert_eq!(imported, 1);

    let stale_symbols = lookup_symbols(SymbolCommandOptions {
        db_path: Some(db_path.clone()),
        query: "coldNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:07Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols after replacement pack");
    assert!(stale_symbols.symbols.is_empty());

    let deleted_pack = serde_json::json!({
        "fullSnapshot": true,
        "documents": [],
        "symbols": []
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id: SnapshotId::new("snap_web"),
        object_key: "indexes_ix_symbols_deleted".to_string(),
        byte_len: deleted_pack.len() as u64,
        hash: "hash-symbols-deleted".to_string(),
        plaintext: deleted_pack.as_bytes(),
        now: "2026-06-25T13:30:08Z",
    })
    .expect("import deleted-document index pack");
    assert_eq!(imported, 0);

    let deleted_search = search_workspace(SearchCommandOptions {
        db_path: Some(db_path.clone()),
        query: "renamedNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:09Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search after deleted-document pack");
    assert!(deleted_search.results.is_empty());
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .index_documents_for_project(&workspace_id, &project_id)
            .expect("documents")
            .is_empty()
    );
}

#[test]
fn phase11_index_pack_import_composes_multiple_packs_for_same_snapshot() {
    let workspace = TempWorkspace::new("phase11-index-pack-multipack").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    for (object_key, hash, path, body, needle) in [
        (
            "indexes_ix_multipack_a",
            "hash-multipack-a",
            "src/a.ts",
            "export const firstPackNeedle = true;",
            "firstPackNeedle",
        ),
        (
            "indexes_ix_multipack_b",
            "hash-multipack-b",
            "src/b.ts",
            "export const secondPackNeedle = true;",
            "secondPackNeedle",
        ),
    ] {
        let payload = serde_json::json!({
            "documents": [{
                "path": path,
                "body": body,
                "contentId": format!("cid_{needle}")
            }],
            "symbols": []
        })
        .to_string();
        let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
            db_path: &db_path,
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            snapshot_id: snapshot_id.clone(),
            object_key: object_key.to_string(),
            byte_len: payload.len() as u64,
            hash: hash.to_string(),
            plaintext: payload.as_bytes(),
            now: "2026-06-25T13:30:04Z",
        })
        .expect("import index pack");
        assert_eq!(imported, 1);
    }

    for (needle, expected_path) in [
        ("firstPackNeedle", "src/a.ts"),
        ("secondPackNeedle", "src/b.ts"),
    ] {
        let search = search_workspace(SearchCommandOptions {
            db_path: Some(db_path.clone()),
            query: needle.to_string(),
            requested_path: Some(project.display().to_string()),
            path_prefix: None,
            generated_at: "2026-06-25T13:30:05Z".to_string(),
            limit: 20,
            project_identity: None,
        })
        .expect("search after multiple packs");
        assert_eq!(search.results.len(), 1);
        assert_eq!(search.results[0].path, expected_path);
    }
}

#[test]
fn phase11_index_pack_import_rejects_traversal_document_paths() {
    let workspace = TempWorkspace::new("phase11-index-pack-traversal-doc").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "../private/hidden.ts",
            "body": "export const hiddenTraversalNeedle = true;",
            "contentId": "cid_hidden"
        }],
        "symbols": [{
            "path": "../private/hidden.ts",
            "name": "hiddenTraversalNeedle",
            "kind": "Function",
            "language": "TypeScript"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_traversal_doc".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-traversal-doc".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import traversal index pack");
    assert_eq!(imported, 0);

    let search = search_workspace(SearchCommandOptions {
        db_path: Some(db_path),
        query: "hiddenTraversalNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search after rejected traversal pack");
    assert!(search.results.is_empty());
}

#[test]
fn phase11_index_pack_import_rejects_traversal_symbol_paths() {
    let workspace = TempWorkspace::new("phase11-index-pack-traversal-symbol").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "src/good.ts",
            "body": "export function goodNeedle() { return true; }",
            "contentId": "cid_good"
        }],
        "symbols": [{
            "path": "../src/good.ts",
            "name": "goodNeedle",
            "kind": "Function",
            "language": "TypeScript"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_traversal_symbol".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-traversal-symbol".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import traversal symbol index pack");
    assert_eq!(imported, 1);

    let symbols = lookup_symbols(SymbolCommandOptions {
        db_path: Some(db_path),
        query: "goodNeedle".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols after rejected traversal symbol");
    assert!(symbols.symbols.is_empty());
}

#[test]
fn phase11_index_pack_import_revalidates_current_local_policy() {
    let workspace = TempWorkspace::new("phase11-index-pack-policy").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(project.join(".bowlineignore"), b"private/**\n").expect("policy");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "private/hidden.ts",
            "body": "export const hiddenPackNeedle = true;",
            "contentId": "cid_hidden"
        }],
        "symbols": [{
            "path": "private/hidden.ts",
            "name": "hiddenPackNeedle",
            "kind": "Function",
            "language": "TypeScript"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_policy".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-policy".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import policy-filtered pack");
    assert_eq!(imported, 0);
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .index_documents_for_project(&workspace_id, &project_id)
            .expect("documents")
            .is_empty()
    );
}

#[test]
fn phase11_index_pack_import_does_not_clear_unrelated_pending_work() {
    let workspace = TempWorkspace::new("phase11-index-pack-pending").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(&project).expect("project dir");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    let snapshot_id = SnapshotId::new("snap_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:pending".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("src/local.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 10,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("local-write-log".to_string()),
            updated_at: "2026-06-25T13:30:01Z".to_string(),
        })
        .expect("pending work");
    store
        .upsert_index_work(&IndexWorkRecord {
            id: "index_work:ws_code:proj_web:path:cold".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some("src/cold.ts".to_string()),
            kind: "path".to_string(),
            source_watermark: 11,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some("projected-node-updated".to_string()),
            updated_at: "2026-06-25T13:30:01Z".to_string(),
        })
        .expect("cold pending work");
    drop(store);

    let plaintext = serde_json::json!({
        "documents": [{
            "path": "src/cold.ts",
            "body": "export const coldPackNeedle = true;",
            "contentId": "cid_cold"
        }]
    })
    .to_string();
    let imported = import_decrypted_index_pack(DecryptedIndexPackImport {
        db_path: &db_path,
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        snapshot_id,
        object_key: "indexes_ix_pending".to_string(),
        byte_len: plaintext.len() as u64,
        hash: "hash-pending".to_string(),
        plaintext: plaintext.as_bytes(),
        now: "2026-06-25T13:30:04Z",
    })
    .expect("import index pack");
    assert_eq!(imported, 1);
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(
                |work| work.state == "pending" && work.reason.as_deref() == Some("local-write-log")
            )
    );
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(|work| work.path.as_deref() == Some("src/cold.ts") && work.state == "ready")
    );
}

#[test]
fn phase11_search_marks_local_index_stale_when_projected_files_are_cold() {
    let workspace = TempWorkspace::new("phase11-search-cold-projected").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("src")).expect("project dirs");
    std::fs::write(
        project.join("src/local.ts"),
        "export function localOnly() {}\n",
    )
    .expect("local source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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
            node_id: "node_remote".to_string(),
            project_id: Some(project_id.clone()),
            parent_node_id: None,
            path: code_root
                .join("apps/web/src/remote.ts")
                .display()
                .to_string(),
            kind: NamespaceEntryKind::File,
            content_id: Some(ContentId::new("cid_remote")),
            hydration_state: HydrationState::Cold,
            updated_at: "2026-06-25T13:30:01Z".to_string(),
        })
        .expect("projected node");
    assert!(
        store
            .index_work_for_project(&workspace_id, &project_id)
            .expect("index work")
            .iter()
            .any(|work| work.path.as_deref() == Some("src/remote.ts") && work.state == "pending")
    );

    let output = search_workspace(SearchCommandOptions {
        db_path: Some(db_path),
        query: "localOnly".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search works");

    assert_eq!(output.index.state, IndexState::Stale);
    assert_eq!(output.index.pending_path_count, Some(1));
    assert_eq!(output.results.len(), 1);
}

#[test]
fn phase11_subdirectory_search_keeps_parent_project_policy() {
    let workspace = TempWorkspace::new("phase11-search-parent-policy").expect("workspace");
    let code_root = workspace.root().join("Code");
    let project = code_root.join("apps/web");
    std::fs::create_dir_all(project.join("private")).expect("private dir");
    std::fs::write(project.join(".bowlineignore"), b"private/**\n").expect("policy");
    std::fs::write(project.join("private/token.txt"), b"hiddenNeedle\n").expect("private source");

    let db_path = workspace.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T13:30:00Z")
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

    let output = search_workspace(SearchCommandOptions {
        db_path: Some(db_path),
        query: "hiddenNeedle".to_string(),
        requested_path: Some(project.join("private").display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:07Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search works");

    assert!(output.results.is_empty());
}

#[test]
fn phase11_subdirectory_search_without_metadata_keeps_parent_policy() {
    let workspace =
        TempWorkspace::new("phase11-search-parent-policy-no-metadata").expect("workspace");
    let project = workspace.create_project("app").expect("project");
    let missing_db_path = workspace.root().join(".missing-state/local.sqlite3");
    std::fs::write(project.join(".bowlineignore"), b"private/**\n").expect("policy");
    std::fs::create_dir_all(project.join("private")).expect("private dir");
    std::fs::write(project.join("private/token.txt"), b"hiddenNeedle\n").expect("private source");
    std::fs::write(project.join("private/visible.txt"), b"visibleNeedle\n").expect("scoped source");

    let output = search_workspace(SearchCommandOptions {
        db_path: Some(missing_db_path),
        query: "Needle".to_string(),
        requested_path: Some(project.join("private").display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:08Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("search works");

    assert!(output.results.is_empty());
}

#[test]
fn phase11_symbol_truncation_uses_one_extra_result_probe() {
    let workspace = TempWorkspace::new("phase11-symbol-truncation").expect("workspace");
    let project = workspace.create_project("app").expect("project");
    for index in 0..10 {
        workspace
            .write_project_file(
                "app",
                format!("src/exact-{index:02}.ts"),
                b"export function repeatedSymbol() {}\n",
            )
            .expect("source");
    }

    let exact = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "repeatedSymbol".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:05Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols work");
    assert_eq!(exact.symbols.len(), 20);
    assert!(!exact.truncated);

    workspace
        .write_project_file(
            "app",
            "src/overflow.ts",
            b"export function repeatedSymbol() {}\n",
        )
        .expect("overflow source");
    let overflow = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "repeatedSymbol".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:06Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols work");
    assert_eq!(overflow.symbols.len(), 20);
    assert!(overflow.truncated);
}

#[test]
fn phase11_symbols_preserve_non_function_kinds() {
    let workspace = TempWorkspace::new("phase11-symbol-kinds").expect("workspace");
    let project = workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "web/model.ts",
            b"export class UserSession {}\nexport interface SessionShape {}\n",
        )
        .expect("typescript");
    workspace
        .write_project_file(
            "app",
            "src/model.rs",
            b"pub struct UserRecord {}\npub enum UserKind {}\npub trait UserTrait {}\n",
        )
        .expect("rust");

    let class = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "UserSession".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:08Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("class symbols");
    assert!(
        class
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Class)
    );

    let interface = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "SessionShape".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:09Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("interface symbols");
    assert!(
        interface
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Interface)
    );

    let structure = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "UserRecord".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:10Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("struct symbols");
    assert_eq!(structure.symbols[0].kind, SymbolKind::Struct);

    let enumeration = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "UserKind".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:11Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("enum symbols");
    assert_eq!(enumeration.symbols[0].kind, SymbolKind::Enum);

    let trait_result = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "UserTrait".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:30:12Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("trait symbols");
    assert_eq!(trait_result.symbols[0].kind, SymbolKind::Trait);
}

#[test]
fn phase11_symbols_find_supported_language_definitions() {
    let workspace = TempWorkspace::new("phase11-symbols").expect("workspace");
    let project = workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "web/session.ts",
            b"export function createSession() {}\n",
        )
        .expect("ts");
    workspace
        .write_project_file(
            "app",
            "api/session.py",
            b"def create_session():\n    pass\n",
        )
        .expect("python");
    workspace
        .write_project_file("app", "src/lib.rs", b"pub fn create_session() {}\n")
        .expect("rust");
    workspace
        .write_project_file("app", "cmd/main.go", b"func CreateSession() {}\n")
        .expect("go");

    let ts = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "createSession".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:31:00Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols work");
    assert!(
        ts.symbols
            .iter()
            .any(|symbol| symbol.path == "web/session.ts" && symbol.kind == SymbolKind::Function)
    );

    let snake = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "create_session".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:32:00Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols work");
    assert_eq!(snake.symbols.len(), 2);

    let go = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "CreateSession".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:33:00Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("symbols work");
    assert_eq!(go.symbols[0].path, "cmd/main.go");
}

#[test]
fn phase11_symbols_include_package_manifest_references() {
    let workspace = TempWorkspace::new("phase11-symbol-manifests").expect("workspace");
    let project = workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "package.json",
            br#"{"name":"bowline-demo","dependencies":{"@tanstack/start":"latest","react":"latest"}}"#,
        )
        .expect("package json");
    workspace
        .write_project_file(
            "app",
            "Cargo.toml",
            b"[package]\nname = \"bowline-rust-demo\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .expect("cargo");
    workspace
        .write_project_file(
            "app",
            "go.mod",
            b"module github.com/crowlabs-dev/bowline-demo\n\nrequire (\n\tgithub.com/charmbracelet/bubbletea v1.3.4\n\tgolang.org/x/sync v0.12.0\n)\n",
        )
        .expect("go mod");

    let package = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "bowline-demo".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:34:00Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("package symbol");
    assert!(package.symbols.iter().any(|symbol| {
        symbol.path == "package.json"
            && symbol.kind == SymbolKind::Export
            && symbol.language == SymbolLanguage::JavaScript
    }));

    let dependency = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "react".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:34:01Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("dependency symbol");
    assert!(dependency.symbols.iter().any(|symbol| {
        symbol.path == "package.json"
            && symbol.kind == SymbolKind::Import
            && symbol.language == SymbolLanguage::JavaScript
    }));

    let rust_dependency = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "serde".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:34:02Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("rust dependency symbol");
    assert!(rust_dependency.symbols.iter().any(|symbol| {
        symbol.path == "Cargo.toml"
            && symbol.kind == SymbolKind::Import
            && symbol.language == SymbolLanguage::Rust
    }));

    let go_dependency = lookup_symbols(SymbolCommandOptions {
        db_path: None,
        query: "github.com/charmbracelet/bubbletea".to_string(),
        requested_path: Some(project.display().to_string()),
        path_prefix: None,
        generated_at: "2026-06-25T13:34:03Z".to_string(),
        limit: 20,
        project_identity: None,
    })
    .expect("go dependency symbol");
    assert!(go_dependency.symbols.iter().any(|symbol| {
        symbol.path == "go.mod"
            && symbol.kind == SymbolKind::Import
            && symbol.language == SymbolLanguage::Go
    }));
}
