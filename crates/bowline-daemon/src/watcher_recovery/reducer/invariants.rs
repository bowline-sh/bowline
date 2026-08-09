use super::{IncidentState, RecoveryStateMachine};
use crate::watcher_recovery::types::{
    ActivityWatermark, RecoveryLifecycle, RecoveryPhase, RecoveryRevision,
};

impl RecoveryStateMachine {
    pub(crate) fn invariant_holds(&self) -> bool {
        match &self.incident {
            None => self.lifecycle() == RecoveryLifecycle::Nominal,
            Some(incident) => {
                let expected_lifecycle = if incident.is_blocked() {
                    RecoveryLifecycle::Blocked
                } else {
                    RecoveryLifecycle::Recovering
                };
                self.lifecycle() == expected_lifecycle
                    && incident.cause_counts.get(incident.primary_cause).get() > 0
                    && attempt_identity_holds(incident, self)
                    && phase_invariant(incident)
            }
        }
    }

    pub(crate) fn set_next_incident_id_for_test(&mut self, next: u64) {
        self.next_incident_id.next = next;
    }

    pub(crate) fn set_next_attempt_id_for_test(&mut self, next: u64) {
        self.next_attempt_id.next = next;
    }

    pub(crate) fn set_next_worker_id_for_test(&mut self, next: u64) {
        self.next_worker_id.next = next;
    }

    pub(crate) fn set_activity_watermark_for_test(&mut self, value: u64) {
        if let Ok(watermark) = ActivityWatermark::new(value) {
            self.activity_watermark = watermark;
        }
    }

    pub(crate) fn set_recovery_revision_for_test(&mut self, value: u64) {
        if let Ok(revision) = RecoveryRevision::new(value) {
            self.revision = revision;
        }
    }
}

fn attempt_identity_holds(incident: &IncidentState, model: &RecoveryStateMachine) -> bool {
    incident.current_attempt.as_ref().is_none_or(|attempt| {
        attempt.token.incident_id() == incident.id
            && Some(attempt.token.worker_id()) == model.active_worker
            && attempt.native_boundary.as_ref() == incident.native_coverage.as_ref()
            && attempt
                .native_boundary
                .as_ref()
                .is_none_or(|boundary| boundary.activity_watermark() <= model.activity_watermark)
            && (attempt.scan_revision.is_none() || attempt.scan_started)
    })
}

fn phase_invariant(incident: &IncidentState) -> bool {
    match incident.phase {
        RecoveryPhase::Rearming => no_attempt_evidence(incident) && no_failure_wait(incident),
        RecoveryPhase::AwaitingCoverage => {
            no_failure_wait(incident)
                && incident.native_coverage.is_none()
                && incident.current_attempt.as_ref().is_some_and(|attempt| {
                    attempt.native_boundary.is_none()
                        && !attempt.scan_started
                        && attempt.scan_revision.is_none()
                })
        }
        RecoveryPhase::Scanning => {
            no_failure_wait(incident)
                && incident.current_attempt.as_ref().is_some_and(|attempt| {
                    attempt.native_boundary.is_none()
                        && attempt.scan_started
                        && attempt.scan_revision.is_none()
                })
        }
        RecoveryPhase::AwaitingSeal => {
            no_failure_wait(incident)
                && incident.current_attempt.as_ref().is_some_and(|attempt| {
                    attempt.native_boundary.is_none()
                        && attempt.scan_started
                        && attempt.scan_revision.is_some()
                })
        }
        RecoveryPhase::Closing => {
            no_failure_wait(incident)
                && incident.current_attempt.as_ref().is_some_and(|attempt| {
                    attempt.native_boundary.is_some()
                        && attempt.scan_started
                        && attempt.scan_revision.is_some()
                })
        }
        RecoveryPhase::BackingOff => {
            no_attempt_evidence(incident)
                && incident.retry_at.is_some()
                && incident
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.class().is_retryable())
                && incident.rescan_required
        }
        RecoveryPhase::Blocked => {
            no_attempt_evidence(incident)
                && incident.retry_at.is_none()
                && incident
                    .failure
                    .as_ref()
                    .is_some_and(|failure| !failure.class().is_retryable())
                && incident.rescan_required
        }
    }
}

fn no_attempt_evidence(incident: &IncidentState) -> bool {
    incident.current_attempt.is_none() && incident.native_coverage.is_none()
}

fn no_failure_wait(incident: &IncidentState) -> bool {
    incident.failure.is_none() && incident.retry_at.is_none()
}
