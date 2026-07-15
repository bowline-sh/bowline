use super::*;

pub(super) fn apply_event_status(
    event: &bowline_core::events::WorkspaceEvent,
    acc: &mut StatusAccumulator,
) {
    let scope = event_fact_scope(event);
    let scope_id = event_fact_scope_id(event, scope);
    if let Some(kind) = event_fact_kind(&event.name) {
        acc.observe_fact(
            kind,
            format!("event:{}", event.id.as_str()),
            status_signal_key(event).unwrap_or_else(|| format!("event:{}", event.id.as_str())),
            scope,
            scope_id,
        );
    } else if event.severity != EventSeverity::Info {
        let (availability, attention) = match event.severity {
            EventSeverity::Info => (StatusFactAvailabilityImpact::None, StatusAttention::None),
            EventSeverity::Attention => (
                StatusFactAvailabilityImpact::None,
                StatusAttention::Required,
            ),
            EventSeverity::Limited => (
                StatusFactAvailabilityImpact::Degraded,
                StatusAttention::None,
            ),
        };
        acc.observe_aggregate_fact(
            format!("event:{}", event.id.as_str()),
            status_signal_key(event).unwrap_or_else(|| format!("event:{}", event.id.as_str())),
            scope,
            scope_id,
            availability,
            attention,
        );
    }
    if event.severity != EventSeverity::Info {
        acc.attention_items.push(event.summary.clone());
    }

    if event.severity != EventSeverity::Info {
        let mut item = base_status_item(status_item_kind_for_event(&event.name), &event.summary);
        item.subject = event.subject.as_ref().map(|subject| StatusSubject {
            kind: status_subject_kind(subject.kind),
            id: subject.id.clone(),
            path: subject.path.clone(),
        });
        item.path = event.path.clone();
        item.event_id = Some(event.id.clone());
        item.event_name = Some(event.name.clone());
        item.device_id = event.device_id.clone();
        item.lease_id = event.lease_id.clone();
        item.project_id = event.project_id.clone();
        acc.items.push(item);
    }
}

fn event_fact_scope_id(
    event: &bowline_core::events::WorkspaceEvent,
    scope: StatusFactScope,
) -> Option<&str> {
    match scope {
        StatusFactScope::Workspace => Some(event.workspace_id.as_str()),
        StatusFactScope::Device => event
            .device_id
            .as_ref()
            .map(DeviceId::as_str)
            .or_else(|| event.subject.as_ref().map(|subject| subject.id.as_str())),
        _ => event.subject.as_ref().map(|subject| subject.id.as_str()),
    }
}

fn event_fact_scope(event: &bowline_core::events::WorkspaceEvent) -> StatusFactScope {
    event
        .subject
        .as_ref()
        .map_or(StatusFactScope::Workspace, |subject| match subject.kind {
            EventSubjectKind::Workspace
            | EventSubjectKind::Root
            | EventSubjectKind::Metadata
            | EventSubjectKind::Component => StatusFactScope::Workspace,
            EventSubjectKind::Project
            | EventSubjectKind::Snapshot
            | EventSubjectKind::SetupReceipt => StatusFactScope::Project,
            EventSubjectKind::Path
            | EventSubjectKind::Content
            | EventSubjectKind::Pack
            | EventSubjectKind::Policy
            | EventSubjectKind::EnvRecord
            | EventSubjectKind::Conflict
            | EventSubjectKind::Overlay => StatusFactScope::Path,
            EventSubjectKind::Lease => StatusFactScope::Lease,
            EventSubjectKind::WorkView => StatusFactScope::WorkView,
            EventSubjectKind::Device => StatusFactScope::Device,
        })
}

fn event_fact_kind(name: &EventName) -> Option<&'static str> {
    Some(match name {
        EventName::ConflictCreated
        | EventName::ConflictBundleCreated
        | EventName::ConflictResolutionProposed => "sync.conflict_unresolved",
        EventName::DeviceApprovalRequested => "device.approval_requested",
        EventName::SetupBlocked => "setup.blocked",
        EventName::HydrationBlocked
        | EventName::DaemonDegraded
        | EventName::SyncLimited
        | EventName::SyncDegraded
        | EventName::StatCacheDivergence => "sync.component_degraded",
        EventName::PolicyNeedsApproval => "policy.path_blocked",
        EventName::LeaseReviewReady => "lease.review_ready",
        EventName::WatcherDegraded => "watcher.degraded",
        EventName::NetworkOffline => "network.offline",
        EventName::WorkReviewReady => "work_view.review_ready",
        EventName::MetadataCorrupt => "metadata.corrupt",
        EventName::SourceStale => "snapshot.base_behind",
        EventName::Unknown(_) => return None,
        _ => return None,
    })
}

