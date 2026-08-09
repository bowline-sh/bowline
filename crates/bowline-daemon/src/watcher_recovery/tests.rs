use std::sync::{Arc, Barrier as ThreadBarrier};
use std::thread;

use super::{
    ActivityAdmission, BackoffPolicy, CloseDisposition, DependencyFailure, DependencyFailureClass,
    FailureDisposition, RecoveryCause, RecoveryFailureCode, RecoveryLifecycle, RecoveryPhase,
    RecoveryScanRevision, RecoveryTransitionError, WatcherRecoveryCoordinator,
    WatcherRecoveryCoordinatorError,
    test_support::{
        drive_to_closing, moment, nominal_model, source_identity, startup_model, timestamp,
    },
    types::{MAX_DURATION_MS, MAX_SCHEMA_INTEGER},
};

fn failure(class: DependencyFailureClass, code: RecoveryFailureCode) -> DependencyFailure {
    DependencyFailure::new(class, code, timestamp())
}

#[test]
fn startup_reconciliation_is_an_open_non_ready_incident() {
    let model = startup_model();
    let snapshot = model.snapshot();
    assert_eq!(snapshot.lifecycle(), RecoveryLifecycle::Recovering);
    assert_eq!(snapshot.phase(), Some(RecoveryPhase::Rearming));
    assert!(snapshot.rescan_required());
    assert!(model.invariant_holds());
}

#[test]
fn first_loss_opens_one_incident_and_later_losses_coalesce() {
    let mut model = nominal_model();
    let first = model
        .observe_loss(RecoveryCause::NativeRescanRequired, moment(1))
        .expect("first loss opens recovery");
    let incident = match first {
        super::LossAdmission::Opened { incident_id } => incident_id,
        other => panic!("unexpected admission: {other:?}"),
    };
    let second = model
        .observe_loss(RecoveryCause::WatcherDisconnected, moment(2))
        .expect("later loss coalesces");
    assert_eq!(
        second,
        super::LossAdmission::Coalesced {
            incident_id: incident
        }
    );
    assert_eq!(
        model.cause_count(RecoveryCause::NativeRescanRequired).get(),
        1
    );
    assert_eq!(
        model.cause_count(RecoveryCause::WatcherDisconnected).get(),
        1
    );
    assert!(model.invariant_holds());
}

#[test]
fn scan_then_post_scan_seal_is_the_only_close_order() {
    let mut model = startup_model();
    let token = model.start_attempt(moment(1)).expect("attempt starts");
    assert!(matches!(
        model.record_native_boundary(token, super::test_support::linux_handoff(1), moment(2)),
        Err(RecoveryTransitionError::OutOfOrder { .. })
    ));
    model
        .record_scan_started(token, moment(3))
        .expect("scan starts");
    let revision = RecoveryScanRevision::new(9).expect("revision is valid");
    model
        .record_scan_completed(token, revision, moment(4))
        .expect("scan completes");
    model
        .record_native_boundary(token, super::test_support::linux_handoff(1), moment(5))
        .expect("post-scan seal records");
    assert_eq!(model.phase(), Some(RecoveryPhase::Closing));
    assert!(model.invariant_holds());
}

#[test]
fn close_receipt_contains_only_local_scan_and_native_seal_authority() {
    let mut model = startup_model();
    let closing = drive_to_closing(&mut model, 1);
    let disposition = model
        .offer_close(&closing.close_offer, moment(10))
        .expect("close succeeds");
    let CloseDisposition::Closed(receipt) = disposition else {
        panic!("stable attempt must close");
    };
    assert_eq!(receipt.incident_id(), closing.token.incident_id());
    assert_eq!(receipt.attempt_id(), closing.token.attempt_id());
    assert_eq!(receipt.authoritative_scan_revision(), closing.scan_revision);
    assert_eq!(receipt.native_boundary(), &closing.boundary);
    assert_eq!(model.lifecycle(), RecoveryLifecycle::Nominal);
}

