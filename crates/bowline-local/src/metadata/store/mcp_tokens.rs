use super::*;

impl MetadataStore {
    pub fn insert_agent_mcp_token(
        &self,
        record: &AgentMcpTokenRecord,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO agent_mcp_tokens
             (id, workspace_id, project_id, lease_id, token_hash, token_file,
              grants_json, expires_at, revoked_at, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_str(),
                record.lease_id.as_str(),
                record.token_hash.as_str(),
                record.token_file.as_str(),
                record.grants_json.as_str(),
                record.expires_at.as_str(),
                record.revoked_at.as_deref(),
                record.created_at.as_str(),
                record.last_used_at.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn agent_mcp_token_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<AgentMcpTokenRecord>, MetadataError> {
        Ok(self
            .connection
            .query_row(
                "SELECT id, workspace_id, project_id, lease_id, token_hash, token_file,
                        grants_json, expires_at, revoked_at, created_at, last_used_at
                 FROM agent_mcp_tokens
                 WHERE token_hash = ?1",
                [token_hash],
                agent_mcp_token_from_row,
            )
            .optional()?)
    }

    pub fn agent_mcp_token_by_file(
        &self,
        token_file: &str,
    ) -> Result<Option<AgentMcpTokenRecord>, MetadataError> {
        Ok(self
            .connection
            .query_row(
                "SELECT id, workspace_id, project_id, lease_id, token_hash, token_file,
                        grants_json, expires_at, revoked_at, created_at, last_used_at
                 FROM agent_mcp_tokens
                 WHERE token_file = ?1",
                [token_file],
                agent_mcp_token_from_row,
            )
            .optional()?)
    }

    pub fn mark_agent_mcp_token_used(
        &self,
        token_hash: &str,
        used_at: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE agent_mcp_tokens SET last_used_at = ?2 WHERE token_hash = ?1",
            params![token_hash, used_at],
        )?;
        Ok(())
    }

    pub fn revoke_agent_mcp_tokens_for_lease(
        &self,
        lease_id: &LeaseId,
        revoked_at: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE agent_mcp_tokens
             SET revoked_at = ?2
             WHERE lease_id = ?1 AND revoked_at IS NULL",
            params![lease_id.as_str(), revoked_at],
        )?;
        Ok(())
    }

    pub fn delete_agent_mcp_tokens_for_lease(
        &self,
        lease_id: &LeaseId,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "DELETE FROM agent_mcp_tokens WHERE lease_id = ?1",
            params![lease_id.as_str()],
        )?;
        Ok(())
    }
}

fn agent_mcp_token_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<AgentMcpTokenRecord, rusqlite::Error> {
    Ok(AgentMcpTokenRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: ProjectId::new(row.get::<_, String>(2)?),
        lease_id: LeaseId::new(row.get::<_, String>(3)?),
        token_hash: row.get(4)?,
        token_file: row.get(5)?,
        grants_json: row.get(6)?,
        expires_at: row.get(7)?,
        revoked_at: row.get(8)?,
        created_at: row.get(9)?,
        last_used_at: row.get(10)?,
    })
}
