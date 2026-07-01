use super::common::*;
use super::*;

impl MetadataStore {
    pub fn upsert_index_document(&self, record: &IndexDocumentRecord) -> Result<(), MetadataError> {
        let path = normalize_workspace_path(&record.path);
        let project_id = record.project_id.as_ref().ok_or_else(|| {
            MetadataError::InvalidStorageMetadata("index document requires project_id".into())
        })?;
        self.connection.execute(
            "INSERT INTO index_documents
             (workspace_id, project_id, path, snapshot_id, content_id, classification, mode,
              access_json, policy_summary, body_text, hydration_state, indexed_bytes,
              source_watermark, indexed_watermark, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(workspace_id, project_id, path) DO UPDATE SET
               snapshot_id = excluded.snapshot_id,
               content_id = excluded.content_id,
               classification = excluded.classification,
               mode = excluded.mode,
               access_json = excluded.access_json,
               policy_summary = excluded.policy_summary,
               body_text = excluded.body_text,
               hydration_state = excluded.hydration_state,
               indexed_bytes = excluded.indexed_bytes,
               source_watermark = excluded.source_watermark,
               indexed_watermark = excluded.indexed_watermark,
               state = excluded.state,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                project_id.as_str(),
                path,
                record.snapshot_id.as_ref().map(|id| id.as_str()),
                record.content_id.as_ref().map(|id| id.as_str()),
                serialize_json_variant(&record.classification)?,
                serialize_json_variant(&record.mode)?,
                serialize_access_flags(&record.access)?,
                record.policy_summary,
                record.body_text,
                serialize_json_variant(&record.hydration_state)?,
                record.indexed_bytes,
                record.source_watermark,
                record.indexed_watermark,
                record.state,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn index_documents_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<IndexDocumentRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, project_id, path, snapshot_id, content_id, classification, mode,
                    access_json, policy_summary, body_text, hydration_state, indexed_bytes,
                    source_watermark, indexed_watermark, state, updated_at
             FROM index_documents
             WHERE workspace_id = ?1 AND project_id = ?2 AND state = 'ready'
             ORDER BY path",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            index_document_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn purge_index_path(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path: &str,
    ) -> Result<usize, MetadataError> {
        let path = normalize_workspace_path(path);
        let documents = self.connection.execute(
            "DELETE FROM index_documents WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![workspace_id.as_str(), project_id.as_str(), path],
        )?;
        let symbols = self.connection.execute(
            "DELETE FROM symbol_records WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![workspace_id.as_str(), project_id.as_str(), path],
        )?;
        Ok(documents + symbols)
    }

    pub fn purge_index_paths_except(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        keep_paths: &BTreeSet<String>,
        preserve_cold_paths: &BTreeSet<String>,
    ) -> Result<usize, MetadataError> {
        let existing = self.index_documents_for_project(workspace_id, project_id)?;
        let mut purged = 0_usize;
        for record in existing {
            let keep_cold = record.hydration_state == HydrationState::Cold
                && preserve_cold_paths.contains(&record.path);
            if !keep_paths.contains(&record.path) && !keep_cold {
                purged += self.purge_index_path(workspace_id, project_id, &record.path)?;
            }
        }
        Ok(purged)
    }

    pub fn purge_index_paths_under_prefix_except(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path_prefix: &str,
        keep_paths: &BTreeSet<String>,
        preserve_cold_paths: &BTreeSet<String>,
    ) -> Result<usize, MetadataError> {
        let path_prefix = normalize_workspace_path(path_prefix);
        let existing = self.index_documents_for_project(workspace_id, project_id)?;
        let mut purged = 0_usize;
        for record in existing {
            let keep_cold = record.hydration_state == HydrationState::Cold
                && preserve_cold_paths.contains(&record.path);
            if path_is_under_prefix(&record.path, &path_prefix)
                && !keep_paths.contains(&record.path)
                && !keep_cold
            {
                purged += self.purge_index_path(workspace_id, project_id, &record.path)?;
            }
        }
        Ok(purged)
    }

    pub fn purge_index_paths_for_snapshot_except(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        snapshot_id: &SnapshotId,
        keep_paths: &BTreeSet<String>,
    ) -> Result<usize, MetadataError> {
        let existing = self.index_documents_for_project(workspace_id, project_id)?;
        let mut purged = 0_usize;
        for record in existing {
            if record.snapshot_id.as_ref() == Some(snapshot_id)
                && !keep_paths.contains(&record.path)
            {
                purged += self.purge_index_path(workspace_id, project_id, &record.path)?;
            }
        }
        Ok(purged)
    }

    pub fn replace_symbol_records_for_path(
        &mut self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path: &str,
        records: &[SymbolIndexRecord],
    ) -> Result<(), MetadataError> {
        let path = normalize_workspace_path(path);
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM symbol_records WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![workspace_id.as_str(), project_id.as_str(), path],
        )?;
        let mut statement = transaction.prepare(
            "INSERT INTO symbol_records
             (id, workspace_id, project_id, path, snapshot_id, name, kind, language, line_start,
              line_end, byte_start, byte_end, parser_status, access_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?;
        for record in records {
            let record_project_id = record.project_id.as_ref().ok_or_else(|| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(
                    MetadataError::InvalidStorageMetadata(
                        "symbol record requires project_id".to_string(),
                    ),
                ))
            })?;
            statement.execute(params![
                record.id,
                record.workspace_id.as_str(),
                record_project_id.as_str(),
                path,
                record.snapshot_id.as_ref().map(|id| id.as_str()),
                record.name,
                record.kind,
                record.language,
                record.line_start,
                record.line_end,
                record.byte_start,
                record.byte_end,
                record.parser_status,
                serialize_access_flags(&record.access)?,
                record.updated_at,
            ])?;
        }
        drop(statement);
        transaction.commit()?;
        Ok(())
    }

    pub fn symbol_records_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<SymbolIndexRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, path, snapshot_id, name, kind, language,
                    line_start, line_end, byte_start, byte_end, parser_status, access_json,
                    updated_at
             FROM symbol_records
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY path, line_start, name, id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            symbol_index_record_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_index_work(&self, record: &IndexWorkRecord) -> Result<(), MetadataError> {
        let path = record.path.as_deref().map(normalize_workspace_path);
        self.connection.execute(
            "INSERT INTO index_work
             (id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark,
              state, reason, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               path = excluded.path,
               kind = excluded.kind,
               source_watermark = excluded.source_watermark,
               indexed_watermark = excluded.indexed_watermark,
               state = excluded.state,
               reason = excluded.reason,
               updated_at = excluded.updated_at",
            params![
                record.id,
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                path,
                record.kind,
                record.source_watermark,
                record.indexed_watermark,
                record.state,
                record.reason,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn mark_index_work_pending_for_path(
        &self,
        workspace_id: &WorkspaceId,
        project_id: Option<&ProjectId>,
        path: &str,
        reason: &str,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let Some(project_id) = project_id else {
            return Ok(());
        };
        let path = self.workspace_relative_path(workspace_id, path)?;
        if is_work_namespace_path(&path) {
            return Ok(());
        }
        if reason == "local-write-log"
            && !project_index_initialized(self, workspace_id, project_id)?
        {
            return Ok(());
        }
        let index_path = self
            .project_by_id(workspace_id, project_id)?
            .map(|project| project_relative_index_path(&path, &project.path))
            .unwrap_or(path);
        let source_watermark = next_index_source_watermark(self, workspace_id, project_id)?;
        self.upsert_index_work(&IndexWorkRecord {
            id: format!(
                "index_work:{}:{}:path:{}",
                workspace_id.as_str(),
                project_id.as_str(),
                stable_store_token(&index_path)
            ),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some(index_path),
            kind: "path".to_string(),
            source_watermark,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some(reason.to_string()),
            updated_at: updated_at.to_string(),
        })
    }

    pub fn mark_index_work_ready_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE index_work
             SET state = 'ready',
                 indexed_watermark = source_watermark,
                 reason = NULL,
                 updated_at = ?3
             WHERE workspace_id = ?1 AND project_id = ?2 AND state != 'ready'",
            params![workspace_id.as_str(), project_id.as_str(), updated_at],
        )?;
        Ok(())
    }

    pub fn mark_index_work_ready_for_paths(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        paths: &BTreeSet<String>,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let work = self.index_work_for_project(workspace_id, project_id)?;
        for record in work {
            let Some(path) = record.path.as_deref() else {
                continue;
            };
            if paths.contains(path) {
                self.connection.execute(
                    "UPDATE index_work
                     SET indexed_watermark = source_watermark,
                         state = 'ready',
                         reason = NULL,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![updated_at, record.id],
                )?;
            }
        }
        Ok(())
    }

    pub fn mark_local_write_index_work_ready_for_scope(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path_prefix: Option<&str>,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let path_prefix = path_prefix.map(normalize_workspace_path);
        let work = self.index_work_for_project(workspace_id, project_id)?;
        for record in work {
            if record.reason.as_deref() != Some("local-write-log") {
                continue;
            }
            let Some(path) = record.path.as_deref() else {
                continue;
            };
            if path_prefix
                .as_deref()
                .is_none_or(|prefix| path_is_under_prefix(path, prefix))
            {
                self.connection.execute(
                    "UPDATE index_work
                     SET indexed_watermark = source_watermark,
                         state = 'ready',
                         reason = NULL,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![updated_at, record.id],
                )?;
            }
        }
        Ok(())
    }

    pub fn mark_index_work_ready_under_prefix(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path_prefix: &str,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let path_prefix = normalize_workspace_path(path_prefix);
        let work = self.index_work_for_project(workspace_id, project_id)?;
        for record in work {
            let Some(path) = record.path.as_deref() else {
                continue;
            };
            if path_is_under_prefix(path, &path_prefix) {
                self.connection.execute(
                    "UPDATE index_work
                     SET indexed_watermark = source_watermark,
                         state = 'ready',
                         reason = NULL,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![updated_at, record.id],
                )?;
            }
        }
        Ok(())
    }

    pub fn upsert_index_pack(&self, record: &IndexPackRecord) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO index_packs
             (workspace_id, project_id, snapshot_id, object_key, byte_len, hash, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(workspace_id, object_key) DO UPDATE SET
               project_id = excluded.project_id,
               snapshot_id = excluded.snapshot_id,
               byte_len = excluded.byte_len,
               hash = excluded.hash,
               state = excluded.state,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                record.snapshot_id.as_ref().map(|id| id.as_str()),
                record.object_key,
                record.byte_len,
                record.hash,
                record.state,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn index_packs_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<IndexPackRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, project_id, snapshot_id, object_key, byte_len, hash, state,
                    updated_at
             FROM index_packs
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY updated_at DESC, object_key",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            index_pack_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn index_work_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<IndexWorkRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark,
                    state, reason, updated_at
             FROM index_work
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY updated_at, id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            index_work_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn put_pack_record(
        &self,
        workspace_id: &WorkspaceId,
        pack_id: &PackId,
        kind: &str,
        byte_len: u64,
        state: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.put_pack_record_with_metadata(
            workspace_id,
            pack_id,
            kind,
            byte_len,
            "",
            1,
            state,
            None,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_pack_record_with_metadata(
        &self,
        workspace_id: &WorkspaceId,
        pack_id: &PackId,
        kind: &str,
        byte_len: u64,
        object_hash: &str,
        key_epoch: u32,
        state: &str,
        retain_until: Option<&str>,
        now: &str,
    ) -> Result<(), MetadataError> {
        validate_pack_kind(kind)?;
        validate_pack_state(state)?;
        if key_epoch == 0 {
            return Err(MetadataError::InvalidStorageMetadata(
                "pack key epoch must be non-zero".to_string(),
            ));
        }
        self.connection.execute(
            "INSERT INTO packs
             (id, workspace_id, kind, byte_len, object_hash, key_epoch, state, retain_until, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, id) DO UPDATE SET
               kind = excluded.kind,
               byte_len = excluded.byte_len,
               object_hash = excluded.object_hash,
               key_epoch = excluded.key_epoch,
               state = excluded.state,
               retain_until = excluded.retain_until",
            params![
                pack_id.as_str(),
                workspace_id.as_str(),
                kind,
                byte_len,
                object_hash,
                key_epoch,
                state,
                retain_until,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn pack_records(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<PackRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, kind, byte_len, object_hash, key_epoch, state, retain_until
             FROM packs
             WHERE workspace_id = ?1
             ORDER BY id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(PackRecord {
                id: PackId::new(row.get::<_, String>(0)?),
                workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
                kind: row.get(2)?,
                byte_len: row.get(3)?,
                object_hash: row.get(4)?,
                key_epoch: row.get(5)?,
                state: row.get(6)?,
                retain_until: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn put_content_locator(
        &self,
        workspace_id: &WorkspaceId,
        locator: &ContentLocator,
        now: &str,
    ) -> Result<(), MetadataError> {
        validate_locator_shape(locator)?;
        if let Some(pack_id) = &locator.pack_id {
            ensure_pack_exists(&self.connection, workspace_id, pack_id)?;
        }
        let locator_json = serde_json::to_string(locator)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        self.connection.execute(
            "INSERT INTO content_locators
             (content_id, workspace_id, storage, raw_size, pack_id, offset, length,
              locator_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, content_id) DO UPDATE SET
               storage = excluded.storage,
               raw_size = excluded.raw_size,
               pack_id = excluded.pack_id,
               offset = excluded.offset,
               length = excluded.length,
               locator_json = excluded.locator_json,
               updated_at = excluded.updated_at",
            params![
                locator.content_id.as_str(),
                workspace_id.as_str(),
                serialize_json_variant(&locator.storage)?,
                locator.raw_size,
                locator.pack_id.as_ref().map(|pack_id| pack_id.as_str()),
                locator.offset,
                locator.length,
                locator_json,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn content_locator(
        &self,
        workspace_id: &WorkspaceId,
        content_id: &ContentId,
    ) -> Result<Option<StoredContentLocator>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, content_id, storage, raw_size, pack_id, offset, length, locator_json,
                        updated_at
                 FROM content_locators
                 WHERE workspace_id = ?1 AND content_id = ?2",
                params![workspace_id.as_str(), content_id.as_str()],
                |row| {
                    Ok(StoredContentLocator {
                        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
                        locator: content_locator_from_row(row)?,
                        updated_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn content_locators(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ContentLocator>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, content_id, storage, raw_size, pack_id, offset, length, locator_json
             FROM content_locators
             WHERE workspace_id = ?1
             ORDER BY content_id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], content_locator_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