pub(super) fn apply_status_signal_events(
    events: &[bowline_core::events::WorkspaceEvent],
    watermarks: &EventWatermarks,
    unresolved_conflict_paths: &BTreeSet<String>,
    acc: &mut StatusAccumulator,
) {
    let mut cleared = HashSet::new();
    let mut applied = HashSet::new();

    for event in events {
        for key in status_clear_keys(event) {
            cleared.insert(key);
        }

        let Some(key) = status_signal_key(event) else {
            continue;
        };
        if cleared.contains(&key) || applied.contains(&key) {
            continue;
        }
        if is_conflict_signal(event)
            && !conflict_signal_is_unresolved(event, unresolved_conflict_paths)
        {
            continue;
        }
        if should_apply_event_status(event, watermarks) {
            apply_event_status(event, acc);
            applied.insert(key);
        }
    }
}

pub(super) fn is_conflict_signal(event: &bowline_core::events::WorkspaceEvent) -> bool {
    matches!(
        &event.name,
        EventName::ConflictCreated
            | EventName::ConflictBundleCreated
            | EventName::ConflictResolutionProposed
    )
}

pub(super) fn conflict_signal_is_unresolved(
    event: &bowline_core::events::WorkspaceEvent,
    unresolved_conflict_paths: &BTreeSet<String>,
) -> bool {
    if unresolved_conflict_paths.is_empty() {
        return false;
    }
    event
        .path
        .as_deref()
        .or_else(|| {
            event
                .subject
                .as_ref()
                .and_then(|subject| subject.path.as_deref())
        })
        .is_none_or(|path| unresolved_conflict_paths.contains(path))
}

pub(super) fn status_clear_keys(event: &bowline_core::events::WorkspaceEvent) -> Vec<String> {
    let categories: &[&str] = match &event.name {
        EventName::ConflictResolutionAccepted | EventName::ConflictResolutionRejected => {
            &["conflict"]
        }
        EventName::DeviceApproved | EventName::DeviceRevoked => &["device-approval"],
        EventName::SetupCompleted => &["setup"],
        EventName::HydrationCompleted => &["materialization"],
        EventName::PolicyChanged => &["policy"],
        EventName::LeaseCreated
        | EventName::LeaseUpdated
        | EventName::LeaseDispatched
        | EventName::LeaseClaimed
        | EventName::LeaseCompleted
        | EventName::LeaseReviewReady => &["lease"],
        EventName::DaemonRecovered => &["daemon"],
        EventName::SyncCompleted | EventName::SyncRecovered => &["sync"],
        EventName::WatcherRecovered => &["watcher"],
        EventName::NetworkRecovered => &["network"],
        EventName::WorkAccepted
        | EventName::WorkCleanupCompleted
        | EventName::WorkDiscarded
        | EventName::WorkRestored => &["work-view"],
        _ => &[],
    };

    categories
        .iter()
        .map(|category| status_key(category, event))
        .collect()
}

pub(super) fn status_signal_key(event: &bowline_core::events::WorkspaceEvent) -> Option<String> {
    if event.severity == EventSeverity::Info {
        return None;
    }

    let category = match &event.name {
        EventName::ConflictCreated
        | EventName::ConflictBundleCreated
        | EventName::ConflictResolutionProposed => "conflict".to_string(),
        EventName::DeviceApprovalRequested => "device-approval".to_string(),
        EventName::SetupBlocked => "setup".to_string(),
        EventName::HydrationBlocked => "materialization".to_string(),
        EventName::PolicyNeedsApproval => "policy".to_string(),
        EventName::LeaseReviewReady => "lease".to_string(),
        EventName::DaemonDegraded => "daemon".to_string(),
        EventName::SyncLimited | EventName::SyncDegraded => "sync".to_string(),
        EventName::WatcherDegraded => "watcher".to_string(),
        EventName::NetworkOffline => "network".to_string(),
        EventName::WorkReviewReady => "work-view".to_string(),
        _ => event_name_label(&event.name),
    };

    Some(status_key(&category, event))
}

pub(super) fn status_key(category: &str, event: &bowline_core::events::WorkspaceEvent) -> String {
    let identity = if category == "setup" {
        status_path_or_project_identity(event)
    } else {
        status_identity(event)
    };
    format!("{category}:{identity}")
}

pub(super) fn status_path_or_project_identity(
    event: &bowline_core::events::WorkspaceEvent,
) -> String {
    if let Some(path) = &event.path {
        return format!("path:{path}");
    }
    if let Some(subject) = &event.subject
        && let Some(path) = &subject.path
    {
        return format!("path:{path}");
    }
    if let Some(project_id) = &event.project_id {
        return format!("project:{}", project_id.as_str());
    }
    status_identity(event)
}

pub(super) fn status_identity(event: &bowline_core::events::WorkspaceEvent) -> String {
    if let Some(subject) = &event.subject {
        if !subject.id.is_empty() {
            return format!("subject:{}", subject.id);
        }
        if let Some(path) = &subject.path {
            return format!("path:{path}");
        }
    }
    if let Some(path) = &event.path {
        return format!("path:{path}");
    }
    if let Some(lease_id) = &event.lease_id {
        return format!("lease:{}", lease_id.as_str());
    }
    if let Some(device_id) = &event.device_id {
        return format!("device:{}", device_id.as_str());
    }
    if let Some(project_id) = &event.project_id {
        return format!("project:{}", project_id.as_str());
    }
    format!("workspace:{}", event.workspace_id.as_str())
}

