use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum DaemonReconcileRequest {
    Normal,
    Full(FullScanReason),
}

// Tri-state, not a bare `is_dir` bool: a vanished root entry (deletion /
// rename-away) has no metadata and must route differently from a confirmed
// directory, or its subtree deletion is masked. Only `NonDirectory` (a present,
// file-like entry — regular file or symlink) earns the fast root-shallow pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum RootEntryKind {
    NonDirectory,
    Directory,
    Unknown,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::daemon) struct DirtyScope {
    roots: BTreeSet<String>,
    // Leaf names of dirty root-level file-like entries. A set, not a bool: the
    // policy guard and future write-back ownership need to know *which* files.
    root_dirty_files: BTreeSet<String>,
    // The `root_dirty_files` drained by the last `take_scope`, kept so a failed
    // `RootShallow`/combined scan can restore them on retry. Root files (unlike
    // subtree roots) are not carried inside the emitted `ScanScope`, so the
    // accumulator must remember them itself.
    drained_root_dirty_files: BTreeSet<String>,
    force_full: Option<FullScanReason>,
}

impl DirtyScope {
    pub(in crate::daemon) fn insert_path_parent(&mut self, path: &str, kind: RootEntryKind) {
        let Some(parent) = dirty_parent(path) else {
            self.force_full(FullScanReason::WatcherOverflow);
            return;
        };
        if !parent.is_empty() {
            self.insert_root(parent);
            return;
        }
        // Root-level path: `path` is the leaf. Check the policy guard before the
        // kind split so an edit, deletion, or rename-away of a root policy input
        // all invalidate the deep classification and force a full rescan.
        if is_root_policy_affecting_path(path) {
            self.force_full(FullScanReason::PolicyChanged);
            return;
        }
        match kind {
            // The only case that earns the fast shallow pass: a confirmed,
            // present file-like entry (regular file or symlink).
            RootEntryKind::NonDirectory => {
                self.root_dirty_files.insert(path.to_string());
            }
            // A created subtree must be recursed; a vanished/unknown one must be
            // scoped-scanned and observed empty rather than shallow-passed, which
            // would re-inject its preserved head entries and mask the deletion.
            RootEntryKind::Directory | RootEntryKind::Unknown => {
                self.insert_root(path.to_string());
            }
        }
    }

    // Re-add dirty roots drained by `take_scope` when the scoped scan that
    // claimed them failed, so a later, narrower scope cannot silently drop the
    // originally-dirty subtrees. Merges against any roots accumulated since
    // (e.g. edits during the retry backoff) with the same subsumption and cap
    // rules as `insert_path_parent`. A pending full scan already subsumes the
    // roots, so restore is a no-op then.
    pub(in crate::daemon) fn restore_roots(&mut self, roots: BTreeSet<String>) {
        if self.force_full.is_some() {
            return;
        }
        for root in roots {
            self.insert_root(root);
            if self.force_full.is_some() {
                return;
            }
        }
    }

    // Re-add root-level dirty files drained by `take_scope` when the shallow /
    // combined scan that claimed them failed, mirroring `restore_roots`. A
    // pending full scan already subsumes the root files, so restore is a no-op
    // then.
    pub(in crate::daemon) fn restore_root_dirty_files(&mut self, files: BTreeSet<String>) {
        if self.force_full.is_some() {
            return;
        }
        self.root_dirty_files.extend(files);
    }

    // Take the root files drained by the last `take_scope`. The retry arm feeds
    // these back through `restore_root_dirty_files`; `RootShallow`/combined
    // scopes carry no root-file payload of their own, so the accumulator is the
    // only place the drained set survives a failed scan.
    pub(in crate::daemon) fn take_drained_root_dirty_files(&mut self) -> BTreeSet<String> {
        std::mem::take(&mut self.drained_root_dirty_files)
    }

    fn insert_root(&mut self, root: String) {
        if self
            .roots
            .iter()
            .any(|existing| root == *existing || root.starts_with(&format!("{existing}/")))
        {
            return;
        }
        self.roots
            .retain(|existing| !existing.starts_with(&format!("{root}/")));
        self.roots.insert(root);
        // No cap-triggered full scan here: `DirtyScope` is a raw accumulator
        // (KTD-16). Breadth beyond `MAX_DIRTY_SUBTREES` is resolved by the
        // `DirtyBatchPlanner` into bounded, cost-aware batches with the remainder
        // deferred, so many dirty top-level projects never force an O(workspace)
        // scan.
    }

    pub(in crate::daemon) fn force_full(&mut self, reason: FullScanReason) {
        self.force_full = Some(reason);
    }

    pub(in crate::daemon) fn take_scope(&mut self, fallback: FullScanReason) -> ScanScope {
        if let Some(reason) = self.force_full.take() {
            self.roots.clear();
            self.root_dirty_files.clear();
            self.drained_root_dirty_files.clear();
            return ScanScope::Full(reason);
        }
        // Drain root files and stash them for retry restore; the emitted scope
        // records only whether a shallow pass runs, never the leaf names.
        self.drained_root_dirty_files = std::mem::take(&mut self.root_dirty_files);
        let root_shallow = !self.drained_root_dirty_files.is_empty();
        if self.roots.is_empty() {
            if root_shallow {
                return ScanScope::RootShallow;
            }
            return ScanScope::Full(fallback);
        }
        ScanScope::DirtySubtrees {
            roots: std::mem::take(&mut self.roots),
            root_shallow,
        }
    }
}

