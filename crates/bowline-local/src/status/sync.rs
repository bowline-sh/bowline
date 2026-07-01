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

pub(super) fn durable_index_status(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
) -> Result<Option<IndexStatus>, MetadataError> {
    let (count, ready_count, source_watermark, indexed_watermark, updated_at): (
        i64,
        i64,
        i64,
        i64,
        Option<String>,
    ) = store.connection().query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN state = 'ready' THEN 1 ELSE 0 END), 0),
                COALESCE(MAX(source_watermark), 0),
                COALESCE(MAX(indexed_watermark), 0),
                MAX(updated_at)
         FROM index_work
         WHERE workspace_id = ?1 AND (?2 IS NULL OR project_id = ?2)",
        rusqlite::params![workspace_id.as_str(), project_id.map(|id| id.as_str())],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    if count == 0 {
        return Ok(None);
    }
    let state = if ready_count == count && indexed_watermark >= source_watermark {
        IndexState::Ready
    } else if indexed_watermark < source_watermark {
        IndexState::Stale
    } else {
        IndexState::Degraded
    };
    Ok(Some(IndexStatus {
        state,
        source: IndexSource::Local,
        indexed_at: (state == IndexState::Ready)
            .then(|| updated_at.clone())
            .flatten(),
        updated_at,
        snapshot_id: None,
        index_pack_object_key: None,
        path_count: 0,
        file_count: 0,
        indexed_bytes: 0,
        pending_path_count: Some((count - ready_count).max(0) as u64),
        degraded_reason: (state != IndexState::Ready).then_some(IndexDegradedReason::Missing),
        summary: if state == IndexState::Ready {
            "Index metadata is current.".to_string()
        } else {
            "Index metadata has pending or stale work.".to_string()
        },
        next_action: None,
    }))
}

pub(super) fn apply_index_status(
    index: Option<&IndexStatus>,
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    let Some(index) = index else {
        return;
    };
    match index.state {
        IndexState::Ready => {}
        IndexState::Stale | IndexState::Rebuilding => {
            if *level == StatusLevel::Healthy {
                *level = StatusLevel::Attention;
            }
            attention_items.push(if index.state == IndexState::Rebuilding {
                "Index is rebuilding.".to_string()
            } else {
                "Index metadata is stale.".to_string()
            });
            let mut item = base_status_item(StatusItemKind::Index, &index.summary);
            item.subject = Some(StatusSubject {
                kind: StatusSubjectKind::Index,
                id: "index-local".to_string(),
                path: None,
            });
            item.event_name = Some(EventName::IndexDegraded);
            items.push(item);
        }
        IndexState::Degraded => {
            *level = StatusLevel::Limited;
            attention_items.push("Index is degraded.".to_string());
            limits.push(LimitedCapability {
                capability: "index".to_string(),
                unavailable_because: index.summary.clone(),
                still_works: vec![
                    "status".to_string(),
                    "local file access".to_string(),
                    "bounded hydration".to_string(),
                ],
                path: None,
            });
            let mut item = base_status_item(StatusItemKind::Index, &index.summary);
            item.subject = Some(StatusSubject {
                kind: StatusSubjectKind::Index,
                id: "index-local".to_string(),
                path: None,
            });
            item.event_name = Some(EventName::IndexDegraded);
            items.push(item);
        }
    }
}

