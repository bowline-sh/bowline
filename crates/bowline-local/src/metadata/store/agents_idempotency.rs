use super::common::*;
use super::*;
use crate::events::LocalEventError;
use bowline_core::events::WorkspaceEvent;

impl MetadataStore {
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
                serialize_json_variant(&record.session_state)?,
                lease_json,
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn persist_agent_lease_with_event(
        &self,
        lease: &AgentLeaseRecord,
        event: WorkspaceEvent,
    ) -> Result<(), LocalEventError> {
        self.in_immediate_transaction(|| {
            self.upsert_agent_lease(lease)?;
            self.append_event(event)?;
            Ok::<(), LocalEventError>(())
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

    pub fn delete_agent_lease(&self, lease_id: &LeaseId) -> Result<u64, MetadataError> {
        self.in_immediate_transaction(|| {
            self.connection.execute(
                "DELETE FROM agent_mcp_tokens WHERE lease_id = ?1",
                params![lease_id.as_str()],
            )?;
            self.connection.execute(
                "DELETE FROM events WHERE lease_id = ?1",
                params![lease_id.as_str()],
            )?;
            self.connection
                .execute(
                    "DELETE FROM leases WHERE id = ?1",
                    params![lease_id.as_str()],
                )
                .map(|changed| changed as u64)
                .map_err(MetadataError::from)
        })
    }

    pub fn rollback_created_agent_work_view_metadata(
        &self,
        lease: &AgentLeaseRecord,
    ) -> Result<(), MetadataError> {
        self.in_immediate_transaction(|| {
            self.connection.execute(
                "DELETE FROM agent_mcp_tokens WHERE lease_id = ?1",
                params![lease.id.as_str()],
            )?;
            self.connection.execute(
                "DELETE FROM leases WHERE id = ?1",
                params![lease.id.as_str()],
            )?;
            self.connection.execute(
                "DELETE FROM events
                 WHERE lease_id = ?1
                    OR (
                      workspace_id = ?2
                      AND project_id = ?3
                      AND json_extract(subject_json, '$.kind') = 'work-view'
                      AND json_extract(subject_json, '$.id') = ?4
                    )",
                params![
                    lease.id.as_str(),
                    lease.workspace_id.as_str(),
                    lease.project_id.as_str(),
                    lease.work_view_id.as_str()
                ],
            )?;
            self.connection.execute(
                "DELETE FROM work_views
                 WHERE workspace_id = ?1 AND id = ?2",
                params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
            )?;
            Ok::<(), MetadataError>(())
        })
    }
}
