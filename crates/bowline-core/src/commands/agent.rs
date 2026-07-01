use super::*;

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
