use super::*;

#[test]
fn schema_initialization_is_idempotent_and_enables_wal() {
    let temp = TempWorkspace::new("metadata-idempotent").expect("temp workspace");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let store = MetadataStore::open(&db_path).expect("metadata opens");
    assert_eq!(store.journal_mode().expect("journal mode"), "wal");
    store.assert_schema_tables().expect("schema tables exist");
    drop(store);

    let reopened = MetadataStore::open(&db_path).expect("metadata reopens");
    reopened
        .assert_schema_tables()
        .expect("schema tables exist");
}

#[test]
fn command_idempotency_reservation_does_not_overwrite_conflicts() {
    let temp = TempWorkspace::new("metadata-command-idempotency").expect("temp workspace");
    let db_path = temp.root().join(".state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_idem");
    store
        .insert_workspace(&workspace_id, "Idempotency", "2026-06-29T12:00:00Z")
        .expect("workspace");

    let mut record = CommandIdempotencyRecord {
        workspace_id: workspace_id.clone(),
        idempotency_key: "key-1".to_string(),
        command: "workon".to_string(),
        request_hash: "hash-a".to_string(),
        result_json: "{}".to_string(),
        status: "pending".to_string(),
        created_at: "2026-06-29T12:00:00Z".to_string(),
        updated_at: "2026-06-29T12:00:00Z".to_string(),
        expires_at: "2026-07-06T12:00:00Z".to_string(),
    };
    assert!(
        store
            .try_insert_command_idempotency_record(&record)
            .expect("reservation insert")
    );

    let mut conflicting = record.clone();
    conflicting.request_hash = "hash-b".to_string();
    assert!(
        !store
            .try_insert_command_idempotency_record(&conflicting)
            .expect("conflicting reservation ignored")
    );

    record.result_json = "{\"ok\":true}".to_string();
    record.status = "success".to_string();
    store
        .finish_command_idempotency_record(&record)
        .expect("finish reservation");
    conflicting.result_json = "{\"ok\":false}".to_string();
    conflicting.status = "success".to_string();
    store
        .upsert_command_idempotency_record(&conflicting)
        .expect("conflicting upsert is ignored");

    let stored = store
        .command_idempotency_record(&workspace_id, "key-1")
        .expect("stored record")
        .expect("record exists");
    assert_eq!(stored.request_hash, "hash-a");
    assert_eq!(stored.result_json, "{\"ok\":true}");
}

#[test]
fn older_versioned_schema_is_refused_without_reinitializing() {
    let temp = TempWorkspace::new("metadata-version-refused").expect("temp workspace");
    let db_path = temp.root().join(".state").join("local.sqlite3");
    fs::create_dir_all(db_path.parent().expect("db parent")).expect("state dir");
    let connection = Connection::open(&db_path).expect("old db");
    connection
        .execute_batch(
            "PRAGMA user_version = 1;
             CREATE TABLE projects (
               id TEXT PRIMARY KEY,
               path TEXT NOT NULL
             );
             INSERT INTO projects (id, path) VALUES ('old-project', 'old');",
        )
        .expect("old schema version");
    drop(connection);

    let error = MetadataStore::open(&db_path).expect_err("old version store is refused");
    assert!(matches!(error, MetadataError::UnsupportedSchema));
    assert_eq!(
        MetadataStore::inspect(&db_path).state,
        DatabaseState::UnsupportedSchema
    );
    let connection = Connection::open(&db_path).expect("inspect db");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("schema version"),
        1
    );
}

#[test]
fn unversioned_existing_schema_is_refused_without_stamping_current() {
    let temp = TempWorkspace::new("metadata-unversioned-refused").expect("temp workspace");
    let db_path = temp.root().join(".state").join("local.sqlite3");
    fs::create_dir_all(db_path.parent().expect("db parent")).expect("state dir");
    let connection = Connection::open(&db_path).expect("unversioned db");
    connection
        .execute_batch(
            "CREATE TABLE packs (
                id TEXT PRIMARY KEY,
                byte_len INTEGER NOT NULL
            );",
        )
        .expect("unversioned schema");
    drop(connection);

    let error = MetadataStore::open(&db_path).expect_err("unversioned store is refused");
    assert!(matches!(error, MetadataError::UnsupportedSchema));
    assert_eq!(
        MetadataStore::inspect(&db_path).state,
        DatabaseState::UnsupportedSchema
    );
    let connection = Connection::open(&db_path).expect("inspect db");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .expect("schema version"),
        0
    );
}

