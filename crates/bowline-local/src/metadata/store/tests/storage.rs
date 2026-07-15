use super::*;
use crate::metadata::{LocalMetadataRetentionPolicy, SyncOperationRecord};

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
    };
    let error = store
        .put_content_locator(&workspace_id, &locator, "2026-06-24T12:02:00Z")
        .expect_err("missing pack rejected");

    assert!(matches!(error, MetadataError::InvalidStorageMetadata(_)));
}

#[test]
fn unsupported_pack_kind_is_rejected_by_validator_and_schema() {
    let temp = TempWorkspace::new("metadata-storage-kind").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");

    let validator_error = store
        .put_pack_record(
            &workspace_id,
            &PackId::new("pk_0011223344556677"),
            "unsupported-pack-kind",
            4096,
            "pending",
            "2026-06-24T12:01:00Z",
        )
        .expect_err("unsupported kind should fail validation");
    assert!(matches!(
        validator_error,
        MetadataError::InvalidStorageMetadata(_)
    ));

    let schema_error = store
        .connection()
        .execute(
            "INSERT INTO packs
             (workspace_id, pack_id, kind, byte_len, object_hash, key_epoch, state, retain_until, created_at, updated_at)
             VALUES (?1, ?2, 'unsupported-pack-kind', 4096, 'b3_hash', 1, 'pending', NULL, ?3, ?3)",
            rusqlite::params![
                workspace_id.as_str(),
                "pk_0011223344556678",
                "2026-06-24T12:02:00Z",
            ],
        )
        .expect_err("unsupported kind should fail SQLite CHECK");
    assert!(matches!(schema_error, rusqlite::Error::SqliteFailure(_, _)));
}

#[test]
fn store_delete_apis_remove_records_and_treat_missing_as_noop() {
    let temp = TempWorkspace::new("metadata-delete-apis").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let device_id = DeviceId::new("dev_mac");
    let pack_id = PackId::new("pk_0011223344556677");
    let content_id = ContentId::new("cid_delete_me");

    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().join("Code").display().to_string(),
            "2026-06-24T12:00:00Z",
        )
        .expect("root insert");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write_delete_me".to_string(),
            workspace_id: workspace_id.clone(),
            device_id,
            project_id: None,
            path: "app/src/lib.rs".to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: Some(content_id.clone()),
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "req_delete".to_string(),
            settled_at: "2026-06-24T12:04:01Z".to_string(),
            created_at: "2026-06-24T12:04:01Z".to_string(),
        })
        .expect("write log append");
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
    store
        .put_content_locator(
            &workspace_id,
            &ContentLocator {
                content_id: content_id.clone(),
                storage: ContentStorage::Packed,
                raw_size: 18,
                pack_id: Some(pack_id.clone()),
                offset: Some(100),
                length: Some(64),
            },
            "2026-06-24T12:02:00Z",
        )
        .expect("locator insert");

    assert_eq!(
        store
            .delete_local_write(&workspace_id, "write_delete_me")
            .expect("write delete"),
        1
    );
    assert_eq!(
        store
            .delete_local_write(&workspace_id, "write_delete_me")
            .expect("missing write delete"),
        0
    );
    assert!(
        store
            .local_write_log(&workspace_id)
            .expect("write log")
            .is_empty()
    );

    assert_eq!(
        store
            .delete_content_locator(&workspace_id, &content_id)
            .expect("locator delete"),
        1
    );
    assert_eq!(
        store
            .delete_content_locator(&workspace_id, &content_id)
            .expect("missing locator delete"),
        0
    );
    assert!(
        store
            .content_locator(&workspace_id, &content_id)
            .expect("locator query")
            .is_none()
    );

    assert_eq!(
        store
            .delete_pack_record(&workspace_id, &pack_id)
            .expect("pack delete"),
        1
    );
    assert_eq!(
        store
            .delete_pack_record(&workspace_id, &pack_id)
            .expect("missing pack delete"),
        0
    );
    assert!(store.pack_records(&workspace_id).expect("packs").is_empty());
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
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_acme_web");
    let code_root_string = temp.root().join("Code").display().to_string();

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

    assert_eq!(
        store.local_write_log(&workspace_id).expect("write log"),
        vec![write]
    );
    assert_scoped_local_write_queries_filter_and_limit();
    assert_completed_sync_queries_filter_and_limit();
    assert_local_metadata_prune_deletes_old_settled_rows_and_keeps_restore_references();
    assert_local_metadata_prune_keeps_writes_for_min_keep_syncs();
    assert_local_metadata_prune_keeps_syncs_referenced_by_retained_writes();
}

