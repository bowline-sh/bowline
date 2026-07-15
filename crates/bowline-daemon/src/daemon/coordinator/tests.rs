use std::{
    collections::BTreeSet,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use crossbeam_channel::Receiver;

use super::{
    clock::FakeCoordinatorClock,
    lanes::{
        CONTROL_PLANE_WORKERS, MUTATION_WORKERS, NOTIFICATION_WORKERS, QUERY_WORKERS, SYNC_WORKERS,
    },
    *,
};

#[derive(Debug, Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    started: usize,
    released: bool,
}

impl Gate {
    fn block(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.started += 1;
        self.changed.notify_all();
        while !state.released {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_for_started(&self, expected: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let timeout = Duration::from_secs(2);
        while state.started < expected {
            let waited = self
                .changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
            assert!(!waited.1.timed_out(), "workers did not reach the gate");
        }
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.released = true;
        self.changed.notify_all();
    }
}

#[test]
fn default_lane_capacities_are_the_plan_contract() {
    let config = CoordinatorExecutorConfig::default();
    assert_eq!(
        config.lane(CoordinatorLane::Mutation).workers,
        MUTATION_WORKERS
    );
    assert_eq!(config.lane(CoordinatorLane::Query).workers, QUERY_WORKERS);
    assert_eq!(config.lane(CoordinatorLane::Sync).workers, SYNC_WORKERS);
    assert_eq!(
        config.lane(CoordinatorLane::ControlPlane).workers,
        CONTROL_PLANE_WORKERS
    );
    assert_eq!(
        config.lane(CoordinatorLane::Notification).workers,
        NOTIFICATION_WORKERS
    );
    assert_eq!((MUTATION_WORKERS, QUERY_WORKERS, SYNC_WORKERS), (4, 8, 2));
    assert_eq!((CONTROL_PLANE_WORKERS, NOTIFICATION_WORKERS), (4, 1));
}

#[test]
fn fake_clock_deadline_has_zero_churn_and_an_event_wakes_early() {
    let clock = FakeCoordinatorClock::default();
    let metrics = Arc::new(CoordinatorMetrics::default());
    let mut state = CoordinatorState::new(clock.clone(), Arc::clone(&metrics));
    let deadline = CoordinatorDeadline {
        due: CoordinatorInstant::ZERO.add(Duration::from_secs(10)),
        kind: CoordinatorDeadlineKind::DurableRetry(CoordinatorJobId::new("retry-1")),
    };
    assert!(
        state
            .handle_event(CoordinatorEvent::ScheduleDeadline(deadline.clone()))
            .is_empty()
    );

    for _ in 0..1_000 {
        assert!(state.process_due_deadlines().is_empty());
    }
    assert_eq!(metrics.snapshot().deadlines_fired, 0);
    assert_eq!(metrics.snapshot().idle_wakeups, 0);
    assert_eq!(state.next_wait(), Some(Duration::from_secs(10)));

    clock.advance(Duration::from_secs(1));
    assert_eq!(
        state.handle_event(CoordinatorEvent::DurableWorkAvailable),
        vec![CoordinatorAction::DiscoverDurableWork]
    );
    assert_eq!(state.next_wait(), Some(Duration::from_secs(9)));
    assert_eq!(metrics.snapshot().deadlines_fired, 0);

    clock.advance(Duration::from_secs(9));
    assert_eq!(
        state.process_due_deadlines(),
        vec![CoordinatorAction::DeadlineDue(deadline.kind)]
    );
    assert_eq!(metrics.snapshot().deadlines_fired, 1);
    assert_eq!(metrics.snapshot().idle_wakeups, 0);
}

#[test]
fn fake_clock_driver_blocks_without_churn_and_is_woken_by_an_earlier_event() {
    let clock = FakeCoordinatorClock::default();
    let metrics = Arc::new(CoordinatorMetrics::default());
    let mut state = CoordinatorState::new(clock, Arc::clone(&metrics));
    state.handle_event(CoordinatorEvent::ScheduleDeadline(CoordinatorDeadline {
        due: CoordinatorInstant::ZERO.add(Duration::from_secs(30)),
        kind: CoordinatorDeadlineKind::DurableRetry(CoordinatorJobId::new("later")),
    }));
    let (handle, receiver) = coordinator_channel(1);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        let mut driver = CoordinatorDriver::new(state, receiver);
        worker_barrier.wait();
        driver.run_turn()
    });
    barrier.wait();
    let started = Instant::now();
    handle
        .try_send(CoordinatorEvent::DurableWorkAvailable)
        .expect("event wakes the blocking driver");
    assert_eq!(
        worker.join().expect("driver joins").expect("turn succeeds"),
        vec![CoordinatorAction::DiscoverDurableWork]
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.deadlines_fired, 0);
    assert_eq!(snapshot.idle_wakeups, 0);
}

