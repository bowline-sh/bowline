use super::*;

pub(super) const DEFAULT_DEVICE_ID: &str = "device-local-agent";
pub(super) const DEFAULT_POLICY_VERSION: &str = "policy-v1";
pub(super) const MAX_READ_BYTES: u64 = 256 * 1024;
pub(super) const MAX_TREE_FILES: u64 = 200;
pub(super) const MAX_TREE_DEPTH: u64 = 4;

#[derive(Debug, Clone)]
pub struct AgentLeaseCreateOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub task: String,
    pub base: AgentLeaseBase,
    pub hydrate_budget_bytes: u64,
    pub work_view: bool,
    pub device_id: DeviceId,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentLeaseSelectorOptions {
    pub db_path: Option<PathBuf>,
    pub lease_id: LeaseId,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentBudgetGrantOptions {
    pub db_path: Option<PathBuf>,
    pub lease_id: LeaseId,
    pub add_bytes: u64,
    pub generated_at: String,
}

pub(super) struct AgentWriteEffect {
    pub(super) path: PathBuf,
    pub(super) previous_contents: Option<Vec<u8>>,
    pub(super) write_log_id: String,
}