#[test]
fn local_write_log_preserves_insertion_order_for_same_timestamp() {
    let temp = TempWorkspace::new("metadata-local-write-order").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let device_id = DeviceId::new("dev_mac");

    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().join("Code").display().to_string(),
            "2026-06-24T12:00:00Z",
        )
        .expect("root insert");

    let first = LocalWriteLogRecord {
        id: "write_z_first".to_string(),
        workspace_id: workspace_id.clone(),
        device_id: device_id.clone(),
        project_id: None,
        path: "app/src/lib.rs".to_string(),
        source_path: None,
        operation: "modify".to_string(),
        staged_content_id: Some(ContentId::new("cid_first")),
        policy_classification: PathClassification::WorkspaceSync,
        causation_id: "req_first".to_string(),
        settled_at: "2026-06-24T12:04:01Z".to_string(),
        created_at: "2026-06-24T12:04:01Z".to_string(),
    };
    let second = LocalWriteLogRecord {
        id: "write_a_second".to_string(),
        staged_content_id: Some(ContentId::new("cid_second")),
        causation_id: "req_second".to_string(),
        ..first.clone()
    };

    store
        .append_local_write_log(&first)
        .expect("first write append");
    store
        .append_local_write_log(&second)
        .expect("second write append");

    assert_eq!(
        store
            .local_write_log(&workspace_id)
            .expect("write log")
            .into_iter()
            .map(|write| write.id)
            .collect::<Vec<_>>(),
        vec!["write_z_first", "write_a_second"]
    );
}

fn assert_scoped_local_write_queries_filter_and_limit() {
    let temp = TempWorkspace::new("metadata-local-write-scoped").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    seed_workspace_project(&store, &workspace_id, &project_id);
    store
        .insert_project(
            &ProjectId::new("proj_api"),
            &workspace_id,
            "root_code",
            "apps/api",
            "2026-06-24T12:00:00Z",
        )
        .expect("api project insert");

    append_test_write(
        &store,
        &workspace_id,
        Some(project_id.clone()),
        "write_old",
        "apps/web/src/old.ts",
        None,
        "2026-06-24T12:00:00Z",
    );
    append_test_write(
        &store,
        &workspace_id,
        Some(project_id.clone()),
        "write_new",
        "apps/web/src/new.ts",
        Some(ContentId::new("cid_new")),
        "2026-06-24T12:05:00Z",
    );
    append_test_write(
        &store,
        &workspace_id,
        None,
        "write_path_scoped",
        "apps/web/src/path.ts",
        None,
        "2026-06-24T12:06:00Z",
    );
    append_test_write(
        &store,
        &workspace_id,
        Some(ProjectId::new("proj_api")),
        "write_other",
        "apps/api/src/main.ts",
        None,
        "2026-06-24T12:07:00Z",
    );

    assert_eq!(
        store
            .local_writes_for_project(
                &workspace_id,
                &project_id,
                "apps/web",
                Some("2026-06-24T12:01:00Z"),
                Some(2),
            )
            .expect("project writes")
            .into_iter()
            .map(|write| write.id)
            .collect::<Vec<_>>(),
        vec!["write_new", "write_path_scoped"]
    );
    assert_eq!(
        store
            .local_writes_for_path_prefix(&workspace_id, "apps/web/src")
            .expect("path writes")
            .len(),
        3
    );
    assert_eq!(
        store
            .local_write_by_id(&workspace_id, "write_new")
            .expect("write by id")
            .expect("write")
            .staged_content_id
            .as_ref()
            .map(ContentId::as_str),
        Some("cid_new")
    );
    assert_eq!(
        store
            .latest_write_for_path(&workspace_id, &project_id, "apps/web/src/new.ts")
            .expect("latest write")
            .expect("write")
            .id,
        "write_new"
    );
    append_test_write(
        &store,
        &workspace_id,
        Some(project_id.clone()),
        "write_new_real",
        "apps/web/src/new.ts",
        None,
        "2026-06-24T12:08:00Z",
    );
    let latest = store
        .latest_write_for_path(&workspace_id, &project_id, "apps/web/src/new.ts")
        .expect("latest write")
        .expect("write");
    assert_eq!(latest.id, "write_new_real");
    assert_eq!(latest.staged_content_id, None);
    assert_eq!(
        store
            .local_writes_for_project(
                &workspace_id,
                &project_id,
                "apps/web",
                Some("2026-06-24T12:01:00Z"),
                Some(2),
            )
            .expect("newest project writes")
            .into_iter()
            .map(|write| write.id)
            .collect::<Vec<_>>(),
        vec!["write_path_scoped", "write_new_real"]
    );
}

