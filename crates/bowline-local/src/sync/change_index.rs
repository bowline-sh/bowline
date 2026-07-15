//! `LocalChangeIndex` is the sync engine's local change-frontier facade over
//! stat-cache projections. Scoped scans, write-back pruning, coalescing, and
//! daemon dirty-batch planning ask this primitive for path-frontier information
//! instead of hand-filtering the full stat cache.
//!
//! It is intentionally internal to the sync engine — not a public database
//! product — but it does expose indexed query/projection semantics with a cost
//! summary so callers can reason about rows loaded vs. rows consulted.

use std::collections::{BTreeMap, BTreeSet};

use bowline_core::ids::WorkspaceId;

use super::ScanScope;
use super::stat_cache::StatCacheDeleteScope;
use crate::metadata::{MetadataError, MetadataStore};

/// Cost counters for a change-frontier session (KTD-10/KTD-11). `rows_loaded`
/// and `rows_returned` count paths materialized/handed back; `rows_consulted`
/// counts the index/primary-key entries the queries visited. For the indexed
/// projections `rows_consulted == rows_returned`, which is the boundedness proof;
/// count-only estimate queries consult rows while returning none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChangeIndexCost {
    pub rows_loaded: u64,
    pub rows_consulted: u64,
    pub rows_returned: u64,
    pub rows_pruned: u64,
    /// v1 limitation flag: directory entries are only counted exactly when the
    /// head manifest is seeded. Without it, subtree estimates infer directories
    /// from cached file paths and cannot see empty directories (KTD-14/R11).
    pub directory_counts_available: bool,
}

/// Estimated size of a subtree for cost-aware coalescing. Directory entries are
/// inferred from cached file paths in v1; empty directories are invisible, so
/// `directory_counts_exact` is false unless a manifest is seeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubtreeEstimate {
    pub stat_cache_rows: u64,
    pub inferred_directory_entries: u64,
    pub manifest_entries: u64,
    pub directory_counts_exact: bool,
}

impl SubtreeEstimate {
    /// Conservative entry-count estimate. A directory-heavy subtree with few
    /// files is not deemed cheap on file-row count alone because inferred and
    /// manifest directory entries are added in.
    pub fn estimated_entries(&self) -> u64 {
        self.stat_cache_rows
            .saturating_add(self.inferred_directory_entries)
            .saturating_add(self.manifest_entries)
    }
}

/// Internal facade over the U7a store projections. Query methods take `&mut self`
/// so they can accumulate the cost summary; `delete_scope_for` is a pure mapping
/// with no store access.
pub struct LocalChangeIndex<'a> {
    store: &'a MetadataStore,
    workspace_id: WorkspaceId,
    // Directory + file paths known from the head manifest. When present, subtree
    // estimates count directories exactly; empty means v1 inferred-directory mode.
    manifest_paths: BTreeSet<String>,
    cost: ChangeIndexCost,
}

impl<'a> LocalChangeIndex<'a> {
    pub fn new(store: &'a MetadataStore, workspace_id: WorkspaceId) -> Self {
        Self {
            store,
            workspace_id,
            manifest_paths: BTreeSet::new(),
            cost: ChangeIndexCost::default(),
        }
    }

    /// Seed known head-manifest paths (files and directories) so subtree
    /// estimates can count directories exactly instead of inferring them.
    pub fn with_manifest_paths(mut self, manifest_paths: BTreeSet<String>) -> Self {
        self.cost.directory_counts_available = !manifest_paths.is_empty();
        self.manifest_paths = manifest_paths;
        self
    }

    /// Root-level cached paths (`path_depth = 0`), loaded through the indexed
    /// projection so a large deep index is never scanned.
    pub fn root_level_paths(&mut self) -> Result<BTreeSet<String>, MetadataError> {
        let projection = self
            .store
            .stat_cache_root_level_projection(&self.workspace_id)?;
        self.record_projection(projection.rows_consulted, projection.paths.len());
        Ok(projection.paths)
    }

