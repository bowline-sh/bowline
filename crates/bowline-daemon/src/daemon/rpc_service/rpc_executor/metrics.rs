use std::{
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use super::{
    super::request_context::{CancellationReason, CancellationToken},
    QueueState, RpcExecutorConfig, RpcLane, SubmissionError,
};

pub(super) struct RpcExecutorMetrics {
    configured_query_workers: usize,
    configured_mutation_workers: usize,
    enqueued_query: AtomicU64,
    enqueued_mutation: AtomicU64,
    rejected_busy: AtomicU64,
    rejected_unknown_method: AtomicU64,
    active_query: AtomicUsize,
    active_mutation: AtomicUsize,
    max_active_query: AtomicUsize,
    max_active_mutation: AtomicUsize,
    max_queued_status: AtomicUsize,
    max_queued_query: AtomicUsize,
    max_queued_mutation: AtomicUsize,
    max_queued_global: AtomicUsize,
    completed: AtomicU64,
    panicked: AtomicU64,
    cancelled_responses: AtomicU64,
    deadline_responses: AtomicU64,
    disconnected_queued: AtomicU64,
    cancelled_queued: AtomicU64,
    completion_receivers_gone: AtomicU64,
    queue_delay_samples: AtomicU64,
    queue_delay_total_nanos: AtomicU64,
    queue_delay_max_nanos: AtomicU64,
    execution_samples: AtomicU64,
    execution_total_nanos: AtomicU64,
    execution_max_nanos: AtomicU64,
    cancellation_latency_total_nanos: AtomicU64,
    cancellation_latency_max_nanos: AtomicU64,
}

impl RpcExecutorMetrics {
    pub(super) fn new(config: RpcExecutorConfig) -> Self {
        Self {
            configured_query_workers: config.query_workers,
            configured_mutation_workers: config.mutation_workers,
            enqueued_query: AtomicU64::new(0),
            enqueued_mutation: AtomicU64::new(0),
            rejected_busy: AtomicU64::new(0),
            rejected_unknown_method: AtomicU64::new(0),
            active_query: AtomicUsize::new(0),
            active_mutation: AtomicUsize::new(0),
            max_active_query: AtomicUsize::new(0),
            max_active_mutation: AtomicUsize::new(0),
            max_queued_status: AtomicUsize::new(0),
            max_queued_query: AtomicUsize::new(0),
            max_queued_mutation: AtomicUsize::new(0),
            max_queued_global: AtomicUsize::new(0),
            completed: AtomicU64::new(0),
            panicked: AtomicU64::new(0),
            cancelled_responses: AtomicU64::new(0),
            deadline_responses: AtomicU64::new(0),
            disconnected_queued: AtomicU64::new(0),
            cancelled_queued: AtomicU64::new(0),
            completion_receivers_gone: AtomicU64::new(0),
            queue_delay_samples: AtomicU64::new(0),
            queue_delay_total_nanos: AtomicU64::new(0),
            queue_delay_max_nanos: AtomicU64::new(0),
            execution_samples: AtomicU64::new(0),
            execution_total_nanos: AtomicU64::new(0),
            execution_max_nanos: AtomicU64::new(0),
            cancellation_latency_total_nanos: AtomicU64::new(0),
            cancellation_latency_max_nanos: AtomicU64::new(0),
        }
    }

    pub(super) fn record_enqueued(&self, lane: RpcLane, lane_depth: usize, global_depth: usize) {
        match lane {
            RpcLane::Status | RpcLane::Query => &self.enqueued_query,
            RpcLane::Mutation => &self.enqueued_mutation,
        }
        .fetch_add(1, Ordering::Relaxed);
        match lane {
            RpcLane::Status => &self.max_queued_status,
            RpcLane::Query => &self.max_queued_query,
            RpcLane::Mutation => &self.max_queued_mutation,
        }
        .fetch_max(lane_depth, Ordering::Relaxed);
        self.max_queued_global
            .fetch_max(global_depth, Ordering::Relaxed);
    }

    pub(super) fn record_rejected(&self, error: SubmissionError) {
        match error {
            SubmissionError::UnknownMethod => &self.rejected_unknown_method,
            _ => &self.rejected_busy,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_queue_delay(&self, _lane: RpcLane, delay: Duration) {
        let nanos = duration_nanos(delay);
        self.queue_delay_samples.fetch_add(1, Ordering::Relaxed);
        self.queue_delay_total_nanos
            .fetch_add(nanos, Ordering::Relaxed);
        self.queue_delay_max_nanos
            .fetch_max(nanos, Ordering::Relaxed);
    }

    pub(super) fn worker_started(&self, lane: RpcLane) {
        let (active, maximum) = match lane {
            RpcLane::Status | RpcLane::Query => (&self.active_query, &self.max_active_query),
            RpcLane::Mutation => (&self.active_mutation, &self.max_active_mutation),
        };
        let now = active.fetch_add(1, Ordering::AcqRel) + 1;
        maximum.fetch_max(now, Ordering::AcqRel);
    }

    pub(super) fn worker_finished(&self, lane: RpcLane, duration: Duration, panicked: bool) {
        match lane {
            RpcLane::Status | RpcLane::Query => &self.active_query,
            RpcLane::Mutation => &self.active_mutation,
        }
        .fetch_sub(1, Ordering::AcqRel);
        let nanos = duration_nanos(duration);
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.execution_samples.fetch_add(1, Ordering::Relaxed);
        self.execution_total_nanos
            .fetch_add(nanos, Ordering::Relaxed);
        self.execution_max_nanos.fetch_max(nanos, Ordering::Relaxed);
        if panicked {
            self.panicked.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn record_terminal_cancellation(&self, token: &CancellationToken) {
        match token.reason() {
            Some(CancellationReason::DeadlineExceeded) => &self.deadline_responses,
            Some(CancellationReason::Cancelled | CancellationReason::Disconnected) | None => {
                &self.cancelled_responses
            }
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_cancellation_checkpoint(&self, token: &CancellationToken) {
        if token.reason().is_some()
            && let Some(age) = token.cancellation_age()
        {
            let nanos = duration_nanos(age);
            self.cancellation_latency_total_nanos
                .fetch_add(nanos, Ordering::Relaxed);
            self.cancellation_latency_max_nanos
                .fetch_max(nanos, Ordering::Relaxed);
        }
    }

    pub(super) fn record_disconnected_queued(&self) {
        self.disconnected_queued.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_cancelled_queued(&self) {
        self.cancelled_queued.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_completion_receiver_gone(&self) {
        self.completion_receivers_gone
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self, state: &QueueState) -> RpcExecutorMetricsSnapshot {
        RpcExecutorMetricsSnapshot {
            configured_query_workers: self.configured_query_workers,
            configured_mutation_workers: self.configured_mutation_workers,
            enqueued_query: self.enqueued_query.load(Ordering::Relaxed),
            enqueued_mutation: self.enqueued_mutation.load(Ordering::Relaxed),
            rejected_busy: self.rejected_busy.load(Ordering::Relaxed),
            rejected_unknown_method: self.rejected_unknown_method.load(Ordering::Relaxed),
            active_query: self.active_query.load(Ordering::Relaxed),
            active_mutation: self.active_mutation.load(Ordering::Relaxed),
            max_active_query: self.max_active_query.load(Ordering::Relaxed),
            max_active_mutation: self.max_active_mutation.load(Ordering::Relaxed),
            queued_status: state.status.len,
            queued_query: state.query.len,
            queued_mutation: state.mutation.len,
            queued_global: state.queued_total,
            max_queued_status: self.max_queued_status.load(Ordering::Relaxed),
            max_queued_query: self.max_queued_query.load(Ordering::Relaxed),
            max_queued_mutation: self.max_queued_mutation.load(Ordering::Relaxed),
            max_queued_global: self.max_queued_global.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            panicked: self.panicked.load(Ordering::Relaxed),
            cancelled_responses: self.cancelled_responses.load(Ordering::Relaxed),
            deadline_responses: self.deadline_responses.load(Ordering::Relaxed),
            disconnected_queued: self.disconnected_queued.load(Ordering::Relaxed),
            cancelled_queued: self.cancelled_queued.load(Ordering::Relaxed),
            completion_receivers_gone: self.completion_receivers_gone.load(Ordering::Relaxed),
            queue_delay_samples: self.queue_delay_samples.load(Ordering::Relaxed),
            queue_delay_total_nanos: self.queue_delay_total_nanos.load(Ordering::Relaxed),
            queue_delay_max_nanos: self.queue_delay_max_nanos.load(Ordering::Relaxed),
            execution_samples: self.execution_samples.load(Ordering::Relaxed),
            execution_total_nanos: self.execution_total_nanos.load(Ordering::Relaxed),
            execution_max_nanos: self.execution_max_nanos.load(Ordering::Relaxed),
            cancellation_latency_total_nanos: self
                .cancellation_latency_total_nanos
                .load(Ordering::Relaxed),
            cancellation_latency_max_nanos: self
                .cancellation_latency_max_nanos
                .load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct RpcExecutorMetricsSnapshot {
    pub(super) configured_query_workers: usize,
    pub(super) configured_mutation_workers: usize,
    pub(super) enqueued_query: u64,
    pub(super) enqueued_mutation: u64,
    pub(super) rejected_busy: u64,
    pub(super) rejected_unknown_method: u64,
    pub(super) active_query: usize,
    pub(super) active_mutation: usize,
    pub(super) max_active_query: usize,
    pub(super) max_active_mutation: usize,
    pub(super) queued_status: usize,
    pub(super) queued_query: usize,
    pub(super) queued_mutation: usize,
    pub(super) queued_global: usize,
    pub(super) max_queued_status: usize,
    pub(super) max_queued_query: usize,
    pub(super) max_queued_mutation: usize,
    pub(super) max_queued_global: usize,
    pub(super) completed: u64,
    pub(super) panicked: u64,
    pub(super) cancelled_responses: u64,
    pub(super) deadline_responses: u64,
    pub(super) disconnected_queued: u64,
    pub(super) cancelled_queued: u64,
    pub(super) completion_receivers_gone: u64,
    pub(super) queue_delay_samples: u64,
    pub(super) queue_delay_total_nanos: u64,
    pub(super) queue_delay_max_nanos: u64,
    pub(super) execution_samples: u64,
    pub(super) execution_total_nanos: u64,
    pub(super) execution_max_nanos: u64,
    pub(super) cancellation_latency_total_nanos: u64,
    pub(super) cancellation_latency_max_nanos: u64,
}

impl RpcExecutorMetricsSnapshot {
    pub(in crate::daemon) fn configured_workers(self) -> usize {
        self.configured_query_workers + self.configured_mutation_workers
    }

    pub(in crate::daemon) fn queue_delay_max_nanos(self) -> u64 {
        self.queue_delay_max_nanos
    }

    pub(in crate::daemon) fn execution_max_nanos(self) -> u64 {
        self.execution_max_nanos
    }

    pub(in crate::daemon) fn cancellation_latency_max_nanos(self) -> u64 {
        self.cancellation_latency_max_nanos
    }

    pub(in crate::daemon) fn panicked(self) -> u64 {
        self.panicked
    }

    pub(in crate::daemon) fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "configuredQueryWorkers": self.configured_query_workers,
            "configuredMutationWorkers": self.configured_mutation_workers,
            "enqueuedQuery": self.enqueued_query,
            "enqueuedMutation": self.enqueued_mutation,
            "rejectedBusy": self.rejected_busy,
            "rejectedUnknownMethod": self.rejected_unknown_method,
            "activeQuery": self.active_query,
            "activeMutation": self.active_mutation,
            "maxActiveQuery": self.max_active_query,
            "maxActiveMutation": self.max_active_mutation,
            "queuedStatus": self.queued_status,
            "queuedQuery": self.queued_query,
            "queuedMutation": self.queued_mutation,
            "queuedGlobal": self.queued_global,
            "maxQueuedStatus": self.max_queued_status,
            "maxQueuedQuery": self.max_queued_query,
            "maxQueuedMutation": self.max_queued_mutation,
            "maxQueuedGlobal": self.max_queued_global,
            "completed": self.completed,
            "panicked": self.panicked,
            "cancelledResponses": self.cancelled_responses,
            "deadlineResponses": self.deadline_responses,
            "disconnectedQueued": self.disconnected_queued,
            "cancelledQueued": self.cancelled_queued,
            "completionReceiversGone": self.completion_receivers_gone,
            "queueDelaySamples": self.queue_delay_samples,
            "queueDelayTotalNanos": self.queue_delay_total_nanos,
            "queueDelayMaxNanos": self.queue_delay_max_nanos,
            "executionSamples": self.execution_samples,
            "executionTotalNanos": self.execution_total_nanos,
            "executionMaxNanos": self.execution_max_nanos,
            "cancellationLatencyTotalNanos": self.cancellation_latency_total_nanos,
            "cancellationLatencyMaxNanos": self.cancellation_latency_max_nanos,
        })
    }
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}
