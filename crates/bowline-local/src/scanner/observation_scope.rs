//! `ObservationWriteScope` — a partial scan observed only a slice of the
//! workspace, so it may replace only the metadata that slice owns. Moved here
//! from the deleted old-sync engine; the scanner and its env-observation
//! consumer are the surviving owners. The scan-scope derivation
//! (`for_scan_scope`) died with the old engine's `ScanScope`; the write-back
//! consumers pass the shape they observed directly.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationWriteScope<'a> {
    /// Full scan: every observed-metadata table may be replaced wholesale.
    Full,
    /// Scoped subtree scan: only metadata under a dirty root is owned.
    UnderRoots(&'a BTreeSet<String>),
    /// Root-shallow scan: only root-level (`!path.contains('/')`) metadata.
    RootLevelOnly,
    /// Combined tick: metadata that is root-level OR under a dirty root.
    UnderRootsAndRootLevel(&'a BTreeSet<String>),
}

impl ObservationWriteScope<'_> {
    /// True when this scan owns `path`'s metadata, i.e. it observed the
    /// directory that would contain it, so it may replace or prune that path.
    pub fn owns_path(&self, path: &str) -> bool {
        match self {
            Self::Full => true,
            Self::RootLevelOnly => is_root_level(path),
            Self::UnderRoots(roots) => path_is_under_any_root(path, roots),
            Self::UnderRootsAndRootLevel(roots) => {
                is_root_level(path) || path_is_under_any_root(path, roots)
            }
        }
    }

    /// True when a full scan observed everything and may replace whole tables.
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

fn is_root_level(path: &str) -> bool {
    !path.contains('/')
}

fn path_is_under_any_root(path: &str, roots: &BTreeSet<String>) -> bool {
    roots.iter().any(|root| {
        root.is_empty()
            || path == root
            || (path.len() > root.len()
                && path.starts_with(root)
                && path.as_bytes()[root.len()] == b'/')
    })
}
