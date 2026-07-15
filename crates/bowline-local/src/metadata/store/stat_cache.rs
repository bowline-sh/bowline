use super::*;
use crate::sync::stat_cache::{
    ContentKeyFingerprint, FileTimestampNanos, StatCacheRow, StatFingerprint,
};

impl MetadataStore {
    pub fn stat_cache_rows(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<BTreeMap<String, StatCacheRow>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT path, size, mtime_ns, ctime_ns, inode, dev, file_mode, key_epoch,
                    content_key_fingerprint, content_id, byte_len, format_version,
                    hashed_at_ns, last_verified_at
             FROM scan_stat_cache
             WHERE workspace_id = ?1
             ORDER BY path",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], stat_cache_row_from_row)?;
        let mut by_path = BTreeMap::new();
        for row in rows {
            let row = row?;
            by_path.insert(row.path.clone(), row);
        }
        Ok(by_path)
    }

    pub fn stat_cache_rows_for_roots(
        &self,
        workspace_id: &WorkspaceId,
        roots: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, StatCacheRow>, MetadataError> {
        if roots.iter().any(|root| root.is_empty()) {
            return self.stat_cache_rows(workspace_id);
        }
        let mut by_path = BTreeMap::new();
        // Lexicographic prefix range (path >= "root/" AND path < "root0") is an
        // indexable seek on the (workspace_id, path) primary key; a `LIKE
        // 'root/%'` predicate cannot use the index and would scan every workspace
        // row per root, reintroducing the O(workspace) cost this loader exists to
        // avoid (Plan 06 U7d boundedness).
        let mut statement = self.connection.prepare(
            "SELECT path, size, mtime_ns, ctime_ns, inode, dev, file_mode, key_epoch,
                    content_key_fingerprint, content_id, byte_len, format_version,
                    hashed_at_ns, last_verified_at
             FROM scan_stat_cache
             WHERE workspace_id = ?1
               AND (path = ?2 OR (path >= ?3 AND path < ?4))
             ORDER BY path",
        )?;
        for root in roots {
            let (lower, upper) = under_root_bounds(root);
            let rows = statement.query_map(
                params![workspace_id.as_str(), root.as_str(), lower, upper],
                stat_cache_row_from_row,
            )?;
            for row in rows {
                let row = row?;
                by_path.insert(row.path.clone(), row);
            }
        }
        Ok(by_path)
    }

    /// Load full cache rows for an explicit, already-bounded set of paths via
    /// primary-key point lookups. Plan 06 U7d loaders feed this the paths from an
    /// indexed change-frontier projection (root-level, or root-level + under
    /// roots), so consultation stays O(paths) rather than scanning the deep
    /// index. Callers must not pass an unbounded/whole-workspace set here — use
    /// [`Self::stat_cache_rows`] for that.
    pub fn stat_cache_rows_for_paths(
        &self,
        workspace_id: &WorkspaceId,
        paths: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, StatCacheRow>, MetadataError> {
        let mut by_path = BTreeMap::new();
        let mut statement = self.connection.prepare(
            "SELECT path, size, mtime_ns, ctime_ns, inode, dev, file_mode, key_epoch,
                    content_key_fingerprint, content_id, byte_len, format_version,
                    hashed_at_ns, last_verified_at
             FROM scan_stat_cache
             WHERE workspace_id = ?1 AND path = ?2",
        )?;
        for path in paths {
            let mut rows = statement.query_map(
                params![workspace_id.as_str(), path.as_str()],
                stat_cache_row_from_row,
            )?;
            if let Some(row) = rows.next() {
                let row = row?;
                by_path.insert(row.path.clone(), row);
            }
        }
        Ok(by_path)
    }

    pub fn apply_stat_cache_write_back(
        &mut self,
        workspace_id: &WorkspaceId,
        upserts: &[StatCacheRow],
        deletes: &BTreeSet<String>,
    ) -> Result<(), MetadataError> {
        self.with_committed(|store| {
            store.apply_stat_cache_write_back_uncommitted(workspace_id, upserts, deletes)
        })
    }

    pub(crate) fn apply_stat_cache_write_back_uncommitted(
        &self,
        workspace_id: &WorkspaceId,
        upserts: &[StatCacheRow],
        deletes: &BTreeSet<String>,
    ) -> Result<(), MetadataError> {
        for row in upserts {
            self.connection.execute(
                "INSERT INTO scan_stat_cache
                 (workspace_id, path, size, mtime_ns, ctime_ns, inode, dev, file_mode,
                  key_epoch, content_key_fingerprint, content_id, byte_len, format_version,
                  hashed_at_ns, last_verified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(workspace_id, path) DO UPDATE SET
                   size = excluded.size,
                   mtime_ns = excluded.mtime_ns,
                   ctime_ns = excluded.ctime_ns,
                   inode = excluded.inode,
                   dev = excluded.dev,
                   file_mode = excluded.file_mode,
                   key_epoch = excluded.key_epoch,
                   content_key_fingerprint = excluded.content_key_fingerprint,
                   content_id = excluded.content_id,
                   byte_len = excluded.byte_len,
                   format_version = excluded.format_version,
                   hashed_at_ns = excluded.hashed_at_ns,
                   last_verified_at = excluded.last_verified_at",
                params![
                    workspace_id.as_str(),
                    row.path.as_str(),
                    i64_from_u64_bits(row.fingerprint.size),
                    row.fingerprint.mtime_ns.as_i64(),
                    row.fingerprint.ctime_ns.as_i64(),
                    i64_from_u64_bits(row.fingerprint.inode),
                    i64_from_u64_bits(row.fingerprint.dev),
                    i64::from(row.fingerprint.file_mode),
                    i64::from(row.key_epoch),
                    row.content_key_fingerprint.as_str(),
                    row.content_id.as_str(),
                    i64_from_u64_bits(row.byte_len),
                    i64::from(row.format_version),
                    row.hashed_at_ns.as_i64(),
                    row.last_verified_at.as_str(),
                ],
            )?;
        }
        for path in deletes {
            self.connection.execute(
                "DELETE FROM scan_stat_cache WHERE workspace_id = ?1 AND path = ?2",
                params![workspace_id.as_str(), path.as_str()],
            )?;
        }
        Ok(())
    }

    pub fn clear_stat_cache(&self, workspace_id: &WorkspaceId) -> Result<(), MetadataError> {
        self.connection.execute(
            "DELETE FROM scan_stat_cache WHERE workspace_id = ?1",
            [workspace_id.as_str()],
        )?;
        Ok(())
    }

    /// Plan 06 U7a: root-level (`path_depth = 0`) path projection. Seeks the
    /// `idx_scan_stat_cache_root_level` index so a workspace with a large deep
    /// index still consults only root-level rows. `rows_consulted` counts the
    /// index entries visited — equal to the returned paths precisely because the
    /// index bounds the scan (see `stat_cache_root_level_query_plan`).
    pub(crate) fn stat_cache_root_level_projection(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<StatCacheProjection, MetadataError> {
        let mut statement = self.connection.prepare(ROOT_LEVEL_PROJECTION_SQL)?;
        let rows = statement.query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?;
        collect_projection(rows)
    }

    /// Plan 06 U7a: rows-under-root projection via a lexicographic prefix range
    /// on the `(workspace_id, path)` primary key, so only rows in `root/…` (plus
    /// the `root` entry itself) are consulted. An empty root means the whole
    /// workspace.
    pub(crate) fn stat_cache_under_root_projection(
        &self,
        workspace_id: &WorkspaceId,
        root: &str,
    ) -> Result<StatCacheProjection, MetadataError> {
        if root.is_empty() {
            let mut statement = self.connection.prepare(ALL_PATHS_PROJECTION_SQL)?;
            let rows =
                statement.query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?;
            return collect_projection(rows);
        }
        let (lower, upper) = under_root_bounds(root);
        let mut statement = self.connection.prepare(UNDER_ROOT_PROJECTION_SQL)?;
        let rows = statement
            .query_map(params![workspace_id.as_str(), root, lower, upper], |row| {
                row.get::<_, String>(0)
            })?;
        collect_projection(rows)
    }

    /// Query-plan probe for the root-level projection. Tests assert the plan
    /// searches `idx_scan_stat_cache_root_level` rather than scanning the table,
    /// which rejects an `instr(path, '/') = 0`-style full scan that would return
    /// only root rows while still consulting every workspace row. Test-only: this
    /// is a diagnostic, never used on a production path.
    #[cfg(test)]
    pub(crate) fn stat_cache_root_level_query_plan(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<String>, MetadataError> {
        self.query_plan_detail(
            &format!("EXPLAIN QUERY PLAN {ROOT_LEVEL_PROJECTION_SQL}"),
            &[&workspace_id.as_str()],
        )
    }

    /// Query-plan probe for the rows-under-root projection; tests assert it uses
    /// the primary-key range rather than a full table scan. Test-only diagnostic.
    #[cfg(test)]
    pub(crate) fn stat_cache_under_root_query_plan(
        &self,
        workspace_id: &WorkspaceId,
        root: &str,
    ) -> Result<Vec<String>, MetadataError> {
        let (lower, upper) = under_root_bounds(root);
        self.query_plan_detail(
            &format!("EXPLAIN QUERY PLAN {UNDER_ROOT_PROJECTION_SQL}"),
            &[
                &workspace_id.as_str(),
                &root,
                &lower.as_str(),
                &upper.as_str(),
            ],
        )
    }

    #[cfg(test)]
    fn query_plan_detail(
        &self,
        explain_sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<String>, MetadataError> {
        let mut statement = self.connection.prepare(explain_sql)?;
        // EXPLAIN QUERY PLAN rows are (id, parent, notused, detail); detail is index 3.
        let rows = statement.query_map(params, |row| row.get::<_, String>(3))?;
        let mut details = Vec::new();
        for row in rows {
            details.push(row?);
        }
        Ok(details)
    }
}

/// Plan 06 U7a: result of an indexed path projection. `rows_consulted` is the
/// count of index/primary-key entries the query visited; for these bounded
/// projections it equals `paths.len()` because the index confines the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatCacheProjection {
    pub paths: BTreeSet<String>,
    pub rows_consulted: u64,
}

const ROOT_LEVEL_PROJECTION_SQL: &str = "SELECT path FROM scan_stat_cache
     WHERE workspace_id = ?1 AND path_depth = 0
     ORDER BY path";

const UNDER_ROOT_PROJECTION_SQL: &str = "SELECT path FROM scan_stat_cache
     WHERE workspace_id = ?1 AND (path = ?2 OR (path >= ?3 AND path < ?4))
     ORDER BY path";

const ALL_PATHS_PROJECTION_SQL: &str = "SELECT path FROM scan_stat_cache
     WHERE workspace_id = ?1
     ORDER BY path";

fn collect_projection(
    rows: impl Iterator<Item = Result<String, rusqlite::Error>>,
) -> Result<StatCacheProjection, MetadataError> {
    let mut paths = BTreeSet::new();
    let mut rows_consulted = 0_u64;
    for row in rows {
        paths.insert(row?);
        rows_consulted = rows_consulted.saturating_add(1);
    }
    Ok(StatCacheProjection {
        paths,
        rows_consulted,
    })
}

// Successor bounds for a lexicographic prefix range over `root/…`. The prefix is
// `root` + '/'; its successor replaces the trailing '/' (0x2F) with '0' (0x30),
// so `[root/, root0)` captures exactly the paths under `root`.
fn under_root_bounds(root: &str) -> (String, String) {
    (format!("{root}/"), format!("{root}0"))
}

fn stat_cache_row_from_row(row: &rusqlite::Row<'_>) -> Result<StatCacheRow, rusqlite::Error> {
    Ok(StatCacheRow {
        path: row.get(0)?,
        fingerprint: StatFingerprint {
            size: u64_from_i64_bits(row.get(1)?),
            mtime_ns: FileTimestampNanos::new(row.get(2)?),
            ctime_ns: FileTimestampNanos::new(row.get(3)?),
            inode: u64_from_i64_bits(row.get(4)?),
            dev: u64_from_i64_bits(row.get(5)?),
            file_mode: u32_from_i64(row.get(6)?, 6)?,
        },
        key_epoch: u32_from_i64(row.get(7)?, 7)?,
        content_key_fingerprint: ContentKeyFingerprint::new(row.get::<_, String>(8)?),
        content_id: ContentId::new(row.get::<_, String>(9)?),
        byte_len: u64_from_i64_bits(row.get(10)?),
        format_version: u32_from_i64(row.get(11)?, 11)?,
        hashed_at_ns: FileTimestampNanos::new(row.get(12)?),
        last_verified_at: row.get(13)?,
    })
}

fn i64_from_u64_bits(value: u64) -> i64 {
    value as i64
}

fn u64_from_i64_bits(value: i64) -> u64 {
    value as u64
}

fn u32_from_i64(value: i64, index: usize) -> Result<u32, rusqlite::Error> {
    u32::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sync::stat_cache::{
            ContentKeyFingerprint, FileTimestampNanos, STAT_CACHE_FORMAT_VERSION,
            StatCacheDeleteScope, StatCacheSession,
        },
        workspace::TempWorkspace,
    };

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn stat_cache_row_roundtrip_write_back_and_clear() {
        let temp = TempWorkspace::new("metadata-stat-cache").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_cache");
        store
            .insert_workspace(&workspace_id, "Code", "2026-07-04T00:00:00Z")
            .expect("workspace");
        let first = row("app/main.rs", "cid_first");
        let second = row("app/old.rs", "cid_old");
        store
            .apply_stat_cache_write_back(&workspace_id, &[first.clone(), second], &BTreeSet::new())
            .expect("write back");

        let mut deletes = BTreeSet::new();
        deletes.insert("app/old.rs".to_string());
        let updated = row("app/main.rs", "cid_second");
        store
            .apply_stat_cache_write_back(&workspace_id, std::slice::from_ref(&updated), &deletes)
            .expect("second write back");

        let rows = store.stat_cache_rows(&workspace_id).expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows.get("app/main.rs"), Some(&updated));
        assert_ne!(rows.get("app/main.rs"), Some(&first));

        store.clear_stat_cache(&workspace_id).expect("clear");
        assert!(
            store
                .stat_cache_rows(&workspace_id)
                .expect("rows after clear")
                .is_empty()
        );
    }

    #[test]
    fn stat_cache_row_roundtrips_large_u64_fingerprint_fields() {
        let temp = TempWorkspace::new("metadata-stat-cache-large-u64").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_cache_large");
        store
            .insert_workspace(&workspace_id, "Code", "2026-07-04T00:00:00Z")
            .expect("workspace");
        let mut large = row("app/large.bin", "cid_large");
        large.fingerprint.size = u64::MAX;
        large.fingerprint.inode = u64::MAX - 1;
        large.fingerprint.dev = u64::MAX - 2;
        large.byte_len = u64::MAX - 3;

        store
            .apply_stat_cache_write_back(
                &workspace_id,
                std::slice::from_ref(&large),
                &BTreeSet::new(),
            )
            .expect("write back");

        let rows = store.stat_cache_rows(&workspace_id).expect("rows");
        assert_eq!(rows.get("app/large.bin"), Some(&large));
    }

    #[test]
    fn stat_cache_rows_for_roots_only_returns_matching_subtrees() {
        let temp = TempWorkspace::new("metadata-stat-cache-scoped").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_cache_scoped");
        store
            .insert_workspace(&workspace_id, "Code", "2026-07-04T00:00:00Z")
            .expect("workspace");
        let rows = [
            row("app/src/main.rs", "cid_main"),
            row("app/src/lib.rs", "cid_lib"),
            row("app/tests/main.rs", "cid_test"),
            row("README.md", "cid_readme"),
        ];
        store
            .apply_stat_cache_write_back(&workspace_id, &rows, &BTreeSet::new())
            .expect("write back");

        let roots = BTreeSet::from(["app/src".to_string()]);
        let scoped = store
            .stat_cache_rows_for_roots(&workspace_id, &roots)
            .expect("scoped rows");

        assert_eq!(
            scoped.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["app/src/lib.rs", "app/src/main.rs"]
        );
    }

    #[test]
    fn root_level_projection_returns_only_root_paths() {
        let (_temp, mut store) = seeded_store("projection-root-level");
        let workspace_id = WorkspaceId::new("ws_proj");
        let rows = [
            row("README.md", "cid_readme"),
            row("Cargo.toml", "cid_cargo"),
            row("app/src/main.rs", "cid_main"),
            row("app/tests/main.rs", "cid_test"),
        ];
        store
            .apply_stat_cache_write_back(&workspace_id, &rows, &BTreeSet::new())
            .expect("write back");

        let projection = store
            .stat_cache_root_level_projection(&workspace_id)
            .expect("root-level projection");

        assert_eq!(
            projection
                .paths
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["Cargo.toml", "README.md"]
        );
        assert_eq!(projection.rows_consulted, 2);
    }

    #[test]
    fn root_level_query_plan_uses_index_and_rejects_full_scan() {
        let (_temp, mut store) = seeded_store("projection-plan");
        let workspace_id = WorkspaceId::new("ws_proj");
        store
            .apply_stat_cache_write_back(
                &workspace_id,
                &[
                    row("README.md", "cid_readme"),
                    row("app/main.rs", "cid_main"),
                ],
                &BTreeSet::new(),
            )
            .expect("write back");

        let plan = store
            .stat_cache_root_level_query_plan(&workspace_id)
            .expect("query plan");

        assert!(
            plan.iter()
                .any(|detail| detail.contains("idx_scan_stat_cache_root_level")),
            "root-level query must search the U7a index, plan was {plan:?}"
        );
        assert!(
            !plan.iter().any(|detail| detail.starts_with("SCAN")),
            "root-level query must not full-scan scan_stat_cache, plan was {plan:?}"
        );
    }

    #[test]
    fn under_root_projection_prefix_range_returns_subtree_and_uses_primary_key() {
        let (_temp, mut store) = seeded_store("projection-under-root");
        let workspace_id = WorkspaceId::new("ws_proj");
        let rows = [
            row("app", "cid_app_dir"),
            row("app/src/main.rs", "cid_main"),
            row("app/src/lib.rs", "cid_lib"),
            row("apple/core.rs", "cid_apple"),
            row("README.md", "cid_readme"),
        ];
        store
            .apply_stat_cache_write_back(&workspace_id, &rows, &BTreeSet::new())
            .expect("write back");

        let projection = store
            .stat_cache_under_root_projection(&workspace_id, "app")
            .expect("under-root projection");

        // `apple/core.rs` shares the `app` prefix but is not under the `app` root;
        // the `[app/, app0)` range plus the exact `app` row must exclude it.
        assert_eq!(
            projection
                .paths
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["app", "app/src/lib.rs", "app/src/main.rs"]
        );
        assert_eq!(projection.rows_consulted, 3);

        let plan = store
            .stat_cache_under_root_query_plan(&workspace_id, "app")
            .expect("under-root query plan");
        assert!(
            !plan.iter().any(|detail| detail.starts_with("SCAN")),
            "under-root query must not full-scan scan_stat_cache, plan was {plan:?}"
        );
    }

    #[test]
    fn root_level_projection_consults_only_root_rows_with_many_deep_rows() {
        let (_temp, mut store) = seeded_store("projection-budget");
        let workspace_id = WorkspaceId::new("ws_proj");
        let mut rows = Vec::with_capacity(100_003);
        rows.push(row("README.md", "cid_readme"));
        rows.push(row("Cargo.toml", "cid_cargo"));
        rows.push(row("LICENSE", "cid_license"));
        for index in 0..100_000_u32 {
            rows.push(row(
                &format!("deep/dir{}/file{index}.rs", index % 512),
                "cid_deep",
            ));
        }
        store
            .apply_stat_cache_write_back(&workspace_id, &rows, &BTreeSet::new())
            .expect("write back");

        let projection = store
            .stat_cache_root_level_projection(&workspace_id)
            .expect("root-level projection");

        // 100k deep rows must not change the cost of a root-level lookup.
        assert_eq!(projection.paths.len(), 3);
        assert_eq!(projection.rows_consulted, 3);
    }

    #[test]
    fn stat_cache_rows_for_paths_point_lookups_return_only_requested_present_rows() {
        let (_temp, mut store) = seeded_store("rows-for-paths");
        let workspace_id = WorkspaceId::new("ws_proj");
        store
            .apply_stat_cache_write_back(
                &workspace_id,
                &[
                    row("README.md", "cid_readme"),
                    row("app/src/main.rs", "cid_main"),
                ],
                &BTreeSet::new(),
            )
            .expect("write back");

        let requested = BTreeSet::from(["README.md".to_string(), "missing.txt".to_string()]);
        let rows = store
            .stat_cache_rows_for_paths(&workspace_id, &requested)
            .expect("rows for paths");

        // Only the requested path that exists is returned; a missing path yields
        // no row and the unrequested deep row is never consulted.
        assert_eq!(
            rows.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["README.md"]
        );
    }

    #[test]
    fn root_shallow_session_loads_only_root_rows_with_large_deep_index() {
        let (_temp, mut store) = seeded_store("root-shallow-budget");
        let workspace_id = WorkspaceId::new("ws_proj");
        let mut rows = Vec::with_capacity(100_003);
        rows.push(row("README.md", "cid_readme"));
        rows.push(row("Cargo.toml", "cid_cargo"));
        rows.push(row("LICENSE", "cid_license"));
        for index in 0..100_000_u32 {
            rows.push(row(
                &format!("deep/dir{}/file{index}.rs", index % 512),
                "cid_deep",
            ));
        }
        store
            .apply_stat_cache_write_back(&workspace_id, &rows, &BTreeSet::new())
            .expect("write back");

        let session = StatCacheSession::load_root_level(&store, &workspace_id, 1, &KEY)
            .expect("root-level session");

        // The root-shallow session loads O(root entries); the 100k deep rows do
        // not enter its working set, so the deep index is untouched this tick.
        assert_eq!(session.loaded_row_count(), 3);
    }

    #[test]
    fn root_shallow_write_back_preserves_deep_stat_cache_rows() {
        let (_temp, mut store) = seeded_store("root-shallow-preserve");
        let workspace_id = WorkspaceId::new("ws_proj");
        store
            .apply_stat_cache_write_back(
                &workspace_id,
                &[
                    row("README.md", "cid_readme"),
                    row("stale-root.txt", "cid_stale"),
                    row("app/src/main.rs", "cid_main"),
                ],
                &BTreeSet::new(),
            )
            .expect("write back");

        // A root-shallow tick observes only root-level children and observed
        // README.md; the deep index is never loaded.
        let mut session = StatCacheSession::load_root_level(&store, &workspace_id, 1, &KEY)
            .expect("root-level session");
        let observed = BTreeSet::from(["README.md".to_string()]);
        let write_back =
            session.finish_with_delete_scope(&observed, StatCacheDeleteScope::RootLevelOnly);
        store
            .apply_stat_cache_write_back(&workspace_id, &write_back.upserts, &write_back.deletes)
            .expect("apply write back");

        let rows = store.stat_cache_rows(&workspace_id).expect("rows");
        // The deep row survives untouched (so the next tick gets a cache hit, no
        // full re-hash) while the unobserved root-level row is pruned.
        assert!(rows.contains_key("app/src/main.rs"));
        assert_eq!(
            rows.get("app/src/main.rs").map(|r| r.content_id.as_str()),
            Some("cid_main")
        );
        assert!(rows.contains_key("README.md"));
        assert!(!rows.contains_key("stale-root.txt"));
    }

    #[test]
    fn combined_session_loads_root_and_under_roots_but_not_unrelated_deep() {
        let (_temp, mut store) = seeded_store("combined-load");
        let workspace_id = WorkspaceId::new("ws_proj");
        store
            .apply_stat_cache_write_back(
                &workspace_id,
                &[
                    row("README.md", "cid_readme"),
                    row("src/app.rs", "cid_app"),
                    row("other/deep.rs", "cid_other"),
                ],
                &BTreeSet::new(),
            )
            .expect("write back");

        let roots = BTreeSet::from(["src".to_string()]);
        let session =
            StatCacheSession::load_roots_and_root_level(&store, &workspace_id, &roots, 1, &KEY)
                .expect("combined session");

        // Root-level + under-`src` rows load; the unrelated deep row does not.
        assert_eq!(session.loaded_row_count(), 2);
    }

    fn seeded_store(label: &str) -> (TempWorkspace, MetadataStore) {
        let temp = TempWorkspace::new(label).expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&WorkspaceId::new("ws_proj"), "Code", "2026-07-04T00:00:00Z")
            .expect("workspace");
        (temp, store)
    }

    fn row(path: &str, content_id: &str) -> StatCacheRow {
        StatCacheRow {
            path: path.to_string(),
            fingerprint: StatFingerprint {
                size: 10,
                mtime_ns: FileTimestampNanos::new(11),
                ctime_ns: FileTimestampNanos::new(12),
                inode: 13,
                dev: 14,
                file_mode: 0o100644,
            },
            key_epoch: 1,
            content_key_fingerprint: ContentKeyFingerprint::new("0123456789abcdef".to_string()),
            content_id: ContentId::new(content_id),
            byte_len: 10,
            format_version: STAT_CACHE_FORMAT_VERSION,
            hashed_at_ns: FileTimestampNanos::new(20),
            last_verified_at: "2026-07-04T00:00:00Z".to_string(),
        }
    }
}