fn assert_completed_sync_queries_filter_and_limit() {
    let temp = TempWorkspace::new("metadata-sync-scoped").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_old",
        SyncOperationState::Completed,
        Some("snap_old"),
        "2026-06-24T12:01:00Z",
    );
    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_new",
        SyncOperationState::Completed,
        Some("snap_new"),
        "2026-06-24T12:02:00Z",
    );
    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_queued",
        SyncOperationState::Queued,
        Some("snap_queued"),
        "2026-06-24T12:03:00Z",
    );

    assert_eq!(
        store
            .completed_sync_operations(
                &workspace_id,
                Some("2026-06-24T12:00:30Z"),
                Some("2026-06-24T12:02:30Z"),
                Some(1),
            )
            .expect("completed operations")
            .into_iter()
            .map(|operation| operation.id)
            .collect::<Vec<_>>(),
        vec!["sync_new"]
    );
    assert_eq!(
        store
            .completed_sync_operation_for_snapshot(&workspace_id, &SnapshotId::new("snap_old"))
            .expect("snapshot operation")
            .expect("operation")
            .id,
        "sync_old"
    );
    assert_eq!(
        store
            .completed_sync_operations_page(
                &workspace_id,
                Some("2026-06-24T12:00:30Z"),
                Some("2026-06-24T12:02:30Z"),
                Some("2026-06-24T12:02:00Z"),
                Some("sync_new"),
                Some(1),
            )
            .expect("completed page")
            .into_iter()
            .map(|operation| operation.id)
            .collect::<Vec<_>>(),
        vec!["sync_old"]
    );
}

fn assert_local_metadata_prune_deletes_old_settled_rows_and_keeps_restore_references() {
    let temp = TempWorkspace::new("metadata-retention").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    seed_workspace_project(&store, &workspace_id, &project_id);
    append_test_write(
        &store,
        &workspace_id,
        Some(project_id.clone()),
        "write_old",
        "apps/web/src/old.ts",
        None,
        "2026-05-01T00:00:00Z",
    );
    append_test_write(
        &store,
        &workspace_id,
        Some(project_id),
        "write_restore",
        "apps/web/src/restore.ts",
        None,
        "2026-05-01T00:00:00Z",
    );
    enqueue_test_sync(
        &store,
        &workspace_id,
        "write_restore",
        SyncOperationState::Completed,
        Some("snap_restore"),
        "2026-06-20T00:00:00Z",
    );
    let report = store
        .prune_local_metadata(
            &workspace_id,
            &LocalMetadataRetentionPolicy {
                completed_sync_min_keep: 0,
                ..LocalMetadataRetentionPolicy::default()
            },
            "2026-07-02T00:00:00Z",
        )
        .expect("prune");

    assert_eq!(report.local_writes_deleted, 1);
    assert!(
        store
            .local_write_by_id(&workspace_id, "write_old")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .local_write_by_id(&workspace_id, "write_restore")
            .unwrap()
            .is_some()
    );
}

