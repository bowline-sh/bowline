use bowline_core::{
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{DeviceId, EventId, ProjectId, SnapshotId, WorkViewId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::{
        FreshnessAxis, FreshnessVerdict, LimitedCapability, ProjectSetupReadinessState,
        StatusAttention, StatusAvailability, StatusFact, StatusFactScope, StatusItemKind,
        StatusLevel,
    },
    work_views::{
        OVERLAY_HEAD_EMPTY, WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    },
};

use crate::{
    metadata::{MetadataStore, ObservedLocalPath, SetupReceiptRecord},
    status::StatusOptions,
    workspace::TempWorkspace,
};

use super::{
    EventQuery, EventsOptions, LocalStatusCollection, LocalStatusError, LocalStatusFactCollector,
    RevisionedStatus, RevisionedStatusComposer, STATUS_SAFETY_REFRESH_INTERVAL, StatusAccumulator,
    StatusComposerMetrics, apply_event_status, base_status_item, compose_events, compose_status,
    empty_watermarks, initial_watch_frame, missing_metadata_status, redact_workspace_path,
    redacted_status_snapshot, reduce_status_facts, render_events_human,
};

mod compose;
mod events;
mod sync;

fn seed_workspace_root(store: &MetadataStore, workspace_id: &WorkspaceId) {
    store
        .insert_workspace(workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", workspace_id, "~/Code", "2026-06-23T12:00:00Z")
        .expect("root insert");
}

fn seed_project(
    store: &MetadataStore,
    project_id: &ProjectId,
    workspace_id: &WorkspaceId,
    root_id: &str,
    path: &str,
) {
    store
        .insert_project(
            project_id,
            workspace_id,
            root_id,
            path,
            "2026-06-23T12:00:00Z",
        )
        .expect("project insert");
    store
        .set_project_latest_snapshot_id(
            workspace_id,
            project_id,
            &SnapshotId::new(format!("snap_{}", project_id.as_str())),
        )
        .expect("project latest snapshot");
}

fn project_event(
    id: &str,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    path: &str,
    severity: EventSeverity,
    summary: &str,
) -> WorkspaceEvent {
    let mut event = WorkspaceEvent::new(
        EventId::new(id),
        EventName::SourceStale,
        "2026-06-23T12:00:00Z",
        severity,
        summary,
        workspace_id.clone(),
    );
    event.project_id = Some(project_id.clone());
    event.path = Some(path.to_string());
    event
}
