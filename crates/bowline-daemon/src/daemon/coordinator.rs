mod clock;
mod lanes;
mod metrics;

#[cfg(test)]
mod tests;

use std::{cmp::Reverse, collections::BinaryHeap, fmt, sync::Arc, time::Duration};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};

use bowline_daemon::status_projection::StatusInputEvent;

pub(super) use clock::{CoordinatorClock, CoordinatorInstant, SystemCoordinatorClock};
#[cfg(test)]
pub(super) use lanes::CoordinatorSubmitErrorKind;
pub(super) use lanes::{
    CoordinatorExecutor, CoordinatorExecutorConfig, CoordinatorJob, CoordinatorWorkFailure,
};
pub(super) use metrics::{CoordinatorMetrics, CoordinatorMetricsSnapshot};

pub(super) const COORDINATOR_EVENT_CAPACITY: usize = 1_024;

pub(super) const COORDINATOR_LANE_COUNT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum CoordinatorLane {
    ControlPlane,
    Notification,
}

impl CoordinatorLane {
    pub(super) const ALL: [Self; COORDINATOR_LANE_COUNT] = [Self::ControlPlane, Self::Notification];

    pub(super) const fn index(self) -> usize {
        match self {
            Self::ControlPlane => 0,
            Self::Notification => 1,
        }
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ControlPlane => "control-plane",
            Self::Notification => "notification",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct CoordinatorJobId(String);

impl CoordinatorJobId {
    pub(super) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CoordinatorDeadlineKind {
    /// A pending manifest-engine rebuild is due for another build attempt.
    EngineRetry(CoordinatorJobId),
    HostedRefresh,
    StatusRefresh,
    NotificationPoll,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct CoordinatorDeadline {
    pub(super) due: CoordinatorInstant,
    pub(super) kind: CoordinatorDeadlineKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CoordinatorWorkerCompletion {
    pub(super) job_id: CoordinatorJobId,
    pub(super) lane: CoordinatorLane,
    pub(super) outcome: CoordinatorWorkerOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CoordinatorWorkerOutcome {
    Succeeded,
    Failed(CoordinatorWorkFailure),
    Panicked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CoordinatorWorkerLoss {
    pub(super) lane: CoordinatorLane,
    pub(super) worker_index: usize,
    pub(super) active_job_id: Option<CoordinatorJobId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CoordinatorEvent {
    EngineWorkAvailable,
    StatusInput(StatusInputEvent),
    ProjectionReady,
    WorkerCompleted(CoordinatorWorkerCompletion),
    WorkerLost(CoordinatorWorkerLoss),
    ScheduleDeadline(CoordinatorDeadline),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CoordinatorAction {
    DriveEngine,
    ForwardStatusInput(StatusInputEvent),
    PublishProjection,
    WorkerCompleted(CoordinatorWorkerCompletion),
    WorkerLost(CoordinatorWorkerLoss),
    DeadlineDue(CoordinatorDeadlineKind),
    Shutdown,
}

#[derive(Clone)]
pub(super) struct CoordinatorHandle {
    sender: Sender<CoordinatorEvent>,
}

impl fmt::Debug for CoordinatorHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoordinatorHandle")
            .finish_non_exhaustive()
    }
}

impl CoordinatorHandle {
    pub(super) fn try_send(
        &self,
        event: CoordinatorEvent,
    ) -> Result<(), CoordinatorEventSendError> {
        self.sender.try_send(event).map_err(|error| match error {
            TrySendError::Full(event) => CoordinatorEventSendError {
                kind: CoordinatorEventSendErrorKind::Full,
                event,
            },
            TrySendError::Disconnected(event) => CoordinatorEventSendError {
                kind: CoordinatorEventSendErrorKind::Disconnected,
                event,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CoordinatorEventSendErrorKind {
    Full,
    Disconnected,
}

#[derive(Debug)]
pub(super) struct CoordinatorEventSendError {
    pub(super) kind: CoordinatorEventSendErrorKind,
    pub(super) event: CoordinatorEvent,
}

impl fmt::Display for CoordinatorEventSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "coordinator event send failed ({:?}): {:?}",
            self.kind, self.event
        )
    }
}

impl std::error::Error for CoordinatorEventSendError {}

pub(super) fn coordinator_channel(
    capacity: usize,
) -> (CoordinatorHandle, Receiver<CoordinatorEvent>) {
    let (sender, receiver) = bounded(capacity);
    (CoordinatorHandle { sender }, receiver)
}

pub(super) struct CoordinatorState<C> {
    clock: C,
    deadlines: BinaryHeap<Reverse<CoordinatorDeadline>>,
    metrics: Arc<CoordinatorMetrics>,
    shutting_down: bool,
}

impl<C: CoordinatorClock> CoordinatorState<C> {
    pub(super) fn new(clock: C, metrics: Arc<CoordinatorMetrics>) -> Self {
        Self {
            clock,
            deadlines: BinaryHeap::new(),
            metrics,
            shutting_down: false,
        }
    }

    pub(super) fn handle_event(&mut self, event: CoordinatorEvent) -> Vec<CoordinatorAction> {
        self.metrics.record_event();
        match event {
            CoordinatorEvent::EngineWorkAvailable => {
                vec![CoordinatorAction::DriveEngine]
            }
            CoordinatorEvent::StatusInput(input) => {
                vec![CoordinatorAction::ForwardStatusInput(input)]
            }
            CoordinatorEvent::ProjectionReady => vec![CoordinatorAction::PublishProjection],
            CoordinatorEvent::WorkerCompleted(completion) => {
                vec![CoordinatorAction::WorkerCompleted(completion)]
            }
            CoordinatorEvent::WorkerLost(loss) => vec![CoordinatorAction::WorkerLost(loss)],
            CoordinatorEvent::ScheduleDeadline(deadline) => {
                self.deadlines.push(Reverse(deadline));
                self.metrics.record_deadline_scheduled();
                Vec::new()
            }
            CoordinatorEvent::Shutdown => {
                self.shutting_down = true;
                vec![CoordinatorAction::Shutdown]
            }
        }
    }

    pub(super) fn process_due_deadlines(&mut self) -> Vec<CoordinatorAction> {
        let now = self.clock.now();
        let mut actions = Vec::new();
        while self
            .deadlines
            .peek()
            .is_some_and(|Reverse(deadline)| deadline.due <= now)
        {
            let Some(Reverse(deadline)) = self.deadlines.pop() else {
                break;
            };
            self.metrics.record_deadline_fired();
            actions.push(CoordinatorAction::DeadlineDue(deadline.kind));
        }
        actions
    }

    pub(super) fn next_wait(&self) -> Option<Duration> {
        self.deadlines
            .peek()
            .map(|Reverse(deadline)| deadline.due.saturating_duration_since(self.clock.now()))
    }

    pub(super) fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }
}

pub(super) struct CoordinatorDriver<C> {
    state: CoordinatorState<C>,
    receiver: Receiver<CoordinatorEvent>,
}

impl<C: CoordinatorClock> CoordinatorDriver<C> {
    pub(super) fn new(state: CoordinatorState<C>, receiver: Receiver<CoordinatorEvent>) -> Self {
        Self { state, receiver }
    }

    pub(super) fn run_turn(&mut self) -> Result<Vec<CoordinatorAction>, CoordinatorDisconnected> {
        let due = self.state.process_due_deadlines();
        if !due.is_empty() {
            return Ok(due);
        }
        let event = match self.state.next_wait() {
            Some(timeout) => match self.receiver.recv_timeout(timeout) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    let due = self.state.process_due_deadlines();
                    if due.is_empty() {
                        self.state.metrics.record_idle_wakeup();
                    }
                    return Ok(due);
                }
                Err(RecvTimeoutError::Disconnected) => return Err(CoordinatorDisconnected),
            },
            None => self.receiver.recv().map_err(|_| CoordinatorDisconnected)?,
        };
        let mut actions = self.state.handle_event(event);
        actions.extend(self.state.process_due_deadlines());
        Ok(actions)
    }

    pub(super) fn state(&self) -> &CoordinatorState<C> {
        &self.state
    }

    pub(super) fn state_mut(&mut self) -> &mut CoordinatorState<C> {
        &mut self.state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CoordinatorDisconnected;
