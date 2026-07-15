use super::common::*;
use super::*;

impl MetadataStore {
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

    pub fn pack_record_by_id(
        &self,
        workspace_id: &WorkspaceId,
        pack_id: &PackId,
    ) -> Result<Option<PackRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, byte_len, object_hash, key_epoch, state, retain_until
                 FROM packs
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), pack_id.as_str()],
                |row| {
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
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn delete_pack_record(
        &self,
        workspace_id: &WorkspaceId,
        pack_id: &PackId,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "DELETE FROM packs
             WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id.as_str(), pack_id.as_str()],
        )?;
        Ok(changed as u64)
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

    pub fn delete_content_locator(
        &self,
        workspace_id: &WorkspaceId,
        content_id: &ContentId,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "DELETE FROM content_locators
             WHERE workspace_id = ?1 AND content_id = ?2",
            params![workspace_id.as_str(), content_id.as_str()],
        )?;
        Ok(changed as u64)
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
