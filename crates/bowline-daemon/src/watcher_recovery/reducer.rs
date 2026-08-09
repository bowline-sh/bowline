mod admissions;

mod attempts;
mod ids;
#[cfg(test)]
mod invariants;
mod projection;

use ids::{IdAllocation, IdSequence};

use super::{
    LossWatermark,
    snapshot::CurrentAttemptSnapshot,
    types::{
        ActivityWatermark, AttemptCoverageBoundary, AttemptId, AttemptToken, AuthorityRestoration,
        BackoffPolicy, CloseDisposition, CloseOffer, DependencyFailure, FailureDisposition,
        IncidentId, LossAdmission, RecoveryCause, RecoveryClosureReceipt,
        RecoveryClosureReceiptInput, RecoveryCount, RecoveryFailureCode, RecoveryIdentifierKind,
        RecoveryInstant, RecoveryMoment, RecoveryPhase, RecoveryRevision, RecoveryScanRevision,
        RecoverySourceIdentity, RecoveryTimestamp, RecoveryTransitionError, RecoveryWorkerId,
        RecoveryWorkerOwnership,
    },
};
use crate::watcher_coverage::{WatcherBoundaryId, WatcherCoverageHandoff, WatcherStreamEpoch};

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptState {
    token: AttemptToken,
    started_at: RecoveryTimestamp,
    native_boundary: Option<AttemptCoverageBoundary>,
    scan_started: bool,
    scan_revision: Option<RecoveryScanRevision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CauseCounters([RecoveryCount; 8]);

impl CauseCounters {
    fn with_primary(cause: RecoveryCause) -> Self {
        let mut counts = [RecoveryCount::ZERO; 8];
        counts[cause_index(cause)] = RecoveryCount::ONE;
        Self(counts)
    }

    #[cfg(test)]
    fn get(self, cause: RecoveryCause) -> RecoveryCount {
        self.0[cause_index(cause)]
    }

    fn increment(&mut self, cause: RecoveryCause) -> Option<()> {
        let count = &mut self.0[cause_index(cause)];
        *count = count.checked_next()?;
        Some(())
    }
}

const fn cause_index(cause: RecoveryCause) -> usize {
    match cause {
        RecoveryCause::StartupReconciliation => 0,
        RecoveryCause::NativeCallbackLaneSaturated => 1,
        RecoveryCause::NativeEventBatchSaturated => 2,
        RecoveryCause::IngressDetailCollapsed => 3,
        RecoveryCause::NativeRescanRequired => 4,
        RecoveryCause::RecoverableAdapterLoss => 5,
        RecoveryCause::WatcherDisconnected => 6,
        RecoveryCause::RootReplaced => 7,
    }
}

impl AttemptState {
    fn new(token: AttemptToken, started_at: RecoveryTimestamp) -> Self {
        Self {
            token,
            started_at,
            native_boundary: None,
            scan_started: false,
            scan_revision: None,
        }
    }

    fn snapshot(&self) -> CurrentAttemptSnapshot {
        CurrentAttemptSnapshot::new(
            self.token.attempt_id(),
            self.native_boundary
                .as_ref()
                .map(AttemptCoverageBoundary::activity_watermark),
            self.started_at,
            self.scan_revision,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncidentState {
    id: IncidentId,
    primary_cause: RecoveryCause,
    cause_counts: CauseCounters,
    phase: RecoveryPhase,
    started_at: RecoveryTimestamp,
    started_tick: RecoveryInstant,
    attempt_count: RecoveryCount,
    scan_count: RecoveryCount,
    rescan_required: bool,
    current_attempt: Option<AttemptState>,
    native_coverage: Option<AttemptCoverageBoundary>,
    retry_failures: u32,
    retry_at: Option<RecoveryInstant>,
    failure: Option<DependencyFailure>,
}

impl IncidentState {
    fn new(id: IncidentId, cause: RecoveryCause, moment: RecoveryMoment) -> Self {
        Self {
            id,
            primary_cause: cause,
            cause_counts: CauseCounters::with_primary(cause),
            phase: RecoveryPhase::Rearming,
            started_at: moment.observed_at,
            started_tick: moment.monotonic,
            attempt_count: RecoveryCount::ZERO,
            scan_count: RecoveryCount::ZERO,
            rescan_required: true,
            current_attempt: None,
            native_coverage: None,
            retry_failures: 0,
            retry_at: None,
            failure: None,
        }
    }

    fn is_blocked(&self) -> bool {
        self.phase == RecoveryPhase::Blocked
    }

    fn record_cause(&mut self, cause: RecoveryCause) -> Result<(), RecoveryTransitionError> {
        self.cause_counts
            .increment(cause)
            .ok_or(RecoveryTransitionError::RecoveryCountExhausted { field: "cause" })
    }

    fn block(&mut self, failure: DependencyFailure) {
        self.phase = RecoveryPhase::Blocked;
        self.clear_attempt_evidence();
        self.retry_at = None;
        self.rescan_required = true;
        self.failure = Some(failure);
    }

    fn clear_attempt_evidence(&mut self) {
        self.current_attempt = None;
        self.native_coverage = None;
    }

    fn rearm_after_worker_change(&mut self) {
        if self.is_blocked() {
            return;
        }
        self.clear_attempt_evidence();
        self.phase = RecoveryPhase::Rearming;
        self.rescan_required = true;
        self.retry_at = None;
        self.failure = None;
    }

    fn retry_attempt(&mut self) {
        self.clear_attempt_evidence();
        self.phase = RecoveryPhase::Rearming;
        self.rescan_required = true;
    }
}

/// The deterministic watcher-recovery reducer. It owns no threads, clocks, I/O,
/// queues, engine work, or metrics; callers inject every timestamp and serialize
/// access at the coordinator boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryStateMachine {
    source_identity: RecoverySourceIdentity,
    incident: Option<IncidentState>,
    activity_watermark: ActivityWatermark,
    loss_watermark: LossWatermark,
    next_incident_id: IdSequence,
    next_attempt_id: IdSequence,
    next_worker_id: IdSequence,
    active_worker: Option<RecoveryWorkerId>,
    revision: RecoveryRevision,
    last_transition_at: RecoveryTimestamp,
    last_completed: Option<RecoveryClosureReceipt>,
    backoff_policy: BackoffPolicy,
    last_native_boundary_id: Option<WatcherBoundaryId>,
    last_native_epoch: Option<WatcherStreamEpoch>,
}

impl RecoveryStateMachine {
    #[cfg(test)]
    pub(crate) fn nominal(
        source_identity: RecoverySourceIdentity,
        initial_at: RecoveryTimestamp,
        backoff_policy: BackoffPolicy,
    ) -> Self {
        Self {
            source_identity,
            incident: None,
            activity_watermark: ActivityWatermark::INITIAL,
            loss_watermark: LossWatermark::INITIAL,
            next_incident_id: IdSequence::initial(),
            next_attempt_id: IdSequence::initial(),
            next_worker_id: IdSequence::initial(),
            active_worker: None,
            revision: RecoveryRevision::INITIAL,
            last_transition_at: initial_at,
            last_completed: None,
            backoff_policy,
            last_native_boundary_id: None,
            last_native_epoch: None,
        }
    }

    pub(crate) fn startup_reconciliation(
        source_identity: RecoverySourceIdentity,
        moment: RecoveryMoment,
        backoff_policy: BackoffPolicy,
    ) -> Self {
        let incident_id = IncidentId::from_valid(1);
        let watermark = ActivityWatermark::from_valid(1);
        let revision = RecoveryRevision::from_valid(1);
        Self {
            source_identity,
            incident: Some(IncidentState::new(
                incident_id,
                RecoveryCause::StartupReconciliation,
                moment,
            )),
            activity_watermark: watermark,
            // Startup reconciliation is itself an admission of lost fidelity:
            // nothing observed the workspace while the daemon was down.
            loss_watermark: LossWatermark::from_valid(1),
            next_incident_id: IdSequence { next: 2 },
            next_attempt_id: IdSequence::initial(),
            next_worker_id: IdSequence::initial(),
            active_worker: None,
            revision,
            last_transition_at: moment.observed_at,
            last_completed: None,
            backoff_policy,
            last_native_boundary_id: None,
            last_native_epoch: None,
        }
    }

    pub(crate) fn replace_worker(
        &mut self,
        moment: RecoveryMoment,
    ) -> Result<RecoveryWorkerOwnership, RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let worker_id = match self.next_worker_id.allocate() {
            IdAllocation::Available(value) => RecoveryWorkerId::new(value).map_err(|_| {
                RecoveryTransitionError::IdentifierExhausted {
                    kind: RecoveryIdentifierKind::Worker,
                }
            })?,
            IdAllocation::Terminal(_) => {
                self.active_worker = None;
                self.terminal_block(RecoveryFailureCode::RecoveryWorkerIdExhausted, moment);
                self.commit(moment.observed_at, next_revision);
                return Err(RecoveryTransitionError::IdentifierExhausted {
                    kind: RecoveryIdentifierKind::Worker,
                });
            }
        };
        self.active_worker = Some(worker_id);
        if let Some(incident) = &mut self.incident {
            incident.rearm_after_worker_change();
        }
        self.commit(moment.observed_at, next_revision);
        Ok(RecoveryWorkerOwnership::new(worker_id))
    }

    pub(crate) fn worker_exited(
        &mut self,
        ownership: RecoveryWorkerOwnership,
        moment: RecoveryMoment,
    ) -> Result<IncidentId, RecoveryTransitionError> {
        self.require_worker(ownership)?;
        let next_revision = self.next_revision_or_block(moment)?;
        self.active_worker = None;
        if self.incident.is_none() {
            return match self.open_incident(
                RecoveryCause::WatcherDisconnected,
                moment,
                next_revision,
            )? {
                LossAdmission::Opened { incident_id } => Ok(incident_id),
                LossAdmission::Coalesced { incident_id }
                | LossAdmission::BlockedIncidentUpdated { incident_id } => Ok(incident_id),
            };
        }
        let incident = self
            .incident
            .as_mut()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        let incident_id = incident.id;
        incident.rearm_after_worker_change();
        self.commit(moment.observed_at, next_revision);
        Ok(incident_id)
    }

    pub(crate) fn require_worker(
        &self,
        ownership: RecoveryWorkerOwnership,
    ) -> Result<(), RecoveryTransitionError> {
        match self.active_worker {
            Some(worker_id) if worker_id == ownership.worker_id() => Ok(()),
            Some(_) => Err(RecoveryTransitionError::WorkerMismatch),
            None => Err(RecoveryTransitionError::WorkerUnavailable),
        }
    }

    pub(crate) fn start_attempt(
        &mut self,
        moment: RecoveryMoment,
    ) -> Result<AttemptToken, RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let worker_id = self
            .active_worker
            .ok_or(RecoveryTransitionError::WorkerUnavailable)?;
        let incident = self
            .incident
            .as_ref()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        if incident.is_blocked() {
            return Err(RecoveryTransitionError::LifecycleBlocked);
        }
        if incident.current_attempt.is_some() {
            return Err(RecoveryTransitionError::AttemptAlreadyInFlight);
        }
        if incident.phase != RecoveryPhase::Rearming {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "start attempt",
                phase: incident.phase,
            });
        }
        let attempt_id = match self.next_attempt_id.allocate() {
            IdAllocation::Available(value) => {
                AttemptId::new(value).map_err(|_| RecoveryTransitionError::IdentifierExhausted {
                    kind: RecoveryIdentifierKind::Attempt,
                })?
            }
            IdAllocation::Terminal(_) => {
                self.block_for_exhaustion(
                    RecoveryFailureCode::RecoveryAttemptIdExhausted,
                    moment.observed_at,
                    next_revision,
                );
                return Err(RecoveryTransitionError::IdentifierExhausted {
                    kind: RecoveryIdentifierKind::Attempt,
                });
            }
        };
        let incident = self
            .incident
            .as_mut()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        let Some(attempt_count) = incident.attempt_count.checked_next() else {
            incident.block(DependencyFailure::fatal_contract(
                RecoveryFailureCode::RecoveryAttemptCountExhausted,
                moment.observed_at,
            ));
            self.commit(moment.observed_at, next_revision);
            return Err(RecoveryTransitionError::RecoveryCountExhausted { field: "attempt" });
        };
        let token = AttemptToken::new(incident.id, attempt_id, worker_id);
        incident.attempt_count = attempt_count;
        incident.phase = RecoveryPhase::AwaitingCoverage;
        incident.rescan_required = false;
        incident.current_attempt = Some(AttemptState::new(token, moment.observed_at));
        incident.native_coverage = None;
        incident.retry_at = None;
        incident.failure = None;
        self.commit(moment.observed_at, next_revision);
        Ok(token)
    }

    pub(crate) fn record_native_boundary(
        &mut self,
        token: AttemptToken,
        handoff: WatcherCoverageHandoff,
        moment: RecoveryMoment,
    ) -> Result<(), RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let incident = self.active_incident()?;
        if incident.phase != RecoveryPhase::AwaitingSeal {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "record native boundary",
                phase: incident.phase,
            });
        }
        matching_attempt(incident, token)?;
        if self
            .last_native_boundary_id
            .is_some_and(|last| handoff.boundary_id() <= last)
        {
            return Err(RecoveryTransitionError::NativeBoundaryNotMonotonic);
        }
        if self
            .last_native_epoch
            .is_some_and(|last| handoff.live_stream_epoch() < last)
        {
            return Err(RecoveryTransitionError::NativeStreamEpochRegressed);
        }
        self.last_native_boundary_id = Some(handoff.boundary_id());
        self.last_native_epoch = Some(handoff.live_stream_epoch());
        let admitted_boundary = AttemptCoverageBoundary::admitted(
            self.activity_watermark,
            self.loss_watermark,
            handoff,
        );
        let incident = self.active_incident_mut()?;
        let attempt = matching_attempt_mut(incident, token)?;
        attempt.native_boundary = Some(admitted_boundary.clone());
        incident.native_coverage = Some(admitted_boundary);
        incident.phase = RecoveryPhase::Closing;
        self.commit(moment.observed_at, next_revision);
        Ok(())
    }

    pub(crate) fn record_scan_started(
        &mut self,
        token: AttemptToken,
        moment: RecoveryMoment,
    ) -> Result<(), RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        self.begin_authoritative_scan(token, moment, next_revision)
    }

    pub(crate) fn record_scan_completed(
        &mut self,
        token: AttemptToken,
        scan_revision: RecoveryScanRevision,
        moment: RecoveryMoment,
    ) -> Result<(), RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        self.complete_authoritative_scan(token, scan_revision, moment, next_revision)
    }

    pub(crate) fn offer_close(
        &mut self,
        offer: &CloseOffer,
        moment: RecoveryMoment,
    ) -> Result<CloseDisposition, RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let incident = self.active_incident()?;
        let attempt = matching_attempt(incident, offer.attempt())?;
        if incident.phase != RecoveryPhase::Closing {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "offer close",
                phase: incident.phase,
            });
        }
        let boundary = attempt
            .native_boundary
            .as_ref()
            .ok_or(RecoveryTransitionError::NativeBoundaryMismatch)?;
        if boundary != offer.native_boundary() {
            return Err(RecoveryTransitionError::NativeBoundaryMismatch);
        }
        let Some(scan_revision) = attempt.scan_revision else {
            return Err(RecoveryTransitionError::ScanRevisionMismatch);
        };
        if scan_revision != offer.scan_revision() {
            return Err(RecoveryTransitionError::ScanRevisionMismatch);
        }
        // Only lost fidelity gates the close. Activity that forwarded normally is
        // durable in the ingress and is not this scan's responsibility, so a
        // steady stream of ordinary writes no longer keeps the incident open.
        let close_lost =
            incident.rescan_required || self.loss_watermark != boundary.loss_watermark();
        if close_lost {
            let incident_id = incident.id;
            let incident = self.active_incident_mut()?;
            incident.retry_attempt();
            self.commit(moment.observed_at, next_revision);
            return Ok(CloseDisposition::RetryRequired { incident_id });
        }
        let duration = moment
            .monotonic
            .elapsed_since(incident.started_tick)
            .ok_or(RecoveryTransitionError::MonotonicTimeReversed)?;
        let receipt_input = RecoveryClosureReceiptInput {
            incident_id: incident.id,
            attempt_id: offer.attempt().attempt_id(),
            attempt_count: incident.attempt_count,
            scan_count: incident.scan_count,
            activity_watermark: boundary.activity_watermark(),
            authoritative_scan_revision: scan_revision,
            native_boundary: boundary.clone(),
            started_at: incident.started_at,
            completed_at: moment.observed_at,
            duration,
            closing_recovery_revision: next_revision,
        };
        let receipt = match RecoveryClosureReceipt::try_from_input(receipt_input) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.active_incident_mut()?
                    .block(DependencyFailure::fatal_contract(
                        RecoveryFailureCode::RecoveryClosureDurationOutOfRange,
                        moment.observed_at,
                    ));
                self.commit(moment.observed_at, next_revision);
                return Err(error);
            }
        };
        self.incident = None;
        self.last_completed = Some(receipt.clone());
        self.commit(moment.observed_at, next_revision);
        Ok(CloseDisposition::Closed(Box::new(receipt)))
    }

    pub(crate) fn reject_native_close(
        &mut self,
        token: AttemptToken,
        moment: RecoveryMoment,
    ) -> Result<CloseDisposition, RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let incident = self.active_incident()?;
        if incident.phase != RecoveryPhase::Closing {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "reject stale native boundary",
                phase: incident.phase,
            });
        }
        matching_attempt(incident, token)?;
        let incident_id = incident.id;
        self.active_incident_mut()?.retry_attempt();
        self.commit(moment.observed_at, next_revision);
        Ok(CloseDisposition::RetryRequired { incident_id })
    }

    pub(crate) fn record_dependency_failure(
        &mut self,
        token: AttemptToken,
        failure: DependencyFailure,
        moment: RecoveryMoment,
    ) -> Result<FailureDisposition, RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let incident = self.active_incident()?;
        matching_attempt(incident, token)?;
        let incident_id = incident.id;
        if failure.class().is_retryable() {
            let Some(consecutive) = incident.retry_failures.checked_add(1) else {
                self.active_incident_mut()?
                    .block(DependencyFailure::fatal_contract(
                        RecoveryFailureCode::RecoveryRetryCountExhausted,
                        moment.observed_at,
                    ));
                self.commit(moment.observed_at, next_revision);
                return Err(RecoveryTransitionError::RecoveryCountExhausted { field: "retry" });
            };
            let delay = self.backoff_policy.delay_for(consecutive);
            let Some(retry_at) = moment.monotonic.checked_add(delay) else {
                self.active_incident_mut()?
                    .block(DependencyFailure::fatal_contract(
                        RecoveryFailureCode::RecoveryRetryDeadlineOverflow,
                        moment.observed_at,
                    ));
                self.commit(moment.observed_at, next_revision);
                return Err(RecoveryTransitionError::RetryDeadlineOverflow);
            };
            let incident = self.active_incident_mut()?;
            incident.retry_failures = consecutive;
            incident.retry_at = Some(retry_at);
            incident.clear_attempt_evidence();
            incident.phase = RecoveryPhase::BackingOff;
            incident.rescan_required = true;
            incident.failure = Some(failure);
            self.commit(moment.observed_at, next_revision);
            return Ok(FailureDisposition::RetryScheduled {
                incident_id,
                retry_at,
                delay,
            });
        }
        let class = failure.class();
        self.active_incident_mut()?.block(failure);
        self.commit(moment.observed_at, next_revision);
        Ok(FailureDisposition::Blocked { incident_id, class })
    }

    pub(crate) fn retry_if_due(
        &mut self,
        moment: RecoveryMoment,
    ) -> Result<IncidentId, RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let incident = self.active_incident()?;
        if incident.phase != RecoveryPhase::BackingOff {
            return Err(RecoveryTransitionError::OutOfOrder {
                operation: "retry recovery",
                phase: incident.phase,
            });
        }
        let retry_at = incident
            .retry_at
            .ok_or(RecoveryTransitionError::RetryNotDue)?;
        if moment.monotonic < retry_at {
            return Err(RecoveryTransitionError::RetryNotDue);
        }
        let incident_id = incident.id;
        let incident = self.active_incident_mut()?;
        incident.retry_at = None;
        incident.phase = RecoveryPhase::Rearming;
        incident.failure = None;
        incident.clear_attempt_evidence();
        self.commit(moment.observed_at, next_revision);
        Ok(incident_id)
    }

    pub(crate) fn restore_authority(
        &mut self,
        moment: RecoveryMoment,
    ) -> Result<AuthorityRestoration, RecoveryTransitionError> {
        let next_revision = self.next_revision_or_block(moment)?;
        let incident = self
            .incident
            .as_ref()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        let failure = incident
            .failure
            .as_ref()
            .ok_or(RecoveryTransitionError::AuthorityNotRestorable)?;
        if !failure.class().is_authority_restorable() {
            return Err(RecoveryTransitionError::AuthorityNotRestorable);
        }
        let incident_id = incident.id;
        let incident = self
            .incident
            .as_mut()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        incident.failure = None;
        incident.phase = RecoveryPhase::Rearming;
        incident.rescan_required = true;
        incident.retry_failures = 0;
        incident.retry_at = None;
        incident.clear_attempt_evidence();
        self.commit(moment.observed_at, next_revision);
        Ok(AuthorityRestoration::Restored { incident_id })
    }

    pub(crate) fn restore_matching_authority(
        &mut self,
        expected_failure: &DependencyFailure,
        moment: RecoveryMoment,
    ) -> Result<AuthorityRestoration, RecoveryTransitionError> {
        let retained_failure = self
            .incident
            .as_ref()
            .and_then(|incident| incident.failure.as_ref())
            .ok_or(RecoveryTransitionError::AuthorityNotRestorable)?;
        if retained_failure != expected_failure {
            return Err(RecoveryTransitionError::RetainedFailureMismatch);
        }
        self.restore_authority(moment)
    }

    fn active_incident(&self) -> Result<&IncidentState, RecoveryTransitionError> {
        let incident = self
            .incident
            .as_ref()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        if incident.is_blocked() {
            return Err(RecoveryTransitionError::LifecycleBlocked);
        }
        Ok(incident)
    }

    fn active_incident_mut(&mut self) -> Result<&mut IncidentState, RecoveryTransitionError> {
        let incident = self
            .incident
            .as_mut()
            .ok_or(RecoveryTransitionError::NoOpenIncident)?;
        if incident.is_blocked() {
            return Err(RecoveryTransitionError::LifecycleBlocked);
        }
        Ok(incident)
    }

    fn next_revision_or_block(
        &mut self,
        moment: RecoveryMoment,
    ) -> Result<RecoveryRevision, RecoveryTransitionError> {
        let Some(revision) = self.revision.checked_next() else {
            self.terminal_block(RecoveryFailureCode::RecoveryRevisionExhausted, moment);
            self.commit(moment.observed_at, RecoveryRevision::terminal());
            return Err(RecoveryTransitionError::RecoveryRevisionExhausted);
        };
        Ok(revision)
    }

    fn commit(&mut self, observed_at: RecoveryTimestamp, revision: RecoveryRevision) {
        self.last_transition_at = observed_at;
        self.revision = revision;
    }

    fn block_for_exhaustion(
        &mut self,
        code: RecoveryFailureCode,
        observed_at: RecoveryTimestamp,
        revision: RecoveryRevision,
    ) {
        if let Some(incident) = &mut self.incident {
            incident.block(DependencyFailure::fatal_contract(code, observed_at));
        }
        self.commit(observed_at, revision);
    }

    fn terminal_block(&mut self, code: RecoveryFailureCode, moment: RecoveryMoment) {
        let failure = DependencyFailure::fatal_contract(code, moment.observed_at);
        if let Some(incident) = &mut self.incident {
            incident.block(failure);
            return;
        }
        let mut incident = IncidentState::new(
            IncidentId::from_valid(u64::MAX),
            RecoveryCause::RecoverableAdapterLoss,
            moment,
        );
        incident.block(failure);
        self.next_incident_id.next = u64::MAX;
        self.incident = Some(incident);
    }
}

fn matching_attempt(
    incident: &IncidentState,
    token: AttemptToken,
) -> Result<&AttemptState, RecoveryTransitionError> {
    let attempt = incident
        .current_attempt
        .as_ref()
        .ok_or(RecoveryTransitionError::NoAttemptInFlight)?;
    if attempt.token != token {
        return Err(RecoveryTransitionError::AttemptMismatch);
    }
    Ok(attempt)
}

fn matching_attempt_mut(
    incident: &mut IncidentState,
    token: AttemptToken,
) -> Result<&mut AttemptState, RecoveryTransitionError> {
    let attempt = incident
        .current_attempt
        .as_mut()
        .ok_or(RecoveryTransitionError::NoAttemptInFlight)?;
    if attempt.token != token {
        return Err(RecoveryTransitionError::AttemptMismatch);
    }
    Ok(attempt)
}
