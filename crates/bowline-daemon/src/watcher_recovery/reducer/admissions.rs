//! How observations enter the recovery state machine.
//!
//! Three admissions, deliberately distinct. Ordinary activity was forwarded and
//! is durable in the ingress, so it advances only the activity frontier that
//! exact barriers fence on. A suppressed event was dropped by the overflow latch
//! and is genuinely unobserved. A loss is an admission that fidelity was lost at
//! the adapter, the lane, or the ingress. The last two advance the loss
//! watermark, which is what a close is answerable for.

use super::{DependencyFailure, RecoveryStateMachine};
use crate::watcher_recovery::{
    ActivityAdmission, ActivityWatermark, LossAdmission, LossWatermark, RecoveryCause,
    RecoveryFailureCode, RecoveryMoment, RecoveryTransitionError,
};

impl RecoveryStateMachine {
    pub(crate) fn observe_activity(
        &mut self,
        moment: RecoveryMoment,
    ) -> Result<ActivityAdmission, RecoveryTransitionError> {
        let Some(next_watermark) = self.activity_watermark.checked_next() else {
            let next_revision = self.next_revision_or_block(moment)?;
            self.activity_watermark = ActivityWatermark::terminal();
            self.terminal_block(
                RecoveryFailureCode::RecoveryActivityWatermarkExhausted,
                moment,
            );
            self.commit(moment.observed_at, next_revision);
            return Err(RecoveryTransitionError::ActivityWatermarkExhausted);
        };
        self.activity_watermark = next_watermark;
        let next_revision = self.next_revision_or_block(moment)?;
        let Some(incident) = self.incident.as_ref() else {
            self.commit(moment.observed_at, next_revision);
            return Ok(ActivityAdmission::Nominal);
        };
        let incident_id = incident.id;
        let blocked = incident.is_blocked();
        // Forwarded activity no longer requires the covering scan to be redone.
        // The event is durable in the ingress the moment it is accepted, and a
        // close attests only that lost fidelity was replaced -- it never claims
        // to have covered what was never lost. Fencing here meant a close could
        // not land while anyone kept writing.
        self.commit(moment.observed_at, next_revision);
        if blocked {
            Ok(ActivityAdmission::BlockedIncidentAdvanced { incident_id })
        } else {
            Ok(ActivityAdmission::ObservedDuringIncident { incident_id })
        }
    }

    /// Admit an event the overflow latch dropped.
    ///
    /// The write is genuinely unobserved, so this fences the close exactly as a
    /// loss does. It deliberately does not count a per-cause occurrence: cause
    /// counters ride `RecoveryCount`, and a sustained storm delivers enough
    /// suppressed events to exhaust it into a fatal block. The incident is
    /// already open and already knows its cause; what matters here is that the
    /// covering scan must be redone.
    pub(crate) fn observe_suppressed(
        &mut self,
        moment: RecoveryMoment,
    ) -> Result<ActivityAdmission, RecoveryTransitionError> {
        let Some(next_watermark) = self.activity_watermark.checked_next() else {
            let next_revision = self.next_revision_or_block(moment)?;
            self.activity_watermark = ActivityWatermark::terminal();
            self.terminal_block(
                RecoveryFailureCode::RecoveryActivityWatermarkExhausted,
                moment,
            );
            self.commit(moment.observed_at, next_revision);
            return Err(RecoveryTransitionError::ActivityWatermarkExhausted);
        };
        let Some(next_loss) = self.loss_watermark.checked_next() else {
            let next_revision = self.next_revision_or_block(moment)?;
            self.loss_watermark = LossWatermark::terminal();
            self.terminal_block(
                RecoveryFailureCode::RecoveryActivityWatermarkExhausted,
                moment,
            );
            self.commit(moment.observed_at, next_revision);
            return Err(RecoveryTransitionError::ActivityWatermarkExhausted);
        };
        self.activity_watermark = next_watermark;
        self.loss_watermark = next_loss;
        let next_revision = self.next_revision_or_block(moment)?;
        let Some(incident) = self.incident.as_ref() else {
            // A drop with no incident open cannot happen while the latch is the
            // only suppressor, but fail closed rather than lose the write.
            return self
                .open_incident(
                    RecoveryCause::NativeCallbackLaneSaturated,
                    moment,
                    next_revision,
                )
                .map(|admission| match admission {
                    LossAdmission::Opened { incident_id }
                    | LossAdmission::Coalesced { incident_id } => {
                        ActivityAdmission::CoverageInvalidated { incident_id }
                    }
                    LossAdmission::BlockedIncidentUpdated { incident_id } => {
                        ActivityAdmission::BlockedIncidentAdvanced { incident_id }
                    }
                });
        };
        let incident_id = incident.id;
        let blocked = incident.is_blocked();
        let incident = self
            .incident
            .as_mut()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        incident.rescan_required = true;
        self.commit(moment.observed_at, next_revision);
        if blocked {
            Ok(ActivityAdmission::BlockedIncidentAdvanced { incident_id })
        } else {
            Ok(ActivityAdmission::CoverageInvalidated { incident_id })
        }
    }

    pub(crate) fn observe_loss(
        &mut self,
        cause: RecoveryCause,
        moment: RecoveryMoment,
    ) -> Result<LossAdmission, RecoveryTransitionError> {
        let Some(next_watermark) = self.activity_watermark.checked_next() else {
            let next_revision = self.next_revision_or_block(moment)?;
            self.activity_watermark = ActivityWatermark::terminal();
            self.terminal_block(
                RecoveryFailureCode::RecoveryActivityWatermarkExhausted,
                moment,
            );
            self.commit(moment.observed_at, next_revision);
            return Err(RecoveryTransitionError::ActivityWatermarkExhausted);
        };
        let Some(next_loss) = self.loss_watermark.checked_next() else {
            let next_revision = self.next_revision_or_block(moment)?;
            self.loss_watermark = LossWatermark::terminal();
            self.terminal_block(
                RecoveryFailureCode::RecoveryActivityWatermarkExhausted,
                moment,
            );
            self.commit(moment.observed_at, next_revision);
            return Err(RecoveryTransitionError::ActivityWatermarkExhausted);
        };
        self.activity_watermark = next_watermark;
        self.loss_watermark = next_loss;
        let next_revision = self.next_revision_or_block(moment)?;
        if self.incident.is_none() {
            return self.open_incident(cause, moment, next_revision);
        }
        let incident = self
            .incident
            .as_mut()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        let incident_id = incident.id;
        let blocked = incident.is_blocked();
        if let Err(error) = incident.record_cause(cause) {
            incident.block(DependencyFailure::fatal_contract(
                RecoveryFailureCode::RecoveryCauseCountExhausted,
                moment.observed_at,
            ));
            self.commit(moment.observed_at, next_revision);
            return Err(error);
        }
        incident.rescan_required = true;
        self.commit(moment.observed_at, next_revision);
        if blocked {
            Ok(LossAdmission::BlockedIncidentUpdated { incident_id })
        } else {
            Ok(LossAdmission::Coalesced { incident_id })
        }
    }
}
