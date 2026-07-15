use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotPinId(String);

impl SnapshotPinId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPinReason {
    WorkspaceRef,
    ProjectRef,
    WorkView,
    Conflict,
    DurableOperation,
    ExplicitHistory,
}

impl SnapshotPinReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceRef => "workspace-ref",
            Self::ProjectRef => "project-ref",
            Self::WorkView => "work-view",
            Self::Conflict => "conflict",
            Self::DurableOperation => "durable-operation",
            Self::ExplicitHistory => "explicit-history",
        }
    }

    fn from_str(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "workspace-ref" => Ok(Self::WorkspaceRef),
            "project-ref" => Ok(Self::ProjectRef),
            "work-view" => Ok(Self::WorkView),
            "conflict" => Ok(Self::Conflict),
            "durable-operation" => Ok(Self::DurableOperation),
            "explicit-history" => Ok(Self::ExplicitHistory),
            _ => Err(invalid_pin_column(4, "unsupported snapshot pin reason")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPinOwnerKind {
    WorkspaceRef,
    ProjectRef,
    WorkView,
    Conflict,
    DurableOperation,
    ExplicitHistory,
}

impl SnapshotPinOwnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceRef => "workspace-ref",
            Self::ProjectRef => "project-ref",
            Self::WorkView => "work-view",
            Self::Conflict => "conflict",
            Self::DurableOperation => "durable-operation",
            Self::ExplicitHistory => "explicit-history",
        }
    }

    fn from_str(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "workspace-ref" => Ok(Self::WorkspaceRef),
            "project-ref" => Ok(Self::ProjectRef),
            "work-view" => Ok(Self::WorkView),
            "conflict" => Ok(Self::Conflict),
            "durable-operation" => Ok(Self::DurableOperation),
            "explicit-history" => Ok(Self::ExplicitHistory),
            _ => Err(invalid_pin_column(5, "unsupported snapshot pin owner kind")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPinOwner {
    pub kind: SnapshotPinOwnerKind,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPinRecord {
    pub id: SnapshotPinId,
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub root_id: NamespacePageId,
    pub reason: SnapshotPinReason,
    pub owner: SnapshotPinOwner,
    pub expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotPinReconcileReport {
    pub acquired: u64,
    pub updated: u64,
    pub released: u64,
    pub active: u64,
}

impl MetadataStore {
    pub fn acquire_snapshot_pin(
        &mut self,
        pin: &SnapshotPinRecord,
    ) -> Result<SnapshotPinRecord, MetadataError> {
        validate_snapshot_pin(self, pin)?;
        self.with_committed(|store| {
            upsert_snapshot_pin(store, pin)?;
            invalidate_gc(store, &pin.workspace_id)?;
            Ok::<(), MetadataError>(())
        })?;
        self.snapshot_pin_by_owner(&pin.workspace_id, &pin.snapshot_id, pin.reason, &pin.owner)?
            .ok_or_else(|| {
                MetadataError::InvalidStorageMetadata("snapshot pin vanished".to_string())
            })
    }

    pub fn reconcile_snapshot_pins(
        &mut self,
        workspace_id: &WorkspaceId,
        desired: &[SnapshotPinRecord],
    ) -> Result<SnapshotPinReconcileReport, MetadataError> {
        let mut desired_by_id = BTreeMap::new();
        for pin in desired {
            if pin.workspace_id != *workspace_id
                || desired_by_id.insert(pin.id.clone(), pin.clone()).is_some()
            {
                return Err(MetadataError::InvalidStorageMetadata(
                    "snapshot pin reconciliation contains a foreign or duplicate pin".to_string(),
                ));
            }
        }
        self.with_committed(|store| {
            for pin in desired_by_id.values() {
                validate_snapshot_pin(store, pin)?;
            }
            let existing = snapshot_pins_for_workspace(store, workspace_id)?
                .into_iter()
                .map(|pin| (pin.id.clone(), pin))
                .collect::<BTreeMap<_, _>>();
            let acquired = desired_by_id
                .keys()
                .filter(|id| !existing.contains_key(*id))
                .count() as u64;
            let updated = desired_by_id
                .iter()
                .filter(|(id, pin)| {
                    existing
                        .get(*id)
                        .is_some_and(|current| !same_reconciled_pin(current, pin))
                })
                .count() as u64;
            let released = existing
                .keys()
                .filter(|id| !desired_by_id.contains_key(*id))
                .count() as u64;
            for pin in desired_by_id.values() {
                upsert_snapshot_pin(store, pin)?;
            }
            for pin_id in existing
                .keys()
                .filter(|id| !desired_by_id.contains_key(*id))
            {
                store.connection.execute(
                    "DELETE FROM snapshot_pins WHERE workspace_id = ?1 AND id = ?2",
                    params![workspace_id.as_str(), pin_id.as_str()],
                )?;
            }
            if acquired > 0 || updated > 0 || released > 0 {
                invalidate_gc(store, workspace_id)?;
            }
            Ok(SnapshotPinReconcileReport {
                acquired,
                updated,
                released,
                active: desired_by_id.len() as u64,
            })
        })
    }

    pub fn release_snapshot_pin(
        &mut self,
        workspace_id: &WorkspaceId,
        pin_id: &SnapshotPinId,
    ) -> Result<bool, MetadataError> {
        self.with_committed(|store| {
            let deleted = store.connection.execute(
                "DELETE FROM snapshot_pins WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id.as_str(), pin_id.as_str()],
            )? > 0;
            if deleted {
                invalidate_gc(store, workspace_id)?;
            }
            Ok(deleted)
        })
    }

    pub fn release_expired_snapshot_pins(
        &mut self,
        workspace_id: &WorkspaceId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        self.with_committed(|store| {
            let deleted = store.connection.execute(
                "DELETE FROM snapshot_pins
                 WHERE workspace_id = ?1 AND expires_at IS NOT NULL AND expires_at <= ?2",
                params![workspace_id.as_str(), now],
            )? as u64;
            if deleted > 0 {
                invalidate_gc(store, workspace_id)?;
            }
            Ok(deleted)
        })
    }

    pub fn active_snapshot_pins(
        &self,
        workspace_id: &WorkspaceId,
        now: &str,
    ) -> Result<Vec<SnapshotPinRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, snapshot_id, root_id, reason, owner_kind, owner_id,
                    expires_at, created_at
             FROM snapshot_pins
             WHERE workspace_id = ?1 AND (expires_at IS NULL OR expires_at > ?2)
             ORDER BY snapshot_id, reason, owner_kind, owner_id",
        )?;
        let rows = statement.query_map(params![workspace_id.as_str(), now], pin_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn snapshot_pin_by_owner(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
        reason: SnapshotPinReason,
        owner: &SnapshotPinOwner,
    ) -> Result<Option<SnapshotPinRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, snapshot_id, root_id, reason, owner_kind, owner_id,
                        expires_at, created_at
                 FROM snapshot_pins
                 WHERE workspace_id = ?1 AND snapshot_id = ?2 AND reason = ?3
                   AND owner_kind = ?4 AND owner_id = ?5",
                params![
                    workspace_id.as_str(),
                    snapshot_id.as_str(),
                    reason.as_str(),
                    owner.kind.as_str(),
                    owner.id,
                ],
                pin_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

fn validate_snapshot_pin(
    store: &MetadataStore,
    pin: &SnapshotPinRecord,
) -> Result<(), MetadataError> {
    if pin.id.as_str().is_empty()
        || pin.owner.id.is_empty()
        || pin.reason.as_str() != pin.owner.kind.as_str()
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "snapshot pin identity, reason, and owner are inconsistent".to_string(),
        ));
    }
    let snapshot = store
        .snapshot(&pin.workspace_id, &pin.snapshot_id)?
        .ok_or_else(|| {
            MetadataError::InvalidStorageMetadata("pinned snapshot was not found".to_string())
        })?;
    if snapshot.root_id != pin.root_id {
        return Err(MetadataError::InvalidStorageMetadata(
            "snapshot pin root does not match the committed snapshot root".to_string(),
        ));
    }
    let completeness = store.snapshot_root_completeness(&pin.workspace_id, &pin.snapshot_id)?;
    if let Some(missing) = completeness.missing_or_unverified.first() {
        return Err(MetadataError::IncompleteSnapshotRoot {
            snapshot_id: pin.snapshot_id.clone(),
            logical_id: missing.logical_id.as_str().to_string(),
        });
    }
    Ok(())
}

fn upsert_snapshot_pin(
    store: &MetadataStore,
    pin: &SnapshotPinRecord,
) -> Result<(), MetadataError> {
    store.connection.execute(
        "INSERT INTO snapshot_pins
         (id, workspace_id, snapshot_id, root_id, reason, owner_kind, owner_id,
          expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
           workspace_id = excluded.workspace_id,
           snapshot_id = excluded.snapshot_id,
           root_id = excluded.root_id,
           reason = excluded.reason,
           owner_kind = excluded.owner_kind,
           owner_id = excluded.owner_id,
           expires_at = excluded.expires_at",
        params![
            pin.id.as_str(),
            pin.workspace_id.as_str(),
            pin.snapshot_id.as_str(),
            pin.root_id.as_str(),
            pin.reason.as_str(),
            pin.owner.kind.as_str(),
            pin.owner.id,
            pin.expires_at,
            pin.created_at,
        ],
    )?;
    Ok(())
}

fn snapshot_pins_for_workspace(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
) -> Result<Vec<SnapshotPinRecord>, MetadataError> {
    let mut statement = store.connection.prepare(
        "SELECT id, workspace_id, snapshot_id, root_id, reason, owner_kind, owner_id,
                expires_at, created_at
         FROM snapshot_pins WHERE workspace_id = ?1 ORDER BY id",
    )?;
    let rows = statement.query_map([workspace_id.as_str()], pin_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn same_reconciled_pin(left: &SnapshotPinRecord, right: &SnapshotPinRecord) -> bool {
    left.id == right.id
        && left.workspace_id == right.workspace_id
        && left.snapshot_id == right.snapshot_id
        && left.root_id == right.root_id
        && left.reason == right.reason
        && left.owner == right.owner
        && left.expires_at == right.expires_at
}

fn pin_from_row(row: &rusqlite::Row<'_>) -> Result<SnapshotPinRecord, rusqlite::Error> {
    Ok(SnapshotPinRecord {
        id: SnapshotPinId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        snapshot_id: SnapshotId::new(row.get::<_, String>(2)?),
        root_id: NamespacePageId::new(row.get::<_, String>(3)?),
        reason: SnapshotPinReason::from_str(&row.get::<_, String>(4)?)?,
        owner: SnapshotPinOwner {
            kind: SnapshotPinOwnerKind::from_str(&row.get::<_, String>(5)?)?,
            id: row.get(6)?,
        },
        expires_at: row.get(7)?,
        created_at: row.get(8)?,
    })
}

fn invalid_pin_column(index: usize, reason: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(io::Error::new(io::ErrorKind::InvalidData, reason)),
    )
}
