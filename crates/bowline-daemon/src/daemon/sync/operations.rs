use super::*;
use bowline_core::hosted::EMPTY_SNAPSHOT_ID;
use bowline_core::retry::BOUNDED_SYNC_RETRY_POLICY;
use bowline_local::metadata::SyncOperationKind;

const SYNC_SUMMARY_NONE_LABEL: &str = "none";

pub(in crate::daemon) fn requeue_startup_sync_claims(options: &ContinuousSyncOptions) {
    let workspace_id = options.args.workspace_id();
    let workspace_key_available = key_store()
        .and_then(|store| store.load_workspace_key(&workspace_id))
        .ok()
        .flatten()
        .is_some();
    requeue_startup_sync_claims_with_resolved_attention(
        options,
        require_convex_url().is_ok(),
        workspace_key_available,
    );
}

pub(in crate::daemon) fn requeue_startup_sync_claims_with_resolved_attention(
    options: &ContinuousSyncOptions,
    hosted_config_available: bool,
    workspace_key_available: bool,
) {
    let store = CachedStore::new(options.args.state_root.join(DEFAULT_DATABASE_FILE));
    let Ok(()) = store.with_store(|store| {
        requeue_startup_sync_claims_in_store(
            store,
            options,
            hosted_config_available,
            workspace_key_available,
        );
        Ok(())
    }) else {
        return;
    };
}

fn requeue_startup_sync_claims_in_store(
    store: &MetadataStore,
    options: &ContinuousSyncOptions,
    hosted_config_available: bool,
    workspace_key_available: bool,
) {
    let workspace_id = options.args.workspace_id();
    let device_id = DeviceId::new(options.args.device_id.clone());
    let now = current_timestamp();
    let operation_kind = SyncOperationKind::Reconcile;
    if hosted_config_available
        && let Err(error) = store.requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            operation_kind,
            &device_id,
            "CONVEX_URL is required for daemon sync",
            &now,
        )
    {
        eprintln!(
            "bowline-daemon store write failed (requeue_attention_sync_operations_hosted_config): {error}"
        );
    }
    if workspace_key_available
        && let Err(error) = store.requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            operation_kind,
            &device_id,
            "workspace key is missing",
            &now,
        )
    {
        eprintln!(
            "bowline-daemon store write failed (requeue_attention_sync_operations_workspace_key): {error}"
        );
    }
}

pub(in crate::daemon) fn sync_event(
    name: EventName,
    severity: EventSeverity,
    summary: String,
    workspace_id: &WorkspaceId,
    device_id: &str,
    operation_id: &str,
    now: &str,
) -> WorkspaceEvent {
    let mut event = WorkspaceEvent::new(
        sync_event_id(&name, operation_id, now),
        name,
        now,
        severity,
        summary,
        workspace_id.clone(),
    );
    event.device_id = Some(DeviceId::new(device_id.to_string()));
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Component,
        id: "sync".to_string(),
        path: None,
    });
    event.payload.insert(
        "operationId".to_string(),
        serde_json::Value::String(operation_id.to_string()),
    );
    event
}

pub(in crate::daemon) fn sync_event_id(name: &EventName, operation_id: &str, now: &str) -> EventId {
    EventId::new(format!(
        "evt_sync_{}_{}_{}",
        stable_token(&format!("{name:?}")),
        stable_token(operation_id),
        stable_token(now)
    ))
}

pub(in crate::daemon) fn retry_delay_seconds(operation_id: &str, attempt_count: u32) -> i64 {
    i64::try_from(
        BOUNDED_SYNC_RETRY_POLICY
            .delay(operation_id, attempt_count)
            .as_secs(),
    )
    .expect("the bounded sync retry delay fits i64 seconds")
}

impl SyncOnceSummary {
    pub(in crate::daemon) fn snapshot_root_manifest_id_label(&self) -> &str {
        self.snapshot_root_manifest_id
            .as_deref()
            .unwrap_or(SYNC_SUMMARY_NONE_LABEL)
    }

    pub(in crate::daemon) fn manifest_object_key_label(&self) -> &str {
        self.manifest_object_key
            .as_deref()
            .unwrap_or(SYNC_SUMMARY_NONE_LABEL)
    }

    pub(in crate::daemon) fn stale(&self) -> bool {
        matches!(
            self.outcome,
            SyncSummaryOutcome::Uploaded { stale: true }
                | SyncSummaryOutcome::Merged { stale: true }
        )
    }

    pub(in crate::daemon) fn merged(&self) -> bool {
        matches!(self.outcome, SyncSummaryOutcome::Merged { .. })
    }

