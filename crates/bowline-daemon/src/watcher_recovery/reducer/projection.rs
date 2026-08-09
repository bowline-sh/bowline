use super::{AttemptState, RecoveryStateMachine};
#[cfg(test)]
use crate::watcher_recovery::types::{ActivityWatermark, IncidentId, RecoveryCause, RecoveryPhase};
use crate::watcher_recovery::types::{
    AttemptCoverageBoundary, AttemptToken, RecoveryWorkerOwnership,
};
use crate::watcher_recovery::{
    snapshot::{
        NativeCoverageSnapshot, RecoveryClosureSnapshot, RecoveryFailureSnapshot,
        RecoverySnapshotInput, WatcherRecoverySnapshot,
    },
    types::{
        RecoveryClosureIdentity, RecoveryClosureReceipt, RecoveryCount, RecoveryFrontier,
        RecoveryLifecycle, RecoveryRevision,
    },
};

impl RecoveryStateMachine {
    pub(crate) fn source_identity(&self) -> &crate::watcher_recovery::RecoverySourceIdentity {
        &self.source_identity
    }

    pub(crate) fn snapshot(&self) -> WatcherRecoverySnapshot {
        let Some(incident) = &self.incident else {
            return WatcherRecoverySnapshot::from_input(RecoverySnapshotInput {
                process_identity: self.source_identity.process_identity().clone(),
                workspace_id: self.source_identity.workspace_id().clone(),
                snapshot_revision: self.revision,
                activity_watermark: self.activity_watermark,
                lifecycle: RecoveryLifecycle::Nominal,
                worker_id: self.active_worker,
                incident_id: None,
                primary_cause: None,
                phase: None,
                started_at: None,
                last_transition_at: self.last_transition_at,
                attempt_count: RecoveryCount::ZERO,
                scan_count: RecoveryCount::ZERO,
                rescan_required: false,
                current_attempt: None,
                native_coverage: self.last_completed.as_ref().map(|receipt| {
                    NativeCoverageSnapshot::from_boundary(receipt.native_boundary())
                }),
                failure: None,
                last_closure: self
                    .last_completed
                    .as_ref()
                    .map(RecoveryClosureSnapshot::from_receipt),
            });
        };
        WatcherRecoverySnapshot::from_input(RecoverySnapshotInput {
            process_identity: self.source_identity.process_identity().clone(),
            workspace_id: self.source_identity.workspace_id().clone(),
            snapshot_revision: self.revision,
            activity_watermark: self.activity_watermark,
            lifecycle: if incident.is_blocked() {
                RecoveryLifecycle::Blocked
            } else {
                RecoveryLifecycle::Recovering
            },
            worker_id: self.active_worker,
            incident_id: Some(incident.id),
            primary_cause: Some(incident.primary_cause),
            phase: Some(incident.phase),
            started_at: Some(incident.started_at),
            last_transition_at: self.last_transition_at,
            attempt_count: incident.attempt_count,
            scan_count: incident.scan_count,
            rescan_required: incident.rescan_required,
            current_attempt: incident
                .current_attempt
                .as_ref()
                .map(AttemptState::snapshot),
            native_coverage: incident
                .native_coverage
                .as_ref()
                .map(NativeCoverageSnapshot::from_boundary),
            failure: incident
                .failure
                .as_ref()
                .map(RecoveryFailureSnapshot::from_failure),
            last_closure: self
                .last_completed
                .as_ref()
                .map(RecoveryClosureSnapshot::from_receipt),
        })
    }

    #[cfg(test)]
    pub(crate) fn lifecycle(&self) -> RecoveryLifecycle {
        match &self.incident {
            None => RecoveryLifecycle::Nominal,
            Some(incident) if incident.is_blocked() => RecoveryLifecycle::Blocked,
            Some(_) => RecoveryLifecycle::Recovering,
        }
    }

    pub(crate) fn revision(&self) -> RecoveryRevision {
        self.revision
    }

    #[cfg(test)]
    pub(crate) fn activity_watermark(&self) -> ActivityWatermark {
        self.activity_watermark
    }

    #[cfg(test)]
    pub(crate) fn current_incident_id(&self) -> Option<IncidentId> {
        self.incident.as_ref().map(|incident| incident.id)
    }

    #[cfg(test)]
    pub(crate) fn current_attempt(&self) -> Option<AttemptToken> {
        self.incident
            .as_ref()
            .and_then(|incident| incident.current_attempt.as_ref())
            .map(|attempt| attempt.token)
    }

    pub(crate) fn current_worker_ownership(&self) -> Option<RecoveryWorkerOwnership> {
        self.active_worker.map(RecoveryWorkerOwnership::new)
    }

    pub(crate) fn current_native_boundary(&self) -> Option<&AttemptCoverageBoundary> {
        self.incident
            .as_ref()
            .and_then(|incident| incident.native_coverage.as_ref())
    }

    pub(crate) fn current_attempt_token(&self) -> Option<AttemptToken> {
        self.incident
            .as_ref()
            .and_then(|incident| incident.current_attempt.as_ref())
            .map(|attempt| attempt.token)
    }

    #[cfg(test)]
    pub(crate) fn current_scan_revision(
        &self,
    ) -> Option<crate::watcher_recovery::RecoveryScanRevision> {
        self.incident
            .as_ref()
            .and_then(|incident| incident.current_attempt.as_ref())
            .and_then(|attempt| attempt.scan_revision)
    }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> Option<RecoveryPhase> {
        self.incident.as_ref().map(|incident| incident.phase)
    }

    #[cfg(test)]
    pub(crate) fn cause_count(&self, cause: RecoveryCause) -> RecoveryCount {
        self.incident
            .as_ref()
            .map_or(RecoveryCount::ZERO, |incident| {
                incident.cause_counts.get(cause)
            })
    }

    #[cfg(test)]
    pub(crate) fn attempt_count(&self) -> RecoveryCount {
        self.incident
            .as_ref()
            .map_or(RecoveryCount::ZERO, |incident| incident.attempt_count)
    }

    pub(crate) fn last_completed_receipt(&self) -> Option<&RecoveryClosureReceipt> {
        self.last_completed.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn has_open_incident(&self) -> bool {
        self.incident.is_some()
    }

    pub(crate) fn nominal_frontier(&self) -> Option<RecoveryFrontier> {
        if self.incident.is_some() {
            return None;
        }
        Some(RecoveryFrontier::new(
            self.revision,
            self.activity_watermark,
            self.last_completed
                .as_ref()
                .map(RecoveryClosureIdentity::from_receipt),
        ))
    }
}
