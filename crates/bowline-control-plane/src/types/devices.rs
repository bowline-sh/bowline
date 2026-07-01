use crate::ControlPlaneTimestamp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequestInput {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_public_key: String,
    pub device_fingerprint: String,
    pub device_authorization_proof_verifier: String,
    pub matching_code: String,
    pub account_id: Option<String>,
    pub host: Option<String>,
    pub root: Option<String>,
    pub expires_in_ticks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequestInputDraft {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub device_public_key: String,
    pub device_fingerprint: String,
    pub matching_code: String,
}

impl DeviceRequestInput {
    pub fn new(draft: DeviceRequestInputDraft) -> Self {
        Self {
            workspace_id: draft.workspace_id,
            device_id: draft.device_id,
            device_name: draft.device_name,
            platform: std::env::consts::OS.to_string(),
            device_public_key: draft.device_public_key,
            device_fingerprint: draft.device_fingerprint,
            device_authorization_proof_verifier: String::new(),
            matching_code: draft.matching_code,
            account_id: None,
            host: None,
            root: None,
            expires_in_ticks: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSessionInput {
    pub workspace_id: String,
    pub host: Option<String>,
    pub root: Option<String>,
    pub expires_in_ticks: u64,
}

impl BootstrapSessionInput {
    pub fn new(workspace_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            host: None,
            root: None,
            expires_in_ticks: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSession {
    pub session_id: String,
    pub workspace_id: String,
    pub token: String,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequest {
    pub request_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_public_key: String,
    pub device_fingerprint: String,
    pub matching_code: String,
    pub account_id: Option<String>,
    pub host: Option<String>,
    pub root: Option<String>,
    pub requested_at: ControlPlaneTimestamp,
    pub expires_at: ControlPlaneTimestamp,
    pub state: DeviceRequestState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRequestState {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedDeviceRecord {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub authorized_at: ControlPlaneTimestamp,
    pub authorized_by_device_id: Option<String>,
    pub revoked_at: Option<ControlPlaneTimestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstAuthorizedDeviceInput {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub device_authorization_proof_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApprovalRequestList {
    pub pending_requests: Vec<DeviceRequest>,
    pub authorized_devices: Vec<AuthorizedDeviceRecord>,
    pub revoked_devices: Vec<RevokedDeviceRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApprovalInput {
    pub request_id: String,
    pub approved_by_device_id: String,
    pub approved_by_device_proof: String,
    pub encrypted_grant_ciphertext: String,
    pub grant_acceptance_proof_verifier: String,
    pub key_epoch: u32,
    pub expires_in_ticks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApproval {
    pub grant_id: String,
    pub request_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub approved_by_device_id: String,
    pub encrypted_grant_ciphertext: String,
    pub key_epoch: u32,
    pub granted_at: ControlPlaneTimestamp,
    pub expires_at: ControlPlaneTimestamp,
    pub accepted_at: Option<ControlPlaneTimestamp>,
    pub harness_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDenialInput {
    pub request_id: String,
    pub denied_by_device_id: String,
    pub denied_by_device_proof: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDenial {
    pub request_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub denied_by_device_id: String,
    pub denied_at: ControlPlaneTimestamp,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRevocationInput {
    pub workspace_id: String,
    pub device_id: String,
    pub revoked_by_device_id: String,
    pub revoked_by_device_proof: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedDeviceRecord {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub revoked_at: ControlPlaneTimestamp,
    pub revoked_by_device_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAcceptanceInput {
    pub request_id: String,
    pub device_id: String,
    pub grant_acceptance_proof: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEnvelopeInput {
    pub workspace_id: String,
    pub envelope_id: String,
    pub created_by_device_id: String,
    pub created_by_device_proof: String,
    pub ciphertext: String,
    pub fingerprint: String,
    pub recovery_proof_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDeviceAuthorizationInput {
    pub workspace_id: String,
    pub envelope_id: String,
    pub request_id: String,
    pub encrypted_grant_ciphertext: String,
    pub grant_acceptance_proof_verifier: String,
    pub key_epoch: u32,
    pub recovery_proof: String,
    pub expires_in_ticks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryEnvelopeState {
    GeneratedUnverified,
    Active,
    Rotated,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEnvelopeRecord {
    pub workspace_id: String,
    pub envelope_id: String,
    pub created_by_device_id: String,
    pub ciphertext: String,
    pub fingerprint: String,
    pub state: RecoveryEnvelopeState,
    pub created_at: ControlPlaneTimestamp,
    pub verified_at: Option<ControlPlaneTimestamp>,
    pub rotated_at: Option<ControlPlaneTimestamp>,
    pub revoked_at: Option<ControlPlaneTimestamp>,
}
