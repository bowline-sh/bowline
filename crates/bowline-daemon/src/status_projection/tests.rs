use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bowline_core::commands::StatusCommandOutput;
use bowline_core::ids::WorkspaceId;
use bowline_local::metadata::MetadataStore;
use bowline_local::status::{LocalStatusFacts, LocalStatusRevision, StatusOptions, compose_status};

use super::retry::RetrySchedule;
use super::types::semantic_fingerprint;
use super::*;

#[test]
fn canonical_convergence_status_owns_readiness_and_queue_projection() {
    use bowline_local::sync::manifest_engine::{Degradation, EnginePhase, EngineSnapshot};

    let metadata = missing_status("2026-07-19T12:00:00Z");
    // A caught-up engine at revision 42 maps to a Ready, empty-queue v8 summary.
    let status = engine_convergence_facts(&EngineSnapshot {
        revision: 42,
        phase: EnginePhase::Idle,
        observed_ref: None,
        applied_manifest: None,
        pending_intents: 0,
        dirty: 0,
        dirty_paths: Arc::new(BTreeSet::new()),
        dirty_subtree_paths: Arc::new(BTreeSet::new()),
        pending_intent_paths: Arc::new(BTreeSet::new()),
        scan_required: false,
        unattributed_pull_pending: false,
        cycle_active: false,
        last_success_at: None,
        degradation: Degradation::Nominal,
    });
    let mut sources = BTreeMap::from([
        (
            StatusSource::Convergence,
            SourceRevision {
                source: StatusSource::Convergence,
                revision: StatusSourceRevision::new(42),
                observed_at: StatusTimestamp::new("2026-07-19T12:00:00Z"),
                freshness: SourceFreshness::Current,
            },
        ),
        (
            StatusSource::SyncRuntime,
            SourceRevision {
                source: StatusSource::SyncRuntime,
                revision: StatusSourceRevision::new(99),
                observed_at: StatusTimestamp::new("2026-07-19T12:00:00Z"),
                freshness: SourceFreshness::Current,
            },
        ),
    ]);
    let source_facts = BTreeMap::from([
        (
            StatusSource::Convergence,
            StatusSourceFacts::Convergence(Box::new(status)),
        ),
        (
            StatusSource::SyncRuntime,
            StatusSourceFacts::SyncRuntime(StatusSourceStateFacts {
                state: StatusSourceState::Ready,
                pending_count: 9,
            }),
        ),
    ]);
    let output = super::reducer::reduce_projection_status(
        &metadata,
        &sources,
        &source_facts,
        &StatusTimestamp::new("2026-07-19T12:00:00Z"),
    );

    let convergence = output.convergence.expect("convergence summary");
    assert_eq!(convergence.revision, 42);
    assert_eq!(
        convergence.state,
        bowline_core::status::ConvergenceReadinessState::Ready
    );
    assert!(convergence.reasons.is_empty());
    assert_eq!(output.sync_queue.expect("canonical queue").queued, 0);

    sources
        .get_mut(&StatusSource::Convergence)
        .expect("convergence revision")
        .freshness = SourceFreshness::Stale;
    let stale = super::reducer::reduce_projection_status(
        &metadata,
        &sources,
        &source_facts,
        &StatusTimestamp::new("2026-07-19T12:00:01Z"),
    );
    assert!(stale.convergence.is_none());
    assert!(stale.sync_queue.is_none());

    sources
        .get_mut(&StatusSource::Convergence)
        .expect("convergence revision")
        .freshness = SourceFreshness::Failed;
    let unavailable = super::reducer::reduce_projection_status(
        &metadata,
        &sources,
        &source_facts,
        &StatusTimestamp::new("2026-07-19T12:00:02Z"),
    );
    assert!(unavailable.convergence.is_none());
    assert!(unavailable.sync_queue.is_none());
    assert!(unavailable.items.iter().any(|item| {
        item.kind == bowline_core::status::StatusItemKind::Continuity
            && item.summary == "Workspace convergence status collection failed."
    }));
    assert!(
        unavailable
            .limits
            .iter()
            .any(|limit| limit.capability == "convergence")
    );
}

#[test]
fn fingerprint_ignores_order_and_explicit_observation_metadata() {
    let output = timestamp_rich_status("2026-07-13T10:00:00Z");
    let mut reordered = output.clone();
    reordered.generated_at = "2026-07-13T11:00:00Z".to_string();
    reordered.status_summary.observed_at = "2026-07-13T11:00:00Z".to_string();
    for fact in &mut reordered.status_summary.facts {
        fact.observed_at = "2026-07-13T11:00:00Z".to_string();
    }
    reordered.items.reverse();
    reordered.next_actions.reverse();
    reordered.status.attention_items.reverse();

    let facts = metadata_facts(output.clone(), 1, "2026-07-13T10:00:00Z");
    let reordered_facts = metadata_facts(reordered.clone(), 41, "2026-07-13T11:00:00Z");
    let sources = source_revisions(1, "2026-07-13T10:00:00Z", SourceFreshness::Current);
    let reordered_sources = source_revisions(99, "2026-07-13T11:00:00Z", SourceFreshness::Current);

    let first = semantic_fingerprint(&output, &sources, &facts).expect("first fingerprint");
    let second = semantic_fingerprint(&reordered, &reordered_sources, &reordered_facts)
        .expect("second fingerprint");

    assert_eq!(first, second);
    assert_eq!(first.as_bytes().len(), 32);
    assert_eq!(StatusSequence::INITIAL.next().get(), 2);
}

#[test]
fn every_public_domain_timestamp_path_changes_the_semantic_fingerprint() {
    let baseline = timestamp_rich_status("2026-07-13T10:00:00Z");
    let sources = source_revisions(1, "2026-07-13T10:00:00Z", SourceFreshness::Current);
    let baseline_facts = metadata_facts(baseline.clone(), 1, "2026-07-13T10:00:00Z");
    let baseline_fingerprint = semantic_fingerprint(&baseline, &sources, &baseline_facts)
        .expect("baseline timestamp fingerprint");

    for (path, mutate) in semantic_timestamp_mutations() {
        let mut changed = baseline.clone();
        mutate(&mut changed);
        let changed_facts = metadata_facts(changed.clone(), 2, "2026-07-13T11:00:00Z");
        let changed_fingerprint = semantic_fingerprint(&changed, &sources, &changed_facts)
            .expect("changed timestamp fingerprint");
        assert_ne!(
            changed_fingerprint, baseline_fingerprint,
            "domain timestamp path must be semantic: {path}"
        );
    }
}

#[test]
fn every_public_domain_timestamp_path_increments_sequence_and_broadcasts() {
    for (path, mutate) in semantic_timestamp_mutations() {
        let baseline = timestamp_rich_status("2026-07-13T10:00:00Z");
        let mut changed = baseline.clone();
        mutate(&mut changed);
        let service = service_with_collectors(
            Duration::from_secs(60),
            vec![Box::new(FakeCollector::metadata_with_outcomes(
                Arc::new(AtomicU64::new(0)),
                VecDeque::from([
                    FakeOutcome::Metadata(Box::new(baseline)),
                    FakeOutcome::Metadata(Box::new(changed)),
                ]),
            ))],
        );
        let subscription = service.subscribe().expect("timestamp subscription");
        service
            .input()
            .send(StatusInputEvent::SourceChanged(StatusSource::Metadata))
            .expect("timestamp source change");
        let update = subscription
            .updates
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_else(|error| {
                panic!("domain timestamp did not broadcast for {path}: {error}")
            });
        assert_eq!(
            update.sequence,
            subscription.initial.sequence.next(),
            "{path}"
        );
        assert_ne!(
            update.semantic_fingerprint, subscription.initial.semantic_fingerprint,
            "{path}"
        );
        let metrics = service.metrics().expect("timestamp metrics");
        assert_eq!(metrics.semantic_changes, 1, "{path}");
        assert_eq!(metrics.broadcasts, 1, "{path}");
    }
}

