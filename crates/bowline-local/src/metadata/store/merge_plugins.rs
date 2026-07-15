use super::*;
use crate::sync::merge_plugins::{
    MergePluginApprovalInput, MergePluginApprovalRecord, MergePluginIdentity,
};

impl MetadataStore {
    pub fn approve_merge_plugin(
        &self,
        input: &MergePluginApprovalInput,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO merge_plugin_approvals
             (workspace_id, plugin_id, plugin_version, digest, matcher_version,
              validator_version, state, approved_by_device_id, approved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'approved', ?7, ?8)
             ON CONFLICT(
               workspace_id, plugin_id, plugin_version, digest, matcher_version,
               validator_version
             ) DO UPDATE SET
               state = 'approved',
               approved_by_device_id = excluded.approved_by_device_id,
               approved_at = excluded.approved_at",
            params![
                input.workspace_id.as_str(),
                input.plugin.id,
                input.plugin.version,
                input.plugin.digest,
                input.plugin.matcher_version,
                input.plugin.validator_version,
                input.approved_by_device_id.as_str(),
                input.approved_at,
            ],
        )?;
        Ok(())
    }

    pub fn merge_plugin_approvals(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<MergePluginApprovalRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, plugin_id, plugin_version, digest, matcher_version,
                    validator_version, state, approved_by_device_id, approved_at
             FROM merge_plugin_approvals
             WHERE workspace_id = ?1
             ORDER BY plugin_id, plugin_version, digest",
        )?;
        let records = statement
            .query_map(params![workspace_id.as_str()], row_to_merge_plugin_approval)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }
}

fn row_to_merge_plugin_approval(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<MergePluginApprovalRecord> {
    Ok(MergePluginApprovalRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        plugin: MergePluginIdentity {
            id: row.get(1)?,
            version: row.get(2)?,
            digest: row.get(3)?,
            matcher_version: row.get(4)?,
            validator_version: row.get(5)?,
        },
        state: row.get(6)?,
        approved_by_device_id: DeviceId::new(row.get::<_, String>(7)?),
        approved_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn merge_plugin_approval_binds_full_identity_tuple() {
        let temp = TempWorkspace::new("metadata-merge-plugin-approval").expect("temp workspace");
        let store = MetadataStore::open(temp.root().join(".state.sqlite3")).expect("store");
        let workspace_id = WorkspaceId::new("ws_code");
        store
            .insert_workspace(&workspace_id, "Code", "2026-07-02T10:00:00Z")
            .expect("workspace");

        store
            .approve_merge_plugin(&MergePluginApprovalInput {
                workspace_id: workspace_id.clone(),
                plugin: MergePluginIdentity {
                    id: "notebooks".to_string(),
                    version: "1.0.0".to_string(),
                    digest: "blake3:abc".to_string(),
                    matcher_version: "1".to_string(),
                    validator_version: "1".to_string(),
                },
                approved_by_device_id: DeviceId::new("device_local"),
                approved_at: "2026-07-02T10:00:00Z".to_string(),
            })
            .expect("approval");

        let approvals = store
            .merge_plugin_approvals(&workspace_id)
            .expect("approvals");
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].plugin.id, "notebooks");
        assert_eq!(
            approvals[0].plugin.stable_key(),
            "notebooks:1.0.0:blake3:abc:1:1"
        );
    }
}