pub(super) fn should_apply_event_status(
    event: &bowline_core::events::WorkspaceEvent,
    watermarks: &EventWatermarks,
) -> bool {
    match &event.name {
        EventName::SyncLimited | EventName::SyncDegraded => matches!(
            watermarks.sync_state,
            Some(ComponentState::Degraded | ComponentState::Unavailable)
        ),
        EventName::WatcherDegraded => matches!(
            watermarks.watcher_state,
            Some(ComponentState::Degraded | ComponentState::Unavailable)
        ),
        EventName::NetworkOffline => matches!(
            watermarks.network_state,
            Some(NetworkState::Offline | NetworkState::Degraded)
        ),
        _ => true,
    }
}

pub(super) fn status_subject_kind(kind: EventSubjectKind) -> StatusSubjectKind {
    match kind {
        EventSubjectKind::Workspace => StatusSubjectKind::Workspace,
        EventSubjectKind::Root => StatusSubjectKind::Root,
        EventSubjectKind::Project => StatusSubjectKind::Project,
        EventSubjectKind::Path | EventSubjectKind::Content | EventSubjectKind::Pack => {
            StatusSubjectKind::Path
        }
        EventSubjectKind::Snapshot => StatusSubjectKind::Snapshot,
        EventSubjectKind::Policy => StatusSubjectKind::Policy,
        EventSubjectKind::EnvRecord => StatusSubjectKind::EnvRecord,
        EventSubjectKind::SetupReceipt => StatusSubjectKind::SetupReceipt,
        EventSubjectKind::Conflict => StatusSubjectKind::Conflict,
        EventSubjectKind::WorkView => StatusSubjectKind::WorkView,
        EventSubjectKind::Lease => StatusSubjectKind::Lease,
        EventSubjectKind::Overlay => StatusSubjectKind::Overlay,
        EventSubjectKind::Device => StatusSubjectKind::Device,
        EventSubjectKind::Metadata => StatusSubjectKind::Metadata,
        EventSubjectKind::Component => StatusSubjectKind::Component,
    }
}

pub(super) fn status_item_kind_for_event(name: &EventName) -> StatusItemKind {
    match name {
        EventName::PolicyClassified | EventName::PolicyNeedsApproval | EventName::PolicyChanged => {
            StatusItemKind::Policy
        }
        EventName::DeviceApprovalRequested
        | EventName::DeviceApproved
        | EventName::DeviceDenied
        | EventName::DeviceRevoked => StatusItemKind::Device,
        EventName::ConflictCreated
        | EventName::ConflictBundleCreated
        | EventName::ConflictResolutionProposed
        | EventName::ConflictResolutionAccepted
        | EventName::ConflictResolutionRejected => StatusItemKind::Conflict,
        EventName::LeaseCreated
        | EventName::LeaseUpdated
        | EventName::LeaseDispatched
        | EventName::LeaseClaimed
        | EventName::LeaseCompleted
        | EventName::LeaseCancelled
        | EventName::LeaseExtended
        | EventName::LeaseReviewReady => StatusItemKind::Lease,
        EventName::WorkCreated
        | EventName::WorkUpdated
        | EventName::WorkReviewReady
        | EventName::WorkAccepted
        | EventName::WorkDiscarded
        | EventName::WorkRestored
        | EventName::WorkCleanupPreviewed
        | EventName::WorkCleanupCompleted => StatusItemKind::WorkView,
        EventName::WatcherDegraded | EventName::WatcherRecovered => StatusItemKind::Watcher,
        EventName::EnvImported | EventName::EnvMaterialized | EventName::EnvRevoked => {
            StatusItemKind::Env
        }
        EventName::HydrationStarted
        | EventName::HydrationCompleted
        | EventName::HydrationBlocked => StatusItemKind::Materialization,
        EventName::SourceStale
        | EventName::NamespaceCreated
        | EventName::NamespaceMoved
        | EventName::NamespaceDeletedOrArchived => StatusItemKind::Source,
        EventName::SetupStarted | EventName::SetupCompleted | EventName::SetupBlocked => {
            StatusItemKind::Setup
        }
        EventName::SyncStarted
        | EventName::SyncCompleted
        | EventName::SyncLimited
        | EventName::SyncDegraded
        | EventName::StatCacheDivergence
        | EventName::SyncRecovered
        | EventName::MergePluginApplied => StatusItemKind::Materialization,
        EventName::NetworkOffline | EventName::NetworkRecovered => StatusItemKind::Network,
        EventName::MetadataCorrupt
        | EventName::DaemonDegraded
        | EventName::DaemonRecovered
        | EventName::RecoveryKeyCreated
        | EventName::RecoveryKeyVerified
        | EventName::RecoveryKeyRotated
        | EventName::RecoveryKeyRevoked
        | EventName::AuthLoginStarted
        | EventName::AuthLoginCompleted
        | EventName::OverlayChanged
        | EventName::Unknown(_) => StatusItemKind::Metadata,
    }
}
