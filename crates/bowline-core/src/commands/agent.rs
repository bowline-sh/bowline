use super::*;

pub const AGENT_LEASE_STATUS_CREATING: &str = "creating";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentSessionState {
    Provisional,
    Open,
    Completed,
    Cancelled,
}

// Handoff-correlation marker mirrored onto the local lease record. `Pending` and
// `Claimed` mark a cross-device handoff lease before/after the target host
// materializes the workspace; Bowline no longer produces run-supervision states
// (the removed `Running`/`ReviewReady`/`Failed`) because it makes the workspace
// appear and scopes the credential rather than supervising the agent's run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLeaseDispatchState {
    #[default]
    None,
    Pending,
    Claimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentLeaseBase {
    #[serde(rename = "latest-workspace")]
    LatestWorkspace,
    #[serde(rename = "latest:main")]
    LatestMain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentWriteTargetMode {
    Direct,
    WorkView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLease {
    pub id: LeaseId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub device_id: DeviceId,
    #[serde(default)]
    pub dispatch_state: AgentLeaseDispatchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_ref: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_device_ref: Option<DeviceId>,
    pub write_target_mode: AgentWriteTargetMode,
    pub write_target_path: String,
    pub work_view_id: WorkViewId,
    pub work_view_path: String,
    pub task: String,
    pub base: AgentLeaseBase,
    pub base_snapshot_id: SnapshotId,
    pub session_state: AgentSessionState,
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
    #[serde(rename = "list_overlay_changes")]
    ListOverlayChanges,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentToolCategory {
    Inspection,
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
pub struct AgentCapability {
    pub name: AgentToolName,
    pub category: AgentToolCategory,
    pub state: AgentCapabilityState,
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
    pub next_action: Option<RepairCommand>,
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
    pub safe_next_actions: Vec<RepairCommand>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_token_file: Option<String>,
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
    pub safe_next_actions: Vec<RepairCommand>,
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
    pub sync_state: Option<DaemonSyncState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_because: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DaemonSyncState {
    Limited,
    Degraded,
    Unclassified,
}

impl DaemonSyncState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Limited => "limited",
            Self::Degraded => "degraded",
            Self::Unclassified => "unclassified",
        }
    }
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
    #[serde(default)]
    pub freshness: crate::status::FreshnessVerdict,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_bases: Vec<crate::status::StaleBaseStatus>,
    pub setup_receipts: Vec<String>,
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
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCompleteCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease: AgentLease,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLeaseUpdateCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease: AgentLease,
    pub status: WorkspaceStatus,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPrompt {
    pub recipe_id: String,
    pub recipe_version: u64,
    pub redaction: AgentPromptRedaction,
    pub text: String,
    pub allowed_tools: Vec<AgentToolName>,
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
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMcpGrant {
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpTokenCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease_id: LeaseId,
    pub token_file: String,
    pub grants: Vec<AgentMcpGrant>,
    pub expires_at: String,
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
pub enum BootstrapStepName {
    Install,
    AuthorizeBootstrap,
    ControlPlane,
    RemoteAuth,
    PrepareRoot,
    Request,
    Parse,
    Compare,
    Approve,
    Accept,
    Trust,
    MetadataDefault,
    DaemonStart,
    DaemonStatus,
    Sync,
    AgentLease,
}

impl std::fmt::Display for BootstrapStepName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Install => "install",
            Self::AuthorizeBootstrap => "authorize-bootstrap",
            Self::ControlPlane => "control-plane",
            Self::RemoteAuth => "remote-auth",
            Self::PrepareRoot => "prepare-root",
            Self::Request => "request",
            Self::Parse => "parse",
            Self::Compare => "compare",
            Self::Approve => "approve",
            Self::Accept => "accept",
            Self::Trust => "trust",
            Self::MetadataDefault => "metadata-default",
            Self::DaemonStart => "daemon-start",
            Self::DaemonStatus => "daemon-status",
            Self::Sync => "sync",
            Self::AgentLease => "agent-lease",
        })
    }
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
    pub name: BootstrapStepName,
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
    // Concrete inspect / retry / verify-trust remedies for a blocked bootstrap
    // step. Bootstrap no longer launches or supervises the remote agent, so
    // these carry only trust/repair guidance — never an agent-launch action.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repair_actions: Vec<RepairCommand>,
}
