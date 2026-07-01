/// Redacted live workspace status snapshot published by a trusted device (the
/// daemon) to the control plane so the dashboard can show sync/index/watcher
/// posture. Paths are workspace-relative and secrets are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatusSnapshot {
    pub workspace_id: String,
    pub snapshot_id: String,
    /// One of "healthy" | "attention" | "limited".
    pub status_level: String,
    pub attention_items: Vec<String>,
    pub generated_at: String,
    pub event_watermarks: StatusEventWatermarks,
    pub sync_queue: Option<StatusSyncQueueSnapshot>,
    pub index: Option<StatusIndexSnapshot>,
    pub workspace_summary: Option<StatusWorkspaceSummarySnapshot>,
    pub items: Vec<StatusItemSnapshot>,
    pub limits: Vec<StatusLimitSnapshot>,
    pub published_by_device_id: String,
}

impl WorkspaceStatusSnapshot {
    /// Canonical proof subject the daemon signs for the
    /// `status:publishWorkspaceStatus` mutation. Must stay byte-for-byte in sync
    /// with `statusPublishProofSubject` on the Convex side.
    pub fn proof_subject(&self) -> String {
        format!(
            "workspaceId={}\nsnapshotId={}\nstatusLevel={}\ngeneratedAt={}",
            self.workspace_id, self.snapshot_id, self.status_level, self.generated_at
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusEventWatermarks {
    pub last_event_id: Option<String>,
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
    pub attention: u64,
    pub completed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusIndexSnapshot {
    pub state: String,
    pub file_count: u64,
    pub path_count: u64,
    pub summary: String,
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
    pub unavailable_because: String,
    pub path: Option<String>,
    pub still_works: Vec<String>,
}
