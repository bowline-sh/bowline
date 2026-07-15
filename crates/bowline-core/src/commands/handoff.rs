use serde::{Deserialize, Serialize};

use super::CommandName;
use crate::status::RepairCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HandoffAgent {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffOutcome {
    DryRun,
    ConfirmationRequired,
    Receipt,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffSessionMode {
    ResumeExisting,
    FreshPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffCandidate {
    pub agent: HandoffAgent,
    pub session_id: String,
    pub source_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    pub modified_at_unix_seconds: u64,
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffPlan {
    pub target: String,
    pub agent: HandoffAgent,
    pub session_mode: HandoffSessionMode,
    pub project_path: String,
    pub remote_project_path: String,
    pub tmux_session: String,
    pub launch_command: String,
    pub transfer: HandoffTransferPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffTransferPlan {
    pub encrypted: bool,
    pub durable_cloud_storage: bool,
    pub installs_byte_exact_session_files: bool,
    pub remote_installer_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffReceipt {
    pub agent: HandoffAgent,
    pub target: String,
    pub remote_project_path: String,
    pub tmux_session: String,
    pub attach_command: String,
    pub monitoring: bool,
    pub workspace_lock: bool,
    pub same_session_concurrency_risk: bool,
    pub session_mode: HandoffSessionMode,
    pub agent_runtime_verified: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffError {
    pub code: String,
    pub message: String,
    pub recoverability: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub outcome: HandoffOutcome,
    pub target: String,
    pub project_path: String,
    pub candidates: Vec<HandoffCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<HandoffCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<HandoffPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<HandoffReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<HandoffError>,
    pub next_actions: Vec<RepairCommand>,
}
