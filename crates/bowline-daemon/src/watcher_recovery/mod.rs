mod coordinator;
mod protocol;
mod reducer;
mod runtime;
mod snapshot;
mod types;

pub use coordinator::{
    RecoverySubscription, RecoverySubscriptionError, RecoverySubscriptionRole,
    WatcherRecoveryCoordinator, WatcherRecoveryCoordinatorError,
};
pub use runtime::{RecoveryWorkDisposition, WatcherRecoveryWorker};
pub use snapshot::{
    CurrentAttemptSnapshot, DarwinCoverageStartSnapshot, NativeCoverageLossSnapshot,
    NativeCoverageSnapshot, RecoveryFailureSnapshot, WatcherRecoverySnapshot,
};
pub use types::{
    ActivityAdmission, ActivityWatermark, AttemptCoverageBoundary, AttemptId, AttemptToken,
    AuthoritativeRefIdentity, AuthorityRestoration, BackoffPolicy, CloseDisposition,
    DependencyFailure, DependencyFailureClass, FailureDisposition, IncidentId, LossAdmission,
    LossWatermark, NativeAdapter, RecoveryAttestation, RecoveryCause, RecoveryClosureIdentity,
    RecoveryClosureReceipt, RecoveryCount, RecoveryFailureCode, RecoveryFrontier,
    RecoveryIdentifierKind, RecoveryInstant, RecoveryLifecycle, RecoveryMoment, RecoveryPhase,
    RecoveryProcessBootId, RecoveryProcessIdentity, RecoveryProcessSessionId, RecoveryProjectorId,
    RecoveryProjectorOwnership, RecoveryRevision, RecoveryScanRevision, RecoverySourceIdentity,
    RecoveryTimestamp, RecoveryTransitionError, RecoveryTypeError, RecoveryWorkerId,
    RecoveryWorkerOwnership, RefObservation,
};

#[cfg(test)]
mod loom_tests;
#[cfg(test)]
mod property_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
