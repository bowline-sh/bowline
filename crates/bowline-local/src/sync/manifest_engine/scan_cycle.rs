//! What a cycle observes about the workspace before it decides anything: the
//! root sentinel and the two stat-walk entry points.
//!
//! Split from `mod.rs` at the seam between the driver's *scheduling* (deadlines,
//! events, phases) and its *observation* of the tree. The sentinel lives here
//! because it is the first observation every cycle makes, and because an
//! unproven root must stop a scan before the scan can mistake it for an empty
//! workspace.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use super::endpoint::{prepare_endpoint_probe_root, refresh_endpoint_capabilities};
use super::stat_walk::{StatWalk, path_is_at_or_below, stat_walk, stat_walk_subtrees};
use super::workspace_root::{self, RootFault};
use super::{
    Clock, CycleError, Degradation, EngineError, EngineIo, FullScanReason, ManifestEngine,
    RemoteObjects, RemoteRef, WorkspacePath,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Engine revision produced by one authoritative local filesystem scan.
pub struct AuthoritativeScanRevision(u64);

impl AuthoritativeScanRevision {
    /// Return the engine revision captured after the scan merged local reality.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Receipt for a scan performed while distributed engine cycles are paused.
pub struct AuthoritativeScanReceipt {
    revision: AuthoritativeScanRevision,
    dirty_paths: usize,
}

#[derive(Debug)]
/// Immutable filesystem work for an authoritative scan.
///
/// Preparing the plan snapshots policy and the committed ancestor on the
/// engine thread. Executing it performs only the filesystem walk, so a daemon
/// may move that expensive work off the engine thread while convergence keeps
/// running.
pub struct AuthoritativeScanPlan {
    root: PathBuf,
    policy: crate::policy::UserPolicy,
    ancestor: BTreeMap<WorkspacePath, super::store::FileRecord>,
    observation_seq: super::dirty_set::DirtySeq,
}

#[derive(Debug)]
/// Complete result of an off-thread authoritative filesystem walk.
pub struct AuthoritativeScanResult {
    walk: Result<StatWalk, RootFault>,
    observation_seq: super::dirty_set::DirtySeq,
}

impl AuthoritativeScanPlan {
    /// Execute the complete stat walk without accessing mutable engine state.
    pub fn execute(self) -> AuthoritativeScanResult {
        let walk = stat_walk(&self.root, &self.policy, &self.ancestor)
            .map_err(|_error| RootFault::Missing);
        AuthoritativeScanResult {
            walk,
            observation_seq: self.observation_seq,
        }
    }
}

impl AuthoritativeScanReceipt {
    /// Return the exact post-scan engine revision.
    pub const fn revision(self) -> AuthoritativeScanRevision {
        self.revision
    }

    /// Return the dirty paths retained for later normal convergence.
    pub const fn dirty_paths(self) -> usize {
        self.dirty_paths
    }
}

#[derive(Debug)]
/// Local-only coverage scan failure. No transport or observer is involved.
pub enum AuthoritativeScanError {
    /// A distributed cycle is still active and must finish before scanning.
    CycleActive,
    /// The workspace root is absent or no longer carries the expected identity.
    RootUnavailable(RootFault),
    /// The engine store or filesystem scan failed unexpectedly.
    Fatal(EngineError),
}

impl std::fmt::Display for AuthoritativeScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CycleActive => formatter.write_str("manifest engine cycle is active"),
            Self::RootUnavailable(fault) => {
                write!(formatter, "workspace root is unavailable: {fault:?}")
            }
            Self::Fatal(error) => write!(formatter, "authoritative scan failed: {error}"),
        }
    }
}

