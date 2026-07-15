use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::daemon) enum SyncComponentState {
    Ready,
    Degraded,
    Idle,
}

impl SyncComponentState {
    pub(in crate::daemon) fn as_wire(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Idle => "idle",
        }
    }
}

impl ContinuousSyncRuntime {
    pub(in crate::daemon) fn record_component_states(
        &self,
        sync: SyncComponentState,
        watcher: &str,
        network: &str,
    ) {
        self.metadata_store_for_write("metadata_store(record_component_states)", |store| {
            let now = current_timestamp();
            let sync = self.sync_component_state_from_store(store, sync);
            let network = self.visible_network_state(network);
            if self
                .store_health
                .record(
                    "set_component_state(sync)",
                    store.set_component_state("sync", sync.as_wire(), &now),
                )
                .is_some()
                && sync == SyncComponentState::Degraded
            {
                // The sync write itself must not recover store health: only
                // the next successful store write after degraded is visible may clear it.
                self.store_health.mark_degraded_status_written();
            }
            self.store_health.record(
                "set_component_state(watcher)",
                store.set_component_state("watcher", watcher, &now),
            );
            self.store_health.record(
                "set_component_state(network)",
                store.set_component_state("network", network, &now),
            );
            Ok(())
        });
    }

    fn sync_component_state_from_store(
        &self,
        store: &MetadataStore,
        sync: SyncComponentState,
    ) -> SyncComponentState {
        if self.remote_observer_is_unavailable() || self.store_health.is_degraded() {
            return SyncComponentState::Degraded;
        }
        if sync == SyncComponentState::Ready
            && self
                .store_health
                .record(
                    "post_commit_sync_state(sync_component_state)",
                    store.has_degraded_post_commit_sync(),
                )
                .unwrap_or(true)
        {
            return SyncComponentState::Degraded;
        }
        sync
    }

    pub(super) fn visible_network_state<'a>(&self, network: &'a str) -> &'a str {
        if self.remote_observer_is_unavailable() && network == "online" {
            return "degraded";
        }
        network
    }
}
