use serde::{Deserialize, Serialize};

mod facts;

pub use facts::*;

pub use crate::wire::DeviceApprovalAffordance;
use crate::{
    events::EventName,
    ids::{DeviceId, EnvRecordId, EventId, LeaseId, PolicyVersion, ProjectId, SnapshotId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
};

/// A concrete, runnable command that repairs the current workspace/account
/// state. Producers set `label`, `command`, and `mutates` directly from what
/// they emit — there is no command-string classifier. `mutates` drives the
/// TUI's confirm-before-run gate; it is never inferred from the command text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairCommand {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub mutates: bool,
}

impl RepairCommand {
    /// A read-only repair affordance (status/diff/review/help/navigation). The
    /// producer states, by choosing this constructor, that running `command`
    /// does not change workspace or account state.
    pub fn inspect(label: impl Into<String>, command: Option<String>) -> Self {
        Self {
            label: label.into(),
            command,
            mutates: false,
        }
    }

    /// A state-changing repair affordance (approve/discard/setup/resolve/apply).
    /// The producer states, by choosing this constructor, that running `command`
    /// mutates workspace or account state.
    pub fn mutating(label: impl Into<String>, command: Option<String>) -> Self {
        Self {
            label: label.into(),
            command,
            mutates: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusLevel {
    Healthy,
    Attention,
    Limited,
}

impl StatusLevel {
    /// Parse the kebab-case serialized label. Unknown labels return `None` so
    /// callers can choose the right fallback for their surface.
    pub fn from_status_label(label: &str) -> Option<Self> {
        match label {
            "healthy" => Some(Self::Healthy),
            "attention" => Some(Self::Attention),
            "limited" => Some(Self::Limited),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitObserverState {
    #[default]
    Ok,
    Partial,
    Unavailable,
}

impl GitObserverState {
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "ok" => Some(Self::Ok),
            "partial" => Some(Self::Partial),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    pub fn worst(left: Self, right: Self) -> Self {
        if left.rank() >= right.rank() {
            left
        } else {
            right
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Partial => 1,
            Self::Unavailable => 2,
        }
    }
}

impl std::str::FromStr for GitObserverState {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_wire(value).ok_or(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusScope {
    Project,
    Workspace,
    Lease,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub level: StatusLevel,
    pub attention_items: Vec<String>,
}

impl WorkspaceStatus {
    pub fn healthy() -> Self {
        Self {
            level: StatusLevel::Healthy,
            attention_items: Vec::new(),
        }
    }

    pub fn needs_attention(&self) -> bool {
        self.level != StatusLevel::Healthy || !self.attention_items.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreshnessVerdict {
    Current,
    Behind,
    Diverged,
    #[default]
    Unknown,
}

impl FreshnessVerdict {
    pub fn is_stale(self) -> bool {
        matches!(self, Self::Behind | Self::Diverged)
    }

    pub fn needs_attention(self) -> bool {
        !matches!(self, Self::Current)
    }

    pub fn worst(left: Self, right: Self) -> Self {
        if left.rank() >= right.rank() {
            left
        } else {
            right
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Current => 0,
            Self::Unknown => 1,
            Self::Behind => 2,
            Self::Diverged => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreshnessAxis {
    Snapshot,
    Git,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectSetupReadinessState {
    Unknown,
    Runnable,
    NeedsSetup,
    Blocked,
}

impl ProjectSetupReadinessState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Runnable => "runnable",
            Self::NeedsSetup => "needs-setup",
            Self::Blocked => "blocked",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "unknown" => Some(Self::Unknown),
            "runnable" => Some(Self::Runnable),
            "needs-setup" => Some(Self::NeedsSetup),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

impl std::str::FromStr for ProjectSetupReadinessState {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_wire(value).ok_or(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupReceiptState {
    Approved,
    ApprovalRequired,
    Completed,
    Failed,
}

impl SetupReceiptState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ApprovalRequired => "approval-required",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "approved" => Some(Self::Approved),
            "approval-required" => Some(Self::ApprovalRequired),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

impl std::str::FromStr for SetupReceiptState {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_wire(value).ok_or(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleBaseStatus {
    pub axis: FreshnessAxis,
    pub verdict: FreshnessVerdict,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot_id: Option<SnapshotId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_snapshot_id: Option<SnapshotId>,
}

impl StaleBaseStatus {
    pub fn git(
        verdict: FreshnessVerdict,
        summary: impl Into<String>,
        project_id: Option<ProjectId>,
        project_path: Option<String>,
        remedy_command: Option<String>,
    ) -> Self {
        Self {
            axis: FreshnessAxis::Git,
            verdict,
            summary: summary.into(),
            remedy_command,
            project_id,
            project_path,
            base_snapshot_id: None,
            latest_snapshot_id: None,
        }
    }

    pub fn snapshot(
        verdict: FreshnessVerdict,
        summary: impl Into<String>,
        project_id: Option<ProjectId>,
        project_path: Option<String>,
        base_snapshot_id: Option<SnapshotId>,
        latest_snapshot_id: Option<SnapshotId>,
        remedy_command: Option<String>,
    ) -> Self {
        Self {
            axis: FreshnessAxis::Snapshot,
            verdict,
            summary: summary.into(),
            remedy_command,
            project_id,
            project_path,
            base_snapshot_id,
            latest_snapshot_id,
        }
    }
}

pub fn freshness_verdict_for(stale_bases: &[StaleBaseStatus]) -> FreshnessVerdict {
    let mut verdicts = stale_bases.iter().map(|status| status.verdict);
    let Some(first) = verdicts.next() else {
        return FreshnessVerdict::Unknown;
    };
    verdicts.fold(first, FreshnessVerdict::worst)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSetupReadiness {
    pub state: ProjectSetupReadinessState,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_receipt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_receipt_state: Option<SetupReceiptState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusItemKind {
    Continuity,
    Policy,
    Device,
    Conflict,
    WorkView,
    Lease,
    Watcher,
    Env,
    Source,
    Setup,
    Metadata,
    Materialization,
    Network,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSubjectKind {
    Workspace,
    Root,
    Project,
    Path,
    Snapshot,
    EnvRecord,
    Policy,
    SetupReceipt,
    Conflict,
    WorkView,
    Lease,
    Overlay,
    Device,
    DeviceApprovalRequest,
    Metadata,
    Component,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSubject {
    pub kind: StatusSubjectKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusItem {
    pub kind: StatusItemKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<StatusSubject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<PathClassification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<MaterializationMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessFlag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<EventName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<LeaseId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<SnapshotId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<PolicyVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_record_id: Option<EnvRecordId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitedCapability {
    pub capability: String,
    #[serde(
        default,
        rename = "supportCapability",
        skip_serializing_if = "Option::is_none"
    )]
    pub support_capability: Option<ControlPlaneSupportCapability>,
    pub unavailable_because: String,
    pub still_works: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlPlaneSupportCapability {
    DeviceApproval,
    ProjectScopedWorkspaceRefCas,
    WorkView,
    AgentLease,
    EncryptedObjectStore,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentState {
    Ready,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkState {
    Online,
    Degraded,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusEvidenceLevel {
    Live,
    Cached,
    FakeAdapter,
    FixtureOnly,
    Unavailable,
    Unproven,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEvidence {
    pub level: StatusEvidenceLevel,
    pub summary: String,
}

impl StatusEvidence {
    pub fn live(summary: impl Into<String>) -> Self {
        Self {
            level: StatusEvidenceLevel::Live,
            summary: summary.into(),
        }
    }

    pub fn fake_adapter(summary: impl Into<String>) -> Self {
        Self {
            level: StatusEvidenceLevel::FakeAdapter,
            summary: summary.into(),
        }
    }

    pub fn unavailable(summary: impl Into<String>) -> Self {
        Self {
            level: StatusEvidenceLevel::Unavailable,
            summary: summary.into(),
        }
    }

    pub fn unproven(summary: impl Into<String>) -> Self {
        Self {
            level: StatusEvidenceLevel::Unproven,
            summary: summary.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncQueueStatus {
    pub queued: u64,
    pub claimed: u64,
    pub waiting_retry: u64,
    pub blocked_offline: u64,
    pub reconciliation_required: u64,
    pub attention: u64,
    pub completed: u64,
}

impl SyncQueueStatus {
    pub fn has_pending_work(&self) -> bool {
        self.queued
            + self.claimed
            + self.waiting_retry
            + self.blocked_offline
            + self.reconciliation_required
            + self.attention
            > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventWatermarks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scan_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_lag_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_state: Option<ComponentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher_state: Option<ComponentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_state: Option<NetworkState>,
}

#[cfg(test)]
mod freshness {
    use super::*;

    #[test]
    fn freshness_verdict_worst_orders_stale_before_unknown_and_current() {
        assert_eq!(
            FreshnessVerdict::worst(FreshnessVerdict::Current, FreshnessVerdict::Unknown),
            FreshnessVerdict::Unknown
        );
        assert_eq!(
            FreshnessVerdict::worst(FreshnessVerdict::Unknown, FreshnessVerdict::Behind),
            FreshnessVerdict::Behind
        );
        assert_eq!(
            FreshnessVerdict::worst(FreshnessVerdict::Behind, FreshnessVerdict::Diverged),
            FreshnessVerdict::Diverged
        );
    }

    #[test]
    fn freshness_verdict_for_empty_signal_is_unknown() {
        assert_eq!(freshness_verdict_for(&[]), FreshnessVerdict::Unknown);
    }

    #[test]
    fn freshness_verdict_for_proven_current_signal_is_current() {
        assert_eq!(
            freshness_verdict_for(&[StaleBaseStatus::git(
                FreshnessVerdict::Current,
                "current",
                None,
                None,
                None,
            )]),
            FreshnessVerdict::Current
        );
    }

    #[test]
    fn freshness_serializes_stable_status_strings() {
        let status = StaleBaseStatus::snapshot(
            FreshnessVerdict::Behind,
            "base snapshot is behind latest project snapshot",
            Some(ProjectId::new("proj_web")),
            Some("apps/web".to_string()),
            Some(SnapshotId::new("snap_base")),
            Some(SnapshotId::new("snap_latest")),
            Some("bowline status --watch".to_string()),
        );

        let value = serde_json::to_value(status).expect("stale base serializes");

        assert_eq!(value["axis"], "snapshot");
        assert_eq!(value["verdict"], "behind");
        assert_eq!(value["projectId"], "proj_web");
        assert_eq!(value["baseSnapshotId"], "snap_base");
        assert_eq!(value["latestSnapshotId"], "snap_latest");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSummary {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects_needing_attention: Vec<ProjectAttentionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_projects: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<ObservedWorkspaceSummary>,
}

impl WorkspaceSummary {
    pub fn empty() -> Self {
        Self {
            projects_needing_attention: Vec::new(),
            total_projects: None,
            observed: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedWorkspaceSummary {
    pub repo_count: u64,
    pub no_remote_repo_count: u64,
    pub stale_remote_tracking_repo_count: u64,
    pub git_partial_project_count: u64,
    pub git_unavailable_project_count: u64,
    pub generated_path_count: u64,
    pub dependency_path_count: u64,
    pub env_file_count: u64,
    pub untracked_file_count: u64,
    pub local_only_path_count: u64,
    pub blocked_path_count: u64,
    pub workspace_sync_path_count: u64,
}

impl ObservedWorkspaceSummary {
    pub fn reset_path_counts(&mut self) {
        self.generated_path_count = 0;
        self.dependency_path_count = 0;
        self.env_file_count = 0;
        self.local_only_path_count = 0;
        self.blocked_path_count = 0;
        self.workspace_sync_path_count = 0;
    }

    pub fn record_path(&mut self, classification: PathClassification, mode: MaterializationMode) {
        match classification {
            PathClassification::Generated | PathClassification::Cache => {
                self.generated_path_count += 1;
            }
            PathClassification::Dependency => self.dependency_path_count += 1,
            PathClassification::ProjectEnv => self.env_file_count += 1,
            PathClassification::Blocked => self.blocked_path_count += 1,
            _ => {}
        }
        match mode {
            MaterializationMode::WorkspaceSync | MaterializationMode::EncryptedSync => {
                self.workspace_sync_path_count += 1;
            }
            MaterializationMode::LocalOnly | MaterializationMode::Ignore => {
                self.local_only_path_count += 1;
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAttentionSummary {
    pub project_id: ProjectId,
    pub path: String,
    pub level: StatusLevel,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::{RepairCommand, StatusLevel, WorkspaceStatus};

    #[test]
    fn healthy_status_is_quiet() {
        assert!(!WorkspaceStatus::healthy().needs_attention());
    }

    #[test]
    fn limited_status_needs_attention() {
        let status = WorkspaceStatus {
            attention_items: Vec::new(),
            level: StatusLevel::Limited,
        };

        assert!(status.needs_attention());
    }

    #[test]
    fn status_level_from_status_label_matches_serde_names() {
        for level in [
            StatusLevel::Healthy,
            StatusLevel::Attention,
            StatusLevel::Limited,
        ] {
            let value = serde_json::to_value(level).expect("status level serializes");
            let name = value.as_str().expect("status level serializes as string");

            assert_eq!(StatusLevel::from_status_label(name), Some(level));
        }
    }

    #[test]
    fn repair_command_mutation_flag_is_producer_set() {
        let inspect = RepairCommand::inspect(
            "Inspect recovery status",
            Some("bowline recover status".to_string()),
        );
        let mutating = RepairCommand::mutating(
            "Verify Recovery Key",
            Some("bowline recover verify rk_1".to_string()),
        );

        assert!(!inspect.mutates);
        assert!(mutating.mutates);
    }

    #[test]
    fn repair_command_serializes_camel_case_fields() {
        let value = serde_json::to_value(RepairCommand::mutating(
            "Resolve conflicts",
            Some("bowline resolve ~/Code".to_string()),
        ))
        .expect("repair command serializes");

        assert_eq!(value["label"], "Resolve conflicts");
        assert_eq!(value["command"], "bowline resolve ~/Code");
        assert_eq!(value["mutates"], true);
    }
}
