use super::common::*;
use super::*;

pub(super) fn upsert_work_view_record(
    connection: &Connection,
    record: &WorkViewRecord,
    project_path: &str,
) -> Result<(), MetadataError> {
    connection.execute(
        "INSERT INTO work_views
         (id, workspace_id, project_id, project_path, name, visible_path, base_snapshot_id,
          overlay_head, overlay_version, env_profile, lifecycle, visibility, sync_state, retention_state,
          retain_until, restorable, owner_device_id, followed_by_json,
          host_materializations_json, attention_json, audit_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                 ?16, ?17, ?18, ?19, ?20, '{}', ?21, ?22)
         ON CONFLICT(id) DO UPDATE SET
           workspace_id = excluded.workspace_id,
           project_id = excluded.project_id,
           project_path = excluded.project_path,
           name = excluded.name,
           visible_path = excluded.visible_path,
           base_snapshot_id = excluded.base_snapshot_id,
           overlay_head = excluded.overlay_head,
           overlay_version = excluded.overlay_version,
           env_profile = excluded.env_profile,
           lifecycle = excluded.lifecycle,
           visibility = excluded.visibility,
           sync_state = excluded.sync_state,
           retention_state = excluded.retention_state,
           retain_until = excluded.retain_until,
           restorable = excluded.restorable,
           owner_device_id = excluded.owner_device_id,
           followed_by_json = excluded.followed_by_json,
           host_materializations_json = excluded.host_materializations_json,
           attention_json = excluded.attention_json,
           updated_at = excluded.updated_at",
        params![
            record.id.as_str(),
            record.workspace_id.as_str(),
            record.project_id.as_str(),
            project_path,
            record.name.as_str(),
            record.visible_path.as_str(),
            record.base_snapshot_id.as_str(),
            record.overlay_head.as_str(),
            record.overlay_version,
            record.env_profile.as_str(),
            serialize_json_variant(&record.lifecycle)?,
            serialize_json_variant(&record.visibility)?,
            serialize_json_variant(&record.sync_state)?,
            serialize_json_variant(&record.retention.state)?,
            record.retention.retain_until.as_deref(),
            if record.retention.restorable {
                1_i64
            } else {
                0_i64
            },
            record.owner_device_id.as_ref().map(|id| id.as_str()),
            serde_json::to_string(&record.followed_by)
                .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
            serde_json::to_string(&record.host_materializations)
                .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
            serde_json::to_string(&record.attention)
                .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
            record.created_at.as_str(),
            record.updated_at.as_str(),
        ],
    )?;
    Ok(())
}

pub(super) fn validate_exposed_base(
    record: &WorkViewRecord,
    descriptor: &WorkViewBaseDescriptor,
    snapshot: &SnapshotRecord,
) -> Result<(), MetadataError> {
    if descriptor.format_version != WORK_VIEW_BASE_DESCRIPTOR_VERSION {
        return Err(MetadataError::InvalidStorageMetadata(
            "unsupported work-view base descriptor version".to_string(),
        ));
    }
    if descriptor.workspace_id != record.workspace_id
        || descriptor.project_id != record.project_id
        || descriptor.work_view_id != record.id
        || descriptor.base_snapshot_id != record.base_snapshot_id
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "work-view base descriptor identity does not match the work view".to_string(),
        ));
    }
    if snapshot.workspace_id != descriptor.workspace_id
        || snapshot.id != descriptor.exposed_snapshot_id
        || snapshot.root_id != descriptor.exposed_namespace_root_id
        || snapshot.semantic_manifest_digest != descriptor.exposed_semantic_manifest_digest
        || snapshot.entry_count != descriptor.exposed_entry_count
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "work-view base descriptor does not match its immutable namespace root".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn replace_exposed_base_records(
    connection: &Connection,
    descriptor: &WorkViewBaseDescriptor,
) -> Result<(), MetadataError> {
    connection.execute(
        "DELETE FROM work_view_base_descriptors
         WHERE workspace_id = ?1 AND work_view_id = ?2",
        params![
            descriptor.workspace_id.as_str(),
            descriptor.work_view_id.as_str()
        ],
    )?;
    connection.execute(
        "INSERT INTO work_view_base_descriptors
         (workspace_id, work_view_id, format_version, project_id, base_snapshot_id,
          project_prefix, policy_fingerprint, exposed_snapshot_id, exposed_namespace_root_id,
          exposed_semantic_manifest_digest, exposed_entry_count, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            descriptor.workspace_id.as_str(),
            descriptor.work_view_id.as_str(),
            i64::from(descriptor.format_version),
            descriptor.project_id.as_str(),
            descriptor.base_snapshot_id.as_str(),
            descriptor.project_prefix.as_str(),
            descriptor.policy_fingerprint.as_str(),
            descriptor.exposed_snapshot_id.as_str(),
            descriptor.exposed_namespace_root_id.as_str(),
            descriptor.exposed_semantic_manifest_digest.as_str(),
            i64::try_from(descriptor.exposed_entry_count).map_err(|_| {
                MetadataError::InvalidStorageMetadata(
                    "work-view exposed entry count exceeds SQLite range".to_string(),
                )
            })?,
            descriptor.created_at.as_str(),
        ],
    )?;
    connection.execute(
        "UPDATE work_views
            SET base_descriptor_version = ?3,
                exposed_snapshot_id = ?4,
                policy_fingerprint = ?5,
                base_review_reason = NULL
          WHERE workspace_id = ?1 AND id = ?2",
        params![
            descriptor.workspace_id.as_str(),
            descriptor.work_view_id.as_str(),
            i64::from(descriptor.format_version),
            descriptor.exposed_snapshot_id.as_str(),
            descriptor.policy_fingerprint.as_str(),
        ],
    )?;
    Ok(())
}

