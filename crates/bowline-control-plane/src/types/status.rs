use bowline_core::ids::{DeviceId, EventId, SnapshotId, WorkspaceId};
use bowline_core::status::StatusFact;

/// Redacted live workspace status snapshot published by a trusted device (the
/// daemon) to the control plane so the dashboard can show sync/watcher
/// posture. Paths are workspace-relative and secrets are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatusSnapshot {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub availability: String,
    pub attention: String,
    pub primary_fact_id: Option<String>,
    pub facts: Vec<StatusFact>,
    pub freshness: String,
    pub schema_hash: String,
    pub snapshot_version: u64,
    pub producer_version: String,
    pub observed_at: String,
    pub attention_items: Vec<String>,
    pub event_watermarks: StatusEventWatermarks,
    pub sync_queue: Option<StatusSyncQueueSnapshot>,
    pub workspace_summary: Option<StatusWorkspaceSummarySnapshot>,
    pub items: Vec<StatusItemSnapshot>,
    pub limits: Vec<StatusLimitSnapshot>,
    pub published_by_device_id: DeviceId,
}

impl WorkspaceStatusSnapshot {
    /// Canonical proof subject the daemon signs for the
    /// `status:publishWorkspaceStatus` mutation. Fixture-first changes live in
    /// `tests/contracts/proofs/device-proof-subjects.json`.
    pub fn proof_subject(&self) -> String {
        format!(
            "workspaceId={}\nsnapshotId={}\navailability={}\nattention={}\nschemaHash={}\nsnapshotVersion={}\nobservedAt={}",
            self.workspace_id.as_str(),
            self.snapshot_id.as_str(),
            self.availability,
            self.attention,
            self.schema_hash,
            self.snapshot_version,
            self.observed_at
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusEventWatermarks {
    pub last_event_id: Option<EventId>,
    pub last_scan_at: Option<String>,
    pub sync_state: Option<String>,
    pub watcher_state: Option<String>,
    pub network_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSyncQueueSnapshot {
    pub queued: u64,
    pub claimed: u64,
    pub waiting_retry: u64,
    pub blocked_offline: u64,
    pub reconciliation_required: u64,
    pub attention: u64,
    pub completed: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusWorkspaceSummarySnapshot {
    pub total_projects: Option<u64>,
    pub repo_count: Option<u64>,
    pub env_file_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusItemSnapshot {
    pub kind: String,
    pub summary: String,
    pub path: Option<String>,
    pub event_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLimitSnapshot {
    pub capability: String,
    pub support_capability: Option<String>,
    pub unavailable_because: String,
    pub path: Option<String>,
    pub still_works: Vec<String>,
}
