use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt, io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Instant,
};

use super::{
    CoordinatorEvent, CoordinatorHandle, CoordinatorJobId, CoordinatorLane, CoordinatorMetrics,
    CoordinatorResourceKey, CoordinatorWorkerCompletion, CoordinatorWorkerLoss,
    CoordinatorWorkerOutcome,
};

pub(super) const MUTATION_WORKERS: usize = 4;
pub(super) const QUERY_WORKERS: usize = 8;
pub(super) const SYNC_WORKERS: usize = 2;
pub(super) const CONTROL_PLANE_WORKERS: usize = 4;
pub(super) const NOTIFICATION_WORKERS: usize = 1;

const MUTATION_QUEUE_CAPACITY: usize = 32;
const QUERY_QUEUE_CAPACITY: usize = 64;
const SYNC_QUEUE_CAPACITY: usize = 32;
const CONTROL_PLANE_QUEUE_CAPACITY: usize = 32;
const NOTIFICATION_QUEUE_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct CoordinatorLaneConfig {
    pub(in crate::daemon) workers: usize,
    pub(in crate::daemon) queue_capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct CoordinatorExecutorConfig {
    lanes: [CoordinatorLaneConfig; 5],
}

impl Default for CoordinatorExecutorConfig {
    fn default() -> Self {
        let mut lanes = [CoordinatorLaneConfig {
            workers: 1,
            queue_capacity: 1,
        }; 5];
        lanes[CoordinatorLane::Mutation.index()] = CoordinatorLaneConfig {
            workers: MUTATION_WORKERS,
            queue_capacity: MUTATION_QUEUE_CAPACITY,
        };
        lanes[CoordinatorLane::Query.index()] = CoordinatorLaneConfig {
            workers: QUERY_WORKERS,
            queue_capacity: QUERY_QUEUE_CAPACITY,
        };
        lanes[CoordinatorLane::Sync.index()] = CoordinatorLaneConfig {
            workers: SYNC_WORKERS,
            queue_capacity: SYNC_QUEUE_CAPACITY,
        };
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
    pub(in crate::daemon) fn testing(workers: [usize; 5], queue_capacity: usize) -> Self {
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
    ExecutionFailed,
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
type CoordinatorDispatchFailure = Box<dyn FnOnce(CoordinatorSubmitErrorKind) + Send + 'static>;
type CoordinatorCompletionDeliveryFailure =
    Box<dyn FnOnce(CoordinatorWorkerCompletion) + Send + 'static>;
type CoordinatorWorkerLossDeliveryFailure = Box<dyn FnOnce(CoordinatorWorkerLoss) + Send + 'static>;

pub(in crate::daemon) struct CoordinatorJob {
    pub(in crate::daemon) id: CoordinatorJobId,
    pub(in crate::daemon) lane: CoordinatorLane,
    pub(in crate::daemon) resource: Option<CoordinatorResourceKey>,
    task: Option<CoordinatorTask>,
    dispatch_failure: Option<CoordinatorDispatchFailure>,
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
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

impl CoordinatorJob {
    pub(in crate::daemon) fn new(
        id: CoordinatorJobId,
        lane: CoordinatorLane,
        resource: Option<CoordinatorResourceKey>,
        task: impl FnOnce() -> Result<(), CoordinatorWorkFailure> + Send + 'static,
    ) -> Self {
        Self {
            id,
            lane,
            resource,
            task: Some(Box::new(task)),
            dispatch_failure: None,
            completion_delivery_failure: None,
            worker_loss_delivery_failure: None,
            enqueued_at: Instant::now(),
        }
    }

    pub(in crate::daemon) fn recoverable<W>(
        id: CoordinatorJobId,
        lane: CoordinatorLane,
        resource: Option<CoordinatorResourceKey>,
        work: W,
        execute: impl FnOnce(W) -> Result<(), CoordinatorWorkFailure> + Send + 'static,
        recover: impl FnOnce(W, CoordinatorSubmitErrorKind) + Send + 'static,
    ) -> Self
    where
        W: Send + 'static,
    {
        let pending = Arc::new(Mutex::new(Some(work)));
        let execution_pending = Arc::clone(&pending);
        let recovery_pending = Arc::clone(&pending);
        let task = move || {
            let work = execution_pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .ok_or_else(|| {
                    CoordinatorWorkFailure::new(
                        CoordinatorWorkFailureCode::OwnershipLost,
                        "coordinator work ownership was lost before execution",
                    )
                })?;
            execute(work)
        };
        let dispatch_failure = move |kind| {
            if let Some(work) = recovery_pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                recover(work, kind);
            }
        };
        Self {
            id,
            lane,
            resource,
            task: Some(Box::new(task)),
            dispatch_failure: Some(Box::new(dispatch_failure)),
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

    fn recover_dispatch_failure(
        mut self,
        kind: CoordinatorSubmitErrorKind,
    ) -> Result<(), Box<Self>> {
        let Some(recovery) = self.dispatch_failure.take() else {
            return Err(Box::new(self));
        };
        recovery(kind);
        Ok(())
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

impl CoordinatorSubmitError {
    pub(in crate::daemon) fn recover(self) -> Result<(), Box<CoordinatorJob>> {
        (*self.job).recover_dispatch_failure(self.kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FairKey {
    Resource(CoordinatorResourceKey),
    Unscoped(CoordinatorJobId),
}

#[derive(Default)]
struct FairLaneQueue {
    by_key: HashMap<FairKey, VecDeque<CoordinatorJob>>,
    ready: VecDeque<FairKey>,
    len: usize,
}

impl FairLaneQueue {
    fn push(&mut self, job: CoordinatorJob) {
        let key = job
            .resource
            .clone()
            .map(FairKey::Resource)
            .unwrap_or_else(|| FairKey::Unscoped(job.id.clone()));
        let queue = self.by_key.entry(key.clone()).or_default();
        if queue.is_empty() {
            self.ready.push_back(key);
        }
        queue.push_back(job);
        self.len += 1;
    }

    fn pop_eligible(
        &mut self,
        active_resources: &HashSet<CoordinatorResourceKey>,
    ) -> Option<CoordinatorJob> {
        let candidates = self.ready.len();
        for _ in 0..candidates {
            let key = self.ready.pop_front()?;
            let blocked = self
                .by_key
                .get(&key)
                .and_then(|queue| queue.front())
                .and_then(|job| job.resource.as_ref())
                .is_some_and(|resource| active_resources.contains(resource));
            if blocked {
                self.ready.push_back(key);
                continue;
            }
            let queue = self.by_key.get_mut(&key)?;
            let job = queue.pop_front()?;
            self.len = self.len.saturating_sub(1);
            if queue.is_empty() {
                self.by_key.remove(&key);
            } else {
                self.ready.push_back(key);
            }
            return Some(job);
        }
        None
    }

    fn drain(&mut self) -> Vec<CoordinatorJob> {
        let mut jobs = Vec::with_capacity(self.len);
        while let Some(key) = self.ready.pop_front() {
            if let Some(mut queue) = self.by_key.remove(&key) {
                jobs.extend(queue.drain(..));
            }
        }
        self.len = 0;
        jobs
    }
}

#[derive(Default)]
struct ExecutorState {
    lanes: [FairLaneQueue; 5],
    active_resources: HashSet<CoordinatorResourceKey>,
    shutting_down: bool,
}

struct ExecutorShared {
    config: CoordinatorExecutorConfig,
    state: Mutex<ExecutorState>,
    ready: [Condvar; 5],
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
        } else if state.lanes[lane.index()].len >= self.shared.config.lane(lane).queue_capacity {
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
        state.lanes[lane.index()].push(job);
        let queued = state.lanes[lane.index()].len;
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
        let queued = stop_workers(&self.shared);
        for job in queued {
            if job
                .recover_dispatch_failure(CoordinatorSubmitErrorKind::ShuttingDown)
                .is_ok()
            {
                self.shared.metrics.record_shutdown_recovery();
            }
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
            queued.extend(state.lanes[lane.index()].drain());
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
        let active_resource = Arc::new(Mutex::new(None::<CoordinatorResourceKey>));
        let worker_active_resource = Arc::clone(&active_resource);
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
                            &worker_active_resource,
                            &worker_active_loss_recovery,
                        );
                    }));
                    if result.is_ok() {
                        break;
                    }
                    let resource = worker_active_resource
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    let active_resources = release_resource(&worker_shared, resource.as_ref());
                    worker_shared.metrics.record_worker_loss(active_resources);
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
    active_resource: &Mutex<Option<CoordinatorResourceKey>>,
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
        let resource = job.resource.clone();
        *active_resource
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = resource.clone();
        let completion_delivery_failure = job.take_completion_delivery_failure();
        *active_loss_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            job.take_worker_loss_delivery_failure();
        let active_resources = active_resource_count(shared);
        shared.metrics.record_worker_started(lane, active_resources);
        let result = catch_unwind(AssertUnwindSafe(|| job.execute()));
        let (outcome, failed, panicked) = match result {
            Ok(Ok(())) => (CoordinatorWorkerOutcome::Succeeded, false, false),
            Ok(Err(error)) => (CoordinatorWorkerOutcome::Failed(error), true, false),
            Err(_) => (CoordinatorWorkerOutcome::Panicked, true, true),
        };
        let active_resources = release_resource(shared, resource.as_ref());
        *active_resource
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        shared.metrics.record_worker_finished(
            lane,
            started_at.elapsed(),
            failed,
            panicked,
            active_resources,
        );
        let completion = CoordinatorWorkerCompletion {
            job_id,
            lane,
            resource,
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
        let active_resources = state.active_resources.clone();
        if let Some(job) = state.lanes[lane.index()].pop_eligible(&active_resources) {
            if let Some(resource) = &job.resource {
                let inserted = state.active_resources.insert(resource.clone());
                debug_assert!(inserted, "eligible resource must not already be active");
            }
            let queued = state.lanes[lane.index()].len;
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

fn active_resource_count(shared: &ExecutorShared) -> usize {
    shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active_resources
        .len()
}

fn release_resource(shared: &ExecutorShared, resource: Option<&CoordinatorResourceKey>) -> usize {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(resource) = resource {
        state.active_resources.remove(resource);
    }
    let active_resources = state.active_resources.len();
    drop(state);
    for ready in &shared.ready {
        ready.notify_all();
    }
    active_resources
}
