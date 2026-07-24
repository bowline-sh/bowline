use super::*;

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
