use super::*;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PreparationLeaseId(String);

impl PreparationLeaseId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PreparationOwnerMarker(String);

impl PreparationOwnerMarker {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SourceFingerprint(String);

impl SourceFingerprint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedStagedPath(PathBuf);

impl OwnedStagedPath {
    pub fn new(value: impl Into<PathBuf>) -> Self {
        Self(value.into())
    }

    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreparationLeaseState {
    Preparing,
    Prepared,
    ReferencedByUpload,
    Committed,
    Abandoned,
}

impl PreparationLeaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Prepared => "prepared",
            Self::ReferencedByUpload => "referenced-by-upload",
            Self::Committed => "committed",
            Self::Abandoned => "abandoned",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "preparing" => Some(Self::Preparing),
            "prepared" => Some(Self::Prepared),
            "referenced-by-upload" => Some(Self::ReferencedByUpload),
            "committed" => Some(Self::Committed),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    fn permits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Preparing, Self::Prepared | Self::Abandoned)
                | (Self::Prepared, Self::ReferencedByUpload | Self::Abandoned)
                | (Self::ReferencedByUpload, Self::Committed | Self::Abandoned)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparationLeaseRecord {
    pub id: PreparationLeaseId,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub snapshot_candidate_id: SnapshotId,
    pub owner_marker: PreparationOwnerMarker,
    pub state: PreparationLeaseState,
    pub reservation_bytes: u64,
    pub prepared_at: Option<String>,
    pub referenced_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedStagedContentRecord {
    pub lease_id: PreparationLeaseId,
    pub content_id: ContentId,
    pub staged_path: OwnedStagedPath,
    pub logical_size: u64,
    pub source_fingerprint: SourceFingerprint,
    pub owner_marker: PreparationOwnerMarker,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparationOrphanRecord {
    pub content: PreparedStagedContentRecord,
    pub terminal_state: PreparationLeaseState,
}

const PREPARATION_LEASE_SELECT: &str =
    "SELECT id, workspace_id, project_id, snapshot_candidate_id, owner_marker, state,
            reservation_bytes, prepared_at, referenced_at, finished_at, created_at, updated_at
     FROM preparation_leases";

const STAGED_CONTENT_SELECT: &str =
    "SELECT lease_id, content_id, staged_path, logical_size, source_fingerprint,
            owner_marker, created_at, updated_at
     FROM prepared_staged_content";

impl MetadataStore {
    pub fn create_preparation_lease(
        &self,
        lease: &PreparationLeaseRecord,
    ) -> Result<bool, MetadataError> {
        if lease.state != PreparationLeaseState::Preparing {
            return Err(MetadataError::InvalidStorageMetadata(
                "a preparation lease must be created in the preparing state".to_string(),
            ));
        }
        if lease.prepared_at.is_some()
            || lease.referenced_at.is_some()
            || lease.finished_at.is_some()
        {
            return Err(MetadataError::InvalidStorageMetadata(
                "a new preparation lease cannot contain lifecycle completion timestamps"
                    .to_string(),
            ));
        }
        validate_owner_marker(&lease.owner_marker)?;
        let reservation_bytes = to_sql_u64(lease.reservation_bytes, "reservation bytes")?;
        self.in_immediate_transaction(|| {
            let changed = self.connection.execute(
                "INSERT OR IGNORE INTO preparation_leases (
                   id, workspace_id, project_id, snapshot_candidate_id, owner_marker, state,
                   reservation_bytes, prepared_at, referenced_at, finished_at, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL, ?8, ?9)",
                params![
                    lease.id.as_str(),
                    lease.workspace_id.as_str(),
                    lease.project_id.as_ref().map(ProjectId::as_str),
                    lease.snapshot_candidate_id.as_str(),
                    lease.owner_marker.as_str(),
                    lease.state.as_str(),
                    reservation_bytes,
                    lease.created_at,
                    lease.updated_at,
                ],
            )?;
            if changed == 1 {
                return Ok(true);
            }
            let existing = self.preparation_lease(&lease.id)?.ok_or_else(|| {
                MetadataError::InvalidStorageMetadata(
                    "preparation lease insert was ignored without an existing identity"
                        .to_string(),
                )
            })?;
            if existing != *lease {
                return Err(MetadataError::InvalidStorageMetadata(
                    "preparation lease identity collided with different durable metadata"
                        .to_string(),
                ));
            }
            Ok(false)
        })
    }

    pub fn preparation_lease(
        &self,
        id: &PreparationLeaseId,
    ) -> Result<Option<PreparationLeaseRecord>, MetadataError> {
        self.connection
            .query_row(
                &format!("{PREPARATION_LEASE_SELECT} WHERE id = ?1"),
                [id.as_str()],
                preparation_lease_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn preparation_leases(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<PreparationLeaseRecord>, MetadataError> {
        let sql =
            format!("{PREPARATION_LEASE_SELECT} WHERE workspace_id = ?1 ORDER BY created_at, id");
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map([workspace_id.as_str()], preparation_lease_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_prepared_staged_content(
        &self,
        content: &PreparedStagedContentRecord,
    ) -> Result<(), MetadataError> {
        validate_owner_marker(&content.owner_marker)?;
        if content.source_fingerprint.as_str().is_empty() {
            return Err(MetadataError::InvalidStorageMetadata(
                "prepared source fingerprint must not be empty".to_string(),
            ));
        }
        if !content.staged_path.as_path().is_absolute() {
            return Err(MetadataError::InvalidStorageMetadata(
                "owned staged paths must be absolute".to_string(),
            ));
        }
        let staged_path = staged_path_to_text(&content.staged_path)?;
        let logical_size = to_sql_u64(content.logical_size, "logical content size")?;
        self.in_immediate_transaction(|| {
            let (lease_owner, lease_state, reservation_bytes) = self.connection.query_row(
                "SELECT owner_marker, state, reservation_bytes
                 FROM preparation_leases WHERE id = ?1",
                [content.lease_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?;
            if lease_owner != content.owner_marker.as_str()
                || lease_state != PreparationLeaseState::Preparing.as_str()
            {
                return Err(MetadataError::InvalidStorageMetadata(
                    "staged content crossed its preparation owner or lifecycle fence".to_string(),
                ));
            }
            let other_bytes = self.connection.query_row(
                "SELECT COALESCE(SUM(logical_size), 0)
                 FROM prepared_staged_content
                 WHERE lease_id = ?1 AND content_id != ?2",
                params![content.lease_id.as_str(), content.content_id.as_str()],
                |row| row.get::<_, i64>(0),
            )?;
            let required_bytes = other_bytes.checked_add(logical_size).ok_or_else(|| {
                MetadataError::InvalidStorageMetadata(
                    "prepared content reservation arithmetic overflowed".to_string(),
                )
            })?;
            if required_bytes > reservation_bytes {
                return Err(MetadataError::InvalidStorageMetadata(
                    "prepared content exceeds its durable staging reservation".to_string(),
                ));
            }

            let changed = self.connection.execute(
                "INSERT INTO prepared_staged_content (
                   lease_id, content_id, staged_path, logical_size, source_fingerprint,
                   owner_marker, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(lease_id, content_id) DO UPDATE SET
                   updated_at = excluded.updated_at
                 WHERE prepared_staged_content.staged_path = excluded.staged_path
                   AND prepared_staged_content.logical_size = excluded.logical_size
                   AND prepared_staged_content.source_fingerprint = excluded.source_fingerprint
                   AND prepared_staged_content.owner_marker = excluded.owner_marker
                   AND prepared_staged_content.created_at = excluded.created_at",
                params![
                    content.lease_id.as_str(),
                    content.content_id.as_str(),
                    staged_path,
                    logical_size,
                    content.source_fingerprint.as_str(),
                    content.owner_marker.as_str(),
                    content.created_at,
                    content.updated_at,
                ],
            )?;
            if changed != 1 {
                return Err(MetadataError::InvalidStorageMetadata(
                    "prepared content identity is immutable within a lease".to_string(),
                ));
            }
            Ok(())
        })
    }

    pub fn prepared_staged_content(
        &self,
        lease_id: &PreparationLeaseId,
        owner_marker: &PreparationOwnerMarker,
    ) -> Result<Vec<PreparedStagedContentRecord>, MetadataError> {
        let sql = format!(
            "{STAGED_CONTENT_SELECT}
             WHERE lease_id = ?1 AND owner_marker = ?2 ORDER BY content_id"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params![lease_id.as_str(), owner_marker.as_str()],
            staged_content_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn transition_preparation_lease(
        &self,
        id: &PreparationLeaseId,
        owner_marker: &PreparationOwnerMarker,
        expected: PreparationLeaseState,
        next: PreparationLeaseState,
        now: &str,
    ) -> Result<bool, MetadataError> {
        if !expected.permits(next) {
            return Err(MetadataError::InvalidStorageMetadata(format!(
                "invalid preparation lifecycle transition from {} to {}",
                expected.as_str(),
                next.as_str()
            )));
        }
        self.connection
            .execute(
                "UPDATE preparation_leases
                 SET state = ?4,
                     prepared_at = CASE WHEN ?4 = 'prepared' THEN ?5 ELSE prepared_at END,
                     referenced_at = CASE WHEN ?4 = 'referenced-by-upload' THEN ?5 ELSE referenced_at END,
                     finished_at = CASE WHEN ?4 IN ('committed', 'abandoned') THEN ?5 ELSE finished_at END,
                     updated_at = ?5
                 WHERE id = ?1 AND owner_marker = ?2 AND state = ?3",
                params![
                    id.as_str(),
                    owner_marker.as_str(),
                    expected.as_str(),
                    next.as_str(),
                    now,
                ],
            )
            .map(|changed| changed == 1)
            .map_err(Into::into)
    }

    pub fn reconcile_preparation_orphans(
        &self,
        owner_marker: &PreparationOwnerMarker,
        updated_before: &str,
    ) -> Result<Vec<PreparationOrphanRecord>, MetadataError> {
        validate_owner_marker(owner_marker)?;
        let mut statement = self.connection.prepare(
            "SELECT staged.lease_id, staged.content_id, staged.staged_path,
                    staged.logical_size, staged.source_fingerprint, staged.owner_marker,
                    staged.created_at, staged.updated_at, lease.state
             FROM prepared_staged_content AS staged
             JOIN preparation_leases AS lease ON lease.id = staged.lease_id
             WHERE staged.owner_marker = ?1
               AND lease.owner_marker = ?1
               AND lease.state IN ('committed', 'abandoned')
               AND lease.updated_at <= ?2
               AND staged.updated_at <= ?2
             ORDER BY staged.updated_at, staged.staged_path",
        )?;
        let rows = statement.query_map(params![owner_marker.as_str(), updated_before], |row| {
            let content = staged_content_from_row(row)?;
            let terminal_state = parse_preparation_state(row.get::<_, String>(8)?, 8)?;
            Ok(PreparationOrphanRecord {
                content,
                terminal_state,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn forget_reconciled_preparation_orphan(
        &self,
        lease_id: &PreparationLeaseId,
        content_id: &ContentId,
        owner_marker: &PreparationOwnerMarker,
    ) -> Result<bool, MetadataError> {
        self.connection
            .execute(
                "DELETE FROM prepared_staged_content
                 WHERE lease_id = ?1 AND content_id = ?2 AND owner_marker = ?3
                   AND EXISTS (
                     SELECT 1 FROM preparation_leases
                     WHERE id = ?1 AND owner_marker = ?3
                       AND state IN ('committed', 'abandoned')
                   )",
                params![
                    lease_id.as_str(),
                    content_id.as_str(),
                    owner_marker.as_str()
                ],
            )
            .map(|changed| changed == 1)
            .map_err(Into::into)
    }
}

fn validate_owner_marker(owner_marker: &PreparationOwnerMarker) -> Result<(), MetadataError> {
    if owner_marker.as_str().is_empty() {
        return Err(MetadataError::InvalidStorageMetadata(
            "staging owner marker must not be empty".to_string(),
        ));
    }
    Ok(())
}

fn staged_path_to_text(path: &OwnedStagedPath) -> Result<&str, MetadataError> {
    path.as_path().to_str().ok_or_else(|| {
        MetadataError::InvalidStorageMetadata(
            "owned staged paths must be valid UTF-8 for durable metadata".to_string(),
        )
    })
}

fn to_sql_u64(value: u64, field: &'static str) -> Result<i64, MetadataError> {
    i64::try_from(value).map_err(|_| {
        MetadataError::InvalidStorageMetadata(format!("{field} exceeds SQLite integer range"))
    })
}

fn preparation_lease_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<PreparationLeaseRecord, rusqlite::Error> {
    Ok(PreparationLeaseRecord {
        id: PreparationLeaseId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        snapshot_candidate_id: SnapshotId::new(row.get::<_, String>(3)?),
        owner_marker: PreparationOwnerMarker::new(row.get::<_, String>(4)?),
        state: parse_preparation_state(row.get::<_, String>(5)?, 5)?,
        reservation_bytes: parse_sql_u64(row.get::<_, i64>(6)?, 6)?,
        prepared_at: row.get(7)?,
        referenced_at: row.get(8)?,
        finished_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn staged_content_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<PreparedStagedContentRecord, rusqlite::Error> {
    Ok(PreparedStagedContentRecord {
        lease_id: PreparationLeaseId::new(row.get::<_, String>(0)?),
        content_id: ContentId::new(row.get::<_, String>(1)?),
        staged_path: OwnedStagedPath::new(row.get::<_, String>(2)?),
        logical_size: parse_sql_u64(row.get::<_, i64>(3)?, 3)?,
        source_fingerprint: SourceFingerprint::new(row.get::<_, String>(4)?),
        owner_marker: PreparationOwnerMarker::new(row.get::<_, String>(5)?),
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn parse_preparation_state(
    value: String,
    column: usize,
) -> Result<PreparationLeaseState, rusqlite::Error> {
    PreparationLeaseState::from_wire(&value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            format!("unknown preparation lease state `{value}`").into(),
        )
    })
}

fn parse_sql_u64(value: i64, column: usize) -> Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}
