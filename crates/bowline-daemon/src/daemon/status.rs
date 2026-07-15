use super::*;

use bowline_control_plane::{
    StatusEventWatermarks, StatusItemSnapshot, StatusLimitSnapshot, StatusSyncQueueSnapshot,
    StatusWorkspaceSummarySnapshot, WorkspaceControlPlaneClient, WorkspaceStatusSnapshot,
};
use serde::Serialize;

type StatusPublishFn = dyn FnMut(StatusPublishPayload) -> Result<StatusPublishOutcome, Box<dyn std::error::Error>>
    + Send
    + 'static;

#[derive(Clone)]
pub(super) struct StatusPublisher(Arc<Mutex<Box<StatusPublishFn>>>);

impl StatusPublisher {
    pub(super) fn new(
        publish: impl FnMut(
            StatusPublishPayload,
        ) -> Result<StatusPublishOutcome, Box<dyn std::error::Error>>
        + Send
        + 'static,
    ) -> Self {
        Self(Arc::new(Mutex::new(Box::new(publish))))
    }

    pub(super) fn publish(
        &self,
        payload: StatusPublishPayload,
    ) -> Result<StatusPublishOutcome, Box<dyn std::error::Error>> {
        self.0
            .lock()
            .map_err(|_| runtime_error("status publisher lock is unavailable"))?(payload)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusPublishOutcome {
    pub(super) fingerprint: String,
}

#[derive(Debug, Clone)]
pub(super) struct StatusPublishRequest {
    pub(super) args: SyncOnceArgs,
}

#[derive(Debug, Clone)]
pub(super) struct StatusPublishPayload {
    pub(super) request: StatusPublishRequest,
    pub(super) snapshot: Option<WorkspaceStatusSnapshot>,
    pub(super) fingerprint: Option<String>,
}

impl StatusPublishPayload {
    #[cfg(test)]
    pub(super) fn from_request(request: StatusPublishRequest) -> Self {
        Self {
            request,
            snapshot: None,
            fingerprint: None,
        }
    }

    pub(super) fn from_projection(
        request: StatusPublishRequest,
        projection: &bowline_daemon::status_projection::DaemonStatusProjection,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Projection timestamps describe semantic observation. Hosted liveness
        // is the control plane's server-set `publishedAt`, refreshed on every
        // successful heartbeat without falsifying source observation times.
        let snapshot = bowline_local::status::redacted_status_snapshot(
            &projection.status,
            &request.args.device_id,
        );
        if snapshot.workspace_id.as_str() != request.args.workspace_id {
            return Err(runtime_error(format!(
                "status projection workspace does not match configured daemon workspace: projection={}, configured={}",
                snapshot.workspace_id.as_str(),
                request.args.workspace_id
            )));
        }
        let fingerprint = status_snapshot_fingerprint(&snapshot)?;
        Ok(Self {
            request,
            snapshot: Some(snapshot),
            fingerprint: Some(fingerprint),
        })
    }

    fn into_snapshot(
        self,
    ) -> Result<(StatusPublishRequest, WorkspaceStatusSnapshot, String), Box<dyn std::error::Error>>
    {
        let snapshot = match self.snapshot {
            Some(snapshot) => snapshot,
            None => return Err(runtime_error("projection status snapshot is missing")),
        };
        let fingerprint = match self.fingerprint {
            Some(fingerprint) => fingerprint,
            None => status_snapshot_fingerprint(&snapshot)?,
        };
        Ok((self.request, snapshot, fingerprint))
    }
}

pub(super) fn hosted_status_publisher_with_context(
    resolver: HostedContextResolver,
) -> StatusPublisher {
    hosted_status_publisher_with_operations(
        resolver,
        Arc::new(publish_workspace_status_with_hosted),
    )
}

pub(in crate::daemon) type HostedStatusOperation = Arc<
    dyn Fn(
            Arc<HostedContext>,
            StatusPublishPayload,
        ) -> Result<StatusPublishOutcome, Box<dyn std::error::Error>>
        + Send
        + Sync,
>;

pub(in crate::daemon) fn hosted_status_publisher_with_operations(
    resolver: HostedContextResolver,
    operation: HostedStatusOperation,
) -> StatusPublisher {
    StatusPublisher::new(move |payload| {
        let hosted = resolver(&payload.request.args)?;
        operation(hosted, payload)
    })
}

fn publish_workspace_status_with_hosted(
    hosted: Arc<HostedContext>,
    payload: StatusPublishPayload,
) -> Result<StatusPublishOutcome, Box<dyn std::error::Error>> {
    let (request, snapshot, fingerprint) = payload.into_snapshot()?;
    if snapshot.workspace_id.as_str() != request.args.workspace_id {
        return Err(runtime_error(
            "status snapshot workspace does not match configured daemon workspace",
        ));
    }
    hosted.client.publish_workspace_status(&snapshot)?;
    Ok(StatusPublishOutcome { fingerprint })
}

pub(super) fn status_snapshot_fingerprint(
    snapshot: &WorkspaceStatusSnapshot,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stable = snapshot.clone();
    stable.snapshot_id = bowline_core::ids::SnapshotId::new("wss_fingerprint");
    Ok(serde_json::to_string(&status_snapshot_fingerprint_value(
        &stable,
    ))?)
}

fn status_snapshot_fingerprint_value(snapshot: &WorkspaceStatusSnapshot) -> serde_json::Value {
    let WorkspaceStatusSnapshot {
        workspace_id,
        snapshot_id,
        availability,
        attention,
        primary_fact_id,
        facts,
        freshness,
        schema_hash,
        snapshot_version,
        producer_version,
        observed_at: _,
        attention_items,
        event_watermarks,
        sync_queue,
        workspace_summary,
        items,
        limits,
        published_by_device_id,
    } = snapshot;
    let StatusEventWatermarks {
        last_event_id,
        last_scan_at,
        sync_state,
        watcher_state,
        network_state,
    } = event_watermarks;
    serde_json::json!({
        "workspaceId": workspace_id,
        "snapshotId": snapshot_id,
        "availability": availability,
        "attention": attention,
        "primaryFactId": primary_fact_id,
        "facts": facts.iter().map(|fact| serde_json::json!({
            "id": fact.id.as_str(),
            "kind": fact.kind.as_str(),
            "source": fact.source.as_str(),
            "scope": fact.scope,
            "scopeId": fact.scope_id,
            "availabilityImpact": fact.availability_impact,
            "attentionImpact": fact.attention_impact,
            "summaryKey": fact.summary_key,
            "parameters": fact.parameters,
            "action": fact.action,
            "dedupeKey": fact.dedupe_key.as_str(),
        })).collect::<Vec<_>>(),
        "freshness": freshness,
        "schemaHash": schema_hash,
        "snapshotVersion": snapshot_version,
        "producerVersion": producer_version,
        "attentionItems": attention_items,
        "eventWatermarks": {
            "lastEventId": last_event_id,
            "lastScanAt": last_scan_at,
            "syncState": sync_state,
            "watcherState": watcher_state,
            "networkState": network_state,
        },
        "syncQueue": sync_queue.as_ref().map(|queue| {
            let StatusSyncQueueSnapshot {
                queued,
                claimed,
                waiting_retry,
                blocked_offline,
                reconciliation_required,
                attention,
                completed,
            } = queue;
            serde_json::json!({
                "queued": queued,
                "claimed": claimed,
                "waitingRetry": waiting_retry,
                "blockedOffline": blocked_offline,
                "reconciliationRequired": reconciliation_required,
                "attention": attention,
                "completed": completed,
            })
        }),
        "workspaceSummary": workspace_summary.as_ref().map(|summary| {
            let StatusWorkspaceSummarySnapshot {
                total_projects,
                repo_count,
                env_file_count,
            } = summary;
            serde_json::json!({
                "totalProjects": total_projects,
                "repoCount": repo_count,
                "envFileCount": env_file_count,
            })
        }),
        "items": items.iter().map(|item| {
            let StatusItemSnapshot {
                kind,
                summary,
                path,
                event_name,
            } = item;
            serde_json::json!({
                "kind": kind,
                "summary": summary,
                "path": path,
                "eventName": event_name,
            })
        }).collect::<Vec<_>>(),
        "limits": limits.iter().map(|limit| {
            let StatusLimitSnapshot {
                capability,
                unavailable_because,
                path,
                still_works,
                support_capability,
            } = limit;
            serde_json::json!({
                "capability": capability,
                "unavailableBecause": unavailable_because,
                "path": path,
                "stillWorks": still_works,
                "supportCapability": support_capability,
            })
        }).collect::<Vec<_>>(),
        "publishedByDeviceId": published_by_device_id,
    })
}

pub(super) fn initial_sync_status_json(
    watcher_state: &WatcherRuntimeState,
    watcher_recovery: &WatcherRecovery,
) -> String {
    daemon_json(&InitialSyncStatusJson {
        state: "queued",
        tick_count: 0,
        watcher_state: WatcherRuntimeStateJson::from_state(watcher_state, watcher_recovery),
    })
}

#[cfg(test)]
pub(super) fn sync_operation_counts_json(counts: &SyncOperationCounts) -> String {
    daemon_json(&SyncOperationCountsJson::from(counts))
}

#[cfg(test)]
pub(super) fn sync_status_with_hosted_calls(status_json: &str) -> String {
    let Ok(mut status) = serde_json::from_str::<serde_json::Value>(status_json) else {
        return status_json.to_string();
    };
    let Some(fields) = status.as_object_mut() else {
        return status_json.to_string();
    };
    let Ok(hosted_calls) = serde_json::to_value(hosted_call_counts_payload()) else {
        return status_json.to_string();
    };
    fields.insert("hostedCalls".to_string(), hosted_calls);
    daemon_json(&status)
}

#[cfg(test)]
fn hosted_call_counts_payload() -> HostedCallCountsJson {
    let counts = hosted_function_call_counts();
    HostedCallCountsJson {
        total: counts.iter().map(|count| count.call_count).sum::<u64>(),
        functions: counts
            .iter()
            .map(|count| HostedFunctionCallCountJson {
                name: count.function_name.clone(),
                count: count.call_count,
            })
            .collect(),
    }
}

pub(super) fn waiting_queue_status_parts(
    counts: &SyncOperationCounts,
) -> (&'static str, &'static str, &'static str, Vec<String>) {
    if counts.reconciliation_required > 0 || counts.attention > 0 {
        return (
            "attention",
            "sync queue needs attention",
            "resolve sync queue attention",
            vec!["local edits".to_string(), "status".to_string()],
        );
    }
    if counts.blocked_offline > 0 {
        return (
            "limited",
            "sync queue is waiting for offline recovery",
            "sync ~/Code",
            vec![
                "local edits".to_string(),
                "status".to_string(),
                "scheduled retry".to_string(),
            ],
        );
    }
    if counts.waiting_retry > 0 {
        return (
            "limited",
            "sync queue is waiting for retry",
            "sync ~/Code",
            vec![
                "local edits".to_string(),
                "status".to_string(),
                "scheduled retry".to_string(),
            ],
        );
    }
    if counts.queued > 0 || counts.claimed > 0 {
        return (
            "syncing",
            "sync queue has pending work",
            "finish sync work",
            vec!["local edits".to_string(), "status".to_string()],
        );
    }
    (
        "idle",
        "no sync work is queued",
        "wait for local or remote changes",
        vec!["local edits".to_string(), "status".to_string()],
    )
}

pub(super) fn daemon_json<T>(value: &T) -> String
where
    T: Serialize,
{
    match serde_json::to_string(value) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("bowline-daemon status serialization failed: {error}");
            serde_json::json!({ "error": "status serialization failed" }).to_string()
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitialSyncStatusJson<'a> {
    state: &'static str,
    tick_count: u64,
    watcher_state: WatcherRuntimeStateJson<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WatcherRuntimeStateJson<'a> {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_because: Option<&'a str>,
    overflow_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rearm_attempt: Option<u32>,
}

impl<'a> WatcherRuntimeStateJson<'a> {
    pub(super) fn from_state(state: &'a WatcherRuntimeState, recovery: &WatcherRecovery) -> Self {
        match state {
            WatcherRuntimeState::Ready => Self {
                state: "ready",
                unavailable_because: None,
                overflow_count: recovery.overflow_total,
                rearm_attempt: None,
            },
            WatcherRuntimeState::Rearming => Self {
                state: "rearming",
                unavailable_because: None,
                overflow_count: recovery.overflow_total,
                rearm_attempt: Some(recovery.consecutive_overflows),
            },
            WatcherRuntimeState::Limited(reason) => Self {
                state: "limited",
                unavailable_because: Some(reason),
                overflow_count: recovery.overflow_total,
                rearm_attempt: None,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SyncOperationCountsJson {
    queued: u64,
    claimed: u64,
    waiting_retry: u64,
    blocked_offline: u64,
    reconciliation_required: u64,
    attention: u64,
    completed: u64,
    cancelled: u64,
}

impl From<&SyncOperationCounts> for SyncOperationCountsJson {
    fn from(value: &SyncOperationCounts) -> Self {
        Self {
            queued: value.queued,
            claimed: value.claimed,
            waiting_retry: value.waiting_retry,
            blocked_offline: value.blocked_offline,
            reconciliation_required: value.reconciliation_required,
            attention: value.attention,
            completed: value.completed,
            cancelled: value.cancelled,
        }
    }
}

#[cfg(test)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HostedCallCountsJson {
    total: u64,
    functions: Vec<HostedFunctionCallCountJson>,
}

#[cfg(test)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HostedFunctionCallCountJson {
    name: String,
    count: u64,
}
