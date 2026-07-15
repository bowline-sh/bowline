use super::common::*;
use super::*;
use rusqlite::params_from_iter;

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
             (id, workspace_id, root_id, path, hot_state, latest_snapshot_id,
              git_observer_state, created_at)
             VALUES (?1, ?2, ?3, ?4, 'cold', NULL, 'ok', ?5)
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
        projects: &[ProjectUpsert],
        now: &str,
    ) -> Result<(), MetadataError> {
        self.with_committed(|store| {
            store.replace_projects_uncommitted(workspace_id, root_id, projects, now)
        })
    }

    pub(crate) fn replace_projects_uncommitted(
        &self,
        workspace_id: &WorkspaceId,
        root_id: &str,
        projects: &[ProjectUpsert],
        now: &str,
    ) -> Result<(), MetadataError> {
        for project in projects {
            let existing_workspace = self
                .connection
                .query_row(
                    "SELECT workspace_id FROM projects WHERE id = ?1",
                    [project.id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if existing_workspace
                .as_deref()
                .is_some_and(|owner| owner != workspace_id.as_str())
            {
                return Err(MetadataError::InvalidStorageMetadata(format!(
                    "project id `{}` already belongs to another workspace",
                    project.id.as_str()
                )));
            }
        }
        let mut statement = self.connection.prepare(
            "INSERT INTO projects
             (id, workspace_id, root_id, path, hot_state, latest_snapshot_id,
              git_observer_state, created_at)
             VALUES (?1, ?2, ?3, ?4, 'cold', NULL, ?5, ?6)
	             ON CONFLICT(id) DO UPDATE SET
	               workspace_id = excluded.workspace_id,
	               root_id = excluded.root_id,
	               path = excluded.path,
                   git_observer_state = excluded.git_observer_state",
        )?;
        for project in projects {
            statement.execute(params![
                project.id.as_str(),
                workspace_id.as_str(),
                root_id,
                project.path,
                project.git_observer_state.wire_str(),
                now
            ])?;
        }
        drop(statement);

        let retained_ids = projects
            .iter()
            .map(|project| project.id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let mut statement = self.connection.prepare(
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
            self.connection
                .execute("DELETE FROM work_views WHERE project_id = ?1", [&id])?;
            self.connection
                .execute("DELETE FROM projects WHERE id = ?1", [id])?;
        }

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

    pub fn workspace_by_accepted_root(
        &self,
        root_path: &str,
    ) -> Result<Option<WorkspaceRecord>, MetadataError> {
        let requested = normalize_path_for_matching(root_path);
        let mut statement = self.connection.prepare(
            "SELECT workspaces.id, workspaces.display_name, roots.accepted_path
             FROM roots
             JOIN workspaces ON workspaces.id = roots.workspace_id
             WHERE roots.state = 'accepted'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                WorkspaceRecord {
                    id: WorkspaceId::new(row.get::<_, String>(0)?),
                    display_name: row.get(1)?,
                },
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (workspace, accepted_path) = row?;
            if normalize_path_for_matching(&accepted_path) == requested {
                return Ok(Some(workspace));
            }
        }
        Ok(None)
    }

    pub fn workspace_by_path(&self, path: &str) -> Result<Option<WorkspaceRecord>, MetadataError> {
        let requested = normalize_path_for_matching(path);
        let mut statement = self.connection.prepare(
            "SELECT workspaces.id, workspaces.display_name, roots.accepted_path
             FROM roots
             JOIN workspaces ON workspaces.id = roots.workspace_id
             WHERE roots.state = 'accepted'
             ORDER BY length(roots.accepted_path) DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                WorkspaceRecord {
                    id: WorkspaceId::new(row.get::<_, String>(0)?),
                    display_name: row.get(1)?,
                },
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (workspace, accepted_path) = row?;
            let root = normalize_path_for_matching(&accepted_path);
            if strip_root_prefix(&requested, &root).is_some() {
                return Ok(Some(workspace));
            }
        }
        Ok(None)
    }

    pub fn workspace_root(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT accepted_path FROM roots
                 WHERE workspace_id = ?1 AND state = 'accepted'
                 ORDER BY created_at, id
                 LIMIT 1",
                [workspace_id.as_str()],
                |row| row.get::<_, String>(0),
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
        self.project_by_path(&workspace.id, path)
    }

    pub fn project_by_path(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        let path = self.workspace_relative_path(workspace_id, path)?;

        for candidate in project_path_candidates(&path) {
            let project = self
                .connection
                .query_row(
                    "SELECT id, path, lifecycle_state, local_materialization_state, purge_after, git_observer_state FROM projects
                     WHERE workspace_id = ?1 AND path = ?2
                     LIMIT 1",
                    params![workspace_id.as_str(), candidate],
                    |row| {
                        Ok(ProjectRecord {
                            id: ProjectId::new(row.get::<_, String>(0)?),
                            path: row.get(1)?,
                            lifecycle_state: parse_project_lifecycle_state(row.get(2)?)?,
                            local_materialization_state:
                                parse_project_local_materialization_state(row.get(3)?)?,
                            purge_after: row.get(4)?,
                            git_observer_state: parse_git_observer_state(row.get(5)?)?,
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
                "SELECT id, path, lifecycle_state, local_materialization_state, purge_after, git_observer_state FROM projects
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), project_id.as_str()],
                |row| {
                    Ok(ProjectRecord {
                        id: ProjectId::new(row.get::<_, String>(0)?),
                        path: row.get(1)?,
                        lifecycle_state: parse_project_lifecycle_state(row.get(2)?)?,
                        local_materialization_state: parse_project_local_materialization_state(
                            row.get(3)?,
                        )?,
                        purge_after: row.get(4)?,
                        git_observer_state: parse_git_observer_state(row.get(5)?)?,
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

    pub fn project_latest_snapshot_ids(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<BTreeMap<ProjectId, SnapshotId>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, latest_snapshot_id FROM projects
             WHERE workspace_id = ?1 AND latest_snapshot_id IS NOT NULL
             ORDER BY path, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok((
                ProjectId::new(row.get::<_, String>(0)?),
                SnapshotId::new(row.get::<_, String>(1)?),
            ))
        })?;

        rows.collect::<Result<BTreeMap<_, _>, _>>()
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

    pub fn upsert_snapshot(&self, record: &SnapshotRecord) -> Result<(), MetadataError> {
        if let Some(existing) = self.snapshot(&record.workspace_id, &record.id)? {
            // `created_at` records the first durable commit. A crash/retry may
            // present the same immutable snapshot under a later operation
            // timestamp and must converge on that first-writer metadata.
            if existing.has_same_immutable_binding(record) {
                return Ok(());
            }
            return Err(MetadataError::ImmutableBindingConflict {
                logical_id: record.id.as_str().to_string(),
                field: "snapshot_root",
            });
        }
        if let Some(project_id) = &record.project_id {
            let project_exists = self
                .connection
                .query_row(
                    "SELECT 1 FROM projects WHERE workspace_id = ?1 AND id = ?2",
                    params![record.workspace_id.as_str(), project_id.as_str()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !project_exists {
                return Err(MetadataError::InvalidStorageMetadata(format!(
                    "snapshot project `{}` was not found",
                    project_id.as_str()
                )));
            }
        }
        let refs_json = serde_json::to_string(&record.refs)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        self.connection.execute(
            "INSERT INTO snapshots
             (id, workspace_id, project_id, kind, base_snapshot_id, root_id,
              semantic_manifest_digest, entry_count, refs_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                serialize_json_variant(&record.kind)?,
                record.base_snapshot_id.as_ref().map(|id| id.as_str()),
                record.root_id.as_str(),
                record.semantic_manifest_digest.as_str(),
                record.entry_count,
                refs_json,
                record.created_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn snapshot(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<Option<SnapshotRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, project_id, kind, base_snapshot_id, root_id,
                        semantic_manifest_digest, entry_count, refs_json, created_at
                 FROM snapshots
                 WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id.as_str(), snapshot_id.as_str()],
                snapshot_record_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn snapshot_project_id(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<Option<Option<ProjectId>>, MetadataError> {
        self.connection
            .query_row(
                "SELECT project_id
                 FROM snapshots
                 WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id.as_str(), snapshot_id.as_str()],
                |row| {
                    row.get::<_, Option<String>>(0)
                        .map(|id| id.map(ProjectId::new))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn snapshot_project_ids(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_ids: &[SnapshotId],
    ) -> Result<BTreeMap<SnapshotId, Option<ProjectId>>, MetadataError> {
        if snapshot_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let placeholders = vec!["?"; snapshot_ids.len()].join(", ");
        let sql = format!(
            "SELECT id, project_id
             FROM snapshots
             WHERE workspace_id = ? AND id IN ({placeholders})"
        );
        let mut params = Vec::with_capacity(snapshot_ids.len() + 1);
        params.push(workspace_id.as_str().to_string());
        params.extend(snapshot_ids.iter().map(|id| id.as_str().to_string()));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(params.iter()), |row| {
            let snapshot_id = SnapshotId::new(row.get::<_, String>(0)?);
            let project_id = row.get::<_, Option<String>>(1)?.map(ProjectId::new);
            Ok((snapshot_id, project_id))
        })?;

        rows.collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(Into::into)
    }

    pub fn current_workspace_root(&self) -> Result<Option<String>, MetadataError> {
        let Some(workspace) = self.current_workspace()? else {
            return Ok(None);
        };

        self.workspace_root(&workspace.id)
    }

    pub fn projects(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ProjectRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, path, lifecycle_state, local_materialization_state, purge_after, git_observer_state FROM projects
             WHERE workspace_id = ?1
             ORDER BY path, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(ProjectRecord {
                id: ProjectId::new(row.get::<_, String>(0)?),
                path: row.get(1)?,
                lifecycle_state: parse_project_lifecycle_state(row.get(2)?)?,
                local_materialization_state: parse_project_local_materialization_state(
                    row.get(3)?,
                )?,
                purge_after: row.get(4)?,
                git_observer_state: parse_git_observer_state(row.get(5)?)?,
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

    pub fn set_project_lifecycle(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        lifecycle_state: ProjectLifecycleState,
        local_materialization_state: Option<ProjectLocalMaterializationState>,
        purge_after: Option<&str>,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE projects
             SET lifecycle_state = ?3,
                 local_materialization_state = COALESCE(?4, local_materialization_state),
                 purge_after = ?5
             WHERE workspace_id = ?1 AND id = ?2",
            params![
                workspace_id.as_str(),
                project_id.as_str(),
                lifecycle_state.as_str(),
                local_materialization_state.map(ProjectLocalMaterializationState::as_str),
                purge_after,
            ],
        )?;
        Ok(())
    }

    pub fn set_project_local_materialization(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        state: ProjectLocalMaterializationState,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE projects
             SET local_materialization_state = ?3
             WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id.as_str(), project_id.as_str(), state.as_str()],
        )?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use bowline_core::{
        ids::{ManifestDigest, NamespacePageId},
        workspace_graph::SnapshotKind,
    };

    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn project_latest_snapshot_ids_returns_only_non_null_project_heads() {
        let temp = TempWorkspace::new("project-latest-snapshot-ids").expect("temp workspace");
        let db_path = temp.root().join("state/local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_a = ProjectId::new("proj_a");
        let project_b = ProjectId::new("proj_b");
        let project_c = ProjectId::new("proj_c");
        let snapshot_a = SnapshotId::new("snap_a");
        let snapshot_c = SnapshotId::new("snap_c");

        seed_workspace_projects(
            &store,
            &workspace_id,
            &[(&project_a, "a"), (&project_b, "b"), (&project_c, "c")],
        );
        store
            .set_project_latest_snapshot_id(&workspace_id, &project_a, &snapshot_a)
            .expect("set a head");
        store
            .set_project_latest_snapshot_id(&workspace_id, &project_c, &snapshot_c)
            .expect("set c head");

        let snapshot_ids = store
            .project_latest_snapshot_ids(&workspace_id)
            .expect("latest snapshot ids");

        assert_eq!(snapshot_ids.len(), 2);
        assert_eq!(snapshot_ids.get(&project_a), Some(&snapshot_a));
        assert_eq!(snapshot_ids.get(&project_b), None);
        assert_eq!(snapshot_ids.get(&project_c), Some(&snapshot_c));
    }

    #[test]
    fn snapshot_project_ids_batches_present_workspace_and_project_snapshots() {
        let temp = TempWorkspace::new("snapshot-project-ids").expect("temp workspace");
        let db_path = temp.root().join("state/local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_a");
        let project_snapshot = SnapshotId::new("snap_project");
        let workspace_snapshot = SnapshotId::new("snap_workspace");
        let missing_snapshot = SnapshotId::new("snap_missing");

        seed_workspace_projects(&store, &workspace_id, &[(&project_id, "a")]);
        seed_snapshot(&store, &workspace_id, Some(&project_id), &project_snapshot);
        seed_snapshot(&store, &workspace_id, None, &workspace_snapshot);

        let project_ids = store
            .snapshot_project_ids(
                &workspace_id,
                &[
                    project_snapshot.clone(),
                    workspace_snapshot.clone(),
                    missing_snapshot.clone(),
                ],
            )
            .expect("snapshot project ids");

        assert_eq!(project_ids.get(&project_snapshot), Some(&Some(project_id)));
        assert_eq!(project_ids.get(&workspace_snapshot), Some(&None));
        assert_eq!(project_ids.get(&missing_snapshot), None);
    }

    fn seed_workspace_projects(
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        projects: &[(&ProjectId, &str)],
    ) {
        store
            .insert_workspace(workspace_id, "Code", "2026-07-07T00:00:00Z")
            .expect("workspace");
        store
            .insert_root("root_code", workspace_id, "~/Code", "2026-07-07T00:00:00Z")
            .expect("root");
        for (project_id, path) in projects {
            store
                .insert_project(
                    project_id,
                    workspace_id,
                    "root_code",
                    path,
                    "2026-07-07T00:00:00Z",
                )
                .expect("project");
        }
    }

    fn seed_snapshot(
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        project_id: Option<&ProjectId>,
        snapshot_id: &SnapshotId,
    ) {
        store
            .upsert_snapshot(&SnapshotRecord {
                id: snapshot_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.cloned(),
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                root_id: NamespacePageId::new(format!("page_{}", snapshot_id.as_str())),
                semantic_manifest_digest: ManifestDigest::new(format!(
                    "digest_{}",
                    snapshot_id.as_str()
                )),
                entry_count: 0,
                refs: Vec::new(),
                created_at: "2026-07-07T00:00:00Z".to_string(),
            })
            .expect("snapshot");
    }
}
