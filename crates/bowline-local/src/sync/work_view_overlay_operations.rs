use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef};
use bowline_core::ids::{DeviceId, SnapshotId, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::metadata::{
    MetadataError, MetadataStore, SyncOperationKind, SyncOperationRecord, SyncOperationState,
    SyncResourceKey,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlaySyncInput {
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub workspace_version: u64,
    pub snapshot_id: SnapshotId,
    pub generated_at: String,
}

impl WorkViewOverlaySyncInput {
    pub fn workspace_ref(&self) -> WorkspaceRef {
        WorkspaceRef {
            workspace_id: self.workspace_id.clone(),
            version: self.workspace_version,
            snapshot_id: self.snapshot_id.clone(),
            updated_at: ControlPlaneTimestamp { tick: 0 },
            updated_by_device_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkViewOverlaySyncPayload {
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    workspace_version: u64,
    snapshot_id: SnapshotId,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkViewOverlaySyncResult {
    pub uploaded: u64,
    pub attention: u64,
    pub entries_total: u64,
    pub entries_completed: u64,
    pub content_objects_uploaded: u64,
    pub content_objects_reused: u64,
    pub plaintext_bytes: u64,
    pub uploaded_bytes: u64,
}

pub fn work_view_overlay_sync_operation(
    workspace_ref: &WorkspaceRef,
    device_id: &DeviceId,
    generated_at: &str,
) -> Result<SyncOperationRecord, serde_json::Error> {
    let payload = WorkViewOverlaySyncPayload {
        workspace_id: workspace_ref.workspace_id.clone(),
        device_id: device_id.clone(),
        workspace_version: workspace_ref.version,
        snapshot_id: workspace_ref.snapshot_id.clone(),
    };
    let idempotency_key = format!(
        "work-view-overlay-sync:{}:{}:{}:{}",
        payload.workspace_id.as_str(),
        payload.workspace_version,
        payload.snapshot_id.as_str(),
        payload.device_id.as_str()
    );
    let operation_id = format!(
        "work-view-overlay-sync-{:020}-{}",
        payload.workspace_version,
        super::short_hash([idempotency_key.as_bytes()])
    );
    Ok(SyncOperationRecord {
        id: operation_id,
        workspace_id: payload.workspace_id.clone(),
        kind: SyncOperationKind::WorkViewOverlaySync,
        resource_key: SyncResourceKey::post_commit(payload.workspace_id.clone()),
        state: SyncOperationState::Queued,
        idempotency_key,
        base_version: Some(payload.workspace_version),
        base_snapshot_id: Some(payload.snapshot_id.as_str().to_string()),
        target_snapshot_id: Some(payload.snapshot_id.as_str().to_string()),
        device_id: Some(payload.device_id.clone()),
        payload_json: serde_json::to_string(&payload)?,
        attempt_count: 0,
        claimed_by: None,
        claim_generation: 0,
        heartbeat_at: None,
        lease_expires_at: None,
        cancellation_requested_at: None,
        next_attempt_at: None,
        result_json: None,
        last_error_code: None,
        last_error: None,
        created_at: generated_at.to_string(),
        updated_at: generated_at.to_string(),
    })
}

pub fn pending_work_view_overlay_sync_operation(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) -> Result<Option<SyncOperationRecord>, MetadataError> {
    store
        .workspace_sync_head(workspace_id)?
        .map(|head| {
            work_view_overlay_sync_operation(&head.workspace_ref, device_id, &head.observed_at)
                .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))
        })
        .transpose()
}

pub fn decode_work_view_overlay_sync_operation(
    operation: &SyncOperationRecord,
) -> Result<WorkViewOverlaySyncInput, serde_json::Error> {
    let payload = serde_json::from_str::<WorkViewOverlaySyncPayload>(&operation.payload_json)?;
    Ok(WorkViewOverlaySyncInput {
        workspace_id: payload.workspace_id,
        device_id: payload.device_id,
        workspace_version: payload.workspace_version,
        snapshot_id: payload.snapshot_id,
        generated_at: operation.created_at.clone(),
    })
}

pub fn work_view_overlay_sync_result(
    result: WorkViewOverlaySyncResult,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_identity_is_stable_for_committed_ref_snapshot_and_device() {
        let workspace_ref = WorkspaceRef {
            workspace_id: WorkspaceId::new("ws_overlay"),
            version: 42,
            snapshot_id: SnapshotId::new("snap_overlay"),
            updated_at: ControlPlaneTimestamp { tick: 9 },
            updated_by_device_id: Some(DeviceId::new("dev_writer")),
        };
        let first = work_view_overlay_sync_operation(
            &workspace_ref,
            &DeviceId::new("dev_worker"),
            "2026-07-13T10:00:00Z",
        )
        .expect("operation");
        let mut refreshed_ref = workspace_ref.clone();
        refreshed_ref.updated_at = ControlPlaneTimestamp { tick: 99 };
        refreshed_ref.updated_by_device_id = Some(DeviceId::new("dev_refresher"));
        let second = work_view_overlay_sync_operation(
            &refreshed_ref,
            &DeviceId::new("dev_worker"),
            "2026-07-13T10:05:00Z",
        )
        .expect("operation");

        assert_eq!(first.id, second.id);
        assert_eq!(first.idempotency_key, second.idempotency_key);
        assert_eq!(first.payload_json, second.payload_json);
        assert_ne!(first.created_at, second.created_at);
        assert_eq!(first.kind, SyncOperationKind::WorkViewOverlaySync);
        assert_eq!(
            first.resource_key,
            SyncResourceKey::post_commit(WorkspaceId::new("ws_overlay"))
        );
        assert_eq!(
            decode_work_view_overlay_sync_operation(&first)
                .expect("first input")
                .generated_at,
            "2026-07-13T10:00:00Z"
        );
    }
}