fn assert_local_metadata_prune_keeps_writes_for_min_keep_syncs() {
    let temp = TempWorkspace::new("metadata-retention-min-keep").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    seed_workspace_project(&store, &workspace_id, &project_id);
    append_test_write(
        &store,
        &workspace_id,
        Some(project_id.clone()),
        "write_orphan",
        "apps/web/src/orphan.ts",
        None,
        "2026-05-01T00:00:00Z",
    );
    append_test_write(
        &store,
        &workspace_id,
        Some(project_id),
        "write_min_keep_restore",
        "apps/web/src/min_keep_restore.ts",
        None,
        "2026-05-01T00:00:00Z",
    );
    enqueue_test_sync(
        &store,
        &workspace_id,
        "write_min_keep_restore",
        SyncOperationState::Completed,
        Some("snap_min_keep_restore"),
        "2026-05-20T00:00:00Z",
    );

    let report = store
        .prune_local_metadata(
            &workspace_id,
            &LocalMetadataRetentionPolicy {
                completed_sync_min_keep: 1,
                ..LocalMetadataRetentionPolicy::default()
            },
            "2026-07-02T00:00:00Z",
        )
        .expect("prune");

    assert_eq!(report.local_writes_deleted, 1);
    assert_eq!(report.completed_sync_deleted, 0);
    assert!(
        store
            .local_write_by_id(&workspace_id, "write_orphan")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .local_write_by_id(&workspace_id, "write_min_keep_restore")
            .unwrap()
            .is_some()
    );
}

fn assert_local_metadata_prune_keeps_syncs_referenced_by_retained_writes() {
    let temp = TempWorkspace::new("metadata-retention-write-reference").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    seed_workspace_project(&store, &workspace_id, &project_id);
    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_retained_write",
        SyncOperationState::Completed,
        Some("snap_retained_write"),
        "2026-05-01T00:00:00Z",
    );
    append_test_write(
        &store,
        &workspace_id,
        Some(project_id),
        "sync_retained_write",
        "apps/web/src/recent.ts",
        None,
        "2026-06-20T00:00:00Z",
    );

    let report = store
        .prune_local_metadata(
            &workspace_id,
            &LocalMetadataRetentionPolicy {
                completed_sync_min_keep: 0,
                ..LocalMetadataRetentionPolicy::default()
            },
            "2026-07-02T00:00:00Z",
        )
        .expect("prune");

    assert_eq!(report.completed_sync_deleted, 0);
    assert_eq!(report.local_writes_deleted, 0);
    assert!(
        store
            .sync_operation_by_id("sync_retained_write")
            .expect("operation")
            .is_some()
    );
}

fn seed_workspace_project(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) {
    store
        .insert_workspace(workspace_id, "User Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            workspace_id,
            "/tmp/Code",
            "2026-06-24T12:00:00Z",
        )
        .expect("root insert");
    store
        .insert_project(
            project_id,
            workspace_id,
            "root_code",
            "apps/web",
            "2026-06-24T12:00:00Z",
        )
        .expect("project insert");
}

fn append_test_write(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<ProjectId>,
    id: &str,
    path: &str,
    staged_content_id: Option<ContentId>,
    created_at: &str,
) {
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: id.to_string(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("dev_mac"),
            project_id,
            path: path.to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: id.to_string(),
            settled_at: created_at.to_string(),
            created_at: created_at.to_string(),
        })
        .expect("write log append");
}