#[test]
fn dirty_paths_coalesce_and_capacity_overflow_becomes_full_scan() {
    let metrics = Arc::new(CoordinatorMetrics::default());
    let mut state = CoordinatorState::with_dirty_capacity(
        FakeCoordinatorClock::default(),
        Arc::clone(&metrics),
        2,
    );
    let scope = DirtyScopeKey::new("workspace-a");
    assert_eq!(
        state.handle_event(CoordinatorEvent::FilesystemDirty(FilesystemDirty::one(
            scope.clone(),
            DirtyPath::new("project/a.rs"),
        ))),
        vec![CoordinatorAction::DirtyReady(scope.clone())]
    );
    assert!(
        state
            .handle_event(CoordinatorEvent::FilesystemDirty(FilesystemDirty::one(
                scope.clone(),
                DirtyPath::new("project/a.rs"),
            )))
            .is_empty()
    );
    assert!(
        state
            .handle_event(CoordinatorEvent::FilesystemDirty(FilesystemDirty::one(
                scope.clone(),
                DirtyPath::new("project/b.rs"),
            )))
            .is_empty()
    );
    assert!(
        state
            .handle_event(CoordinatorEvent::FilesystemDirty(FilesystemDirty::one(
                scope.clone(),
                DirtyPath::new("project/c.rs"),
            )))
            .is_empty()
    );
    assert_eq!(
        state.take_dirty(&scope),
        Some(PendingDirtyBatch::FullScan(
            FullScanRecoveryReason::WatcherOverflow
        ))
    );
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.filesystem_events, 4);
    assert!(snapshot.filesystem_events_coalesced >= 1);
    assert_eq!(snapshot.filesystem_overflows, 1);
    assert!(snapshot.filesystem_wakes_coalesced >= 3);
    assert_eq!(snapshot.pending_dirty_scopes, 0);
}

#[test]
fn two_workspaces_progress_while_the_same_resource_never_overlaps() {
    let (executor, receiver) = test_executor([1, 1, 2, 1, 1], 16);
    let workspace_a = CoordinatorResourceKey::new("workspace-a");
    let workspace_b = CoordinatorResourceKey::new("workspace-b");
    let first_a_gate = Arc::new(Gate::default());
    let second_a_started = Arc::new(Mutex::new(false));
    let b_started = Arc::new(Mutex::new(false));

    let job_gate = Arc::clone(&first_a_gate);
    executor
        .submit(job(
            "a-1",
            CoordinatorLane::Sync,
            Some(workspace_a.clone()),
            move || {
                job_gate.block();
                Ok(())
            },
        ))
        .expect("first workspace A job queues");
    first_a_gate.wait_for_started(1);

    let second_started = Arc::clone(&second_a_started);
    executor
        .submit(job(
            "a-2",
            CoordinatorLane::Sync,
            Some(workspace_a),
            move || {
                *second_started.lock().expect("second A state") = true;
                Ok(())
            },
        ))
        .expect("second workspace A job queues");
    let started_b = Arc::clone(&b_started);
    executor
        .submit(job(
            "b-1",
            CoordinatorLane::Sync,
            Some(workspace_b),
            move || {
                *started_b.lock().expect("B state") = true;
                Ok(())
            },
        ))
        .expect("workspace B job queues");

    let first_completion = recv_completion(&receiver);
    assert_eq!(first_completion.job_id.as_str(), "b-1");
    assert!(*b_started.lock().expect("B state"));
    assert!(!*second_a_started.lock().expect("second A state"));

    first_a_gate.release();
    let remaining = [recv_completion(&receiver), recv_completion(&receiver)];
    assert!(
        remaining
            .iter()
            .any(|completion| completion.job_id.as_str() == "a-1")
    );
    assert!(
        remaining
            .iter()
            .any(|completion| completion.job_id.as_str() == "a-2")
    );
    assert!(*second_a_started.lock().expect("second A state"));
    executor.shutdown_and_join().expect("workers join");
    let metrics = executor.metrics();
    assert_eq!(metrics.lane(CoordinatorLane::Sync).max_active, 2);
    assert_eq!(metrics.joined_workers, metrics.configured_workers);
}