#[test]
fn only_explicit_observation_timestamps_remain_sequence_stable_without_broadcast() {
    let baseline = timestamp_rich_status("2026-07-13T10:00:00Z");
    let mut observation_only = baseline.clone();
    observation_only.generated_at = "2026-07-13T11:00:00Z".to_string();
    observation_only.status_summary.observed_at = "2026-07-13T11:00:00Z".to_string();
    for fact in &mut observation_only.status_summary.facts {
        fact.observed_at = "2026-07-13T11:00:00Z".to_string();
    }
    let service = service_with_collectors(
        Duration::from_secs(60),
        vec![Box::new(FakeCollector::metadata_with_outcomes(
            Arc::new(AtomicU64::new(0)),
            VecDeque::from([
                FakeOutcome::Metadata(Box::new(baseline)),
                FakeOutcome::Metadata(Box::new(observation_only)),
            ]),
        ))],
    );
    let subscription = service.subscribe().expect("observation subscription");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::Metadata))
        .expect("observation source change");
    wait_for_no_op(&service);

    let current = service.current().expect("observation current projection");
    assert_ne!(current.generated_at, subscription.initial.generated_at);
    assert_ne!(
        current.sources[&StatusSource::Metadata].observed_at,
        subscription.initial.sources[&StatusSource::Metadata].observed_at
    );
    assert_eq!(current.sequence, subscription.initial.sequence);
    assert_eq!(
        current.semantic_fingerprint,
        subscription.initial.semantic_fingerprint
    );
    assert!(
        subscription
            .updates
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
    let metrics = service.metrics().expect("observation metrics");
    assert_eq!(metrics.semantic_changes, 0);
    assert_eq!(metrics.broadcasts, 0);
}

#[test]
fn source_change_calls_only_its_collector() {
    let metadata_calls = Arc::new(AtomicU64::new(0));
    let sync_calls = Arc::new(AtomicU64::new(0));
    let service = service_with_collectors(
        Duration::from_secs(60),
        vec![
            Box::new(FakeCollector::metadata(Arc::clone(&metadata_calls))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::new(),
            )),
        ],
    );
    let subscription = service.subscribe().expect("subscription");
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sync_calls.load(Ordering::SeqCst), 1);

    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("dirty sync runtime");
    let changed = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("projection update");

    assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sync_calls.load(Ordering::SeqCst), 2);
    assert_eq!(changed.sequence.get(), 2);
    let StatusSourceFacts::SyncRuntime(sync_facts) =
        &changed.source_facts[&StatusSource::SyncRuntime]
    else {
        panic!("sync runtime facts must remain observable");
    };
    assert_eq!(sync_facts.pending_count, 2);
    let metrics = service.metrics().expect("metrics");
    assert_eq!(
        metrics.collector_calls.get(&StatusSource::Metadata),
        Some(&1)
    );
    assert_eq!(
        metrics.collector_calls.get(&StatusSource::SyncRuntime),
        Some(&2)
    );
}

#[test]
fn collector_failure_retains_stale_facts_or_discards_failed_facts_by_policy() {
    let trust_outcomes = VecDeque::from([
        FakeOutcome::Updated(StatusSourceState::Ready),
        FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
    ]);
    let update_outcomes = VecDeque::from([
        FakeOutcome::Updated(StatusSourceState::Ready),
        FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
    ]);
    let service = service_with_collectors(
        Duration::from_secs(60),
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::DeviceTrust,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::new(AtomicU64::new(0)),
                trust_outcomes,
            )),
            Box::new(FakeCollector::state(
                StatusSource::UpdateAvailability,
                StatusSourceFailurePolicy::Discard,
                Arc::new(AtomicU64::new(0)),
                update_outcomes,
            )),
        ],
    );
    let subscription = service.subscribe().expect("subscription");

    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::DeviceTrust))
        .expect("dirty trust");
    let stale = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("stale projection");
    assert_eq!(
        stale.sources[&StatusSource::DeviceTrust].freshness,
        SourceFreshness::Stale
    );
    assert_eq!(stale.sources[&StatusSource::DeviceTrust].revision.get(), 1);
    assert!(stale.source_facts.contains_key(&StatusSource::DeviceTrust));

    service
        .input()
        .send(StatusInputEvent::SourceChanged(
            StatusSource::UpdateAvailability,
        ))
        .expect("dirty update availability");
    let failed = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("failed projection");
    assert_eq!(
        failed.sources[&StatusSource::UpdateAvailability].freshness,
        SourceFreshness::Failed
    );
    assert_eq!(
        failed.sources[&StatusSource::UpdateAvailability]
            .revision
            .get(),
        1
    );
    assert!(
        !failed
            .source_facts
            .contains_key(&StatusSource::UpdateAvailability)
    );
    assert_eq!(
        failed.sources[&StatusSource::DeviceTrust]
            .observed_at
            .as_str(),
        stale.sources[&StatusSource::DeviceTrust]
            .observed_at
            .as_str()
    );
}

