use std::{
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use bowline_core::wire::generated::{DaemonRpcErrorCode, DaemonRpcRequest, DaemonRpcResponse};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};

use super::{
    request_context::{
        CancellationPoint, CancellationReason, CancellationToken, RequestContext, RpcConnectionId,
        RpcRequestId,
    },
    request_context_error, response_for, rpc_error,
};
use metrics::RpcExecutorMetrics;
pub(in crate::daemon) use metrics::RpcExecutorMetricsSnapshot;
use queue::{QueueState, validate_capacity};

mod metrics;
mod queue;
#[cfg(test)]
mod tests;

pub(super) const QUERY_WORKERS: usize = 8;
pub(super) const RESERVED_STATUS_WORKERS: usize = 1;
pub(super) const MUTATION_WORKERS: usize = 4;
pub(super) const STATUS_QUEUE_CAPACITY: usize = 16;
pub(super) const QUERY_QUEUE_CAPACITY: usize = 64;
pub(super) const MUTATION_QUEUE_CAPACITY: usize = 32;
pub(super) const GLOBAL_QUEUE_CAPACITY: usize = 96;
pub(super) const PER_CONNECTION_QUEUE_CAPACITY: usize = 16;

pub(super) type RequestRouter =
    dyn Fn(RequestContext, DaemonRpcRequest) -> DaemonRpcResponse + Send + Sync + 'static;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RpcLane {
    Status,
    Query,
    Mutation,
}

