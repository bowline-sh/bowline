use std::collections::{HashMap, VecDeque};

use super::{
    super::request_context::{CancellationToken, RpcConnectionId},
    RpcExecutorConfig, RpcJob, RpcLane, SubmissionError,
};

#[derive(Default)]
pub(super) struct FairLaneQueue {
    by_connection: HashMap<RpcConnectionId, VecDeque<RpcJob>>,
    ready_connections: VecDeque<RpcConnectionId>,
    pub(super) len: usize,
}

impl FairLaneQueue {
    pub(super) fn push(&mut self, job: RpcJob) {
        let connection_id = job.connection_id;
        let queue = self.by_connection.entry(connection_id).or_default();
        if queue.is_empty() {
            self.ready_connections.push_back(connection_id);
        }
        queue.push_back(job);
        self.len += 1;
    }

    pub(super) fn pop(&mut self) -> Option<RpcJob> {
        let connection_id = self.ready_connections.pop_front()?;
        let queue = self.by_connection.get_mut(&connection_id)?;
        let job = queue.pop_front()?;
        self.len = self.len.saturating_sub(1);
        if queue.is_empty() {
            self.by_connection.remove(&connection_id);
        } else {
            self.ready_connections.push_back(connection_id);
        }
        Some(job)
    }

    pub(super) fn remove_connection(&mut self, connection_id: RpcConnectionId) -> Vec<RpcJob> {
        self.ready_connections
            .retain(|queued_connection| *queued_connection != connection_id);
        let removed = self
            .by_connection
            .remove(&connection_id)
            .map(VecDeque::into_iter)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        self.len = self.len.saturating_sub(removed.len());
        removed
    }

    pub(super) fn remove_request(
        &mut self,
        connection_id: RpcConnectionId,
        cancellation: &CancellationToken,
    ) -> Option<RpcJob> {
        let queue = self.by_connection.get_mut(&connection_id)?;
        let position = queue
            .iter()
            .position(|job| job.context.cancellation().same_request(cancellation))?;
        let removed = queue.remove(position)?;
        self.len = self.len.saturating_sub(1);
        if queue.is_empty() {
            self.by_connection.remove(&connection_id);
            self.ready_connections
                .retain(|queued_connection| *queued_connection != connection_id);
        }
        Some(removed)
    }
}

#[derive(Default)]
pub(super) struct QueueState {
    pub(super) status: FairLaneQueue,
    pub(super) query: FairLaneQueue,
    pub(super) mutation: FairLaneQueue,
    pub(super) queued_per_connection: HashMap<RpcConnectionId, usize>,
    pub(super) queued_total: usize,
    pub(super) shutting_down: bool,
}

impl QueueState {
    pub(super) fn lane(&self, lane: RpcLane) -> &FairLaneQueue {
        match lane {
            RpcLane::Status => &self.status,
            RpcLane::Query => &self.query,
            RpcLane::Mutation => &self.mutation,
        }
    }

    pub(super) fn lane_mut(&mut self, lane: RpcLane) -> &mut FairLaneQueue {
        match lane {
            RpcLane::Status => &mut self.status,
            RpcLane::Query => &mut self.query,
            RpcLane::Mutation => &mut self.mutation,
        }
    }
}

pub(super) fn validate_capacity(
    state: &QueueState,
    config: RpcExecutorConfig,
    connection_id: RpcConnectionId,
    lane: RpcLane,
) -> Result<(), SubmissionError> {
    if state.shutting_down {
        return Err(SubmissionError::ShuttingDown);
    }
    if state.queued_total >= config.global_queue_capacity {
        return Err(SubmissionError::GlobalQueueFull);
    }
    if state
        .queued_per_connection
        .get(&connection_id)
        .copied()
        .unwrap_or_default()
        >= config.per_connection_queue_capacity
    {
        return Err(SubmissionError::ConnectionQueueFull);
    }
    let lane_capacity = match lane {
        RpcLane::Status => config.status_queue_capacity,
        RpcLane::Query => config.query_queue_capacity,
        RpcLane::Mutation => config.mutation_queue_capacity,
    };
    if state.lane(lane).len >= lane_capacity {
        return Err(SubmissionError::LaneQueueFull(lane));
    }
    Ok(())
}