#[test]
fn initial_failure_then_unchanged_stays_failed_until_updated_recovery() {
    let sync_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(50), Duration::from_millis(100))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::Discard,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                    FakeOutcome::Unchanged,
                    FakeOutcome::Updated(StatusSourceState::Ready),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("initial failure subscription");
    assert_eq!(
        subscription.initial.sources[&StatusSource::SyncRuntime].freshness,
        SourceFreshness::Failed
    );
    assert!(
        !subscription
            .initial
            .source_facts
            .contains_key(&StatusSource::SyncRuntime)
    );
    assert_eq!(
        subscription.initial.status.status_summary.availability,
        bowline_core::status::StatusAvailability::Unavailable
    );

    wait_for_contract_retry_schedule(&service, StatusSource::SyncRuntime, 1);
    let after_unchanged = service.current().expect("failed current projection");
    assert_eq!(after_unchanged.sequence, subscription.initial.sequence);
    assert_eq!(
        after_unchanged.sources[&StatusSource::SyncRuntime].freshness,
        SourceFreshness::Failed
    );
    assert!(
        !after_unchanged
            .source_facts
            .contains_key(&StatusSource::SyncRuntime)
    );
    assert_eq!(
        after_unchanged.status.status_summary.availability,
        bowline_core::status::StatusAvailability::Unavailable
    );
    let recovered = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("updated recovery projection");
    assert_eq!(
        recovered.sources[&StatusSource::SyncRuntime].freshness,
        SourceFreshness::Current
    );
    assert!(
        recovered
            .source_facts
            .contains_key(&StatusSource::SyncRuntime)
    );
    assert_eq!(sync_calls.load(Ordering::SeqCst), 3);
    wait_for_contract_retry_recovery(&service, StatusSource::SyncRuntime, 1);
    let metrics = service.metrics().expect("initial unchanged metrics");
    assert_eq!(metrics.broadcasts, 1);
    assert_eq!(
        metrics.collector_contract_retries_scheduled[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(
        metrics.collector_contract_retry_attempts[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(
        metrics
            .collector_skips
            .get(&StatusSource::SyncRuntime)
            .copied()
            .unwrap_or(0),
        0
    );
}

#[test]
fn discard_failure_then_unchanged_stays_failed_until_updated_recovery() {
    let notification_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(50), Duration::from_millis(100))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::NotificationState,
                StatusSourceFailurePolicy::Discard,
                Arc::clone(&notification_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                    FakeOutcome::Unchanged,
                    FakeOutcome::Updated(StatusSourceState::Ready),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("discard failure subscription");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(
            StatusSource::NotificationState,
        ))
        .expect("discard failure input");
    let failed = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("discard failed projection");
    assert_eq!(
        failed.sources[&StatusSource::NotificationState].freshness,
        SourceFreshness::Failed
    );
    assert!(
        !failed
            .source_facts
            .contains_key(&StatusSource::NotificationState)
    );

    wait_for_contract_retry_schedule(&service, StatusSource::NotificationState, 1);
    let after_unchanged = service.current().expect("discard failed current");
    assert_eq!(after_unchanged.sequence, failed.sequence);
    assert_eq!(
        after_unchanged.sources[&StatusSource::NotificationState].freshness,
        SourceFreshness::Failed
    );
    assert!(
        !after_unchanged
            .source_facts
            .contains_key(&StatusSource::NotificationState)
    );
    assert_eq!(
        after_unchanged.status.status_summary.availability,
        bowline_core::status::StatusAvailability::Unavailable
    );
    let recovered = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("discard updated recovery");
    assert_eq!(recovered.sequence, failed.sequence.next());
    assert_eq!(
        recovered.sources[&StatusSource::NotificationState].freshness,
        SourceFreshness::Current
    );
    assert!(
        recovered
            .source_facts
            .contains_key(&StatusSource::NotificationState)
    );
    assert_eq!(notification_calls.load(Ordering::SeqCst), 4);
    wait_for_contract_retry_recovery(&service, StatusSource::NotificationState, 1);
    let metrics = service.metrics().expect("discard unchanged metrics");
    assert_eq!(metrics.broadcasts, 2);
    assert_eq!(
        metrics.collector_contract_retries_scheduled[&StatusSource::NotificationState],
        1
    );
    assert_eq!(
        metrics.collector_contract_retry_attempts[&StatusSource::NotificationState],
        1
    );
    assert_eq!(
        metrics
            .collector_skips
            .get(&StatusSource::NotificationState)
            .copied()
            .unwrap_or(0),
        0
    );
}

#[test]
fn local_projection_adapter_matches_existing_composition() {
    let db_path = unique_missing_db_path();
    assert_local_projection_parity(&db_path, "/tmp/bowline-parity-missing");

    let current_db_path = unique_missing_db_path();
    std::fs::create_dir_all(
        current_db_path
            .parent()
            .expect("current database parent path"),
    )
    .expect("create current database parent");
    let store = MetadataStore::open(&current_db_path).expect("open current metadata");
    let workspace_id = WorkspaceId::new("ws_projection_parity");
    store
        .insert_workspace(&workspace_id, "Projection Parity", "2026-07-13T10:00:00Z")
        .expect("insert parity workspace");
    store
        .insert_root(
            "root_projection_parity",
            &workspace_id,
            "/tmp/bowline-parity-current",
            "2026-07-13T10:00:00Z",
        )
        .expect("insert parity root");
    drop(store);
    assert_local_projection_parity(&current_db_path, "/tmp/bowline-parity-current");
}

#[test]
fn local_projection_adapter_uses_configured_workspace_instead_of_requested_path() {
    let db_path = unique_missing_db_path();
    std::fs::create_dir_all(db_path.parent().expect("database parent"))
        .expect("create database parent");
    let store = MetadataStore::open(&db_path).expect("open metadata");
    let path_workspace = WorkspaceId::new("ws_path_selected");
    let configured_workspace = WorkspaceId::new("ws_configured");
    store
        .insert_workspace(&path_workspace, "Path Selected", "2026-07-15T10:00:00Z")
        .expect("insert path workspace");
    store
        .insert_root(
            "root_path_selected",
            &path_workspace,
            "/tmp/bowline-path-selected",
            "2026-07-15T10:00:00Z",
        )
        .expect("insert path root");
    store
        .insert_workspace(&configured_workspace, "Configured", "2026-07-15T10:00:01Z")
        .expect("insert configured workspace");
    store
        .insert_root(
            "root_configured",
            &configured_workspace,
            "/tmp/bowline-configured",
            "2026-07-15T10:00:01Z",
        )
        .expect("insert configured root");
    drop(store);

    let collector = LocalStatusProjectionCollector::new_for_workspace(
        db_path,
        "/tmp/bowline-path-selected".to_string(),
        configured_workspace.clone(),
    )
    .expect("configured collector");
    let service = service_with_collectors(Duration::from_secs(60), vec![Box::new(collector)]);

    let projection = service.current().expect("current projection");
    assert_eq!(projection.status.workspace_id, configured_workspace);
    assert_eq!(
        projection.status.resolved_workspace_root.as_deref(),
        Some("/tmp/bowline-configured")
    );
}

fn assert_local_projection_parity(db_path: &std::path::Path, requested_path: &str) {
    let collector = LocalStatusProjectionCollector::new(
        Some(db_path.to_path_buf()),
        Some(requested_path.to_string()),
        true,
    )
    .expect("local collector");
    assert_eq!(collector.metrics().collector_calls, 0);
    let service = service_with_collectors(Duration::from_secs(60), vec![Box::new(collector)]);
    let projection = service.current().expect("current projection");
    let direct = compose_status(StatusOptions {
        db_path: Some(db_path.to_path_buf()),
        requested_path: Some(requested_path.to_string()),
        workspace_scope: true,
        generated_at: projection.generated_at.as_str().to_string(),
    })
    .expect("direct composition");

    assert_eq!(projection.status, direct);
    assert_eq!(projection.sequence, StatusSequence::INITIAL);
    assert_eq!(projection.instance_id.as_str(), "test-instance");
    assert_eq!(
        projection.sources[&StatusSource::Metadata].freshness,
        SourceFreshness::Current
    );
}

#[test]
fn heartbeat_deadline_blocks_without_recollecting_or_incrementing_sequence() {
    let calls = Arc::new(AtomicU64::new(0));
    let service = service_with_collectors(
        Duration::from_millis(40),
        vec![Box::new(FakeCollector::metadata(Arc::clone(&calls)))],
    );
    let updates = service.subscribe().expect("projection subscription");
    let heartbeats = service
        .subscribe_heartbeats()
        .expect("heartbeat subscription");
    assert_eq!(heartbeats.current.sequence, StatusSequence::INITIAL);

    let heartbeat = heartbeats
        .deadlines
        .recv_timeout(Duration::from_secs(1))
        .expect("heartbeat deadline");

    assert_eq!(heartbeat.sequence, StatusSequence::INITIAL);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        updates
            .updates
            .recv_timeout(Duration::from_millis(80))
            .is_err()
    );
    let metrics = service.metrics().expect("metrics");
    assert!(metrics.heartbeats_emitted >= 1);
    assert_eq!(metrics.semantic_changes, 0);
    assert_eq!(
        metrics
            .builds_by_reason
            .get(&ProjectionBuildReason::Initial),
        Some(&1)
    );
}

#[test]
fn unchanged_observation_refreshes_current_without_sequence_or_broadcast() {
    let sync_outcomes = VecDeque::from([
        FakeOutcome::Updated(StatusSourceState::Ready),
        FakeOutcome::Unchanged,
    ]);
    let service = service_with_collectors(
        Duration::from_secs(60),
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::new(AtomicU64::new(0)),
                sync_outcomes,
            )),
        ],
    );
    let subscription = service.subscribe().expect("subscription");
    let initial_observed_at = subscription.initial.sources[&StatusSource::SyncRuntime]
        .observed_at
        .clone();
    thread::sleep(Duration::from_millis(5));
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("unchanged source observation");

    wait_for_no_op(&service);
    let current = service.current().expect("refreshed current projection");
    assert_eq!(current.sequence, subscription.initial.sequence);
    assert_ne!(
        current.sources[&StatusSource::SyncRuntime].observed_at,
        initial_observed_at
    );
    assert!(
        subscription
            .updates
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
}

