use super::contracted::{
    ActionRequirement, BarrierFailure, ContractedProductModel, CrashPhase, RecoveryCause,
    ScanFailure,
};

#[test]
fn dense_multi_producer_load_converges_without_lost_bytes_or_serial_incidents() {
    let mut model = ContractedProductModel::new();
    model.begin_dense_source_producer();
    for change in 0..8_192 {
        let producer = change % 64;
        model.write_source(format!("producer-{producer}/change-{change}"));
    }

    assert_eq!(model.source_scan(), Err(ScanFailure::ProducerActive));
    assert!(model.source_recovery_open());
    assert_eq!(
        model.source_incident_cause(),
        Some(RecoveryCause::IngressDetailCollapsed)
    );
    model.end_dense_source_producer();
    let attempt = model.source_scan().expect("producer stopped");
    model
        .close_source_scan(attempt)
        .expect("post-scan native seal is stable");
    assert_eq!(model.source_last_closed_incident(), 1);

    model.publish_source();
    model.advance_source_observer();
    model.apply_peer(0);
    assert!(model.source_files_equal_remote());
    assert!(model.peer_files_equal_remote());
    assert!(model.source_barrier().is_ok());
    assert!(model.peer_barrier().is_ok());
}

#[test]
fn activity_across_scan_seal_and_close_forces_an_attempt_in_the_same_incident() {
    let mut model = ContractedProductModel::new();
    model.write_source("before-admission");
    model.source_loss(RecoveryCause::NativeLoss);
    let first = model
        .source_scan()
        .expect("control admission is independent");
    model.write_source("during-scan");
    assert_eq!(
        model.close_source_scan(first),
        Err(ScanFailure::RescanRequired)
    );
    assert_eq!(model.source_incident_attempts(), 1);

    let second = model.source_scan().expect("same incident rescans");
    model.write_source("during-seal");
    assert_eq!(
        model.close_source_scan(second),
        Err(ScanFailure::RescanRequired)
    );
    let third = model.source_scan().expect("third covering attempt");
    model
        .close_source_scan(third)
        .expect("stable post-scan seal closes");
    assert_eq!(model.source_last_closed_incident(), 1);
}

#[test]
fn stale_attempt_completion_cannot_close_a_newer_covering_attempt() {
    let mut model = ContractedProductModel::new();
    model.source_loss(RecoveryCause::NativeLoss);
    let stale = model.source_scan().expect("first attempt");
    model.write_source("late-callback");
    assert_eq!(
        model.close_source_scan(stale),
        Err(ScanFailure::RescanRequired)
    );
    let current = model.source_scan().expect("replacement attempt");
    assert_eq!(
        model.close_source_scan(stale),
        Err(ScanFailure::StaleAttempt)
    );
    model
        .close_source_scan(current)
        .expect("current attempt closes");
}

#[test]
fn recovery_closure_is_independent_of_transport_and_observer_convergence() {
    let mut model = ContractedProductModel::new();
    model.write_source("src/main.rs");
    model.source_loss(RecoveryCause::NativeLoss);
    let attempt = model.source_scan().expect("scan admitted");
    model.close_source_scan(attempt).expect("coverage closes");

    assert_eq!(model.source_barrier(), Err(BarrierFailure::Syncing));
    model.publish_source();
    assert_eq!(model.source_barrier(), Err(BarrierFailure::RefMismatch));
    model.advance_source_observer();
    assert!(model.source_barrier().is_ok());
}

#[test]
fn public_barrier_capacity_cannot_starve_recovery_control() {
    let mut model = ContractedProductModel::new();
    for _ in 0..64 {
        model
            .register_source_barrier()
            .expect("bounded public registration");
    }
    assert_eq!(
        model.register_source_barrier(),
        Err(BarrierFailure::ResourceExhausted)
    );
    model.source_loss(RecoveryCause::NativeLoss);
    assert!(model.source_scan().is_ok());
}