    /// Cached paths under any of `roots`, loaded through the indexed prefix-range
    /// projection. Callers must use this instead of hand-filtering the full map.
    pub fn paths_under_roots(
        &mut self,
        roots: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, MetadataError> {
        let mut union = BTreeSet::new();
        for root in roots {
            let projection = self
                .store
                .stat_cache_under_root_projection(&self.workspace_id, root)?;
            self.cost.rows_consulted = self
                .cost
                .rows_consulted
                .saturating_add(projection.rows_consulted);
            self.cost.rows_loaded = self
                .cost
                .rows_loaded
                .saturating_add(projection.paths.len() as u64);
            union.extend(projection.paths);
        }
        self.cost.rows_returned = self.cost.rows_returned.saturating_add(union.len() as u64);
        Ok(union)
    }

    /// Estimated entry count for `root`'s subtree. Consults a bounded row count
    /// plus inferred directories (from cached file paths) and, when seeded, exact
    /// manifest directory entries. Returns an estimate, not paths, so it adds to
    /// `rows_consulted` without adding to `rows_returned`.
    pub fn estimated_subtree_entry_count(
        &mut self,
        root: &str,
    ) -> Result<SubtreeEstimate, MetadataError> {
        let projection = self
            .store
            .stat_cache_under_root_projection(&self.workspace_id, root)?;
        self.cost.rows_consulted = self
            .cost
            .rows_consulted
            .saturating_add(projection.rows_consulted);
        self.cost.rows_loaded = self
            .cost
            .rows_loaded
            .saturating_add(projection.paths.len() as u64);
        let stat_cache_rows = projection.paths.len() as u64;
        let inferred_directory_entries = inferred_directories_under_root(&projection.paths, root);
        let manifest_entries = self.manifest_entries_under_root(root);
        Ok(SubtreeEstimate {
            stat_cache_rows,
            inferred_directory_entries,
            manifest_entries,
            directory_counts_exact: !self.manifest_paths.is_empty(),
        })
    }

    /// Record a preview of how many cached rows a delete scope would prune given
    /// the observed paths, feeding `rows_pruned` so the cost summary is complete.
    pub fn record_prune_preview(
        &mut self,
        loaded_rows: &BTreeSet<String>,
        observed_paths: &BTreeSet<String>,
        scope: StatCacheDeleteScope<'_>,
    ) -> u64 {
        let pruned = loaded_rows
            .iter()
            .filter(|path| scope.owns_path(path))
            .filter(|path| !observed_paths.contains(*path))
            .count() as u64;
        self.cost.rows_pruned = self.cost.rows_pruned.saturating_add(pruned);
        pruned
    }

    /// Point-in-time snapshot of the root frontier and per-root estimates for
    /// cost-aware coalescing, also used to feed fake snapshots in tests.
    pub fn snapshot_for_roots(
        &mut self,
        roots: &BTreeSet<String>,
    ) -> Result<ChangeIndexSnapshot, MetadataError> {
        let root_level_paths = self.root_level_paths()?;
        let mut subtree_estimates = BTreeMap::new();
        for root in roots {
            let estimate = self.estimated_subtree_entry_count(root)?;
            subtree_estimates.insert(root.clone(), estimate.estimated_entries());
        }
        Ok(ChangeIndexSnapshot {
            root_level_paths,
            subtree_estimates,
            cost: self.cost,
        })
    }

    pub fn cost_summary(&self) -> &ChangeIndexCost {
        &self.cost
    }

    /// Map a scan scope to the write-back delete scope it authorizes (KTD-4).
    /// Pure mapping — no store access — so the coalescer can call it without an
    /// index instance.
    pub fn delete_scope_for(scan_scope: &ScanScope) -> StatCacheDeleteScope<'_> {
        match scan_scope {
            ScanScope::Full(_) => StatCacheDeleteScope::All,
            ScanScope::RootShallow => StatCacheDeleteScope::RootLevelOnly,
            ScanScope::DirtySubtrees {
                roots,
                root_shallow: false,
            } => StatCacheDeleteScope::UnderRoots(roots),
            ScanScope::DirtySubtrees {
                roots,
                root_shallow: true,
            } => StatCacheDeleteScope::UnderRootsAndRootLevel(roots),
        }
    }

    fn record_projection(&mut self, rows_consulted: u64, rows_returned: usize) {
        self.cost.rows_consulted = self.cost.rows_consulted.saturating_add(rows_consulted);
        self.cost.rows_loaded = self.cost.rows_loaded.saturating_add(rows_returned as u64);
        self.cost.rows_returned = self.cost.rows_returned.saturating_add(rows_returned as u64);
    }

    fn manifest_entries_under_root(&self, root: &str) -> u64 {
        if self.manifest_paths.is_empty() {
            return 0;
        }
        let prefix = if root.is_empty() {
            String::new()
        } else {
            format!("{root}/")
        };
        self.manifest_paths
            .iter()
            .filter(|path| path.as_str() == root || prefix.is_empty() || path.starts_with(&prefix))
            .count() as u64
    }
}

/// Cheap immutable snapshot of the change frontier's estimates and counters.
/// Production callers build real snapshots through
/// [`LocalChangeIndex::snapshot_for_roots`]; tests can build fake ones through
/// [`ChangeIndexSnapshot::from_parts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeIndexSnapshot {
    root_level_paths: BTreeSet<String>,
    subtree_estimates: BTreeMap<String, u64>,
    cost: ChangeIndexCost,
}

impl ChangeIndexSnapshot {
    pub fn from_parts(
        root_level_paths: BTreeSet<String>,
        subtree_estimates: BTreeMap<String, u64>,
        cost: ChangeIndexCost,
    ) -> Self {
        Self {
            root_level_paths,
            subtree_estimates,
            cost,
        }
    }

    pub fn root_level_paths(&self) -> &BTreeSet<String> {
        &self.root_level_paths
    }

    pub fn estimated_subtree_entry_count(&self, root: &str) -> Option<u64> {
        self.subtree_estimates.get(root).copied()
    }

    pub fn cost(&self) -> &ChangeIndexCost {
        &self.cost
    }
}

// Directory entries under `root` inferred from cached file paths: every
// intermediate directory that contains at least one cached file. Empty
// directories are invisible to the stat cache and therefore not counted (the v1
// limitation recorded in `ChangeIndexCost::directory_counts_available`).
fn inferred_directories_under_root(paths: &BTreeSet<String>, root: &str) -> u64 {
    let prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    let mut directories = BTreeSet::new();
    for path in paths {
        let relative = if root.is_empty() {
            path.as_str()
        } else {
            match path.strip_prefix(&prefix) {
                Some(relative) => relative,
                None => continue,
            }
        };
        let segments: Vec<&str> = relative.split('/').collect();
        if segments.len() < 2 {
            continue;
        }
        let mut accumulated = String::new();
        for segment in &segments[..segments.len() - 1] {
            if !accumulated.is_empty() {
                accumulated.push('/');
            }
            accumulated.push_str(segment);
            let directory = if root.is_empty() {
                accumulated.clone()
            } else {
                format!("{root}/{accumulated}")
            };
            directories.insert(directory);
        }
    }
    directories.len() as u64
}

#[cfg(test)]
mod tests;
