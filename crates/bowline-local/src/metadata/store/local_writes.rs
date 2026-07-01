use super::common::*;
use super::*;

impl MetadataStore {
    pub fn replace_observed_paths(
        &mut self,
        workspace_id: &WorkspaceId,
        paths: &[ObservedLocalPath],
        now: &str,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM local_paths WHERE workspace_id = ?1",
            [workspace_id.as_str()],
        )?;

        let mut statement = transaction.prepare(
            "INSERT INTO local_paths
             (id, workspace_id, project_id, path, classification, mode, access_json,
              matched_rule, rule_source, risk, summary, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                path.matched_rule,
                path.rule_source,
                path.risk,
                path.summary,
                now,
            ])?;
        }
        drop(statement);
        transaction.commit()?;

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
                "SELECT path, classification, mode, access_json, matched_rule, rule_source,
                        risk, summary
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
                        matched_rule: row.get(4)?,
                        rule_source: row.get(5)?,
                        risk: row.get(6)?,
                        summary: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
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
        self.mark_index_work_pending_for_path(
            &record.workspace_id,
            record.project_id.as_ref(),
            &path,
            "local-write-log",
            &record.created_at,
        )?;
        Ok(())
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
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
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
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