#[test]
fn remote_apply_events_and_an_external_edit_use_the_same_conservative_protocol() {
    let mut model = ContractedProductModel::new();
    model.write_source("shared/from-source");
    model.source_loss(RecoveryCause::NativeLoss);
    let source_attempt = model.source_scan().expect("source scan");
    model
        .close_source_scan(source_attempt)
        .expect("source seal");
    model.publish_source();
    model.advance_source_observer();

    model.apply_peer(300);
    model.write_peer("external/concurrent-edit");
    assert!(model.peer_barrier().is_err());
    let first = model.peer_scan().expect("peer coverage scan");
    model.write_peer("external/tail");
    assert_eq!(
        model.close_peer_scan(first),
        Err(ScanFailure::RescanRequired)
    );
    let second = model.peer_scan().expect("peer rescan");
    model.close_peer_scan(second).expect("peer stable seal");
}

#[test]
fn observer_lag_exit_and_ambiguous_cas_never_report_ready() {
    let mut model = converged_model();
    model.source_observer_live(false);
    assert_eq!(
        model.source_barrier(),
        Err(BarrierFailure::ObserverUnavailable)
    );
    model.source_observer_live(true);
    model.mark_source_cas_uncertain(true);
    assert_eq!(model.source_barrier(), Err(BarrierFailure::Syncing));
    model.mark_source_cas_uncertain(false);
    assert!(model.source_barrier().is_ok());
}

#[test]
fn unstable_files_active_cycles_and_root_replacement_block_exactness() {
    let mut model = ContractedProductModel::new();
    model.source_loss(RecoveryCause::NativeLoss);
    model.set_source_cycle_active(true);
    assert_eq!(model.source_scan(), Err(ScanFailure::EngineBusy));
    model.set_source_cycle_active(false);
    model.set_source_unstable(true);
    assert_eq!(model.source_scan(), Err(ScanFailure::UnstableFiles));
    model.set_source_unstable(false);
    model.replace_source_root();
    let attempt = model.source_scan().expect("replacement stream scans");
    model.close_source_scan(attempt).expect("replacement seal");
}

#[test]
fn terminal_and_action_required_failures_cannot_become_retry_loops() {
    for requirement in [
        ActionRequirement::Authentication,
        ActionRequirement::SignerTrust,
        ActionRequirement::Integrity,
        ActionRequirement::MassDeletion,
        ActionRequirement::EngineDisconnected,
    ] {
        let mut model = converged_model();
        model.block_source(requirement);
        assert_eq!(
            model.source_barrier(),
            Err(BarrierFailure::Blocked(requirement))
        );
        if requirement != ActionRequirement::Integrity {
            model.restore_source(requirement);
            assert_eq!(model.source_barrier(), Err(BarrierFailure::Recovering));
        }
    }
}

#[test]
fn every_recovery_phase_crash_restarts_with_reconciliation_and_no_false_ready() {
    for phase in [
        CrashPhase::Rearming,
        CrashPhase::AwaitingCoverage,
        CrashPhase::Scanning,
        CrashPhase::AwaitingSeal,
        CrashPhase::Closing,
    ] {
        let mut model = converged_model();
        model.source_loss(RecoveryCause::NativeLoss);
        model.crash_source(phase);
        assert!(model.source_recovery_open());
        assert_eq!(model.source_barrier(), Err(BarrierFailure::Recovering));
        let attempt = model.source_scan().expect("startup scan after restart");
        model.close_source_scan(attempt).expect("startup seal");
    }
}

#[test]
fn an_old_exact_receipt_remains_valid_history_after_a_later_incident() {
    let mut model = converged_model();
    let receipt = model.source_barrier().expect("initial exact receipt");
    model.source_loss(RecoveryCause::NativeLoss);
    assert_eq!(model.source_barrier(), Err(BarrierFailure::Recovering));
    assert_eq!(receipt.remote_head, 1);
    assert_eq!(receipt.incident_id, 1);
}

fn converged_model() -> ContractedProductModel {
    let mut model = ContractedProductModel::new();
    model.write_source("src/main.rs");
    model.source_loss(RecoveryCause::NativeLoss);
    let attempt = model.source_scan().expect("scan");
    model.close_source_scan(attempt).expect("seal");
    model.publish_source();
    model.advance_source_observer();
    model.apply_peer(0);
    model
}
