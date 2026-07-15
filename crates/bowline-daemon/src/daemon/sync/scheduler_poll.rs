use super::*;

impl ContinuousSyncRuntime {
    #[cfg(test)]
    pub(in crate::daemon) fn poll(&mut self) {
        let Some(work) = self.poll_prepare() else {
            return;
        };
        self.apply_worker_completion(work.execute_caught());
    }

    #[cfg(test)]
    pub(in crate::daemon) fn poll_prepare(&mut self) -> Option<PreparedDaemonWork> {
        self.poll_prepare_with_preference(true)
    }

    pub(in crate::daemon) fn poll_prepare_with_preference(
        &mut self,
        prefer_work_view_accept: bool,
    ) -> Option<PreparedDaemonWork> {
        if prefer_work_view_accept {
            self.prepare_work_view_accept()
                .or_else(|| self.poll_prepare_reconcile())
        } else {
            self.poll_prepare_reconcile()
                .or_else(|| self.prepare_work_view_accept())
        }
    }

    fn poll_prepare_reconcile(&mut self) -> Option<PreparedDaemonWork> {
        let now = Instant::now();
        self.requeue_expired_sync_claims();
        if let Some(claimed_operation) = self.claim_ready_reconcile_sync_operation() {
            self.tick_count += 1;
            self.next_tick = now + self.options.interval;
            return Some(self.prepare_claimed_operation(claimed_operation));
        }
        if self.maybe_rearm_watcher(now) {
            self.record_component_states(
                SyncComponentState::Ready,
                self.watcher_component_state(),
                "online",
            );
            self.last_json = self.waiting_for_sync_queue_json();
        }
        let watcher_drain = self.drain_changes();
        let settling_local_change = watcher_drain.changed && !watcher_drain.sync_now;
        if watcher_drain.sync_now {
            self.next_tick = now;
        } else if watcher_drain.changed {
            self.next_tick = now + WATCHER_SETTLE_WINDOW;
        }
        let remote_observe_due = now >= self.next_remote_observe;
        if now < self.next_tick && !remote_observe_due {
            return None;
        }
        if self
            .options
            .max_ticks
            .is_some_and(|max_ticks| self.tick_count >= max_ticks)
        {
            self.next_tick = now + self.options.interval;
            return None;
        }
        if remote_observe_due && self.observe_remote_ref_cursor() {
            self.next_tick = now;
        }
        if settling_local_change {
            return None;
        }
        if Instant::now() < self.next_tick {
            return None;
        }

        self.tick_count += 1;
        self.sweep_local_metadata_if_due();
        let Some(claimed_operation) = self.claim_daemon_reconcile_operation() else {
            if let Err(error) = self.claim_pending_dispatch_lease_if_due(false) {
                eprintln!("bowline-daemon dispatch claim failed: {error}");
            }
            self.record_component_states(
                SyncComponentState::Ready,
                self.watcher_component_state(),
                "online",
            );
            self.last_json = self.waiting_for_sync_queue_json();
            self.next_tick = Instant::now() + self.options.interval;
            return None;
        };
        let work = self.prepare_claimed_operation(claimed_operation);
        self.next_tick = Instant::now() + self.options.interval;
        Some(work)
    }
}