    pub(in crate::daemon) fn has_committed_effect(&self) -> bool {
        self.cancelled_late
            || matches!(
                self.outcome,
                SyncSummaryOutcome::Imported
                    | SyncSummaryOutcome::Uploaded { .. }
                    | SyncSummaryOutcome::Merged { .. }
                    | SyncSummaryOutcome::Conflicted
            )
    }

    pub(in crate::daemon) fn sync_state(&self) -> &'static str {
        match self.outcome {
            SyncSummaryOutcome::Conflicted => "conflicted",
            SyncSummaryOutcome::Merged { .. } => "merged",
            SyncSummaryOutcome::Uploaded { stale: true } => "stale",
            SyncSummaryOutcome::NoWorkspaceRef
            | SyncSummaryOutcome::NoChanges
            | SyncSummaryOutcome::Imported => "no-changes",
            SyncSummaryOutcome::Uploaded { stale: false } => "advanced",
        }
    }

    pub(in crate::daemon) fn daemon_state(&self) -> &'static str {
        match self.outcome {
            SyncSummaryOutcome::Conflicted => "attention",
            SyncSummaryOutcome::Uploaded { stale: true }
            | SyncSummaryOutcome::Merged { stale: true } => "retrying",
            SyncSummaryOutcome::NoWorkspaceRef
            | SyncSummaryOutcome::NoChanges
            | SyncSummaryOutcome::Imported
            | SyncSummaryOutcome::Uploaded { stale: false }
            | SyncSummaryOutcome::Merged { stale: false } => "idle",
        }
    }
}

impl SyncOnceArgs {
    pub(in crate::daemon) fn workspace_id(&self) -> WorkspaceId {
        WorkspaceId::new(self.workspace_id.clone())
    }
}

impl ContinuousSyncRuntime {
    pub(in crate::daemon) fn append_sync_completed_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        summary: &SyncOnceSummary,
        now: &str,
    ) {
        let workspace_id = self.options.args.workspace_id();
        let mut event = sync_event(
            EventName::SyncCompleted,
            EventSeverity::Info,
            format!(
                "Continuous sync completed with outcome `{}`.",
                summary.sync_state()
            ),
            &workspace_id,
            &self.options.args.device_id,
            operation_id,
            now,
        );
        event.payload.insert(
            "outcome".to_string(),
            serde_json::Value::String(summary.sync_state().to_string()),
        );
        event.payload.insert(
            "snapshotId".to_string(),
            serde_json::Value::String(summary.snapshot_id.clone()),
        );
        event.payload.insert(
            "version".to_string(),
            serde_json::Value::from(summary.version),
        );
        event.payload.insert(
            "conflictCount".to_string(),
            serde_json::Value::from(summary.conflict_count),
        );
        event.payload.insert(
            "scan".to_string(),
            serde_json::to_value(&summary.scan).unwrap_or_else(|_| serde_json::json!({})),
        );
        self.store_health
            .record("append_event(sync_completed)", store.append_event(event));
        for conflict in &summary.conflicts {
            self.append_conflict_created_event(store, operation_id, conflict, now);
        }
    }

    pub(in crate::daemon) fn append_conflict_created_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        conflict: &ConflictSummary,
        now: &str,
    ) {
        let workspace_id = self.options.args.workspace_id();
        let event_operation_id = format!("{operation_id}:{}", conflict.id);
        let mut event = WorkspaceEvent::new(
            sync_event_id(&EventName::ConflictCreated, &event_operation_id, now),
            EventName::ConflictCreated,
            now,
            EventSeverity::Attention,
            format!(
                "Continuous sync detected a conflict in {} path(s).",
                conflict.paths.len()
            ),
            workspace_id,
        );
        event.device_id = Some(DeviceId::new(self.options.args.device_id.clone()));
        event.path = conflict.paths.first().cloned();
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: conflict.id.clone(),
            path: event.path.clone(),
        });
        event.payload.insert(
            "operationId".to_string(),
            serde_json::Value::String(operation_id.to_string()),
        );
        event.payload.insert(
            "conflictId".to_string(),
            serde_json::Value::String(conflict.id.clone()),
        );
        event.payload.insert(
            "pathCount".to_string(),
            serde_json::Value::from(conflict.paths.len()),
        );
        self.store_health
            .record("append_event(conflict_created)", store.append_event(event));
    }

    pub(in crate::daemon) fn append_sync_failure_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        action: SyncFailureAction,
        now: &str,
    ) {
        let (name, severity, outcome) = match action {
            SyncFailureAction::Attention => (
                EventName::SyncDegraded,
                EventSeverity::Attention,
                "attention",
            ),
            SyncFailureAction::Offline => {
                (EventName::SyncLimited, EventSeverity::Limited, "offline")
            }
            SyncFailureAction::Retry => (EventName::SyncLimited, EventSeverity::Limited, "retry"),
        };
        let workspace_id = self.options.args.workspace_id();
        let mut event = sync_event(
            name,
            severity,
            format!("Continuous sync is waiting for {outcome}."),
            &workspace_id,
            &self.options.args.device_id,
            operation_id,
            now,
        );
        event.payload.insert(
            "outcome".to_string(),
            serde_json::Value::String(outcome.to_string()),
        );
        event.redaction = EventRedaction::applied(["error-message-not-included"]);
        self.store_health
            .record("append_event(sync_failure)", store.append_event(event));
    }
}

