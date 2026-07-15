use super::*;

pub(super) fn empty_watermarks() -> EventWatermarks {
    EventWatermarks {
        last_scan_at: None,
        last_event_id: None,
        event_lag_ms: Some(0),
        sync_state: None,
        watcher_state: None,
        network_state: None,
    }
}

pub(super) fn metadata_item(summary: &str, event_name: Option<EventName>) -> StatusItem {
    let mut item = base_status_item(StatusItemKind::Metadata, summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Metadata,
        id: "metadata-local".to_string(),
        path: None,
    });
    item.event_name = event_name;
    item
}

pub(super) fn apply_watermark_status(
    watermarks: &EventWatermarks,
    requested_limited_path: Option<&str>,
    acc: &mut StatusAccumulator,
) {
    if matches!(
        watermarks.sync_state,
        Some(ComponentState::Degraded | ComponentState::Unavailable)
    ) {
        let kind = if watermarks.sync_state == Some(ComponentState::Unavailable) {
            "sync.component_unavailable"
        } else {
            "sync.component_degraded"
        };
        acc.observe_fact(
            kind,
            "sync-component",
            "sync-component",
            StatusFactScope::Workspace,
            None,
        );
        acc.attention_items.push("Sync is degraded.".to_string());
        acc.limits.push(LimitedCapability {
            capability: "sync".to_string(),
            support_capability: None,
            unavailable_because: "sync degraded".to_string(),
            still_works: vec![
                "local files".to_string(),
                "status".to_string(),
                "local metadata inspection".to_string(),
            ],
            path: None,
        });
        acc.items.push(component_item(
            StatusItemKind::Materialization,
            "Sync is degraded; local files and status still work.",
            EventName::SyncDegraded,
        ));
    }

    if matches!(
        watermarks.watcher_state,
        Some(ComponentState::Degraded | ComponentState::Unavailable)
    ) {
        let kind = if watermarks.watcher_state == Some(ComponentState::Unavailable) {
            "watcher.unavailable"
        } else {
            "watcher.degraded"
        };
        acc.observe_fact(kind, "watcher", "watcher", StatusFactScope::Workspace, None);
        acc.attention_items
            .push("Native file watching is degraded.".to_string());
        acc.limits.push(LimitedCapability {
            capability: "watch".to_string(),
            support_capability: None,
            unavailable_because: "native watcher unavailable".to_string(),
            still_works: vec![
                "manual status".to_string(),
                "scheduled reconciliation".to_string(),
            ],
            path: None,
        });
        acc.items.push(component_item(
            StatusItemKind::Watcher,
            "The watcher is degraded, so bowline is using reconciliation.",
            EventName::WatcherDegraded,
        ));
    }

    if let Some(network_state @ (NetworkState::Offline | NetworkState::Degraded)) =
        watermarks.network_state
    {
        let kind = if network_state == NetworkState::Offline {
            "network.offline"
        } else {
            "network.degraded"
        };
        acc.observe_fact(kind, "network", "network", StatusFactScope::Device, None);
        let (unavailable_because, item_summary) = match network_state {
            NetworkState::Offline => (
                "cannot fetch content while offline",
                "Network is offline; local cached state remains available.",
            ),
            NetworkState::Degraded => (
                "content fetch is degraded",
                "Network is degraded; local cached state remains available.",
            ),
            NetworkState::Online => unreachable!("network state is matched above"),
        };
        acc.attention_items
            .push("Network is unavailable.".to_string());
        acc.limits.push(LimitedCapability {
            capability: "content-fetch".to_string(),
            support_capability: None,
            unavailable_because: unavailable_because.to_string(),
            still_works: vec![
                "project structure".to_string(),
                "local cached reads".to_string(),
                "offline edits to hydrated files".to_string(),
            ],
            path: requested_limited_path.map(str::to_string),
        });
        acc.items.push(component_item(
            StatusItemKind::Network,
            item_summary,
            EventName::NetworkOffline,
        ));
    }
}

