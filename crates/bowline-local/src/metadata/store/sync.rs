use super::common::*;
use super::*;

impl MetadataStore {
    pub fn upsert_workspace_sync_head(
        &self,
        record: &WorkspaceSyncHeadRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(
            &WorkspaceId::new(record.workspace_ref.workspace_id.clone()),
            "Code",
            &record.observed_at,
        )?;
        self.connection.execute(
            "INSERT INTO workspace_sync_heads
             (workspace_id, version, snapshot_id, updated_at_tick, updated_by_device_id, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(workspace_id) DO UPDATE SET
               version = excluded.version,
               snapshot_id = excluded.snapshot_id,
               updated_at_tick = excluded.updated_at_tick,
               updated_by_device_id = excluded.updated_by_device_id,
               observed_at = excluded.observed_at",
            params![
                record.workspace_ref.workspace_id.as_str(),
                record.workspace_ref.version,
                record.workspace_ref.snapshot_id.as_str(),
                record.workspace_ref.updated_at.tick,
                record.workspace_ref.updated_by_device_id.as_deref(),
                record.observed_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn workspace_sync_head(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceSyncHeadRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, version, snapshot_id, updated_at_tick, updated_by_device_id, observed_at
                 FROM workspace_sync_heads
                 WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                |row| {
                    Ok(WorkspaceSyncHeadRecord {
                        workspace_ref: WorkspaceRef {
                            workspace_id: row.get(0)?,
                            version: row.get::<_, u64>(1)?,
                            snapshot_id: row.get(2)?,
                            updated_at: bowline_control_plane::ControlPlaneTimestamp {
                                tick: row.get::<_, u64>(3)?,
                            },
                            updated_by_device_id: row.get(4)?,
                        },
                        observed_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn enqueue_sync_operation(
        &self,
        record: &SyncOperationRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(&record.workspace_id, "Code", &record.updated_at)?;
        self.connection.execute(
            "INSERT INTO sync_operations
             (id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
              target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
              heartbeat_at, next_attempt_at, last_error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(workspace_id, idempotency_key) DO UPDATE SET
               kind = excluded.kind,
               state = CASE
                 WHEN sync_operations.state = 'completed' THEN sync_operations.state
                 ELSE excluded.state
               END,
               base_version = excluded.base_version,
               base_snapshot_id = excluded.base_snapshot_id,
               target_snapshot_id = excluded.target_snapshot_id,
               device_id = excluded.device_id,
               payload_json = excluded.payload_json,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.kind.as_str(),
                record.state.as_str(),
                record.idempotency_key.as_str(),
                record.base_version,
                record.base_snapshot_id.as_deref(),
                record.target_snapshot_id.as_deref(),
                record.device_id.as_ref().map(|id| id.as_str()),
                record.payload_json.as_str(),
                record.attempt_count,
                record.claimed_by.as_deref(),
                record.heartbeat_at.as_deref(),
                record.next_attempt_at.as_deref(),
                record.last_error.as_deref(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn sync_operations(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SyncOperationRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
                    target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                    heartbeat_at, next_attempt_at, last_error, created_at, updated_at
             FROM sync_operations
             WHERE workspace_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], sync_operation_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn active_sync_operation_for_device(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        heartbeat_at, next_attempt_at, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE workspace_id = ?1
                   AND kind = ?2
                   AND device_id = ?3
                   AND state != 'completed'
                 ORDER BY created_at, id
                 LIMIT 1",
                params![workspace_id.as_str(), kind, device_id.as_str()],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn claim_next_sync_operation(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        now: &str,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        let Some(id) = self
            .connection
            .query_row(
                "SELECT id FROM sync_operations
                 WHERE workspace_id = ?1
                   AND (
                     state = 'queued'
                     OR (state = 'waiting_retry' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                     OR (state = 'blocked_offline' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                   )
                 ORDER BY created_at, id
                 LIMIT 1",
                params![workspace_id.as_str(), now],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };

        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'claimed',
                 claimed_by = ?1,
                 heartbeat_at = ?2,
                 attempt_count = attempt_count + 1,
                 updated_at = ?2
             WHERE id = ?3
               AND workspace_id = ?4
               AND (
                 state = 'queued'
                 OR (state = 'waiting_retry' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                 OR (state = 'blocked_offline' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
               )",
            params![claimant, now, id, workspace_id.as_str()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.sync_operation_by_id(&id)
    }

    pub fn refresh_sync_operation_heartbeat(
        &self,
        id: &str,
        claimant: &str,
        now: &str,
    ) -> Result<bool, MetadataError> {
        Ok(self.connection.execute(
            "UPDATE sync_operations
             SET heartbeat_at = ?3, updated_at = ?3
             WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2",
            params![id, claimant, now],
        )? > 0)
    }

    pub fn complete_sync_operation(
        &self,
        id: &str,
        completion_payload_json: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'completed',
                 payload_json = ?2,
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 last_error = NULL,
                 updated_at = ?3
             WHERE id = ?1",
            params![id, completion_payload_json, now],
        )?;
        Ok(())
    }

    pub fn fail_sync_operation_for_retry(
        &self,
        id: &str,
        message: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'waiting_retry',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = ?3,
                 last_error = ?2,
                 updated_at = ?4
             WHERE id = ?1",
            params![id, message, next_attempt_at, now],
        )?;
        Ok(())
    }

    pub fn block_sync_operation_offline(
        &self,
        id: &str,
        message: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'blocked_offline',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = ?3,
                 last_error = ?2,
                 updated_at = ?4
             WHERE id = ?1",
            params![id, message, next_attempt_at, now],
        )?;
        Ok(())
    }

    pub fn mark_sync_operation_attention(
        &self,
        id: &str,
        message: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'attention',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 last_error = ?2,
                 updated_at = ?3
             WHERE id = ?1",
            params![id, message, now],
        )?;
        Ok(())
    }

    pub fn complete_obsolete_daemon_reconciles_for_device(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        completion_payload_json: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'completed',
                 payload_json = ?3,
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 last_error = NULL,
                 updated_at = ?4
             WHERE workspace_id = ?1
               AND device_id = ?2
               AND kind = 'daemon-reconcile'
               AND state IN ('waiting_retry', 'blocked_offline', 'attention')",
            params![
                workspace_id.as_str(),
                device_id.as_str(),
                completion_payload_json,
                now,
            ],
        )?;
        Ok(changed as u64)
    }

    pub fn append_sync_operation_checkpoint(
        &self,
        record: &SyncOperationCheckpointRecord,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO sync_operation_checkpoints
             (id, workspace_id, operation_id, step, state, payload_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
               state = excluded.state,
               payload_json = excluded.payload_json,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.operation_id.as_str(),
                record.step.as_str(),
                record.state.as_str(),
                record.payload_json.as_str(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn sync_operation_checkpoints(
        &self,
        operation_id: &str,
    ) -> Result<Vec<SyncOperationCheckpointRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, operation_id, step, state, payload_json, created_at, updated_at
             FROM sync_operation_checkpoints
             WHERE operation_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([operation_id], sync_operation_checkpoint_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn requeue_expired_sync_claims(
        &self,
        workspace_id: &WorkspaceId,
        expired_before: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 updated_at = ?3
             WHERE workspace_id = ?1
               AND state = 'claimed'
               AND heartbeat_at < ?2",
            params![workspace_id.as_str(), expired_before, now],
        )?;
        Ok(changed as u64)
    }

    pub fn requeue_claimed_sync_operations_for_device_kind(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 updated_at = ?4
             WHERE workspace_id = ?1
               AND kind = ?2
               AND device_id = ?3
               AND state = 'claimed'",
            params![workspace_id.as_str(), kind, device_id.as_str(), now],
        )?;
        Ok(changed as u64)
    }

    pub fn requeue_waiting_retry_sync_operations_for_device_kind(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 updated_at = ?4
             WHERE workspace_id = ?1
               AND kind = ?2
               AND device_id = ?3
               AND state = 'waiting_retry'",
            params![workspace_id.as_str(), kind, device_id.as_str(), now],
        )?;
        Ok(changed as u64)
    }

    pub fn requeue_attention_sync_operations_for_device_kind_with_error(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
        error_substring: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 last_error = NULL,
                 updated_at = ?5
             WHERE workspace_id = ?1
               AND kind = ?2
               AND device_id = ?3
               AND state = 'attention'
               AND last_error LIKE ?4",
            params![
                workspace_id.as_str(),
                kind,
                device_id.as_str(),
                format!("%{error_substring}%"),
                now,
            ],
        )?;
        Ok(changed as u64)
    }

    pub fn put_remote_ref_cursor(
        &self,
        record: &RemoteRefCursorRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(&record.workspace_id, "Code", &record.updated_at)?;
        self.connection.execute(
            "INSERT INTO sync_remote_cursors
             (workspace_id, cursor, last_observed_version, last_observed_snapshot_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(workspace_id) DO UPDATE SET
               cursor = excluded.cursor,
               last_observed_version = excluded.last_observed_version,
               last_observed_snapshot_id = excluded.last_observed_snapshot_id,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                record.cursor.as_deref(),
                record.last_observed_version,
                record.last_observed_snapshot_id.as_deref(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn remote_ref_cursor(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<RemoteRefCursorRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, cursor, last_observed_version, last_observed_snapshot_id, updated_at
                 FROM sync_remote_cursors
                 WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                |row| {
                    Ok(RemoteRefCursorRecord {
                        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
                        cursor: row.get(1)?,
                        last_observed_version: row.get(2)?,
                        last_observed_snapshot_id: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn sync_operation_counts(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<SyncOperationCounts, MetadataError> {
        let mut counts = SyncOperationCounts::default();
        let mut statement = self.connection.prepare(
            "SELECT state, COUNT(*)
             FROM sync_operations
             WHERE workspace_id = ?1
             GROUP BY state",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (state, count) = row?;
            match state.as_str() {
                "queued" => counts.queued = count,
                "claimed" => counts.claimed = count,
                "waiting_retry" => counts.waiting_retry = count,
                "blocked_offline" => counts.blocked_offline = count,
                "attention" => counts.attention = count,
                "completed" => counts.completed = count,
                _ => {}
            }
        }
        Ok(counts)
    }

    pub fn sync_operation_counts_for_device(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> Result<SyncOperationCounts, MetadataError> {
        let mut counts = SyncOperationCounts::default();
        let mut statement = self.connection.prepare(
            "SELECT state, COUNT(*)
             FROM sync_operations
             WHERE workspace_id = ?1 AND device_id = ?2
             GROUP BY state",
        )?;
        let rows = statement
            .query_map(params![workspace_id.as_str(), device_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?;
        for row in rows {
            let (state, count) = row?;
            match state.as_str() {
                "queued" => counts.queued = count,
                "claimed" => counts.claimed = count,
                "waiting_retry" => counts.waiting_retry = count,
                "blocked_offline" => counts.blocked_offline = count,
                "attention" => counts.attention = count,
                "completed" => counts.completed = count,
                _ => {}
            }
        }
        Ok(counts)
    }

    pub fn sync_operation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        heartbeat_at, next_attempt_at, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE id = ?1",
                [id],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}
