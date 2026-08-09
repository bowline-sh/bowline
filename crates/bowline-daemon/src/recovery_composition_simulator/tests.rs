use super::model::{
    DaemonId, DeterministicCompositionSimulator, Milestone, SeamFailure, SimMillis,
};

#[test]
fn pre_scan_boundary_omits_a_delayed_callback() {
    let result = DeterministicCompositionSimulator::new().reproduce_pre_scan_boundary_race();

    assert_eq!(result.failure, SeamFailure::LateCallbackOmitted);
    assert_eq!(
        milestones(&result),
        vec![
            Milestone::FilesystemMutation,
            Milestone::NativeBoundary,
            Milestone::AuthoritativeScan,
            Milestone::FilesystemMutation,
            Milestone::RecoveryClosed,
            Milestone::CallbackDelivered,
        ]
    );
}

#[test]
fn watcher_data_pressure_rejects_recovery_control() {
    let result = DeterministicCompositionSimulator::new().reproduce_shared_inbox_starvation();

    assert_eq!(result.failure, SeamFailure::RecoveryControlStarved);
    assert_eq!(
        milestones(&result),
        vec![
            Milestone::IncrementalWakeAdmitted,
            Milestone::ControlRejected,
        ]
    );
}

#[test]
fn a_bounded_queue_can_still_contain_an_unbounded_work_item() {
    let result = DeterministicCompositionSimulator::new().reproduce_unbounded_work_item();

    assert_eq!(result.failure, SeamFailure::UnboundedDataWorkItem);
    assert_eq!(
        milestones(&result),
        vec![Milestone::IncrementalWakeAdmitted]
    );
}

#[test]
fn cycle_activity_cannot_identify_remote_materialization() {
    let result = DeterministicCompositionSimulator::new().reproduce_materialization_inference();

    assert_eq!(result.failure, SeamFailure::MaterializationMisclassified);
    assert_eq!(milestones(&result), vec![Milestone::PublishStarted]);
}

#[test]
fn serial_tree_patching_and_object_transactions_exceed_the_wave_budget() {
    let result = DeterministicCompositionSimulator::new().reproduce_transport_budget();
    let operations = result
        .operations
        .expect("transport case records operations");

    assert_eq!(result.failure, SeamFailure::TransferWaveBudgetExceeded);
    assert_eq!(operations.tree_node_reads, 1_024);
    assert_eq!(operations.tree_node_writes, 1_024);
    assert_eq!(operations.reserve_calls, 256);
    assert_eq!(operations.commit_calls, 256);
    assert_eq!(operations.transfer_waves, 32);
}

#[test]
fn grouped_tree_and_batched_transfer_fit_the_contracted_operation_budget() {
    let proof = DeterministicCompositionSimulator::new().prove_contracted_transport_budget();
    let operations = proof.operations;

    assert_eq!(operations.affected_tree_nodes, 33);
    assert_eq!(operations.tree_node_reads, operations.affected_tree_nodes);
    assert_eq!(operations.tree_node_writes, operations.affected_tree_nodes);
    assert_eq!(operations.reserve_batches, 5);
    assert_eq!(operations.commit_batches, 5);
    assert!(operations.file_put_waves <= 8);
    assert!(operations.file_get_waves <= 8);
    assert!(
        proof.modeled_p95_millis <= 18_000,
        "modeled p95 {}ms must preserve 12s against the 30s contract",
        proof.modeled_p95_millis
    );
}

#[test]
fn a_mismatched_content_identity_reaches_the_legacy_overwrite_path() {
    let result = DeterministicCompositionSimulator::new().reproduce_integrity_overwrite();

    assert_eq!(result.failure, SeamFailure::ImmutableObjectOverwritten);
    assert_eq!(milestones(&result), vec![Milestone::IntegrityMismatch]);
}

#[test]
fn a_successor_campaign_resets_the_legacy_release_circuit() {
    let result = DeterministicCompositionSimulator::new().reproduce_release_circuit_reset();

    assert_eq!(result.failure, SeamFailure::ReleaseCircuitReset);
    assert_eq!(
        milestones(&result),
        vec![
            Milestone::ReleaseCircuitOpened,
            Milestone::SuccessorCampaignOpened,
        ]
    );
}

#[test]
fn the_simulator_tracks_two_daemons_observer_lag_and_virtual_time() {
    let mut simulator = DeterministicCompositionSimulator::new();
    simulator.observer_and_peer_are_explicit_authorities();

    assert_eq!(
        simulator.trace(),
        [
            super::model::TraceEvent {
                at: SimMillis::from_millis(25),
                daemon: DaemonId::Source,
                milestone: Milestone::ObserverAdvanced,
            },
            super::model::TraceEvent {
                at: SimMillis::from_millis(26),
                daemon: DaemonId::Peer,
                milestone: Milestone::PeerMaterialized,
            },
        ]
    );
}

fn milestones(result: &super::model::ScenarioResult) -> Vec<Milestone> {
    result.trace.iter().map(|event| event.milestone).collect()
}
