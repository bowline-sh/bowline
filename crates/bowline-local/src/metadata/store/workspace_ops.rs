use super::common::*;
use super::*;

impl MetadataStore {
    pub fn insert_workspace(
        &self,
        id: &WorkspaceId,
        display_name: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO workspaces (id, display_name, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET
               display_name = excluded.display_name,
               updated_at = excluded.updated_at",
            params![id.as_str(), display_name, now],
        )?;
        Ok(())
    }

    pub fn insert_root(
        &self,
        id: &str,
        workspace_id: &WorkspaceId,
        accepted_path: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        let existing_workspace = self
            .connection
            .query_row(
                "SELECT workspace_id FROM roots WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if existing_workspace
            .as_deref()
            .is_some_and(|owner| owner != workspace_id.as_str())
        {
            return Err(MetadataError::InvalidStorageMetadata(format!(
                "root id `{id}` already belongs to another workspace"
            )));
        }
        self.connection.execute(
            "INSERT INTO roots
             (id, workspace_id, accepted_path, state, materialization_state, created_at)
             VALUES (?1, ?2, ?3, 'accepted', 'ready', ?4)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               accepted_path = excluded.accepted_path,
               state = excluded.state,
               materialization_state = excluded.materialization_state",
            params![id, workspace_id.as_str(), accepted_path, now],
        )?;
        Ok(())
    }

    pub fn insert_project(
        &self,
        id: &ProjectId,
        workspace_id: &WorkspaceId,
        root_id: &str,
        path: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO projects
             (id, workspace_id, root_id, path, hot_state, latest_snapshot_id, created_at)
             VALUES (?1, ?2, ?3, ?4, 'cold', NULL, ?5)
	             ON CONFLICT(id) DO UPDATE SET
	               workspace_id = excluded.workspace_id,
	               root_id = excluded.root_id,
	               path = excluded.path,
	               latest_snapshot_id = excluded.latest_snapshot_id",
            params![id.as_str(), workspace_id.as_str(), root_id, path, now],
        )?;
        Ok(())
    }

