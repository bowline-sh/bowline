use std::{
    collections::VecDeque,
    fmt, io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Instant,
};

use super::{
    COORDINATOR_LANE_COUNT, CoordinatorEvent, CoordinatorHandle, CoordinatorJobId, CoordinatorLane,
    CoordinatorMetrics, CoordinatorWorkerCompletion, CoordinatorWorkerLoss,
    CoordinatorWorkerOutcome,
};

pub(super) const CONTROL_PLANE_WORKERS: usize = 4;
pub(super) const NOTIFICATION_WORKERS: usize = 1;

const CONTROL_PLANE_QUEUE_CAPACITY: usize = 32;
const NOTIFICATION_QUEUE_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct CoordinatorLaneConfig {
    pub(in crate::daemon) workers: usize,
    pub(in crate::daemon) queue_capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct CoordinatorExecutorConfig {
    lanes: [CoordinatorLaneConfig; COORDINATOR_LANE_COUNT],
}

impl Default for CoordinatorExecutorConfig {
    fn default() -> Self {
        let mut lanes = [CoordinatorLaneConfig {
            workers: 1,
            queue_capacity: 1,
        }; COORDINATOR_LANE_COUNT];
        lanes[CoordinatorLane::ControlPlane.index()] = CoordinatorLaneConfig {
            workers: CONTROL_PLANE_WORKERS,
            queue_capacity: CONTROL_PLANE_QUEUE_CAPACITY,
        };
        lanes[CoordinatorLane::Notification.index()] = CoordinatorLaneConfig {
            workers: NOTIFICATION_WORKERS,
            queue_capacity: NOTIFICATION_QUEUE_CAPACITY,
        };
        Self { lanes }
    }
}

impl CoordinatorExecutorConfig {
    pub(in crate::daemon) fn lane(self, lane: CoordinatorLane) -> CoordinatorLaneConfig {
        self.lanes[lane.index()]
    }

    #[cfg(test)]
    pub(in crate::daemon) fn testing(
        workers: [usize; COORDINATOR_LANE_COUNT],
        queue_capacity: usize,
    ) -> Self {
        Self {
            lanes: std::array::from_fn(|index| CoordinatorLaneConfig {
                workers: workers[index],
                queue_capacity,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum CoordinatorWorkFailureCode {
    OwnershipLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::daemon) struct CoordinatorWorkFailure {
    pub(in crate::daemon) code: CoordinatorWorkFailureCode,
    pub(in crate::daemon) message: String,
}

impl CoordinatorWorkFailure {
    pub(in crate::daemon) fn new(
        code: CoordinatorWorkFailureCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

type CoordinatorTask = Box<dyn FnOnce() -> Result<(), CoordinatorWorkFailure> + Send + 'static>;
type CoordinatorCompletionDeliveryFailure =
    Box<dyn FnOnce(CoordinatorWorkerCompletion) + Send + 'static>;
type CoordinatorWorkerLossDeliveryFailure = Box<dyn FnOnce(CoordinatorWorkerLoss) + Send + 'static>;

pub(in crate::daemon) struct CoordinatorJob {
    pub(in crate::daemon) id: CoordinatorJobId,
    pub(in crate::daemon) lane: CoordinatorLane,
    task: Option<CoordinatorTask>,
    completion_delivery_failure: Option<CoordinatorCompletionDeliveryFailure>,
    worker_loss_delivery_failure: Option<CoordinatorWorkerLossDeliveryFailure>,
    enqueued_at: Instant,
}

impl fmt::Debug for CoordinatorJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoordinatorJob")
            .field("id", &self.id)
            .field("lane", &self.lane)
            .finish_non_exhaustive()
    }
}

impl CoordinatorJob {
    pub(in crate::daemon) fn new(
        id: CoordinatorJobId,
        lane: CoordinatorLane,
        task: impl FnOnce() -> Result<(), CoordinatorWorkFailure> + Send + 'static,
    ) -> Self {
        Self {
            id,
            lane,
            task: Some(Box::new(task)),
            completion_delivery_failure: None,
            worker_loss_delivery_failure: None,
            enqueued_at: Instant::now(),
        }
    }

    pub(in crate::daemon) fn on_completion_delivery_failure(
        mut self,
        recover: impl FnOnce(CoordinatorWorkerCompletion) + Send + 'static,
    ) -> Self {
        self.completion_delivery_failure = Some(Box::new(recover));
        self
    }

    pub(in crate::daemon) fn on_worker_loss_delivery_failure(
        mut self,
        recover: impl FnOnce(CoordinatorWorkerLoss) + Send + 'static,
    ) -> Self {
        self.worker_loss_delivery_failure = Some(Box::new(recover));
        self
    }

    fn execute(mut self) -> Result<(), CoordinatorWorkFailure> {
        let Some(task) = self.task.take() else {
            return Err(CoordinatorWorkFailure::new(
                CoordinatorWorkFailureCode::OwnershipLost,
                "coordinator job was already consumed",
            ));
        };
        task()
    }

    fn take_completion_delivery_failure(&mut self) -> Option<CoordinatorCompletionDeliveryFailure> {
        self.completion_delivery_failure.take()
    }

    fn take_worker_loss_delivery_failure(
        &mut self,
    ) -> Option<CoordinatorWorkerLossDeliveryFailure> {
        self.worker_loss_delivery_failure.take()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum CoordinatorSubmitErrorKind {
    LaneQueueFull,
    ShuttingDown,
}

#[derive(Debug)]
pub(in crate::daemon) struct CoordinatorSubmitError {
    pub(in crate::daemon) kind: CoordinatorSubmitErrorKind,
    pub(in crate::daemon) job: Box<CoordinatorJob>,
}

#[derive(Default)]
struct LaneQueue {
    jobs: VecDeque<CoordinatorJob>,
}

impl LaneQueue {
    fn len(&self) -> usize {
        self.jobs.len()
    }
}

#[derive(Default)]
struct ExecutorState {
    lanes: [LaneQueue; COORDINATOR_LANE_COUNT],
    shutting_down: bool,
}

struct ExecutorShared {
    config: CoordinatorExecutorConfig,
    state: Mutex<ExecutorState>,
    ready: [Condvar; COORDINATOR_LANE_COUNT],
    coordinator: CoordinatorHandle,
    metrics: Arc<CoordinatorMetrics>,
}

pub(in crate::daemon) struct CoordinatorExecutor {
    shared: Arc<ExecutorShared>,
    workers: Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl CoordinatorExecutor {
    pub(in crate::daemon) fn new(
        config: CoordinatorExecutorConfig,
        coordinator: CoordinatorHandle,
        metrics: Arc<CoordinatorMetrics>,
    ) -> io::Result<Self> {
        for lane in CoordinatorLane::ALL {
            let lane_config = config.lane(lane);
            if lane_config.workers == 0 || lane_config.queue_capacity == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "coordinator lanes require non-zero worker and queue capacities",
                ));
            }
        }
        let shared = Arc::new(ExecutorShared {
            config,
            state: Mutex::new(ExecutorState::default()),
            ready: std::array::from_fn(|_| Condvar::new()),
            coordinator,
            metrics,
        });
        let configured_workers = CoordinatorLane::ALL
            .into_iter()
            .map(|lane| config.lane(lane).workers)
            .sum();
        shared.metrics.record_configured_workers(configured_workers);
        let mut workers = Vec::new();
        for lane in CoordinatorLane::ALL {
            if let Err(error) =
                spawn_lane_workers(&shared, lane, config.lane(lane).workers, &mut workers)
            {
                let _queued = stop_workers(&shared);
                join_workers(&shared.metrics, workers)?;
                return Err(error);
            }
        }
        Ok(Self {
            shared,
            workers: Mutex::new(Some(workers)),
        })
    }

    pub(in crate::daemon) fn submit(
        &self,
        job: CoordinatorJob,
    ) -> Result<(), CoordinatorSubmitError> {
        let lane = job.lane;
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rejection = if state.shutting_down {
            Some(CoordinatorSubmitErrorKind::ShuttingDown)
        } else if state.lanes[lane.index()].len() >= self.shared.config.lane(lane).queue_capacity {
            Some(CoordinatorSubmitErrorKind::LaneQueueFull)
        } else {
            None
        };
        if let Some(kind) = rejection {
            self.shared.metrics.record_dispatch_rejected(lane);
            return Err(CoordinatorSubmitError {
                kind,
                job: Box::new(job),
            });
        }
        state.lanes[lane.index()].jobs.push_back(job);
        let queued = state.lanes[lane.index()].len();
        self.shared.metrics.record_enqueued(lane, queued);
        drop(state);
        self.shared.ready[lane.index()].notify_one();
        Ok(())
    }

    pub(in crate::daemon) fn metrics(&self) -> super::CoordinatorMetricsSnapshot {
        self.shared.metrics.snapshot()
    }

    pub(in crate::daemon) fn worker_count(&self) -> usize {
        CoordinatorLane::ALL
            .into_iter()
            .map(|lane| self.shared.config.lane(lane).workers)
            .sum()
    }

    pub(in crate::daemon) fn shutdown_and_join(&self) -> io::Result<()> {
        // Side-lane jobs (status publish, notification, trust refresh) are safe
        // to drop unexecuted at shutdown: each is a periodic, idempotent poll.
        let queued = stop_workers(&self.shared);
        for _job in queued {
            self.shared.metrics.record_shutdown_recovery();
        }
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        join_workers(&self.shared.metrics, workers)?;
        self.verify_stopped_metrics()
    }

    fn verify_stopped_metrics(&self) -> io::Result<()> {
        let snapshot = self.metrics();
        let within_bounds = CoordinatorLane::ALL.into_iter().all(|lane| {
            let metrics = snapshot.lane(lane);
            metrics.active == 0
                && metrics.queued == 0
                && metrics.max_active <= self.shared.config.lane(lane).workers
                && metrics.max_queued <= self.shared.config.lane(lane).queue_capacity
        }) && snapshot.joined_workers == snapshot.configured_workers;
        if within_bounds {
            Ok(())
        } else {
            Err(io::Error::other(
                "coordinator lane executor stopped outside configured bounds",
            ))
        }
    }
}

fn stop_workers(shared: &ExecutorShared) -> Vec<CoordinatorJob> {
    let queued = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.shutting_down = true;
        let mut queued = Vec::new();
        for lane in CoordinatorLane::ALL {
            queued.extend(state.lanes[lane.index()].jobs.drain(..));
            shared.metrics.record_dequeued(lane, 0);
        }
        queued
    };
    for ready in &shared.ready {
        ready.notify_all();
    }
    queued
}

fn join_workers(metrics: &CoordinatorMetrics, workers: Vec<JoinHandle<()>>) -> io::Result<()> {
    let mut panicked = false;
    for worker in workers {
        panicked |= worker.join().is_err();
        metrics.record_worker_joined();
    }
    if panicked {
        Err(io::Error::other("coordinator lane worker panicked"))
    } else {
        Ok(())
    }
}

impl Drop for CoordinatorExecutor {
    fn drop(&mut self) {
        let _result = self.shutdown_and_join();
    }
}

fn spawn_lane_workers(
    shared: &Arc<ExecutorShared>,
    lane: CoordinatorLane,
    count: usize,
    workers: &mut Vec<JoinHandle<()>>,
) -> io::Result<()> {
    for worker_index in 0..count {
        let worker_shared = Arc::clone(shared);
        let active_job_id = Arc::new(Mutex::new(None::<CoordinatorJobId>));
        let worker_active_job_id = Arc::clone(&active_job_id);
        let active_loss_recovery =
            Arc::new(Mutex::new(None::<CoordinatorWorkerLossDeliveryFailure>));
        let worker_active_loss_recovery = Arc::clone(&active_loss_recovery);
        let handle = thread::Builder::new()
            .name(format!(
                "bowline-coordinator-{}-{worker_index}",
                lane.as_str()
            ))
            .spawn(move || {
                loop {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        run_lane_worker(
                            &worker_shared,
                            lane,
                            &worker_active_job_id,
                            &worker_active_loss_recovery,
                        );
                    }));
                    if result.is_ok() {
                        break;
                    }
                    worker_shared.metrics.record_worker_loss();
                    let active_job_id = worker_active_job_id
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    let loss = CoordinatorWorkerLoss {
                        lane,
                        worker_index,
                        active_job_id,
                    };
                    if worker_shared
                        .coordinator
                        .try_send(CoordinatorEvent::WorkerLost(loss.clone()))
                        .is_err()
                    {
                        let fallback = worker_active_loss_recovery
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                        if let Some(fallback) = fallback {
                            fallback(loss);
                            worker_shared.metrics.record_worker_loss_delivery_recovery();
                        } else {
                            worker_shared.metrics.record_worker_loss_event_dropped();
                        }
                    } else {
                        worker_active_loss_recovery
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                    }
                    let shutting_down = worker_shared
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .shutting_down;
                    if shutting_down {
                        break;
                    }
                }
            })?;
        workers.push(handle);
    }
    Ok(())
}

fn run_lane_worker(
    shared: &ExecutorShared,
    lane: CoordinatorLane,
    active_job_id: &Mutex<Option<CoordinatorJobId>>,
    active_loss_recovery: &Mutex<Option<CoordinatorWorkerLossDeliveryFailure>>,
) {
    while let Some(mut job) = take_job(shared, lane) {
        *active_job_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job.id.clone());
        shared
            .metrics
            .record_queue_delay(lane, job.enqueued_at.elapsed());
        let started_at = Instant::now();
        let job_id = job.id.clone();
        let completion_delivery_failure = job.take_completion_delivery_failure();
        *active_loss_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            job.take_worker_loss_delivery_failure();
        shared.metrics.record_worker_started(lane);
        let result = catch_unwind(AssertUnwindSafe(|| job.execute()));
        let (outcome, failed, panicked) = match result {
            Ok(Ok(())) => (CoordinatorWorkerOutcome::Succeeded, false, false),
            Ok(Err(error)) => (CoordinatorWorkerOutcome::Failed(error), true, false),
            Err(_) => (CoordinatorWorkerOutcome::Panicked, true, true),
        };
        shared
            .metrics
            .record_worker_finished(lane, started_at.elapsed(), failed, panicked);
        let completion = CoordinatorWorkerCompletion {
            job_id,
            lane,
            outcome,
        };
        if shared
            .coordinator
            .try_send(CoordinatorEvent::WorkerCompleted(completion.clone()))
            .is_err()
        {
            if let Some(recover) = completion_delivery_failure {
                recover(completion);
                shared.metrics.record_completion_delivery_recovery();
            } else {
                shared.metrics.record_completion_event_dropped();
            }
        }
        *active_job_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *active_loss_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

fn take_job(shared: &ExecutorShared, lane: CoordinatorLane) -> Option<CoordinatorJob> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if let Some(job) = state.lanes[lane.index()].jobs.pop_front() {
            let queued = state.lanes[lane.index()].len();
            shared.metrics.record_dequeued(lane, queued);
            return Some(job);
        }
        if state.shutting_down {
            return None;
        }
        state = shared.ready[lane.index()]
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}