pub(in crate::daemon) fn latest_completed_daemon_reconcile(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) -> Option<SyncOperationRecord> {
    store
        .latest_completed_sync_operation_for_device_kind(
            workspace_id,
            SyncOperationKind::Reconcile,
            device_id,
        )
        .ok()
        .flatten()
}

pub(in crate::daemon) fn local_writes_after(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    completed_at: &str,
) -> bool {
    store
        .has_local_write_after_device(workspace_id, device_id, completed_at)
        .unwrap_or(false)
}

pub(in crate::daemon) fn remote_cursor_ahead_of_local_head(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
) -> bool {
    let Ok(Some(cursor)) = store.remote_ref_cursor(workspace_id) else {
        return false;
    };
    let Some(remote_version) = cursor.last_observed_version else {
        return false;
    };
    match store.workspace_sync_head(workspace_id) {
        Ok(Some(head)) => remote_version > head.workspace_ref.version,
        Ok(None) => cursor
            .last_observed_snapshot_id
            .as_deref()
            .is_some_and(|snapshot_id| snapshot_id != EMPTY_SNAPSHOT_ID),
        Err(_) => false,
    }
}

pub(in crate::daemon) fn safety_reconcile_due(
    completed_at: &str,
    interval: Duration,
    now: &str,
) -> bool {
    let Ok(completed_at) = OffsetDateTime::parse(completed_at, &Rfc3339) else {
        return true;
    };
    let Ok(now) = OffsetDateTime::parse(now, &Rfc3339) else {
        return true;
    };
    let Ok(interval) = time::Duration::try_from(interval) else {
        return true;
    };
    completed_at + interval <= now
}

pub(in crate::daemon) trait SyncOperationCountsExt {
    fn has_no_pending_work(&self) -> bool;
}

impl SyncOperationCountsExt for SyncOperationCounts {
    fn has_no_pending_work(&self) -> bool {
        self.queued == 0
            && self.claimed == 0
            && self.waiting_retry == 0
            && self.blocked_offline == 0
            && self.reconciliation_required == 0
            && self.attention == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_once_summary_derives_presentation_state_labels_from_outcome() {
        for (outcome, sync_state, daemon_state) in [
            (SyncSummaryOutcome::NoWorkspaceRef, "no-changes", "idle"),
            (SyncSummaryOutcome::NoChanges, "no-changes", "idle"),
            (SyncSummaryOutcome::Imported, "no-changes", "idle"),
            (
                SyncSummaryOutcome::Uploaded { stale: false },
                "advanced",
                "idle",
            ),
            (
                SyncSummaryOutcome::Uploaded { stale: true },
                "stale",
                "retrying",
            ),
            (
                SyncSummaryOutcome::Merged { stale: false },
                "merged",
                "idle",
            ),
            (
                SyncSummaryOutcome::Merged { stale: true },
                "merged",
                "retrying",
            ),
            (SyncSummaryOutcome::Conflicted, "conflicted", "attention"),
        ] {
            let summary = SyncOnceSummary {
                workspace_id: "ws_test".to_string(),
                snapshot_id: "snap_test".to_string(),
                version: 1,
                outcome,
                snapshot_root_manifest_id: None,
                manifest_object_key: None,
                namespace_root_id: None,
                conflict_count: usize::from(outcome == SyncSummaryOutcome::Conflicted),
                conflicts: Vec::new(),
                scan: SyncScanSummary::default(),
                cancelled_late: false,
            };
            assert_eq!(summary.sync_state(), sync_state);
            assert_eq!(summary.daemon_state(), daemon_state);
        }
    }
}