#[test]
fn sync_operation_enums_round_trip_as_bare_column_values() {
    let temp = TempWorkspace::new("metadata-sync-enum-round-trip").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-07T12:00:00Z")
        .expect("workspace insert");

    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_enum",
        SyncOperationState::Queued,
        None,
        "2026-07-07T12:00:00Z",
    );

    let operation = store
        .sync_operations(&workspace_id)
        .expect("sync operations")
        .pop()
        .expect("operation");
    assert_eq!(operation.kind, SyncOperationKind::Reconcile);
    assert_eq!(operation.state, SyncOperationState::Queued);
    let (kind, state): (String, String) = store
        .connection()
        .query_row(
            "SELECT kind, state FROM sync_operations WHERE id = 'sync_enum'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("raw sync operation");
    assert_eq!(kind, "daemon-reconcile");
    assert_eq!(state, "queued");
}

#[test]
fn sync_claims_are_atomic_and_stale_workers_are_fenced() {
    let temp = TempWorkspace::new("metadata-sync-claim-fencing").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let first = MetadataStore::open(&db_path).expect("first metadata connection");
    let second = MetadataStore::open(&db_path).expect("second metadata connection");
    let workspace_id = WorkspaceId::new("ws_code");
    first
        .insert_workspace(&workspace_id, "User Code", "2026-07-07T12:00:00Z")
        .expect("workspace insert");
    enqueue_test_sync(
        &first,
        &workspace_id,
        "sync_fenced",
        SyncOperationState::Queued,
        None,
        "2026-07-07T12:00:00Z",
    );

    let original = first
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-a",
            "2026-07-07T12:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("first claim")
        .expect("claim available");
    assert_eq!(original.claim.generation(), 1);
    enqueue_test_sync(
        &second,
        &workspace_id,
        "sync_fenced",
        SyncOperationState::Queued,
        None,
        "2026-07-07T12:00:02Z",
    );
    let (state_after_duplicate, token_after_duplicate): (String, Option<String>) = first
        .connection()
        .query_row(
            "SELECT state, claim_token FROM sync_operations WHERE id = ?1",
            ["sync_fenced"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("claim survives duplicate enqueue");
    assert_eq!(state_after_duplicate, "claimed");
    assert_eq!(
        token_after_duplicate.as_deref(),
        Some(original.claim.token().as_str())
    );
    assert!(
        second
            .claim_next_sync_operation(
                &workspace_id,
                "daemon-b",
                "2026-07-07T12:00:02Z",
                "2999-01-01T00:00:00Z",
            )
            .expect("competing claim")
            .is_none()
    );

    first
        .connection()
        .execute(
            "UPDATE sync_operations SET lease_expires_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
            ["sync_fenced"],
        )
        .expect("expire original claim");
    assert_eq!(
        second
            .requeue_expired_sync_claims(&workspace_id, "2026-07-07T12:01:01Z")
            .expect("expired claim requeued"),
        1
    );
    let replacement = second
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-b",
            "2026-07-07T12:01:02Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("replacement claim")
        .expect("replacement available");
    assert_eq!(replacement.claim.generation(), 2);
    assert_ne!(
        original.claim.token().as_str(),
        replacement.claim.token().as_str()
    );

    assert_eq!(
        first
            .complete_claimed_sync_operation(
                &original.claim,
                r#"{"outcome":"stale"}"#,
                "2026-07-07T12:01:03Z",
            )
            .expect("stale completion checked"),
        SyncClaimTransition::OwnershipLost
    );
    let checkpoint = SyncOperationCheckpointRecord {
        id: "checkpoint_fenced".to_string(),
        workspace_id: workspace_id.clone(),
        operation_id: "sync_fenced".to_string(),
        step: "ref-accepted".to_string(),
        state: "completed".to_string(),
        payload_json: "{}".to_string(),
        created_at: "2026-07-07T12:01:03Z".to_string(),
        updated_at: "2026-07-07T12:01:03Z".to_string(),
    };
    assert_eq!(
        first
            .append_claimed_sync_operation_checkpoint(&original.claim, &checkpoint)
            .expect("stale checkpoint checked"),
        SyncClaimTransition::OwnershipLost
    );
    assert_eq!(
        second
            .append_claimed_sync_operation_checkpoint(&replacement.claim, &checkpoint)
            .expect("current checkpoint appended"),
        SyncClaimTransition::Applied
    );
    assert_eq!(
        second
            .renew_sync_operation_claim(
                &replacement.claim,
                "2026-07-07T12:01:03Z",
                "2999-01-01T00:00:00Z",
            )
            .expect("heartbeat renewed"),
        SyncClaimTransition::Applied
    );
    assert_eq!(
        second
            .complete_claimed_sync_operation(
                &replacement.claim,
                r#"{"outcome":"complete"}"#,
                "2026-07-07T12:01:04Z",
            )
            .expect("current completion"),
        SyncClaimTransition::Applied
    );

    let (state, generation, result): (String, u64, Option<String>) = first
        .connection()
        .query_row(
            "SELECT state, claim_generation, result_json FROM sync_operations WHERE id = ?1",
            ["sync_fenced"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("stored claim state");
    assert_eq!(state, "completed");
    assert_eq!(generation, 2);
    assert_eq!(result.as_deref(), Some(r#"{"outcome":"complete"}"#));
}

#[test]
fn boundary_authorization_renews_near_expiry_claim_before_a_replacement_can_overlap() {
    let temp = TempWorkspace::new("metadata-sync-boundary-renewal").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let owner = MetadataStore::open(&db_path).expect("owner metadata connection");
    let replacement = MetadataStore::open(&db_path).expect("replacement metadata connection");
    let workspace_id = WorkspaceId::new("ws_boundary_renewal");
    owner
        .insert_workspace(&workspace_id, "User Code", "2026-07-07T12:00:00Z")
        .expect("workspace insert");
    enqueue_test_sync(
        &owner,
        &workspace_id,
        "sync_boundary_renewal",
        SyncOperationState::Queued,
        Some("snap_boundary"),
        "2026-07-07T12:00:00Z",
    );
    let claimed = owner
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-owner",
            "2026-07-07T12:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim query")
        .expect("operation claimed");
    owner
        .connection()
        .execute(
            "UPDATE sync_operations
             SET lease_expires_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+1 second')
             WHERE id = ?1",
            [claimed.claim.operation_id()],
        )
        .expect("move claim near expiry");
    let near_expiry: String = owner
        .connection()
        .query_row(
            "SELECT lease_expires_at FROM sync_operations WHERE id = ?1",
            [claimed.claim.operation_id()],
            |row| row.get(0),
        )
        .expect("near expiry lease");

    assert_eq!(
        owner
            .authorize_sync_operation_boundary(&claimed.claim)
            .expect("boundary authorization"),
        super::super::SyncClaimCheck::Owned
    );

    let renewed_expiry: String = replacement
        .connection()
        .query_row(
            "SELECT lease_expires_at FROM sync_operations WHERE id = ?1",
            [claimed.claim.operation_id()],
            |row| row.get(0),
        )
        .expect("renewed expiry lease");
    assert!(renewed_expiry > near_expiry);
    assert_eq!(
        replacement
            .requeue_expired_sync_claims(&workspace_id, "1900-01-01T00:00:00Z")
            .expect("replacement requeue check"),
        0
    );
    assert!(
        replacement
            .claim_next_sync_operation(
                &workspace_id,
                "daemon-replacement",
                "2999-01-01T00:00:00Z",
                "2999-01-01T00:01:00Z",
            )
            .expect("replacement claim query")
            .is_none()
    );
}

#[test]
fn expired_cancelled_claim_requires_reconciliation_instead_of_false_terminal_cancellation() {
    let temp =
        TempWorkspace::new("metadata-sync-expired-cancel-reconcile").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata connection");
    let workspace_id = WorkspaceId::new("ws_expired_cancel_reconcile");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-07T12:00:00Z")
        .expect("workspace insert");
    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_expired_cancel_reconcile",
        SyncOperationState::Queued,
        Some("snap_committed_remotely"),
        "2026-07-07T12:00:00Z",
    );
    let original = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-before-crash",
            "2026-07-07T12:00:01Z",
            "2000-01-01T00:00:00Z",
        )
        .expect("claim query")
        .expect("operation claimed");
    assert_eq!(
        store
            .request_sync_operation_cancellation(
                original.claim.operation_id(),
                "2026-07-07T12:00:02Z",
            )
            .expect("cancellation request"),
        Some(super::super::SyncCancellationOutcome::Requested)
    );

    assert_eq!(
        store
            .requeue_expired_sync_claims(&workspace_id, "2026-07-07T12:00:03Z")
            .expect("expired claim sweep"),
        1
    );
    let recovery = store
        .sync_operation_by_id(original.claim.operation_id())
        .expect("operation query")
        .expect("operation remains");
    assert_eq!(recovery.state, SyncOperationState::ReconciliationRequired);
    assert_eq!(recovery.last_error_code, None);
    assert_eq!(
        store
            .sync_operation_counts(&workspace_id)
            .expect("reconciliation count")
            .reconciliation_required,
        1
    );
    assert_ne!(
        recovery.result_json.as_deref(),
        Some(r#"{"outcome":"cancelled"}"#)
    );

    let reconciler = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-after-crash",
            "2026-07-07T12:00:04Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("reconciliation claim query")
        .expect("reconciliation is scheduled before new work");
    assert_eq!(reconciler.operation.state, SyncOperationState::Claimed);
    assert_eq!(
        reconciler.claim.claimed_from_state(),
        SyncOperationState::ReconciliationRequired
    );
    assert_eq!(
        store
            .authorize_sync_operation_boundary(&reconciler.claim)
            .expect("fresh domain boundary remains cancelled"),
        super::super::SyncClaimCheck::CancellationRequested
    );
}

#[test]
fn sync_claim_heartbeat_requires_the_full_claim_identity() {
    let temp = TempWorkspace::new("metadata-sync-heartbeat-fencing").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_heartbeat",
        SyncOperationState::Queued,
        None,
        "2026-07-07T12:00:00Z",
    );
    let claimed = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-a",
            "2026-07-07T12:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("claim available");
    store
        .connection()
        .execute(
            "UPDATE sync_operations SET claim_token = 'replaced-token' WHERE id = ?1",
            [&claimed.operation.id],
        )
        .expect("simulate replacement token");

    assert_eq!(
        store
            .renew_sync_operation_claim(
                &claimed.claim,
                "2026-07-07T12:00:02Z",
                "2026-07-07T12:01:02Z",
            )
            .expect("heartbeat checked"),
        SyncClaimTransition::OwnershipLost
    );
}

#[test]
fn expired_sync_claim_cannot_be_renewed_checkpointed_or_completed() {
    let temp = TempWorkspace::new("metadata-expired-sync-claim").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    enqueue_test_sync(
        &store,
        &workspace_id,
        "sync_expired",
        SyncOperationState::Queued,
        None,
        "2026-07-07T12:00:00Z",
    );
    let claimed = store
        .claim_next_sync_operation(
            &workspace_id,
            "daemon-a",
            "2026-07-07T12:00:01Z",
            "2026-07-07T12:00:02Z",
        )
        .expect("claim")
        .expect("claim available");
    let checkpoint = SyncOperationCheckpointRecord {
        id: "checkpoint_expired".to_string(),
        workspace_id,
        operation_id: claimed.operation.id.clone(),
        step: "ref-cas-authorized".to_string(),
        state: "completed".to_string(),
        payload_json: "{}".to_string(),
        created_at: "2026-07-07T12:00:03Z".to_string(),
        updated_at: "2026-07-07T12:00:03Z".to_string(),
    };

    assert_eq!(
        store
            .renew_sync_operation_claim(
                &claimed.claim,
                "2026-07-07T12:00:03Z",
                "2026-07-07T12:01:03Z",
            )
            .expect("renewal checked"),
        SyncClaimTransition::OwnershipLost
    );
    assert_eq!(
        store
            .append_claimed_sync_operation_checkpoint(&claimed.claim, &checkpoint)
            .expect("checkpoint checked"),
        SyncClaimTransition::OwnershipLost
    );
    assert_eq!(
        store
            .complete_claimed_sync_operation(
                &claimed.claim,
                r#"{"outcome":"late"}"#,
                "2026-07-07T12:00:03Z",
            )
            .expect("completion checked"),
        SyncClaimTransition::OwnershipLost
    );
}

#[test]
fn workspace_sync_head_never_regresses_to_an_older_or_divergent_ref() {
    let temp = TempWorkspace::new("metadata-monotonic-sync-head").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let head =
        |version, snapshot_id: &str, observed_at: &str| super::super::WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version,
                snapshot_id: SnapshotId::new(snapshot_id),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: version },
                updated_by_device_id: Some(DeviceId::new("device-local")),
            },
            observed_at: observed_at.to_string(),
        };

    store
        .upsert_workspace_sync_head(&head(4, "snap_new", "2026-07-07T12:04:00Z"))
        .expect("new head");
    store
        .upsert_workspace_sync_head(&head(3, "snap_old", "2026-07-07T12:05:00Z"))
        .expect("stale write is ignored");
    store
        .upsert_workspace_sync_head(&head(4, "snap_divergent", "2026-07-07T12:06:00Z"))
        .expect("same-version divergence is ignored");
    store
        .upsert_workspace_sync_head(&head(4, "snap_new", "2026-07-07T12:07:00Z"))
        .expect("same ref refreshes observation");

    let stored = store
        .workspace_sync_head(&workspace_id)
        .expect("head query")
        .expect("stored head");
    assert_eq!(stored.workspace_ref.version, 4);
    assert_eq!(stored.workspace_ref.snapshot_id.as_str(), "snap_new");
    assert_eq!(stored.observed_at, "2026-07-07T12:07:00Z");
}

