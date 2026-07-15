use super::*;

impl MetadataStore {
    pub(crate) fn database_path(&self) -> Result<PathBuf, MetadataError> {
        let path = self.connection.query_row(
            "SELECT file FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get::<_, String>(0),
        )?;
        if path.is_empty() {
            return Err(MetadataError::InvalidStorageMetadata(
                "metadata database has no filesystem path".into(),
            ));
        }
        Ok(PathBuf::from(path))
    }

    pub(crate) fn content_cache_root(&self) -> Result<PathBuf, MetadataError> {
        self.database_path()?
            .parent()
            .map(|root| root.join("cache"))
            .ok_or_else(|| {
                MetadataError::InvalidStorageMetadata(
                    "metadata database path has no state root".into(),
                )
            })
    }
}
use crate::metadata::sqlite::current_schema_version;

pub(super) fn configure_connection(connection: &Connection) -> Result<(), MetadataError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.busy_timeout(std::time::Duration::from_millis(2000))?;
    Ok(())
}

pub(super) fn random_hex_token(context: &str) -> Result<String, MetadataError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        MetadataError::Io(io::Error::other(format!(
            "{context} token generation failed: {error}"
        )))
    })?;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
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
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let actual = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected = TABLES.to_vec();
    expected.sort_unstable();
    Ok(actual == expected)
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

pub(super) fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub(super) fn sql_limit(limit: Option<u64>) -> i64 {
    i64::try_from(limit.unwrap_or(i64::MAX as u64)).unwrap_or(i64::MAX)
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
    if matches!(kind, "source-pack" | "overlay-pack" | "agent-overlay") {
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
        ContentStorage::Inline => {
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

pub(super) fn snapshot_record_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SnapshotRecord, rusqlite::Error> {
    Ok(SnapshotRecord {
        id: SnapshotId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        kind: deserialize_json_variant::<SnapshotKind>(row.get::<_, String>(3)?)?,
        base_snapshot_id: row.get::<_, Option<String>>(4)?.map(SnapshotId::new),
        root_id: NamespacePageId::new(row.get::<_, String>(5)?),
        semantic_manifest_digest: ManifestDigest::new(row.get::<_, String>(6)?),
        entry_count: row.get(7)?,
        refs: serde_json::from_str(&row.get::<_, String>(8)?).map_err(json_to_sql_read_error)?,
        created_at: row.get(9)?,
    })
}

pub(super) fn sync_operation_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SyncOperationRecord, rusqlite::Error> {
    let workspace_id = WorkspaceId::new(row.get::<_, String>(1)?);
    let kind = deserialize_json_variant::<SyncOperationKind>(row.get::<_, String>(2)?)?;
    let resource_key =
        SyncResourceKey::from_stored(kind, &workspace_id, row.get(3)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(SyncOperationRecord {
        id: row.get(0)?,
        workspace_id,
        kind,
        resource_key,
        state: deserialize_json_variant::<SyncOperationState>(row.get::<_, String>(4)?)?,
        idempotency_key: row.get(5)?,
        base_version: row.get(6)?,
        base_snapshot_id: row.get(7)?,
        target_snapshot_id: row.get(8)?,
        device_id: row.get::<_, Option<String>>(9)?.map(DeviceId::new),
        payload_json: row.get(10)?,
        attempt_count: row.get(11)?,
        claimed_by: row.get(12)?,
        claim_generation: row.get(13)?,
        heartbeat_at: row.get(14)?,
        lease_expires_at: row.get(15)?,
        cancellation_requested_at: row.get(16)?,
        next_attempt_at: row.get(17)?,
        result_json: row.get(18)?,
        last_error_code: row.get(19)?,
        last_error: row.get(20)?,
        created_at: row.get(21)?,
        updated_at: row.get(22)?,
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