pub(super) fn apply_sync_operation_status(
    workspace_id: &WorkspaceId,
    counts: &SyncOperationCounts,
    acc: &mut StatusAccumulator,
) {
    let pending = counts.queued
        + counts.claimed
        + counts.waiting_retry
        + counts.blocked_offline
        + counts.reconciliation_required
        + counts.attention;
    if pending == 0 {
        return;
    }

    let summary = sync_operation_summary(counts);
    let mut item = base_status_item(StatusItemKind::Materialization, &summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    acc.items.push(item);
    acc.observe_fact(
        "sync.queue_pending",
        "sync-queue-pending",
        "sync-queue",
        StatusFactScope::Workspace,
        Some(workspace_id.as_str()),
    );

    if counts.reconciliation_required > 0 || counts.attention > 0 {
        acc.observe_fact(
            "sync.queue_blocked",
            "sync-queue-blocked",
            "sync-queue-blocked",
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
        acc.attention_items
            .push("Sync queue needs attention.".to_string());
        acc.limits.push(LimitedCapability {
            capability: "sync".to_string(),
            support_capability: None,
            unavailable_because: "sync queue needs attention".to_string(),
            still_works: vec!["local files".to_string(), "status".to_string()],
            path: None,
        });
    } else if counts.blocked_offline > 0 {
        acc.observe_fact(
            "sync.offline_waiting",
            "sync-queue-offline",
            "sync-queue-offline",
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
        acc.attention_items
            .push("Sync queue is waiting for offline recovery.".to_string());
        acc.limits.push(LimitedCapability {
            capability: "sync".to_string(),
            support_capability: None,
            unavailable_because: "sync queue is waiting for offline recovery".to_string(),
            still_works: sync_queue_wait_still_works(),
            path: None,
        });
    } else if counts.waiting_retry > 0 {
        acc.observe_fact(
            "sync.retry_waiting",
            "sync-queue-retry",
            "sync-queue-retry",
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
        acc.attention_items
            .push("Sync queue is waiting for retry.".to_string());
        acc.limits.push(LimitedCapability {
            capability: "sync".to_string(),
            support_capability: None,
            unavailable_because: "sync queue is waiting for retry".to_string(),
            still_works: sync_queue_wait_still_works(),
            path: None,
        });
    }
}

pub(super) fn sync_queue_status(counts: &SyncOperationCounts) -> Option<SyncQueueStatus> {
    let status = SyncQueueStatus {
        queued: counts.queued,
        claimed: counts.claimed,
        waiting_retry: counts.waiting_retry,
        blocked_offline: counts.blocked_offline,
        reconciliation_required: counts.reconciliation_required,
        attention: counts.attention,
        completed: counts.completed,
    };
    status.has_pending_work().then_some(status)
}

const STALE_BASE_REMEDY_COMMAND: &str = "bowline status --watch";

pub(crate) fn snapshot_stale_bases(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
) -> Result<Vec<StaleBaseStatus>, MetadataError> {
    let projects = match project_id {
        Some(project_id) => store
            .project_by_id(workspace_id, project_id)?
            .into_iter()
            .collect::<Vec<_>>(),
        None => store.projects(workspace_id)?,
    };
    let agent_leases = store.agent_leases(workspace_id)?;
    let active_work_views = store.work_views(workspace_id, true, None)?;
    snapshot_stale_bases_from_inputs(
        store,
        workspace_id,
        &projects,
        &agent_leases,
        &active_work_views,
        project_id,
    )
}

pub(crate) fn snapshot_stale_bases_from_inputs(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    projects: &[ProjectRecord],
    agent_leases: &[AgentLeaseRecord],
    active_work_views: &[WorkViewRecord],
    project_id: Option<&ProjectId>,
) -> Result<Vec<StaleBaseStatus>, MetadataError> {
    let latest_snapshot_ids = store.project_latest_snapshot_ids(workspace_id)?;
    let mut stale_bases = Vec::new();
    for project in projects {
        if let Some(project_id) = project_id
            && &project.id != project_id
        {
            continue;
        }
        let latest_snapshot_id = latest_snapshot_ids.get(&project.id).cloned();
        let Some(latest_snapshot_id) = latest_snapshot_id else {
            if project_id.is_some() {
                stale_bases.push(StaleBaseStatus::snapshot(
                    FreshnessVerdict::Unknown,
                    format!(
                        "{} has no local project snapshot yet; freshness cannot be proven.",
                        project.path
                    ),
                    Some(project.id.clone()),
                    Some(project.path.clone()),
                    None,
                    None,
                    Some(STALE_BASE_REMEDY_COMMAND.to_string()),
                ));
            }
            continue;
        };

        let project_stale_count = stale_bases.len();
        for lease in agent_leases {
            if lease.project_id != project.id
                || !matches!(
                    lease.session_state,
                    AgentSessionState::Open | AgentSessionState::Provisional
                )
                || lease.base_snapshot_id == latest_snapshot_id
            {
                continue;
            }
            stale_bases.push(StaleBaseStatus::snapshot(
                FreshnessVerdict::Behind,
                format!(
                    "Agent lease {} is based on an older snapshot for {}.",
                    lease.id.as_str(),
                    project.path
                ),
                Some(project.id.clone()),
                Some(project.path.clone()),
                Some(lease.base_snapshot_id.clone()),
                Some(latest_snapshot_id.clone()),
                Some(STALE_BASE_REMEDY_COMMAND.to_string()),
            ));
        }

        for view in active_work_views {
            if view.project_id != project.id
                || !matches!(
                    view.lifecycle,
                    WorkViewLifecycle::Active | WorkViewLifecycle::ReviewReady
                )
                || view.base_snapshot_id == latest_snapshot_id
            {
                continue;
            }
            stale_bases.push(StaleBaseStatus::snapshot(
                FreshnessVerdict::Behind,
                format!(
                    "Work view {} is based on an older snapshot for {}.",
                    view.name, project.path
                ),
                Some(project.id.clone()),
                Some(project.path.clone()),
                Some(view.base_snapshot_id.clone()),
                Some(latest_snapshot_id.clone()),
                Some(STALE_BASE_REMEDY_COMMAND.to_string()),
            ));
        }

        if project_id.is_some() && stale_bases.len() == project_stale_count {
            let (verdict, summary, remedy) = match project.git_observer_state {
                GitObserverState::Ok => (
                    FreshnessVerdict::Current,
                    format!("Git observation for {} is current.", project.path),
                    None,
                ),
                GitObserverState::Partial => (
                    FreshnessVerdict::Unknown,
                    format!(
                        "Git observation for {} is partial; freshness cannot be proven.",
                        project.path
                    ),
                    Some(STALE_BASE_REMEDY_COMMAND.to_string()),
                ),
                GitObserverState::Unavailable => (
                    FreshnessVerdict::Unknown,
                    format!(
                        "Git observation for {} is unavailable; freshness cannot be proven.",
                        project.path
                    ),
                    Some(STALE_BASE_REMEDY_COMMAND.to_string()),
                ),
            };
            stale_bases.push(StaleBaseStatus::git(
                verdict,
                summary,
                Some(project.id.clone()),
                Some(project.path.clone()),
                remedy,
            ));
        }
    }
    Ok(stale_bases)
}

pub(crate) fn freshness_for_stale_bases(stale_bases: &[StaleBaseStatus]) -> FreshnessVerdict {
    bowline_core::status::freshness_verdict_for(stale_bases)
}

pub(super) fn apply_stale_base_status(
    stale_bases: &[StaleBaseStatus],
    acc: &mut StatusAccumulator,
) {
    for stale_base in stale_bases {
        if !stale_base.verdict.needs_attention() {
            continue;
        }
        let kind = if stale_base.verdict == FreshnessVerdict::Unknown {
            "observation.freshness_unknown"
        } else if stale_base.verdict == FreshnessVerdict::Diverged {
            "snapshot.base_diverged"
        } else {
            "snapshot.base_behind"
        };
        acc.observe_fact(
            kind,
            format!(
                "stale-base:{}",
                stale_base
                    .project_id
                    .as_ref()
                    .map_or("workspace", ProjectId::as_str)
            ),
            format!(
                "stale-base:{}",
                stale_base
                    .project_id
                    .as_ref()
                    .map_or("workspace", ProjectId::as_str)
            ),
            StatusFactScope::Project,
            stale_base.project_id.as_ref().map(ProjectId::as_str),
        );
        acc.attention_items.push(stale_base.summary.clone());
        let mut item = base_status_item(StatusItemKind::Source, &stale_base.summary);
        item.subject = stale_base
            .project_id
            .as_ref()
            .map(|project_id| StatusSubject {
                kind: StatusSubjectKind::Project,
                id: project_id.as_str().to_string(),
                path: stale_base.project_path.clone(),
            });
        item.path = stale_base.project_path.clone();
        item.project_id = stale_base.project_id.clone();
        item.snapshot_id = stale_base.base_snapshot_id.clone();
        item.event_name = Some(EventName::SourceStale);
        acc.items.push(item);
        if let Some(command) = stale_base.remedy_command.as_ref()
            && !acc
                .next_actions
                .iter()
                .any(|action| action.command.as_deref() == Some(command.as_str()))
        {
            acc.next_actions.push(RepairCommand::inspect(
                "Inspect freshness".to_string(),
                Some(command.clone()),
            ));
        }
    }
}

pub(super) fn apply_unresolved_conflict_status(
    paths: &BTreeSet<String>,
    workspace_id: &WorkspaceId,
    workspace_root: &str,
    acc: &mut StatusAccumulator,
) -> Result<(), LocalStatusError> {
    if paths.is_empty() {
        return Ok(());
    }

    acc.observe_fact(
        "sync.conflict_unresolved",
        "unresolved-conflicts",
        "unresolved-conflicts",
        StatusFactScope::Workspace,
        Some(workspace_id.as_str()),
    );
    let summary = if let Some(path) = paths.iter().next().filter(|_| paths.len() == 1) {
        format!("1 unresolved conflict needs attention: {path}.")
    } else {
        format!("{} unresolved conflicts need attention.", paths.len())
    };
    acc.attention_items.push(summary.clone());

    let mut item = base_status_item(StatusItemKind::Conflict, &summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    item.path = paths.iter().next().cloned();
    item.event_name = Some(EventName::ConflictBundleCreated);
    acc.items.push(item);

    acc.limits.push(LimitedCapability {
        capability: "sync".to_string(),
        support_capability: None,
        unavailable_because: "unresolved conflict".to_string(),
        still_works: vec![
            "local files".to_string(),
            "status".to_string(),
            "conflict resolution".to_string(),
        ],
        path: None,
    });
    acc.next_actions
        .push(conflict_resolution_action(workspace_root));
    Ok(())
}

pub(super) fn sync_operation_counts_for_local_device(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    recent_events: &[bowline_core::events::WorkspaceEvent],
) -> Result<SyncOperationCounts, MetadataError> {
    match env::var("BOWLINE_DEVICE_ID") {
        Ok(device_id) if !device_id.trim().is_empty() => {
            store.sync_operation_counts_for_device(workspace_id, &DeviceId::new(device_id))
        }
        _ => {
            if let Some(device_id) = recent_sync_device_id(recent_events) {
                store.sync_operation_counts_for_device(workspace_id, &device_id)
            } else {
                store.sync_operation_counts(workspace_id)
            }
        }
    }
}

pub(super) fn recent_sync_device_id(
    events: &[bowline_core::events::WorkspaceEvent],
) -> Option<DeviceId> {
    events
        .iter()
        .find(|event| {
            matches!(
                &event.name,
                EventName::SyncStarted
                    | EventName::SyncCompleted
                    | EventName::SyncLimited
                    | EventName::SyncDegraded
                    | EventName::SyncRecovered
            ) && event.device_id.is_some()
        })
        .and_then(|event| event.device_id.clone())
}

pub(super) fn sync_queue_wait_still_works() -> Vec<String> {
    vec![
        "local files".to_string(),
        "status".to_string(),
        "scheduled retry".to_string(),
    ]
}

pub(super) fn sync_operation_summary(counts: &SyncOperationCounts) -> String {
    format!(
        "Sync queue: {} queued, {} running, {} waiting retry, {} offline, {} reconciling, {} attention.",
        counts.queued,
        counts.claimed,
        counts.waiting_retry,
        counts.blocked_offline,
        counts.reconciliation_required,
        counts.attention
    )
}
