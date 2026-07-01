use super::*;

fn sample_snapshot() -> WorkspaceStatusSnapshot {
    WorkspaceStatusSnapshot {
        workspace_id: "ws_code".to_string(),
        snapshot_id: "snap_abc123".to_string(),
        status_level: "attention".to_string(),
        attention_items: vec!["device approval pending".to_string()],
        generated_at: "2026-06-29T12:00:00Z".to_string(),
        event_watermarks: StatusEventWatermarks::default(),
        sync_queue: None,
        index: None,
        workspace_summary: None,
        items: Vec::new(),
        limits: Vec::new(),
        published_by_device_id: "device-daemon".to_string(),
    }
}

#[test]
fn status_publish_proof_subject_matches_convex_contract() {
    let snapshot = sample_snapshot();
    assert_eq!(
        snapshot.proof_subject(),
        "workspaceId=ws_code\nsnapshotId=snap_abc123\nstatusLevel=attention\ngeneratedAt=2026-06-29T12:00:00Z"
    );
}

#[test]
fn publish_workspace_status_is_noop_for_in_memory_client() {
    let client = FakeControlPlaneClient::default();
    assert!(ControlPlaneClient::publish_workspace_status(&client, &sample_snapshot()).is_ok());
}