impl MetadataStore {
    pub fn delete_unpublished_work_view(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
    ) -> Result<bool, MetadataError> {
        let changed = self.connection.execute(
            "DELETE FROM work_views
             WHERE workspace_id = ?1 AND id = ?2 AND lifecycle = 'review-ready'
               AND host_materializations_json = '[]'",
            params![workspace_id.as_str(), work_view_id.as_str()],
        )?;
        Ok(changed == 1)
    }

    pub fn upsert_work_view(&self, record: &WorkViewRecord) -> Result<(), MetadataError> {
        let project_path =
            self.workspace_relative_path(&record.workspace_id, &record.project_path)?;
        upsert_work_view_record(&self.connection, record, &project_path)
    }

    pub fn record_materialized_overlay_receipt(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
        overlay_root_id: &str,
        encoded_overlay: &str,
    ) -> Result<(), MetadataError> {
        let changed = self.connection.execute(
            "UPDATE work_views
                SET materialized_overlay_root_id = ?3,
                    materialized_overlay_manifest_json = ?4
              WHERE workspace_id = ?1 AND id = ?2",
            params![
                workspace_id.as_str(),
                work_view_id.as_str(),
                overlay_root_id,
                encoded_overlay,
            ],
        )?;
        if changed != 1 {
            return Err(MetadataError::InvalidStorageMetadata(
                "materialized overlay receipt has no work view".to_string(),
            ));
        }
        Ok(())
    }

