use super::{
    IdAllocation, IncidentState, RecoveryStateMachine, matching_attempt, matching_attempt_mut,
};
use crate::watcher_recovery::types::{
    AttemptToken, DependencyFailure, IncidentId, LossAdmission, RecoveryCause, RecoveryFailureCode,
    RecoveryIdentifierKind, RecoveryMoment, RecoveryPhase, RecoveryRevision, RecoveryScanRevision,
    RecoveryTransitionError,
};

impl RecoveryStateMachine {
    pub(super) fn open_incident(
        &mut self,
        cause: RecoveryCause,
        moment: RecoveryMoment,
        next_revision: RecoveryRevision,
    ) -> Result<LossAdmission, RecoveryTransitionError> {
        match self.next_incident_id.allocate() {
            IdAllocation::Available(value) => {
                let incident_id = IncidentId::new(value).map_err(|_| {
                    RecoveryTransitionError::IdentifierExhausted {
                        kind: RecoveryIdentifierKind::Incident,
                    }
                })?;
                self.incident = Some(IncidentState::new(incident_id, cause, moment));
                self.commit(moment.observed_at, next_revision);
                Ok(LossAdmission::Opened { incident_id })
            }
            IdAllocation::Terminal(value) => {
                let incident_id = IncidentId::new(value).map_err(|_| {
                    RecoveryTransitionError::IdentifierExhausted {
                        kind: RecoveryIdentifierKind::Incident,
                    }
                })?;
                let mut incident = IncidentState::new(incident_id, cause, moment);
                incident.block(DependencyFailure::fatal_contract(
                    RecoveryFailureCode::RecoveryIncidentIdExhausted,
                    moment.observed_at,
                ));
                self.incident = Some(incident);
                self.commit(moment.observed_at, next_revision);
                Err(RecoveryTransitionError::IdentifierExhausted {
                    kind: RecoveryIdentifierKind::Incident,
                })
            }
        }
    }

    pub(super) fn begin_authoritative_scan(
        &mut self,
        token: AttemptToken,
        moment: RecoveryMoment,
        next_revision: RecoveryRevision,
    ) -> Result<(), RecoveryTransitionError> {
        let incident = self.active_incident()?;
        if incident.phase != RecoveryPhase::AwaitingCoverage {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "start authoritative scan",
                phase: incident.phase,
            });
        }
        let attempt = matching_attempt(incident, token)?;
        if attempt.scan_started || attempt.scan_revision.is_some() {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "start authoritative scan",
                phase: incident.phase,
            });
        }
        let incident = self.active_incident_mut()?;
        let Some(scan_count) = incident.scan_count.checked_next() else {
            incident.block(DependencyFailure::fatal_contract(
                RecoveryFailureCode::RecoveryScanCountExhausted,
                moment.observed_at,
            ));
            self.commit(moment.observed_at, next_revision);
            return Err(RecoveryTransitionError::RecoveryCountExhausted { field: "scan" });
        };
        incident.scan_count = scan_count;
        matching_attempt_mut(incident, token)?.scan_started = true;
        // Every observation already admitted before the lease starts is covered
        // by this scan. Later observations set the bit again and force a rescan.
        incident.rescan_required = false;
        incident.phase = RecoveryPhase::Scanning;
        self.commit(moment.observed_at, next_revision);
        Ok(())
    }

    pub(super) fn complete_authoritative_scan(
        &mut self,
        token: AttemptToken,
        scan_revision: RecoveryScanRevision,
        moment: RecoveryMoment,
        next_revision: RecoveryRevision,
    ) -> Result<(), RecoveryTransitionError> {
        let incident = self.active_incident()?;
        if incident.phase != RecoveryPhase::Scanning {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "complete authoritative scan",
                phase: incident.phase,
            });
        }
        let attempt = matching_attempt(incident, token)?;
        if !attempt.scan_started || attempt.scan_revision.is_some() {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "complete authoritative scan",
                phase: incident.phase,
            });
        }
        let incident = self.active_incident_mut()?;
        matching_attempt_mut(incident, token)?.scan_revision = Some(scan_revision);
        incident.phase = RecoveryPhase::AwaitingSeal;
        self.commit(moment.observed_at, next_revision);
        Ok(())
    }
}