impl RpcLane {
    pub(super) fn for_method(method: &str) -> Option<Self> {
        Some(match method {
            "daemon.ping" | "status.getSnapshot" => Self::Status,
            "daemon.info" | "daemon.metrics" | "sync.barrier" => Self::Query,
            "device.approve" | "device.deny" => Self::Mutation,
            "work.create" | "work.review" | "work.accept" => Self::Mutation,
            _ => return None,
        })
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Query => "query",
            Self::Mutation => "mutation",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RpcExecutorConfig {
    pub(super) query_workers: usize,
    pub(super) reserved_status_workers: usize,
    pub(super) mutation_workers: usize,
    pub(super) status_queue_capacity: usize,
    pub(super) query_queue_capacity: usize,
    pub(super) mutation_queue_capacity: usize,
    pub(super) global_queue_capacity: usize,
    pub(super) per_connection_queue_capacity: usize,
}

impl Default for RpcExecutorConfig {
    fn default() -> Self {
        Self {
            query_workers: QUERY_WORKERS,
            reserved_status_workers: RESERVED_STATUS_WORKERS,
            mutation_workers: MUTATION_WORKERS,
            status_queue_capacity: STATUS_QUEUE_CAPACITY,
            query_queue_capacity: QUERY_QUEUE_CAPACITY,
            mutation_queue_capacity: MUTATION_QUEUE_CAPACITY,
            global_queue_capacity: GLOBAL_QUEUE_CAPACITY,
            per_connection_queue_capacity: PER_CONNECTION_QUEUE_CAPACITY,
        }
    }
}

#[cfg(test)]
impl RpcExecutorConfig {
    pub(super) fn testing(query_workers: usize, mutation_workers: usize) -> Self {
        Self {
            query_workers,
            reserved_status_workers: 1,
            mutation_workers,
            status_queue_capacity: 64,
            query_queue_capacity: 64,
            mutation_queue_capacity: 64,
            global_queue_capacity: 128,
            per_connection_queue_capacity: 64,
        }
    }
}

pub(super) struct RpcCompletion {
    pub(super) request_id: RpcRequestId,
    pub(super) cancellation: CancellationToken,
    pub(super) response: DaemonRpcResponse,
}

struct RpcJob {
    connection_id: RpcConnectionId,
    context: RequestContext,
    request: DaemonRpcRequest,
    router: Arc<RequestRouter>,
    completion_sender: Sender<RpcCompletion>,
    enqueued_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubmissionError {
    GlobalQueueFull,
    LaneQueueFull(RpcLane),
    ConnectionQueueFull,
    ShuttingDown,
    UnknownMethod,
}

impl SubmissionError {
    pub(super) const fn scope(self) -> &'static str {
        match self {
            Self::GlobalQueueFull => "global",
            Self::LaneQueueFull(_) => "lane",
            Self::ConnectionQueueFull => "connection",
            Self::ShuttingDown => "executor",
            Self::UnknownMethod => "method",
        }
    }
}

struct ExecutorShared {
    config: RpcExecutorConfig,
    state: Mutex<QueueState>,
    status_ready: Condvar,
    query_ready: Condvar,
    mutation_ready: Condvar,
    metrics: RpcExecutorMetrics,
}

pub(crate) struct RpcExecutor {
    shared: Arc<ExecutorShared>,
    workers: Mutex<Option<Vec<RpcWorker>>>,
    next_connection_id: AtomicU64,
    next_correlation_id: AtomicU64,
}

struct RpcWorker {
    handle: JoinHandle<()>,
    done: Receiver<()>,
}

impl RpcExecutor {
    pub(crate) fn new(config: RpcExecutorConfig) -> io::Result<Self> {
        if config.query_workers <= config.reserved_status_workers
            || config.reserved_status_workers == 0
            || config.mutation_workers == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RPC executor lanes require status, query, and mutation workers",
            ));
        }
        let shared = Arc::new(ExecutorShared {
            config,
            state: Mutex::new(QueueState::default()),
            status_ready: Condvar::new(),
            query_ready: Condvar::new(),
            mutation_ready: Condvar::new(),
            metrics: RpcExecutorMetrics::new(config),
        });
        let mut workers = Vec::with_capacity(config.query_workers + config.mutation_workers);
        if let Err(error) = spawn_workers(
            &shared,
            RpcLane::Status,
            config.reserved_status_workers,
            &mut workers,
        )
        .and_then(|()| {
            spawn_workers(
                &shared,
                RpcLane::Query,
                config.query_workers - config.reserved_status_workers,
                &mut workers,
            )
        })
        .and_then(|()| {
            spawn_workers(
                &shared,
                RpcLane::Mutation,
                config.mutation_workers,
                &mut workers,
            )
        }) {
            request_worker_shutdown(&shared);
            for worker in workers {
                let _finished = worker.done.recv();
                if worker.handle.join().is_err() {
                    eprintln!("bowline RPC worker panicked while startup was rolled back");
                }
            }
            return Err(error);
        }
        Ok(Self {
            shared,
            workers: Mutex::new(Some(workers)),
            next_connection_id: AtomicU64::new(1),
            next_correlation_id: AtomicU64::new(1),
        })
    }

    pub(super) fn next_connection_id(&self) -> RpcConnectionId {
        RpcConnectionId::new(self.next_connection_id.fetch_add(1, Ordering::Relaxed))
    }

    pub(super) fn request_context(
        &self,
        connection_id: RpcConnectionId,
        request_id: RpcRequestId,
        deadline: Option<Instant>,
    ) -> RequestContext {
        RequestContext::new(
            connection_id,
            self.next_correlation_id.fetch_add(1, Ordering::Relaxed),
            request_id,
            deadline,
        )
    }

    pub(super) fn submit(
        &self,
        connection_id: RpcConnectionId,
        context: RequestContext,
        request: DaemonRpcRequest,
        router: Arc<RequestRouter>,
        completion_sender: Sender<RpcCompletion>,
    ) -> Result<(), SubmissionError> {
        let Some(lane) = RpcLane::for_method(&request.method) else {
            self.shared
                .metrics
                .record_rejected(SubmissionError::UnknownMethod);
            return Err(SubmissionError::UnknownMethod);
        };
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = validate_capacity(&state, self.shared.config, connection_id, lane) {
            self.shared.metrics.record_rejected(error);
            return Err(error);
        }
        state.lane_mut(lane).push(RpcJob {
            connection_id,
            context,
            request,
            router,
            completion_sender,
            enqueued_at: Instant::now(),
        });
        state.queued_total += 1;
        *state
            .queued_per_connection
            .entry(connection_id)
            .or_default() += 1;
        let lane_depth = state.lane(lane).len;
        self.shared
            .metrics
            .record_enqueued(lane, lane_depth, state.queued_total);
        drop(state);
        self.shared.ready(lane).notify_one();
        Ok(())
    }

    pub(super) fn cancel_connection(&self, connection_id: RpcConnectionId) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = state
            .status
            .remove_connection(connection_id)
            .into_iter()
            .chain(state.query.remove_connection(connection_id))
            .chain(state.mutation.remove_connection(connection_id))
            .collect::<Vec<_>>();
        state.queued_total = state.queued_total.saturating_sub(removed.len());
        state.queued_per_connection.remove(&connection_id);
        drop(state);
        for job in removed {
            job.context
                .cancellation()
                .cancel(CancellationReason::Disconnected);
            self.shared.metrics.record_disconnected_queued();
        }
    }

    pub(super) fn cancel_request(
        &self,
        connection_id: RpcConnectionId,
        cancellation: &CancellationToken,
    ) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = state
            .status
            .remove_request(connection_id, cancellation)
            .or_else(|| state.query.remove_request(connection_id, cancellation))
            .or_else(|| state.mutation.remove_request(connection_id, cancellation));
        if removed.is_none() {
            return;
        }
        state.queued_total = state.queued_total.saturating_sub(1);
        if let Some(queued) = state.queued_per_connection.get_mut(&connection_id) {
            *queued = queued.saturating_sub(1);
            if *queued == 0 {
                state.queued_per_connection.remove(&connection_id);
            }
        }
        self.shared.metrics.record_cancelled_queued();
    }

    pub(super) fn record_terminal_cancellation(&self, token: &CancellationToken) {
        self.shared.metrics.record_terminal_cancellation(token);
    }

    pub(super) fn record_cancellation_checkpoint(&self, token: &CancellationToken) {
        self.shared.metrics.record_cancellation_checkpoint(token);
    }

    pub(crate) fn shutdown_strict(
        &self,
        grace: std::time::Duration,
    ) -> io::Result<super::super::protocol::ThreadJoinReport> {
        request_worker_shutdown(&self.shared);
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        let deadline = Instant::now() + grace;
        let expected = workers.len();
        let mut joined = 0_usize;
        let mut forced_recovery = false;
        for worker in workers {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match worker.done.recv_timeout(remaining) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
                Err(RecvTimeoutError::Timeout) => {
                    forced_recovery = true;
                    super::super::protocol::handle_shutdown_grace_expiry("RPC request worker");
                    let _finished_or_panicked = worker.done.recv();
                }
            }
            worker
                .handle
                .join()
                .map_err(|_| io::Error::other("bowline RPC executor worker panicked"))?;
            joined += 1;
        }
        self.verify_stopped_metrics()?;
        Ok(super::super::protocol::ThreadJoinReport {
            expected,
            joined,
            forced_recovery,
        })
    }

    #[cfg(test)]
    pub(super) fn shutdown_and_join(&self) -> io::Result<()> {
        self.shutdown_strict(std::time::Duration::from_secs(5))
            .map(|_| ())
    }

    pub(in crate::daemon) fn metrics(&self) -> RpcExecutorMetricsSnapshot {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.shared.metrics.snapshot(&state)
    }

    fn verify_stopped_metrics(&self) -> io::Result<()> {
        let metrics = self.metrics();
        if metrics.active_query == 0
            && metrics.active_mutation == 0
            && metrics.queued_global == 0
            && metrics.max_active_query <= metrics.configured_query_workers
            && metrics.max_active_mutation <= metrics.configured_mutation_workers
        {
            Ok(())
        } else {
            Err(io::Error::other(
                "bowline RPC executor stopped outside its configured bounds",
            ))
        }
    }

    #[cfg(test)]
    pub(super) fn disconnected_queued(&self) -> u64 {
        self.metrics().disconnected_queued
    }
}