#[test]
fn attention_requeue_escapes_like_wildcards() {
    let temp = TempWorkspace::new("metadata-sync-like-escape").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_code");
    let device_id = DeviceId::new("dev_mac");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-07T12:00:00Z")
        .expect("workspace insert");

    for (id, error) in [
        ("literal", "prefix 100%_literal suffix"),
        ("wildcard", "prefix 100XYliteral suffix"),
    ] {
        let operation = SyncOperationRecord {
            id: id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: crate::metadata::SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Attention,
            idempotency_key: id.to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(device_id.clone()),
            payload_json: "{}".to_string(),
            attempt_count: 0,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: Some(error.to_string()),
            created_at: "2026-07-07T12:00:00Z".to_string(),
            updated_at: "2026-07-07T12:00:00Z".to_string(),
        };
        store
            .enqueue_sync_operation(&operation)
            .expect("operation insert");
    }

    let changed = store
        .requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            SyncOperationKind::Reconcile,
            &device_id,
            "100%_literal",
            "2026-07-07T12:01:00Z",
        )
        .expect("requeue");
    assert_eq!(changed, 1);
    let operations = store
        .sync_operations(&workspace_id)
        .expect("sync operations");
    assert_eq!(
        operations
            .iter()
            .find(|operation| operation.id == "literal")
            .expect("literal")
            .state,
        SyncOperationState::Queued
    );
    assert_eq!(
        operations
            .iter()
            .find(|operation| operation.id == "wildcard")
            .expect("wildcard")
            .state,
        SyncOperationState::Attention
    );
}

fn enqueue_test_sync(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    id: &str,
    state: SyncOperationState,
    target_snapshot_id: Option<&str>,
    updated_at: &str,
) {
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: crate::metadata::SyncResourceKey::workspace_sync(workspace_id.clone()),
            state,
            idempotency_key: id.to_string(),
            base_version: Some(1),
            base_snapshot_id: None,
            target_snapshot_id: target_snapshot_id.map(ToString::to_string),
            device_id: Some(DeviceId::new("dev_mac")),
            payload_json: "{}".to_string(),
            attempt_count: 0,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
        })
        .expect("sync operation");
}
