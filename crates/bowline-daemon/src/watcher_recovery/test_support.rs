use super::reducer::RecoveryStateMachine;
use super::types::CloseOffer;
use super::{
    AttemptCoverageBoundary, AttemptToken, BackoffPolicy, RecoveryInstant, RecoveryMoment,
    RecoveryProcessBootId, RecoveryProcessIdentity, RecoveryProcessSessionId, RecoveryScanRevision,
    RecoverySourceIdentity, RecoveryTimestamp,
};
use crate::watcher_coverage::{WatcherCoverageHandoff, test_linux_handoff};
use bowline_core::ids::WorkspaceId;

pub(crate) fn timestamp() -> RecoveryTimestamp {
    RecoveryTimestamp::parse("2026-08-02T12:00:00Z").expect("fixed timestamp must parse")
}

pub(crate) fn moment(milliseconds: u64) -> RecoveryMoment {
    RecoveryMoment::new(timestamp(), RecoveryInstant::from_millis(milliseconds))
}

pub(crate) fn source_identity() -> RecoverySourceIdentity {
    RecoverySourceIdentity::new(
        RecoveryProcessIdentity::new(
            RecoveryProcessBootId::new(11).expect("test process boot id must be valid"),
            RecoveryProcessSessionId::new(17).expect("test process session id must be valid"),
            timestamp(),
        ),
        WorkspaceId::new("ws_recovery_test"),
    )
}

pub(crate) fn startup_model() -> RecoveryStateMachine {
    let mut model = RecoveryStateMachine::startup_reconciliation(
        source_identity(),
        moment(0),
        BackoffPolicy::standard(),
    );
    model
        .replace_worker(moment(0))
        .expect("test recovery worker must claim ownership");
    model
}

pub(crate) fn nominal_model() -> RecoveryStateMachine {
    let mut model =
        RecoveryStateMachine::nominal(source_identity(), timestamp(), BackoffPolicy::standard());
    model
        .replace_worker(moment(0))
        .expect("test recovery worker must claim ownership");
    model
}

pub(crate) fn linux_handoff(boundary_sequence: u64) -> WatcherCoverageHandoff {
    test_linux_handoff(
        boundary_sequence + 100,
        boundary_sequence + 10,
        boundary_sequence + 1,
    )
}

pub(crate) struct ClosingAttempt {
    pub token: AttemptToken,
    pub boundary: AttemptCoverageBoundary,
    pub scan_revision: RecoveryScanRevision,
    pub close_offer: CloseOffer,
}

pub(crate) fn drive_to_closing(model: &mut RecoveryStateMachine, base_ms: u64) -> ClosingAttempt {
    let token = model
        .start_attempt(moment(base_ms))
        .expect("attempt must start");
    drive_started_attempt_to_closing(model, token, base_ms)
}

pub(crate) fn drive_started_attempt_to_closing(
    model: &mut RecoveryStateMachine,
    token: AttemptToken,
    base_ms: u64,
) -> ClosingAttempt {
    model
        .record_scan_started(token, moment(base_ms + 1))
        .expect("scan must start");
    let scan_revision =
        RecoveryScanRevision::new(token.attempt_id().get() + 10).expect("scan revision is valid");
    model
        .record_scan_completed(token, scan_revision, moment(base_ms + 2))
        .expect("scan must complete");
    model
        .record_native_boundary(
            token,
            linux_handoff(token.attempt_id().get()),
            moment(base_ms + 3),
        )
        .expect("post-scan native seal must be admitted");
    let boundary = model
        .current_native_boundary()
        .cloned()
        .expect("admitted native seal must be retained");
    let close_offer = CloseOffer::from_attempt_evidence(token, boundary.clone(), scan_revision);
    ClosingAttempt {
        token,
        boundary,
        scan_revision,
        close_offer,
    }
}