impl ExecutorShared {
    fn ready(&self, lane: RpcLane) -> &Condvar {
        match lane {
            RpcLane::Status => &self.status_ready,
            RpcLane::Query => &self.query_ready,
            RpcLane::Mutation => &self.mutation_ready,
        }
    }

    fn take_job(&self, lane: RpcLane) -> Option<RpcJob> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(job) = state.lane_mut(lane).pop() {
                state.queued_total = state.queued_total.saturating_sub(1);
                if let Some(queued) = state.queued_per_connection.get_mut(&job.connection_id) {
                    *queued = queued.saturating_sub(1);
                    if *queued == 0 {
                        state.queued_per_connection.remove(&job.connection_id);
                    }
                }
                return Some(job);
            }
            if state.shutting_down {
                return None;
            }
            state = self
                .ready(lane)
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Drop for RpcExecutor {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown_strict(std::time::Duration::from_secs(30)) {
            eprintln!("bowline-daemon RPC executor ownership drop failed: {error}");
        }
    }
}

fn spawn_workers(
    shared: &Arc<ExecutorShared>,
    lane: RpcLane,
    count: usize,
    workers: &mut Vec<RpcWorker>,
) -> io::Result<()> {
    for index in 0..count {
        let worker_shared = Arc::clone(shared);
        let (done_sender, done) = bounded(1);
        let handle = thread::Builder::new()
            .name(format!("bowline-rpc-{}-{index}", lane.as_str()))
            .spawn(move || {
                run_worker(worker_shared, lane);
                let _receiver_gone = done_sender.send(());
            })?;
        workers.push(RpcWorker { handle, done });
    }
    Ok(())
}