#[test]
fn activity_before_scan_is_covered_but_activity_after_scan_forces_rescan() {
    let mut model = startup_model();
    let token = model.start_attempt(moment(1)).expect("attempt starts");
    model.observe_activity(moment(2)).expect("activity records");
    model
        .record_scan_started(token, moment(3))
        .expect("scan starts and covers prior activity");
    let revision = RecoveryScanRevision::new(7).expect("revision is valid");
    model
        .record_scan_completed(token, revision, moment(4))
        .expect("scan completes");
    // A write the scan could not have seen. Suppressed, not forwarded: a
    // forwarded write is durable in the ingress and is deliberately not this
    // scan's responsibility.
    model
        .observe_suppressed(moment(5))
        .expect("tail loss records");
    model
        .record_native_boundary(token, super::test_support::linux_handoff(1), moment(6))
        .expect("seal records");
    let boundary = model
        .current_native_boundary()
        .cloned()
        .expect("seal retained");
    let offer = super::types::CloseOffer::from_attempt_evidence(token, boundary, revision);
    assert!(matches!(
        model
            .offer_close(&offer, moment(7))
            .expect("close checks tail"),
        CloseDisposition::RetryRequired { .. }
    ));
    assert_eq!(model.phase(), Some(RecoveryPhase::Rearming));
}

#[test]
fn stale_attempt_cannot_close_a_replacement_attempt() {
    let mut model = startup_model();
    let stale = drive_to_closing(&mut model, 1);
    model
        .observe_suppressed(moment(6))
        .expect("tail invalidates");
    assert!(matches!(
        model
            .offer_close(&stale.close_offer, moment(7))
            .expect("retry required"),
        CloseDisposition::RetryRequired { .. }
    ));
    model.start_attempt(moment(8)).expect("replacement starts");
    assert_eq!(
        model.offer_close(&stale.close_offer, moment(9)),
        Err(RecoveryTransitionError::AttemptMismatch)
    );
}

// The safety leg of the split. A write lost after the covering scan sealed its
// native boundary is a write nothing has observed, and closing over it would
// attest coverage that does not exist. Both the rescan flag and the loss
// watermark carry this today, so the assertion pins the property rather than one
// mechanism -- but without it, deleting the watermark leg from the close fence
// passes the whole suite.
#[test]
fn a_loss_after_the_native_boundary_cannot_be_closed_over() {
    let mut model = startup_model();
    let closing = drive_to_closing(&mut model, 1);
    model
        .observe_loss(RecoveryCause::NativeCallbackLaneSaturated, moment(6))
        .expect("a post-boundary loss is admitted");
    assert!(
        matches!(
            model
                .offer_close(&closing.close_offer, moment(7))
                .expect("close is offered"),
            CloseDisposition::RetryRequired { .. }
        ),
        "a write lost after the seal must force another covering scan, never be closed over"
    );
}

// The counterpart to the test below, and the reason the watermark is split. A
// user typing, a compile writing objects, an agent editing files -- all forward
// normally and are durable in the ingress the moment they are accepted. Fencing
// the close on them meant an incident could not close while anyone was working,
// so the engine stayed paused through the chase and a post-burst edit sat on the
// machine for 79s against a 30s budget.
#[test]
fn continuous_forwarded_writes_do_not_keep_an_incident_open() {
    let mut model = startup_model();
    let closing = drive_to_closing(&mut model, 1);
    for tick in 1..=8 {
        model
            .observe_activity(moment(5 + tick))
            .expect("forwarded activity records");
    }
    assert!(
        matches!(
            model
                .offer_close(&closing.close_offer, moment(20))
                .expect("close is offered"),
            CloseDisposition::Closed(_)
        ),
        "forwarded writes are durable in the ingress, so they must not force the covering scan to be redone"
    );
}

#[test]
fn continuous_suppressed_writes_keep_one_incident_non_ready() {
    let mut model = startup_model();
    let incident = model
        .current_incident_id()
        .expect("startup incident exists");
    for base in 1..=8 {
        let closing = drive_to_closing(&mut model, base * 10);
        model
            .observe_suppressed(moment(base * 10 + 4))
            .expect("producer loss records");
        assert!(matches!(
            model
                .offer_close(&closing.close_offer, moment(base * 10 + 5))
                .expect("attempt retries"),
            CloseDisposition::RetryRequired { incident_id } if incident_id == incident
        ));
        assert_eq!(model.lifecycle(), RecoveryLifecycle::Recovering);
    }
    assert_eq!(model.current_incident_id(), Some(incident));
    assert_eq!(model.attempt_count().get(), 8);
}

