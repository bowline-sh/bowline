use super::common::*;
use super::*;

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
}
