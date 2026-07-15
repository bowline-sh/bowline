use super::*;

impl SchedulerCoordinator {
    pub(super) fn submit_notification(&mut self, executor: &CoordinatorExecutor) {
        if self.notification_in_flight.is_some() {
            return;
        }
        let notification = self
            .runtime
            .lock()
            .ok()
            .and_then(|mut runtime| runtime.prepare_notification_poll());
        let Some(notification) = notification else {
            return;
        };
        let result_fallback = self.loss_fallback_tx.clone();
        let completion_fallback = self.loss_fallback_tx.clone();
        let loss_fallback = self.loss_fallback_tx.clone();
        let notification_state = Arc::clone(&self.state);
        let job_id = self.next_side_lane_job_id("notification-poll");
        let result_job_id = job_id.clone();
        let completion_job_id = job_id.clone();
        let loss_job_id = job_id.clone();
        let job = CoordinatorJob::new(
            job_id.clone(),
            CoordinatorLane::Notification,
            None,
            move || {
                let completion =
                    notification.execute_cancellable(|| !notification_state.cancels_side_work());
                let _coordinator_gone =
                    result_fallback.send(SchedulerFallback::NotificationCompleted {
                        job_id: result_job_id,
                        completion: Box::new(completion),
                    });
                Ok(())
            },
        )
        .on_completion_delivery_failure(move |_| {
            let _coordinator_gone = completion_fallback.send(
                SchedulerFallback::NotificationWorkerLost(completion_job_id.clone()),
            );
        })
        .on_worker_loss_delivery_failure(move |_| {
            let _coordinator_gone = loss_fallback.send(SchedulerFallback::NotificationWorkerLost(
                loss_job_id.clone(),
            ));
        });
        if executor.submit(job).is_ok() {
            self.notification_in_flight = Some(job_id);
        }
    }

    pub(super) fn submit_trust_refresh(&mut self, executor: &CoordinatorExecutor) {
        if self.trust_refresh_in_flight {
            return;
        }
        let state = Arc::clone(&self.state);
        let completion_fallback = self.loss_fallback_tx.clone();
        let loss_fallback = self.loss_fallback_tx.clone();
        let job = CoordinatorJob::new(
            CoordinatorJobId::new("trust-refresh"),
            CoordinatorLane::ControlPlane,
            None,
            move || {
                state.refresh_device_trust_if_due();
                Ok(())
            },
        )
        .on_completion_delivery_failure(move |_| {
            let _coordinator_gone =
                completion_fallback.send(SchedulerFallback::TrustRefreshCompleted);
        })
        .on_worker_loss_delivery_failure(move |_| {
            let _coordinator_gone = loss_fallback.send(SchedulerFallback::TrustRefreshCompleted);
        });
        if executor.submit(job).is_ok() {
            self.trust_refresh_in_flight = true;
        }
    }

    pub(super) fn publish_status(&mut self, executor: &CoordinatorExecutor) {
        let projection_poll = self.state.prepare_projection_adapter_poll();
        let prepared = self.runtime.lock().ok().and_then(|mut runtime| {
            self.state.poll_projection_adapters(
                &mut runtime,
                self.status_publish_in_flight.is_none(),
                projection_poll,
            )
        });
        let Some(prepared) = prepared else {
            return;
        };
        let result_fallback = self.loss_fallback_tx.clone();
        let completion_fallback = self.loss_fallback_tx.clone();
        let loss_fallback = self.loss_fallback_tx.clone();
        let publish_state = Arc::clone(&self.state);
        let job_id = self.next_side_lane_job_id("status-publish");
        let result_job_id = job_id.clone();
        let completion_job_id = job_id.clone();
        let loss_job_id = job_id.clone();
        let job = CoordinatorJob::new(
            job_id.clone(),
            CoordinatorLane::ControlPlane,
            None,
            move || {
                let completion =
                    prepared.execute_cancellable(|| !publish_state.cancels_side_work());
                let _coordinator_gone =
                    result_fallback.send(SchedulerFallback::StatusPublishCompleted {
                        job_id: result_job_id,
                        completion,
                    });
                Ok(())
            },
        )
        .on_completion_delivery_failure(move |_| {
            let _coordinator_gone = completion_fallback.send(
                SchedulerFallback::StatusPublishWorkerLost(completion_job_id.clone()),
            );
        })
        .on_worker_loss_delivery_failure(move |_| {
            let _coordinator_gone = loss_fallback.send(SchedulerFallback::StatusPublishWorkerLost(
                loss_job_id.clone(),
            ));
        });
        if executor.submit(job).is_ok() {
            self.status_publish_in_flight = Some(job_id);
        }
    }

    fn next_side_lane_job_id(&mut self, prefix: &str) -> CoordinatorJobId {
        self.side_lane_sequence = self.side_lane_sequence.wrapping_add(1);
        CoordinatorJobId::new(format!("{prefix}-{}", self.side_lane_sequence))
    }
}
