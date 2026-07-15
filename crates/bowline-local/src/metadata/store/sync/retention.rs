use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalMetadataRetentionPolicy {
    pub local_write_retention_days: i64,
    pub restore_point_retention_days: i64,
    pub restore_point_min_keep: u64,
    pub completed_sync_retention_days: i64,
    pub completed_sync_min_keep: u64,
    pub snapshot_gc_grace_days: i64,
    pub snapshot_delete_batch: u64,
    pub metadata_gc_batch: u64,
    pub metadata_cache_delete_batch: u64,
}

impl Default for LocalMetadataRetentionPolicy {
    fn default() -> Self {
        Self {
            local_write_retention_days: 30,
            restore_point_retention_days: 30,
            restore_point_min_keep: 500,
            completed_sync_retention_days: 30,
            completed_sync_min_keep: 500,
            snapshot_gc_grace_days: 1,
            snapshot_delete_batch: 128,
            metadata_gc_batch: 256,
            metadata_cache_delete_batch: 128,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalMetadataPruneReport {
    pub local_writes_deleted: u64,
    pub completed_sync_deleted: u64,
}

impl MetadataStore {
    pub fn prune_local_metadata(
        &self,
        workspace_id: &WorkspaceId,
        policy: &LocalMetadataRetentionPolicy,
        now: &str,
    ) -> Result<LocalMetadataPruneReport, MetadataError> {
        let local_write_cutoff = retention_cutoff(now, policy.local_write_retention_days)?;
        let sync_cutoff = retention_cutoff(now, policy.completed_sync_retention_days)?;

        let completed_sync_deleted = self.connection.execute(
            "DELETE FROM sync_operations
             WHERE workspace_id = ?1
               AND state = 'completed'
               AND updated_at < ?2
               AND NOT EXISTS (
                 SELECT 1 FROM local_write_log
                 WHERE local_write_log.workspace_id = sync_operations.workspace_id
                   AND local_write_log.causation_id = sync_operations.id
                   AND (
                     local_write_log.settled_at = ''
                     OR local_write_log.settled_at >= ?4
                   )
               )
               AND id NOT IN (
                 SELECT id FROM sync_operations
                 WHERE workspace_id = ?1 AND state = 'completed'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?3
               )",
            params![
                workspace_id.as_str(),
                sync_cutoff,
                sql_limit(Some(policy.completed_sync_min_keep)),
                local_write_cutoff,
            ],
        )? as u64;

        let local_writes_deleted = self.connection.execute(
            "DELETE FROM local_write_log
             WHERE workspace_id = ?1
               AND settled_at != ''
               AND settled_at < ?2
               AND NOT EXISTS (
                 SELECT 1 FROM sync_operations
                 WHERE sync_operations.workspace_id = local_write_log.workspace_id
                   AND sync_operations.id = local_write_log.causation_id
                   AND sync_operations.state = 'completed'
               )",
            params![workspace_id.as_str(), local_write_cutoff],
        )? as u64;

        Ok(LocalMetadataPruneReport {
            local_writes_deleted,
            completed_sync_deleted,
        })
    }
}

pub(crate) fn retention_cutoff(now: &str, days: i64) -> Result<String, MetadataError> {
    let parsed = time::OffsetDateTime::parse(now, &time::format_description::well_known::Rfc3339)
        .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
    (parsed - time::Duration::days(days))
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))
}
