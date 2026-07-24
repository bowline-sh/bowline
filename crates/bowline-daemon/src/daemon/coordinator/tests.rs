use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use crossbeam_channel::Receiver;

use super::{
    clock::FakeCoordinatorClock,
    lanes::{CONTROL_PLANE_WORKERS, NOTIFICATION_WORKERS},
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
        config.lane(CoordinatorLane::ControlPlane).workers,
        CONTROL_PLANE_WORKERS
    );
    assert_eq!(
        config.lane(CoordinatorLane::Notification).workers,
        NOTIFICATION_WORKERS
    );
    assert_eq!((CONTROL_PLANE_WORKERS, NOTIFICATION_WORKERS), (4, 1));
}

#[test]
fn fake_clock_deadline_has_zero_churn_and_an_event_wakes_early() {
    let clock = FakeCoordinatorClock::default();
    let metrics = Arc::new(CoordinatorMetrics::default());
    let mut state = CoordinatorState::new(clock.clone(), Arc::clone(&metrics));
    let deadline = CoordinatorDeadline {
        due: CoordinatorInstant::ZERO.add(Duration::from_secs(10)),
        kind: CoordinatorDeadlineKind::EngineRetry(CoordinatorJobId::new("retry-1")),
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
        state.handle_event(CoordinatorEvent::EngineWorkAvailable),
        vec![CoordinatorAction::DriveEngine]
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
        kind: CoordinatorDeadlineKind::EngineRetry(CoordinatorJobId::new("later")),
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
        .try_send(CoordinatorEvent::EngineWorkAvailable)
        .expect("event wakes the blocking driver");
    assert_eq!(
        worker.join().expect("driver joins").expect("turn succeeds"),
        vec![CoordinatorAction::DriveEngine]
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.deadlines_fired, 0);
    assert_eq!(snapshot.idle_wakeups, 0);
}

#[test]
fn blocked_control_plane_worker_cannot_starve_the_notification_lane() {
    let (executor, receiver) = test_executor([1, 1], 16);
    let control_gate = Arc::new(Gate::default());
    let worker_gate = Arc::clone(&control_gate);
    executor
        .submit(job(
            "control-long",
            CoordinatorLane::ControlPlane,
            move || {
                worker_gate.block();
                Ok(())
            },
        ))
        .expect("long control-plane job queues");
    control_gate.wait_for_started(1);

    executor
        .submit(job(
            "notification",
            CoordinatorLane::Notification,
            || Ok(()),
        ))
        .expect("notification queues");
    assert_eq!(recv_completion(&receiver).job_id.as_str(), "notification");
    control_gate.release();
    assert_eq!(recv_completion(&receiver).job_id.as_str(), "control-long");
    executor.shutdown_and_join().expect("workers join");
}

#[test]
fn full_lane_queue_rejects_submission_with_typed_error() {
    let (executor, _receiver) = test_executor([1, 1], 1);
    let gate = Arc::new(Gate::default());
    let worker_gate = Arc::clone(&gate);
    executor
        .submit(job("active", CoordinatorLane::ControlPlane, move || {
            worker_gate.block();
            Ok(())
        }))
        .expect("active job queues");
    gate.wait_for_started(1);
    executor
        .submit(job("queued", CoordinatorLane::ControlPlane, || Ok(())))
        .expect("queue slot fills");
    let error = executor
        .submit(job("rejected", CoordinatorLane::ControlPlane, || Ok(())))
        .expect_err("full queue rejects");
    assert_eq!(error.kind, CoordinatorSubmitErrorKind::LaneQueueFull);
    assert_eq!(error.job.id.as_str(), "rejected");
    assert_eq!(
        executor
            .metrics()
            .lane(CoordinatorLane::ControlPlane)
            .dispatch_rejected,
        1
    );
    gate.release();
    executor.shutdown_and_join().expect("workers join");
}

#[test]
fn panicked_worker_job_reports_typed_completion_and_pool_continues() {
    let (executor, receiver) = test_executor([1, 1], 8);
    executor
        .submit(job("panic", CoordinatorLane::ControlPlane, || {
            panic!("synthetic coordinator panic")
        }))
        .expect("panic job queues");
    executor
        .submit(job("after", CoordinatorLane::ControlPlane, || Ok(())))
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
        .try_send(CoordinatorEvent::EngineWorkAvailable)
        .expect("coordinator channel is saturated");
    let metrics = Arc::new(CoordinatorMetrics::default());
    let executor = CoordinatorExecutor::new(
        CoordinatorExecutorConfig::testing([1, 1], 4),
        handle,
        Arc::clone(&metrics),
    )
    .expect("coordinator executor starts");
    let (recovery_tx, recovery_rx) = std::sync::mpsc::channel();
    executor
        .submit(
            job("status-saturated", CoordinatorLane::ControlPlane, || Ok(()))
                .on_completion_delivery_failure(move |completion| {
                    recovery_tx
                        .send(completion)
                        .expect("recovery observation sends");
                }),
        )
        .expect("status job queues");

    let recovered = recovery_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("saturated delivery invokes recovery");
    assert_eq!(recovered.job_id.as_str(), "status-saturated");
    assert_eq!(recovered.lane, CoordinatorLane::ControlPlane);
    assert_eq!(recovered.outcome, CoordinatorWorkerOutcome::Succeeded);
    assert!(matches!(
        receiver.try_recv(),
        Ok(CoordinatorEvent::EngineWorkAvailable)
    ));
    executor.shutdown_and_join().expect("workers join");
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.completion_delivery_recoveries, 1);
    assert_eq!(snapshot.completion_events_dropped, 0);
}

