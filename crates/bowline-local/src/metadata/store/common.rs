use super::*;

pub(super) fn configure_connection(connection: &Connection) -> Result<(), MetadataError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

pub(super) fn initialize_schema(connection: &Connection) -> Result<(), MetadataError> {
    let existing_version = current_schema_version(connection)?;
    if existing_version > CURRENT_SCHEMA_VERSION {
        return Err(MetadataError::FutureIncompatible {
            found: existing_version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    if existing_version == 0 && user_schema_has_tables(connection)? {
        return Err(MetadataError::UnsupportedSchema);
    }
    if existing_version > 0 && existing_version < CURRENT_SCHEMA_VERSION {
        return Err(MetadataError::UnsupportedSchema);
    }

    connection.execute_batch(SCHEMA_CORE)?;
    connection.execute_batch(SCHEMA_MATERIALIZATION)?;
    connection.execute_batch(SCHEMA_ENV_SETUP_INDEXES)?;
    connection.execute_batch(SCHEMA_WORK_VIEWS)?;
    connection.execute_batch(SCHEMA_INDEXING)?;
    connection.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    Ok(())
}

pub(super) fn current_schema_version(connection: &Connection) -> Result<u32, MetadataError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .map_err(Into::into)
}

pub(super) fn user_schema_has_tables(connection: &Connection) -> Result<bool, MetadataError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table'
                  AND name NOT LIKE 'sqlite_%'
            )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(Into::into)
}

pub(super) fn inspect_open_connection(connection: &Connection) -> DatabaseState {
    match current_schema_version(connection) {
        Ok(0) => match user_schema_has_tables(connection) {
            Ok(true) => DatabaseState::UnsupportedSchema,
            Ok(false) => DatabaseState::Empty,
            Err(MetadataError::Sqlite(error)) => classify_open_error(&error),
            Err(_) => DatabaseState::Corrupt,
        },
        Ok(version) if version > CURRENT_SCHEMA_VERSION => DatabaseState::FutureIncompatible {
            found: version,
            supported: CURRENT_SCHEMA_VERSION,
        },
        Ok(version) if version < CURRENT_SCHEMA_VERSION => DatabaseState::UnsupportedSchema,
        Ok(_) => DatabaseState::Current,
        Err(MetadataError::Sqlite(error)) => classify_open_error(&error),
        Err(_) => DatabaseState::Corrupt,
    }
}

pub(super) fn classify_open_error(error: &rusqlite::Error) -> DatabaseState {
    match error {
        rusqlite::Error::SqliteFailure(sqlite_error, _) => {
            use rusqlite::ffi;
            match sqlite_error.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    DatabaseState::Locked
                }
                rusqlite::ErrorCode::CannotOpen
                    if sqlite_error.extended_code == ffi::SQLITE_CANTOPEN =>
                {
                    DatabaseState::PermissionDenied
                }
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt => {
                    DatabaseState::Corrupt
                }
                _ => DatabaseState::Corrupt,
            }
        }
        _ => DatabaseState::Corrupt,
    }
}

pub(super) fn normalize_path_for_matching(path: &str) -> String {
    let mut normalized = expand_tilde_for_matching(path).replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    normalized.trim_end_matches('/').to_string()
}

pub(super) fn expand_tilde_for_matching(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_string();
    }
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_string();
    };

    format!("{}{}", home.to_string_lossy(), rest)
}

