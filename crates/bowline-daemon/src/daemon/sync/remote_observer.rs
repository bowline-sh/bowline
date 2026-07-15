use super::*;

impl ContinuousSyncRuntime {
    pub(in crate::daemon) fn observe_remote_ref_cursor(&mut self) -> bool {
        self.next_remote_observe = Instant::now() + REMOTE_OBSERVER_DRAIN_INTERVAL;
        let workspace_id = self.options.args.workspace_id();
        let observed = match self.remote_ref_observer.observe(self.options.args.clone()) {
            Ok(observed) => observed,
            Err(error) => {
                let state_changed = self.remote_observer_state != RemoteObserverState::Unavailable;
                self.latest_observed_ref = None;
                self.remote_observer_state = RemoteObserverState::Unavailable;
                // Local diagnostics keep the raw error; the external status
                // payload gets only a fixed, path-free code.
                if state_changed {
                    eprintln!("bowline-daemon remote ref observe failed: {error}");
                }
                self.record_component_states(
                    SyncComponentState::Idle,
                    self.watcher_component_state(),
                    "degraded",
                );
                self.last_json = self.remote_observer_failure_status_json();
                return false;
            }
        };
        let recovered = self.remote_observer_state == RemoteObserverState::Unavailable;
        self.remote_observer_state = RemoteObserverState::Ready;
        if recovered {
            self.record_component_states(
                SyncComponentState::Ready,
                self.watcher_component_state(),
                "online",
            );
        }
        let Some(remote_ref) = observed else {
            self.latest_observed_ref = None;
            return false;
        };
        self.latest_observed_ref = Some(remote_ref.clone());
        self.metadata_store_for_write("metadata_store(observe_remote_ref_cursor)", |store| {
            self.store_health.record(
                "put_remote_ref_cursor(observed_ref)",
                store.put_remote_ref_cursor(&RemoteRefCursorRecord {
                    workspace_id: workspace_id.clone(),
                    cursor: None,
                    last_observed_version: Some(remote_ref.version),
                    last_observed_snapshot_id: Some(remote_ref.snapshot_id.into()),
                    updated_at: current_timestamp(),
                }),
            );
            Ok(remote_cursor_ahead_of_local_head(store, &workspace_id))
        })
        .unwrap_or(false)
    }

    pub(in crate::daemon) fn remote_observer_is_unavailable(&self) -> bool {
        self.remote_observer_state == RemoteObserverState::Unavailable
    }

    pub(super) fn remote_observer_failure_status_json(&self) -> String {
        daemon_json(&RemoteObserverErrorStatusJson {
            state: "limited",
            tick_count: self.tick_count,
            unavailable_because: SyncExternalFailureCode::ControlPlaneUnavailable.as_code(),
            next_action: "check network or hosted auth",
            queue: SyncOperationCountsJson::from(&self.queue_counts()),
            local_head: self.local_head_payload(),
            remote_head: self.remote_head_payload(),
        })
    }
}