#[test]
fn input_flood_coalesces_dirty_state_without_starving_sources_heartbeat_or_shutdown() {
    let sources = [
        StatusSource::Metadata,
        StatusSource::SyncRuntime,
        StatusSource::DeviceTrust,
        StatusSource::UpdateAvailability,
        StatusSource::NotificationState,
        StatusSource::ServiceRuntime,
    ];
    let calls = sources
        .iter()
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect::<Vec<_>>();
    let mut collectors: Vec<Box<dyn StatusSourceCollector>> =
        vec![Box::new(FakeCollector::metadata(Arc::clone(&calls[0])))];
    for (source, source_calls) in sources.iter().zip(&calls).skip(1) {
        collectors.push(Box::new(FakeCollector::state(
            *source,
            StatusSourceFailurePolicy::RetainLastKnown,
            Arc::clone(source_calls),
            VecDeque::new(),
        )));
    }
    let service = service_with_collectors(Duration::from_millis(10), collectors);
    let heartbeats = service
        .subscribe_heartbeats()
        .expect("heartbeat subscription");
    let input = service.input();
    for index in 0..20_000 {
        input
            .send(StatusInputEvent::SourceChanged(
                sources[index % sources.len()],
            ))
            .expect("nonblocking dirty input");
        if index % 1_000 == 0 {
            input
                .send(StatusInputEvent::RefreshAll)
                .expect("priority refresh input");
        }
    }

    for source_calls in &calls {
        wait_for_collector_calls(source_calls, 2);
    }
    heartbeats
        .deadlines
        .recv_timeout(Duration::from_secs(1))
        .expect("non-starved heartbeat");
    let metrics = service.metrics().expect("flood metrics");
    assert_eq!(metrics.input_events_received, 20_020);
    assert!(metrics.input_events_coalesced > 10_000);
    assert!(metrics.input_wakes_coalesced > 10_000);
    assert!(metrics.max_pending_input_sources <= sources.len() as u64);
    assert!(metrics.heartbeats_emitted > 0);

    for _ in 0..20_000 {
        input
            .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
            .expect("shutdown churn input");
    }
    let (dropped_sender, dropped_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        drop(service);
        let _result = dropped_sender.send(());
    });
    dropped_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("shutdown must not wait behind historical churn");
}

#[test]
fn slow_projection_subscriber_retains_only_latest_projection() {
    let sync_calls = Arc::new(AtomicU64::new(0));
    let service = service_with_collectors(
        Duration::from_secs(60),
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::new(),
            )),
        ],
    );
    let subscription = service.subscribe().expect("slow projection subscriber");
    for call in 2..=26 {
        service
            .input()
            .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
            .expect("semantic input");
        wait_for_collector_calls(&sync_calls, call);
        wait_for_sequence(&service, call);
    }

    assert_eq!(subscription.updates.pending_count(), 1);
    let current = service.current().expect("latest current projection");
    let latest = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("latest projection");
    assert_eq!(latest.sequence, current.sequence);
    assert_eq!(latest.semantic_fingerprint, current.semantic_fingerprint);
    assert_eq!(subscription.updates.pending_count(), 0);
    assert!(subscription.updates.try_recv().is_err());
    let metrics = service.metrics().expect("projection delivery metrics");
    assert_eq!(metrics.projection_updates_delivered, 1);
    assert!(metrics.projection_updates_coalesced >= 24);
    assert_eq!(metrics.broadcasts, 25);
}

#[test]
fn slow_heartbeat_subscriber_retains_only_latest_deadline_independently() {
    let service = service_with_collectors(
        Duration::from_millis(20),
        vec![Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(
            0,
        ))))],
    );
    let heartbeats = service
        .subscribe_heartbeats()
        .expect("slow heartbeat subscriber");
    wait_for_heartbeats(&service, 4);

    assert_eq!(heartbeats.deadlines.pending_count(), 1);
    let current = service.current().expect("heartbeat current projection");
    let latest = heartbeats
        .deadlines
        .recv_timeout(Duration::from_secs(1))
        .expect("latest heartbeat");
    assert_eq!(latest.sequence, current.sequence);
    assert_eq!(latest.semantic_fingerprint, current.semantic_fingerprint);
    assert_eq!(heartbeats.deadlines.pending_count(), 0);
    let metrics = service.metrics().expect("heartbeat delivery metrics");
    assert_eq!(metrics.heartbeat_deliveries, 1);
    assert!(metrics.heartbeat_deliveries_coalesced >= 3);
    assert_eq!(metrics.projection_updates_coalesced, 0);
}

#[test]
fn stable_service_prunes_all_disconnected_subscriber_slots_and_projection_arcs() {
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_safety_refresh_interval(
        SafetyRefreshInterval::new(Duration::from_millis(10)).expect("safety refresh interval"),
    );
    let mut outcomes = VecDeque::from([FakeOutcome::Updated(StatusSourceState::Ready)]);
    outcomes.extend(std::iter::repeat_with(|| FakeOutcome::Unchanged).take(10_000));
    let service = service_with_config(
        config,
        vec![Box::new(FakeCollector::metadata_with_outcomes(
            Arc::new(AtomicU64::new(0)),
            outcomes,
        ))],
    );
    let initial = service.current().expect("initial projection");
    for _ in 0..10_000 {
        drop(service.subscribe().expect("projection subscription"));
        drop(
            service
                .subscribe_heartbeats()
                .expect("heartbeat subscription"),
        );
    }

    wait_for_disconnected_subscribers(&service, 10_000);
    let metrics = service.metrics().expect("disconnect metrics");
    assert_eq!(metrics.projection_subscribers_disconnected, 10_000);
    assert_eq!(metrics.heartbeat_subscribers_disconnected, 10_000);
    assert_eq!(metrics.projection_subscribers_active, 0);
    assert_eq!(metrics.heartbeat_subscribers_active, 0);
    let current = service.current().expect("stable current projection");
    assert_eq!(current.sequence, initial.sequence);
    assert_eq!(current.semantic_fingerprint, initial.semantic_fingerprint);
}