#[test]
fn lane_queue_round_robins_resources_instead_of_draining_one_backlog() {
    let (executor, receiver) = test_executor([1, 1, 1, 1, 1], 16);
    let gate = Arc::new(Gate::default());
    let starts = Arc::new(Mutex::new(Vec::<String>::new()));
    let first_gate = Arc::clone(&gate);
    let first_starts = Arc::clone(&starts);
    executor
        .submit(job(
            "a-1",
            CoordinatorLane::Sync,
            Some(CoordinatorResourceKey::new("workspace-a")),
            move || {
                first_starts
                    .lock()
                    .expect("start order")
                    .push("a-1".to_string());
                first_gate.block();
                Ok(())
            },
        ))
        .expect("first A job queues");
    gate.wait_for_started(1);

    for (id, resource) in [
        ("a-2", "workspace-a"),
        ("a-3", "workspace-a"),
        ("b-1", "workspace-b"),
        ("c-1", "workspace-c"),
    ] {
        let job_starts = Arc::clone(&starts);
        let started_id = id.to_string();
        executor
            .submit(job(
                id,
                CoordinatorLane::Sync,
                Some(CoordinatorResourceKey::new(resource)),
                move || {
                    job_starts.lock().expect("start order").push(started_id);
                    Ok(())
                },
            ))
            .expect("fairness job queues");
    }
    gate.release();
    for _ in 0..5 {
        let _completion = recv_completion(&receiver);
    }
    executor.shutdown_and_join().expect("workers join");
    assert_eq!(
        *starts.lock().expect("start order"),
        ["a-1", "a-2", "b-1", "c-1", "a-3"]
    );
}

#[test]
fn resource_exclusion_is_global_across_lane_types() {
    let (executor, receiver) = test_executor([1, 1, 1, 1, 1], 8);
    let resource = CoordinatorResourceKey::new("workspace-shared");
    let sync_gate = Arc::new(Gate::default());
    let worker_gate = Arc::clone(&sync_gate);
    let control_started = Arc::new(Mutex::new(false));
    executor
        .submit(job(
            "sync",
            CoordinatorLane::Sync,
            Some(resource.clone()),
            move || {
                worker_gate.block();
                Ok(())
            },
        ))
        .expect("sync queues");
    sync_gate.wait_for_started(1);
    let started = Arc::clone(&control_started);
    executor
        .submit(job(
            "control",
            CoordinatorLane::ControlPlane,
            Some(resource),
            move || {
                *started.lock().expect("control started") = true;
                Ok(())
            },
        ))
        .expect("control queues");
    std::thread::yield_now();
    assert!(!*control_started.lock().expect("control started"));

    sync_gate.release();
    let completions = [recv_completion(&receiver), recv_completion(&receiver)];
    assert!(
        completions
            .iter()
            .any(|completion| completion.job_id.as_str() == "control")
    );
    assert!(*control_started.lock().expect("control started"));
    executor.shutdown_and_join().expect("workers join");
}

