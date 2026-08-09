use bowline_local::sync::manifest_engine::{EngineConvergenceReceipt, EngineRef};

use crate::manifest_transport::{
    RefObserverFrontier, RefObserverReadiness, RefObserverSnapshot, RefObserverSnapshotHandle,
    VerifiedWorkspaceRefView,
};

use super::{ExactBarrierTimestamp, SyncBarrierError};

#[derive(Debug)]
pub(super) enum ObserverEndpoint {
    NotRequired,
    Starting,
    Snapshot(RefObserverSnapshotHandle),
}

#[derive(Clone)]
pub(super) enum ObserverBarrierGuard {
    NotRequired,
    Snapshot {
        snapshot: RefObserverSnapshotHandle,
        admission: RefObserverFrontier,
        admitted_at: ExactBarrierTimestamp,
    },
}

impl ObserverBarrierGuard {
    pub(super) fn admit(snapshot: RefObserverSnapshotHandle) -> Result<Self, SyncBarrierError> {
        let admission = exact_observer_frontier(&snapshot.current())?;
        Ok(Self::Snapshot {
            snapshot,
            admission,
            admitted_at: ExactBarrierTimestamp::now(),
        })
    }

    pub(super) fn admitted_at(&self) -> Option<&ExactBarrierTimestamp> {
        let Self::Snapshot { admitted_at, .. } = self else {
            return None;
        };
        Some(admitted_at)
    }

    pub(super) fn require_live(&self) -> Result<(), SyncBarrierError> {
        let Self::Snapshot { snapshot, .. } = self else {
            return Ok(());
        };
        match snapshot.readiness() {
            RefObserverReadiness::Live => Ok(()),
            RefObserverReadiness::Retrying => Err(SyncBarrierError::Unavailable {
                reason: "remote manifest observer has not delivered its initial value",
            }),
            RefObserverReadiness::Blocked { class, code } => {
                Err(SyncBarrierError::ObserverBlocked { class, code })
            }
        }
    }

    pub(super) fn require_unchanged(
        &self,
    ) -> Result<Option<RefObserverFrontier>, SyncBarrierError> {
        let Self::Snapshot {
            snapshot,
            admission,
            ..
        } = self
        else {
            return Ok(None);
        };
        let completion = exact_observer_frontier(&snapshot.current())?;
        if completion != *admission {
            return Err(SyncBarrierError::Unavailable {
                reason: "remote manifest observer frontier changed during convergence",
            });
        }
        Ok(Some(completion))
    }

    pub(super) fn validate_completion(
        &self,
        engine: &EngineConvergenceReceipt,
    ) -> Result<Option<RefObserverFrontier>, SyncBarrierError> {
        let frontier = self.require_unchanged()?;
        if frontier
            .as_ref()
            .is_some_and(|frontier| !observer_frontier_matches_engine(frontier, engine))
        {
            return Err(SyncBarrierError::Unavailable {
                reason: "engine convergence does not match the exact observer frontier",
            });
        }
        Ok(frontier)
    }
}

fn exact_observer_frontier(
    snapshot: &RefObserverSnapshot,
) -> Result<RefObserverFrontier, SyncBarrierError> {
    if let RefObserverReadiness::Blocked { class, code } = snapshot.readiness() {
        return Err(SyncBarrierError::ObserverBlocked { class, code });
    }
    snapshot.frontier().ok_or(SyncBarrierError::Unavailable {
        reason: "remote manifest observer has not established an exact frontier",
    })
}

fn observer_frontier_matches_engine(
    frontier: &RefObserverFrontier,
    engine: &EngineConvergenceReceipt,
) -> bool {
    if frontier.authority_source.workspace_identity() != engine.workspace_identity()
        || frontier.authority_source.process_identity() != engine.process_identity()
        || frontier.authority_source.endpoint_generation().get() != engine.endpoint_generation().0
    {
        return false;
    }
    match frontier.verified_ref.view() {
        VerifiedWorkspaceRefView::Genesis => {
            engine.observed_ref() == &EngineRef::Genesis
                && engine.applied_ref() == &EngineRef::Genesis
        }
        VerifiedWorkspaceRefView::Head {
            version,
            manifest_key,
        } => {
            let expected = bowline_local::sync::manifest_engine::RefObservation {
                version,
                manifest_key: manifest_key.clone(),
            };
            engine.observed_ref() == &EngineRef::Head(expected.clone())
                && engine.applied_ref() == &EngineRef::Head(expected)
        }
    }
}