#[test]
fn service_shutdown_disconnects_latest_value_receivers_even_if_input_clone_lives() {
    let service = service_with_collectors(
        Duration::from_secs(60),
        vec![Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(
            0,
        ))))],
    );
    let projection_subscription = service.subscribe().expect("projection subscription");
    let heartbeat_subscription = service
        .subscribe_heartbeats()
        .expect("heartbeat subscription");
    let input = service.input();

    drop(service);

    assert!(input.send(StatusInputEvent::RefreshAll).is_err());
    assert!(matches!(
        projection_subscription
            .updates
            .recv_timeout(Duration::from_millis(10)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    assert!(matches!(
        heartbeat_subscription
            .deadlines
            .recv_timeout(Duration::from_millis(10)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn heartbeat_interval_rejects_busy_loop_configuration() {
    assert!(matches!(
        ProjectionServiceConfig::new(DaemonInstanceId::new("invalid"), Duration::ZERO),
        Err(StatusProjectionError::InvalidHeartbeatInterval)
    ));
    assert!(SafetyRefreshInterval::new(Duration::ZERO).is_err());
    assert!(StatusRetryPolicy::new(Duration::ZERO, Duration::from_secs(1)).is_err());
    assert!(StatusRetryPolicy::new(Duration::from_secs(2), Duration::from_secs(1)).is_err());
}

#[test]
fn retry_schedule_exponentially_backs_off_caps_and_resets_without_wall_clock_waits() {
    let policy = StatusRetryPolicy::new(Duration::from_millis(5), Duration::from_millis(20))
        .expect("retry policy");
    let now = Instant::now();
    let mut schedule = RetrySchedule::default();

    let first = schedule.record_failure(StatusSource::SyncRuntime, now, policy);
    let second = schedule.record_failure(StatusSource::SyncRuntime, now, policy);
    let third = schedule.record_failure(StatusSource::SyncRuntime, now, policy);
    let fourth = schedule.record_failure(StatusSource::SyncRuntime, now, policy);

    assert_eq!(first.delay, Duration::from_millis(5));
    assert!(!first.capped);
    assert_eq!(second.delay, Duration::from_millis(10));
    assert!(!second.capped);
    assert_eq!(third.delay, Duration::from_millis(20));
    assert!(third.capped);
    assert_eq!(fourth.delay, Duration::from_millis(20));
    assert!(fourth.capped);
    assert!(
        schedule
            .due_sources(now + Duration::from_millis(19))
            .is_empty()
    );
    assert_eq!(
        schedule.due_sources(now + Duration::from_millis(20)),
        vec![StatusSource::SyncRuntime]
    );
    assert!(schedule.record_success(StatusSource::SyncRuntime));
    let reset = schedule.record_failure(StatusSource::SyncRuntime, now, policy);
    assert_eq!(reset.delay, Duration::from_millis(5));
    assert_eq!(schedule.len(), 1);
}

#[test]
fn recoverable_failure_retries_automatically_with_capped_backoff_and_success_reset() {
    let sync_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_millis(5),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(20), Duration::from_millis(40))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                    FakeOutcome::Updated(StatusSourceState::Degraded),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("retry subscription");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("initial failure input");
    let failed = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("visible failed projection");
    assert_eq!(
        failed.sources[&StatusSource::SyncRuntime].freshness,
        SourceFreshness::Stale
    );
    let recovered = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("automatic recovery projection");
    assert_eq!(
        recovered.sources[&StatusSource::SyncRuntime].freshness,
        SourceFreshness::Current
    );
    assert_eq!(sync_calls.load(Ordering::SeqCst), 5);
    wait_for_retry_recovery(&service, StatusSource::SyncRuntime, 1);
    let metrics = service.metrics().expect("retry metrics");
    assert_eq!(
        metrics.collector_retries_scheduled[&StatusSource::SyncRuntime],
        3
    );
    assert_eq!(
        metrics.collector_retry_attempts[&StatusSource::SyncRuntime],
        3
    );
    assert_eq!(
        metrics.collector_retry_recoveries[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(
        metrics.collector_retry_delays_capped[&StatusSource::SyncRuntime],
        2
    );
    assert_eq!(
        metrics.collector_retry_delay_nanos[&StatusSource::SyncRuntime],
        Duration::from_millis(40).as_nanos()
    );
    assert_eq!(metrics.active_collector_retries, 0);
    assert_eq!(metrics.max_pending_collector_retries, 1);
    assert!(metrics.heartbeats_emitted > 0);
}

#[test]
fn source_change_accelerates_pending_retry_before_its_deadline() {
    let sync_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(500), Duration::from_secs(1))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                    FakeOutcome::Updated(StatusSourceState::Ready),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("acceleration subscription");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("failure input");
    subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("failed projection");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("accelerating input");
    let recovered = subscription
        .updates
        .recv_timeout(Duration::from_millis(200))
        .expect("accelerated recovery");
    assert_eq!(
        recovered.sources[&StatusSource::SyncRuntime].freshness,
        SourceFreshness::Current
    );
    wait_for_retry_recovery(&service, StatusSource::SyncRuntime, 1);
    let metrics = service.metrics().expect("acceleration metrics");
    assert_eq!(
        metrics.collector_retry_accelerations[&StatusSource::SyncRuntime],
        1
    );
    assert!(
        !metrics
            .collector_retry_attempts
            .contains_key(&StatusSource::SyncRuntime)
    );
    assert_eq!(
        metrics.collector_retry_recoveries[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(metrics.active_collector_retries, 0);
}

#[test]
fn unrecoverable_failure_waits_for_source_change_instead_of_retrying() {
    let sync_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(10), Duration::from_millis(20))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Failed(StatusCollectorFailureCode::LocalStatusUnrecoverable),
                    FakeOutcome::Updated(StatusSourceState::Ready),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("unrecoverable subscription");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("unrecoverable input");
    subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("unrecoverable projection");
    assert!(
        subscription
            .updates
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
    assert_eq!(sync_calls.load(Ordering::SeqCst), 2);
    let metrics = service.metrics().expect("metrics");
    assert_eq!(metrics.active_collector_retries, 0);
    assert_eq!(
        metrics.collector_retry_abandoned[&StatusSource::SyncRuntime],
        1
    );
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("recovery source change");
    subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("source-change recovery");
    assert_eq!(sync_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn safety_refresh_retry_and_heartbeat_deadlines_coexist_without_starvation() {
    let metadata_calls = Arc::new(AtomicU64::new(0));
    let sync_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_millis(5),
    )
    .expect("projection config")
    .with_safety_refresh_interval(
        SafetyRefreshInterval::new(Duration::from_millis(100)).expect("safety refresh interval"),
    )
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(10), Duration::from_millis(10))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata_with_outcomes(
                Arc::clone(&metadata_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Unchanged,
                    FakeOutcome::Unchanged,
                ]),
            )),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Unchanged,
                ]),
            )),
        ],
    );
    wait_for_collector_calls(&metadata_calls, 2);
    wait_for_collector_calls(&sync_calls, 3);
    wait_for_heartbeats(&service, 4);
    wait_for_retry_recovery(&service, StatusSource::SyncRuntime, 1);
    let metrics = service.metrics().expect("timer metrics");
    assert!(metrics.safety_refreshes >= 1);
    assert!(
        metrics
            .builds_by_reason
            .contains_key(&ProjectionBuildReason::SourceFailure)
    );
    assert_eq!(
        metrics.collector_retry_attempts[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(
        metrics.collector_retry_recoveries[&StatusSource::SyncRuntime],
        1
    );
    assert!(metrics.heartbeats_emitted >= 4);
}

#[test]
fn shutdown_interrupts_long_retry_deadline_without_an_extra_collection() {
    let sync_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_secs(60), Duration::from_secs(60))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("shutdown retry subscription");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("retry failure input");
    subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("failed projection");
    wait_for_retry_schedule(&service, StatusSource::SyncRuntime, 1);
    assert_eq!(
        service.metrics().expect("metrics").active_collector_retries,
        1
    );
    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        drop(service);
        let _result = done_sender.send(());
    });
    done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("shutdown must interrupt retry wait");
    assert_eq!(sync_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn supplemental_degradation_and_failure_reduce_into_visible_status() {
    let service = service_with_collectors(
        Duration::from_secs(60),
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::new(AtomicU64::new(0)),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Updated(StatusSourceState::Degraded),
                ]),
            )),
            Box::new(FakeCollector::state(
                StatusSource::ServiceRuntime,
                StatusSourceFailurePolicy::Discard,
                Arc::new(AtomicU64::new(0)),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Failed(StatusCollectorFailureCode::InjectedFailure),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("subscription");
    assert_eq!(
        subscription.initial.status.status_summary.availability,
        bowline_core::status::StatusAvailability::Ready
    );
    assert_eq!(
        subscription.initial.status.status.level,
        bowline_core::status::StatusLevel::Healthy
    );

    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("degrade sync runtime");
    let degraded = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("degraded projection");
    assert_eq!(
        degraded.status.status_summary.availability,
        bowline_core::status::StatusAvailability::Degraded
    );
    assert_eq!(
        degraded.status.status_summary.attention,
        bowline_core::status::StatusAttention::Recommended
    );
    assert_eq!(
        degraded.status.status.level,
        bowline_core::status::StatusLevel::Limited
    );
    assert_eq!(
        degraded
            .status
            .sync_queue
            .as_ref()
            .map(|queue| queue.queued),
        Some(2)
    );
    assert!(degraded.status.items.iter().any(|item| {
        item.kind == bowline_core::status::StatusItemKind::Network
            && item.summary == "Sync runtime is degraded."
    }));
    assert!(
        degraded
            .status
            .limits
            .iter()
            .any(|limit| limit.capability == "sync-runtime")
    );
    assert!(degraded.status.status_summary.facts.iter().any(|fact| {
        fact.parameters.get("source").map(String::as_str) == Some("sync-runtime")
            && fact.availability_impact
                == bowline_core::status::StatusFactAvailabilityImpact::Degraded
    }));

    service
        .input()
        .send(StatusInputEvent::SourceChanged(
            StatusSource::ServiceRuntime,
        ))
        .expect("fail service runtime");
    let failed = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("failed projection");
    assert_eq!(
        failed.status.status_summary.availability,
        bowline_core::status::StatusAvailability::Unavailable
    );
    assert_eq!(
        failed.status.status_summary.attention,
        bowline_core::status::StatusAttention::Required
    );
    assert_eq!(
        failed.status.status_summary.freshness,
        bowline_core::status::StatusSnapshotFreshness::Unknown
    );
    assert_eq!(
        failed.status.status.level,
        bowline_core::status::StatusLevel::Attention
    );
    assert!(failed.status.items.iter().any(|item| {
        item.kind == bowline_core::status::StatusItemKind::Watcher
            && item.summary == "Service runtime status collection failed."
    }));
    assert!(
        failed
            .status
            .limits
            .iter()
            .any(|limit| limit.capability == "service-runtime")
    );
}

#[test]
fn collector_contract_error_replays_valid_stage_and_recovers_without_external_retrigger() {
    let metadata_calls = Arc::new(AtomicU64::new(0));
    let sync_calls = Arc::new(AtomicU64::new(0));
    let update_calls = Arc::new(AtomicU64::new(0));
    let trust_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(250), Duration::from_millis(250))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::clone(&metadata_calls))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::WrongSource,
                    FakeOutcome::Updated(StatusSourceState::Degraded),
                ]),
            )),
            Box::new(FakeCollector::state(
                StatusSource::UpdateAvailability,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&update_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Updated(StatusSourceState::Degraded),
                ]),
            )),
            Box::new(FakeCollector::state(
                StatusSource::DeviceTrust,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&trust_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::Updated(StatusSourceState::Unavailable),
                ]),
            )),
        ],
    );
    let subscription = service.subscribe().expect("subscription");
    service
        .input()
        .send(StatusInputEvent::RefreshAll)
        .expect("refresh all");
    wait_for_collector_calls(&sync_calls, 2);

    let current = service.current().expect("current projection");
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 2);
    assert_eq!(update_calls.load(Ordering::SeqCst), 1);
    assert_eq!(trust_calls.load(Ordering::SeqCst), 1);
    assert_eq!(current.sequence, subscription.initial.sequence);
    assert_eq!(
        current.semantic_fingerprint,
        subscription.initial.semantic_fingerprint
    );
    assert!(
        subscription
            .updates
            .recv_timeout(Duration::from_millis(10))
            .is_err()
    );

    let recovered = subscription
        .updates
        .recv_timeout(Duration::from_secs(1))
        .expect("automatic recovered projection");
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 2);
    assert_eq!(sync_calls.load(Ordering::SeqCst), 3);
    assert_eq!(update_calls.load(Ordering::SeqCst), 2);
    assert_eq!(trust_calls.load(Ordering::SeqCst), 2);
    assert_eq!(recovered.sources[&StatusSource::Metadata].revision.get(), 2);
    let StatusSourceFacts::Metadata(metadata) = &recovered.source_facts[&StatusSource::Metadata]
    else {
        panic!("metadata facts must remain typed");
    };
    assert_eq!(metadata.revision.get(), 2);
    let StatusSourceFacts::UpdateAvailability(update) =
        &recovered.source_facts[&StatusSource::UpdateAvailability]
    else {
        panic!("update availability facts must remain typed");
    };
    assert_eq!(update.state, StatusSourceState::Degraded);
    assert_eq!(update.pending_count, 2);
    let StatusSourceFacts::DeviceTrust(trust) = &recovered.source_facts[&StatusSource::DeviceTrust]
    else {
        panic!("device trust facts must remain typed");
    };
    assert_eq!(trust.state, StatusSourceState::Unavailable);
    assert_eq!(trust.pending_count, 2);
    assert_eq!(
        recovered.status.status_summary.availability,
        bowline_core::status::StatusAvailability::Unavailable
    );
    assert_eq!(
        recovered.status.status_summary.attention,
        bowline_core::status::StatusAttention::Required
    );
    assert!(
        subscription
            .updates
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
    wait_for_contract_retry_recovery(&service, StatusSource::SyncRuntime, 1);
    let metrics = service.metrics().expect("contract retry metrics");
    assert_eq!(
        metrics.collector_contract_retries_scheduled[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(
        metrics.collector_contract_retry_attempts[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(
        metrics.collector_contract_retry_recoveries[&StatusSource::SyncRuntime],
        1
    );
    assert_eq!(metrics.active_collector_retries, 0);
}

#[test]
fn persistent_wrong_source_contract_uses_capped_backoff_without_spin_and_shuts_down() {
    let sync_calls = Arc::new(AtomicU64::new(0));
    let config = ProjectionServiceConfig::new(
        DaemonInstanceId::new("test-instance"),
        Duration::from_secs(60),
    )
    .expect("projection config")
    .with_retry_policy(
        StatusRetryPolicy::new(Duration::from_millis(20), Duration::from_millis(100))
            .expect("retry policy"),
    );
    let service = service_with_config(
        config,
        vec![
            Box::new(FakeCollector::metadata(Arc::new(AtomicU64::new(0)))),
            Box::new(FakeCollector::state(
                StatusSource::SyncRuntime,
                StatusSourceFailurePolicy::RetainLastKnown,
                Arc::clone(&sync_calls),
                VecDeque::from([
                    FakeOutcome::Updated(StatusSourceState::Ready),
                    FakeOutcome::WrongError,
                    FakeOutcome::WrongError,
                    FakeOutcome::WrongError,
                    FakeOutcome::WrongError,
                    FakeOutcome::WrongError,
                    FakeOutcome::WrongError,
                ]),
            )),
        ],
    );
    let subscription = service
        .subscribe()
        .expect("persistent contract subscription");
    service
        .input()
        .send(StatusInputEvent::SourceChanged(StatusSource::SyncRuntime))
        .expect("malformed source input");
    wait_for_contract_retry_schedule(&service, StatusSource::SyncRuntime, 5);

    let calls_after_fourth_retry = sync_calls.load(Ordering::SeqCst);
    assert_eq!(calls_after_fourth_retry, 6);
    thread::sleep(Duration::from_millis(10));
    assert_eq!(sync_calls.load(Ordering::SeqCst), calls_after_fourth_retry);
    let current = service.current().expect("unchanged projection");
    assert_eq!(current.sequence, subscription.initial.sequence);
    assert_eq!(
        current.semantic_fingerprint,
        subscription.initial.semantic_fingerprint
    );
    assert!(subscription.updates.try_recv().is_err());
    let metrics = service.metrics().expect("persistent contract metrics");
    assert_eq!(
        metrics.collector_contract_retries_scheduled[&StatusSource::SyncRuntime],
        5
    );
    assert_eq!(
        metrics.collector_contract_retry_attempts[&StatusSource::SyncRuntime],
        4
    );
    assert_eq!(
        metrics.collector_contract_retry_delays_capped[&StatusSource::SyncRuntime],
        2
    );
    assert_eq!(
        metrics.collector_retry_delay_nanos[&StatusSource::SyncRuntime],
        Duration::from_millis(100).as_nanos()
    );
    assert_eq!(metrics.active_collector_retries, 1);
    assert_eq!(metrics.max_pending_collector_retries, 1);

    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        drop(service);
        let _result = done_sender.send(());
    });
    done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("shutdown must interrupt contract retry wait");
    assert_eq!(sync_calls.load(Ordering::SeqCst), calls_after_fourth_retry);
}

fn service_with_collectors(
    heartbeat_interval: Duration,
    collectors: Vec<Box<dyn StatusSourceCollector>>,
) -> StatusProjectionService {
    let config =
        ProjectionServiceConfig::new(DaemonInstanceId::new("test-instance"), heartbeat_interval)
            .expect("projection config");
    service_with_config(config, collectors)
}

fn service_with_config(
    config: ProjectionServiceConfig,
    collectors: Vec<Box<dyn StatusSourceCollector>>,
) -> StatusProjectionService {
    StatusProjectionService::start(config, collectors).expect("projection service")
}

fn wait_for_no_op(service: &StatusProjectionService) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service
            .metrics()
            .is_ok_and(|metrics| metrics.no_op_refreshes > 0)
        {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("projection did not record a no-op refresh");
}

fn wait_for_collector_calls(calls: &AtomicU64, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if calls.load(Ordering::SeqCst) >= expected {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("collector did not reach {expected} calls");
}

fn wait_for_sequence(service: &StatusProjectionService, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service
            .current()
            .is_ok_and(|projection| projection.sequence.get() >= expected)
        {
            return;
        }
        thread::yield_now();
    }
    panic!("projection did not reach sequence {expected}");
}

fn wait_for_heartbeats(service: &StatusProjectionService, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service
            .metrics()
            .is_ok_and(|metrics| metrics.heartbeats_emitted >= expected)
        {
            return;
        }
        thread::yield_now();
    }
    panic!("projection did not emit {expected} heartbeats");
}

fn wait_for_retry_recovery(service: &StatusProjectionService, source: StatusSource, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service.metrics().is_ok_and(|metrics| {
            metrics
                .collector_retry_recoveries
                .get(&source)
                .is_some_and(|recoveries| *recoveries >= expected)
                && metrics.active_collector_retries == 0
        }) {
            return;
        }
        thread::yield_now();
    }
    panic!("collector retry did not recover {source:?}");
}

