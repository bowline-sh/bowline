mod collector;
mod delivery;
mod input;
mod reducer;
mod retry;
mod service;
mod subscriptions;
mod types;

pub use collector::{
    DeviceTrustStatusFacts, LocalStatusProjectionCollector, SharedStatusSourceCollector,
    SharedStatusSourceHandle, StatusCollectorFailure, StatusCollectorFailureCode,
    StatusSourceCollection, StatusSourceCollector, StatusSourceFacts, StatusSourceFailurePolicy,
    StatusSourceState, StatusSourceStateFacts,
};
pub use delivery::LatestProjectionReceiver;
pub use input::StatusProjectionInput;
pub use service::{
    ProjectionHeartbeatSubscription, ProjectionSubscription, StatusProjectionService,
};
pub use types::{
    DaemonInstanceId, DaemonStatusProjection, ProjectionBuildReason, ProjectionServiceConfig,
    SafetyRefreshInterval, SourceFreshness, SourceRevision, StatusFingerprint, StatusInputEvent,
    StatusProjectionError, StatusProjectionMetrics, StatusRetryPolicy, StatusSequence,
    StatusSource, StatusSourceRevision, StatusTimestamp,
};

#[cfg(test)]
mod tests;
