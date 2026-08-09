//! Live composition of native coverage, a paused local scan, and the recovery
//! state machine. Distributed upload, observation, and peer materialization are
//! deliberately outside this protocol and remain owned by the public exact
//! workspace barrier.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    AttemptToken, CloseDisposition, DependencyFailure, DependencyFailureClass, FailureDisposition,
    RecoveryFailureCode, RecoveryLifecycle, RecoveryMoment, RecoveryPhase, RecoveryScanRevision,
    RecoveryTransitionError, RecoveryWorkerOwnership, WatcherRecoveryCoordinator,
    WatcherRecoveryCoordinatorError,
};
use crate::manifest_driver::{
    CoverageScanError, CoverageScanFailure, EngineSnapshotHandle, WalkStartHook,
};
use crate::watcher_coverage::{
    CoverageCancellation, CoverageWait, WatcherCoverageAdapter, WatcherCoverageError,
};

const RECOVERY_ATTEMPT_BUDGET: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryWorkDisposition {
    Nominal,
    Closed,
    RetryRequired,
    RetryDeferred,
    Blocked,
    Cancelled,
}

/// Fenced I/O owner for one workspace recovery coordinator.
pub struct WatcherRecoveryWorker {
    coordinator: Arc<WatcherRecoveryCoordinator>,
    ownership: RecoveryWorkerOwnership,
}

impl WatcherRecoveryWorker {
    pub fn claim(
        coordinator: Arc<WatcherRecoveryCoordinator>,
        moment: RecoveryMoment,
    ) -> Result<Self, WatcherRecoveryCoordinatorError> {
        let ownership = coordinator.replace_worker(moment)?;
        Ok(Self {
            coordinator,
            ownership,
        })
    }

    pub fn recover_once(
        &self,
        coverage: &mut impl WatcherCoverageAdapter,
        engine: &EngineSnapshotHandle,
        // Shareable because the walk-start hook runs on the engine thread, which
        // a borrowed closure cannot cross. One source, so there is no second way
        // to ask what time it is.
        moment: &Arc<dyn Fn() -> RecoveryMoment + Send + Sync>,
        cancelled: &impl Fn() -> bool,
        coverage_cancellation: &CoverageCancellation,
        before_close: &mut impl FnMut() -> Result<(), WatcherRecoveryCoordinatorError>,
    ) -> Result<RecoveryWorkDisposition, WatcherRecoveryCoordinatorError> {
        if let Some(disposition) = self.prepare_recovery(moment)? {
            return Ok(disposition);
        }
        if cancelled() {
            return Ok(RecoveryWorkDisposition::Cancelled);
        }
        let token = self.coordinator.start_attempt(self.ownership, moment())?;
        let deadline = recovery_deadline()?;
        let wait = CoverageWait::new(deadline, coverage_cancellation.clone());
        let preparation = match coverage.begin_recovery(&wait) {
            Ok(preparation) => preparation,
            Err(error) => {
                return self.record_failure(token, coverage_failure(error, moment()), moment());
            }
        };
        // The fence starts where coverage starts. Recording it here, at scan
        // request time, put the engine's whole publication pass inside the
        // fenced window: a write dropped while blobs uploaded doomed an
        // attempt whose walk had not begun and would have seen that write on
        // disk. The hook below fires on the engine thread immediately before
        // the walk observes anything.
        let walk_start = self.walk_start_hook(token, moment);
        let scan_waiter = match engine.request_coverage_scan(walk_start) {
            Ok(waiter) => waiter,
            Err(CoverageScanError::Cancelled) => {
                return Ok(RecoveryWorkDisposition::Cancelled);
            }
            Err(error) => {
                return self.record_failure(token, scan_failure(error, moment()), moment());
            }
        };
        // The scan gets its own budget, not whatever `begin_recovery`'s
        // native round-trips left of the attempt's. Under an FSEvents storm
        // those seals are the slow part, so charging the wait their leftover
        // made it time out against a busy engine; the abandoned waiter then
        // poisoned the scan slot and every retry failed until the engine
        // reached a loop top. A longer wait can only delay a close, never
        // authorise a wrong one: closure is fenced independently by the
        // attempt's own scan revision, the boundary sealed after it, and the
        // watermark equality below.
        let scan_deadline = recovery_deadline()?;
        let remaining = scan_deadline.saturating_duration_since(Instant::now());
        let lease = match scan_waiter.wait(remaining, cancelled) {
            Ok(lease) => lease,
            Err(CoverageScanError::Cancelled) => {
                return Ok(RecoveryWorkDisposition::Cancelled);
            }
            Err(error) => {
                return self.record_failure(token, scan_failure(error, moment()), moment());
            }
        };
        let scan_revision =
            RecoveryScanRevision::new(lease.receipt().revision().get()).map_err(|_| {
                WatcherRecoveryCoordinatorError::Transition(
                    RecoveryTransitionError::ScanRevisionMismatch,
                )
            })?;
        self.coordinator
            .record_scan_completed(self.ownership, token, scan_revision, moment())?;
        let handoff = match coverage.seal_after_scan(preparation, &wait) {
            Ok(handoff) => handoff,
            Err(error) => {
                return self.record_failure(token, coverage_failure(error, moment()), moment());
            }
        };
        self.coordinator
            .record_native_boundary(self.ownership, token, handoff, moment())?;
        // The close fence. Everything the incident must account for has to be
        // admitted here: the authoritative scan and the post-scan native seal
        // already cover what came before, and activity admitted after this
        // point still invalidates the close below. A failure here must not
        // authorise closure, so it fails closed rather than being discarded.
        before_close()?;
        match self
            .coordinator
            .offer_close(self.ownership, token, scan_revision, moment())?
        {
            CloseDisposition::Closed(_) => {
                lease.release();
                Ok(RecoveryWorkDisposition::Closed)
            }
            CloseDisposition::RetryRequired { .. } => {
                // One attempt's scan, seal, and close offer remain atomic. The
                // caller gets a forwarding opportunity before it starts the
                // next attempt, so queued watcher detail cannot repeatedly
                // collapse inside the next close fence.
                lease.release();
                Ok(RecoveryWorkDisposition::RetryRequired)
            }
        }
    }