#[test]
fn long_sync_cannot_starve_control_notification_query_or_mutation() {
    let (executor, receiver) = test_executor([1, 1, 1, 1, 1], 16);
    let sync_gate = Arc::new(Gate::default());
    let worker_gate = Arc::clone(&sync_gate);
    executor
        .submit(job(
            "sync-long",
            CoordinatorLane::Sync,
            Some(CoordinatorResourceKey::new("workspace-sync")),
            move || {
                worker_gate.block();
                Ok(())
            },
        ))
        .expect("long sync queues");
    sync_gate.wait_for_started(1);

    for (id, lane) in [
        ("mutation", CoordinatorLane::Mutation),
        ("query", CoordinatorLane::Query),
        ("control", CoordinatorLane::ControlPlane),
        ("notification", CoordinatorLane::Notification),
    ] {
        executor
            .submit(job(id, lane, None, || Ok(())))
            .expect("side lane queues");
    }
    let side_completions = (0..4)
        .map(|_| recv_completion(&receiver).job_id.0)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        side_completions,
        BTreeSet::from([
            "control".to_string(),
            "mutation".to_string(),
            "notification".to_string(),
            "query".to_string(),
        ])
    );
    sync_gate.release();
    assert_eq!(recv_completion(&receiver).job_id.as_str(), "sync-long");
    executor.shutdown_and_join().expect("workers join");
}

#[test]
fn dispatch_failure_returns_the_unstarted_job_for_durable_requeue() {
    let (executor, _receiver) = test_executor([1, 1, 1, 1, 1], 1);
    let gate = Arc::new(Gate::default());
    let worker_gate = Arc::clone(&gate);
    executor
        .submit(job("active", CoordinatorLane::Sync, None, move || {
            worker_gate.block();
            Ok(())
        }))
        .expect("active sync queues");
    gate.wait_for_started(1);
    executor
        .submit(job("queued", CoordinatorLane::Sync, None, || Ok(())))
        .expect("queue slot fills");
    let recovered = Arc::new(Mutex::new(None::<String>));
    let recovered_work = Arc::clone(&recovered);
    let error = executor
        .submit(CoordinatorJob::recoverable(
            CoordinatorJobId::new("rejected"),
            CoordinatorLane::Sync,
            None,
            "durable-claim".to_string(),
            |_| Ok(()),
            move |work, kind| {
                assert_eq!(kind, CoordinatorSubmitErrorKind::LaneQueueFull);
                *recovered_work.lock().expect("recovered work") = Some(work);
            },
        ))
        .expect_err("full queue rejects");
    assert_eq!(error.kind, CoordinatorSubmitErrorKind::LaneQueueFull);
    assert_eq!(error.job.id.as_str(), "rejected");
    error.recover().expect("durable work recovery runs");
    assert_eq!(
        recovered.lock().expect("recovered work").as_deref(),
        Some("durable-claim")
    );
    assert_eq!(
        executor
            .metrics()
            .lane(CoordinatorLane::Sync)
            .dispatch_rejected,
        1
    );
    gate.release();
    executor.shutdown_and_join().expect("workers join");
}

#[test]
fn panicked_worker_job_reports_typed_completion_and_pool_continues() {
    let (executor, receiver) = test_executor([1, 1, 1, 1, 1], 8);
    executor
        .submit(job("panic", CoordinatorLane::ControlPlane, None, || {
            panic!("synthetic coordinator panic")
        }))
        .expect("panic job queues");
    executor
        .submit(job("after", CoordinatorLane::ControlPlane, None, || Ok(())))
        .expect("post-panic job queues");
    let first = recv_completion(&receiver);
    let second = recv_completion(&receiver);
    assert_eq!(first.job_id.as_str(), "panic");
    assert_eq!(first.outcome, CoordinatorWorkerOutcome::Panicked);
    assert_eq!(second.job_id.as_str(), "after");
    assert_eq!(second.outcome, CoordinatorWorkerOutcome::Succeeded);
    executor.shutdown_and_join().expect("workers join");
    let metrics = executor.metrics().lane(CoordinatorLane::ControlPlane);
    assert_eq!(metrics.panicked, 1);
    assert_eq!(metrics.completed, 2);
}