#[test]
fn retryable_failure_backs_off_and_terminal_failure_blocks() {
    let mut retrying = startup_model();
    let token = retrying.start_attempt(moment(1)).expect("attempt starts");
    assert!(matches!(
        retrying
            .record_dependency_failure(
                token,
                failure(
                    DependencyFailureClass::Retryable,
                    RecoveryFailureCode::DependencyUnavailable,
                ),
                moment(2),
            )
            .expect("retry records"),
        FailureDisposition::RetryScheduled { .. }
    ));
    assert_eq!(retrying.phase(), Some(RecoveryPhase::BackingOff));

    let mut blocked = startup_model();
    let token = blocked.start_attempt(moment(1)).expect("attempt starts");
    assert!(matches!(
        blocked
            .record_dependency_failure(
                token,
                failure(
                    DependencyFailureClass::Integrity,
                    RecoveryFailureCode::IntegrityMismatch,
                ),
                moment(2),
            )
            .expect("terminal failure records"),
        FailureDisposition::Blocked { .. }
    ));
    assert_eq!(blocked.lifecycle(), RecoveryLifecycle::Blocked);
}

#[test]
fn only_authority_failures_can_be_restored() {
    let mut model = startup_model();
    let token = model.start_attempt(moment(1)).expect("attempt starts");
    model
        .record_dependency_failure(
            token,
            failure(
                DependencyFailureClass::AuthorizationLost,
                RecoveryFailureCode::AuthorizationLost,
            ),
            moment(2),
        )
        .expect("authority loss blocks");
    model
        .restore_authority(moment(3))
        .expect("authority restores");
    assert_eq!(model.phase(), Some(RecoveryPhase::Rearming));
}

#[test]
fn worker_replacement_rearms_same_incident_and_fences_stale_owner() {
    let coordinator = WatcherRecoveryCoordinator::startup_reconciliation(
        source_identity(),
        moment(0),
        BackoffPolicy::standard(),
    );
    let stale = coordinator.replace_worker(moment(1)).expect("first owner");
    let incident = coordinator
        .snapshot()
        .expect("snapshot")
        .incident_id()
        .expect("incident");
    let current = coordinator
        .replace_worker(moment(2))
        .expect("replacement owner");
    assert_ne!(stale, current);
    assert!(matches!(
        coordinator.start_attempt(stale, moment(3)),
        Err(WatcherRecoveryCoordinatorError::Transition(
            RecoveryTransitionError::WorkerMismatch
        ))
    ));
    assert_eq!(
        coordinator.snapshot().expect("snapshot").incident_id(),
        Some(incident)
    );
}

#[test]
fn native_invalidation_at_close_forces_another_attempt() {
    let mut model = startup_model();
    let closing = drive_to_closing(&mut model, 1);
    crate::watcher_coverage::invalidate_test_handoff(closing.boundary.handoff());
    assert!(!closing.boundary.is_current());
    let worker = model
        .current_worker_ownership()
        .expect("model retains worker");
    let coordinator = WatcherRecoveryCoordinator::from_model_for_test(model);
    assert!(matches!(
        coordinator
            .offer_close(worker, closing.token, closing.scan_revision, moment(10))
            .expect("stale native seal rejects"),
        CloseDisposition::RetryRequired { .. }
    ));
}

#[test]
fn activity_and_close_have_one_linearizable_owner() {
    let mut model = startup_model();
    let closing = drive_to_closing(&mut model, 1);
    let worker = model
        .current_worker_ownership()
        .expect("model retains worker");
    let coordinator = Arc::new(WatcherRecoveryCoordinator::from_model_for_test(model));
    let gate = Arc::new(ThreadBarrier::new(3));
    let close_coordinator = Arc::clone(&coordinator);
    let close_gate = Arc::clone(&gate);
    let close = thread::spawn(move || {
        close_gate.wait();
        close_coordinator.offer_close(worker, closing.token, closing.scan_revision, moment(20))
    });
    let activity_coordinator = Arc::clone(&coordinator);
    let activity_gate = Arc::clone(&gate);
    let activity = thread::spawn(move || {
        activity_gate.wait();
        activity_coordinator.observe_activity(moment(20))
    });
    gate.wait();
    let close_result = close.join().expect("close thread");
    activity
        .join()
        .expect("activity thread")
        .expect("activity admitted");
    let disposition = close_result.expect("close transition remains valid");
    let lifecycle = coordinator.snapshot().expect("snapshot").lifecycle();
    match disposition {
        CloseDisposition::Closed(_) => assert_eq!(lifecycle, RecoveryLifecycle::Nominal),
        CloseDisposition::RetryRequired { .. } => {
            assert_eq!(lifecycle, RecoveryLifecycle::Recovering)
        }
    }
}

