use super::*;

use bowline_control_plane::{
    StatusEventWatermarks, StatusItemSnapshot, StatusLimitSnapshot, StatusSyncQueueSnapshot,
    StatusWorkspaceSummarySnapshot, WorkspaceControlPlaneClient, WorkspaceStatusSnapshot,
};
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
    pub(super) args: SyncArgs,
}

#[derive(Debug, Clone)]
pub(super) struct StatusPublishPayload {
    pub(super) request: StatusPublishRequest,
    pub(super) snapshot: Option<WorkspaceStatusSnapshot>,
    pub(super) fingerprint: Option<String>,
}

impl StatusPublishPayload {
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