    pub fn commit_materialized_overlay(
        &self,
        record: &WorkViewRecord,
        overlay_root_id: &str,
        encoded_overlay: &str,
    ) -> Result<(), MetadataError> {
        let project_path =
            self.workspace_relative_path(&record.workspace_id, &record.project_path)?;
        let transaction = self.connection.unchecked_transaction()?;
        upsert_work_view_record(&transaction, record, &project_path)?;
        let changed = transaction.execute(
            "UPDATE work_views
                SET materialized_overlay_root_id = ?3,
                    materialized_overlay_manifest_json = ?4
              WHERE workspace_id = ?1 AND id = ?2",
            params![
                record.workspace_id.as_str(),
                record.id.as_str(),
                overlay_root_id,
                encoded_overlay,
            ],
        )?;
        if changed != 1 {
            return Err(MetadataError::InvalidStorageMetadata(
                "materialized overlay receipt has no work view".to_string(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn materialized_overlay_receipt(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
    ) -> Result<Option<(String, String)>, MetadataError> {
        self.connection
            .query_row(
                "SELECT materialized_overlay_root_id, materialized_overlay_manifest_json
                   FROM work_views
                  WHERE workspace_id = ?1 AND id = ?2
                    AND materialized_overlay_root_id IS NOT NULL
                    AND materialized_overlay_manifest_json IS NOT NULL",
                params![workspace_id.as_str(), work_view_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn insert_work_view_with_exposed_base(
        &self,
        record: &WorkViewRecord,
        descriptor: &WorkViewBaseDescriptor,
    ) -> Result<(), MetadataError> {
        let snapshot = self
            .snapshot(&record.workspace_id, &descriptor.exposed_snapshot_id)?
            .ok_or_else(|| {
                MetadataError::InvalidStorageMetadata(
                    "work-view exposed snapshot root is missing".to_string(),
                )
            })?;
        validate_exposed_base(record, descriptor, &snapshot)?;
        if !self
            .snapshot_root_completeness(&record.workspace_id, &snapshot.id)?
            .complete
        {
            return Err(MetadataError::InvalidStorageMetadata(
                "work-view exposed snapshot graph is incomplete".to_string(),
            ));
        }
        let project_path =
            self.workspace_relative_path(&record.workspace_id, &record.project_path)?;
        let transaction = self.connection.unchecked_transaction()?;
        upsert_work_view_record(&transaction, record, &project_path)?;
        replace_exposed_base_records(&transaction, descriptor)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn work_view_exposed_base(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
    ) -> Result<Option<WorkViewBaseDescriptor>, MetadataError> {
        self.connection
            .query_row(
                "SELECT format_version, project_id, base_snapshot_id, project_prefix,
                        policy_fingerprint, exposed_snapshot_id, exposed_namespace_root_id,
                        exposed_semantic_manifest_digest, exposed_entry_count, created_at
                   FROM work_view_base_descriptors
                  WHERE workspace_id = ?1 AND work_view_id = ?2",
                params![workspace_id.as_str(), work_view_id.as_str()],
                |row| {
                    Ok(WorkViewBaseDescriptor {
                        format_version: row.get(0)?,
                        workspace_id: workspace_id.clone(),
                        project_id: ProjectId::new(row.get::<_, String>(1)?),
                        work_view_id: work_view_id.clone(),
                        base_snapshot_id: SnapshotId::new(row.get::<_, String>(2)?),
                        project_prefix: row.get(3)?,
                        policy_fingerprint: row.get(4)?,
                        exposed_snapshot_id: SnapshotId::new(row.get::<_, String>(5)?),
                        exposed_namespace_root_id: NamespacePageId::new(row.get::<_, String>(6)?),
                        exposed_semantic_manifest_digest: ManifestDigest::new(
                            row.get::<_, String>(7)?,
                        ),
                        exposed_entry_count: row.get(8)?,
                        created_at: row.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn work_view_base_state(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
    ) -> Result<WorkViewBaseState, MetadataError> {
        let review_reason = self
            .connection
            .query_row(
                "SELECT base_review_reason FROM work_views
                  WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id.as_str(), work_view_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        if review_reason.as_deref() == Some("legacy-base-unverifiable") {
            return Ok(WorkViewBaseState::LegacyUnverifiable);
        }
        match self.work_view_exposed_base(workspace_id, work_view_id)? {
            Some(descriptor) => Ok(WorkViewBaseState::Authoritative {
                descriptor: Box::new(descriptor),
            }),
            None => Ok(WorkViewBaseState::Missing),
        }
    }

    pub fn work_views(
        &self,
        workspace_id: &WorkspaceId,
        include_hidden: bool,
        current_device_id: Option<&DeviceId>,
    ) -> Result<Vec<WorkViewRecord>, MetadataError> {
        let base_query = "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                                base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                                sync_state, retention_state, retain_until, restorable, owner_device_id,
                                followed_by_json, host_materializations_json, attention_json,
                                created_at, updated_at
                         FROM work_views";
        let order_by = "ORDER BY updated_at DESC, project_path, name";
        let mut rows = Vec::new();
        if include_hidden {
            let mut statement = self.connection.prepare(&format!(
                "{base_query}
                 WHERE workspace_id = ?1
                 {order_by}",
            ))?;
            let mapped = statement.query_map([workspace_id.as_str()], work_view_from_row)?;
            for row in mapped {
                rows.push(row?);
            }
            return Ok(rows);
        }

        let mut statement = self.connection.prepare(&format!(
            "{base_query}
             WHERE workspace_id = ?1
               AND visibility != 'hidden'
               AND lifecycle IN ('active', 'review-ready')
               AND (
                 visibility IN ('pinned', 'followed')
                 OR lifecycle = 'review-ready'
                 OR owner_device_id IS NULL
                 OR owner_device_id = ?2
                 OR followed_by_json LIKE ?3
               )
             {order_by}",
        ))?;
        let current_device_id = current_device_id
            .map(DeviceId::as_str)
            .unwrap_or_default()
            .to_string();
        let followed_token = format!("%\"{current_device_id}\"%");
        let mapped = statement.query_map(
            params![workspace_id.as_str(), current_device_id, followed_token],
            work_view_from_row,
        )?;
        for row in mapped {
            rows.push(row?);
        }
        Ok(rows)
    }

    pub fn work_view_by_id(
        &self,
        workspace_id: &WorkspaceId,
        id: &WorkViewId,
    ) -> Result<Option<WorkViewRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                        base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                        sync_state, retention_state, retain_until, restorable, owner_device_id,
                        followed_by_json, host_materializations_json, attention_json,
                        created_at, updated_at
                 FROM work_views
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), id.as_str()],
                work_view_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn work_views_by_name(
        &self,
        workspace_id: &WorkspaceId,
        project_id: Option<&ProjectId>,
        name: &str,
    ) -> Result<Vec<WorkViewRecord>, MetadataError> {
        if let Some(project_id) = project_id {
            let mut statement = self.connection.prepare(
                "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                        base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                        sync_state, retention_state, retain_until, restorable, owner_device_id,
                        followed_by_json, host_materializations_json, attention_json,
                        created_at, updated_at
                 FROM work_views
                 WHERE workspace_id = ?1 AND project_id = ?2 AND name = ?3 COLLATE NOCASE
                 ORDER BY updated_at DESC",
            )?;
            let rows = statement.query_map(
                params![workspace_id.as_str(), project_id.as_str(), name],
                work_view_from_row,
            )?;
            return rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        }

        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                    base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                    sync_state, retention_state, retain_until, restorable, owner_device_id,
                    followed_by_json, host_materializations_json, attention_json,
                    created_at, updated_at
             FROM work_views
             WHERE workspace_id = ?1 AND name = ?2 COLLATE NOCASE
             ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map(params![workspace_id.as_str(), name], work_view_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
