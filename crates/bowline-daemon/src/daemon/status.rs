use super::*;

pub(super) type StatusPublisher =
    Box<dyn FnMut(StatusPublishRequest) -> Result<(), Box<dyn std::error::Error>> + 'static>;

/// Inputs for one redacted status publish. The daemon attaches its live
/// in-memory component states; `None` lets the composed snapshot's
/// store-derived state stand.
#[derive(Debug, Clone)]
pub(super) struct StatusPublishRequest {
    pub(super) args: SyncOnceArgs,
    pub(super) sync_state: Option<String>,
    pub(super) watcher_state: Option<String>,
    pub(super) network_state: Option<String>,
}

pub(super) fn hosted_status_publisher() -> StatusPublisher {
    Box::new(publish_workspace_status_once)
}

/// Compose the local status, redact it, attach the daemon's live component
/// states, and publish it to the hosted control plane. Returns an error on any
/// failure so the caller can log and continue; status publishing must never
/// break the sync loop.
pub(super) fn publish_workspace_status_once(
    request: StatusPublishRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let args = &request.args;
    let output = bowline_local::status::compose_status(StatusOptions {
        db_path: Some(args.state_root.join(DEFAULT_DATABASE_FILE)),
        requested_path: Some(args.root.display().to_string()),
        workspace_scope: true,
        generated_at: current_timestamp(),
    })
    .map_err(|error| runtime_error(error.to_string()))?;
    let mut snapshot = bowline_local::status::redacted_status_snapshot(&output, &args.device_id);
    if let Some(sync_state) = request.sync_state {
        snapshot.event_watermarks.sync_state = Some(sync_state);
    }
    if let Some(watcher_state) = request.watcher_state {
        snapshot.event_watermarks.watcher_state = Some(watcher_state);
    }
    if let Some(network_state) = request.network_state {
        snapshot.event_watermarks.network_state = Some(network_state);
    }
    let key_store = key_store()?;
    let control_plane = hosted_control_plane(
        &*key_store,
        args.workspace_id(),
        DeviceId::new(args.device_id.clone()),
    )?;
    control_plane.publish_workspace_status(&snapshot)?;
    Ok(())
}

pub(super) fn initial_sync_status_json(watcher_state: &WatcherRuntimeState) -> String {
    format!(
        "{{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{}}}",
        watcher_runtime_state_json(watcher_state)
    )
}

pub(super) fn watcher_runtime_state_json(watcher_state: &WatcherRuntimeState) -> String {
    match watcher_state {
        WatcherRuntimeState::Ready => "{\"state\":\"ready\"}".to_string(),
        WatcherRuntimeState::Limited(reason) => format!(
            "{{\"state\":\"limited\",\"unavailableBecause\":{}}}",
            json_string(reason)
        ),
    }
}
pub(super) fn sync_operation_counts_json(counts: &SyncOperationCounts) -> String {
    format!(
        "{{\"queued\":{},\"claimed\":{},\"waitingRetry\":{},\"blockedOffline\":{},\"attention\":{},\"completed\":{}}}",
        counts.queued,
        counts.claimed,
        counts.waiting_retry,
        counts.blocked_offline,
        counts.attention,
        counts.completed,
    )
}

pub(super) fn sync_status_with_hosted_calls(status_json: &str) -> String {
    let mut output = status_json.to_string();
    if output.ends_with('}') {
        output.pop();
        output.push_str(",\"hostedCalls\":");
        output.push_str(&hosted_call_counts_json());
        output.push('}');
    }
    output
}

pub(super) fn hosted_call_counts_json() -> String {
    let counts = hosted_function_call_counts();
    let total = counts.iter().map(|count| count.call_count).sum::<u64>();
    let functions = counts
        .iter()
        .map(|count| {
            format!(
                "{{\"name\":{},\"count\":{}}}",
                json_string(&count.function_name),
                count.call_count
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"total\":{},\"functions\":[{}]}}", total, functions)
}

pub(super) fn waiting_queue_status_parts(
    counts: &SyncOperationCounts,
) -> (&'static str, &'static str, &'static str, Vec<String>) {
    if counts.attention > 0 {
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
