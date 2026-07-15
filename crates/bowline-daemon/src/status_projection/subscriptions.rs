use super::{
    delivery::{LatestProjectionSender, Projection},
    types::StatusProjectionMetrics,
};

#[derive(Debug)]
pub(crate) struct SharedProjectionState {
    pub(crate) current: Option<Projection>,
    pub(crate) projection_subscribers: Vec<LatestProjectionSender>,
    pub(crate) heartbeat_subscribers: Vec<LatestProjectionSender>,
    pub(crate) metrics: StatusProjectionMetrics,
}

impl SharedProjectionState {
    pub(crate) fn new() -> Self {
        Self {
            current: None,
            projection_subscribers: Vec::new(),
            heartbeat_subscribers: Vec::new(),
            metrics: StatusProjectionMetrics::default(),
        }
    }

    pub(crate) fn prune_disconnected_subscribers(&mut self) {
        let projection_before = self.projection_subscribers.len();
        self.projection_subscribers
            .retain(LatestProjectionSender::is_connected);
        let heartbeat_before = self.heartbeat_subscribers.len();
        self.heartbeat_subscribers
            .retain(LatestProjectionSender::is_connected);
        let projection_removed =
            projection_before.saturating_sub(self.projection_subscribers.len());
        let heartbeat_removed = heartbeat_before.saturating_sub(self.heartbeat_subscribers.len());
        self.metrics.projection_subscribers_disconnected = self
            .metrics
            .projection_subscribers_disconnected
            .saturating_add(projection_removed as u64);
        self.metrics.heartbeat_subscribers_disconnected = self
            .metrics
            .heartbeat_subscribers_disconnected
            .saturating_add(heartbeat_removed as u64);
        self.update_subscriber_gauges();
    }

    pub(crate) fn update_subscriber_gauges(&mut self) {
        self.metrics.projection_subscribers_active = self.projection_subscribers.len() as u64;
        self.metrics.heartbeat_subscribers_active = self.heartbeat_subscribers.len() as u64;
    }
}
