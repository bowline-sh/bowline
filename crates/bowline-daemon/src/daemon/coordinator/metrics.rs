use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::{COORDINATOR_LANE_COUNT, CoordinatorLane};

#[derive(Debug, Default)]
struct CoordinatorLaneMetrics {
    queued: AtomicUsize,
    max_queued: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    completed: AtomicU64,
    failed: AtomicU64,
    panicked: AtomicU64,
    dispatch_rejected: AtomicU64,
    queue_delay_samples: AtomicU64,
    queue_delay_total_nanos: AtomicU64,
    queue_delay_max_nanos: AtomicU64,
    execution_samples: AtomicU64,
    execution_total_nanos: AtomicU64,
    execution_max_nanos: AtomicU64,
}

#[derive(Debug)]
pub(in crate::daemon) struct CoordinatorMetrics {
    lanes: [CoordinatorLaneMetrics; COORDINATOR_LANE_COUNT],
    events_received: AtomicU64,
    deadlines_scheduled: AtomicU64,
    deadlines_fired: AtomicU64,
    worker_losses: AtomicU64,
    completion_events_dropped: AtomicU64,
    completion_delivery_recoveries: AtomicU64,
    idle_wakeups: AtomicU64,
    configured_workers: AtomicUsize,
    joined_workers: AtomicUsize,
    shutdown_recoveries: AtomicU64,
    worker_loss_delivery_recoveries: AtomicU64,
    worker_loss_events_dropped: AtomicU64,
}

impl Default for CoordinatorMetrics {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| CoordinatorLaneMetrics::default()),
            events_received: AtomicU64::new(0),
            deadlines_scheduled: AtomicU64::new(0),
            deadlines_fired: AtomicU64::new(0),
            worker_losses: AtomicU64::new(0),
            completion_events_dropped: AtomicU64::new(0),
            completion_delivery_recoveries: AtomicU64::new(0),
            idle_wakeups: AtomicU64::new(0),
            configured_workers: AtomicUsize::new(0),
            joined_workers: AtomicUsize::new(0),
            shutdown_recoveries: AtomicU64::new(0),
            worker_loss_delivery_recoveries: AtomicU64::new(0),
            worker_loss_events_dropped: AtomicU64::new(0),
        }
    }
}

