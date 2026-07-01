use super::*;

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
