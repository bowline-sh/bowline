use super::*;

pub(in crate::daemon) fn requeue_startup_sync_claims(options: &ContinuousSyncOptions) {
    let workspace_id = options.args.workspace_id();
    let workspace_key_available = key_store()
        .and_then(|store| {
            store
                .load_workspace_key(&workspace_id)
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
        })
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
    let Ok(store) = MetadataStore::open(options.args.state_root.join(DEFAULT_DATABASE_FILE)) else {
        return;
    };
    let workspace_id = options.args.workspace_id();
    let device_id = DeviceId::new(options.args.device_id.clone());
    let now = current_timestamp();
    let _ = store.requeue_claimed_sync_operations_for_device_kind(
        &workspace_id,
        "daemon-reconcile",
        &device_id,
        &now,
    );
    let _ = store.requeue_waiting_retry_sync_operations_for_device_kind(
        &workspace_id,
        "daemon-reconcile",
        &device_id,
        &now,
    );
    if hosted_config_available {
        let _ = store.requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            "daemon-reconcile",
            &device_id,
            "CONVEX_URL is required for daemon sync",
            &now,
        );
    }
    if workspace_key_available {
        let _ = store.requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            "daemon-reconcile",
            &device_id,
            "workspace key is missing",
            &now,
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
        sync_event_id(name, operation_id, now),
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

pub(in crate::daemon) fn sync_event_id(name: EventName, operation_id: &str, now: &str) -> EventId {
    EventId::new(format!(
        "evt_sync_{}_{}_{}",
        stable_token(&format!("{name:?}")),
        stable_token(operation_id),
        stable_token(now)
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum SyncFailureAction {
    Attention,
    Offline,
    Retry,
}

pub(in crate::daemon) fn sync_failure_action(message: &str) -> SyncFailureAction {
    if message.contains("CONVEX_URL")
        || message.contains("workspace key is missing")
        || message.contains("trusted")
        || message.contains("approve this device")
    {
        return SyncFailureAction::Attention;
    }
    if message.contains("offline")
        || message.contains("network")
        || message.contains("timed out")
        || message.contains("connection")
        || message.contains("snapshot manifest")
        || message.contains("missing object")
        || message.contains("missing metadata for object")
        || (message.contains("R2 download for object") && message.contains("HTTP 404"))
    {
        return SyncFailureAction::Offline;
    }
    SyncFailureAction::Retry
}

pub(in crate::daemon) fn retry_delay_seconds(operation_id: &str, attempt_count: u32) -> i64 {
    let exponent = attempt_count.saturating_sub(1).min(5);
    let base = (SYNC_RETRY_INITIAL_SECONDS * 2_i64.pow(exponent)).min(SYNC_RETRY_MAX_SECONDS);
    let jitter = operation_id.bytes().fold(0_u64, |state, byte| {
        state.wrapping_mul(31).wrapping_add(byte as u64)
    }) % (SYNC_RETRY_JITTER_SECONDS as u64 + 1);
    (base + jitter as i64).min(SYNC_RETRY_MAX_SECONDS)
}

impl SyncOnceSummary {
    pub(in crate::daemon) fn sync_state(&self) -> &'static str {
        if self.conflict_count > 0 {
            "conflicted"
        } else if self.merged {
            "merged"
        } else if self.stale {
            "stale"
        } else if self.object_manifest_id == "none" {
            "no-changes"
        } else {
            "advanced"
        }
    }

    pub(in crate::daemon) fn daemon_state(&self) -> &'static str {
        if self.conflict_count > 0 {
            "attention"
        } else if self.stale {
            "retrying"
        } else {
            "idle"
        }
    }
}

impl SyncOnceArgs {
    pub(in crate::daemon) fn workspace_id(&self) -> WorkspaceId {
        WorkspaceId::new(self.workspace_id.clone())
    }
}

pub(in crate::daemon) fn latest_completed_daemon_reconcile(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) -> Option<SyncOperationRecord> {
    store
        .sync_operations(workspace_id)
        .ok()?
        .into_iter()
        .filter(|operation| {
            operation.kind == "daemon-reconcile"
                && operation.state == "completed"
                && operation.device_id.as_ref() == Some(device_id)
        })
        .max_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then(left.id.cmp(&right.id))
        })
}

pub(in crate::daemon) fn local_writes_after(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    completed_at: &str,
) -> bool {
    store
        .local_write_log(workspace_id)
        .map(|writes| {
            writes.into_iter().any(|write| {
                write.device_id == *device_id && write.created_at.as_str() > completed_at
            })
        })
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
            .is_some_and(|snapshot_id| snapshot_id != "empty"),
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
            && self.attention == 0
    }
}