pub(super) fn durable_hydration_budget_status(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
) -> Result<Option<HydrationBudgetStatus>, MetadataError> {
    let active_leases = store
        .agent_leases(workspace_id)?
        .into_iter()
        .filter(|lease| match project_id {
            Some(project_id) => lease.project_id.as_str() == project_id.as_str(),
            None => true,
        })
        .filter(|lease| {
            matches!(
                lease.execution_state,
                AgentLeaseExecutionState::Active | AgentLeaseExecutionState::Blocked
            )
        })
        .collect::<Vec<_>>();

    match active_leases.as_slice() {
        [lease] => lease_budget_status(
            store,
            workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .map(Some),
        _ => Ok(None),
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
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    if matches!(
        watermarks.sync_state,
        Some(ComponentState::Degraded | ComponentState::Unavailable)
    ) {
        *level = StatusLevel::Limited;
        attention_items.push("Sync is degraded.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync degraded".to_string(),
            still_works: vec![
                "local files".to_string(),
                "status".to_string(),
                "local metadata inspection".to_string(),
            ],
            path: None,
        });
        items.push(component_item(
            StatusItemKind::Materialization,
            "Sync is degraded; local files and status still work.",
            EventName::SyncDegraded,
        ));
    }

    if matches!(
        watermarks.watcher_state,
        Some(ComponentState::Degraded | ComponentState::Unavailable)
    ) {
        *level = StatusLevel::Limited;
        attention_items.push("Native file watching is degraded.".to_string());
        limits.push(LimitedCapability {
            capability: "watch".to_string(),
            unavailable_because: "native watcher unavailable".to_string(),
            still_works: vec![
                "manual status".to_string(),
                "scheduled reconciliation".to_string(),
            ],
            path: None,
        });
        items.push(component_item(
            StatusItemKind::Watcher,
            "The watcher is degraded, so bowline is using reconciliation.",
            EventName::WatcherDegraded,
        ));
    }

    if matches!(
        watermarks.network_state,
        Some(NetworkState::Offline | NetworkState::Degraded)
    ) {
        *level = StatusLevel::Limited;
        let unavailable_because = if matches!(watermarks.network_state, Some(NetworkState::Offline))
        {
            "network offline"
        } else {
            "network degraded"
        };
        attention_items.push("Network is unavailable.".to_string());
        limits.push(LimitedCapability {
            capability: "hydrate".to_string(),
            unavailable_because: unavailable_because.to_string(),
            still_works: vec![
                "project structure".to_string(),
                "local cached reads".to_string(),
            ],
            path: None,
        });
        items.push(component_item(
            StatusItemKind::Network,
            "Network is offline; local cached state remains available.",
            EventName::NetworkOffline,
        ));
    }
}

pub(super) fn apply_sync_operation_status(
    workspace_id: &WorkspaceId,
    counts: &SyncOperationCounts,
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    let pending = counts.queued
        + counts.claimed
        + counts.waiting_retry
        + counts.blocked_offline
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
    items.push(item);

    if counts.attention > 0 {
        *level = StatusLevel::Attention;
        attention_items.push("Sync queue needs attention.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync queue needs attention".to_string(),
            still_works: vec!["local files".to_string(), "status".to_string()],
            path: None,
        });
    } else if counts.blocked_offline > 0 {
        *level = StatusLevel::Limited;
        attention_items.push("Sync queue is waiting for offline recovery.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync queue is waiting for offline recovery".to_string(),
            still_works: sync_queue_wait_still_works(),
            path: None,
        });
    } else if counts.waiting_retry > 0 {
        *level = StatusLevel::Limited;
        attention_items.push("Sync queue is waiting for retry.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
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
        attention: counts.attention,
        completed: counts.completed,
    };
    status.has_pending_work().then_some(status)
}

pub(super) fn apply_unresolved_conflict_status(
    paths: &BTreeSet<String>,
    workspace_id: &WorkspaceId,
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    next_actions: &mut Vec<SafeAction>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    if paths.is_empty() {
        return Ok(());
    }

    *level = StatusLevel::Attention;
    let summary = if paths.len() == 1 {
        format!(
            "1 unresolved conflict needs attention: {}.",
            paths.iter().next().expect("path exists")
        )
    } else {
        format!("{} unresolved conflicts need attention.", paths.len())
    };
    attention_items.push(summary.clone());

    let mut item = base_status_item(StatusItemKind::Conflict, &summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    item.path = paths.iter().next().cloned();
    item.event_name = Some(EventName::ConflictBundleCreated);
    items.push(item);

    limits.push(LimitedCapability {
        capability: "sync".to_string(),
        unavailable_because: "unresolved conflict".to_string(),
        still_works: vec![
            "local files".to_string(),
            "status".to_string(),
            "conflict resolution".to_string(),
        ],
        path: None,
    });
    next_actions.push(conflict_resolution_action());
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
                event.name,
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
        "Sync queue: {} queued, {} running, {} waiting retry, {} offline, {} attention.",
        counts.queued,
        counts.claimed,
        counts.waiting_retry,
        counts.blocked_offline,
        counts.attention
    )
}

pub(super) fn hydration_progress_from_events(
    events: &[bowline_core::events::WorkspaceEvent],
) -> Vec<HydrationProgress> {
    let Some(event) = events
        .iter()
        .find(|event| event.name == EventName::HydrationCompleted)
        .or_else(|| {
            events
                .iter()
                .find(|event| event.name == EventName::HydrationBlocked)
        })
        .or_else(|| {
            events
                .iter()
                .find(|event| event.name == EventName::HydrationStarted)
        })
    else {
        return Vec::new();
    };
    let bytes = payload_u64(event, "bytes");
    let (bytes_done, bytes_remaining) = match event.name {
        EventName::HydrationCompleted => (bytes, 0),
        _ => (0, bytes),
    };
    let cause = payload_str(event, "cause").unwrap_or_else(|| event_name_label(event.name));
    vec![HydrationProgress {
        project_id: event.project_id.clone(),
        bytes_done,
        bytes_remaining,
        cause,
    }]
}

pub(super) fn payload_u64(event: &bowline_core::events::WorkspaceEvent, key: &str) -> u64 {
    event
        .payload
        .get(key)
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

pub(super) fn payload_str(
    event: &bowline_core::events::WorkspaceEvent,
    key: &str,
) -> Option<String> {
    event
        .payload
        .get(key)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}