    pub fn replace_projects(
        &mut self,
        workspace_id: &WorkspaceId,
        root_id: &str,
        projects: &[(ProjectId, String)],
        now: &str,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        for (id, _) in projects {
            let existing_workspace = transaction
                .query_row(
                    "SELECT workspace_id FROM projects WHERE id = ?1",
                    [id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if existing_workspace
                .as_deref()
                .is_some_and(|owner| owner != workspace_id.as_str())
            {
                return Err(MetadataError::InvalidStorageMetadata(format!(
                    "project id `{}` already belongs to another workspace",
                    id.as_str()
                )));
            }
        }
        let mut statement = transaction.prepare(
            "INSERT INTO projects
             (id, workspace_id, root_id, path, hot_state, latest_snapshot_id, created_at)
             VALUES (?1, ?2, ?3, ?4, 'cold', NULL, ?5)
	             ON CONFLICT(id) DO UPDATE SET
	               workspace_id = excluded.workspace_id,
	               root_id = excluded.root_id,
	               path = excluded.path",
        )?;
        for (id, path) in projects {
            statement.execute(params![
                id.as_str(),
                workspace_id.as_str(),
                root_id,
                path,
                now
            ])?;
        }
        drop(statement);

        let retained_ids = projects
            .iter()
            .map(|(id, _)| id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let mut statement = transaction.prepare(
            "SELECT id FROM projects
             WHERE workspace_id = ?1",
        )?;
        let stale_ids = statement
            .query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|id| !retained_ids.contains(id))
            .collect::<Vec<_>>();
        drop(statement);
        for id in stale_ids {
            transaction.execute("DELETE FROM namespace_entries WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM index_documents WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM index_packs WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM index_work WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM symbol_records WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM work_views WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        }
        transaction.commit()?;

        Ok(())
    }

    pub fn current_workspace(&self) -> Result<Option<WorkspaceRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, display_name FROM workspaces
                 ORDER BY (
                     SELECT MAX(created_at) FROM roots
                     WHERE roots.workspace_id = workspaces.id
                       AND roots.state = 'accepted'
                 ) IS NOT NULL DESC,
                 (
                     SELECT MAX(created_at) FROM roots
                     WHERE roots.workspace_id = workspaces.id
                       AND roots.state = 'accepted'
                 ) DESC,
                 created_at DESC,
                 id DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok(WorkspaceRecord {
                        id: WorkspaceId::new(row.get::<_, String>(0)?),
                        display_name: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn current_project_by_path(
        &self,
        path: &str,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        let Some(workspace) = self.current_workspace()? else {
            return Ok(None);
        };
        let path = self.workspace_relative_path(&workspace.id, path)?;

        for candidate in project_path_candidates(&path) {
            let project = self
                .connection
                .query_row(
                    "SELECT id, path FROM projects
                     WHERE workspace_id = ?1 AND path = ?2
                     LIMIT 1",
                    params![workspace.id.as_str(), candidate],
                    |row| {
                        Ok(ProjectRecord {
                            id: ProjectId::new(row.get::<_, String>(0)?),
                            path: row.get(1)?,
                        })
                    },
                )
                .optional()?;
            if project.is_some() {
                return Ok(project);
            }
        }

        Ok(None)
    }

    pub fn project_by_id(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, path FROM projects
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), project_id.as_str()],
                |row| {
                    Ok(ProjectRecord {
                        id: ProjectId::new(row.get::<_, String>(0)?),
                        path: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn project_latest_snapshot_id(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Option<SnapshotId>, MetadataError> {
        self.connection
            .query_row(
                "SELECT latest_snapshot_id FROM projects
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), project_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten().map(SnapshotId::new))
            .map_err(Into::into)
    }

    pub fn set_project_latest_snapshot_id(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        snapshot_id: &SnapshotId,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE projects
             SET latest_snapshot_id = ?3
             WHERE workspace_id = ?1 AND id = ?2",
            params![
                workspace_id.as_str(),
                project_id.as_str(),
                snapshot_id.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn current_workspace_root(&self) -> Result<Option<String>, MetadataError> {
        let Some(workspace) = self.current_workspace()? else {
            return Ok(None);
        };

        self.connection
            .query_row(
                "SELECT accepted_path FROM roots
                 WHERE workspace_id = ?1 AND state = 'accepted'
                 ORDER BY created_at, id
                 LIMIT 1",
                [workspace.id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn projects(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ProjectRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, path FROM projects
             WHERE workspace_id = ?1
             ORDER BY path, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(ProjectRecord {
                id: ProjectId::new(row.get::<_, String>(0)?),
                path: row.get(1)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn project_count(&self, workspace_id: &WorkspaceId) -> Result<u64, MetadataError> {
        self.connection
            .query_row(
                "SELECT count(*) FROM projects WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(Into::into)
    }

    pub fn accepted_root_count(&self, workspace_id: &WorkspaceId) -> Result<u64, MetadataError> {
        self.connection
            .query_row(
                "SELECT count(*) FROM roots WHERE workspace_id = ?1 AND state = 'accepted'",
                [workspace_id.as_str()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(Into::into)
    }

    pub fn set_project_hot_state(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        hot_state: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE projects
             SET hot_state = ?3
             WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id.as_str(), project_id.as_str(), hot_state],
        )?;
        Ok(())
    }

    pub fn project_hot_state(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT hot_state FROM projects
                 WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id.as_str(), project_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn workspace_relative_path(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<String, MetadataError> {
        let path = normalize_path_for_matching(path);
        for root in self.accepted_roots(workspace_id)? {
            let root = normalize_path_for_matching(&root);
            if let Some(relative) = strip_root_prefix(&path, &root) {
                return Ok(normalize_workspace_path(relative));
            }
        }

        Ok(normalize_workspace_path(&path))
    }

    pub fn accepted_roots(&self, workspace_id: &WorkspaceId) -> Result<Vec<String>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT accepted_path FROM roots
             WHERE workspace_id = ?1 AND state = 'accepted'
             ORDER BY length(accepted_path) DESC",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn accepted_root_id_for_path(
        &self,
        workspace_id: &WorkspaceId,
        accepted_path: &str,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id FROM roots
                 WHERE workspace_id = ?1 AND accepted_path = ?2 AND state = 'accepted'",
                params![workspace_id.as_str(), accepted_path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }
}