fn wait_for_retry_schedule(service: &StatusProjectionService, source: StatusSource, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service.metrics().is_ok_and(|metrics| {
            metrics
                .collector_retries_scheduled
                .get(&source)
                .is_some_and(|scheduled| *scheduled >= expected)
                && metrics.active_collector_retries > 0
        }) {
            return;
        }
        thread::yield_now();
    }
    panic!("collector retry was not scheduled for {source:?}");
}

fn wait_for_contract_retry_schedule(
    service: &StatusProjectionService,
    source: StatusSource,
    expected: u64,
) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service.metrics().is_ok_and(|metrics| {
            metrics
                .collector_contract_retries_scheduled
                .get(&source)
                .is_some_and(|scheduled| *scheduled >= expected)
        }) {
            return;
        }
        thread::yield_now();
    }
    panic!("collector contract retry was not scheduled for {source:?}");
}

fn wait_for_contract_retry_recovery(
    service: &StatusProjectionService,
    source: StatusSource,
    expected: u64,
) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service.metrics().is_ok_and(|metrics| {
            metrics
                .collector_contract_retry_recoveries
                .get(&source)
                .is_some_and(|recoveries| *recoveries >= expected)
                && metrics.active_collector_retries == 0
        }) {
            return;
        }
        thread::yield_now();
    }
    panic!("collector contract retry did not recover {source:?}");
}

