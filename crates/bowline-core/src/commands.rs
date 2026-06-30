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
    #[serde(rename = "recovery")]
    Recovery,
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
    #[serde(rename = "agent lease create")]
    AgentLeaseCreate,
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
    #[serde(rename = "bootstrap ssh")]
    BootstrapSsh,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLeaseExecutionState {
    Active,
    Blocked,
    Completed,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLeaseOutputState {
    Empty,
    Dirty,
    ReviewReady,
    Accepted,
    Discarded,
    Conflicted,
    Retained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLeaseCleanupState {
    Current,
    Retained,
    CleanupPending,
    CleanupCompleted,
    Scrubbed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentLeaseBase {
    #[serde(rename = "latest-workspace")]
    LatestWorkspace,
    #[serde(rename = "latest:main")]
    LatestMain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLeaseScope {
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classifications: Vec<PathClassification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes_per_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files_per_request: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLeaseScopes {
    pub read: AgentLeaseScope,
    pub write: AgentLeaseScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentEnvRestrictionKind {
    Allowlist,
    BlockedSecret,
    GrantRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnvRestriction {
    pub kind: AgentEnvRestrictionKind,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentEnvMaterialization {
    LeaseWorkView,
    ProjectPath,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnvProfile {
    pub name: String,
    pub materialization: AgentEnvMaterialization,
    pub available_keys: Vec<String>,
    pub restrictions: Vec<AgentEnvRestriction>,
    pub grant_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentOutputTargetKind {
    RealProject,
    WorkView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOutputTarget {
    pub kind: AgentOutputTargetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_view_id: Option<WorkViewId>,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentWriteTargetMode {
    Direct,
    WorkView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAuditPointer {
    pub local_event_id: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_receipt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_object_pointer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLease {
    pub id: LeaseId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub device_id: DeviceId,
    pub write_target_mode: AgentWriteTargetMode,
    pub write_target_path: String,
    pub work_view_id: WorkViewId,
    pub work_view_path: String,
    pub task: String,
    pub base: AgentLeaseBase,
    pub base_snapshot_id: SnapshotId,
    pub execution_state: AgentLeaseExecutionState,
    pub output_state: AgentLeaseOutputState,
    pub scopes: AgentLeaseScopes,
    pub hydrate_budget_bytes: u64,
    pub env_profile: AgentEnvProfile,
    pub env_restrictions: Vec<AgentEnvRestriction>,
    pub output_target: AgentOutputTarget,
    pub audit: AgentAuditPointer,
    pub cleanup_state: AgentLeaseCleanupState,
    pub status_summary: String,
    pub expires_at: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentToolName {
    #[serde(rename = "workspace_status")]
    WorkspaceStatus,
    #[serde(rename = "list_capabilities")]
    ListCapabilities,
    #[serde(rename = "resolve_path")]
    ResolvePath,
    #[serde(rename = "explain_path_policy")]
    ExplainPathPolicy,
    #[serde(rename = "list_attention_items")]
    ListAttentionItems,
    #[serde(rename = "list_tree_at_snapshot")]
    ListTreeAtSnapshot,
    #[serde(rename = "read_file_at_snapshot")]
    ReadFileAtSnapshot,
    #[serde(rename = "search_workspace")]
    SearchWorkspace,
    #[serde(rename = "symbol_lookup")]
    SymbolLookup,
    #[serde(rename = "request_hydration")]
    RequestHydration,
    #[serde(rename = "get_hydration_status")]
    GetHydrationStatus,
    #[serde(rename = "write_overlay_file")]
    WriteOverlayFile,
    #[serde(rename = "list_overlay_changes")]
    ListOverlayChanges,
    #[serde(rename = "diff_snapshots")]
    DiffSnapshots,
    #[serde(rename = "run_command_with_receipt")]
    RunCommandWithReceipt,
    #[serde(rename = "inspect_setup_receipts")]
    InspectSetupReceipts,
    #[serde(rename = "propose_policy_change")]
    ProposePolicyChange,
    #[serde(rename = "request_human_decision")]
    RequestHumanDecision,
    #[serde(rename = "publish_overlay_for_review")]
    PublishOverlayForReview,
    #[serde(rename = "complete_task")]
    CompleteTask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentToolCategory {
    Inspection,
    Exploration,
    Hydration,
    Write,
    Execution,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentCapabilityState {
    Available,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DegradedExplorationBounds {
    pub max_bytes: u64,
    pub max_files: u64,
    pub max_depth: u64,
    pub truncation_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
    pub safe_next_action: SafeAction,
    pub index_backed_search_unavailable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapability {
    pub name: AgentToolName,
    pub category: AgentToolCategory,
    pub state: AgentCapabilityState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<DegradedExplorationBounds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentCliName {
    Codex,
    Claude,
    Cursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCliCapability {
    pub name: AgentCliName,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub supports_prompt_file_launch: bool,
    pub supports_stdin_launch: bool,
    pub supports_cwd_selection: bool,
    pub supports_noninteractive_execution: bool,
    pub supports_receipt_capture: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentReadinessState {
    Ready,
    Attention,
    Limited,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentReadinessSignal {
    pub name: String,
    pub state: AgentReadinessState,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProjectReadiness {
    pub state: AgentReadinessState,
    pub signals: Vec<AgentReadinessSignal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStartWork {
    pub cwd: String,
    pub context_command: String,
    pub prompt_command: String,
    pub safe_next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentToolTransport {
    LocalDaemon,
    McpAdapter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolAuthority {
    pub transport: AgentToolTransport,
    pub peer_credential_checked: bool,
    pub nonce_presented: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolInvokeRequest {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol_version: u16,
    pub request_id: String,
    pub lease_id: LeaseId,
    pub tool: AgentToolName,
    pub authority: AgentToolAuthority,
    pub arguments: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentToolResultOutcome {
    Allowed,
    Denied,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolDenial {
    pub code: String,
    pub safe_next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    pub request_id: String,
    pub lease_id: LeaseId,
    pub tool: AgentToolName,
    pub outcome: AgentToolResultOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<AgentToolDenial>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<DegradedExplorationBounds>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonProcessOutput {
    pub state: String,
    pub socket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonServiceState {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub unit_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_because: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub daemon: DaemonProcessOutput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatusOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub daemon: DaemonProcessOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<DaemonServiceState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonServiceOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub service: DaemonServiceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsCollectCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub redaction_rules: Vec<String>,
    pub bundle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentContextV1 {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease: AgentLease,
    pub policy_version: PolicyVersion,
    pub status: WorkspaceStatus,
    pub write_target_path: String,
    pub work_view_path: String,
    pub attention: Vec<crate::status::StatusItem>,
    pub capabilities: Vec<AgentCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<IndexStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_budget: Option<HydrationBudgetStatus>,
    pub setup_receipts: Vec<String>,
    pub env: AgentEnvProfile,
    pub scopes: AgentLeaseScopes,
    pub readiness: AgentProjectReadiness,
    pub start_work: AgentStartWork,
    pub adapter_capabilities: Vec<AgentCliCapability>,
    pub instructions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentContextCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub context: AgentContextV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLeaseCreateCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease: AgentLease,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPrompt {
    pub recipe_id: String,
    pub recipe_version: u64,
    pub redaction: AgentPromptRedaction,
    pub text: String,
    pub allowed_tools: Vec<AgentToolName>,
    pub output_target: AgentOutputTarget,
    pub adapter_capabilities: Vec<AgentCliCapability>,
    pub instructions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentPromptRedaction {
    Applied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPromptCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease: AgentLease,
    pub prompt: AgentPrompt,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBudgetCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease: AgentLease,
    pub previous_limit_bytes: u64,
    pub added_bytes: u64,
    pub budget: HydrationBudgetStatus,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}

pub type AcceptCommandOutput = WorkLifecycleCommandOutput;
pub type DiscardCommandOutput = WorkLifecycleCommandOutput;
pub type RestoreCommandOutput = WorkLifecycleCommandOutput;
pub type CleanupCommandOutput = WorkCleanupCommandOutput;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootstrapStepState {
    Pending,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootstrapSyncState {
    Ready,
    Prepared,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootstrapSecretStore {
    OsKeychain,
    ServerLocal,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapStep {
    pub name: String,
    pub state: BootstrapStepState,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapSshCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub host: String,
    pub root: String,
    pub steps: Vec<BootstrapStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_request: Option<DeviceApprovalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_device: Option<DeviceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_device_fingerprint: Option<crate::devices::DeviceFingerprint>,
    pub trusted: bool,
    pub secret_store: BootstrapSecretStore,
    pub sync: BootstrapSyncState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_required_phase: Option<u16>,
    pub remote_status: WorkspaceStatus,
    pub next_actions: Vec<SafeAction>,
}
