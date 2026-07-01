use bowline_core::{
    commands::{AgentLeaseBase, HydrationBudgetState, IndexState},
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId},
    status::{LimitedCapability, StatusItemKind, StatusLevel},
};

use crate::{
    agents::{AgentLeaseCreateOptions, create_agent_lease},
    metadata::{MetadataStore, SyncOperationRecord},
    status::StatusOptions,
    sync::conflicts::{ConflictFile, ConflictRecord, create_conflict_bundle},
    workspace::TempWorkspace,
};

use super::{
    EventsOptions, LocalStatusError, base_status_item, compose_events, compose_status,
    initial_watch_frame, missing_metadata_status, redact_workspace_path, redacted_status_snapshot,
    render_events_human, render_status_human,
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

fn sync_operation_record(
    id: &str,
    workspace_id: &WorkspaceId,
    state: &str,
    idempotency_key: &str,
) -> SyncOperationRecord {
    SyncOperationRecord {
        id: id.to_string(),
        workspace_id: workspace_id.clone(),
        kind: "daemon_tick".to_string(),
        state: state.to_string(),
        idempotency_key: idempotency_key.to_string(),
        base_version: Some(1),
        base_snapshot_id: Some("snap_base".to_string()),
        target_snapshot_id: Some("snap_target".to_string()),
        device_id: Some(DeviceId::new("device-test")),
        payload_json: "{}".to_string(),
        attempt_count: 0,
        claimed_by: None,
        heartbeat_at: None,
        next_attempt_at: None,
        last_error: None,
        created_at: "2026-06-23T12:00:00Z".to_string(),
        updated_at: "2026-06-23T12:00:00Z".to_string(),
    }
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