fn wait_for_disconnected_subscribers(service: &StatusProjectionService, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if service.metrics().is_ok_and(|metrics| {
            metrics.projection_subscribers_disconnected == expected
                && metrics.heartbeat_subscribers_disconnected == expected
                && metrics.projection_subscribers_active == 0
                && metrics.heartbeat_subscribers_active == 0
        }) {
            return;
        }
        thread::yield_now();
    }
    panic!("disconnected subscribers were not observed");
}

fn source_revisions(
    revision: u64,
    observed_at: &str,
    freshness: SourceFreshness,
) -> BTreeMap<StatusSource, SourceRevision> {
    BTreeMap::from([(
        StatusSource::Metadata,
        SourceRevision {
            source: StatusSource::Metadata,
            revision: StatusSourceRevision::new(revision),
            observed_at: StatusTimestamp::new(observed_at),
            freshness,
        },
    )])
}

fn metadata_facts(
    output: StatusCommandOutput,
    revision: u64,
    observed_at: &str,
) -> BTreeMap<StatusSource, StatusSourceFacts> {
    BTreeMap::from([(
        StatusSource::Metadata,
        StatusSourceFacts::Metadata(Box::new(LocalStatusFacts {
            revision: LocalStatusRevision::new(revision),
            observed_at: observed_at.to_string(),
            output,
        })),
    )])
}

fn missing_status(generated_at: &str) -> StatusCommandOutput {
    compose_status(StatusOptions {
        db_path: Some(unique_missing_db_path()),
        requested_path: Some("/tmp/bowline-status-projection".to_string()),
        workspace_scope: true,
        generated_at: generated_at.to_string(),
    })
    .expect("missing metadata status")
}

fn unique_missing_db_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "bowline-status-projection-{}-{nanos}/metadata.sqlite3",
        std::process::id()
    ))
}

type StatusTimestampMutation = fn(&mut StatusCommandOutput);

