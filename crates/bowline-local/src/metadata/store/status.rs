use super::*;

impl MetadataStore {
    pub fn set_component_state(
        &self,
        component: &str,
        state: &str,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT OR REPLACE INTO component_states (component, state, updated_at)
             VALUES (?1, ?2, ?3)",
            params![component, state, updated_at],
        )?;
        Ok(())
    }

    pub fn event_watermarks(&self) -> Result<EventWatermarks, MetadataError> {
        let last_event_id = self
            .connection
            .query_row(
                "SELECT id FROM events ORDER BY occurred_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        let mut sync_state = self.component_state("sync")?;
        if self.has_degraded_post_commit_sync()? {
            sync_state = Some(ComponentState::Degraded);
        }
        let watcher_state = self.component_state("watcher")?;
        let network_state = match self
            .component_state_raw("network")?
            .as_deref()
            .unwrap_or("online")
        {
            "offline" => Some(NetworkState::Offline),
            "degraded" => Some(NetworkState::Degraded),
            "online" => Some(NetworkState::Online),
            _ => None,
        };

        Ok(EventWatermarks {
            last_scan_at: self.last_scan_at()?,
            last_event_id: last_event_id.map(bowline_core::ids::EventId::new),
            event_lag_ms: Some(0),
            sync_state,
            watcher_state,
            network_state,
        })
    }

    pub fn has_degraded_post_commit_sync(&self) -> Result<bool, MetadataError> {
        Ok(PostCommitSyncComponent::ALL
            .iter()
            .map(|component| self.post_commit_component_state(*component))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|state| state == Some(ComponentState::Degraded)))
    }

    pub fn post_commit_component_state(
        &self,
        component: PostCommitSyncComponent,
    ) -> Result<Option<ComponentState>, MetadataError> {
        self.component_state(component.as_str())
    }

    fn component_state(&self, component: &str) -> Result<Option<ComponentState>, MetadataError> {
        Ok(match self.component_state_raw(component)?.as_deref() {
            Some("ready") => Some(ComponentState::Ready),
            Some("degraded") => Some(ComponentState::Degraded),
            Some("unavailable") => Some(ComponentState::Unavailable),
            _ => None,
        })
    }

    fn component_state_raw(&self, component: &str) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT state FROM component_states WHERE component = ?1",
                [component],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn last_scan_at(&self) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT watermark FROM indexes
                 WHERE kind = 'scan-summary'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(Into::into)
    }
}