    /// Build the hook that records the scan as started, to run on the engine
    /// thread the instant before the walk observes the filesystem.
    ///
    /// A failure to record is not swallowed: `record_scan_completed` then fails
    /// `OutOfOrder`, the attempt is recorded as failed, and recovery retries.
    fn walk_start_hook(
        &self,
        token: AttemptToken,
        moment: &Arc<dyn Fn() -> RecoveryMoment + Send + Sync>,
    ) -> Option<WalkStartHook> {
        let coordinator = Arc::clone(&self.coordinator);
        let ownership = self.ownership;
        let moment = Arc::clone(moment);
        Some(WalkStartHook::new(move || {
            let _started = coordinator.record_scan_started(ownership, token, moment());
        }))
    }

    fn prepare_recovery(
        &self,
        moment: &Arc<dyn Fn() -> RecoveryMoment + Send + Sync>,
    ) -> Result<Option<RecoveryWorkDisposition>, WatcherRecoveryCoordinatorError> {
        let snapshot = self.coordinator.snapshot()?;
        match snapshot.lifecycle() {
            RecoveryLifecycle::Nominal => return Ok(Some(RecoveryWorkDisposition::Nominal)),
            RecoveryLifecycle::Blocked => return Ok(Some(RecoveryWorkDisposition::Blocked)),
            RecoveryLifecycle::Recovering => {}
        }
        if snapshot.phase() != Some(RecoveryPhase::BackingOff) {
            return Ok(None);
        }
        match self.coordinator.retry_if_due(self.ownership, moment()) {
            Ok(_) => Ok(None),
            Err(WatcherRecoveryCoordinatorError::Transition(
                RecoveryTransitionError::RetryNotDue,
            )) => Ok(Some(RecoveryWorkDisposition::RetryDeferred)),
            Err(error) => Err(error),
        }
    }

    pub fn worker_exited(
        &self,
        moment: RecoveryMoment,
    ) -> Result<super::IncidentId, WatcherRecoveryCoordinatorError> {
        self.coordinator.worker_exited(self.ownership, moment)
    }

    pub fn restore_matching_authority(
        &self,
        expected_failure: &DependencyFailure,
        moment: RecoveryMoment,
    ) -> Result<super::AuthorityRestoration, WatcherRecoveryCoordinatorError> {
        self.coordinator
            .restore_matching_authority(self.ownership, expected_failure, moment)
    }

    fn record_failure(
        &self,
        token: super::AttemptToken,
        failure: DependencyFailure,
        moment: RecoveryMoment,
    ) -> Result<RecoveryWorkDisposition, WatcherRecoveryCoordinatorError> {
        match self
            .coordinator
            .record_dependency_failure(self.ownership, token, failure, moment)?
        {
            FailureDisposition::RetryScheduled { .. } => Ok(RecoveryWorkDisposition::RetryDeferred),
            FailureDisposition::Blocked { .. } => Ok(RecoveryWorkDisposition::Blocked),
        }
    }
}

fn recovery_deadline() -> Result<Instant, WatcherRecoveryCoordinatorError> {
    Instant::now().checked_add(RECOVERY_ATTEMPT_BUDGET).ok_or(
        WatcherRecoveryCoordinatorError::Transition(RecoveryTransitionError::RetryDeadlineOverflow),
    )
}

fn coverage_failure(error: WatcherCoverageError, moment: RecoveryMoment) -> DependencyFailure {
    let class = match error {
        WatcherCoverageError::IdentifierExhausted => DependencyFailureClass::FatalContract,
        WatcherCoverageError::Cancelled
        | WatcherCoverageError::CoverageUnavailable
        | WatcherCoverageError::TimedOut
        | WatcherCoverageError::Loss(_)
        | WatcherCoverageError::StaleBoundary
        | WatcherCoverageError::Shutdown => DependencyFailureClass::Retryable,
    };
    DependencyFailure::new(
        class,
        RecoveryFailureCode::DependencyUnavailable,
        moment.observed_at,
    )
}

fn scan_failure(error: CoverageScanError, moment: RecoveryMoment) -> DependencyFailure {
    let (class, code) = match error {
        CoverageScanError::ResourceExhausted
        | CoverageScanError::Scan(CoverageScanFailure::CycleActive) => (
            DependencyFailureClass::Retryable,
            RecoveryFailureCode::DependencyBusy,
        ),
        CoverageScanError::EngineStopped
        | CoverageScanError::TimedOut
        | CoverageScanError::Cancelled
        | CoverageScanError::Scan(CoverageScanFailure::RootUnavailable) => (
            DependencyFailureClass::Retryable,
            RecoveryFailureCode::DependencyUnavailable,
        ),
        CoverageScanError::Scan(CoverageScanFailure::Fatal) => (
            DependencyFailureClass::FatalContract,
            RecoveryFailureCode::DependencyUnavailable,
        ),
    };
    DependencyFailure::new(class, code, moment.observed_at)
}
