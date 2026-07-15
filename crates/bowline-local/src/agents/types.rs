use super::*;

pub(super) const DEFAULT_DEVICE_ID: &str = "device-local-agent";
pub(super) const DEFAULT_POLICY_VERSION: &str = "policy-v1";

#[derive(Debug, Clone)]
pub struct AgentLeaseCreateOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub task: String,
    pub base: AgentLeaseBase,
    pub work_view: bool,
    pub force_stale: bool,
    pub device_id: DeviceId,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct DispatchedAgentLeaseCreateOptions {
    pub lease: AgentLeaseCreateOptions,
    pub identity: DispatchedAgentLeaseIdentity,
    pub workspace_content_key: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct AgentLeaseSelectorOptions {
    pub db_path: Option<PathBuf>,
    pub lease_id: LeaseId,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentLeaseExtendOptions {
    pub db_path: Option<PathBuf>,
    pub lease_id: LeaseId,
    pub hours: u16,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentMcpTokenIssueOptions {
    pub db_path: Option<PathBuf>,
    pub lease_id: LeaseId,
    pub grants: Vec<AgentMcpGrant>,
    pub generated_at: String,
}
