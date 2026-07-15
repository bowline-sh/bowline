use super::*;

impl DaemonRuntime {
    pub(in crate::daemon) fn poll_notifications_for_projection(
        &mut self,
        status: &bowline_core::commands::StatusCommandOutput,
        _projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) {
        if !self.notify_approvals {
            return;
        }
        self.pending_notification_status = Some(status.clone());
        self.next_notification_poll = Instant::now();
    }

    #[cfg(test)]
    pub(super) fn poll_notifications_with<S>(
        &mut self,
        sender: &S,
    ) -> Result<NotificationDispatchReport, String>
    where
        S: NotificationSender,
    {
        if !self.notify_approvals {
            return Ok(NotificationDispatchReport::default());
        }
        let Some(status) = self.pending_notification_status.as_ref() else {
            return Ok(NotificationDispatchReport::default());
        };
        let payloads = pending_device_payloads(status);
        let mut dedupe = self
            .notification_dedupe
            .lock()
            .map_err(|_| "notification dedupe state is unavailable".to_string())?;
        Ok(dispatch_new_notifications(&payloads, &mut dedupe, sender))
    }

    pub(in crate::daemon) fn complete_notification_poll(
        &mut self,
        completion: NotificationPollCompletion,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) {
        match completion.result {
            Ok(report) => {
                let failure_count = report.failures.len();
                projection_input.record_notifications(
                    report.attempted,
                    report.skipped,
                    report.sent,
                    failure_count,
                );
                for failure in report.failures {
                    eprintln!(
                        "bowline-daemon notification failed for {}: {}",
                        failure.title, failure.message
                    );
                }
                if failure_count == 0
                    && self.pending_notification_status.as_ref() == Some(&completion.status)
                {
                    self.pending_notification_status = None;
                }
            }
            Err(error) => {
                eprintln!("bowline-daemon notifications unavailable: {error}");
            }
        }
    }

    #[cfg(test)]
    pub(in crate::daemon) fn dispatch_projection_notifications_with<S>(
        &mut self,
        status: &bowline_core::commands::StatusCommandOutput,
        sender: &S,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) -> NotificationDispatchReport
    where
        S: NotificationSender,
    {
        self.pending_notification_status = Some(status.clone());
        let report = self
            .poll_notifications_with(sender)
            .expect("projection notification dispatch");
        projection_input.record_notifications(
            report.attempted,
            report.skipped,
            report.sent,
            report.failures.len(),
        );
        if report.failures.is_empty() {
            self.pending_notification_status = None;
        }
        report
    }
}
