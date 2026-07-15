use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRootKind {
    Namespace,
    Extra,
}

impl SnapshotRootKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::Extra => "extra",
        }
    }

    fn from_str(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "namespace" => Ok(Self::Namespace),
            "extra" => Ok(Self::Extra),
            _ => Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported snapshot root kind",
                )),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRootReference {
    pub root_kind: SnapshotRootKind,
    pub record: MetadataRecordRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRootRecord {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub roots: Vec<SnapshotRootReference>,
    pub committed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRootCompleteness {
    pub snapshot_id: SnapshotId,
    pub complete: bool,
    pub missing_or_unverified: Vec<MetadataRecordRef>,
}

impl MetadataStore {
    pub fn commit_snapshot_root(
        &mut self,
        snapshot: &SnapshotRecord,
        extra_roots: &[MetadataRecordRef],
        committed_at: &str,
    ) -> Result<SnapshotRootRecord, MetadataError> {
        self.with_committed(|store| {
            store.commit_snapshot_root_uncommitted(snapshot, extra_roots, committed_at)
        })
    }

    pub(crate) fn commit_snapshot_root_uncommitted(
        &mut self,
        snapshot: &SnapshotRecord,
        extra_roots: &[MetadataRecordRef],
        committed_at: &str,
    ) -> Result<SnapshotRootRecord, MetadataError> {
        let namespace_root = MetadataRecordRef {
            kind: MetadataRecordKind::NamespacePage,
            logical_id: MetadataLogicalId::new(snapshot.root_id.as_str()),
        };
        let mut required = vec![namespace_root.clone()];
        required.extend(extra_roots.iter().cloned());
        if let Some(existing_snapshot) = self.snapshot(&snapshot.workspace_id, &snapshot.id)?
            && !existing_snapshot.has_same_immutable_binding(snapshot)
        {
            return Err(MetadataError::ImmutableBindingConflict {
                logical_id: snapshot.id.as_str().to_string(),
                field: "snapshot_root",
            });
        }
        if let Some(existing) = self.snapshot_root(&snapshot.workspace_id, &snapshot.id)? {
            let mut existing_records = existing
                .roots
                .iter()
                .map(|root| (root.root_kind.as_str(), root.record.clone()))
                .collect::<Vec<_>>();
            let mut requested_records =
                std::iter::once((SnapshotRootKind::Namespace.as_str(), namespace_root.clone()))
                    .chain(
                        extra_roots
                            .iter()
                            .cloned()
                            .map(|root| (SnapshotRootKind::Extra.as_str(), root)),
                    )
                    .collect::<Vec<_>>();
            existing_records.sort();
            requested_records.sort();
            if existing_records != requested_records {
                return Err(MetadataError::ImmutableBindingConflict {
                    logical_id: snapshot.id.as_str().to_string(),
                    field: "required_roots",
                });
            }
            return Ok(existing);
        }
        ensure_records_verified(self, snapshot, &required)?;

        self.upsert_snapshot(snapshot)?;
        self.connection.execute(
            "DELETE FROM snapshot_roots WHERE workspace_id = ?1 AND snapshot_id = ?2",
            params![snapshot.workspace_id.as_str(), snapshot.id.as_str()],
        )?;
        insert_root(
            self,
            snapshot,
            SnapshotRootKind::Namespace,
            &namespace_root,
            committed_at,
        )?;
        for root in extra_roots {
            insert_root(self, snapshot, SnapshotRootKind::Extra, root, committed_at)?;
        }
        invalidate_gc(self, &snapshot.workspace_id)?;

        Ok(SnapshotRootRecord {
            workspace_id: snapshot.workspace_id.clone(),
            snapshot_id: snapshot.id.clone(),
            roots: std::iter::once(SnapshotRootReference {
                root_kind: SnapshotRootKind::Namespace,
                record: namespace_root,
            })
            .chain(
                extra_roots
                    .iter()
                    .cloned()
                    .map(|record| SnapshotRootReference {
                        root_kind: SnapshotRootKind::Extra,
                        record,
                    }),
            )
            .collect(),
            committed_at: committed_at.to_string(),
        })
    }

    pub fn snapshot_root(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<Option<SnapshotRootRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT root_kind, record_kind, logical_id, committed_at
             FROM snapshot_roots
             WHERE workspace_id = ?1 AND snapshot_id = ?2
             ORDER BY root_kind DESC, record_kind, logical_id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), snapshot_id.as_str()],
            |row| {
                Ok((
                    SnapshotRootReference {
                        root_kind: SnapshotRootKind::from_str(&row.get::<_, String>(0)?)?,
                        record: MetadataRecordRef {
                            kind: MetadataRecordKind::from_str(&row.get::<_, String>(1)?)?,
                            logical_id: MetadataLogicalId::new(row.get::<_, String>(2)?),
                        },
                    },
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        let Some((_, committed_at)) = rows.first() else {
            return Ok(None);
        };
        Ok(Some(SnapshotRootRecord {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            roots: rows.iter().map(|(root, _)| root.clone()).collect(),
            committed_at: committed_at.clone(),
        }))
    }

    pub fn snapshot_root_completeness(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<SnapshotRootCompleteness, MetadataError> {
        let roots = self.snapshot_root(workspace_id, snapshot_id)?;
        let mut missing = Vec::new();
        if roots.is_some() {
            let mut statement = self.connection.prepare(
                "WITH RECURSIVE reachable(record_kind, logical_id) AS (
                   SELECT record_kind, logical_id FROM snapshot_roots
                   WHERE workspace_id = ?1 AND snapshot_id = ?2
                   UNION
                   SELECT edges.child_kind, edges.child_logical_id
                   FROM metadata_record_edges AS edges
                   JOIN reachable
                     ON reachable.record_kind = edges.parent_kind
                    AND reachable.logical_id = edges.parent_logical_id
                   WHERE edges.workspace_id = ?1
                 )
                 SELECT reachable.record_kind, reachable.logical_id,
                        CASE
                          WHEN bindings.verification_state = 'verified' THEN 'verified'
                          WHEN reachable.record_kind = 'namespace-page' AND EXISTS (
                            SELECT 1 FROM namespace_pages AS cache
                            WHERE cache.workspace_id = ?1
                              AND cache.logical_id = reachable.logical_id
                              AND cache.cache_state = 'present'
                          ) THEN 'verified'
                          WHEN reachable.record_kind = 'content-layout' AND EXISTS (
                            SELECT 1 FROM content_layouts AS cache
                            WHERE cache.workspace_id = ?1
                              AND cache.logical_id = reachable.logical_id
                              AND cache.cache_state = 'present'
                          ) THEN 'verified'
                          WHEN reachable.record_kind = 'segment-page' AND EXISTS (
                            SELECT 1 FROM segment_pages AS cache
                            WHERE cache.workspace_id = ?1
                              AND cache.logical_id = reachable.logical_id
                              AND cache.cache_state = 'present'
                          ) THEN 'verified'
                          ELSE 'missing'
                        END
                 FROM reachable
                 LEFT JOIN metadata_object_bindings AS bindings
                   ON bindings.workspace_id = ?1
                  AND bindings.record_kind = reachable.record_kind
                  AND bindings.logical_id = reachable.logical_id
                 ORDER BY reachable.record_kind, reachable.logical_id
                 LIMIT 1000001",
            )?;
            let reachable = statement
                .query_map(
                    params![workspace_id.as_str(), snapshot_id.as_str()],
                    |row| {
                        Ok((
                            MetadataRecordRef {
                                kind: MetadataRecordKind::from_str(&row.get::<_, String>(0)?)?,
                                logical_id: MetadataLogicalId::new(row.get::<_, String>(1)?),
                            },
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            if reachable.len() > 1_000_000 {
                return Err(MetadataError::InvalidStorageMetadata(
                    "snapshot root reachability exceeds the local completeness limit".to_string(),
                ));
            }
            missing.extend(
                reachable
                    .into_iter()
                    .filter(|(_, state)| state != "verified")
                    .map(|(record, _)| record),
            );
        } else {
            let root_id = self
                .snapshot(workspace_id, snapshot_id)?
                .map(|snapshot| snapshot.root_id.as_str().to_string())
                .unwrap_or_else(|| snapshot_id.as_str().to_string());
            missing.push(MetadataRecordRef {
                kind: MetadataRecordKind::NamespacePage,
                logical_id: MetadataLogicalId::new(root_id),
            });
        }
        Ok(SnapshotRootCompleteness {
            snapshot_id: snapshot_id.clone(),
            complete: missing.is_empty(),
            missing_or_unverified: missing,
        })
    }
}

fn ensure_records_verified(
    store: &MetadataStore,
    snapshot: &SnapshotRecord,
    records: &[MetadataRecordRef],
) -> Result<(), MetadataError> {
    for record in records {
        let verified = metadata_record_is_verified(store, &snapshot.workspace_id, record)?;
        if !verified {
            return Err(MetadataError::IncompleteSnapshotRoot {
                snapshot_id: snapshot.id.clone(),
                logical_id: record.logical_id.as_str().to_string(),
            });
        }
    }
    Ok(())
}

fn metadata_record_is_verified(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
) -> Result<bool, MetadataError> {
    if store
        .metadata_object_binding(workspace_id, record.kind, &record.logical_id)?
        .is_some_and(|binding| binding.verification_state == MetadataVerificationState::Verified)
    {
        return Ok(true);
    }
    if record.kind == MetadataRecordKind::SnapshotRoot {
        return Ok(false);
    }
    Ok(store
        .metadata_cache_record(workspace_id, record)?
        .is_some_and(|cache| cache.state == MetadataCacheState::Present))
}

fn insert_root(
    store: &MetadataStore,
    snapshot: &SnapshotRecord,
    root_kind: SnapshotRootKind,
    root: &MetadataRecordRef,
    committed_at: &str,
) -> Result<(), MetadataError> {
    store.connection.execute(
        "INSERT INTO snapshot_roots
         (workspace_id, snapshot_id, root_kind, record_kind, logical_id, committed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            snapshot.workspace_id.as_str(),
            snapshot.id.as_str(),
            root_kind.as_str(),
            root.kind.as_str(),
            root.logical_id.as_str(),
            committed_at,
        ],
    )?;
    Ok(())
}

pub(super) fn invalidate_gc(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
) -> Result<(), MetadataError> {
    store.connection.execute(
        "DELETE FROM metadata_gc_checkpoints WHERE workspace_id = ?1",
        [workspace_id.as_str()],
    )?;
    store.connection.execute(
        "DELETE FROM metadata_gc_queue WHERE workspace_id = ?1",
        [workspace_id.as_str()],
    )?;
    Ok(())
}