#[test]
fn saturated_coordinator_channel_invokes_exact_completion_recovery() {
    let (handle, receiver) = coordinator_channel(1);
    handle
        .try_send(CoordinatorEvent::DurableWorkAvailable)
        .expect("coordinator channel is saturated");
    let metrics = Arc::new(CoordinatorMetrics::default());
    let executor = CoordinatorExecutor::new(
        CoordinatorExecutorConfig::testing([1, 1, 1, 1, 1], 4),
        handle,
        Arc::clone(&metrics),
    )
    .expect("coordinator executor starts");
    let (recovery_tx, recovery_rx) = std::sync::mpsc::channel();
    executor
        .submit(
            job(
                "durable-saturated",
                CoordinatorLane::Sync,
                Some(CoordinatorResourceKey::new("workspace-saturated")),
                || Ok(()),
            )
            .on_completion_delivery_failure(move |completion| {
                recovery_tx
                    .send(completion)
                    .expect("recovery observation sends");
            }),
        )
        .expect("durable job queues");

    let recovered = recovery_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("saturated delivery invokes recovery");
    assert_eq!(recovered.job_id.as_str(), "durable-saturated");
    assert_eq!(recovered.lane, CoordinatorLane::Sync);
    assert_eq!(recovered.outcome, CoordinatorWorkerOutcome::Succeeded);
    assert!(matches!(
        receiver.try_recv(),
        Ok(CoordinatorEvent::DurableWorkAvailable)
    ));
    executor.shutdown_and_join().expect("workers join");
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.completion_delivery_recoveries, 1);
    assert_eq!(snapshot.completion_events_dropped, 0);
}

#[test]
fn shutdown_requeues_unstarted_durable_job_instead_of_draining_queue() {
    let (executor, _receiver) = test_executor([1, 1, 1, 1, 1], 4);
    let executor = Arc::new(executor);
    let gate = Arc::new(Gate::default());
    let active_gate = Arc::clone(&gate);
    executor
        .submit(job("active", CoordinatorLane::Sync, None, move || {
            active_gate.block();
            Ok(())
        }))
        .expect("active job queues");
    gate.wait_for_started(1);

    let (recovery_tx, recovery_rx) = std::sync::mpsc::channel();
    executor
        .submit(CoordinatorJob::recoverable(
            CoordinatorJobId::new("queued-durable"),
            CoordinatorLane::Sync,
            None,
            "durable-claim".to_string(),
            |_| panic!("queued durable work must not execute during shutdown"),
            move |claim, kind| {
                recovery_tx
                    .send((claim, kind))
                    .expect("shutdown recovery sends");
            },
        ))
        .expect("durable job queues behind active work");

    let shutdown_executor = Arc::clone(&executor);
    let shutdown = std::thread::spawn(move || shutdown_executor.shutdown_and_join());
    let (claim, kind) = recovery_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("queued durable claim is recovered before join");
    assert_eq!(claim, "durable-claim");
    assert_eq!(kind, CoordinatorSubmitErrorKind::ShuttingDown);
    gate.release();
    shutdown
        .join()
        .expect("shutdown thread joins")
        .expect("workers join");
    assert_eq!(executor.metrics().shutdown_recoveries, 1);
}

#[test]
fn worker_loss_event_identifies_only_its_active_job() {
    let metrics = Arc::new(CoordinatorMetrics::default());
    let mut state = CoordinatorState::new(FakeCoordinatorClock::default(), metrics);
    let loss = CoordinatorWorkerLoss {
        lane: CoordinatorLane::Sync,
        worker_index: 0,
        active_job_id: Some(CoordinatorJobId::new("workspace-a-claim")),
    };
    assert_eq!(
        state.handle_event(CoordinatorEvent::WorkerLost(loss.clone())),
        vec![CoordinatorAction::WorkerLost(loss)]
    );
}

