use super::*;

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
            b"module github.com/bowline-sh/bowline-demo\n\nrequire (\n\tgithub.com/charmbracelet/bubbletea v1.3.4\n\tgolang.org/x/sync v0.12.0\n)\n",
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
