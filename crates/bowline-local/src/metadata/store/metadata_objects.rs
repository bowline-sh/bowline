use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetadataLogicalId(String);

impl MetadataLogicalId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetadataObjectKey(String);

impl MetadataObjectKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetadataRecordKind {
    SnapshotRoot,
    NamespacePage,
    ContentLayout,
    SegmentPage,
}

impl MetadataRecordKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotRoot => "snapshot-root",
            Self::NamespacePage => "namespace-page",
            Self::ContentLayout => "content-layout",
            Self::SegmentPage => "segment-page",
        }
    }

    pub(super) fn from_str(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "snapshot-root" => Ok(Self::SnapshotRoot),
            "namespace-page" => Ok(Self::NamespacePage),
            "content-layout" => Ok(Self::ContentLayout),
            "segment-page" => Ok(Self::SegmentPage),
            _ => Err(invalid_column(2, "unsupported metadata record kind")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataVerificationState {
    Unverified,
    Verified,
    Rejected,
}

impl MetadataVerificationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
        }
    }

    fn from_str(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "unverified" => Ok(Self::Unverified),
            "verified" => Ok(Self::Verified),
            "rejected" => Ok(Self::Rejected),
            _ => Err(invalid_column(7, "unsupported metadata verification state")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataObjectBindingRecord {
    pub workspace_id: WorkspaceId,
    pub logical_id: MetadataLogicalId,
    pub kind: MetadataRecordKind,
    pub object_key: MetadataObjectKey,
    pub byte_len: u64,
    pub object_hash: String,
    pub key_epoch: u32,
    pub verification_state: MetadataVerificationState,
    pub created_at: String,
    pub verified_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MetadataRecordRef {
    pub kind: MetadataRecordKind,
    pub logical_id: MetadataLogicalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataCacheState {
    Absent,
    Present,
    Deleting,
    Corrupt,
}

impl MetadataCacheState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Present => "present",
            Self::Deleting => "deleting",
            Self::Corrupt => "corrupt",
        }
    }

    fn from_str(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "absent" => Ok(Self::Absent),
            "present" => Ok(Self::Present),
            "deleting" => Ok(Self::Deleting),
            "corrupt" => Ok(Self::Corrupt),
            _ => Err(invalid_column(4, "unsupported metadata cache state")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataCacheRecord {
    pub workspace_id: WorkspaceId,
    pub logical_id: MetadataLogicalId,
    pub kind: MetadataRecordKind,
    pub cache_path: Option<String>,
    pub encoded_bytes: u64,
    pub state: MetadataCacheState,
    pub last_accessed_at: String,
}

impl MetadataStore {
    pub fn register_metadata_identity_key(
        &mut self,
        workspace_id: &WorkspaceId,
        key: [u8; 32],
        created_at: &str,
    ) -> Result<(), MetadataError> {
        let encoded = encode_identity_key(key);
        self.with_committed(|store| {
            let existing = store
                .connection
                .query_row(
                    "SELECT key_hex FROM metadata_identity_contexts WHERE workspace_id = ?1",
                    [workspace_id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if existing
                .as_deref()
                .is_some_and(|current| current != encoded)
            {
                return Err(MetadataError::ImmutableBindingConflict {
                    logical_id: workspace_id.as_str().to_string(),
                    field: "metadata_identity_key",
                });
            }
            store.connection.execute(
                "INSERT OR IGNORE INTO metadata_identity_contexts
                 (workspace_id, key_hex, created_at) VALUES (?1, ?2, ?3)",
                params![workspace_id.as_str(), encoded, created_at],
            )?;
            Ok(())
        })
    }

    pub fn metadata_identity_key(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<[u8; 32]>, MetadataError> {
        self.connection
            .query_row(
                "SELECT key_hex FROM metadata_identity_contexts WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|encoded| decode_identity_key(&encoded))
            .transpose()
    }

    pub fn register_metadata_record(
        &self,
        workspace_id: &WorkspaceId,
        record: &MetadataRecordRef,
        created_at: &str,
    ) -> Result<(), MetadataError> {
        if record.logical_id.as_str().is_empty() || created_at.is_empty() {
            return Err(MetadataError::InvalidStorageMetadata(
                "metadata record identity and creation time must be non-empty".to_string(),
            ));
        }
        self.connection.execute(
            "INSERT INTO metadata_records
             (workspace_id, logical_id, record_kind, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(workspace_id, record_kind, logical_id) DO NOTHING",
            params![
                workspace_id.as_str(),
                record.logical_id.as_str(),
                record.kind.as_str(),
                created_at,
            ],
        )?;
        Ok(())
    }

    pub fn insert_metadata_object_binding(
        &self,
        record: &MetadataObjectBindingRecord,
    ) -> Result<MetadataObjectBindingRecord, MetadataError> {
        validate_binding(record)?;
        if let Some(existing) =
            self.metadata_object_binding(&record.workspace_id, record.kind, &record.logical_id)?
        {
            ensure_same_binding(&existing, record)?;
            return Ok(existing);
        }
        let object_key_owner = self
            .connection
            .query_row(
                "SELECT logical_id FROM metadata_object_bindings
                 WHERE workspace_id = ?1 AND object_key = ?2",
                params![record.workspace_id.as_str(), record.object_key.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if object_key_owner.is_some() {
            return Err(MetadataError::ImmutableBindingConflict {
                logical_id: record.logical_id.as_str().to_string(),
                field: "object_key",
            });
        }
        self.register_metadata_record(
            &record.workspace_id,
            &MetadataRecordRef {
                kind: record.kind,
                logical_id: record.logical_id.clone(),
            },
            &record.created_at,
        )?;
        self.connection.execute(
            "INSERT INTO metadata_object_bindings
             (workspace_id, logical_id, record_kind, object_key, byte_len, object_hash,
              key_epoch, verification_state, created_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.workspace_id.as_str(),
                record.logical_id.as_str(),
                record.kind.as_str(),
                record.object_key.as_str(),
                record.byte_len,
                record.object_hash,
                record.key_epoch,
                record.verification_state.as_str(),
                record.created_at,
                record.verified_at,
            ],
        )?;
        Ok(record.clone())
    }

    pub fn metadata_object_binding(
        &self,
        workspace_id: &WorkspaceId,
        kind: MetadataRecordKind,
        logical_id: &MetadataLogicalId,
    ) -> Result<Option<MetadataObjectBindingRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, logical_id, record_kind, object_key, byte_len,
                        object_hash, key_epoch, verification_state, created_at, verified_at
                 FROM metadata_object_bindings
                 WHERE workspace_id = ?1 AND record_kind = ?2 AND logical_id = ?3",
                params![workspace_id.as_str(), kind.as_str(), logical_id.as_str()],
                binding_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_metadata_binding_verification(
        &self,
        workspace_id: &WorkspaceId,
        record: &MetadataRecordRef,
        state: MetadataVerificationState,
        verified_at: Option<&str>,
    ) -> Result<(), MetadataError> {
        if (state == MetadataVerificationState::Verified) != verified_at.is_some() {
            return Err(MetadataError::InvalidStorageMetadata(
                "verified metadata bindings require exactly one verification timestamp".to_string(),
            ));
        }
        let changed = self.connection.execute(
            "UPDATE metadata_object_bindings
             SET verification_state = ?4, verified_at = ?5
             WHERE workspace_id = ?1 AND record_kind = ?2 AND logical_id = ?3",
            params![
                workspace_id.as_str(),
                record.kind.as_str(),
                record.logical_id.as_str(),
                state.as_str(),
                verified_at,
            ],
        )?;
        if changed == 0 {
            return Err(MetadataError::InvalidStorageMetadata(
                "metadata binding was not found".to_string(),
            ));
        }
        Ok(())
    }

    pub fn replace_metadata_record_edges(
        &mut self,
        workspace_id: &WorkspaceId,
        parent: &MetadataRecordRef,
        children: &[MetadataRecordRef],
    ) -> Result<(), MetadataError> {
        if !metadata_record_exists(self, workspace_id, parent)? {
            return Err(MetadataError::InvalidStorageMetadata(
                "metadata edge parent record was not found".to_string(),
            ));
        }
        for child in children {
            if !metadata_record_exists(self, workspace_id, child)? {
                return Err(MetadataError::InvalidStorageMetadata(
                    "metadata edge child record was not found".to_string(),
                ));
            }
        }
        self.with_committed(|store| {
            store.connection.execute(
                "DELETE FROM metadata_record_edges
                 WHERE workspace_id = ?1 AND parent_kind = ?2 AND parent_logical_id = ?3",
                params![
                    workspace_id.as_str(),
                    parent.kind.as_str(),
                    parent.logical_id.as_str(),
                ],
            )?;
            for child in children {
                store.connection.execute(
                    "INSERT INTO metadata_record_edges
                     (workspace_id, parent_kind, parent_logical_id, child_kind, child_logical_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        workspace_id.as_str(),
                        parent.kind.as_str(),
                        parent.logical_id.as_str(),
                        child.kind.as_str(),
                        child.logical_id.as_str(),
                    ],
                )?;
            }
            Ok::<(), MetadataError>(())
        })
    }

    pub fn metadata_record_children(
        &self,
        workspace_id: &WorkspaceId,
        parent: &MetadataRecordRef,
    ) -> Result<Vec<MetadataRecordRef>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT child_kind, child_logical_id
             FROM metadata_record_edges
             WHERE workspace_id = ?1 AND parent_kind = ?2 AND parent_logical_id = ?3
             ORDER BY child_kind, child_logical_id",
        )?;
        let rows = statement.query_map(
            params![
                workspace_id.as_str(),
                parent.kind.as_str(),
                parent.logical_id.as_str(),
            ],
            |row| {
                Ok(MetadataRecordRef {
                    kind: MetadataRecordKind::from_str(&row.get::<_, String>(0)?)?,
                    logical_id: MetadataLogicalId::new(row.get::<_, String>(1)?),
                })
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn put_metadata_cache_record(
        &self,
        record: &MetadataCacheRecord,
    ) -> Result<(), MetadataError> {
        validate_cache_record(record)?;
        self.register_metadata_record(
            &record.workspace_id,
            &MetadataRecordRef {
                kind: record.kind,
                logical_id: record.logical_id.clone(),
            },
            &record.last_accessed_at,
        )?;
        let table = cache_table(record.kind)?;
        let sql = format!(
            "INSERT INTO {table}
             (workspace_id, logical_id, record_kind, cache_path, encoded_bytes,
              cache_state, last_accessed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(workspace_id, logical_id) DO UPDATE SET
               cache_path = excluded.cache_path,
               encoded_bytes = excluded.encoded_bytes,
               cache_state = excluded.cache_state,
               last_accessed_at = excluded.last_accessed_at"
        );
        self.connection.execute(
            &sql,
            params![
                record.workspace_id.as_str(),
                record.logical_id.as_str(),
                record.kind.as_str(),
                record.cache_path,
                record.encoded_bytes,
                record.state.as_str(),
                record.last_accessed_at,
            ],
        )?;
        Ok(())
    }

    pub fn metadata_cache_record(
        &self,
        workspace_id: &WorkspaceId,
        record: &MetadataRecordRef,
    ) -> Result<Option<MetadataCacheRecord>, MetadataError> {
        let table = cache_table(record.kind)?;
        let sql = format!(
            "SELECT workspace_id, logical_id, record_kind, cache_path, encoded_bytes,
                    cache_state, last_accessed_at
             FROM {table}
             WHERE workspace_id = ?1 AND logical_id = ?2"
        );
        self.connection
            .query_row(
                &sql,
                params![workspace_id.as_str(), record.logical_id.as_str()],
                cache_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

fn encode_identity_key(key: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in key {
        encoded.push(char::from(HEX[(byte >> 4) as usize]));
        encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    encoded
}

fn decode_identity_key(encoded: &str) -> Result<[u8; 32], MetadataError> {
    if encoded.len() != 64 {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata identity key has an invalid encoded length".to_string(),
        ));
    }
    let mut key = [0_u8; 32];
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        key[index] = (decode_hex_nibble(pair[0])? << 4) | decode_hex_nibble(pair[1])?;
    }
    Ok(key)
}

fn decode_hex_nibble(value: u8) -> Result<u8, MetadataError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(MetadataError::InvalidStorageMetadata(
            "metadata identity key is not lowercase hexadecimal".to_string(),
        )),
    }
}

fn metadata_record_exists(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
) -> Result<bool, MetadataError> {
    store
        .connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM metadata_records
               WHERE workspace_id = ?1 AND record_kind = ?2 AND logical_id = ?3
             )",
            params![
                workspace_id.as_str(),
                record.kind.as_str(),
                record.logical_id.as_str(),
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn binding_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<MetadataObjectBindingRecord, rusqlite::Error> {
    Ok(MetadataObjectBindingRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        logical_id: MetadataLogicalId::new(row.get::<_, String>(1)?),
        kind: MetadataRecordKind::from_str(&row.get::<_, String>(2)?)?,
        object_key: MetadataObjectKey::new(row.get::<_, String>(3)?),
        byte_len: row.get(4)?,
        object_hash: row.get(5)?,
        key_epoch: row.get(6)?,
        verification_state: MetadataVerificationState::from_str(&row.get::<_, String>(7)?)?,
        created_at: row.get(8)?,
        verified_at: row.get(9)?,
    })
}

fn cache_from_row(row: &rusqlite::Row<'_>) -> Result<MetadataCacheRecord, rusqlite::Error> {
    Ok(MetadataCacheRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        logical_id: MetadataLogicalId::new(row.get::<_, String>(1)?),
        kind: MetadataRecordKind::from_str(&row.get::<_, String>(2)?)?,
        cache_path: row.get(3)?,
        encoded_bytes: row.get(4)?,
        state: MetadataCacheState::from_str(&row.get::<_, String>(5)?)?,
        last_accessed_at: row.get(6)?,
    })
}

fn validate_binding(record: &MetadataObjectBindingRecord) -> Result<(), MetadataError> {
    if record.logical_id.as_str().is_empty()
        || record.object_key.as_str().is_empty()
        || record.object_hash.is_empty()
        || record.key_epoch == 0
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata binding fields must be non-empty and key epoch must be non-zero".to_string(),
        ));
    }
    if (record.verification_state == MetadataVerificationState::Verified)
        != record.verified_at.is_some()
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "verified metadata bindings require exactly one verification timestamp".to_string(),
        ));
    }
    Ok(())
}

fn ensure_same_binding(
    existing: &MetadataObjectBindingRecord,
    requested: &MetadataObjectBindingRecord,
) -> Result<(), MetadataError> {
    for (same, field) in [
        (existing.object_key == requested.object_key, "object_key"),
        (existing.byte_len == requested.byte_len, "byte_len"),
        (existing.object_hash == requested.object_hash, "object_hash"),
        (existing.key_epoch == requested.key_epoch, "key_epoch"),
    ] {
        if !same {
            return Err(MetadataError::ImmutableBindingConflict {
                logical_id: requested.logical_id.as_str().to_string(),
                field,
            });
        }
    }
    Ok(())
}

fn validate_cache_record(record: &MetadataCacheRecord) -> Result<(), MetadataError> {
    if record.kind == MetadataRecordKind::SnapshotRoot {
        return Err(MetadataError::InvalidStorageMetadata(
            "snapshot roots do not use page-cache tables".to_string(),
        ));
    }
    if matches!(
        record.state,
        MetadataCacheState::Present | MetadataCacheState::Deleting
    ) != record.cache_path.is_some()
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "present or deleting cache records require a path and other states forbid one"
                .to_string(),
        ));
    }
    Ok(())
}

pub(super) fn cache_table(kind: MetadataRecordKind) -> Result<&'static str, MetadataError> {
    match kind {
        MetadataRecordKind::NamespacePage => Ok("namespace_pages"),
        MetadataRecordKind::ContentLayout => Ok("content_layouts"),
        MetadataRecordKind::SegmentPage => Ok("segment_pages"),
        MetadataRecordKind::SnapshotRoot => Err(MetadataError::InvalidStorageMetadata(
            "snapshot roots do not use page-cache tables".to_string(),
        )),
    }
}

fn invalid_column(index: usize, reason: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(io::Error::new(io::ErrorKind::InvalidData, reason)),
    )
}
