use super::*;

impl MetadataStore {
    pub fn upsert_workspace_sync_head(
        &self,
        record: &WorkspaceSyncHeadRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(
            &record.workspace_ref.workspace_id,
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
               observed_at = excluded.observed_at
             WHERE excluded.version > workspace_sync_heads.version
                OR (
                  excluded.version = workspace_sync_heads.version
                  AND excluded.snapshot_id = workspace_sync_heads.snapshot_id
                )",
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
                        workspace_ref: ControlPlaneWorkspaceRef {
                            workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
                            version: row.get::<_, u64>(1)?,
                            snapshot_id: SnapshotId::new(row.get::<_, String>(2)?),
                            updated_at: bowline_control_plane::ControlPlaneTimestamp {
                                tick: row.get::<_, u64>(3)?,
                            },
                            updated_by_device_id: row.get::<_, Option<String>>(4)?.map(DeviceId::new),
                        },
                        observed_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
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
            Ok((
                deserialize_json_variant::<SyncOperationState>(row.get::<_, String>(0)?)?,
                row.get::<_, u64>(1)?,
            ))
        })?;
        for row in rows {
            let (state, count) = row?;
            set_sync_operation_count(&mut counts, state, count);
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
        let rows =
            statement.query_map(params![workspace_id.as_str(), device_id.as_str()], |row| {
                Ok((
                    deserialize_json_variant::<SyncOperationState>(row.get::<_, String>(0)?)?,
                    row.get::<_, u64>(1)?,
                ))
            })?;
        for row in rows {
            let (state, count) = row?;
            set_sync_operation_count(&mut counts, state, count);
        }
        Ok(counts)
    }

    pub fn sync_operation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
                        next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE id = ?1",
                [id],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn next_sync_operation_deadline(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT MIN(deadline) FROM (
                   SELECT next_attempt_at AS deadline
                   FROM sync_operations
                   WHERE workspace_id = ?1
                     AND state IN ('waiting_retry', 'blocked_offline')
                     AND next_attempt_at IS NOT NULL
                   UNION ALL
                   SELECT lease_expires_at AS deadline
                   FROM sync_operations
                   WHERE workspace_id = ?1
                     AND state = 'claimed'
                     AND lease_expires_at IS NOT NULL
                 )",
                [workspace_id.as_str()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }
}

fn set_sync_operation_count(
    counts: &mut SyncOperationCounts,
    state: SyncOperationState,
    count: u64,
) {
    match state {
        SyncOperationState::Queued => counts.queued = count,
        SyncOperationState::Claimed => counts.claimed = count,
        SyncOperationState::WaitingRetry => counts.waiting_retry = count,
        SyncOperationState::BlockedOffline => counts.blocked_offline = count,
        SyncOperationState::ReconciliationRequired => counts.reconciliation_required = count,
        SyncOperationState::Attention => counts.attention = count,
        SyncOperationState::Completed => counts.completed = count,
        SyncOperationState::Cancelled => counts.cancelled = count,
    }
}
