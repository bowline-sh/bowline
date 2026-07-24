use super::*;

impl MetadataStore {
    pub fn event_watermarks(&self) -> Result<EventWatermarks, MetadataError> {
        let last_event_id = self
            .connection
            .query_row(
                "SELECT id FROM events ORDER BY occurred_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(EventWatermarks {
            last_scan_at: self.last_scan_at()?,
            last_event_id: last_event_id.map(bowline_core::ids::EventId::new),
            event_lag_ms: Some(0),
        })
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
