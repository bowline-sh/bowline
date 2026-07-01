use super::*;

#[test]
fn packs_and_content_locators_round_trip_through_reserved_tables() {
    let temp = TempWorkspace::new("metadata-storage").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let content_id = ContentId::new("cid_source");
    let first_pack_id = PackId::new("pk_source_00000001");
    let second_pack_id = PackId::new("pk_source_00000002");

    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    store
        .put_pack_record_with_metadata(
            &workspace_id,
            &first_pack_id,
            "source-pack",
            4096,
            "b3_first",
            3,
            "pending",
            Some("2026-06-25T12:00:00Z"),
            "2026-06-24T12:01:00Z",
        )
        .expect("pack insert");
    store
        .put_pack_record(
            &workspace_id,
            &second_pack_id,
            "source-pack",
            8192,
            "pending",
            "2026-06-24T12:01:30Z",
        )
        .expect("second pack insert");

    let first_locator = ContentLocator {
        content_id: content_id.clone(),
        storage: ContentStorage::Packed,
        raw_size: 18,
        pack_id: Some(first_pack_id.clone()),
        offset: Some(100),
        length: Some(64),
        chunk_ids: Vec::new(),
    };
    store
        .put_content_locator(&workspace_id, &first_locator, "2026-06-24T12:02:00Z")
        .expect("locator insert");

    let packs = store.pack_records(&workspace_id).expect("packs");
    assert_eq!(packs.len(), 2);
    assert_eq!(packs[0].object_hash, "b3_first");
    assert_eq!(packs[0].key_epoch, 3);
    assert_eq!(
        packs[0].retain_until.as_deref(),
        Some("2026-06-25T12:00:00Z")
    );
    assert_eq!(
        store
            .content_locator(&workspace_id, &content_id)
            .expect("locator query")
            .expect("locator exists")
            .locator,
        first_locator
    );

    let remapped_locator = ContentLocator {
        content_id: content_id.clone(),
        storage: ContentStorage::Packed,
        raw_size: 18,
        pack_id: Some(second_pack_id),
        offset: Some(2048),
        length: Some(64),
        chunk_ids: Vec::new(),
    };
    store
        .put_content_locator(&workspace_id, &remapped_locator, "2026-06-24T12:03:00Z")
        .expect("locator remap");

    let stored = store
        .content_locator(&workspace_id, &content_id)
        .expect("locator query")
        .expect("locator exists");
    assert_eq!(stored.workspace_id, workspace_id);
    assert_eq!(stored.locator.content_id, content_id);
    assert_eq!(stored.locator, remapped_locator);
    assert_eq!(stored.updated_at, "2026-06-24T12:03:00Z");

    drop(store);
    let reopened = MetadataStore::open(&db_path).expect("metadata reopens");
    let reopened_locator = reopened
        .content_locator(&workspace_id, &content_id)
        .expect("locator query after reopen")
        .expect("locator exists after reopen");
    assert_eq!(
        reopened.pack_records(&workspace_id).expect("packs").len(),
        2
    );
    assert_eq!(reopened_locator.locator, remapped_locator);
    assert_eq!(reopened_locator.updated_at, "2026-06-24T12:03:00Z");
}

#[test]
fn storage_metadata_is_workspace_scoped() {
    let temp = TempWorkspace::new("metadata-storage-workspaces").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let first_workspace = WorkspaceId::new("ws_first");
    let second_workspace = WorkspaceId::new("ws_second");
    let shared_pack_id = PackId::new("pk_0011223344556677");
    let shared_content_id = ContentId::new("cid_shared");

    store
        .insert_workspace(&first_workspace, "First", "2026-06-24T12:00:00Z")
        .expect("first workspace");
    store
        .insert_workspace(&second_workspace, "Second", "2026-06-24T12:00:00Z")
        .expect("second workspace");
    for workspace_id in [&first_workspace, &second_workspace] {
        store
            .put_pack_record(
                workspace_id,
                &shared_pack_id,
                "source-pack",
                4096,
                "pending",
                "2026-06-24T12:01:00Z",
            )
            .expect("pack insert");
    }

    let first_locator = ContentLocator {
        content_id: shared_content_id.clone(),
        storage: ContentStorage::Packed,
        raw_size: 18,
        pack_id: Some(shared_pack_id.clone()),
        offset: Some(100),
        length: Some(64),
        chunk_ids: Vec::new(),
    };
    let second_locator = ContentLocator {
        offset: Some(2048),
        ..first_locator.clone()
    };
    store
        .put_content_locator(&first_workspace, &first_locator, "2026-06-24T12:02:00Z")
        .expect("first locator");
    store
        .put_content_locator(&second_workspace, &second_locator, "2026-06-24T12:03:00Z")
        .expect("second locator");

    assert_eq!(
        store
            .content_locator(&first_workspace, &shared_content_id)
            .expect("first lookup")
            .expect("first locator")
            .locator,
        first_locator
    );
    assert_eq!(
        store
            .content_locator(&second_workspace, &shared_content_id)
            .expect("second lookup")
            .expect("second locator")
            .locator,
        second_locator
    );
    assert_eq!(store.pack_records(&first_workspace).unwrap().len(), 1);
    assert_eq!(store.pack_records(&second_workspace).unwrap().len(), 1);
}

