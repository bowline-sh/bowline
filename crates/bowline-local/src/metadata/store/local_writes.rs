use super::common::*;
use super::*;
use rusqlite::params_from_iter;

impl MetadataStore {
    pub fn replace_observed_paths(
        &mut self,
        workspace_id: &WorkspaceId,
        paths: &[ObservedLocalPath],
        now: &str,
    ) -> Result<(), MetadataError> {
        self.with_committed(|store| {
            store.replace_observed_paths_uncommitted(workspace_id, paths, now)
        })
    }

    pub(crate) fn replace_observed_paths_uncommitted(
        &self,
        workspace_id: &WorkspaceId,
        paths: &[ObservedLocalPath],
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "DELETE FROM local_paths WHERE workspace_id = ?1",
            [workspace_id.as_str()],
        )?;

        let mut statement = self.connection.prepare(
            "INSERT INTO local_paths
             (id, workspace_id, project_id, path, classification, mode, access_json,
              updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for path in paths {
            let id = local_path_id(workspace_id, &path.path);
            statement.execute(params![
                id,
                workspace_id.as_str(),
                path.project_id.as_ref().map(|id| id.as_str()),
                path.path,
                serialize_json_variant(&path.classification)?,
                serialize_json_variant(&path.mode)?,
                serde_json::to_string(&path.access)
                    .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
                now,
            ])?;
        }

        Ok(())
    }

    pub fn observed_path(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<Option<LocalPathRecord>, MetadataError> {
        let path = self.workspace_relative_path(workspace_id, path)?;
        let id = local_path_id(workspace_id, &path);
        self.connection
            .query_row(
                "SELECT path, classification, mode, access_json
                 FROM local_paths
                 WHERE id = ?1
                 LIMIT 1",
                params![id],
                |row| {
                    Ok(LocalPathRecord {
                        path: row.get(0)?,
                        classification: deserialize_json_variant(row.get::<_, String>(1)?)?,
                        mode: deserialize_json_variant(row.get::<_, String>(2)?)?,
                        access: serde_json::from_str(&row.get::<_, String>(3)?)
                            .map_err(json_to_sql_read_error)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn blocked_and_local_only_observed_paths(
        &self,
        workspace_id: &WorkspaceId,
        project_scope: Option<&ProjectId>,
    ) -> Result<Vec<ObservedLocalPath>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT project_id, path, classification, mode, access_json
             FROM local_paths
             WHERE workspace_id = ?1
               AND (?2 IS NULL OR project_id = ?2)
               AND (
                 classification IN ('blocked', 'local-only')
                 OR mode IN ('blocked', 'local-only', 'ignore')
               )
               AND path <> '.git'
               AND path NOT LIKE '%/.git'
               AND path NOT LIKE '.git/%'
               AND path NOT LIKE '%/.git/%'
             ORDER BY
               CASE
                 WHEN classification = 'blocked' OR mode = 'blocked' THEN 0
                 ELSE 1
               END,
               path",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_scope.map(ProjectId::as_str),],
            |row| {
                Ok(ObservedLocalPath {
                    project_id: row.get::<_, Option<String>>(0)?.map(ProjectId::new),
                    path: row.get(1)?,
                    classification: deserialize_json_variant(row.get::<_, String>(2)?)?,
                    mode: deserialize_json_variant(row.get::<_, String>(3)?)?,
                    access: serde_json::from_str(&row.get::<_, String>(4)?)
                        .map_err(json_to_sql_read_error)?,
                })
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn set_observed_summary(
        &self,
        workspace_id: &WorkspaceId,
        summary: &ObservedWorkspaceSummary,
        now: &str,
    ) -> Result<(), MetadataError> {
        let summary_json = serde_json::to_string(summary)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        self.connection.execute(
            "INSERT INTO indexes
             (id, workspace_id, project_id, kind, state, watermark, updated_at)
             VALUES (?1, ?2, NULL, 'scan-summary', ?3, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               state = excluded.state,
               watermark = excluded.watermark,
               updated_at = excluded.updated_at",
            params![
                scan_summary_id(workspace_id),
                workspace_id.as_str(),
                summary_json,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn observed_summary(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<ObservedWorkspaceSummary>, MetadataError> {
        let summary_json = self
            .connection
            .query_row(
                "SELECT state FROM indexes
                 WHERE workspace_id = ?1 AND kind = 'scan-summary'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT 1",
                [workspace_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        summary_json
            .map(|json| {
                serde_json::from_str(&json)
                    .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))
            })
            .transpose()
    }

    pub fn append_local_write_log(
        &self,
        record: &LocalWriteLogRecord,
    ) -> Result<(), MetadataError> {
        let path = self.workspace_relative_path(&record.workspace_id, &record.path)?;
        let source_path = record
            .source_path
            .as_deref()
            .map(|path| self.workspace_relative_path(&record.workspace_id, path))
            .transpose()?;
        self.connection.execute(
            "INSERT INTO local_write_log
             (id, workspace_id, device_id, project_id, path, source_path, operation,
              staged_content_id, policy_classification, causation_id, settled_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.device_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                path,
                source_path,
                record.operation.as_str(),
                record.staged_content_id.as_ref().map(|id| id.as_str()),
                serialize_json_variant(&record.policy_classification)?,
                record.causation_id.as_str(),
                record.settled_at.as_str(),
                record.created_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn delete_local_write(
        &self,
        workspace_id: &WorkspaceId,
        id: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "DELETE FROM local_write_log
             WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id.as_str(), id],
        )?;
        Ok(changed as u64)
    }

    pub fn local_write_log(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<LocalWriteLogRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, device_id, project_id, path, source_path, operation,
                    staged_content_id, policy_classification, causation_id, settled_at, created_at
             FROM local_write_log
             WHERE workspace_id = ?1
             ORDER BY created_at, rowid",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], local_write_log_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn local_writes_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        project_path: &str,
        since: Option<&str>,
        limit: Option<u64>,
    ) -> Result<Vec<LocalWriteLogRecord>, MetadataError> {
        let project_path = normalize_workspace_path(project_path);
        let project_prefix = format!("{}/%", escape_like(&project_path));
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, device_id, project_id, path, source_path, operation,
                    staged_content_id, policy_classification, causation_id, settled_at, created_at
             FROM (
                 SELECT rowid AS local_rowid, id, workspace_id, device_id, project_id, path,
                        source_path, operation, staged_content_id, policy_classification,
                        causation_id, settled_at, created_at
                 FROM local_write_log
                 WHERE workspace_id = ?1
                   AND (project_id = ?2 OR path = ?3 OR path LIKE ?4 ESCAPE '\\')
                   AND (?5 IS NULL OR created_at > ?5)
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT ?6
             )
             ORDER BY created_at, local_rowid",
        )?;
        let rows = statement.query_map(
            params![
                workspace_id.as_str(),
                project_id.as_str(),
                project_path,
                project_prefix,
                since,
                sql_limit(limit),
            ],
            local_write_log_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn local_writes_for_causation_ids(
        &self,
        workspace_id: &WorkspaceId,
        causation_ids: &[String],
    ) -> Result<Vec<LocalWriteLogRecord>, MetadataError> {
        if causation_ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = vec!["?"; causation_ids.len()].join(", ");
        let sql = format!(
            "SELECT id, workspace_id, device_id, project_id, path, source_path, operation,
                    staged_content_id, policy_classification, causation_id, settled_at, created_at
             FROM local_write_log
             WHERE workspace_id = ? AND causation_id IN ({placeholders})
             ORDER BY created_at, rowid"
        );
        let mut params = Vec::with_capacity(causation_ids.len() + 1);
        params.push(workspace_id.as_str().to_string());
        params.extend(causation_ids.iter().cloned());
        let mut statement = self.connection.prepare(&sql)?;
        let rows =
            statement.query_map(params_from_iter(params.iter()), local_write_log_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn local_writes_for_path_prefix(
        &self,
        workspace_id: &WorkspaceId,
        path_prefix: &str,
    ) -> Result<Vec<LocalWriteLogRecord>, MetadataError> {
        let path_prefix =
            normalize_workspace_path(&self.workspace_relative_path(workspace_id, path_prefix)?);
        let like_prefix = format!("{}/%", escape_like(&path_prefix));
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, device_id, project_id, path, source_path, operation,
                    staged_content_id, policy_classification, causation_id, settled_at, created_at
             FROM local_write_log
             WHERE workspace_id = ?1
               AND (path = ?2 OR path LIKE ?3 ESCAPE '\\')
             ORDER BY created_at, rowid",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), path_prefix, like_prefix],
            local_write_log_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn local_write_by_id(
        &self,
        workspace_id: &WorkspaceId,
        write_id: &str,
    ) -> Result<Option<LocalWriteLogRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, device_id, project_id, path, source_path, operation,
                        staged_content_id, policy_classification, causation_id, settled_at, created_at
                 FROM local_write_log
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), write_id],
                local_write_log_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn latest_write_for_path(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path: &str,
    ) -> Result<Option<LocalWriteLogRecord>, MetadataError> {
        let path = normalize_workspace_path(&self.workspace_relative_path(workspace_id, path)?);
        self.connection
            .query_row(
                "SELECT id, workspace_id, device_id, project_id, path, source_path, operation,
                        staged_content_id, policy_classification, causation_id, settled_at, created_at
                 FROM local_write_log
	                 WHERE workspace_id = ?1
	                   AND project_id = ?2
	                   AND path = ?3
	                 ORDER BY created_at DESC, rowid DESC
	                 LIMIT 1",
                params![workspace_id.as_str(), project_id.as_str(), path],
                local_write_log_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn has_local_write_after_device(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        completed_at: &str,
    ) -> Result<bool, MetadataError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM local_write_log
                   WHERE workspace_id = ?1
                     AND device_id = ?2
                     AND created_at > ?3
                 )",
                params![workspace_id.as_str(), device_id.as_str(), completed_at],
                |row| row.get::<_, bool>(0),
            )
            .map_err(Into::into)
    }
}

fn local_write_log_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<LocalWriteLogRecord, rusqlite::Error> {
    Ok(LocalWriteLogRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        device_id: DeviceId::new(row.get::<_, String>(2)?),
        project_id: row.get::<_, Option<String>>(3)?.map(ProjectId::new),
        path: row.get(4)?,
        source_path: row.get(5)?,
        operation: row.get(6)?,
        staged_content_id: row.get::<_, Option<String>>(7)?.map(ContentId::new),
        policy_classification: deserialize_json_variant(row.get::<_, String>(8)?)?,
        causation_id: row.get(9)?,
        settled_at: row.get(10)?,
        created_at: row.get(11)?,
    })
}