#[test]
fn future_versioned_schema_is_refused_without_stamping_current() {
    let temp = TempWorkspace::new("metadata-future-refused").expect("temp workspace");
    let db_path = temp.root().join(".state").join("local.sqlite3");
    fs::create_dir_all(db_path.parent().expect("db parent")).expect("state dir");
    let connection = Connection::open(&db_path).expect("future db");
    let future_version = CURRENT_SCHEMA_VERSION + 1;
    connection
        .pragma_update(None, "user_version", future_version)
        .expect("future schema version");
    drop(connection);

    let error = MetadataStore::open(&db_path).expect_err("future store is refused");
    assert!(matches!(
        error,
        MetadataError::FutureIncompatible {
            found,
            supported
        } if found == future_version && supported == CURRENT_SCHEMA_VERSION
    ));
    assert_eq!(
        MetadataStore::inspect(&db_path).state,
        DatabaseState::FutureIncompatible {
            found: future_version,
            supported: CURRENT_SCHEMA_VERSION,
        }
    );
}

#[test]
fn phase8_env_records_and_setup_receipts_round_trip_without_plaintext_values() {
    let temp = TempWorkspace::new("metadata-phase8").expect("temp workspace");
    let db_path = temp.root().join(".state").join("local.sqlite3");
    let mut store = MetadataStore::open(&db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_phase8");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_phase8",
            &workspace_id,
            &temp.root().to_string_lossy(),
            "2026-06-25T00:00:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_phase8",
            "apps/web",
            "2026-06-25T00:00:00Z",
        )
        .expect("project");

    let records = [
        EnvRecord {
            id: EnvRecordId::new("env_api_url_env"),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            source_path: "apps/web/.env".to_string(),
            profile: "default".to_string(),
            key_name: "API_URL".to_string(),
            occurrence_index: 0,
            line_kind: "key-value".to_string(),
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            encrypted_locator_json: "{\"contentId\":\"cid_env_1\",\"storage\":\"packed\"}"
                .to_string(),
            format_json: "{\"quote\":\"none\"}".to_string(),
            materialization_state: "materialized".to_string(),
            restriction_state: "unrestricted".to_string(),
            key_epoch: 1,
            metadata_json: "{\"redacted\":true}".to_string(),
            updated_at: "2026-06-25T00:00:01Z".to_string(),
        },
        EnvRecord {
            id: EnvRecordId::new("env_api_url_local"),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            source_path: "apps/web/.env.local".to_string(),
            profile: "local".to_string(),
            key_name: "API_URL".to_string(),
            occurrence_index: 0,
            line_kind: "key-value".to_string(),
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            encrypted_locator_json: "{\"contentId\":\"cid_env_2\",\"storage\":\"packed\"}"
                .to_string(),
            format_json: "{\"quote\":\"double\"}".to_string(),
            materialization_state: "pending".to_string(),
            restriction_state: "unrestricted".to_string(),
            key_epoch: 1,
            metadata_json: "{\"redacted\":true}".to_string(),
            updated_at: "2026-06-25T00:00:01Z".to_string(),
        },
    ];
    store
        .replace_env_records_for_source(&workspace_id, "apps/web/.env", &records[0..1])
        .expect("replace env");
    store
        .upsert_env_record(&records[1])
        .expect("upsert second env");

    let stored = store.env_records(&workspace_id).expect("env records");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].key_name, "API_URL");
    assert_ne!(stored[0].source_path, stored[1].source_path);
    let env_rows = format!("{stored:?}");
    assert!(!env_rows.contains("super-secret"));

    store
        .upsert_setup_receipt(&SetupReceiptRecord {
            id: "receipt_web_setup".to_string(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id),
            command: "pnpm install --ignore-scripts".to_string(),
            state: "completed".to_string(),
            recipe_hash: "blake3:recipe".to_string(),
            approval_state: "approved".to_string(),
            trigger: "setup".to_string(),
            cwd: "apps/web".to_string(),
            os: "macos".to_string(),
            arch: "arm64".to_string(),
            env_profile: "default".to_string(),
            output_path: Some(".bowline/logs/setup.log".to_string()),
            redacted_summary: "installed dependencies with [redacted]".to_string(),
            receipt_json: "{\"command\":\"pnpm install --ignore-scripts\"}".to_string(),
            updated_at: "2026-06-25T00:00:02Z".to_string(),
        })
        .expect("receipt");

    let receipts = store.setup_receipts(&workspace_id).expect("setup receipts");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].state, "completed");
    assert!(receipts[0].redacted_summary.contains("[redacted]"));
}