#[test]
fn packed_locators_must_reference_existing_pack_in_same_workspace() {
    let temp = TempWorkspace::new("metadata-storage-missing-pack").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");

    let locator = ContentLocator {
        content_id: ContentId::new("cid_source"),
        storage: ContentStorage::Packed,
        raw_size: 18,
        pack_id: Some(PackId::new("pk_0011223344556677")),
        offset: Some(100),
        length: Some(64),
        chunk_ids: Vec::new(),
    };
    let error = store
        .put_content_locator(&workspace_id, &locator, "2026-06-24T12:02:00Z")
        .expect_err("missing pack rejected");

    assert!(matches!(error, MetadataError::InvalidStorageMetadata(_)));
}

#[test]
fn locator_json_drift_is_rejected_on_read() {
    let temp = TempWorkspace::new("metadata-storage-drift").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let pack_id = PackId::new("pk_0011223344556677");
    let content_id = ContentId::new("cid_source");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    store
        .put_pack_record(
            &workspace_id,
            &pack_id,
            "source-pack",
            4096,
            "pending",
            "2026-06-24T12:01:00Z",
        )
        .expect("pack insert");

    let drifted_json = serde_json::to_string(&ContentLocator {
        content_id: content_id.clone(),
        storage: ContentStorage::Packed,
        raw_size: 18,
        pack_id: Some(pack_id.clone()),
        offset: Some(200),
        length: Some(64),
        chunk_ids: Vec::new(),
    })
    .expect("locator json");
    store
        .connection()
        .execute(
            "INSERT INTO content_locators
             (content_id, workspace_id, storage, raw_size, pack_id, offset, length,
              locator_json, updated_at)
             VALUES (?1, ?2, 'packed', 18, ?3, 100, 64, ?4, '2026-06-24T12:02:00Z')",
            rusqlite::params![
                content_id.as_str(),
                workspace_id.as_str(),
                pack_id.as_str(),
                drifted_json,
            ],
        )
        .expect("drifted row insert");

    assert!(
        store
            .content_locator(&workspace_id, &content_id)
            .expect_err("drift rejected")
            .to_string()
            .contains("locator_json drifted")
    );
}

#[test]
fn materialization_metadata_round_trips() {
    let temp = TempWorkspace::new("metadata-materialization").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let code_root = temp.root().join("Code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_acme_web");
    let content_id = ContentId::new("cid_source");
    let code_root_string = code_root.display().to_string();

    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root_string,
            "2026-06-24T12:00:00Z",
        )
        .expect("root insert");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "acme/web",
            "2026-06-24T12:00:00Z",
        )
        .expect("project insert");

    let projected_path = code_root.join("acme/web/src/main.rs").display().to_string();
    store
        .upsert_projected_node(&ProjectedNodeRecord {
            workspace_id: workspace_id.clone(),
            node_id: "node_src".to_string(),
            project_id: Some(project_id.clone()),
            parent_node_id: Some("node_web".to_string()),
            path: projected_path.clone(),
            kind: NamespaceEntryKind::File,
            content_id: Some(content_id.clone()),
            hydration_state: HydrationState::Cold,
            updated_at: "2026-06-24T12:02:00Z".to_string(),
        })
        .expect("node upsert");

    store
        .enqueue_hydration(&HydrationQueueRecord {
            id: "hydrate_src".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: projected_path.clone(),
            content_id: Some(content_id.clone()),
            priority: "active-read".to_string(),
            state: "queued".to_string(),
            cause: "open-read".to_string(),
            updated_at: "2026-06-24T12:03:00Z".to_string(),
        })
        .expect("hydration enqueue");

    let write = LocalWriteLogRecord {
        id: "write_src".to_string(),
        workspace_id: workspace_id.clone(),
        device_id: DeviceId::new("dev_mac"),
        project_id: Some(project_id),
        path: "acme/web/src/main.rs".to_string(),
        source_path: None,
        operation: "update".to_string(),
        staged_content_id: Some(ContentId::new("cid_staged")),
        policy_classification: PathClassification::WorkspaceSync,
        causation_id: "event_edit".to_string(),
        settled_at: "2026-06-24T12:04:00Z".to_string(),
        created_at: "2026-06-24T12:04:01Z".to_string(),
    };
    store
        .append_local_write_log(&write)
        .expect("write log append");

    let node = store
        .projected_node_by_path(&workspace_id, "acme/web/src/main.rs")
        .expect("node lookup")
        .expect("node exists");
    assert_eq!(node.node_id, "node_src");
    assert_eq!(node.path, "acme/web/src/main.rs");
    assert_eq!(node.hydration_state, HydrationState::Cold);
    assert_eq!(
        store.hydration_queue(&workspace_id).expect("queue")[0].path,
        "acme/web/src/main.rs"
    );
    assert_eq!(
        store.local_write_log(&workspace_id).expect("write log"),
        vec![write]
    );
}
