//! What a cycle observes about the workspace before it decides anything: the
//! root sentinel and the two stat-walk entry points.
//!
//! Split from `mod.rs` at the seam between the driver's *scheduling* (deadlines,
//! events, phases) and its *observation* of the tree. The sentinel lives here
//! because it is the first observation every cycle makes, and because an
//! unproven root must stop a scan before the scan can mistake it for an empty
//! workspace.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::endpoint::{prepare_endpoint_probe_root, refresh_endpoint_capabilities};
use super::stat_walk::{StatWalk, path_is_at_or_below, stat_walk, stat_walk_subtrees};
use super::workspace_root::{self, RootFault};
use super::{
    Clock, CycleError, Degradation, EngineError, EngineIo, FullScanReason, ManifestEngine,
    RemoteObjects, RemoteRef, WorkspacePath,
};

impl ManifestEngine {
    /// Prove the workspace root before any scan, pull, or push.
    ///
    /// A brand-new workspace has no marker yet and this device has no committed
    /// ancestor, so there is nothing to lose: claim the root. Once ancestor rows
    /// exist, a missing marker means the directory under this path is NOT the
    /// workspace those rows describe — an unmounted volume, a rename, a container
    /// bind mount that is not ready — and adopting it would walk an empty tree,
    /// report every ancestor row deleted, and publish a manifest that erases the
    /// workspace on every other trusted device.
    pub(super) fn guard_root(&mut self) -> Result<(), CycleError> {
        let state = match workspace_root::verify_root(&self.ctx) {
            workspace_root::RootState::Ready => workspace_root::RootState::Ready,
            workspace_root::RootState::Faulted(RootFault::MarkerMissing)
                if self.ancestor_is_empty()? =>
            {
                workspace_root::adopt_root(&self.ctx)
                    .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
                workspace_root::RootState::Ready
            }
            faulted => faulted,
        };
        match state {
            workspace_root::RootState::Ready => {
                if let Degradation::RootUnavailable(_) = self.degradation {
                    // The root came back. Re-observe everything: what returned may
                    // be arbitrarily different from what this device last saw —
                    // including the volume itself, so the endpoint's name folding
                    // is measured again rather than carried over. A probe that ran
                    // against a missing root answered with the safe default; this
                    // is where it gets a real answer.
                    let endpoint_probe_root = prepare_endpoint_probe_root(&self.ctx.workspace_root)
                        .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
                    let capabilities = refresh_endpoint_capabilities(&endpoint_probe_root);
                    self.ctx.endpoint_probe_root = endpoint_probe_root;
                    self.ctx.names = capabilities.names;
                    self.ctx.timestamps = capabilities.timestamps;
                    self.set_degradation(Degradation::FullScanRequired(
                        FullScanReason::RootReplaced,
                    ));
                    self.scan_required = true;
                }
                Ok(())
            }
            workspace_root::RootState::Faulted(fault) => Err(CycleError::RootUnavailable(fault)),
        }
    }

    pub(super) fn ancestor_is_empty(&self) -> Result<bool, CycleError> {
        self.store
            .file_count()
            .map(|count| count == 0)
            .map_err(|error| CycleError::Fatal(EngineError::Store(error)))
    }

    pub(super) fn full_scan<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        _io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), CycleError> {
        let policy = crate::policy::UserPolicy::load(&self.ctx.workspace_root)
            .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
        let ancestor = self
            .store
            .all_files()
            .map_err(|error| CycleError::Fatal(EngineError::Store(error)))?;
        self.counters.record_ancestor_rows_read(ancestor.len());
        let walk =
            stat_walk(&self.ctx.workspace_root, &policy, &ancestor).map_err(walk_cycle_error)?;
        self.counters.record_stat_walk(walk.scanned, walk.hashes);
        self.record_walk_unsyncable(&walk, WalkScope::WholeWorkspace)?;
        self.absorb_dirty(walk.dirty);
        Ok(())
    }

    /// Persist a walk's unsyncable verdict, clearing the entries it is entitled
    /// to clear. A walk may only retire an attention item for a path it actually
    /// visited — a scoped walk that cleared the whole set would hide a real
    /// problem elsewhere in the workspace.
    fn record_walk_unsyncable(
        &mut self,
        walk: &StatWalk,
        scope: WalkScope,
    ) -> Result<(), CycleError> {
        let previous = self
            .store
            .unsyncable()
            .map_err(|error| CycleError::Fatal(EngineError::Store(error)))?;
        let resolved = previous
            .keys()
            .filter(|path| !walk.unsyncable.contains_key(*path) && self.walk_visited(scope, path))
            .cloned()
            .collect();
        self.store
            .record_unsyncable(&walk.unsyncable, &resolved)
            .map_err(|error| CycleError::Fatal(EngineError::Store(error)))
    }

    fn walk_visited(&self, scope: WalkScope, path: &WorkspacePath) -> bool {
        match scope {
            WalkScope::WholeWorkspace => true,
            WalkScope::DirtySubtrees => self
                .dirty_subtrees
                .iter()
                .any(|root| path_is_at_or_below(path.as_str(), root.as_str())),
        }
    }

    pub(super) fn scan_dirty_subtrees<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        _io: &EngineIo<'_, O, R, C>,
    ) -> Result<(), CycleError> {
        let scoped_roots = self
            .dirty_subtrees
            .iter()
            .map(|path| path.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let policy =
            crate::policy::UserPolicy::load_scoped(&self.ctx.workspace_root, &scoped_roots)
                .map_err(|error| CycleError::Fatal(EngineError::Io(error)))?;
        let ancestor = self
            .store
            .files_in_scopes(&self.dirty_subtrees)
            .map_err(|error| CycleError::Fatal(EngineError::Store(error)))?;
        self.counters.record_ancestor_rows_read(ancestor.len());
        let walk = stat_walk_subtrees(
            &self.ctx.workspace_root,
            &policy,
            &ancestor,
            &self.dirty_subtrees,
        )
        .map_err(walk_cycle_error)?;
        self.counters.record_stat_walk(walk.scanned, walk.hashes);
        self.record_walk_unsyncable(&walk, WalkScope::DirtySubtrees)?;
        self.absorb_dirty(walk.dirty);
        Arc::make_mut(&mut self.dirty_subtrees).clear();
        Ok(())
    }
}

/// The only failure a stat walk can still raise is its own root check: every
/// per-path condition is recorded in `StatWalk::unsyncable` instead. So a walk
/// error means the workspace root went away between the sentinel and the walk —
/// an unavailable root, which stalls non-destructively and re-probes, never a
/// fatal that kills the engine over a disk that will come back.
fn walk_cycle_error(_error: std::io::Error) -> CycleError {
    CycleError::RootUnavailable(RootFault::Missing)
}

/// How much of the workspace a walk actually visited, and therefore how much of
/// the unsyncable set it is allowed to retire.
#[derive(Clone, Copy)]
enum WalkScope {
    WholeWorkspace,
    DirtySubtrees,
}
