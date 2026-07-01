use super::*;

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
