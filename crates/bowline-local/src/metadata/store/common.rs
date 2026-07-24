use super::*;

use crate::metadata::sqlite::current_schema_version;

// Two seconds absorbs short commits without hiding a writer that is holding the database too long.
const CONNECTION_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);

pub(super) fn configure_connection(connection: &Connection) -> Result<(), MetadataError> {
    // Install contention policy before any pragma that may need SQLite's schema lock.
    connection.busy_timeout(CONNECTION_BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

pub(super) fn configure_read_only_connection(
    connection: &Connection,
    _role: MetadataReadRole,
) -> Result<(), MetadataError> {
    connection.busy_timeout(CONNECTION_BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    // The open flags enforce the filesystem capability; query_only also prevents a future
    // read-oriented caller from accidentally relying on a writable in-memory attachment.
    connection.pragma_update(None, "query_only", "ON")?;
    Ok(())
}

pub(super) fn initialize_schema(connection: &mut Connection) -> Result<(), MetadataError> {
    let existing_version = current_schema_version(connection)?;
    if existing_version > CURRENT_SCHEMA_VERSION {
        return Err(MetadataError::FutureIncompatible {
            found: existing_version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    if existing_version != 0 && existing_version != CURRENT_SCHEMA_VERSION {
        return Err(MetadataError::UnsupportedSchema);
    }
    if existing_version == CURRENT_SCHEMA_VERSION {
        return if user_schema_matches_current(connection)? {
            Ok(())
        } else {
            Err(MetadataError::UnsupportedSchema)
        };
    }
    if existing_version == 0 && user_schema_has_tables(connection)? {
        return Err(MetadataError::UnsupportedSchema);
    }
    if existing_version == 0 {
        // Journal mode is database state, so establish it once with the new schema instead of
        // turning every future handle open into a competing write-like operation.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        apply_current_schema_batches(connection)?;
        connection.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
        return Ok(());
    }

    Err(MetadataError::UnsupportedSchema)
}

fn apply_current_schema_batches(connection: &Connection) -> Result<(), MetadataError> {
    for batch in CURRENT_SCHEMA_BATCHES {
        connection.execute_batch(batch)?;
    }
    Ok(())
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

fn user_schema_matches_current(connection: &Connection) -> Result<bool, MetadataError> {
    // Greenfield metadata has one exact schema. Extra or altered objects are
    // drift, not a compatibility surface.
    Ok(
        schema_tables(connection)? == TABLES.iter().copied().map(String::from).collect()
            && schema_objects(connection)? == canonical_current_schema_objects()?,
    )
}

type SchemaObject = (String, String, String, String);

fn canonical_current_schema_objects()
-> Result<std::collections::BTreeSet<SchemaObject>, MetadataError> {
    let connection = Connection::open_in_memory()?;
    apply_current_schema_batches(&connection)?;
    schema_objects(&connection)
}

fn schema_objects(
    connection: &Connection,
) -> Result<std::collections::BTreeSet<SchemaObject>, MetadataError> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE type IN ('table', 'index', 'trigger', 'view') AND sql IS NOT NULL
         ORDER BY type, name",
    )?;
    let objects = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                normalize_schema_sql(&row.get::<_, String>(3)?),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(objects.into_iter().collect())
}

fn normalize_schema_sql(sql: &str) -> String {
    let mut normalized = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    let mut in_literal = false;
    while let Some(character) = characters.next() {
        if character == '\'' {
            normalized.push(character);
            if in_literal && characters.peek() == Some(&'\'') {
                normalized.push('\'');
                characters.next();
            } else {
                in_literal = !in_literal;
            }
        } else if !in_literal && character.is_ascii_whitespace() {
            continue;
        } else if in_literal {
            normalized.push(character);
        } else {
            normalized.push(character.to_ascii_lowercase());
        }
    }
    for prefix in ["createtable", "createindex", "createtrigger"] {
        let with_guard = format!("{prefix}ifnotexists");
        if normalized.starts_with(&with_guard) {
            return normalized.replacen(&with_guard, prefix, 1);
        }
    }
    normalized
}

fn schema_tables(
    connection: &Connection,
) -> Result<std::collections::BTreeSet<String>, MetadataError> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()
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
        Ok(_) => match user_schema_matches_current(connection) {
            Ok(true) => DatabaseState::Current,
            Ok(false) => DatabaseState::UnsupportedSchema,
            Err(MetadataError::Sqlite(error)) => classify_open_error(&error),
            Err(_) => DatabaseState::Corrupt,
        },
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

pub(super) fn parse_git_observer_state(value: String) -> Result<GitObserverState, rusqlite::Error> {
    GitObserverState::from_wire(&value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown git observer state `{value}`").into(),
        )
    })
}

pub(super) fn parse_project_lifecycle_state(
    value: String,
) -> Result<ProjectLifecycleState, rusqlite::Error> {
    ProjectLifecycleState::from_wire(&value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown project lifecycle state `{value}`").into(),
        )
    })
}

pub(super) fn parse_project_local_materialization_state(
    value: String,
) -> Result<ProjectLocalMaterializationState, rusqlite::Error> {
    ProjectLocalMaterializationState::from_wire(&value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown project local materialization state `{value}`").into(),
        )
    })
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

pub(super) fn env_record_from_row(row: &rusqlite::Row<'_>) -> Result<EnvRecord, rusqlite::Error> {
    let occurrence_index = row.get::<_, i64>(9)?;
    let key_epoch = row.get::<_, i64>(15)?;
    Ok(EnvRecord {
        id: EnvRecordId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        source_path: row.get(3)?,
        key_name: row.get(4)?,
        access: deserialize_access_flags(row.get::<_, String>(5)?)?,
        value_ciphertext_ref: row.get(6)?,
        updated_at: row.get(7)?,
        profile: row.get(8)?,
        occurrence_index: u32::try_from(occurrence_index).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        line_kind: row.get(10)?,
        encrypted_locator_json: row.get(11)?,
        format_json: row.get(12)?,
        materialization_state: row.get(13)?,
        restriction_state: row.get(14)?,
        key_epoch: u32::try_from(key_epoch).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                15,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        metadata_json: row.get(16)?,
    })
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
        setup_identity_hash: row.get(16)?,
        readiness_state: row.get(17)?,
        readiness_reason: row.get(18)?,
        readiness_remedy: row.get(19)?,
    })
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

pub(super) fn deserialize_json_variant<T>(value: String) -> Result<T, rusqlite::Error>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::Value::String(value)).map_err(json_to_sql_error)
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
