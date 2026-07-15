use super::common::*;
use super::*;

impl MetadataStore {
    pub fn reconcile_materialization_tasks(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
        desired: &[MaterializationTaskRecord],
        now: &str,
    ) -> Result<MaterializationReconcileReport, MetadataError> {
        self.in_immediate_transaction(|| {
            self.connection.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS desired_materialization_task_ids (
                   id TEXT PRIMARY KEY
                 );
                 DELETE FROM desired_materialization_task_ids;",
            )?;

            let mut report = MaterializationReconcileReport::default();
            for task in desired {
                if task.workspace_id != *workspace_id || task.snapshot_id != *snapshot_id {
                    return Err(MetadataError::InvalidStorageMetadata(
                        "materialization reconcile task crossed its workspace or snapshot fence"
                            .to_string(),
                    ));
                }
                validate_materialization_kind(task.expected_kind)?;
                self.connection.execute(
                    "INSERT INTO desired_materialization_task_ids (id) VALUES (?1)",
                    [task.id.as_str()],
                )?;

                let inserted = self.connection.execute(
                    "INSERT OR IGNORE INTO materialization_tasks (
                       id, workspace_id, project_id, snapshot_id, path, expected_kind,
                       expected_content_id, expected_byte_len, expected_executable,
                       priority_class, state, attempt_count, claim_generation, not_before,
                       claim_token, claimed_by, claimed_at, lease_expires_at, last_error_kind,
                       last_error, created_at, updated_at
                     ) VALUES (
                       ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                       'queued', 0, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, ?11, ?11
                     )",
                    params![
                        task.id.as_str(),
                        workspace_id.as_str(),
                        task.project_id.as_ref().map(ProjectId::as_str),
                        snapshot_id.as_str(),
                        task.path.as_str(),
                        serialize_json_variant(&task.expected_kind)?,
                        task.expected_content_id.as_ref().map(ContentId::as_str),
                        task.expected_byte_len,
                        task.expected_executable,
                        task.priority_class.as_str(),
                        now,
                    ],
                )?;
                report.inserted += inserted as u64;

                let reprioritized = self.connection.execute(
                    "UPDATE materialization_tasks
                     SET project_id = ?2,
                         expected_kind = ?3,
                         expected_byte_len = ?4,
                         expected_executable = ?5,
                         priority_class = ?6,
                         updated_at = ?7
                     WHERE id = ?1
                       AND workspace_id = ?8
                       AND snapshot_id = ?9
                       AND path = ?10
                       AND COALESCE(expected_content_id, '') = COALESCE(?11, '')
                       AND priority_class != ?6",
                    params![
                        task.id.as_str(),
                        task.project_id.as_ref().map(ProjectId::as_str),
                        serialize_json_variant(&task.expected_kind)?,
                        task.expected_byte_len,
                        task.expected_executable,
                        task.priority_class.as_str(),
                        now,
                        workspace_id.as_str(),
                        snapshot_id.as_str(),
                        task.path.as_str(),
                        task.expected_content_id.as_ref().map(ContentId::as_str),
                    ],
                )?;
                report.reprioritized += reprioritized as u64;
            }

            let cancelled = self.connection.execute(
                "UPDATE materialization_tasks
                 SET state = 'cancelled',
                     claim_token = NULL,
                     claimed_by = NULL,
                     claimed_at = NULL,
                     lease_expires_at = NULL,
                     not_before = NULL,
                     updated_at = ?3
                 WHERE workspace_id = ?1
                   AND state != 'cancelled'
                   AND (
                     snapshot_id != ?2
                     OR id NOT IN (SELECT id FROM desired_materialization_task_ids)
                   )",
                params![workspace_id.as_str(), snapshot_id.as_str(), now],
            )?;
            report.cancelled = cancelled as u64;
            self.connection
                .execute("DELETE FROM desired_materialization_task_ids", [])?;
            Ok(report)
        })
    }

    pub fn materialization_tasks_for_snapshot(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<Vec<MaterializationTaskRecord>, MetadataError> {
        let mut statement = self.connection.prepare(&format!(
            "{} WHERE workspace_id = ?1 AND snapshot_id = ?2 ORDER BY path, id",
            MATERIALIZATION_TASK_SELECT
        ))?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), snapshot_id.as_str()],
            materialization_task_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn has_pending_materialization_retry(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<bool, MetadataError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                   SELECT 1
                   FROM materialization_tasks
                   WHERE workspace_id = ?1
                     AND snapshot_id = ?2
                     AND state IN ('waiting-retry', 'blocked-offline')
                 )",
                params![workspace_id.as_str(), snapshot_id.as_str()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn materialization_tasks(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<MaterializationTaskRecord>, MetadataError> {
        let mut statement = self.connection.prepare(&format!(
            "{} WHERE workspace_id = ?1 AND state != 'cancelled' ORDER BY snapshot_id, path, id",
            MATERIALIZATION_TASK_SELECT
        ))?;
        let rows = statement.query_map([workspace_id.as_str()], materialization_task_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn materialization_task(
        &self,
        id: &MaterializationTaskId,
    ) -> Result<Option<MaterializationTaskRecord>, MetadataError> {
        self.connection
            .query_row(
                &format!("{} WHERE id = ?1", MATERIALIZATION_TASK_SELECT),
                [id.as_str()],
                materialization_task_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn claim_next_materialization_task(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        claim_token: &str,
        now: &str,
    ) -> Result<Option<MaterializationTaskRecord>, MetadataError> {
        let lease_expires_at = materialization_lease_expiry(now)?;
        self.in_immediate_transaction(|| {
            let id = self
                .connection
                .query_row(
                    "SELECT id
                     FROM materialization_tasks
                     WHERE workspace_id = ?1
                       AND (
                         state = 'queued'
                         OR (state = 'waiting-retry' AND (not_before IS NULL OR not_before <= ?2))
                         OR (state = 'blocked-offline' AND (not_before IS NULL OR not_before <= ?2))
                         OR (
                           state = 'claimed'
                           AND julianday(lease_expires_at) <= julianday(?2)
                         )
                       )
                     ORDER BY
                       MAX(
                         0,
                         CASE priority_class
                           WHEN 'correctness-critical' THEN 0
                           WHEN 'active-project' THEN 1
                           WHEN 'requested-path' THEN 2
                           WHEN 'recent-project' THEN 3
                           WHEN 'small-file' THEN 4
                           WHEN 'background-large' THEN 5
                           ELSE 6
                         END - CAST(MAX(0, julianday(?2) - julianday(created_at)) AS INTEGER)
                       ),
                       CASE WHEN priority_class = 'cleanup' THEN -LENGTH(path) ELSE 0 END,
                       expected_byte_len,
                       created_at,
                       id
                     LIMIT 1",
                    params![workspace_id.as_str(), now],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(id) = id else {
                return Ok(None);
            };
            let changed = self.connection.execute(
                "UPDATE materialization_tasks
                 SET state = 'claimed',
                     attempt_count = attempt_count + 1,
                     claim_generation = claim_generation + 1,
                     claim_token = ?2,
                     claimed_by = ?3,
                     claimed_at = ?4,
                     lease_expires_at = ?5,
                     not_before = NULL,
                     last_error_kind = NULL,
                     last_error = NULL,
                     updated_at = ?4
                 WHERE id = ?1
                   AND workspace_id = ?6
                   AND (
                     state = 'queued'
                     OR (state = 'waiting-retry' AND (not_before IS NULL OR not_before <= ?4))
                     OR (state = 'blocked-offline' AND (not_before IS NULL OR not_before <= ?4))
                     OR (
                       state = 'claimed'
                       AND julianday(lease_expires_at) <= julianday(?4)
                     )
                   )",
                params![
                    id,
                    claim_token,
                    claimant,
                    now,
                    lease_expires_at,
                    workspace_id.as_str()
                ],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            self.materialization_task(&MaterializationTaskId::new(id))
        })
    }

    pub fn renew_materialization_task_claim(
        &self,
        id: &MaterializationTaskId,
        claim_token: &str,
        claim_generation: u64,
        now: &str,
    ) -> Result<bool, MetadataError> {
        let lease_expires_at = materialization_lease_expiry(now)?;
        Ok(self.connection.execute(
            "UPDATE materialization_tasks
             SET lease_expires_at = ?5,
                 updated_at = ?4
             WHERE id = ?1
               AND state = 'claimed'
               AND claim_token = ?2
               AND claim_generation = ?3
               AND julianday(lease_expires_at) > julianday(?4)",
            params![
                id.as_str(),
                claim_token,
                claim_generation,
                now,
                lease_expires_at
            ],
        )? > 0)
    }

    pub fn materialization_task_fence_is_current(
        &self,
        fence: &MaterializationTaskFence<'_>,
    ) -> Result<bool, MetadataError> {
        parse_materialization_timestamp(fence.now)?;
        if unresolved_conflict_blocks_materialization(
            fence.path,
            fence.expected_kind,
            fence.unresolved_conflict_paths,
        ) {
            return Ok(false);
        }
        let expected_kind = serialize_json_variant(&fence.expected_kind)?;
        self.connection
            .query_row(
                "SELECT EXISTS(
                   SELECT 1
                   FROM materialization_tasks AS task
                   WHERE task.id = ?1
                     AND task.state = 'claimed'
                     AND task.claim_token = ?2
                     AND task.claim_generation = ?3
                     AND task.snapshot_id = ?4
                     AND task.path = ?5
                     AND task.expected_kind = ?6
                     AND COALESCE(task.expected_content_id, '') = COALESCE(?7, '')
                     AND julianday(task.lease_expires_at) > julianday(?8)
                     AND NOT EXISTS (
                       SELECT 1 FROM local_write_log
                       WHERE local_write_log.workspace_id = task.workspace_id
                         AND (
                           local_write_log.path = task.path
                           OR (
                             LENGTH(local_write_log.path) < LENGTH(task.path)
                             AND SUBSTR(task.path, 1, LENGTH(local_write_log.path)) = local_write_log.path
                             AND SUBSTR(task.path, LENGTH(local_write_log.path) + 1, 1) = '/'
                           )
                           OR (
                             task.expected_kind = 'tombstone'
                             AND LENGTH(task.path) < LENGTH(local_write_log.path)
                             AND SUBSTR(local_write_log.path, 1, LENGTH(task.path)) = task.path
                             AND SUBSTR(local_write_log.path, LENGTH(task.path) + 1, 1) = '/'
                           )
                         )
                         AND (
                           local_write_log.settled_at = ''
                           OR local_write_log.created_at >= task.created_at
                         )
                     )
                 )",
                params![
                    fence.id.as_str(),
                    fence.claim_token,
                    fence.claim_generation,
                    fence.snapshot_id.as_str(),
                    fence.path,
                    expected_kind,
                    fence.expected_content_id.map(ContentId::as_str),
                    fence.now,
                ],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn finish_materialization_task(
        &self,
        finish: &MaterializationTaskFinish<'_>,
    ) -> Result<bool, MetadataError> {
        parse_materialization_timestamp(finish.now)?;
        if finish.state == MaterializationTaskState::Claimed {
            return Err(MetadataError::InvalidStorageMetadata(
                "a claimed materialization task must retain its active claim".to_string(),
            ));
        }
        Ok(self.connection.execute(
            "UPDATE materialization_tasks
             SET state = ?3,
                 claim_token = NULL,
                 claimed_by = NULL,
                 claimed_at = NULL,
                 lease_expires_at = NULL,
                 not_before = ?5,
                 last_error_kind = ?6,
                 last_error = ?7,
                 updated_at = ?8
             WHERE id = ?1
               AND state = 'claimed'
               AND claim_token = ?2
               AND claim_generation = ?4
               AND julianday(lease_expires_at) > julianday(?8)",
            params![
                finish.id.as_str(),
                finish.claim_token,
                finish.state.as_str(),
                finish.claim_generation,
                finish.not_before,
                finish.error_kind.map(MaterializationFailureKind::as_str),
                finish.error,
                finish.now,
            ],
        )? > 0)
    }

    pub fn complete_materialization_snapshot(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        self.in_immediate_transaction(|| {
            let completed = self.connection.execute(
                "UPDATE materialization_tasks
                 SET state = 'ready',
                     claim_token = NULL,
                     claimed_by = NULL,
                     claimed_at = NULL,
                     lease_expires_at = NULL,
                     not_before = NULL,
                     last_error_kind = NULL,
                     last_error = NULL,
                     updated_at = ?3
                 WHERE workspace_id = ?1
                   AND snapshot_id = ?2
                   AND state = 'staged'
                   AND EXISTS (
                     SELECT 1 FROM workspace_sync_heads
                     WHERE workspace_sync_heads.workspace_id = materialization_tasks.workspace_id
                       AND workspace_sync_heads.snapshot_id = materialization_tasks.snapshot_id
                   )",
                params![workspace_id.as_str(), snapshot_id.as_str(), now],
            )? as u64;
            self.connection.execute(
                "INSERT INTO materialization_path_states (
                   workspace_id, project_id, path, snapshot_id, expected_content_id, state,
                   observed_content_id, observed_byte_len, source_hydration_state,
                   verified_at, updated_at
                 )
                 SELECT
                   workspace_id, project_id, path, snapshot_id, expected_content_id, 'ready',
                   expected_content_id, expected_byte_len, NULL, ?3, ?3
                 FROM materialization_tasks
                 WHERE workspace_id = ?1
                   AND snapshot_id = ?2
                   AND state = 'ready'
                   AND EXISTS (
                     SELECT 1 FROM workspace_sync_heads
                     WHERE workspace_sync_heads.workspace_id = materialization_tasks.workspace_id
                       AND workspace_sync_heads.snapshot_id = materialization_tasks.snapshot_id
                   )
                 ON CONFLICT(workspace_id, path) DO UPDATE SET
                   project_id = excluded.project_id,
                   snapshot_id = excluded.snapshot_id,
                   expected_content_id = excluded.expected_content_id,
                   state = excluded.state,
                   observed_content_id = excluded.observed_content_id,
                   observed_byte_len = excluded.observed_byte_len,
                   source_hydration_state = NULL,
                   verified_at = excluded.verified_at,
                   updated_at = excluded.updated_at",
                params![workspace_id.as_str(), snapshot_id.as_str(), now],
            )?;
            Ok(completed)
        })
    }

    pub fn promote_ready_current_namespace_hydration(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        self.in_immediate_transaction(|| {
            Ok(self.connection.execute(
                "UPDATE current_namespace_entries
                 SET hydration_state = 'local', updated_at = ?3
                 WHERE workspace_id = ?1
                   AND snapshot_id = ?2
                   AND EXISTS (
                     SELECT 1 FROM materialization_tasks AS task
                     WHERE task.workspace_id = current_namespace_entries.workspace_id
                       AND task.snapshot_id = current_namespace_entries.snapshot_id
                       AND task.path = current_namespace_entries.path
                       AND task.state = 'ready'
                   )",
                params![workspace_id.as_str(), snapshot_id.as_str(), now],
            )? as u64)
        })
    }

    pub fn release_materialization_claims(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        Ok(self.connection.execute(
            "UPDATE materialization_tasks
             SET state = 'queued',
                 claim_token = NULL,
                 claimed_by = NULL,
                 claimed_at = NULL,
                 lease_expires_at = NULL,
                 updated_at = ?3
             WHERE workspace_id = ?1 AND state = 'claimed' AND claimed_by = ?2",
            params![workspace_id.as_str(), claimant, now],
        )? as u64)
    }

    pub fn upsert_materialization_path_state(
        &self,
        record: &MaterializationPathStateRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(&record.workspace_id, "Code", &record.updated_at)?;
        self.connection.execute(
            "INSERT INTO materialization_path_states (
               workspace_id, project_id, path, snapshot_id, expected_content_id, state,
               observed_content_id, observed_byte_len, source_hydration_state,
               verified_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(workspace_id, path) DO UPDATE SET
               project_id = excluded.project_id,
               snapshot_id = excluded.snapshot_id,
               expected_content_id = excluded.expected_content_id,
               state = excluded.state,
               observed_content_id = excluded.observed_content_id,
               observed_byte_len = excluded.observed_byte_len,
               source_hydration_state = excluded.source_hydration_state,
               verified_at = excluded.verified_at,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(ProjectId::as_str),
                record.path.as_str(),
                record.snapshot_id.as_ref().map(SnapshotId::as_str),
                record.expected_content_id.as_ref().map(ContentId::as_str),
                record.state.as_str(),
                record.observed_content_id.as_ref().map(ContentId::as_str),
                record.observed_byte_len,
                record.source_hydration_state.as_deref(),
                record.verified_at.as_deref(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn materialization_path_state(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<Option<MaterializationPathStateRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, project_id, path, snapshot_id, expected_content_id,
                        state, observed_content_id, observed_byte_len, source_hydration_state,
                        verified_at, updated_at
                 FROM materialization_path_states
                 WHERE workspace_id = ?1 AND path = ?2",
                params![workspace_id.as_str(), path],
                materialization_path_state_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

const MATERIALIZATION_TASK_SELECT: &str =
    "SELECT id, workspace_id, project_id, snapshot_id, path, expected_kind,
            expected_content_id, expected_byte_len, expected_executable, priority_class,
            state, attempt_count, claim_generation, not_before, claim_token, claimed_by,
            claimed_at, lease_expires_at, last_error_kind, last_error, created_at, updated_at
     FROM materialization_tasks";

fn materialization_lease_expiry(now: &str) -> Result<String, MetadataError> {
    let claimed_at = parse_materialization_timestamp(now)?;
    (claimed_at + time::Duration::seconds(MATERIALIZATION_TASK_LEASE_SECONDS))
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))
}

fn parse_materialization_timestamp(now: &str) -> Result<time::OffsetDateTime, MetadataError> {
    time::OffsetDateTime::parse(now, &time::format_description::well_known::Rfc3339)
        .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))
}

fn unresolved_conflict_blocks_materialization(
    task_path: &str,
    expected_kind: NamespaceEntryKind,
    unresolved_conflict_paths: &BTreeSet<String>,
) -> bool {
    unresolved_conflict_paths.iter().any(|conflict_path| {
        path_is_same_or_descendant(task_path, conflict_path)
            || (expected_kind == NamespaceEntryKind::Tombstone
                && path_is_same_or_descendant(conflict_path, task_path))
    })
}

fn path_is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_materialization_kind(kind: NamespaceEntryKind) -> Result<(), MetadataError> {
    match kind {
        NamespaceEntryKind::Directory
        | NamespaceEntryKind::File
        | NamespaceEntryKind::Symlink
        | NamespaceEntryKind::Tombstone => Ok(()),
        NamespaceEntryKind::Placeholder => Err(MetadataError::InvalidStorageMetadata(
            "placeholder entries cannot be materialization tasks".to_string(),
        )),
    }
}

fn materialization_task_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<MaterializationTaskRecord, rusqlite::Error> {
    Ok(MaterializationTaskRecord {
        id: MaterializationTaskId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        snapshot_id: SnapshotId::new(row.get::<_, String>(3)?),
        path: row.get(4)?,
        expected_kind: deserialize_json_variant(row.get::<_, String>(5)?)?,
        expected_content_id: row.get::<_, Option<String>>(6)?.map(ContentId::new),
        expected_byte_len: row.get(7)?,
        expected_executable: row.get(8)?,
        priority_class: parse_priority_class(row.get::<_, String>(9)?)?,
        state: parse_task_state(row.get::<_, String>(10)?)?,
        attempt_count: row.get(11)?,
        claim_generation: row.get(12)?,
        not_before: row.get(13)?,
        claim_token: row.get(14)?,
        claimed_by: row.get(15)?,
        claimed_at: row.get(16)?,
        lease_expires_at: row.get(17)?,
        last_error_kind: row
            .get::<_, Option<String>>(18)?
            .map(parse_materialization_failure_kind)
            .transpose()?,
        last_error: row.get(19)?,
        created_at: row.get(20)?,
        updated_at: row.get(21)?,
    })
}

fn materialization_path_state_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<MaterializationPathStateRecord, rusqlite::Error> {
    Ok(MaterializationPathStateRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        project_id: row.get::<_, Option<String>>(1)?.map(ProjectId::new),
        path: row.get(2)?,
        snapshot_id: row.get::<_, Option<String>>(3)?.map(SnapshotId::new),
        expected_content_id: row.get::<_, Option<String>>(4)?.map(ContentId::new),
        state: parse_path_state(row.get::<_, String>(5)?)?,
        observed_content_id: row.get::<_, Option<String>>(6)?.map(ContentId::new),
        observed_byte_len: row.get(7)?,
        source_hydration_state: row.get(8)?,
        verified_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn parse_priority_class(value: String) -> Result<MaterializationPriorityClass, rusqlite::Error> {
    MaterializationPriorityClass::from_wire(&value)
        .ok_or_else(|| invalid_materialization_value("priority class", value))
}

fn parse_task_state(value: String) -> Result<MaterializationTaskState, rusqlite::Error> {
    MaterializationTaskState::from_wire(&value)
        .ok_or_else(|| invalid_materialization_value("task state", value))
}

fn parse_materialization_failure_kind(
    value: String,
) -> Result<MaterializationFailureKind, rusqlite::Error> {
    MaterializationFailureKind::from_wire(&value)
        .ok_or_else(|| invalid_materialization_value("failure kind", value))
}

fn parse_path_state(value: String) -> Result<MaterializationPathState, rusqlite::Error> {
    MaterializationPathState::from_wire(&value)
        .ok_or_else(|| invalid_materialization_value("path state", value))
}

fn invalid_materialization_value(field: &'static str, value: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unknown materialization {field} `{value}`").into(),
    )
}
