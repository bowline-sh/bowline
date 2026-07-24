use super::*;
use bowline_core::ids::{DeviceId, SnapshotId, WorkspaceId};

fn sample_snapshot() -> WorkspaceStatusSnapshot {
    WorkspaceStatusSnapshot {
        workspace_id: WorkspaceId::new("ws_code"),
        snapshot_id: SnapshotId::new("snap_abc123"),
        availability: "ready".to_string(),
        attention: "required".to_string(),
        primary_fact_id: None,
        facts: Vec::new(),
        freshness: "fresh".to_string(),
        schema_hash: bowline_core::wire::WIRE_SCHEMA_HASH.to_string(),
        snapshot_version: 1,
        producer_version: "0.1.1".to_string(),
        observed_at: "2026-06-29T12:00:00Z".to_string(),
        attention_items: vec!["device approval pending".to_string()],
        event_watermarks: StatusEventWatermarks::default(),
        sync_queue: None,
        workspace_summary: None,
        items: Vec::new(),
        limits: Vec::new(),
        published_by_device_id: DeviceId::new("device-daemon"),
    }
}

#[test]
fn status_publish_proof_subject_matches_convex_contract() {
    let snapshot = sample_snapshot();
    assert_eq!(
        snapshot.proof_subject(),
        format!(
            "workspaceId=ws_code\nsnapshotId=snap_abc123\navailability=ready\nattention=required\nschemaHash={}\nsnapshotVersion=1\nobservedAt=2026-06-29T12:00:00Z",
            bowline_core::wire::WIRE_SCHEMA_HASH
        )
    );
}

#[test]
fn publish_workspace_status_is_noop_for_in_memory_client() {
    let client = FakeControlPlaneClient::default();
    assert!(
        WorkspaceControlPlaneClient::publish_workspace_status(&client, &sample_snapshot()).is_ok()
    );
}
