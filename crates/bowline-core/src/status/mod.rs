use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use crate::{
    events::EventName,
    ids::{DeviceId, EnvRecordId, EventId, LeaseId, PolicyVersion, ProjectId, SnapshotId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusLevel {
    Healthy,
    Attention,
    Limited,
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
    Hydration,
    Source,
    Setup,
    Metadata,
    Materialization,
    Network,
    Index,
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
    Hydration,
    Lease,
    Overlay,
    Device,
    DeviceApprovalRequest,
    Metadata,
    Component,
    Index,
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
    pub unavailable_because: String,
    pub still_works: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HydrationProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub bytes_done: u64,
    pub bytes_remaining: u64,
    pub cause: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncQueueStatus {
    pub queued: u64,
    pub claimed: u64,
    pub waiting_retry: u64,
    pub blocked_offline: u64,
    pub attention: u64,
    pub completed: u64,
}

impl SyncQueueStatus {
    pub fn has_pending_work(&self) -> bool {
        self.queued + self.claimed + self.waiting_retry + self.blocked_offline + self.attention > 0
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SafeActionEffect {
    Inspect,
    Trust,
    Setup,
    Mutate,
    Destructive,
}

impl SafeActionEffect {
    pub fn requires_confirmation(self) -> bool {
        !matches!(self, Self::Inspect)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SafeActionTarget {
    Workspace,
    Device,
    Setup,
    WorkView,
    Conflict,
    Agent,
    Recovery,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeAction {
    pub label: String,
    #[serde(default)]
    pub command: Option<String>,
}

impl SafeAction {
    pub fn effect_category(&self) -> SafeActionEffect {
        classify_action(self.command.as_deref()).0
    }

    pub fn target_kind(&self) -> SafeActionTarget {
        classify_action(self.command.as_deref()).1
    }
}

impl Serialize for SafeAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("SafeAction", 4)?;
        state.serialize_field("label", &self.label)?;
        if let Some(command) = &self.command {
            state.serialize_field("command", command)?;
        }
        state.serialize_field("effectCategory", &self.effect_category())?;
        state.serialize_field("targetKind", &self.target_kind())?;
        state.end()
    }
}

fn classify_action(command: Option<&str>) -> (SafeActionEffect, SafeActionTarget) {
    let Some(command) = command else {
        return (SafeActionEffect::Inspect, SafeActionTarget::Unknown);
    };
    let command = command.trim();
    if command.starts_with("bowline approve") || command.contains(" devices approve") {
        return (SafeActionEffect::Trust, SafeActionTarget::Device);
    }
    if command.starts_with("bowline revoke") || command.contains(" devices revoke") {
        return (SafeActionEffect::Destructive, SafeActionTarget::Device);
    }
    if command == "bowline recover"
        || command.starts_with("bowline recover status")
        || command.contains(" bowline recover status")
    {
        return (SafeActionEffect::Inspect, SafeActionTarget::Recovery);
    }
    if command.starts_with("bowline recover create")
        || command.starts_with("bowline recover verify")
        || command.starts_with("bowline recover rotate")
        || command.starts_with("bowline recover revoke")
        || command.starts_with("bowline recover use")
        || command.contains(" bowline recover create")
        || command.contains(" bowline recover verify")
        || command.contains(" bowline recover rotate")
        || command.contains(" bowline recover revoke")
        || command.contains(" bowline recover use")
        || command.contains(" recovery create")
        || command.contains(" recovery verify")
        || command.contains(" recovery rotate")
        || command.contains(" recovery revoke")
        || command.contains(" recovery use")
    {
        return (SafeActionEffect::Trust, SafeActionTarget::Recovery);
    }
    if command.starts_with("bowline setup") || command.contains(" prewarm") {
        return (SafeActionEffect::Setup, SafeActionTarget::Setup);
    }
    if command.starts_with("bowline accept")
        || command.starts_with("bowline discard")
        || command.starts_with("bowline restore")
        || command.contains(" --accept ")
        || command.contains(" --reject ")
    {
        return (SafeActionEffect::Mutate, SafeActionTarget::WorkView);
    }
    if command.starts_with("bowline cleanup --apply") {
        return (SafeActionEffect::Destructive, SafeActionTarget::WorkView);
    }
    if command.starts_with("bowline agent start")
        || command.contains(" agent start ")
        || command.starts_with("bowline agent publish")
        || command.starts_with("bowline agent complete")
        || command.starts_with("bowline agent budget")
    {
        return (SafeActionEffect::Mutate, SafeActionTarget::Agent);
    }
    if command.starts_with("bowline resolve") {
        return (SafeActionEffect::Inspect, SafeActionTarget::Conflict);
    }
    if command.starts_with("bowline review") || command.starts_with("bowline diff") {
        return (SafeActionEffect::Inspect, SafeActionTarget::WorkView);
    }
    if command.starts_with("bowline agent") {
        return (SafeActionEffect::Inspect, SafeActionTarget::Agent);
    }
    if command.starts_with("bowline connect") {
        return (SafeActionEffect::Trust, SafeActionTarget::Device);
    }
    (SafeActionEffect::Inspect, SafeActionTarget::Workspace)
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
    pub generated_path_count: u64,
    pub dependency_path_count: u64,
    pub env_file_count: u64,
    pub untracked_file_count: u64,
    pub local_only_path_count: u64,
    pub blocked_path_count: u64,
    pub workspace_sync_path_count: u64,
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
    use super::{SafeAction, SafeActionEffect, SafeActionTarget, StatusLevel, WorkspaceStatus};

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
    fn piped_recovery_action_is_trust_work() {
        let action = SafeAction {
            label: "Verify Recovery Key".to_string(),
            command: Some("printf '%s\\n' '<words>' | bowline recover verify rk_1".to_string()),
        };

        assert_eq!(action.effect_category(), SafeActionEffect::Trust);
        assert_eq!(action.target_kind(), SafeActionTarget::Recovery);
    }

    #[test]
    fn recovery_status_action_is_inspection() {
        let action = SafeAction {
            label: "Inspect recovery status".to_string(),
            command: Some("bowline recover status".to_string()),
        };

        assert_eq!(action.effect_category(), SafeActionEffect::Inspect);
        assert_eq!(action.target_kind(), SafeActionTarget::Recovery);
    }

    #[test]
    fn agent_start_action_is_mutating_agent_work() {
        let action = SafeAction {
            label: "Start agent work".to_string(),
            command: Some("bowline agent start . --task 'fix auth'".to_string()),
        };

        assert_eq!(action.effect_category(), SafeActionEffect::Mutate);
        assert_eq!(action.target_kind(), SafeActionTarget::Agent);
    }
}
