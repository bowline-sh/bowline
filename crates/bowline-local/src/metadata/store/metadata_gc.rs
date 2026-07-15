use super::common::*;
use super::*;

mod cache_delete;
use cache_delete::*;

#[cfg(test)]
pub(crate) fn set_metadata_cache_delete_fault(path: &PathBuf, enabled: bool) {
    set_cache_delete_fault(path, enabled);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataGcPhase {
    Mark,
    Sweep,
    Complete,
}

impl MetadataGcPhase {
    fn from_str(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "mark" => Ok(Self::Mark),
            "sweep" => Ok(Self::Sweep),
            "complete" => Ok(Self::Complete),
            _ => Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported metadata GC phase",
                )),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataGcCheckpoint {
    pub workspace_id: WorkspaceId,
    pub generation: String,
    pub phase: MetadataGcPhase,
    pub sweep_cursor: Option<MetadataRecordRef>,
    pub grace_before: String,
    pub started_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataGcCandidate {
    pub generation: String,
    pub record: MetadataRecordRef,
    pub object_key: MetadataObjectKey,
    pub cache_path: Option<String>,
    pub cache_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetadataGcFinalizeReport {
    pub metadata_record_deleted: bool,
    pub cache_file_deleted: bool,
    pub cache_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataGcBatchReport {
    pub generation: String,
    pub phase: MetadataGcPhase,
    pub records_processed: u64,
    pub records_marked: u64,
    pub delete_candidates: Vec<MetadataGcCandidate>,
    pub cache_files_deleted: u64,
    pub cache_bytes_deleted: u64,
    pub metadata_records_deleted: u64,
    pub complete: bool,
}

impl MetadataStore {
    pub fn start_metadata_gc(
        &mut self,
        workspace_id: &WorkspaceId,
        generation: &str,
        grace_before: &str,
        now: &str,
    ) -> Result<MetadataGcCheckpoint, MetadataError> {
        if generation.is_empty() {
            return Err(MetadataError::InvalidStorageMetadata(
                "metadata GC generation must be non-empty".to_string(),
            ));
        }
        self.with_committed(|store| {
            store.connection.execute(
                "DELETE FROM metadata_gc_queue WHERE workspace_id = ?1",
                [workspace_id.as_str()],
            )?;
            store.connection.execute(
                "INSERT INTO metadata_gc_checkpoints
                 (workspace_id, generation, phase, sweep_cursor_kind, sweep_cursor_id,
                  grace_before, started_at, updated_at)
                 VALUES (?1, ?2, 'mark', NULL, NULL, ?3, ?4, ?4)
                 ON CONFLICT(workspace_id) DO UPDATE SET
                   generation = excluded.generation,
                   phase = excluded.phase,
                   sweep_cursor_kind = NULL,
                   sweep_cursor_id = NULL,
                   grace_before = excluded.grace_before,
                   started_at = excluded.started_at,
                   updated_at = excluded.updated_at",
                params![workspace_id.as_str(), generation, grace_before, now],
            )?;
            store.connection.execute(
                "INSERT OR IGNORE INTO metadata_gc_queue
                 (workspace_id, generation, record_kind, logical_id, state, enqueued_at)
                 SELECT roots.workspace_id, ?2, roots.record_kind, roots.logical_id, 'pending', ?3
                 FROM snapshot_roots AS roots
                 JOIN snapshot_pins AS pins
                   ON pins.workspace_id = roots.workspace_id
                  AND pins.snapshot_id = roots.snapshot_id
                 WHERE roots.workspace_id = ?1
                   AND (pins.expires_at IS NULL OR pins.expires_at > ?3)",
                params![workspace_id.as_str(), generation, now],
            )?;
            Ok::<(), MetadataError>(())
        })?;
        Ok(MetadataGcCheckpoint {
            workspace_id: workspace_id.clone(),
            generation: generation.to_string(),
            phase: MetadataGcPhase::Mark,
            sweep_cursor: None,
            grace_before: grace_before.to_string(),
            started_at: now.to_string(),
            updated_at: now.to_string(),
        })
    }

    pub fn metadata_gc_checkpoint(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<MetadataGcCheckpoint>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, generation, phase, sweep_cursor_kind, sweep_cursor_id,
                        grace_before, started_at, updated_at
                 FROM metadata_gc_checkpoints WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                checkpoint_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn run_metadata_gc_batch(
        &mut self,
        workspace_id: &WorkspaceId,
        max_records: u64,
        now: &str,
    ) -> Result<MetadataGcBatchReport, MetadataError> {
        require_top_level_gc_transaction(self)?;
        if max_records == 0 {
            return Err(MetadataError::InvalidStorageMetadata(
                "metadata GC batch limit must be non-zero".to_string(),
            ));
        }
        let checkpoint = self.metadata_gc_checkpoint(workspace_id)?.ok_or_else(|| {
            MetadataError::InvalidStorageMetadata("metadata GC has not been started".to_string())
        })?;
        match checkpoint.phase {
            MetadataGcPhase::Mark => self.run_mark_batch(&checkpoint, max_records, now),
            MetadataGcPhase::Sweep => self.run_sweep_batch(&checkpoint, max_records, now),
            MetadataGcPhase::Complete => Ok(MetadataGcBatchReport {
                generation: checkpoint.generation,
                phase: MetadataGcPhase::Complete,
                records_processed: 0,
                records_marked: 0,
                delete_candidates: Vec::new(),
                cache_files_deleted: 0,
                cache_bytes_deleted: 0,
                metadata_records_deleted: 0,
                complete: true,
            }),
        }
    }

    pub fn finalize_metadata_gc_candidate(
        &mut self,
        workspace_id: &WorkspaceId,
        candidate: &MetadataGcCandidate,
    ) -> Result<MetadataGcFinalizeReport, MetadataError> {
        require_top_level_gc_transaction(self)?;
        let Some(checkpoint) = self.metadata_gc_checkpoint(workspace_id)? else {
            return Ok(MetadataGcFinalizeReport::default());
        };
        if checkpoint.generation != candidate.generation
            || checkpoint.phase == MetadataGcPhase::Mark
        {
            return Ok(MetadataGcFinalizeReport::default());
        }
        let (eligible, cache_plan) = self.with_committed(|store| {
            let eligible = store.connection.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM metadata_gc_queue
                   WHERE workspace_id = ?1 AND generation = ?2 AND record_kind = ?3
                     AND logical_id = ?4 AND state = 'delete-eligible'
                 )",
                params![
                    workspace_id.as_str(),
                    candidate.generation,
                    candidate.record.kind.as_str(),
                    candidate.record.logical_id.as_str(),
                ],
                |row| row.get::<_, bool>(0),
            )?;
            if !eligible {
                return Ok((false, None));
            }
            let binding_matches = store.connection.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM metadata_object_bindings
                   WHERE workspace_id = ?1 AND record_kind = ?2 AND logical_id = ?3
                     AND object_key = ?4
                 )",
                params![
                    workspace_id.as_str(),
                    candidate.record.kind.as_str(),
                    candidate.record.logical_id.as_str(),
                    candidate.object_key.as_str(),
                ],
                |row| row.get::<_, bool>(0),
            )?;
            if !binding_matches {
                return Ok((false, None));
            }
            begin_candidate_cache_delete(store, workspace_id, candidate).map(|plan| (true, plan))
        })?;
        if !eligible {
            return Ok(MetadataGcFinalizeReport::default());
        }
        let cache = execute_cache_delete(cache_plan)?;
        let deleted = self.with_committed(|store| {
            let checkpoint_matches = store.connection.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM metadata_gc_checkpoints
                   WHERE workspace_id = ?1 AND generation = ?2 AND phase != 'mark'
                 )",
                params![workspace_id.as_str(), candidate.generation],
                |row| row.get::<_, bool>(0),
            )?;
            if !checkpoint_matches {
                return Ok(false);
            }
            finish_candidate_cache_delete(store, workspace_id, candidate)?;
            let deleted = store.connection.execute(
                "DELETE FROM metadata_records
                 WHERE workspace_id = ?1 AND record_kind = ?2 AND logical_id = ?3
                   AND EXISTS (
                     SELECT 1 FROM metadata_object_bindings AS bindings
                     WHERE bindings.workspace_id = metadata_records.workspace_id
                       AND bindings.record_kind = metadata_records.record_kind
                       AND bindings.logical_id = metadata_records.logical_id
                       AND bindings.object_key = ?4
                   )",
                params![
                    workspace_id.as_str(),
                    candidate.record.kind.as_str(),
                    candidate.record.logical_id.as_str(),
                    candidate.object_key.as_str(),
                ],
            )? > 0;
            Ok::<_, MetadataError>(deleted)
        })?;
        if !deleted {
            return Ok(MetadataGcFinalizeReport::default());
        }
        Ok(MetadataGcFinalizeReport {
            metadata_record_deleted: true,
            cache_file_deleted: cache.file_deleted,
            cache_bytes: cache.bytes_deleted,
        })
    }

    pub fn metadata_gc_delete_candidates(
        &self,
        workspace_id: &WorkspaceId,
        limit: u64,
    ) -> Result<Vec<MetadataGcCandidate>, MetadataError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(checkpoint) = self.metadata_gc_checkpoint(workspace_id)? else {
            return Ok(Vec::new());
        };
        if checkpoint.phase == MetadataGcPhase::Mark {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT queue.record_kind, queue.logical_id, bindings.object_key
             FROM metadata_gc_queue AS queue
             JOIN metadata_object_bindings AS bindings
               ON bindings.workspace_id = queue.workspace_id
              AND bindings.record_kind = queue.record_kind
              AND bindings.logical_id = queue.logical_id
             WHERE queue.workspace_id = ?1 AND queue.generation = ?2
               AND queue.state = 'delete-eligible'
             ORDER BY queue.record_kind, queue.logical_id LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![
                    workspace_id.as_str(),
                    checkpoint.generation,
                    sql_limit(Some(limit)),
                ],
                |row| {
                    Ok((
                        MetadataRecordRef {
                            kind: MetadataRecordKind::from_str(&row.get::<_, String>(0)?)?,
                            logical_id: MetadataLogicalId::new(row.get::<_, String>(1)?),
                        },
                        MetadataObjectKey::new(row.get::<_, String>(2)?),
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        rows.into_iter()
            .map(|(record, object_key)| {
                let cache = gc_cache_record(self, workspace_id, &record)?;
                Ok(MetadataGcCandidate {
                    generation: checkpoint.generation.clone(),
                    record,
                    object_key,
                    cache_path: cache.as_ref().and_then(|cache| cache.cache_path.clone()),
                    cache_bytes: cache.map_or(0, |cache| cache.encoded_bytes),
                })
            })
            .collect()
    }

    pub fn delete_unpinned_snapshots_batch(
        &mut self,
        workspace_id: &WorkspaceId,
        grace_before: &str,
        limit: u64,
        now: &str,
    ) -> Result<Vec<SnapshotId>, MetadataError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.with_committed(|store| {
            store.delete_unpinned_snapshots_batch_uncommitted(
                workspace_id,
                grace_before,
                limit,
                now,
            )
        })
    }

    fn delete_unpinned_snapshots_batch_uncommitted(
        &mut self,
        workspace_id: &WorkspaceId,
        grace_before: &str,
        limit: u64,
        now: &str,
    ) -> Result<Vec<SnapshotId>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM snapshots
             WHERE workspace_id = ?1 AND created_at < ?2
               AND NOT EXISTS (
                 SELECT 1 FROM snapshot_pins
                 WHERE snapshot_pins.workspace_id = snapshots.workspace_id
                   AND snapshot_pins.snapshot_id = snapshots.id
                   AND (snapshot_pins.expires_at IS NULL OR snapshot_pins.expires_at > ?3)
               )
               AND NOT EXISTS (
                 SELECT 1 FROM workspace_sync_heads AS heads
                 WHERE heads.workspace_id = snapshots.workspace_id
                   AND heads.snapshot_id = snapshots.id
               )
               AND NOT EXISTS (
                 SELECT 1 FROM projects
                 WHERE projects.workspace_id = snapshots.workspace_id
                   AND projects.latest_snapshot_id = snapshots.id
               )
               AND NOT EXISTS (
                 SELECT 1 FROM work_views
                 WHERE work_views.workspace_id = snapshots.workspace_id
                   AND work_views.base_snapshot_id = snapshots.id
                   AND (
                     work_views.lifecycle IN ('active', 'review-ready')
                     OR (work_views.retention_state = 'retained' AND work_views.retain_until > ?3)
                   )
               )
               AND NOT EXISTS (
                 SELECT 1 FROM work_views
                 WHERE work_views.workspace_id = snapshots.workspace_id
                   AND work_views.exposed_snapshot_id = snapshots.id
                   AND (
                     work_views.lifecycle IN ('active', 'review-ready')
                     OR (work_views.retention_state = 'retained' AND work_views.retain_until > ?3)
                   )
               )
               AND NOT EXISTS (
                 SELECT 1 FROM sync_operations AS operations
                 WHERE operations.workspace_id = snapshots.workspace_id
                   AND operations.state NOT IN ('completed', 'cancelled')
                   AND (
                     operations.base_snapshot_id = snapshots.id
                     OR operations.target_snapshot_id = snapshots.id
                   )
               )
               AND NOT EXISTS (
                 SELECT 1 FROM work_view_accept_operations AS accepts
                 WHERE accepts.workspace_id = snapshots.workspace_id
                   AND accepts.state IN ('queued', 'claimed', 'waiting-retry', 'review-required')
                   AND (
                     accepts.observed_main_snapshot_id = snapshots.id
                     OR accepts.observed_ref_snapshot_id = snapshots.id
                     OR accepts.target_snapshot_id = snapshots.id
                   )
               )
             ORDER BY created_at, id LIMIT ?4",
        )?;
        let ids = statement
            .query_map(
                params![
                    workspace_id.as_str(),
                    grace_before,
                    now,
                    sql_limit(Some(limit)),
                ],
                |row| row.get::<_, String>(0).map(SnapshotId::new),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for id in &ids {
            self.connection.execute(
                "DELETE FROM snapshots WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id.as_str(), id.as_str()],
            )?;
        }
        if !ids.is_empty() {
            invalidate_gc(self, workspace_id)?;
        }
        Ok(ids)
    }

    fn run_mark_batch(
        &mut self,
        checkpoint: &MetadataGcCheckpoint,
        max_records: u64,
        now: &str,
    ) -> Result<MetadataGcBatchReport, MetadataError> {
        let pending = gc_queue_records(
            &self.connection,
            &checkpoint.workspace_id,
            &checkpoint.generation,
            "pending",
            max_records,
        )?;
        if pending.is_empty() {
            self.connection.execute(
                "UPDATE metadata_gc_checkpoints SET phase = 'sweep', updated_at = ?2
                 WHERE workspace_id = ?1 AND generation = ?3",
                params![checkpoint.workspace_id.as_str(), now, checkpoint.generation,],
            )?;
            return Ok(MetadataGcBatchReport {
                generation: checkpoint.generation.clone(),
                phase: MetadataGcPhase::Sweep,
                records_processed: 0,
                records_marked: 0,
                delete_candidates: Vec::new(),
                cache_files_deleted: 0,
                cache_bytes_deleted: 0,
                metadata_records_deleted: 0,
                complete: false,
            });
        }
        self.with_committed(|store| {
            for record in &pending {
                store.connection.execute(
                    "UPDATE metadata_gc_queue SET state = 'marked', processed_at = ?5
                     WHERE workspace_id = ?1 AND generation = ?2 AND record_kind = ?3
                       AND logical_id = ?4 AND state = 'pending'",
                    params![
                        checkpoint.workspace_id.as_str(),
                        checkpoint.generation,
                        record.kind.as_str(),
                        record.logical_id.as_str(),
                        now,
                    ],
                )?;
                store.connection.execute(
                    "INSERT OR IGNORE INTO metadata_gc_queue
                     (workspace_id, generation, record_kind, logical_id, state, enqueued_at)
                     SELECT workspace_id, ?2, child_kind, child_logical_id, 'pending', ?5
                     FROM metadata_record_edges
                     WHERE workspace_id = ?1 AND parent_kind = ?3 AND parent_logical_id = ?4",
                    params![
                        checkpoint.workspace_id.as_str(),
                        checkpoint.generation,
                        record.kind.as_str(),
                        record.logical_id.as_str(),
                        now,
                    ],
                )?;
            }
            store.connection.execute(
                "UPDATE metadata_gc_checkpoints SET updated_at = ?2
                 WHERE workspace_id = ?1 AND generation = ?3",
                params![checkpoint.workspace_id.as_str(), now, checkpoint.generation,],
            )?;
            Ok::<(), MetadataError>(())
        })?;
        Ok(MetadataGcBatchReport {
            generation: checkpoint.generation.clone(),
            phase: MetadataGcPhase::Mark,
            records_processed: pending.len() as u64,
            records_marked: pending.len() as u64,
            delete_candidates: Vec::new(),
            cache_files_deleted: 0,
            cache_bytes_deleted: 0,
            metadata_records_deleted: 0,
            complete: false,
        })
    }

    fn run_sweep_batch(
        &mut self,
        checkpoint: &MetadataGcCheckpoint,
        max_records: u64,
        now: &str,
    ) -> Result<MetadataGcBatchReport, MetadataError> {
        let cursor_kind = checkpoint
            .sweep_cursor
            .as_ref()
            .map(|cursor| cursor.kind.as_str())
            .unwrap_or("");
        let cursor_id = checkpoint
            .sweep_cursor
            .as_ref()
            .map(|cursor| cursor.logical_id.as_str())
            .unwrap_or("");
        let mut statement = self.connection.prepare(
            "SELECT records.record_kind, records.logical_id, bindings.object_key
             FROM metadata_records AS records
             LEFT JOIN metadata_object_bindings AS bindings
               ON bindings.workspace_id = records.workspace_id
              AND bindings.record_kind = records.record_kind
              AND bindings.logical_id = records.logical_id
             WHERE records.workspace_id = ?1 AND records.created_at < ?2
               AND (records.record_kind > ?3
                    OR (records.record_kind = ?3 AND records.logical_id > ?4))
               AND NOT EXISTS (
                 SELECT 1 FROM metadata_gc_queue AS queue
                 WHERE queue.workspace_id = records.workspace_id
                   AND queue.generation = ?5
                   AND queue.record_kind = records.record_kind
                   AND queue.logical_id = records.logical_id
                   AND queue.state = 'marked'
               )
             ORDER BY records.record_kind, records.logical_id LIMIT ?6",
        )?;
        let candidates = statement
            .query_map(
                params![
                    checkpoint.workspace_id.as_str(),
                    checkpoint.grace_before,
                    cursor_kind,
                    cursor_id,
                    checkpoint.generation,
                    sql_limit(Some(max_records)),
                ],
                |row| {
                    Ok((
                        MetadataRecordRef {
                            kind: MetadataRecordKind::from_str(&row.get::<_, String>(0)?)?,
                            logical_id: MetadataLogicalId::new(row.get::<_, String>(1)?),
                        },
                        row.get::<_, Option<String>>(2)?.map(MetadataObjectKey::new),
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let candidates = candidates
            .into_iter()
            .map(|(record, object_key)| {
                let cache = gc_cache_record(self, &checkpoint.workspace_id, &record)?;
                Ok((record, object_key, cache))
            })
            .collect::<Result<Vec<_>, MetadataError>>()?;
        if candidates.is_empty() {
            self.connection.execute(
                "UPDATE metadata_gc_checkpoints SET phase = 'complete', updated_at = ?2
                 WHERE workspace_id = ?1 AND generation = ?3",
                params![checkpoint.workspace_id.as_str(), now, checkpoint.generation,],
            )?;
            return Ok(MetadataGcBatchReport {
                generation: checkpoint.generation.clone(),
                phase: MetadataGcPhase::Complete,
                records_processed: 0,
                records_marked: 0,
                delete_candidates: Vec::new(),
                cache_files_deleted: 0,
                cache_bytes_deleted: 0,
                metadata_records_deleted: 0,
                complete: true,
            });
        }
        let delete_candidates = candidates
            .iter()
            .filter_map(|(record, object_key, cache)| {
                object_key.as_ref().map(|object_key| MetadataGcCandidate {
                    generation: checkpoint.generation.clone(),
                    record: record.clone(),
                    object_key: object_key.clone(),
                    cache_path: cache.as_ref().and_then(|cache| cache.cache_path.clone()),
                    cache_bytes: cache.as_ref().map_or(0, |cache| cache.encoded_bytes),
                })
            })
            .collect::<Vec<_>>();
        let mut cache_files_deleted = 0;
        let mut cache_bytes_deleted = 0_u64;
        let mut metadata_records_deleted = 0;
        for (record, object_key, cache) in &candidates {
            if object_key.is_some() {
                self.with_committed(|store| {
                    store.connection.execute(
                        "INSERT OR REPLACE INTO metadata_gc_queue
                         (workspace_id, generation, record_kind, logical_id, state, enqueued_at, processed_at)
                         VALUES (?1, ?2, ?3, ?4, 'delete-eligible', ?5, ?5)",
                        params![
                            checkpoint.workspace_id.as_str(),
                            checkpoint.generation,
                            record.kind.as_str(),
                            record.logical_id.as_str(),
                            now,
                        ],
                    )?;
                    advance_sweep_cursor(store, checkpoint, record, now)
                })?;
                continue;
            }
            let plan = self.with_committed(|store| {
                begin_cache_delete(
                    store,
                    &checkpoint.workspace_id,
                    record,
                    cache.as_ref().and_then(|cache| cache.cache_path.as_deref()),
                    cache.as_ref().map_or(0, |cache| cache.encoded_bytes),
                )
            })?;
            let cache_delete = execute_cache_delete(plan)?;
            let deleted = self.with_committed(|store| {
                finish_cache_delete(
                    store,
                    &checkpoint.workspace_id,
                    record,
                    cache.as_ref().and_then(|cache| cache.cache_path.as_deref()),
                    cache.as_ref().map_or(0, |cache| cache.encoded_bytes),
                )?;
                let deleted = store.connection.execute(
                    "DELETE FROM metadata_records
                     WHERE workspace_id = ?1 AND record_kind = ?2 AND logical_id = ?3
                       AND NOT EXISTS (
                         SELECT 1 FROM metadata_object_bindings AS bindings
                         WHERE bindings.workspace_id = metadata_records.workspace_id
                           AND bindings.record_kind = metadata_records.record_kind
                           AND bindings.logical_id = metadata_records.logical_id
                       )",
                    params![
                        checkpoint.workspace_id.as_str(),
                        record.kind.as_str(),
                        record.logical_id.as_str(),
                    ],
                )? as u64;
                advance_sweep_cursor(store, checkpoint, record, now)?;
                Ok::<_, MetadataError>(deleted)
            })?;
            cache_files_deleted += u64::from(cache_delete.file_deleted);
            cache_bytes_deleted = cache_bytes_deleted.saturating_add(cache_delete.bytes_deleted);
            metadata_records_deleted += deleted;
        }
        Ok(MetadataGcBatchReport {
            generation: checkpoint.generation.clone(),
            phase: MetadataGcPhase::Sweep,
            records_processed: candidates.len() as u64,
            records_marked: 0,
            delete_candidates,
            cache_files_deleted,
            cache_bytes_deleted,
            metadata_records_deleted,
            complete: false,
        })
    }
}

fn require_top_level_gc_transaction(store: &MetadataStore) -> Result<(), MetadataError> {
    if store.connection.is_autocommit() {
        return Ok(());
    }
    Err(MetadataError::InvalidStorageMetadata(
        "metadata GC filesystem finalization requires a top-level transaction".to_string(),
    ))
}

fn checkpoint_from_row(row: &rusqlite::Row<'_>) -> Result<MetadataGcCheckpoint, rusqlite::Error> {
    let cursor_kind = row.get::<_, Option<String>>(3)?;
    let cursor_id = row.get::<_, Option<String>>(4)?;
    let sweep_cursor = match (cursor_kind, cursor_id) {
        (Some(kind), Some(id)) => Some(MetadataRecordRef {
            kind: MetadataRecordKind::from_str(&kind)?,
            logical_id: MetadataLogicalId::new(id),
        }),
        (None, None) => None,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "partial metadata GC cursor",
                )),
            ));
        }
    };
    Ok(MetadataGcCheckpoint {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        generation: row.get(1)?,
        phase: MetadataGcPhase::from_str(&row.get::<_, String>(2)?)?,
        sweep_cursor,
        grace_before: row.get(5)?,
        started_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn gc_queue_records(
    connection: &Connection,
    workspace_id: &WorkspaceId,
    generation: &str,
    state: &str,
    limit: u64,
) -> Result<Vec<MetadataRecordRef>, MetadataError> {
    let mut statement = connection.prepare(
        "SELECT record_kind, logical_id FROM metadata_gc_queue
         WHERE workspace_id = ?1 AND generation = ?2 AND state = ?3
         ORDER BY record_kind, logical_id LIMIT ?4",
    )?;
    let rows = statement.query_map(
        params![
            workspace_id.as_str(),
            generation,
            state,
            sql_limit(Some(limit)),
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
