//! Plan 06 U7f — `ObservationWriteScope` (KTD-13). A partial scan observed only
//! a slice of the workspace, so it may replace only the metadata that slice
//! owns. This type is the write-back parallel of [`StatCacheDeleteScope`]: same
//! four ownership shapes, applied to observed-path summaries, project
//! observations, env-source/env-record replacement, and scan summaries, so no
//! consumer hand-rolls "which paths did I observe?".
//!
//! The ownership predicate is defined once, on [`StatCacheDeleteScope`]. This
//! type is a distinct domain wrapper (metadata write authority, not stat-cache
//! delete authority) that reuses that single predicate rather than restating it.

use super::ScanScope;
use super::change_index::LocalChangeIndex;
use super::stat_cache::StatCacheDeleteScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationWriteScope<'a> {
    /// Full scan: every observed-metadata table may be replaced wholesale.
    Full,
    /// Scoped subtree scan: only metadata under a dirty root is owned.
    UnderRoots(&'a std::collections::BTreeSet<String>),
    /// Root-shallow scan: only root-level (`!path.contains('/')`) metadata.
    RootLevelOnly,
    /// Combined tick: metadata that is root-level OR under a dirty root.
    UnderRootsAndRootLevel(&'a std::collections::BTreeSet<String>),
}

impl<'a> ObservationWriteScope<'a> {
    /// The write scope a scan scope authorizes. Derived from the single
    /// `ScanScope → delete scope` mapping so the two never drift.
    pub fn for_scan_scope(scan_scope: &'a ScanScope) -> Self {
        match LocalChangeIndex::delete_scope_for(scan_scope) {
            StatCacheDeleteScope::All => Self::Full,
            StatCacheDeleteScope::UnderRoots(roots) => Self::UnderRoots(roots),
            StatCacheDeleteScope::RootLevelOnly => Self::RootLevelOnly,
            StatCacheDeleteScope::UnderRootsAndRootLevel(roots) => {
                Self::UnderRootsAndRootLevel(roots)
            }
        }
    }

    /// True when this scan owns `path`'s metadata, i.e. it observed the directory
    /// that would contain it, so it may replace or prune metadata for that path.
    pub fn owns_path(&self, path: &str) -> bool {
        self.as_delete_scope().owns_path(path)
    }

    /// True when a full scan observed everything and may replace whole tables.
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    fn as_delete_scope(&self) -> StatCacheDeleteScope<'a> {
        match *self {
            Self::Full => StatCacheDeleteScope::All,
            Self::UnderRoots(roots) => StatCacheDeleteScope::UnderRoots(roots),
            Self::RootLevelOnly => StatCacheDeleteScope::RootLevelOnly,
            Self::UnderRootsAndRootLevel(roots) => {
                StatCacheDeleteScope::UnderRootsAndRootLevel(roots)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::sync::FullScanReason;

    #[test]
    fn scan_scope_maps_to_matching_write_scope() {
        let roots = BTreeSet::from(["src".to_string()]);
        assert!(
            ObservationWriteScope::for_scan_scope(&ScanScope::Full(FullScanReason::CliRequested))
                .is_full()
        );
        assert_eq!(
            ObservationWriteScope::for_scan_scope(&ScanScope::RootShallow),
            ObservationWriteScope::RootLevelOnly
        );
        assert_eq!(
            ObservationWriteScope::for_scan_scope(&ScanScope::DirtySubtrees {
                roots: roots.clone(),
                root_shallow: false,
            }),
            ObservationWriteScope::UnderRoots(&roots)
        );
        assert_eq!(
            ObservationWriteScope::for_scan_scope(&ScanScope::DirtySubtrees {
                roots: roots.clone(),
                root_shallow: true,
            }),
            ObservationWriteScope::UnderRootsAndRootLevel(&roots)
        );
    }

    #[test]
    fn ownership_matches_the_delete_scope_predicate() {
        let roots = BTreeSet::from(["src".to_string()]);
        let root_level = ObservationWriteScope::RootLevelOnly;
        assert!(root_level.owns_path(".env"));
        assert!(!root_level.owns_path("app/.env"));

        let combined = ObservationWriteScope::UnderRootsAndRootLevel(&roots);
        assert!(combined.owns_path(".env"));
        assert!(combined.owns_path("src/app/.env"));
        assert!(!combined.owns_path("other/.env"));

        let under = ObservationWriteScope::UnderRoots(&roots);
        assert!(!under.owns_path(".env"));
        assert!(under.owns_path("src/.env"));
    }
}
