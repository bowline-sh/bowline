use serde::{Deserialize, Serialize};

use crate::{
    commands::CommandName,
    ids::{DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId},
    status::RepairCommand,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryScopeKind {
    Project,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryScope {
    pub kind: HistoryScopeKind,
    pub root: String,
    pub project_path: String,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryCause {
    Sync,
    Accept,
    ConflictResolution,
    Restore,
    Lifecycle,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryActorKind {
    Human,
    Agent,
    Daemon,
    ControlPlane,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryActor {
    pub kind: HistoryActorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryChangeSummary {
    pub files_changed: u32,
    pub files_added: u32,
    pub files_modified: u32,
    pub files_deleted: u32,
    pub files_renamed: u32,
    pub binary_or_large_files_changed: u32,
    pub env_keys_changed: u32,
    pub paths_sample: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestorePoint {
    pub id: String,
    pub snapshot_id: SnapshotId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot_id: Option<SnapshotId>,
    pub occurred_at: String,
    pub label: String,
    pub cause: HistoryCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<HistoryActor>,
    pub summary: HistoryChangeSummary,
    pub event_ids: Vec<EventId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathHistoryOperation {
    Create,
    Modify,
    Delete,
    Rename,
    Policy,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathHistoryEntry {
    pub restore_point_id: String,
    pub snapshot_id: SnapshotId,
    pub occurred_at: String,
    pub operation: PathHistoryOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<HistoryActor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    pub event_ids: Vec<EventId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_point_id: Option<String>,
    pub snapshot_id: SnapshotId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub scope: HistoryScope,
    pub restore_points: Vec<RestorePoint>,
    pub path_entries: Vec<PathHistoryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<HistoryEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<HistoryEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_summary: Option<HistoryChangeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub next_actions: Vec<RepairCommand>,
}