impl std::error::Error for AuthoritativeScanError {}

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

    pub(super) fn full_scan(&mut self) -> Result<(), CycleError> {
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

    /// Snapshot the inputs for an authoritative scan without walking the tree.
    /// Normal convergence remains runnable until the complete result is merged.
    pub fn begin_authoritative_local_scan(
        &mut self,
    ) -> Result<AuthoritativeScanPlan, AuthoritativeScanError> {
        if self.cycle_active || self.authoritative_scan_active {
            return Err(AuthoritativeScanError::CycleActive);
        }
        self.phase = super::EnginePhase::Syncing;
        self.authoritative_scan_active = true;
        self.bump_revision_if_changed();
        if let Err(error) = self.guard_root() {
            self.authoritative_scan_active = false;
            match error {
                CycleError::RootUnavailable(fault) => {
                    self.set_degradation(Degradation::RootUnavailable(fault));
                    self.scan_required = true;
                    self.phase = super::EnginePhase::Stalled;
                    self.bump_revision_if_changed();
                    return Err(AuthoritativeScanError::RootUnavailable(fault));
                }
                other => return Err(authoritative_cycle_error(other)),
            }
        }
        let inputs = (|| {
            let policy = crate::policy::UserPolicy::load(&self.ctx.workspace_root)
                .map_err(|error| AuthoritativeScanError::Fatal(EngineError::Io(error)))?;
            let ancestor = self
                .store
                .all_files()
                .map_err(|error| AuthoritativeScanError::Fatal(EngineError::Store(error)))?;
            Ok((policy, ancestor))
        })();
        let (policy, ancestor) = match inputs {
            Ok(inputs) => inputs,
            Err(error) => {
                self.authoritative_scan_active = false;
                return Err(error);
            }
        };
        self.counters.record_ancestor_rows_read(ancestor.len());

        // This scan owns only the work that existed when it began. A watcher
        // event arriving during the walk may set either flag again and must not
        // be erased when the older scan completes.
        self.scan_required = false;
        Arc::make_mut(&mut self.dirty_subtrees).clear();
        Ok(AuthoritativeScanPlan {
            root: self.ctx.workspace_root.clone(),
            policy,
            ancestor,
            observation_seq: self.next_dirty_seq,
        })
    }

    /// Atomically merge a completed authoritative walk into live engine state.
    pub fn complete_authoritative_local_scan(
        &mut self,
        result: AuthoritativeScanResult,
    ) -> Result<AuthoritativeScanReceipt, AuthoritativeScanError> {
        self.authoritative_scan_active = false;
        let observation_seq = result.observation_seq;
        match result.walk {
            Ok(walk) => {
                self.counters.record_stat_walk(walk.scanned, walk.hashes);
                self.record_walk_unsyncable(&walk, WalkScope::WholeWorkspace)
                    .map_err(authoritative_cycle_error)?;
                self.absorb_authoritative_dirty(walk.dirty, observation_seq);
                if matches!(self.degradation, Degradation::FullScanRequired(_)) {
                    self.set_degradation(Degradation::Nominal);
                }
                self.phase = super::EnginePhase::Syncing;
                self.refresh_durable_state();
                self.bump_revision_if_changed();
                Ok(AuthoritativeScanReceipt {
                    revision: AuthoritativeScanRevision(self.revision),
                    dirty_paths: self.dirty.len(),
                })
            }
            Err(fault) => {
                self.set_degradation(Degradation::RootUnavailable(fault));
                self.scan_required = true;
                self.phase = super::EnginePhase::Stalled;
                self.bump_revision_if_changed();
                Err(AuthoritativeScanError::RootUnavailable(fault))
            }
        }
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

fn authoritative_cycle_error(error: CycleError) -> AuthoritativeScanError {
    match error {
        CycleError::RootUnavailable(fault) => AuthoritativeScanError::RootUnavailable(fault),
        CycleError::Fatal(error) => AuthoritativeScanError::Fatal(error),
        CycleError::Transport
        | CycleError::Integrity
        | CycleError::MassDeletionBlocked { .. }
        | CycleError::PathScoped => AuthoritativeScanError::Fatal(EngineError::Internal),
    }
}

/// How much of the workspace a walk actually visited, and therefore how much of
/// the unsyncable set it is allowed to retire.
#[derive(Clone, Copy)]
enum WalkScope {
    WholeWorkspace,
    DirtySubtrees,
}
