//! Change-proportional ancestor and tree-epoch queries.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, params};

use super::{
    FileRecord, KeyEpoch, ManifestKey, ManifestStore, ManifestStoreError, WorkspacePath, from_i64,
    row_to_file, u32_from_i64,
};

impl ManifestStore {
    pub fn all_files(&self) -> Result<BTreeMap<WorkspacePath, FileRecord>, ManifestStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT path, kind, size, mode, symlink_target, content_id, blob_key, key_epoch, \
             mtime_ns, ctime_ns, inode, dev, hashed_at, verified_at FROM files ORDER BY path",
        )?;
        let rows = statement.query_map([], |row| Ok(row_to_file(row)))?;
        let mut files = BTreeMap::new();
        for row in rows {
            let (path, record) = row??;
            files.insert(path, record);
        }
        Ok(files)
    }

    /// Ancestor rows at or below the requested paths. Each scope is answered by
    /// SQLite's primary-key range rather than materializing the whole workspace.
    pub fn files_in_scopes(
        &self,
        scopes: &BTreeSet<WorkspacePath>,
    ) -> Result<BTreeMap<WorkspacePath, FileRecord>, ManifestStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT path, kind, size, mode, symlink_target, content_id, blob_key, key_epoch, \
             mtime_ns, ctime_ns, inode, dev, hashed_at, verified_at FROM files \
             WHERE path = ?1 OR (path >= ?2 AND path < ?3) ORDER BY path",
        )?;
        let mut files = BTreeMap::new();
        for scope in scopes {
            let prefix = format!("{}/", scope.as_str());
            let upper = format!("{}0", scope.as_str());
            let rows = statement.query_map(params![scope.as_str(), prefix, upper], |row| {
                Ok(row_to_file(row))
            })?;
            for row in rows {
                let (path, record) = row??;
                files.insert(path, record);
            }
        }
        Ok(files)
    }

    /// Ancestor rows at exactly the requested paths. A Merkle diff already
    /// expands real subtree additions/removals into individual touched paths;
    /// treating every touched directory as a prefix scope would make an
    /// unfetched unchanged descendant look remotely deleted.
    pub fn files_at_paths(
        &self,
        paths: &BTreeSet<WorkspacePath>,
    ) -> Result<BTreeMap<WorkspacePath, FileRecord>, ManifestStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT path, kind, size, mode, symlink_target, content_id, blob_key, key_epoch, \
             mtime_ns, ctime_ns, inode, dev, hashed_at, verified_at FROM files WHERE path = ?1",
        )?;
        let mut files = BTreeMap::new();
        for path in paths {
            let row = statement
                .query_row(params![path.as_str()], |row| Ok(row_to_file(row)))
                .optional()?;
            if let Some(row) = row {
                let (path, record) = row?;
                files.insert(path, record);
            }
        }
        Ok(files)
    }

    /// The workspace-root sentinel distinguishes genesis from a replaced root.
    pub fn file_count(&self) -> Result<u64, ManifestStoreError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        from_i64(count)
    }

    pub fn has_files_outside_key_epoch(
        &self,
        key_epoch: KeyEpoch,
    ) -> Result<bool, ManifestStoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE key_epoch IS NOT NULL \
                 AND key_epoch != ?1 LIMIT 1)",
                params![i64::from(key_epoch.get())],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Epoch of the recorded tree node at one physical object key. Missing
    /// means the device cannot prove the applied root belongs to the current
    /// epoch, so callers conservatively rebuild instead of retaining unknown
    /// child nodes across key rotation.
    pub fn tree_node_key_epoch(
        &self,
        node_key: &ManifestKey,
    ) -> Result<Option<KeyEpoch>, ManifestStoreError> {
        self.connection
            .query_row(
                "SELECT key_epoch FROM tree_node_epochs WHERE node_key = ?1",
                params![node_key.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|value| u32_from_i64(value, "key_epoch").map(KeyEpoch::new))
            .transpose()
    }
}
