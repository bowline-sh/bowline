use serde::{Deserialize, Serialize};

use crate::ids::{
    AccountId, DeviceApprovalRequestId, DeviceId, EncryptedDeviceGrantId, RecoveryEnvelopeId,
    WorkOsOrganizationId, WorkOsUserId, WorkspaceId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DevicePlatform {
    Macos,
    Linux,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceFingerprint(String);

impl DeviceFingerprint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicDeviceKey(String);

impl PublicDeviceKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceApprovalRequestState {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceApprovalRequest {
    pub request_id: DeviceApprovalRequestId,
    pub workspace_id: WorkspaceId,
    pub requester_device_id: DeviceId,
    pub device_name: String,
    pub platform: DevicePlatform,
    pub device_public_key: PublicDeviceKey,
    pub device_fingerprint: DeviceFingerprint,
    pub matching_code: String,
    pub requested_at: String,
    pub expires_at: String,
    pub state: DeviceApprovalRequestState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_receipts_digest: Option<String>,
}

/// Short, human-comparable rendering of a device matching code.
///
/// The full `matching_code` (`bowline-<64 hex>`) is the binding value and must
/// stay full wherever it is verified. This display-only token lets both devices
/// render the same short code for eyeball comparison.
pub fn display_matching_code(full: &str) -> String {
    let digest = full.strip_prefix("bowline-").unwrap_or(full);
    let head: String = digest.chars().take(8).collect();
    if head.len() == 8 {
        format!("{}-{}", &head[..4], &head[4..])
    } else {
        head
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizedDevice {
    pub id: DeviceId,
    pub name: String,
    pub workspace_id: WorkspaceId,
    pub platform: DevicePlatform,
    pub device_fingerprint: DeviceFingerprint,
    pub authorized_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_by_device_id: Option<DeviceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceTrustState {
    Trusted,
    Pending,
    Revoked,
    Limited,
    Unavailable,
    FirstDeviceSetup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub id: DeviceId,
    pub name: String,
    pub workspace_id: WorkspaceId,
    pub platform: DevicePlatform,
    pub trust_state: DeviceTrustState,
    pub device_fingerprint: DeviceFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_at: Option<String>,
    pub updated_at: String,
    pub is_current_device: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limitation_reason: Option<String>,
}

impl From<AuthorizedDevice> for DeviceRecord {
    fn from(device: AuthorizedDevice) -> Self {
        Self {
            id: device.id,
            name: device.name,
            workspace_id: device.workspace_id,
            platform: device.platform,
            trust_state: DeviceTrustState::Trusted,
            device_fingerprint: device.device_fingerprint,
            authorized_at: Some(device.authorized_at.clone()),
            updated_at: device.authorized_at,
            is_current_device: false,
            limitation_reason: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokedDevice {
    pub id: DeviceId,
    pub name: String,
    pub workspace_id: WorkspaceId,
    pub platform: DevicePlatform,
    pub device_fingerprint: DeviceFingerprint,
    pub revoked_at: String,
    pub revoked_by_device_id: DeviceId,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncryptedDeviceGrantState {
    Created,
    Accepted,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptedDeviceGrant {
    pub grant_id: EncryptedDeviceGrantId,
    pub request_id: DeviceApprovalRequestId,
    pub workspace_id: WorkspaceId,
    pub requester_device_id: DeviceId,
    pub requester_device_fingerprint: DeviceFingerprint,
    pub approver_device_id: DeviceId,
    pub key_epoch: u32,
    pub ciphertext: String,
    pub created_at: String,
    pub expires_at: String,
    pub state: EncryptedDeviceGrantState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryKeyLifecycle {
    Missing,
    GeneratedUnverified,
    Active,
    Rotated,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryKeyState {
    pub lifecycle: RecoveryKeyLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_id: Option<RecoveryEnvelopeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

impl RecoveryKeyState {
    pub fn missing() -> Self {
        Self {
            lifecycle: RecoveryKeyLifecycle::Missing,
            envelope_id: None,
            fingerprint: None,
            created_at: None,
            verified_at: None,
            rotated_at: None,
            revoked_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountLoginStatus {
    NotLoggedIn,
    LoginPending,
    AccountAuthenticated,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLoginState {
    pub status: AccountLoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<AccountId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_os_user_id: Option<WorkOsUserId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_os_organization_id: Option<WorkOsOrganizationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_seconds: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceApprovalError {
    EmptyDeviceName,
    EmptyWorkspaceId,
    EmptyRequesterDeviceId,
    EmptyRequestId,
    EmptyPublicKey,
    EmptyFingerprint,
}

pub fn approve_device(
    request: &DeviceApprovalRequest,
    authorized_by_device_id: DeviceId,
    authorized_at: impl Into<String>,
) -> Result<AuthorizedDevice, DeviceApprovalError> {
    if request.device_name.is_empty() {
        return Err(DeviceApprovalError::EmptyDeviceName);
    }
    if request.workspace_id.as_str().is_empty() {
        return Err(DeviceApprovalError::EmptyWorkspaceId);
    }
    if request.requester_device_id.as_str().is_empty() {
        return Err(DeviceApprovalError::EmptyRequesterDeviceId);
    }
    if request.request_id.as_str().is_empty() {
        return Err(DeviceApprovalError::EmptyRequestId);
    }
    if request.device_public_key.as_str().is_empty() {
        return Err(DeviceApprovalError::EmptyPublicKey);
    }
    if request.device_fingerprint.as_str().is_empty() {
        return Err(DeviceApprovalError::EmptyFingerprint);
    }

    Ok(AuthorizedDevice {
        authorized_at: authorized_at.into(),
        authorized_by_device_id: Some(authorized_by_device_id),
        id: request.requester_device_id.clone(),
        name: request.device_name.clone(),
        workspace_id: request.workspace_id.clone(),
        platform: request.platform,
        device_fingerprint: request.device_fingerprint.clone(),
    })
}

#[cfg(test)]
mod tests {
    use crate::ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId};

    use super::{
        AuthorizedDevice, DeviceApprovalRequest, DeviceApprovalRequestState, DeviceFingerprint,
        DevicePlatform, PublicDeviceKey, approve_device, display_matching_code,
    };

    #[test]
    fn approval_creates_workspace_wide_authorized_device() {
        let request = DeviceApprovalRequest {
            request_id: DeviceApprovalRequestId::new("request_linux"),
            workspace_id: WorkspaceId::new("workspace_code"),
            requester_device_id: DeviceId::new("device_linux"),
            device_name: "linux-server-1".to_string(),
            platform: DevicePlatform::Linux,
            device_public_key: PublicDeviceKey::new("age1public"),
            device_fingerprint: DeviceFingerprint::new("fp_linux"),
            matching_code: "maple-river-4821".to_string(),
            requested_at: "2026-06-23T12:00:00Z".to_string(),
            expires_at: "2026-06-23T12:10:00Z".to_string(),
            state: DeviceApprovalRequestState::Pending,
            host: Some("linux-server-1".to_string()),
            root: Some("~/Code".to_string()),
            setup_receipts_digest: None,
        };

        let device = approve_device(
            &request,
            DeviceId::new("device_mac"),
            "2026-06-23T12:00:05Z",
        )
        .expect("valid approval");

        assert_eq!(
            device,
            AuthorizedDevice {
                authorized_at: "2026-06-23T12:00:05Z".to_string(),
                authorized_by_device_id: Some(DeviceId::new("device_mac")),
                id: DeviceId::new("device_linux"),
                name: "linux-server-1".to_string(),
                workspace_id: WorkspaceId::new("workspace_code"),
                platform: DevicePlatform::Linux,
                device_fingerprint: DeviceFingerprint::new("fp_linux"),
            }
        );
    }

    #[test]
    fn display_matching_code_groups_first_eight_digest_hex() {
        assert_eq!(
            display_matching_code(
                "bowline-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
            "0123-4567"
        );
    }

    #[test]
    fn display_matching_code_is_deterministic_and_distinguishes_prefixes() {
        let first = "bowline-aaaaaaaa9abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let second = "bowline-bbbbbbbb9abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        assert_eq!(display_matching_code(first), display_matching_code(first));
        assert_ne!(display_matching_code(first), display_matching_code(second));
    }

    #[test]
    fn display_matching_code_handles_short_unexpected_inputs() {
        assert_eq!(display_matching_code("bowline-ab"), "ab");
        assert_eq!(display_matching_code("xyz"), "xyz");
    }
}
