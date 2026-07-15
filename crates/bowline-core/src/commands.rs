use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    devices::{
        AccountLoginState, DeviceApprovalRequest, DeviceRecord, EncryptedDeviceGrant,
        RecoveryKeyState, RevokedDevice,
    },
    events::WorkspaceEvent,
    ids::{
        DeviceId, EventId, LeaseId, PolicyVersion, ProjectId, SnapshotId, WorkViewId, WorkspaceId,
    },
    status::{
        DeviceApprovalAffordance, EventWatermarks, FreshnessVerdict, RepairCommand,
        StaleBaseStatus, StatusScope, SyncQueueStatus, WorkspaceStatus, WorkspaceSummary,
    },
};

pub use crate::history::*;
pub use crate::wire::generated::CommandName;
pub use crate::work_views::{
    WorkCleanupCommandOutput, WorkCreateCommandOutput, WorkDiffCommandOutput,
    WorkLifecycleCommandOutput, WorkListCommandOutput,
};

pub const CONTRACT_VERSION: u16 = crate::wire::MACHINE_CONTRACT_VERSION;

mod agent;
pub use agent::BootstrapStepName;
pub use agent::*;
mod handoff;
pub use handoff::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandOption {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_name: Option<String>,
    pub summary: String,
    pub required: bool,
    pub repeatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandPositional {
    pub name: String,
    pub required: bool,
    pub repeatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandExample {
    pub command: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundedOutputControls {
    pub default_limit: u16,
    pub max_limit: u16,
    pub cursor_format: String,
    pub path_prefix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandDescriptor {
    pub group: String,
    pub name: String,
    pub summary: String,
    pub usage: String,
    pub positionals: Vec<CliCommandPositional>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<CliCommandOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<CliCommandExample>,
    pub json_output_type: String,
    pub side_effect_level: String,
    pub supports_json: bool,
    pub supports_dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounded_output: Option<BoundedOutputControls>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandSummary {
    pub name: String,
    pub group: String,
    pub summary: String,
    pub side_effect_level: String,
    pub supports_json: bool,
    pub supports_dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandGroup {
    pub name: String,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelpCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub groups: Vec<CliCommandGroup>,
    pub commands: Vec<CliCommandDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub cli_version: String,
    pub protocol: String,
    pub protocol_version: u32,
    pub default_socket: String,
    pub package: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCommandOutput {
    pub contract_version: u16,
    pub ok: bool,
    pub command: CommandName,
    pub generated_at: String,
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub update_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogoutCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub signed_out: bool,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractFixtureDescriptor {
    pub name: String,
    pub path: String,
    pub output_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExitCode {
    Success,
    UsageError,
    RetryableRuntimeError,
    UserActionRequired,
    BlockedOrDegradedBySafety,
}

impl CommandExitCode {
    pub const ALL: [Self; 5] = [
        Self::Success,
        Self::UsageError,
        Self::RetryableRuntimeError,
        Self::UserActionRequired,
        Self::BlockedOrDegradedBySafety,
    ];

    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::UsageError => 2,
            Self::RetryableRuntimeError => 3,
            Self::UserActionRequired => 4,
            Self::BlockedOrDegradedBySafety => 5,
        }
    }

    pub fn contract_table() -> BTreeMap<Self, u8> {
        Self::ALL
            .into_iter()
            .map(|exit_code| (exit_code, exit_code.code()))
            .collect()
    }

    pub const fn for_error(
        status: CommandErrorStatus,
        recoverability: CommandRecoverability,
    ) -> Self {
        match status {
            CommandErrorStatus::UsageError => Self::UsageError,
            CommandErrorStatus::Unsupported | CommandErrorStatus::Limited => {
                Self::BlockedOrDegradedBySafety
            }
            CommandErrorStatus::Failed => match recoverability {
                CommandRecoverability::Retry => Self::RetryableRuntimeError,
                CommandRecoverability::UserAction => Self::UserActionRequired,
                CommandRecoverability::Unsupported | CommandRecoverability::None => {
                    Self::BlockedOrDegradedBySafety
                }
            },
        }
    }
}

impl From<CommandExitCode> for std::process::ExitCode {
    fn from(exit_code: CommandExitCode) -> Self {
        Self::from(exit_code.code())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub cli_version: String,
    pub protocol: String,
    pub protocol_version: u32,
    pub event_schema_version: u16,
    pub package: String,
    pub package_contract_source: String,
    pub exit_codes: BTreeMap<CommandExitCode, u8>,
    pub command_output_types: Vec<String>,
    pub commands: Vec<CliCommandDescriptor>,
    pub fixtures: Vec<ContractFixtureDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractSummaryCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub cli_version: String,
    pub protocol: String,
    pub protocol_version: u32,
    pub event_schema_version: u16,
    pub package: String,
    pub package_contract_source: String,
    pub exit_codes: BTreeMap<CommandExitCode, u8>,
    pub commands: Vec<CliCommandSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedContractCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub cli_version: String,
    pub protocol: String,
    pub protocol_version: u32,
    pub event_schema_version: u16,
    pub package: String,
    pub package_contract_source: String,
    pub exit_codes: BTreeMap<CommandExitCode, u8>,
    pub descriptor: CliCommandDescriptor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DryRunStatus {
    DryRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DryRunCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub status: DryRunStatus,
    pub allowed: bool,
    pub risk: String,
    pub target: String,
    pub would_change: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub apply_command: String,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NamespaceLifecycleAction {
    ForgetLocal,
    Archive,
    Restore,
    PurgePending,
    PurgeCancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceLifecyclePreview {
    pub paths: Vec<String>,
    pub byte_total: u64,
    pub pack_count: u64,
    pub grace_days: Option<u32>,
    pub purge_after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceLifecycleCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub project_path: String,
    pub action: NamespaceLifecycleAction,
    pub preview: NamespaceLifecyclePreview,
    pub changed: bool,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceCommandAction {
    List,
    Request,
    Approve,
    Accept,
    Deny,
    Revoke,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryCommandAction {
    Status,
    Create,
    Verify,
    Rotate,
    Revoke,
    Use,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<StatusScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_summary: Option<WorkspaceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_readiness: Option<crate::status::ProjectSetupReadiness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_queue: Option<SyncQueueStatus>,
    #[serde(default)]
    pub freshness: FreshnessVerdict,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_bases: Vec<StaleBaseStatus>,
    pub status: WorkspaceStatus,
    pub status_summary: crate::status::StatusSummary,
    pub items: Vec<crate::status::StatusItem>,
    pub limits: Vec<crate::status::LimitedCapability>,
    pub event_watermarks: EventWatermarks,
    pub next_actions: Vec<RepairCommand>,
    // Sensitive local trust material: approval codes/commands are present only on
    // trusted local status surfaces and must never reach hosted/persisted payloads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_approvals: Vec<DeviceApprovalAffordance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub account: AccountLoginState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_device: Option<DeviceRecord>,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootChoiceState {
    ExplicitExisting,
    ExplicitCreated,
    DefaultSelected,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootInitOutput {
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub root: String,
    pub root_choice: RootChoiceState,
    pub observed_only: bool,
    pub changed_workspace_files: bool,
    pub created_root: bool,
    pub scan_summary: crate::status::ObservedWorkspaceSummary,
    pub non_actions: Vec<String>,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupProjectState {
    Hot,
    SetupBlocked,
    NoSetupNeeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupProjectOutcome {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub project_path: String,
    pub state: SetupProjectState,
    pub receipt_ids: Vec<String>,
    pub redacted_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupProjectOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub outcome: SetupProjectOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub root: String,
    pub root_choice: RootChoiceState,
    pub login: AccountLoginState,
    pub next_actions: Vec<RepairCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevicesCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub action: DeviceCommandAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_device: Option<DeviceRecord>,
    pub devices: Vec<DeviceRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revoked_devices: Vec<RevokedDevice>,
    pub pending_requests: Vec<DeviceApprovalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_request: Option<DeviceApprovalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_device: Option<DeviceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_request: Option<DeviceApprovalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_device: Option<RevokedDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_key: Option<RecoveryKeyState>,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub action: RecoveryCommandAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    pub recovery_key: RecoveryKeyState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_request: Option<DeviceApprovalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_grant: Option<EncryptedDeviceGrant>,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<StatusScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_path: Option<String>,
    pub events: Vec<WorkspaceEvent>,
    pub event_watermarks: EventWatermarks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandErrorOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub status: CommandErrorStatus,
    pub error: CommandError,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandErrorStatus {
    UsageError,
    Unsupported,
    Limited,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: String,
    pub message: String,
    pub recoverability: CommandRecoverability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandRecoverability {
    Retry,
    UserAction,
    Unsupported,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WatchFrame {
    Status {
        contract_version: u16,
        sequence: u64,
        generated_at: String,
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
        status: Box<StatusCommandOutput>,
        watermark: EventWatermarks,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_event_id: Option<EventId>,
    },
    Event {
        contract_version: u16,
        sequence: u64,
        generated_at: String,
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
        event: Box<WorkspaceEvent>,
        watermark: EventWatermarks,
    },
    Error {
        contract_version: u16,
        sequence: u64,
        generated_at: String,
        workspace_id: WorkspaceId,
        error: CommandErrorOutput,
    },
}

#[cfg(test)]
mod exit_code_tests {
    use super::*;

    #[test]
    fn exit_code_table_and_error_mapping_are_stable() {
        assert_eq!(CommandExitCode::Success.code(), 0);
        assert_eq!(CommandExitCode::UsageError.code(), 2);
        assert_eq!(CommandExitCode::RetryableRuntimeError.code(), 3);
        assert_eq!(CommandExitCode::UserActionRequired.code(), 4);
        assert_eq!(CommandExitCode::BlockedOrDegradedBySafety.code(), 5);
        assert_eq!(
            CommandExitCode::for_error(
                CommandErrorStatus::UsageError,
                CommandRecoverability::UserAction,
            ),
            CommandExitCode::UsageError
        );
        assert_eq!(
            CommandExitCode::for_error(CommandErrorStatus::Failed, CommandRecoverability::Retry,),
            CommandExitCode::RetryableRuntimeError
        );
        assert_eq!(
            CommandExitCode::for_error(
                CommandErrorStatus::Failed,
                CommandRecoverability::UserAction,
            ),
            CommandExitCode::UserActionRequired
        );
        assert_eq!(
            CommandExitCode::for_error(CommandErrorStatus::Limited, CommandRecoverability::None,),
            CommandExitCode::BlockedOrDegradedBySafety
        );
    }
}