fn dirty_parent(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    Some(
        path.rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_local::policy::POLICY_MARKER_FILENAME;

    #[test]
    fn root_level_watcher_path_yields_root_shallow() {
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("README.md", RootEntryKind::NonDirectory);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::RootShallow
        );
    }

    #[test]
    fn root_level_symlink_yields_root_shallow() {
        // A root symlink is a file-like sync object: the watcher classifies it
        // as `NonDirectory`, so it earns the shallow pass, not a subtree scan.
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("current", RootEntryKind::NonDirectory);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::RootShallow
        );
    }

    #[test]
    fn root_file_plus_deep_subtree_yields_combined_scope() {
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("README.md", RootEntryKind::NonDirectory);
        scope.insert_path_parent("src/app.rs", RootEntryKind::NonDirectory);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::DirtySubtrees {
                roots: BTreeSet::from(["src".to_string()]),
                root_shallow: true,
            }
        );
    }

    #[test]
    fn root_directory_create_routes_to_scoped_subtree() {
        // A created root-level directory must be recursed, not shallow-passed.
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("newrepo", RootEntryKind::Directory);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::DirtySubtrees {
                roots: BTreeSet::from(["newrepo".to_string()]),
                root_shallow: false,
            }
        );
    }

    #[test]
    fn root_directory_deletion_routes_to_scoped_subtree() {
        // A deletion / rename-away carries no metadata (`Unknown`); it must be
        // scoped-scanned and observed empty, never shallow-passed, or the
        // vanished subtree's preserved head entries mask the deletion.
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("oldrepo", RootEntryKind::Unknown);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::DirtySubtrees {
                roots: BTreeSet::from(["oldrepo".to_string()]),
                root_shallow: false,
            }
        );
    }

    #[test]
    fn policy_marker_edit_forces_full_scan() {
        let mut scope = DirtyScope::default();
        scope.insert_path_parent(POLICY_MARKER_FILENAME, RootEntryKind::NonDirectory);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::Full(FullScanReason::PolicyChanged)
        );
    }

    #[test]
    fn policy_marker_deletion_forces_full_scan() {
        // A deleted / renamed-away root policy input still invalidates deep
        // classification; the guard runs before the kind split, so `Unknown`
        // forces a full scan rather than a scoped subtree of `.bowlineignore`.
        let mut scope = DirtyScope::default();
        scope.insert_path_parent(POLICY_MARKER_FILENAME, RootEntryKind::Unknown);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::Full(FullScanReason::PolicyChanged)
        );
    }

    #[test]
    fn policy_marker_dominates_other_root_files() {
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("README.md", RootEntryKind::NonDirectory);
        scope.insert_path_parent(POLICY_MARKER_FILENAME, RootEntryKind::NonDirectory);
        scope.insert_path_parent("src/app.rs", RootEntryKind::NonDirectory);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::Full(FullScanReason::PolicyChanged)
        );
    }

    #[test]
    fn drained_root_shallow_restored_on_retry() {
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("README.md", RootEntryKind::NonDirectory);
        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::RootShallow
        );

        // The shallow scan failed: the retry arm drains the stash and restores
        // it, so the next scope re-emits `RootShallow`.
        let files = scope.take_drained_root_dirty_files();
        assert_eq!(files, BTreeSet::from(["README.md".to_string()]));
        scope.restore_root_dirty_files(files);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::RootShallow
        );
    }

    #[test]
    fn restore_root_dirty_files_yields_to_pending_full_scan() {
        let mut scope = DirtyScope::default();
        scope.force_full(FullScanReason::WatcherOverflow);
        scope.restore_root_dirty_files(BTreeSet::from(["README.md".to_string()]));

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::Full(FullScanReason::WatcherOverflow)
        );
    }

    #[test]
    fn restore_roots_reinstates_failed_scope_alongside_new_edits() {
        let mut scope = DirtyScope::default();
        scope.insert_path_parent("src/app.rs", RootEntryKind::NonDirectory);
        let taken = scope.take_scope(FullScanReason::CliRequested);
        assert_eq!(
            taken,
            ScanScope::DirtySubtrees {
                roots: BTreeSet::from(["src".to_string()]),
                root_shallow: false,
            }
        );

        // An edit lands during the retry backoff, then the failed scan's roots
        // are restored; the retry must cover both subtrees.
        scope.insert_path_parent("docs/readme.md", RootEntryKind::NonDirectory);
        let ScanScope::DirtySubtrees { roots, .. } = taken else {
            panic!("expected scoped scope");
        };
        scope.restore_roots(roots);

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::DirtySubtrees {
                roots: BTreeSet::from(["docs".to_string(), "src".to_string()]),
                root_shallow: false,
            }
        );
    }

    #[test]
    fn restore_roots_yields_to_pending_full_scan() {
        let mut scope = DirtyScope::default();
        scope.force_full(FullScanReason::WatcherOverflow);
        scope.restore_roots(BTreeSet::from(["src".to_string()]));

        assert_eq!(
            scope.take_scope(FullScanReason::CliRequested),
            ScanScope::Full(FullScanReason::WatcherOverflow)
        );
    }
}