#[test]
fn worker_loss_event_identifies_only_its_active_job() {
    let metrics = Arc::new(CoordinatorMetrics::default());
    let mut state = CoordinatorState::new(FakeCoordinatorClock::default(), metrics);
    let loss = CoordinatorWorkerLoss {
        lane: CoordinatorLane::ControlPlane,
        worker_index: 0,
        active_job_id: Some(CoordinatorJobId::new("status-publish-1")),
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
        .try_send(CoordinatorEvent::EngineWorkAvailable)
        .expect("coordinator channel is saturated");
    let metrics = Arc::new(CoordinatorMetrics::default());
    let executor = CoordinatorExecutor::new(
        CoordinatorExecutorConfig::testing([2, 1], 4),
        handle,
        Arc::clone(&metrics),
    )
    .expect("coordinator executor starts");
    let peer_gate = Arc::new(Gate::default());
    let peer_worker_gate = Arc::clone(&peer_gate);
    let (peer_completion_tx, peer_completion_rx) = std::sync::mpsc::channel();
    executor
        .submit(
            job("peer-job", CoordinatorLane::ControlPlane, move || {
                peer_worker_gate.block();
                Ok(())
            })
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
            job("failed-job", CoordinatorLane::ControlPlane, || Ok(()))
                .on_completion_delivery_failure(|_| {
                    panic!("synthetic completion fallback worker loss")
                })
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
        Some("failed-job")
    );
    assert!(peer_completion_rx.try_recv().is_err());
    let (replacement_tx, replacement_rx) = std::sync::mpsc::channel();
    executor
        .submit(
            job("replacement-job", CoordinatorLane::ControlPlane, || Ok(()))
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
        "replacement-job"
    );
    peer_gate.release();
    assert_eq!(
        peer_completion_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("peer completes independently")
            .as_str(),
        "peer-job"
    );
    assert!(matches!(
        receiver.try_recv(),
        Ok(CoordinatorEvent::EngineWorkAvailable)
    ));
    executor.shutdown_and_join().expect("workers join");
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.worker_losses, 1);
    assert_eq!(snapshot.worker_loss_delivery_recoveries, 1);
    assert_eq!(snapshot.worker_loss_events_dropped, 0);
}

fn test_executor(
    workers: [usize; COORDINATOR_LANE_COUNT],
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
    task: impl FnOnce() -> Result<(), CoordinatorWorkFailure> + Send + 'static,
) -> CoordinatorJob {
    CoordinatorJob::new(CoordinatorJobId::new(id), lane, task)
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
