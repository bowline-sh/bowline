use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bowline_local::sync::manifest_engine::{
    Degradation, EngineEvent, EnginePhase, EngineRef, EngineSnapshot, MaterializationRevision,
};

use super::{
    CoverageScanError, CoverageScanWaiter, EngineConvergenceBarrierError,
    EngineConvergenceBarrierWaiter, EngineSnapshotHandle, EngineSnapshotSink,
    ExactBarrierTimestamp, ObserverBarrierGuard, ObserverEndpoint, RefObserverReadiness,
    SyncBarrierError, SyncBarrierWaiter, WalkStartHook,
};

impl EngineSnapshotHandle {
    /// The most recently published snapshot, or a synthesized `Starting`
    /// snapshot if the lock is momentarily poisoned (never blocks status).
    pub fn current(&self) -> EngineSnapshot {
        self.0
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| starting_snapshot())
    }

    /// Request the engine-owned part of an exact convergence boundary.
    pub fn request_engine_convergence_barrier(
        &self,
    ) -> Result<EngineConvergenceBarrierWaiter, EngineConvergenceBarrierError> {
        let endpoint =
            self.0
                .endpoint
                .lock()
                .map_err(|_| EngineConvergenceBarrierError::Unavailable {
                    reason: "sync barrier state is unavailable",
                })?;
        let endpoint = endpoint
            .as_ref()
            .ok_or(EngineConvergenceBarrierError::Unavailable {
                reason: "manifest sync engine is unavailable",
            })?;
        EngineSnapshotSink {
            shared: Arc::clone(&self.0),
            generation: None,
        }
        .request_engine_barrier(endpoint)
    }

    pub fn request_sync_barrier(&self) -> Result<SyncBarrierWaiter, SyncBarrierError> {
        let endpoint = self
            .0
            .endpoint
            .lock()
            .map_err(|_| SyncBarrierError::Unavailable {
                reason: "sync barrier state is unavailable",
            })?;
        let endpoint = endpoint.as_ref().ok_or(SyncBarrierError::Unavailable {
            reason: "manifest sync engine is unavailable",
        })?;
        let observer = match &endpoint.observer {
            ObserverEndpoint::NotRequired => ObserverBarrierGuard::NotRequired,
            ObserverEndpoint::Starting => {
                return Err(SyncBarrierError::Unavailable {
                    reason: "remote manifest observer is starting",
                });
            }
            ObserverEndpoint::Snapshot(snapshot) => ObserverBarrierGuard::admit(snapshot.clone())?,
        };
        observer.require_live()?;
        let sink = EngineSnapshotSink {
            shared: Arc::clone(&self.0),
            generation: None,
        };
        let inner = sink
            .request_engine_barrier(endpoint)
            .map_err(SyncBarrierError::from_engine)?;
        Ok(SyncBarrierWaiter {
            inner,
            observer,
            engine_admitted_at: ExactBarrierTimestamp::now(),
        })
    }

    /// Request an authoritative local scan while normal engine cycles are paused.
    pub fn request_coverage_scan(
        &self,
        on_walk_start: Option<WalkStartHook>,
    ) -> Result<CoverageScanWaiter, CoverageScanError> {
        let controls = {
            let endpoint = self
                .0
                .endpoint
                .lock()
                .map_err(|_| CoverageScanError::EngineStopped)?;
            Arc::clone(
                &endpoint
                    .as_ref()
                    .ok_or(CoverageScanError::EngineStopped)?
                    .controls,
            )
        };
        controls.request_scan(on_walk_start)
    }

    pub fn observer_readiness(&self) -> Result<Option<RefObserverReadiness>, SyncBarrierError> {
        let endpoint = self
            .0
            .endpoint
            .lock()
            .map_err(|_| SyncBarrierError::Unavailable {
                reason: "sync barrier state is unavailable",
            })?;
        let endpoint = endpoint.as_ref().ok_or(SyncBarrierError::Unavailable {
            reason: "manifest sync engine is unavailable",
        })?;
        match &endpoint.observer {
            ObserverEndpoint::NotRequired => Ok(None),
            ObserverEndpoint::Starting => Err(SyncBarrierError::Unavailable {
                reason: "remote manifest observer is starting",
            }),
            ObserverEndpoint::Snapshot(snapshot) => Ok(Some(snapshot.readiness())),
        }
    }

    /// Authorise the removal batch the engine is currently refusing.
    pub fn confirm_mass_deletion(&self) -> Result<(), EngineCommandError> {
        let events = {
            let endpoint = self
                .0
                .endpoint
                .lock()
                .map_err(|_| EngineCommandError::Unavailable {
                    reason: "manifest sync engine state is unavailable",
                })?;
            endpoint
                .as_ref()
                .ok_or(EngineCommandError::Unavailable {
                    reason: "manifest sync engine is unavailable",
                })?
                .events
                .clone()
        };
        events
            .send(EngineEvent::ConfirmMassDeletion)
            .map_err(|_| EngineCommandError::EngineStopped)
    }
}

/// Why a command to the live engine could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommandError {
    Unavailable { reason: &'static str },
    EngineStopped,
}

impl EngineCommandError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable { reason } => reason,
            Self::EngineStopped => "manifest sync engine stopped before the command was applied",
        }
    }
}

impl std::fmt::Display for EngineCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for EngineCommandError {}

/// Disjoint high revision band for the daemon's synthetic host-status snapshot.
pub(crate) const HOST_STATUS_REVISION: u64 = 1 << 60;

pub fn host_status_snapshot() -> EngineSnapshot {
    snapshot_with_phase(HOST_STATUS_REVISION, EnginePhase::Stopped, false)
}

pub(super) fn starting_snapshot() -> EngineSnapshot {
    snapshot_with_phase(0, EnginePhase::Starting, true)
}

fn snapshot_with_phase(
    revision: u64,
    phase: EnginePhase,
    unattributed_pull_pending: bool,
) -> EngineSnapshot {
    EngineSnapshot {
        revision,
        phase,
        observed_ref: None,
        applied_ref: EngineRef::Genesis,
        materialization_revision: MaterializationRevision::INITIAL,
        pending_intents: 0,
        dirty: 0,
        dirty_paths: Arc::new(BTreeSet::new()),
        dirty_subtree_paths: Arc::new(BTreeSet::new()),
        pending_intent_paths: Arc::new(BTreeSet::new()),
        scan_required: false,
        unattributed_pull_pending,
        cycle_active: false,
        last_success_at: None,
        degradation: Degradation::Nominal,
        unsyncable: Arc::new(BTreeMap::new()),
        refused_removals: Arc::new(BTreeSet::new()),
    }
}
