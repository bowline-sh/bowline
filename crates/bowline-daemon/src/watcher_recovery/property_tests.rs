use proptest::prelude::*;

use super::reducer::RecoveryStateMachine;
use super::{
    DependencyFailure, DependencyFailureClass, RecoveryCause, RecoveryFailureCode, RecoveryPhase,
    RecoveryScanRevision,
    test_support::{linux_handoff, moment, startup_model, timestamp},
};

proptest! {
    #[test]
    fn arbitrary_transition_sequences_preserve_state_invariants(
        actions in prop::collection::vec(0_u8..13, 1..256)
    ) {
        let mut model = startup_model();
        for (index, action) in actions.into_iter().enumerate() {
            apply_action(&mut model, action, (index as u64 + 1) * 100_000);
            prop_assert!(model.invariant_holds());
        }
    }
}

fn apply_action(model: &mut RecoveryStateMachine, action: u8, now: u64) {
    match action {
        0 => {
            let _ = model.observe_activity(moment(now));
        }
        1 => {
            let _ = model.observe_loss(RecoveryCause::NativeRescanRequired, moment(now));
        }
        2 => {
            let _ = model.start_attempt(moment(now));
        }
        3 => start_scan(model, now),
        4 => complete_scan(model, now),
        5 => record_seal(model, now),
        6 => offer_current_close(model, now),
        7 => fail_current_attempt(model, DependencyFailureClass::Retryable, now),
        8 => {
            let _ = model.retry_if_due(moment(now));
        }
        9 => fail_current_attempt(model, DependencyFailureClass::AuthorizationLost, now),
        10 => {
            let _ = model.restore_authority(moment(now));
        }
        11 => {
            let _ = model.replace_worker(moment(now));
        }
        12 => {
            if let Some(worker) = model.current_worker_ownership() {
                let _ = model.worker_exited(worker, moment(now));
            }
        }
        _ => {}
    }
}

fn start_scan(model: &mut RecoveryStateMachine, now: u64) {
    if let Some(token) = model.current_attempt() {
        let _ = model.record_scan_started(token, moment(now));
    }
}

fn complete_scan(model: &mut RecoveryStateMachine, now: u64) {
    if let Some(token) = model.current_attempt()
        && let Ok(revision) = RecoveryScanRevision::new(now)
    {
        let _ = model.record_scan_completed(token, revision, moment(now));
    }
}

fn record_seal(model: &mut RecoveryStateMachine, now: u64) {
    if let Some(token) = model.current_attempt() {
        let _ = model.record_native_boundary(
            token,
            linux_handoff(token.attempt_id().get()),
            moment(now),
        );
    }
}

fn offer_current_close(model: &mut RecoveryStateMachine, now: u64) {
    if model.phase() != Some(RecoveryPhase::Closing) {
        return;
    }
    let Some(token) = model.current_attempt() else {
        return;
    };
    let Some(boundary) = model.current_native_boundary().cloned() else {
        return;
    };
    let Some(scan_revision) = model.current_scan_revision() else {
        return;
    };
    let offer = super::types::CloseOffer::from_attempt_evidence(token, boundary, scan_revision);
    let _ = model.offer_close(&offer, moment(now));
}

fn fail_current_attempt(model: &mut RecoveryStateMachine, class: DependencyFailureClass, now: u64) {
    let Some(token) = model.current_attempt() else {
        return;
    };
    let failure = DependencyFailure::new(
        class,
        RecoveryFailureCode::DependencyUnavailable,
        timestamp(),
    );
    let _ = model.record_dependency_failure(token, failure, moment(now));
}