fn semantic_timestamp_mutations() -> [(&'static str, StatusTimestampMutation); 7] {
    [
        ("setupReadiness.updatedAt", change_setup_updated_at),
        ("eventWatermarks.lastScanAt", change_last_scan_at),
        ("statusSummary.facts[].staleAfter", change_fact_stale_after),
        (
            "statusSummary.facts[].parameters.updatedAt",
            change_parameter_updated_at,
        ),
        (
            "statusSummary.facts[].parameters.generatedAt",
            change_parameter_generated_at,
        ),
        (
            "statusSummary.facts[].parameters.publishedAt",
            change_parameter_published_at,
        ),
        (
            "statusSummary.facts[].parameters.observedAt",
            change_parameter_observed_at,
        ),
    ]
}

fn timestamp_rich_status(generated_at: &str) -> StatusCommandOutput {
    let mut output = healthy_status(generated_at);
    output.setup_readiness = Some(bowline_core::status::ProjectSetupReadiness {
        state: bowline_core::status::ProjectSetupReadinessState::Runnable,
        reason: "timestamp policy fixture".to_string(),
        remedy: None,
        identity_hash: Some("setup-hash".to_string()),
        latest_receipt_id: Some("receipt-timestamp".to_string()),
        latest_receipt_state: None,
        updated_at: Some("2026-07-13T09:00:00Z".to_string()),
    });
    output.event_watermarks.last_scan_at = Some("2026-07-13T09:05:00Z".to_string());
    let mut fact = bowline_core::status::StatusFact::new(
        "timestamp-policy-fact",
        "status.aggregate_input",
        "status-reducer",
        bowline_core::status::StatusFactScope::Workspace,
        "2026-07-13T09:10:00Z",
        "timestamp-policy-fact",
    );
    fact.stale_after = Some("2026-07-13T09:15:00Z".to_string());
    for key in ["updatedAt", "generatedAt", "publishedAt", "observedAt"] {
        fact.parameters
            .insert(key.to_string(), "2026-07-13T09:20:00Z".to_string());
    }
    output.status_summary.facts.push(fact);
    output
}

fn change_setup_updated_at(output: &mut StatusCommandOutput) {
    output
        .setup_readiness
        .as_mut()
        .expect("timestamp setup readiness")
        .updated_at = Some("2026-07-13T10:00:00Z".to_string());
}

fn change_last_scan_at(output: &mut StatusCommandOutput) {
    output.event_watermarks.last_scan_at = Some("2026-07-13T10:05:00Z".to_string());
}

fn change_fact_stale_after(output: &mut StatusCommandOutput) {
    output
        .status_summary
        .facts
        .first_mut()
        .expect("timestamp status fact")
        .stale_after = Some("2026-07-13T10:15:00Z".to_string());
}

fn change_parameter_updated_at(output: &mut StatusCommandOutput) {
    change_timestamp_parameter(output, "updatedAt");
}

fn change_parameter_generated_at(output: &mut StatusCommandOutput) {
    change_timestamp_parameter(output, "generatedAt");
}

fn change_parameter_published_at(output: &mut StatusCommandOutput) {
    change_timestamp_parameter(output, "publishedAt");
}

fn change_parameter_observed_at(output: &mut StatusCommandOutput) {
    change_timestamp_parameter(output, "observedAt");
}

fn change_timestamp_parameter(output: &mut StatusCommandOutput, key: &str) {
    output
        .status_summary
        .facts
        .first_mut()
        .expect("timestamp status fact")
        .parameters
        .insert(key.to_string(), "2026-07-13T10:20:00Z".to_string());
}

struct FakeCollector {
    source: StatusSource,
    failure_policy: StatusSourceFailurePolicy,
    calls: Arc<AtomicU64>,
    outcomes: VecDeque<FakeOutcome>,
    staged: Option<Result<StatusSourceCollection, StatusCollectorFailure>>,
}

enum FakeOutcome {
    Updated(StatusSourceState),
    Metadata(Box<StatusCommandOutput>),
    Unchanged,
    Failed(StatusCollectorFailureCode),
    WrongSource,
    WrongError,
}

impl FakeCollector {
    fn metadata(calls: Arc<AtomicU64>) -> Self {
        Self::metadata_with_outcomes(calls, VecDeque::new())
    }

    fn metadata_with_outcomes(calls: Arc<AtomicU64>, outcomes: VecDeque<FakeOutcome>) -> Self {
        Self {
            source: StatusSource::Metadata,
            failure_policy: StatusSourceFailurePolicy::RetainLastKnown,
            calls,
            outcomes,
            staged: None,
        }
    }

    fn state(
        source: StatusSource,
        failure_policy: StatusSourceFailurePolicy,
        calls: Arc<AtomicU64>,
        outcomes: VecDeque<FakeOutcome>,
    ) -> Self {
        Self {
            source,
            failure_policy,
            calls,
            outcomes,
            staged: None,
        }
    }

    fn state_facts(&self, state: StatusSourceState, pending_count: u64) -> StatusSourceFacts {
        let facts = StatusSourceStateFacts {
            state,
            pending_count,
        };
        match self.source {
            StatusSource::SyncRuntime => StatusSourceFacts::SyncRuntime(facts),
            StatusSource::DeviceTrust => StatusSourceFacts::DeviceTrust(facts),
            StatusSource::UpdateAvailability => StatusSourceFacts::UpdateAvailability(facts),
            StatusSource::NotificationState => StatusSourceFacts::NotificationState(facts),
            StatusSource::ServiceRuntime => StatusSourceFacts::ServiceRuntime(facts),
            StatusSource::Metadata | StatusSource::Convergence => {
                unreachable!("durable collectors use typed facts")
            }
        }
    }
}

impl StatusSourceCollector for FakeCollector {
    fn source(&self) -> StatusSource {
        self.source
    }

    fn failure_policy(&self) -> StatusSourceFailurePolicy {
        self.failure_policy
    }

    fn mark_dirty(&mut self) {}

    fn stage(
        &mut self,
        observed_at: StatusTimestamp,
        _now: Instant,
    ) -> Result<StatusSourceCollection, StatusCollectorFailure> {
        if let Some(staged) = self.staged.as_ref() {
            return staged.clone();
        }
        let revision = self.calls.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let outcome = self
            .outcomes
            .pop_front()
            .unwrap_or(FakeOutcome::Updated(StatusSourceState::Ready));
        let staged = match outcome {
            FakeOutcome::Metadata(output) => Ok(StatusSourceCollection::Updated {
                revision: StatusSourceRevision::new(revision),
                observed_at: observed_at.clone(),
                facts: StatusSourceFacts::Metadata(Box::new(LocalStatusFacts {
                    revision: LocalStatusRevision::new(revision),
                    observed_at: observed_at.as_str().to_string(),
                    output: *output,
                })),
            }),
            FakeOutcome::Unchanged => Ok(StatusSourceCollection::Unchanged),
            FakeOutcome::Failed(code) => Err(StatusCollectorFailure {
                source: self.source,
                code,
            }),
            FakeOutcome::WrongSource => Ok(StatusSourceCollection::Updated {
                revision: StatusSourceRevision::new(revision),
                observed_at,
                facts: StatusSourceFacts::DeviceTrust(StatusSourceStateFacts {
                    state: StatusSourceState::Ready,
                    pending_count: revision,
                }),
            }),
            FakeOutcome::WrongError => Err(StatusCollectorFailure {
                source: StatusSource::DeviceTrust,
                code: StatusCollectorFailureCode::InjectedFailure,
            }),
            FakeOutcome::Updated(state) => {
                let facts = if self.source == StatusSource::Metadata {
                    let mut output = healthy_status(observed_at.as_str());
                    output.items.push(metadata_revision_item(revision));
                    StatusSourceFacts::Metadata(Box::new(LocalStatusFacts {
                        revision: LocalStatusRevision::new(revision),
                        observed_at: observed_at.as_str().to_string(),
                        output,
                    }))
                } else {
                    self.state_facts(state, revision)
                };
                Ok(StatusSourceCollection::Updated {
                    revision: StatusSourceRevision::new(revision),
                    observed_at,
                    facts,
                })
            }
        };
        self.staged = Some(staged.clone());
        staged
    }

    fn commit_staged(&mut self) {
        self.staged = None;
    }

    fn abort_staged(&mut self) {}

    fn reject_staged(&mut self) {
        self.staged = None;
    }
}

fn healthy_status(generated_at: &str) -> StatusCommandOutput {
    let mut output = missing_status(generated_at);
    output.status = bowline_core::status::WorkspaceStatus::healthy();
    output.status_summary = bowline_core::status::reduce_status_facts(
        Vec::new(),
        output.status_summary.snapshot_version,
        generated_at,
    );
    output.items.clear();
    output.limits.clear();
    output.next_actions.clear();
    output
}

fn metadata_revision_item(revision: u64) -> bowline_core::status::StatusItem {
    bowline_core::status::StatusItem {
        kind: bowline_core::status::StatusItemKind::Metadata,
        summary: format!("Metadata revision {revision}."),
        subject: None,
        path: None,
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: None,
        device_id: None,
        lease_id: None,
        project_id: None,
        snapshot_id: None,
        policy_version: None,
        env_record_id: None,
    }
}