pub(super) fn upsert_env_record_tx(
    transaction: &Transaction<'_>,
    record: &EnvRecord,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO env_records
         (id, workspace_id, project_id, source_path, key_name, access,
          value_ciphertext_ref, updated_at, profile, occurrence_index, line_kind,
          encrypted_locator_json, format_json, materialization_state, restriction_state,
          key_epoch, metadata_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(id) DO UPDATE SET
           workspace_id = excluded.workspace_id,
           project_id = excluded.project_id,
           source_path = excluded.source_path,
           key_name = excluded.key_name,
           access = excluded.access,
           value_ciphertext_ref = excluded.value_ciphertext_ref,
           updated_at = excluded.updated_at,
           profile = excluded.profile,
           occurrence_index = excluded.occurrence_index,
           line_kind = excluded.line_kind,
           encrypted_locator_json = excluded.encrypted_locator_json,
           format_json = excluded.format_json,
           materialization_state = excluded.materialization_state,
           restriction_state = excluded.restriction_state,
           key_epoch = excluded.key_epoch,
           metadata_json = excluded.metadata_json",
        params![
            record.id.as_str(),
            record.workspace_id.as_str(),
            record.project_id.as_ref().map(|id| id.as_str()),
            record.source_path.as_str(),
            record.key_name.as_str(),
            serialize_access_flags(&record.access)?,
            record.updated_at.as_str(),
            record.profile.as_str(),
            record.occurrence_index,
            record.line_kind.as_str(),
            record.encrypted_locator_json.as_str(),
            record.format_json.as_str(),
            record.materialization_state.as_str(),
            record.restriction_state.as_str(),
            record.key_epoch,
            record.metadata_json.as_str(),
        ],
    )?;
    Ok(())
}

pub(super) fn env_record_from_row(row: &rusqlite::Row<'_>) -> Result<EnvRecord, rusqlite::Error> {
    let occurrence_index = row.get::<_, i64>(8)?;
    let key_epoch = row.get::<_, i64>(14)?;
    Ok(EnvRecord {
        id: EnvRecordId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        source_path: row.get(3)?,
        key_name: row.get(4)?,
        access: deserialize_access_flags(row.get::<_, String>(5)?)?,
        updated_at: row.get(6)?,
        profile: row.get(7)?,
        occurrence_index: u32::try_from(occurrence_index).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        line_kind: row.get(9)?,
        encrypted_locator_json: row.get(10)?,
        format_json: row.get(11)?,
        materialization_state: row.get(12)?,
        restriction_state: row.get(13)?,
        key_epoch: u32::try_from(key_epoch).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                14,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        metadata_json: row.get(15)?,
    })
}