#[test]
fn saturated_worker_loss_delivery_recovers_only_the_failed_workers_job() {
    let (handle, receiver) = coordinator_channel(1);
    handle
        .try_send(CoordinatorEvent::DurableWorkAvailable)
        .expect("coordinator channel is saturated");
    let metrics = Arc::new(CoordinatorMetrics::default());
    let executor = CoordinatorExecutor::new(
        CoordinatorExecutorConfig::testing([1, 1, 2, 1, 1], 4),
        handle,
        Arc::clone(&metrics),
    )
    .expect("coordinator executor starts");
    let peer_gate = Arc::new(Gate::default());
    let peer_worker_gate = Arc::clone(&peer_gate);
    let (peer_completion_tx, peer_completion_rx) = std::sync::mpsc::channel();
    executor
        .submit(
            job(
                "peer-claim",
                CoordinatorLane::Sync,
                Some(CoordinatorResourceKey::new("workspace-peer")),
                move || {
                    peer_worker_gate.block();
                    Ok(())
                },
            )
            .on_completion_delivery_failure(move |completion| {
                peer_completion_tx
                    .send(completion.job_id)
                    .expect("peer completion fallback sends");
            }),
        )
        .expect("peer job queues");
    peer_gate.wait_for_started(1);

    let (loss_tx, loss_rx) = std::sync::mpsc::channel();
    executor
        .submit(
            job(
                "failed-claim",
                CoordinatorLane::Sync,
                Some(CoordinatorResourceKey::new("workspace-failed")),
                || Ok(()),
            )
            .on_completion_delivery_failure(|_| panic!("synthetic completion fallback worker loss"))
            .on_worker_loss_delivery_failure(move |loss| {
                loss_tx.send(loss).expect("worker loss fallback sends");
            }),
        )
        .expect("failed job queues");

    let loss = loss_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker loss uses nonblocking recovery fallback");
    assert_eq!(
        loss.active_job_id.as_ref().map(CoordinatorJobId::as_str),
        Some("failed-claim")
    );
    assert!(peer_completion_rx.try_recv().is_err());
    let (replacement_tx, replacement_rx) = std::sync::mpsc::channel();
    executor
        .submit(
            job(
                "replacement-claim",
                CoordinatorLane::Sync,
                Some(CoordinatorResourceKey::new("workspace-replacement")),
                || Ok(()),
            )
            .on_completion_delivery_failure(move |completion| {
                replacement_tx
                    .send(completion.job_id)
                    .expect("replacement completion fallback sends");
            }),
        )
        .expect("replacement job queues on restarted worker");
    assert_eq!(
        replacement_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("lost worker is replaced while peer remains blocked")
            .as_str(),
        "replacement-claim"
    );
    peer_gate.release();
    assert_eq!(
        peer_completion_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("peer completes independently")
            .as_str(),
        "peer-claim"
    );
    assert!(matches!(
        receiver.try_recv(),
        Ok(CoordinatorEvent::DurableWorkAvailable)
    ));
    executor.shutdown_and_join().expect("workers join");
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.worker_losses, 1);
    assert_eq!(snapshot.worker_loss_delivery_recoveries, 1);
    assert_eq!(snapshot.worker_loss_events_dropped, 0);
}

fn test_executor(
    workers: [usize; 5],
    queue_capacity: usize,
) -> (CoordinatorExecutor, Receiver<CoordinatorEvent>) {
    let (handle, receiver) = coordinator_channel(128);
    let metrics = Arc::new(CoordinatorMetrics::default());
    let executor = CoordinatorExecutor::new(
        CoordinatorExecutorConfig::testing(workers, queue_capacity),
        handle,
        metrics,
    )
    .expect("coordinator executor starts");
    (executor, receiver)
}

fn job(
    id: &str,
    lane: CoordinatorLane,
    resource: Option<CoordinatorResourceKey>,
    task: impl FnOnce() -> Result<(), CoordinatorWorkFailure> + Send + 'static,
) -> CoordinatorJob {
    CoordinatorJob::new(CoordinatorJobId::new(id), lane, resource, task)
}

fn recv_completion(receiver: &Receiver<CoordinatorEvent>) -> CoordinatorWorkerCompletion {
    match receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("coordinator completion arrives")
    {
        CoordinatorEvent::WorkerCompleted(completion) => completion,
        event => panic!("unexpected coordinator event: {event:?}"),
    }
}
