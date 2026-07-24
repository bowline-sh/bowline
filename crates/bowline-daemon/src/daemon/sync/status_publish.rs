use std::time::Instant;

use crate::daemon::{
    STATUS_PUBLISH_INTERVAL, STATUS_PUBLISH_KEEPALIVE_INTERVAL, StatusPublishOutcome,
    StatusPublishPayload, StatusPublishRequest,
};
use bowline_daemon::status_projection::DaemonStatusProjection;
use bowline_daemon::status_projection::StatusProjectionInput;

use super::{ContinuousSyncRuntime, PreparedStatusPublish, StatusPublishCompletion};

impl ContinuousSyncRuntime {
    #[cfg(test)]
    pub(in crate::daemon) fn publish_projection_status(
        &mut self,
        projection: &DaemonStatusProjection,
        heartbeat: bool,
        now: Instant,
        projection_input: &StatusProjectionInput,
    ) {
        if let Some(prepared) =
            self.prepare_projection_status(projection, heartbeat, now, projection_input)
        {
            self.complete_status_publish(prepared.execute(), projection_input);
        }
    }

    #[cfg(test)]
    pub(in crate::daemon) fn retry_projection_status_if_due(
        &mut self,
        projection: &DaemonStatusProjection,
        now: Instant,
        projection_input: &StatusProjectionInput,
    ) {
        if let Some(prepared) =
            self.prepare_projection_status_retry_if_due(projection, now, projection_input)
        {
            self.complete_status_publish(prepared.execute(), projection_input);
        }
    }

    pub(in crate::daemon) fn prepare_projection_status(
        &mut self,
        projection: &DaemonStatusProjection,
        heartbeat: bool,
        now: Instant,
        projection_input: &StatusProjectionInput,
    ) -> Option<PreparedStatusPublish> {
        let request = StatusPublishRequest {
            args: self.args.clone(),
        };
        let Ok(payload) = StatusPublishPayload::from_projection(request, projection) else {
            eprintln!("bowline-daemon status projection serialization failed");
            self.next_status_publish = now + STATUS_PUBLISH_INTERVAL;
            return None;
        };
        projection_input.record_hosted_serialization();
        let Some(fingerprint) = payload.fingerprint.as_deref() else {
            eprintln!("bowline-daemon status projection fingerprint is missing");
            self.next_status_publish = now + STATUS_PUBLISH_INTERVAL;
            return None;
        };
        if self.should_skip_publish(now, fingerprint, heartbeat) {
            if self.last_status_publish_failed_at.is_none() {
                self.next_status_publish = now + STATUS_PUBLISH_INTERVAL;
            }
            return None;
        }
        Some(PreparedStatusPublish {
            payload,
            published_at: now,
            publisher: self.status_publisher.clone(),
        })
    }

    pub(in crate::daemon) fn prepare_projection_status_retry_if_due(
        &mut self,
        projection: &DaemonStatusProjection,
        now: Instant,
        projection_input: &StatusProjectionInput,
    ) -> Option<PreparedStatusPublish> {
        if now < self.next_status_publish {
            return None;
        }
        self.prepare_projection_status(projection, true, now, projection_input)
    }

    fn should_skip_publish(&self, now: Instant, fingerprint: &str, heartbeat: bool) -> bool {
        if self
            .last_status_publish_failed_at
            .is_some_and(|_| now < self.next_status_publish)
        {
            return true;
        }
        if self.last_status_publish_fingerprint.as_deref() != Some(fingerprint) {
            return false;
        }
        !heartbeat
            || self.last_status_publish_at.is_some_and(|published_at| {
                now.duration_since(published_at) < STATUS_PUBLISH_KEEPALIVE_INTERVAL
            })
    }

    pub(in crate::daemon) fn complete_status_publish(
        &mut self,
        completion: StatusPublishCompletion,
        projection_input: &StatusProjectionInput,
    ) {
        match completion.result {
            Ok(outcome) => {
                projection_input.record_hosted_publish(true);
                self.last_status_publish_failed_at = None;
                self.record_status_publish_outcome(outcome, completion.published_at);
            }
            Err(error) => {
                projection_input.record_hosted_publish(false);
                self.last_status_publish_failed_at = Some(completion.published_at);
                eprintln!("bowline-daemon status publish skipped: {error}");
            }
        }
        self.next_status_publish = completion.published_at + STATUS_PUBLISH_INTERVAL;
    }

    fn record_status_publish_outcome(
        &mut self,
        outcome: StatusPublishOutcome,
        published_at: Instant,
    ) {
        self.last_status_publish_fingerprint = Some(outcome.fingerprint);
        self.last_status_publish_at = Some(published_at);
    }
}

impl PreparedStatusPublish {
    #[cfg(test)]
    pub(in crate::daemon) fn execute(self) -> StatusPublishCompletion {
        self.execute_cancellable(|| true)
    }

    pub(in crate::daemon) fn execute_cancellable(
        self,
        mut checkpoint: impl FnMut() -> bool,
    ) -> StatusPublishCompletion {
        StatusPublishCompletion {
            published_at: self.published_at,
            result: if checkpoint() {
                self.publisher
                    .publish(self.payload)
                    .map_err(|error| error.to_string())
            } else {
                Err("status publish cancelled during daemon shutdown".to_string())
            },
        }
    }
}
