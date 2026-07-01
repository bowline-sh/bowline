use super::common::*;
use super::*;

impl MetadataStore {
    pub fn command_idempotency_record(
        &self,
        workspace_id: &WorkspaceId,
        idempotency_key: &str,
    ) -> Result<Option<CommandIdempotencyRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, idempotency_key, command, request_hash, result_json,
                        status, created_at, updated_at, expires_at
                 FROM command_idempotency_records
                 WHERE workspace_id = ?1 AND idempotency_key = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), idempotency_key],
                command_idempotency_record_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn upsert_command_idempotency_record(
        &self,
        record: &CommandIdempotencyRecord,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO command_idempotency_records
             (workspace_id, idempotency_key, command, request_hash, result_json, status,
              created_at, updated_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, idempotency_key) DO UPDATE SET
               command = excluded.command,
               result_json = excluded.result_json,
               status = excluded.status,
               updated_at = excluded.updated_at,
               expires_at = excluded.expires_at
             WHERE command_idempotency_records.request_hash = excluded.request_hash",
            params![
                record.workspace_id.as_str(),
                record.idempotency_key.as_str(),
                record.command.as_str(),
                record.request_hash.as_str(),
                record.result_json.as_str(),
                record.status.as_str(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
                record.expires_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn try_insert_command_idempotency_record(
        &self,
        record: &CommandIdempotencyRecord,
    ) -> Result<bool, MetadataError> {
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO command_idempotency_records
             (workspace_id, idempotency_key, command, request_hash, result_json, status,
              created_at, updated_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.workspace_id.as_str(),
                record.idempotency_key.as_str(),
                record.command.as_str(),
                record.request_hash.as_str(),
                record.result_json.as_str(),
                record.status.as_str(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
                record.expires_at.as_str(),
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn finish_command_idempotency_record(
        &self,
        record: &CommandIdempotencyRecord,
    ) -> Result<(), MetadataError> {
        let changed = self.connection.execute(
            "UPDATE command_idempotency_records
             SET result_json = ?4,
                 status = ?5,
                 updated_at = ?6,
                 expires_at = ?7
             WHERE workspace_id = ?1
               AND idempotency_key = ?2
               AND request_hash = ?3",
            params![
                record.workspace_id.as_str(),
                record.idempotency_key.as_str(),
                record.request_hash.as_str(),
                record.result_json.as_str(),
                record.status.as_str(),
                record.updated_at.as_str(),
                record.expires_at.as_str(),
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(MetadataError::InvalidStorageMetadata(
                "idempotency reservation changed before finish".to_string(),
            ))
        }
    }

    pub fn delete_command_idempotency_record(
        &self,
        workspace_id: &WorkspaceId,
        idempotency_key: &str,
        request_hash: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "DELETE FROM command_idempotency_records
             WHERE workspace_id = ?1
               AND idempotency_key = ?2
               AND request_hash = ?3",
            params![workspace_id.as_str(), idempotency_key, request_hash],
        )?;
        Ok(())
    }

    pub fn upsert_agent_lease(&self, record: &AgentLeaseRecord) -> Result<(), MetadataError> {
        let lease_json = serde_json::to_string(record)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        self.connection.execute(
            "INSERT INTO leases (id, workspace_id, project_id, state, lease_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               state = excluded.state,
               lease_json = excluded.lease_json,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_str(),
                serialize_json_variant(&record.execution_state)?,
                lease_json,
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn grant_agent_lease_budget_override(
        &mut self,
        lease: &AgentLeaseRecord,
        override_id: &str,
        added_bytes: u64,
        now: &str,
    ) -> Result<(), MetadataError> {
        let lease_json = serde_json::to_string(lease)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        let lease_state = serialize_json_variant(&lease.execution_state)?;
        self.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO leases (id, workspace_id, project_id, state, lease_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   workspace_id = excluded.workspace_id,
                   project_id = excluded.project_id,
                   state = excluded.state,
                   lease_json = excluded.lease_json,
                   updated_at = excluded.updated_at",
                params![
                    lease.id.as_str(),
                    lease.workspace_id.as_str(),
                    lease.project_id.as_str(),
                    lease_state,
                    lease_json,
                    lease.updated_at.as_str(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO hydration_budget_ledger
                 (id, workspace_id, project_id, lease_id, path, content_id, cause,
                  requested_bytes, reserved_bytes, committed_bytes, outcome, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '.', NULL, 'human-override', ?5, 0, 0,
                         'override-granted', ?6, ?6)",
                params![
                    override_id,
                    lease.workspace_id.as_str(),
                    lease.project_id.as_str(),
                    lease.id.as_str(),
                    added_bytes,
                    now,
                ],
            )?;
            Ok(())
        })
    }

    pub fn agent_lease_by_id(
        &self,
        lease_id: &LeaseId,
    ) -> Result<Option<AgentLeaseRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT lease_json FROM leases WHERE id = ?1 LIMIT 1",
                params![lease_id.as_str()],
                agent_lease_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn agent_leases(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<AgentLeaseRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT lease_json
             FROM leases
             WHERE workspace_id = ?1
             ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], agent_lease_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