impl CoordinatorMetrics {
    pub(super) fn record_event(&self) {
        self.events_received.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_deadline_scheduled(&self) {
        self.deadlines_scheduled.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_deadline_fired(&self) {
        self.deadlines_fired.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_enqueued(&self, lane: CoordinatorLane, queued: usize) {
        let metrics = &self.lanes[lane.index()];
        metrics.queued.store(queued, Ordering::Relaxed);
        metrics.max_queued.fetch_max(queued, Ordering::Relaxed);
    }

    pub(super) fn record_dequeued(&self, lane: CoordinatorLane, queued: usize) {
        self.lanes[lane.index()]
            .queued
            .store(queued, Ordering::Relaxed);
    }

    pub(super) fn record_dispatch_rejected(&self, lane: CoordinatorLane) {
        self.lanes[lane.index()]
            .dispatch_rejected
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_queue_delay(&self, lane: CoordinatorLane, delay: std::time::Duration) {
        let metrics = &self.lanes[lane.index()];
        let nanos = duration_nanos(delay);
        metrics.queue_delay_samples.fetch_add(1, Ordering::Relaxed);
        metrics
            .queue_delay_total_nanos
            .fetch_add(nanos, Ordering::Relaxed);
        metrics
            .queue_delay_max_nanos
            .fetch_max(nanos, Ordering::Relaxed);
    }

    pub(super) fn record_worker_started(&self, lane: CoordinatorLane) {
        let metrics = &self.lanes[lane.index()];
        let active = metrics.active.fetch_add(1, Ordering::AcqRel) + 1;
        metrics.max_active.fetch_max(active, Ordering::AcqRel);
    }

    pub(super) fn record_worker_finished(
        &self,
        lane: CoordinatorLane,
        duration: std::time::Duration,
        failed: bool,
        panicked: bool,
    ) {
        let metrics = &self.lanes[lane.index()];
        metrics.active.fetch_sub(1, Ordering::AcqRel);
        metrics.completed.fetch_add(1, Ordering::Relaxed);
        if failed {
            metrics.failed.fetch_add(1, Ordering::Relaxed);
        }
        if panicked {
            metrics.panicked.fetch_add(1, Ordering::Relaxed);
        }
        let nanos = duration_nanos(duration);
        metrics.execution_samples.fetch_add(1, Ordering::Relaxed);
        metrics
            .execution_total_nanos
            .fetch_add(nanos, Ordering::Relaxed);
        metrics
            .execution_max_nanos
            .fetch_max(nanos, Ordering::Relaxed);
    }

    pub(super) fn record_worker_loss(&self) {
        self.worker_losses.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_completion_event_dropped(&self) {
        self.completion_events_dropped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_completion_delivery_recovery(&self) {
        self.completion_delivery_recoveries
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_idle_wakeup(&self) {
        self.idle_wakeups.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_configured_workers(&self, count: usize) {
        self.configured_workers.store(count, Ordering::Relaxed);
    }

    pub(super) fn record_worker_joined(&self) {
        self.joined_workers.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_shutdown_recovery(&self) {
        self.shutdown_recoveries.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_worker_loss_delivery_recovery(&self) {
        self.worker_loss_delivery_recoveries
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_worker_loss_event_dropped(&self) {
        self.worker_loss_events_dropped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(in crate::daemon) fn snapshot(&self) -> CoordinatorMetricsSnapshot {
        CoordinatorMetricsSnapshot {
            lanes: std::array::from_fn(|index| self.lane_snapshot(index)),
            events_received: self.events_received.load(Ordering::Relaxed),
            deadlines_scheduled: self.deadlines_scheduled.load(Ordering::Relaxed),
            deadlines_fired: self.deadlines_fired.load(Ordering::Relaxed),
            worker_losses: self.worker_losses.load(Ordering::Relaxed),
            completion_events_dropped: self.completion_events_dropped.load(Ordering::Relaxed),
            completion_delivery_recoveries: self
                .completion_delivery_recoveries
                .load(Ordering::Relaxed),
            idle_wakeups: self.idle_wakeups.load(Ordering::Relaxed),
            configured_workers: self.configured_workers.load(Ordering::Relaxed),
            joined_workers: self.joined_workers.load(Ordering::Relaxed),
            shutdown_recoveries: self.shutdown_recoveries.load(Ordering::Relaxed),
            worker_loss_delivery_recoveries: self
                .worker_loss_delivery_recoveries
                .load(Ordering::Relaxed),
            worker_loss_events_dropped: self.worker_loss_events_dropped.load(Ordering::Relaxed),
        }
    }

    fn lane_snapshot(&self, index: usize) -> CoordinatorLaneMetricsSnapshot {
        let metrics = &self.lanes[index];
        CoordinatorLaneMetricsSnapshot {
            queued: metrics.queued.load(Ordering::Relaxed),
            max_queued: metrics.max_queued.load(Ordering::Relaxed),
            active: metrics.active.load(Ordering::Relaxed),
            max_active: metrics.max_active.load(Ordering::Relaxed),
            completed: metrics.completed.load(Ordering::Relaxed),
            failed: metrics.failed.load(Ordering::Relaxed),
            panicked: metrics.panicked.load(Ordering::Relaxed),
            dispatch_rejected: metrics.dispatch_rejected.load(Ordering::Relaxed),
            queue_delay_samples: metrics.queue_delay_samples.load(Ordering::Relaxed),
            queue_delay_total_nanos: metrics.queue_delay_total_nanos.load(Ordering::Relaxed),
            queue_delay_max_nanos: metrics.queue_delay_max_nanos.load(Ordering::Relaxed),
            execution_samples: metrics.execution_samples.load(Ordering::Relaxed),
            execution_total_nanos: metrics.execution_total_nanos.load(Ordering::Relaxed),
            execution_max_nanos: metrics.execution_max_nanos.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::daemon) struct CoordinatorLaneMetricsSnapshot {
    pub(in crate::daemon) queued: usize,
    pub(in crate::daemon) max_queued: usize,
    pub(in crate::daemon) active: usize,
    pub(in crate::daemon) max_active: usize,
    pub(in crate::daemon) completed: u64,
    pub(in crate::daemon) failed: u64,
    pub(in crate::daemon) panicked: u64,
    pub(in crate::daemon) dispatch_rejected: u64,
    pub(in crate::daemon) queue_delay_samples: u64,
    pub(in crate::daemon) queue_delay_total_nanos: u64,
    pub(in crate::daemon) queue_delay_max_nanos: u64,
    pub(in crate::daemon) execution_samples: u64,
    pub(in crate::daemon) execution_total_nanos: u64,
    pub(in crate::daemon) execution_max_nanos: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct CoordinatorMetricsSnapshot {
    lanes: [CoordinatorLaneMetricsSnapshot; COORDINATOR_LANE_COUNT],
    pub(in crate::daemon) events_received: u64,
    pub(in crate::daemon) deadlines_scheduled: u64,
    pub(in crate::daemon) deadlines_fired: u64,
    pub(in crate::daemon) worker_losses: u64,
    pub(in crate::daemon) completion_events_dropped: u64,
    pub(in crate::daemon) completion_delivery_recoveries: u64,
    pub(in crate::daemon) idle_wakeups: u64,
    pub(in crate::daemon) configured_workers: usize,
    pub(in crate::daemon) joined_workers: usize,
    pub(in crate::daemon) shutdown_recoveries: u64,
    pub(in crate::daemon) worker_loss_delivery_recoveries: u64,
    pub(in crate::daemon) worker_loss_events_dropped: u64,
}

impl CoordinatorMetricsSnapshot {
    pub(in crate::daemon) fn lane(self, lane: CoordinatorLane) -> CoordinatorLaneMetricsSnapshot {
        self.lanes[lane.index()]
    }

    pub(in crate::daemon) fn to_json(self) -> serde_json::Value {
        let lanes = CoordinatorLane::ALL
            .into_iter()
            .map(|lane| {
                let metrics = self.lane(lane);
                (
                    lane.as_str().to_string(),
                    serde_json::json!({
                        "queued": metrics.queued,
                        "maxQueued": metrics.max_queued,
                        "active": metrics.active,
                        "maxActive": metrics.max_active,
                        "completed": metrics.completed,
                        "failed": metrics.failed,
                        "panicked": metrics.panicked,
                        "dispatchRejected": metrics.dispatch_rejected,
                        "queueDelaySamples": metrics.queue_delay_samples,
                        "queueDelayTotalNanos": metrics.queue_delay_total_nanos,
                        "queueDelayMaxNanos": metrics.queue_delay_max_nanos,
                        "executionSamples": metrics.execution_samples,
                        "executionTotalNanos": metrics.execution_total_nanos,
                        "executionMaxNanos": metrics.execution_max_nanos,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({
            "lanes": lanes,
            "eventsReceived": self.events_received,
            "deadlinesScheduled": self.deadlines_scheduled,
            "deadlinesFired": self.deadlines_fired,
            "workerLosses": self.worker_losses,
            "completionEventsDropped": self.completion_events_dropped,
            "completionDeliveryRecoveries": self.completion_delivery_recoveries,
            "idleWakeups": self.idle_wakeups,
            "configuredWorkers": self.configured_workers,
            "joinedWorkers": self.joined_workers,
            "shutdownRecoveries": self.shutdown_recoveries,
            "workerLossDeliveryRecoveries": self.worker_loss_delivery_recoveries,
            "workerLossEventsDropped": self.worker_loss_events_dropped,
        })
    }
}

fn duration_nanos(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}
