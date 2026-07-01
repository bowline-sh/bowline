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
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::{
        EventWatermarks, HydrationProgress, SafeAction, StatusScope, SyncQueueStatus,
        WorkspaceStatus, WorkspaceSummary,
    },
};

pub use crate::work_views::{
    WorkCleanupCommandOutput, WorkDiffCommandOutput, WorkLifecycleCommandOutput,
    WorkListCommandOutput, WorkonCommandOutput,
};

pub const CONTRACT_VERSION: u16 = 3;

mod agent;
pub use agent::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandName {
    #[serde(rename = "help")]
    Help,
    #[serde(rename = "version")]
    Version,
    #[serde(rename = "contract")]
    Contract,
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "login")]
    Login,
    #[serde(rename = "approve")]
    Approve,
    #[serde(rename = "revoke")]
    Revoke,
    #[serde(rename = "recover")]
    Recover,
    #[serde(rename = "init")]
    Init,
    #[serde(rename = "setup")]
    Setup,
    #[serde(rename = "prewarm")]
    Prewarm,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "search")]
    Search,
    #[serde(rename = "symbols")]
    Symbols,
    #[serde(rename = "explain")]
    Explain,
    #[serde(rename = "devices")]
    Devices,
    #[serde(rename = "events")]
    Events,
    #[serde(rename = "actions")]
    Actions,
    #[serde(rename = "tui")]
    Tui,
    #[serde(rename = "resolve")]
    Resolve,
    #[serde(rename = "workon")]
    Workon,
    #[serde(rename = "review")]
    Review,
    #[serde(rename = "work")]
    Work,
    #[serde(rename = "diff")]
    Diff,
    #[serde(rename = "accept")]
    Accept,
    #[serde(rename = "discard")]
    Discard,
    #[serde(rename = "restore")]
    Restore,
    #[serde(rename = "cleanup")]
    Cleanup,
    #[serde(rename = "agent context")]
    AgentContext,
    #[serde(rename = "agent start")]
    AgentStart,
    #[serde(rename = "agent prompt")]
    AgentPrompt,
    #[serde(rename = "agent publish")]
    AgentPublish,
    #[serde(rename = "agent complete")]
    AgentComplete,
    #[serde(rename = "agent budget")]
    AgentBudget,
    #[serde(rename = "daemon start")]
    DaemonStart,
    #[serde(rename = "daemon stop")]
    DaemonStop,
    #[serde(rename = "daemon status")]
    DaemonStatus,
    #[serde(rename = "daemon install")]
    DaemonInstall,
    #[serde(rename = "daemon restart")]
    DaemonRestart,
    #[serde(rename = "daemon uninstall")]
    DaemonUninstall,
    #[serde(rename = "diagnostics collect")]
    DiagnosticsCollect,
    #[serde(rename = "connect")]
    Connect,
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub summary: String,
    pub usage: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<CliCommandOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<CliCommandExample>,
    pub json_output_type: String,
    pub side_effect_level: String,
    pub supports_json: bool,
    pub supports_dry_run: bool,
    pub supports_idempotency_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounded_output: Option<BoundedOutputControls>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_commands: Vec<String>,
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
pub struct ContractFixtureDescriptor {
    pub name: String,
    pub path: String,
    pub output_type: String,
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
    pub command_output_types: Vec<String>,
    pub commands: Vec<CliCommandDescriptor>,
    pub fixtures: Vec<ContractFixtureDescriptor>,
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
    pub next_actions: Vec<SafeAction>,
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
    pub index: Option<IndexStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_budget: Option<HydrationBudgetStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hydration_progress: Vec<HydrationProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_queue: Option<SyncQueueStatus>,
    pub status: WorkspaceStatus,
    pub items: Vec<crate::status::StatusItem>,
    pub limits: Vec<crate::status::LimitedCapability>,
    pub event_watermarks: EventWatermarks,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexState {
    Ready,
    Stale,
    Rebuilding,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexSource {
    Local,
    EncryptedIndexPack,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexDegradedReason {
    Missing,
    Corrupt,
    Unsupported,
    PolicyLimited,
    RebuildFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    pub state: IndexState,
    pub source: IndexSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<SnapshotId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_pack_object_key: Option<String>,
    pub path_count: u64,
    pub file_count: u64,
    pub indexed_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_path_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<IndexDegradedReason>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<SafeAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HydrationBudgetState {
    Available,
    Exhausted,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HydrationBudgetScope {
    Lease,
    Project,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HydrationBudgetStatus {
    pub state: HydrationBudgetState,
    pub limit_bytes: u64,
    pub used_bytes: u64,
    pub reserved_bytes: u64,
    pub remaining_bytes: u64,
    pub scope: HydrationBudgetScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<LeaseId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub path: String,
    pub score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<SnapshotId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessFlag>,
    pub hydration_state: crate::workspace_graph::HydrationState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_path: Option<String>,
    pub index: IndexStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<HydrationBudgetStatus>,
    pub results: Vec<SearchResult>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Variable,
    Constant,
    Type,
    Interface,
    Module,
    Import,
    Export,
    Struct,
    Enum,
    Trait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SymbolLanguage {
    #[serde(rename = "typescript")]
    TypeScript,
    #[serde(rename = "javascript")]
    JavaScript,
    Python,
    Rust,
    Go,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolResult {
    pub name: String,
    pub kind: SymbolKind,
    pub language: SymbolLanguage,
    pub path: String,
    pub line_start: u64,
    pub line_end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<SnapshotId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_count: Option<u64>,
    pub classification: PathClassification,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessFlag>,
    pub hydration_state: crate::workspace_graph::HydrationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_path: Option<String>,
    pub index: IndexStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<HydrationBudgetStatus>,
    pub symbols: Vec<SymbolResult>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
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
    pub next_actions: Vec<SafeAction>,
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
pub struct InitCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub root: String,
    pub root_choice: RootChoiceState,
    pub observed_only: bool,
    pub changed_workspace_files: bool,
    pub created_root: bool,
    pub scan_summary: crate::status::ObservedWorkspaceSummary,
    pub non_actions: Vec<String>,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrewarmCommandState {
    Hot,
    SetupBlocked,
    NoSetupNeeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrewarmCommandOutcome {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub project_path: String,
    pub state: PrewarmCommandState,
    pub receipt_ids: Vec<String>,
    pub redacted_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrewarmCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub outcome: PrewarmCommandOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub matched_rule: String,
    pub rule_source: String,
    pub risk: String,
    pub observed_state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisory_notes: Vec<String>,
    pub summary: String,
    pub next_actions: Vec<SafeAction>,
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
    pub next_actions: Vec<SafeAction>,
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
    pub next_actions: Vec<SafeAction>,
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
pub struct ActionsCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<StatusScope>,
    pub status: WorkspaceStatus,
    pub actions: Vec<SafeAction>,
    #[serde(default)]
    pub non_actions: Vec<String>,
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
    pub next_actions: Vec<SafeAction>,
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