fn run_worker(shared: Arc<ExecutorShared>, lane: RpcLane) {
    while let Some(job) = shared.take_job(lane) {
        shared
            .metrics
            .record_queue_delay(lane, job.enqueued_at.elapsed());
        let started_at = Instant::now();
        shared.metrics.worker_started(lane);
        let (completion, completion_sender, panicked) = execute_job(job);
        shared
            .metrics
            .worker_finished(lane, started_at.elapsed(), panicked);
        if completion_sender.send(completion).is_err() {
            shared.metrics.record_completion_receiver_gone();
        }
    }
}

fn execute_job(job: RpcJob) -> (RpcCompletion, Sender<RpcCompletion>, bool) {
    let request_id = job.context.request_id().clone();
    let cancellation = job.context.cancellation().clone();
    let (response, panicked) = match job.context.checkpoint(CancellationPoint::HandlerStart) {
        Ok(()) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (job.router)(job.context.clone(), job.request)
        })) {
            Ok(response) => (response, false),
            Err(_) => {
                let mut error = rpc_error(
                    DaemonRpcErrorCode::Internal,
                    "the daemon request worker terminated unexpectedly",
                    false,
                );
                error.details = Some(serde_json::json!({
                    "correlationId": job.context.correlation_id().as_str(),
                }));
                (
                    response_for(request_id.as_str().to_string(), Err(error)),
                    true,
                )
            }
        },
        Err(error) => (
            response_for(
                request_id.as_str().to_string(),
                Err(request_context_error(&job.context, error)),
            ),
            false,
        ),
    };
    (
        RpcCompletion {
            request_id,
            cancellation,
            response,
        },
        job.completion_sender,
        panicked,
    )
}

fn request_worker_shutdown(shared: &ExecutorShared) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.shutting_down = true;
    drop(state);
    shared.status_ready.notify_all();
    shared.query_ready.notify_all();
    shared.mutation_ready.notify_all();
}
