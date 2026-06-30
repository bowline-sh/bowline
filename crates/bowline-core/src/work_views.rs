use serde::{Deserialize, Serialize};

use crate::{
    ids::{DeviceId, ProjectId, SnapshotId, WorkViewId, WorkspaceId},
    status::{SafeAction, WorkspaceStatus},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewLifecycle {
    Active,
    ReviewReady,
    Accepted,
    Discarded,
    Expired,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewVisibility {
    DefaultVisible,
    Hidden,
    Pinned,
    Followed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewSyncState {
    LocalOnly,
    Synced,
    Uploading,
    Attention,
    Conflicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkViewRetentionState {
    Current,
    Retained,
    Expired,
    DeleteEligible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkViewRetention {
    pub state: WorkViewRetentionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_until: Option<String>,
    pub restorable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkView {
    pub id: WorkViewId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub project_path: String,
    pub name: String,
    pub visible_path: String,
    pub base_snapshot_id: SnapshotId,
    pub overlay_head: String,
    pub overlay_version: u64,
    pub env_profile: String,
    pub lifecycle: WorkViewLifecycle,
    pub visibility: WorkViewVisibility,
    pub sync_state: WorkViewSyncState,
    pub retention: WorkViewRetention,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_device_id: Option<DeviceId>,
    #[serde(default)]
    pub followed_by: Vec<String>,
    #[serde(default)]
    pub host_materializations: Vec<String>,
    #[serde(default)]
    pub attention: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkDiffChangeKind {
    Added,
    Modified,
    Deleted,
    PolicyReview,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkDiffEntry {
    pub path: String,
    pub kind: WorkDiffChangeKind,
    pub summary: String,
    pub contains_secrets: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkCommandAction {
    Created,
    Listed,
    Diffed,
    ReviewReady,
    Accepted,
    Discarded,
    Restored,
    CleanupPreviewed,
    CleanupApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkonCommandOutput {
    pub contract_version: u16,
    pub command: crate::commands::CommandName,
    pub generated_at: String,
    pub action: WorkCommandAction,
    pub work_view: WorkView,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkListCommandOutput {
    pub contract_version: u16,
    pub command: crate::commands::CommandName,
    pub generated_at: String,
    pub action: WorkCommandAction,
    pub workspace_id: WorkspaceId,
    pub work_views: Vec<WorkView>,
    pub include_hidden: bool,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkDiffCommandOutput {
    pub contract_version: u16,
    pub command: crate::commands::CommandName,
    pub generated_at: String,
    pub action: WorkCommandAction,
    pub work_view: WorkView,
    pub changes: Vec<WorkDiffEntry>,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkLifecycleCommandOutput {
    pub contract_version: u16,
    pub command: crate::commands::CommandName,
    pub generated_at: String,
    pub action: WorkCommandAction,
    pub work_view: WorkView,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkCleanupCommandOutput {
    pub contract_version: u16,
    pub command: crate::commands::CommandName,
    pub generated_at: String,
    pub action: WorkCommandAction,
    pub workspace_id: WorkspaceId,
    pub previewed_paths: Vec<String>,
    pub deleted_paths: Vec<String>,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}