#[test]
fn completed_receipt_remains_valid_after_a_later_incident() {
    let mut model = startup_model();
    let closing = drive_to_closing(&mut model, 1);
    let receipt = match model
        .offer_close(&closing.close_offer, moment(10))
        .expect("close succeeds")
    {
        CloseDisposition::Closed(receipt) => receipt,
        CloseDisposition::RetryRequired { .. } => panic!("stable close must win"),
    };
    model
        .observe_loss(RecoveryCause::WatcherDisconnected, moment(11))
        .expect("later incident opens");
    assert_eq!(receipt.authoritative_scan_revision(), closing.scan_revision);
    assert_eq!(model.lifecycle(), RecoveryLifecycle::Recovering);
}

#[test]
fn callback_activity_advances_blocked_incident_without_unblocking_it() {
    let mut model = startup_model();
    let token = model.start_attempt(moment(1)).expect("attempt starts");
    model
        .record_dependency_failure(
            token,
            failure(
                DependencyFailureClass::Integrity,
                RecoveryFailureCode::IntegrityMismatch,
            ),
            moment(2),
        )
        .expect("blocks");
    assert!(matches!(
        model.observe_activity(moment(3)).expect("activity records"),
        ActivityAdmission::BlockedIncidentAdvanced { .. }
    ));
    assert_eq!(model.lifecycle(), RecoveryLifecycle::Blocked);
}

#[test]
fn snapshot_schema_three_contains_scan_and_post_scan_native_evidence() {
    let mut model = startup_model();
    let closing = drive_to_closing(&mut model, 1);
    model
        .offer_close(&closing.close_offer, moment(10))
        .expect("close succeeds");
    let json = serde_json::to_value(model.snapshot()).expect("snapshot serializes");
    assert_eq!(json["schemaVersion"], 3);
    assert_eq!(
        json["lastClosure"]["authoritativeScanRevision"],
        closing.scan_revision.get()
    );
    assert!(json["lastClosure"].get("engineConvergence").is_none());
    assert!(json["lastClosure"].get("observedRef").is_none());
}

#[test]
fn duration_and_identifier_exhaustion_fail_closed() {
    let mut duration = startup_model();
    let closing = drive_to_closing(&mut duration, 1);
    assert_eq!(
        duration.offer_close(&closing.close_offer, moment(MAX_DURATION_MS + 2)),
        Err(RecoveryTransitionError::ClosureDurationOutOfRange)
    );
    assert_eq!(duration.lifecycle(), RecoveryLifecycle::Blocked);

    let mut ids = startup_model();
    ids.set_next_attempt_id_for_test(u64::MAX);
    assert!(matches!(
        ids.start_attempt(moment(1)),
        Err(RecoveryTransitionError::IdentifierExhausted { .. })
    ));
    assert_eq!(ids.lifecycle(), RecoveryLifecycle::Blocked);

    let mut incidents = nominal_model();
    incidents.set_next_incident_id_for_test(u64::MAX);
    assert!(
        incidents
            .observe_loss(RecoveryCause::RootReplaced, moment(1))
            .is_err()
    );
    assert_eq!(incidents.lifecycle(), RecoveryLifecycle::Blocked);

    let mut workers = startup_model();
    workers.set_next_worker_id_for_test(u64::MAX);
    assert!(workers.replace_worker(moment(1)).is_err());
    assert_eq!(workers.lifecycle(), RecoveryLifecycle::Blocked);

    let mut activity = startup_model();
    activity.set_activity_watermark_for_test(MAX_SCHEMA_INTEGER);
    assert!(activity.observe_activity(moment(1)).is_err());
    assert_eq!(activity.lifecycle(), RecoveryLifecycle::Blocked);

    let mut revision = startup_model();
    revision.set_recovery_revision_for_test(MAX_SCHEMA_INTEGER);
    assert!(revision.start_attempt(moment(1)).is_err());
    assert_eq!(revision.lifecycle(), RecoveryLifecycle::Blocked);
}

#[test]
fn successful_transitions_advance_revision_once() {
    let mut model = startup_model();
    let before = model.revision();
    let token = model.start_attempt(moment(1)).expect("attempt starts");
    assert_eq!(model.revision().get(), before.get() + 1);
    let before = model.revision();
    model
        .record_scan_started(token, moment(2))
        .expect("scan starts");
    assert_eq!(model.revision().get(), before.get() + 1);
}