pub(super) fn index_document_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<IndexDocumentRecord, rusqlite::Error> {
    Ok(IndexDocumentRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        project_id: row.get::<_, Option<String>>(1)?.map(ProjectId::new),
        path: row.get(2)?,
        snapshot_id: row.get::<_, Option<String>>(3)?.map(SnapshotId::new),
        content_id: row.get::<_, Option<String>>(4)?.map(ContentId::new),
        classification: deserialize_json_variant(row.get::<_, String>(5)?)?,
        mode: deserialize_json_variant(row.get::<_, String>(6)?)?,
        access: deserialize_access_flags(row.get::<_, String>(7)?)?,
        policy_summary: row.get(8)?,
        body_text: row.get(9)?,
        hydration_state: deserialize_json_variant(row.get::<_, String>(10)?)?,
        indexed_bytes: row.get(11)?,
        source_watermark: row.get(12)?,
        indexed_watermark: row.get(13)?,
        state: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

pub(super) fn symbol_index_record_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SymbolIndexRecord, rusqlite::Error> {
    Ok(SymbolIndexRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        path: row.get(3)?,
        snapshot_id: row.get::<_, Option<String>>(4)?.map(SnapshotId::new),
        name: row.get(5)?,
        kind: row.get(6)?,
        language: row.get(7)?,
        line_start: row.get(8)?,
        line_end: row.get(9)?,
        byte_start: row.get(10)?,
        byte_end: row.get(11)?,
        parser_status: row.get(12)?,
        access: deserialize_access_flags(row.get::<_, String>(13)?)?,
        updated_at: row.get(14)?,
    })
}

pub(super) fn index_work_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<IndexWorkRecord, rusqlite::Error> {
    Ok(IndexWorkRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        path: row.get(3)?,
        kind: row.get(4)?,
        source_watermark: row.get(5)?,
        indexed_watermark: row.get(6)?,
        state: row.get(7)?,
        reason: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

pub(super) fn index_pack_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<IndexPackRecord, rusqlite::Error> {
    let byte_len: i64 = row.get(4)?;
    Ok(IndexPackRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        project_id: row.get::<_, Option<String>>(1)?.map(ProjectId::new),
        snapshot_id: row.get::<_, Option<String>>(2)?.map(SnapshotId::new),
        object_key: row.get(3)?,
        byte_len: byte_len.try_into().unwrap_or(0),
        hash: row.get(5)?,
        state: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

pub(super) fn next_index_source_watermark(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> Result<u64, MetadataError> {
    let value: i64 = store.connection.query_row(
        "SELECT COALESCE(MAX(source_watermark), 0) + 1
         FROM index_work
         WHERE workspace_id = ?1 AND project_id = ?2",
        params![workspace_id.as_str(), project_id.as_str()],
        |row| row.get(0),
    )?;
    Ok(value.try_into().unwrap_or(1))
}

pub(super) fn project_index_initialized(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> Result<bool, MetadataError> {
    let count: i64 = store.connection.query_row(
        "SELECT
           (SELECT COUNT(*) FROM index_work WHERE workspace_id = ?1 AND project_id = ?2) +
           (SELECT COUNT(*) FROM index_documents WHERE workspace_id = ?1 AND project_id = ?2)",
        params![workspace_id.as_str(), project_id.as_str()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub(super) fn stable_store_token(value: &str) -> String {
    blake3::hash(value.as_bytes())
        .to_hex()
        .chars()
        .take(16)
        .collect()
}

pub(super) fn is_work_namespace_path(path: &str) -> bool {
    let normalized = normalize_workspace_path(path);
    normalized == ".work" || normalized.starts_with(".work/") || normalized.contains("/.work/")
}

pub(super) fn path_is_under_prefix(path: &str, prefix: &str) -> bool {
    let path = normalize_workspace_path(path);
    let prefix = normalize_workspace_path(prefix);
    !prefix.is_empty() && (path == prefix || path.starts_with(&format!("{prefix}/")))
}

pub(super) fn project_relative_index_path(
    workspace_relative_path: &str,
    project_path: &str,
) -> String {
    let path = normalize_workspace_path(workspace_relative_path);
    let project_path = normalize_workspace_path(project_path);
    if project_path.is_empty() {
        return path;
    }
    path.strip_prefix(&format!("{project_path}/"))
        .map(str::to_string)
        .unwrap_or(path)
}

pub(super) fn setup_receipt_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SetupReceiptRecord, rusqlite::Error> {
    Ok(SetupReceiptRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        command: row.get(3)?,
        state: row.get(4)?,
        receipt_json: row.get(5)?,
        updated_at: row.get(6)?,
        recipe_hash: row.get(7)?,
        approval_state: row.get(8)?,
        trigger: row.get(9)?,
        cwd: row.get(10)?,
        os: row.get(11)?,
        arch: row.get(12)?,
        env_profile: row.get(13)?,
        output_path: row.get(14)?,
        redacted_summary: row.get(15)?,
    })
}

pub(super) fn command_idempotency_record_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<CommandIdempotencyRecord, rusqlite::Error> {
    Ok(CommandIdempotencyRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        idempotency_key: row.get(1)?,
        command: row.get(2)?,
        request_hash: row.get(3)?,
        result_json: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

pub(super) fn agent_lease_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<AgentLeaseRecord, rusqlite::Error> {
    serde_json::from_str(&row.get::<_, String>(0)?).map_err(json_to_sql_read_error)
}

pub(super) fn work_view_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<WorkViewRecord, rusqlite::Error> {
    Ok(WorkViewRecord {
        id: WorkViewId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: ProjectId::new(row.get::<_, String>(2)?),
        project_path: row.get(3)?,
        name: row.get(4)?,
        visible_path: row.get(5)?,
        base_snapshot_id: SnapshotId::new(row.get::<_, String>(6)?),
        overlay_head: row.get(7)?,
        overlay_version: row.get(8)?,
        env_profile: row.get(9)?,
        lifecycle: deserialize_json_variant::<WorkViewLifecycle>(row.get::<_, String>(10)?)?,
        visibility: deserialize_json_variant::<WorkViewVisibility>(row.get::<_, String>(11)?)?,
        sync_state: deserialize_json_variant::<WorkViewSyncState>(row.get::<_, String>(12)?)?,
        retention: WorkViewRetention {
            state: deserialize_json_variant::<WorkViewRetentionState>(row.get::<_, String>(13)?)?,
            retain_until: row.get(14)?,
            restorable: row.get::<_, i64>(15)? != 0,
        },
        owner_device_id: row.get::<_, Option<String>>(16)?.map(DeviceId::new),
        followed_by: serde_json::from_str(&row.get::<_, String>(17)?)
            .map_err(json_to_sql_read_error)?,
        host_materializations: serde_json::from_str(&row.get::<_, String>(18)?)
            .map_err(json_to_sql_read_error)?,
        attention: serde_json::from_str(&row.get::<_, String>(19)?)
            .map_err(json_to_sql_read_error)?,
        created_at: row.get(20)?,
        updated_at: row.get(21)?,
    })
}

pub(super) fn serialize_access_flags(access: &[AccessFlag]) -> Result<String, rusqlite::Error> {
    serde_json::to_string(access).map_err(json_to_sql_error)
}

pub(super) fn deserialize_access_flags(value: String) -> Result<Vec<AccessFlag>, rusqlite::Error> {
    serde_json::from_str(&value).or_else(|_| {
        serde_json::from_str::<AccessFlag>(&value)
            .map(|flag| vec![flag])
            .map_err(json_to_sql_read_error)
    })
}

pub(super) fn strip_root_prefix<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if path == root {
        return Some("");
    }

    path.strip_prefix(&format!("{root}/"))
}

pub(super) fn scan_summary_id(workspace_id: &WorkspaceId) -> String {
    format!("scan-summary:{}", workspace_id.as_str())
}

pub(super) fn local_path_id(workspace_id: &WorkspaceId, path: &str) -> String {
    format!("{}:{path}", workspace_id.as_str())
}

pub(super) fn serialize_json_variant<T>(value: &T) -> Result<String, rusqlite::Error>
where
    T: serde::Serialize,
{
    serde_json::to_value(value)
        .map_err(json_to_sql_error)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("expected string enum".into()))
}

pub(super) fn validate_pack_kind(kind: &str) -> Result<(), MetadataError> {
    if matches!(
        kind,
        "source-pack"
            | "large-chunk"
            | "snapshot-manifest"
            | "locator-index"
            | "index-pack"
            | "overlay-pack"
            | "agent-overlay"
    ) {
        Ok(())
    } else {
        Err(MetadataError::InvalidStorageMetadata(format!(
            "unsupported pack kind `{kind}`"
        )))
    }
}

pub(super) fn validate_pack_state(state: &str) -> Result<(), MetadataError> {
    if matches!(
        state,
        "pending" | "current" | "orphan-candidate" | "retained" | "delete-eligible"
    ) {
        Ok(())
    } else {
        Err(MetadataError::InvalidStorageMetadata(format!(
            "unsupported pack state `{state}`"
        )))
    }
}

pub(super) fn validate_locator_shape(locator: &ContentLocator) -> Result<(), MetadataError> {
    match locator.storage {
        ContentStorage::Packed => {
            if locator.pack_id.is_some() && locator.offset.is_some() && locator.length.is_some() {
                Ok(())
            } else {
                Err(MetadataError::InvalidStorageMetadata(
                    "packed locators require pack_id, offset, and length".to_string(),
                ))
            }
        }
        ContentStorage::Inline | ContentStorage::Chunked => {
            if locator.pack_id.is_none() && locator.offset.is_none() && locator.length.is_none() {
                Ok(())
            } else {
                Err(MetadataError::InvalidStorageMetadata(
                    "non-packed locators must not carry pack ranges".to_string(),
                ))
            }
        }
    }
}

pub(super) fn ensure_pack_exists(
    connection: &Connection,
    workspace_id: &WorkspaceId,
    pack_id: &PackId,
) -> Result<(), MetadataError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM packs WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id.as_str(), pack_id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();

    if exists {
        Ok(())
    } else {
        Err(MetadataError::InvalidStorageMetadata(format!(
            "packed locator references missing pack `{}`",
            pack_id.as_str()
        )))
    }
}

pub(super) fn deserialize_json_variant<T>(value: String) -> Result<T, rusqlite::Error>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::Value::String(value)).map_err(json_to_sql_error)
}

pub(super) fn content_locator_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<ContentLocator, rusqlite::Error> {
    let scalar = ContentLocator {
        content_id: ContentId::new(row.get::<_, String>(1)?),
        storage: deserialize_json_variant::<ContentStorage>(row.get::<_, String>(2)?)?,
        raw_size: row.get(3)?,
        pack_id: row.get::<_, Option<String>>(4)?.map(PackId::new),
        offset: row.get(5)?,
        length: row.get(6)?,
        chunk_ids: Vec::new(),
    };
    let canonical: ContentLocator =
        serde_json::from_str(&row.get::<_, String>(7)?).map_err(json_to_sql_read_error)?;

    if canonical.content_id != scalar.content_id
        || canonical.storage != scalar.storage
        || canonical.raw_size != scalar.raw_size
        || canonical.pack_id != scalar.pack_id
        || canonical.offset != scalar.offset
        || canonical.length != scalar.length
    {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                "locator_json drifted from indexed locator columns",
            )),
        ));
    }

    Ok(canonical)
}

pub(super) fn projected_node_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<ProjectedNodeRecord, rusqlite::Error> {
    Ok(ProjectedNodeRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        node_id: row.get(1)?,
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        parent_node_id: row.get(3)?,
        path: row.get(4)?,
        kind: deserialize_json_variant(row.get::<_, String>(5)?)?,
        content_id: row.get::<_, Option<String>>(6)?.map(ContentId::new),
        hydration_state: deserialize_json_variant(row.get::<_, String>(7)?)?,
        updated_at: row.get(8)?,
    })
}

pub(super) fn sync_operation_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SyncOperationRecord, rusqlite::Error> {
    Ok(SyncOperationRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        kind: row.get(2)?,
        state: row.get(3)?,
        idempotency_key: row.get(4)?,
        base_version: row.get(5)?,
        base_snapshot_id: row.get(6)?,
        target_snapshot_id: row.get(7)?,
        device_id: row.get::<_, Option<String>>(8)?.map(DeviceId::new),
        payload_json: row.get(9)?,
        attempt_count: row.get(10)?,
        claimed_by: row.get(11)?,
        heartbeat_at: row.get(12)?,
        next_attempt_at: row.get(13)?,
        last_error: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

pub(super) fn sync_operation_checkpoint_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SyncOperationCheckpointRecord, rusqlite::Error> {
    Ok(SyncOperationCheckpointRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        operation_id: row.get(2)?,
        step: row.get(3)?,
        state: row.get(4)?,
        payload_json: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

pub(super) fn json_to_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

pub(super) fn json_to_sql_read_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

pub(super) fn project_path_candidates(path: &str) -> Vec<String> {
    let path = normalize_workspace_path(path);
    if path.is_empty() {
        return vec![String::new()];
    }

    let mut candidates = Vec::new();
    let mut parts = path.split('/').collect::<Vec<_>>();
    while !parts.is_empty() {
        candidates.push(parts.join("/"));
        parts.pop();
    }
    candidates
}
