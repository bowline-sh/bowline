use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        mpsc::{SyncSender, TrySendError},
    },
};

use super::{
    subscriptions::SharedProjectionState,
    types::{StatusInputEvent, StatusProjectionError, StatusSource},
};

const STATUS_SOURCE_COUNT: usize = 7;

#[derive(Debug, Default)]
pub(crate) struct PendingInputState {
    pub(crate) dirty: BTreeSet<StatusSource>,
    pub(crate) refresh_all: bool,
    pub(crate) shutdown: bool,
}

#[derive(Debug)]
pub(crate) struct PendingInputBatch {
    pub(crate) dirty: BTreeSet<StatusSource>,
    pub(crate) refresh_all: bool,
    pub(crate) shutdown: bool,
}

#[derive(Debug, Clone)]
pub struct StatusProjectionInput {
    pending: Arc<Mutex<PendingInputState>>,
    wake_sender: SyncSender<()>,
    shared: Arc<Mutex<SharedProjectionState>>,
}

impl StatusProjectionInput {
    pub(crate) fn new(
        pending: Arc<Mutex<PendingInputState>>,
        wake_sender: SyncSender<()>,
        shared: Arc<Mutex<SharedProjectionState>>,
    ) -> Self {
        Self {
            pending,
            wake_sender,
            shared,
        }
    }

    pub fn send(&self, event: StatusInputEvent) -> Result<(), StatusProjectionError> {
        let (coalesced, pending_sources) = {
            let mut pending =
                self.pending
                    .lock()
                    .map_err(|_| StatusProjectionError::ChannelClosed {
                        operation: "record input",
                    })?;
            if pending.shutdown {
                return Err(StatusProjectionError::ChannelClosed {
                    operation: "send input after shutdown",
                });
            }
            let coalesced = match event {
                StatusInputEvent::SourceChanged(source) => {
                    pending.refresh_all || !pending.dirty.insert(source)
                }
                StatusInputEvent::RefreshAll => {
                    let already_pending = pending.refresh_all;
                    pending.refresh_all = true;
                    pending.dirty.clear();
                    already_pending
                }
            };
            let pending_sources = if pending.refresh_all {
                STATUS_SOURCE_COUNT
            } else {
                pending.dirty.len()
            };
            (coalesced, pending_sources)
        };
        if let Ok(mut shared) = self.shared.lock() {
            shared.metrics.input_events_received =
                shared.metrics.input_events_received.saturating_add(1);
            if coalesced {
                shared.metrics.input_events_coalesced =
                    shared.metrics.input_events_coalesced.saturating_add(1);
            }
            shared.metrics.max_pending_input_sources = shared
                .metrics
                .max_pending_input_sources
                .max(pending_sources as u64);
        }
        match self.wake_sender.try_send(()) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(())) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.metrics.input_wakes_coalesced =
                        shared.metrics.input_wakes_coalesced.saturating_add(1);
                }
                Ok(())
            }
            Err(TrySendError::Disconnected(())) => Err(StatusProjectionError::ChannelClosed {
                operation: "wake projection worker",
            }),
        }
    }

    pub fn record_rpc_serialization(&self) {
        self.update_metrics(|metrics| {
            metrics.rpc_serializations = metrics.rpc_serializations.saturating_add(1);
        });
    }

    pub fn record_hosted_serialization(&self) {
        self.update_metrics(|metrics| {
            metrics.hosted_serializations = metrics.hosted_serializations.saturating_add(1);
        });
    }

    pub fn record_hosted_publish(&self, success: bool) {
        self.update_metrics(|metrics| {
            metrics.hosted_publish_attempts = metrics.hosted_publish_attempts.saturating_add(1);
            if success {
                metrics.hosted_publish_successes =
                    metrics.hosted_publish_successes.saturating_add(1);
            } else {
                metrics.hosted_publish_failures = metrics.hosted_publish_failures.saturating_add(1);
            }
        });
    }

    pub fn record_notifications(
        &self,
        candidates: usize,
        suppressed: usize,
        sent: usize,
        failures: usize,
    ) {
        self.update_metrics(|metrics| {
            metrics.notification_candidates = metrics
                .notification_candidates
                .saturating_add(candidates as u64);
            metrics.notification_suppressed = metrics
                .notification_suppressed
                .saturating_add(suppressed as u64);
            metrics.notification_sent = metrics.notification_sent.saturating_add(sent as u64);
            metrics.notification_failures = metrics
                .notification_failures
                .saturating_add(failures as u64);
        });
    }

    pub fn record_finder_snapshot(&self, success: bool) {
        self.update_metrics(|metrics| {
            if success {
                metrics.finder_snapshot_writes = metrics.finder_snapshot_writes.saturating_add(1);
            } else {
                metrics.finder_snapshot_failures =
                    metrics.finder_snapshot_failures.saturating_add(1);
            }
        });
    }

    fn update_metrics(&self, update: impl FnOnce(&mut super::types::StatusProjectionMetrics)) {
        if let Ok(mut shared) = self.shared.lock() {
            update(&mut shared.metrics);
        }
    }
}

pub(crate) fn take_pending_input(
    pending: &Arc<Mutex<PendingInputState>>,
) -> Option<PendingInputBatch> {
    let mut pending = pending.lock().ok()?;
    Some(PendingInputBatch {
        dirty: std::mem::take(&mut pending.dirty),
        refresh_all: std::mem::take(&mut pending.refresh_all),
        shutdown: pending.shutdown,
    })
}
