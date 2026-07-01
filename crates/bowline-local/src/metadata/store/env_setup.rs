use super::common::*;
use super::*;

impl MetadataStore {
    pub fn replace_env_records_for_source(
        &mut self,
        workspace_id: &WorkspaceId,
        source_path: &str,
        records: &[EnvRecord],
    ) -> Result<(), MetadataError> {
        let source_path = self.workspace_relative_path(workspace_id, source_path)?;
        let normalized_records = records
            .iter()
            .map(|record| {
                let mut record = record.clone();
                record.source_path =
                    self.workspace_relative_path(&record.workspace_id, &record.source_path)?;
                Ok(record)
            })
            .collect::<Result<Vec<_>, MetadataError>>()?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM env_records
             WHERE workspace_id = ?1 AND source_path = ?2",
            params![workspace_id.as_str(), source_path],
        )?;
        for record in normalized_records {
            upsert_env_record_tx(&transaction, &record)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn upsert_env_record(&self, record: &EnvRecord) -> Result<(), MetadataError> {
        let mut record = record.clone();
        record.source_path =
            self.workspace_relative_path(&record.workspace_id, &record.source_path)?;
        self.connection
            .execute(
                "INSERT INTO env_records
                 (id, workspace_id, project_id, source_path, key_name, access,
                  value_ciphertext_ref, updated_at, profile, occurrence_index, line_kind,
                  encrypted_locator_json, format_json, materialization_state, restriction_state,
                  key_epoch, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(id) DO UPDATE SET
                   workspace_id = excluded.workspace_id,
                   project_id = excluded.project_id,
                   source_path = excluded.source_path,
                   key_name = excluded.key_name,
                   access = excluded.access,
                   value_ciphertext_ref = excluded.value_ciphertext_ref,
                   updated_at = excluded.updated_at,
                   profile = excluded.profile,
                   occurrence_index = excluded.occurrence_index,
                   line_kind = excluded.line_kind,
                   encrypted_locator_json = excluded.encrypted_locator_json,
                   format_json = excluded.format_json,
                   materialization_state = excluded.materialization_state,
                   restriction_state = excluded.restriction_state,
                   key_epoch = excluded.key_epoch,
                   metadata_json = excluded.metadata_json",
                params![
                    record.id.as_str(),
                    record.workspace_id.as_str(),
                    record.project_id.as_ref().map(|id| id.as_str()),
                    record.source_path.as_str(),
                    record.key_name.as_str(),
                    serialize_access_flags(&record.access)?,
                    record.updated_at.as_str(),
                    record.profile.as_str(),
                    record.occurrence_index,
                    record.line_kind.as_str(),
                    record.encrypted_locator_json.as_str(),
                    record.format_json.as_str(),
                    record.materialization_state.as_str(),
                    record.restriction_state.as_str(),
                    record.key_epoch,
                    record.metadata_json.as_str(),
                ],
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn env_records(&self, workspace_id: &WorkspaceId) -> Result<Vec<EnvRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, source_path, key_name, access,
                    updated_at, profile, occurrence_index, line_kind, encrypted_locator_json,
                    format_json, materialization_state, restriction_state, key_epoch, metadata_json
             FROM env_records
             WHERE workspace_id = ?1
             ORDER BY source_path, occurrence_index, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], env_record_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn env_records_for_source(
        &self,
        workspace_id: &WorkspaceId,
        source_path: &str,
    ) -> Result<Vec<EnvRecord>, MetadataError> {
        let source_path = self.workspace_relative_path(workspace_id, source_path)?;
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, source_path, key_name, access,
                    updated_at, profile, occurrence_index, line_kind, encrypted_locator_json,
                    format_json, materialization_state, restriction_state, key_epoch, metadata_json
             FROM env_records
             WHERE workspace_id = ?1 AND source_path = ?2
             ORDER BY occurrence_index, id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), source_path],
            env_record_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_setup_receipt(&self, record: &SetupReceiptRecord) -> Result<(), MetadataError> {
        let cwd = self.workspace_relative_path(&record.workspace_id, &record.cwd)?;
        self.connection.execute(
            "INSERT INTO setup_receipts
             (id, workspace_id, project_id, command, state, receipt_json, updated_at,
              recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
              output_path, redacted_summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               command = excluded.command,
               state = excluded.state,
               receipt_json = excluded.receipt_json,
               updated_at = excluded.updated_at,
               recipe_hash = excluded.recipe_hash,
               approval_state = excluded.approval_state,
               trigger = excluded.trigger,
               cwd = excluded.cwd,
               os = excluded.os,
               arch = excluded.arch,
               env_profile = excluded.env_profile,
               output_path = excluded.output_path,
               redacted_summary = excluded.redacted_summary",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                record.command.as_str(),
                record.state.as_str(),
                record.receipt_json.as_str(),
                record.updated_at.as_str(),
                record.recipe_hash.as_str(),
                record.approval_state.as_str(),
                record.trigger.as_str(),
                cwd,
                record.os.as_str(),
                record.arch.as_str(),
                record.env_profile.as_str(),
                record.output_path.as_deref(),
                record.redacted_summary.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn setup_receipts(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SetupReceiptRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, command, state, receipt_json, updated_at,
                    recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
                    output_path, redacted_summary
             FROM setup_receipts
             WHERE workspace_id = ?1
             ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], setup_receipt_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_projected_node(&self, record: &ProjectedNodeRecord) -> Result<(), MetadataError> {
        let path = self.workspace_relative_path(&record.workspace_id, &record.path)?;
        self.connection.execute(
            "INSERT INTO projected_nodes
             (workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
              hydration_state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, node_id) DO UPDATE SET
               project_id = excluded.project_id,
               parent_node_id = excluded.parent_node_id,
               path = excluded.path,
               kind = excluded.kind,
               content_id = excluded.content_id,
               hydration_state = excluded.hydration_state,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                record.node_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                record.parent_node_id.as_deref(),
                path,
                serialize_json_variant(&record.kind)?,
                record.content_id.as_ref().map(|id| id.as_str()),
                serialize_json_variant(&record.hydration_state)?,
                record.updated_at.as_str(),
            ],
        )?;
        if record.kind == NamespaceEntryKind::File {
            let index_target = match record.project_id.as_ref() {
                Some(project_id) => {
                    let index_path = self
                        .project_by_id(&record.workspace_id, project_id)?
                        .map(|project| project_relative_index_path(&path, &project.path))
                        .unwrap_or_else(|| path.clone());
                    Some((project_id.clone(), index_path))
                }
                None => self.current_project_by_path(&path)?.map(|project| {
                    let index_path = project_relative_index_path(&path, &project.path);
                    (project.id, index_path)
                }),
            };
            if let Some((project_id, index_path)) = index_target {
                self.mark_index_work_pending_for_path(
                    &record.workspace_id,
                    Some(&project_id),
                    &index_path,
                    "projected-node-updated",
                    &record.updated_at,
                )?;
            }
        }
        Ok(())
    }

    pub fn projected_node_by_path(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<Option<ProjectedNodeRecord>, MetadataError> {
        let path = self.workspace_relative_path(workspace_id, path)?;
        self.connection
            .query_row(
                "SELECT workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
                        hydration_state, updated_at
                 FROM projected_nodes
                 WHERE workspace_id = ?1 AND path = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), path],
                projected_node_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn projected_nodes_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<ProjectedNodeRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
                    hydration_state, updated_at
             FROM projected_nodes
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY path",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            projected_node_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn projected_nodes_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ProjectedNodeRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
                    hydration_state, updated_at
             FROM projected_nodes
             WHERE workspace_id = ?1
             ORDER BY path",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], projected_node_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn delete_unlisted_workspace_projected_nodes(
        &self,
        workspace_id: &WorkspaceId,
        retained_paths: &BTreeSet<String>,
    ) -> Result<(), MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT path FROM projected_nodes
             WHERE workspace_id = ?1 AND project_id IS NULL",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?;
        let stale_paths = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|path| !retained_paths.contains(path))
            .collect::<Vec<_>>();
        drop(statement);
        for path in stale_paths {
            self.connection.execute(
                "DELETE FROM projected_nodes
                 WHERE workspace_id = ?1 AND project_id IS NULL AND path = ?2",
                params![workspace_id.as_str(), path],
            )?;
        }
        Ok(())
    }

    pub fn enqueue_hydration(&self, record: &HydrationQueueRecord) -> Result<(), MetadataError> {
        let path = self.workspace_relative_path(&record.workspace_id, &record.path)?;
        self.connection.execute(
            "INSERT INTO hydration_queue
             (id, workspace_id, project_id, path, content_id, priority, state, cause, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, path, cause) DO UPDATE SET
               id = excluded.id,
               project_id = excluded.project_id,
               content_id = excluded.content_id,
               priority = excluded.priority,
               state = excluded.state,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                path,
                record.content_id.as_ref().map(|id| id.as_str()),
                record.priority.as_str(),
                record.state.as_str(),
                record.cause.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn hydration_queue(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<HydrationQueueRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, path, content_id, priority, state, cause, updated_at
             FROM hydration_queue
             WHERE workspace_id = ?1
             ORDER BY CASE priority
               WHEN 'active-read' THEN 0
               WHEN 'explicit-pin' THEN 1
               WHEN 'agent-lease' THEN 2
               WHEN 'hot-project-prefetch' THEN 3
               WHEN 'index-request' THEN 4
               WHEN 'background' THEN 5
               ELSE 6
             END, updated_at, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(HydrationQueueRecord {
                id: row.get(0)?,
                workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
                project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
                path: row.get(3)?,
                content_id: row.get::<_, Option<String>>(4)?.map(ContentId::new),
                priority: row.get(5)?,
                state: row.get(6)?,
                cause: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
