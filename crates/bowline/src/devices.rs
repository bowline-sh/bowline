use bowline_control_plane::{DeviceDenialInput, DeviceRevocationInput};
use bowline_core::{
    commands::{CONTRACT_VERSION, DeviceCommandAction, DevicesCommandOutput},
    devices::{
        DeviceApprovalRequestState, DeviceFingerprint, DeviceRecord, DeviceTrustState,
        RecoveryKeyState, RevokedDevice,
    },
    ids::{DeviceApprovalRequestId, DeviceId},
    status::SafeAction,
};
use bowline_local::trust::{self, ApproveDeviceOptions, DeviceRequestOptions, grants};

use crate::runtime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicesArgs {
    List,
    Request { root: Option<String> },
    Approve { request_id: String },
    Accept { request_id: String },
    Deny { request_id: String },
    Revoke { device_id: String },
}

pub fn pending_requests() -> Result<Vec<bowline_core::devices::DeviceApprovalRequest>, String> {
    let control_plane = runtime::control_plane()?;
    let workspace_id = runtime::active_workspace_id();
    let trust = control_plane
        .list_device_trust(workspace_id.as_str())
        .map_err(|error| error.to_string())?;
    Ok(trust
        .pending_requests
        .into_iter()
        .map(local_request)
        .filter(|request| request_is_awaiting_approval(request.state))
        .collect())
}

fn request_is_awaiting_approval(state: DeviceApprovalRequestState) -> bool {
    matches!(state, DeviceApprovalRequestState::Pending)
}

pub fn approve(request_id: String, generated_at: String) -> Result<DevicesCommandOutput, String> {
    let control_plane = runtime::control_plane()?;
    let key_store = runtime::key_store()?;
    let workspace_id = runtime::active_workspace_id();
    trust::approve_device_request(
        &*control_plane,
        &*key_store,
        ApproveDeviceOptions {
            workspace_id: workspace_id.clone(),
            request_id: DeviceApprovalRequestId::new(request_id),
            approver_device_id: runtime::daemon_device_id(&workspace_id),
            generated_at,
        },
    )
    .map_err(|error| error.to_string())
}

pub fn run(args: DevicesArgs, generated_at: String) -> Result<DevicesCommandOutput, String> {
    let control_plane = runtime::control_plane()?;
    let key_store = runtime::key_store()?;
    let workspace_id = runtime::active_workspace_id();

    match args {
        DevicesArgs::List => {
            let local_device_id = runtime::daemon_device_id(&workspace_id);
            let trust = control_plane
                .list_device_trust(workspace_id.as_str())
                .map_err(|error| error.to_string())?;
            Ok(DevicesCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Devices,
                generated_at,
                action: DeviceCommandAction::List,
                workspace_id: Some(workspace_id.clone()),
                local_device: None,
                devices: trust
                    .authorized_devices
                    .into_iter()
                    .map(|device| DeviceRecord {
                        id: DeviceId::new(device.device_id.clone()),
                        name: device.device_name,
                        workspace_id: workspace_id.clone(),
                        platform: platform_from_str(&device.platform),
                        trust_state: DeviceTrustState::Trusted,
                        device_fingerprint: DeviceFingerprint::new(device.device_fingerprint),
                        authorized_at: Some(device.authorized_at.to_string()),
                        updated_at: device.authorized_at.to_string(),
                        is_current_device: device.device_id == local_device_id.as_str(),
                        limitation_reason: None,
                    })
                    .collect(),
                revoked_devices: trust
                    .revoked_devices
                    .into_iter()
                    .map(|device| RevokedDevice {
                        id: DeviceId::new(device.device_id),
                        name: device.device_name,
                        workspace_id: workspace_id.clone(),
                        platform: platform_from_str(&device.platform),
                        device_fingerprint: DeviceFingerprint::new(device.device_fingerprint),
                        revoked_at: device.revoked_at.to_string(),
                        revoked_by_device_id: DeviceId::new(device.revoked_by_device_id),
                        reason: device.reason,
                    })
                    .collect(),
                pending_requests: trust
                    .pending_requests
                    .into_iter()
                    .map(local_request)
                    .collect(),
                created_request: None,
                approved_device: None,
                denied_request: None,
                revoked_device: None,
                recovery_key: Some(RecoveryKeyState::missing()),
                next_actions: Vec::new(),
            })
        }
        DevicesArgs::Request { root } => {
            let request = trust::create_device_request(
                &*control_plane,
                &*key_store,
                DeviceRequestOptions {
                    workspace_id: workspace_id.clone(),
                    device_id: runtime::device_id(),
                    device_name: runtime::device_name(),
                    platform: runtime::platform(),
                    host: None,
                    root: Some(root.unwrap_or_else(|| "~/Code".to_string())),
                    generated_at: generated_at.clone(),
                },
            )
            .map_err(|error| error.to_string())?;
            Ok(trust::devices_output_for_request(generated_at, request))
        }
        DevicesArgs::Approve { request_id } => approve(request_id, generated_at),
        DevicesArgs::Accept { request_id } => {
            let grant = trust::accept_device_grant(
                &*control_plane,
                &*key_store,
                &workspace_id,
                &DeviceApprovalRequestId::new(request_id),
                &runtime::device_id(),
            )
            .map_err(|error| error.to_string())?;
            let identity = key_store
                .load_or_create_device_identity()
                .map_err(|error| error.to_string())?;
            let local_device = DeviceRecord {
                id: runtime::device_id(),
                name: runtime::device_name(),
                workspace_id: workspace_id.clone(),
                platform: runtime::platform(),
                trust_state: DeviceTrustState::Trusted,
                device_fingerprint: identity.fingerprint,
                authorized_at: grant.accepted_at.clone().or(Some(grant.created_at.clone())),
                updated_at: grant.accepted_at.unwrap_or(grant.created_at),
                is_current_device: true,
                limitation_reason: None,
            };
            Ok(DevicesCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Devices,
                generated_at,
                action: DeviceCommandAction::Accept,
                workspace_id: Some(workspace_id),
                local_device: Some(local_device.clone()),
                devices: vec![local_device.clone()],
                revoked_devices: Vec::new(),
                pending_requests: Vec::new(),
                created_request: None,
                approved_device: Some(local_device),
                denied_request: None,
                revoked_device: None,
                recovery_key: Some(RecoveryKeyState::missing()),
                next_actions: Vec::new(),
            })
        }
        DevicesArgs::Deny { request_id } => {
            let local_device_id = runtime::daemon_device_id(&workspace_id);
            let identity = key_store
                .load_or_create_device_identity()
                .map_err(|error| error.to_string())?;
            let denied_by_device_proof = grants::device_authorization_proof(
                &identity,
                &workspace_id,
                &local_device_id,
                "deny-device-request",
                &request_id,
            );
            let denial = control_plane
                .deny_device_request(DeviceDenialInput {
                    request_id: request_id.clone(),
                    denied_by_device_id: local_device_id.as_str().to_string(),
                    denied_by_device_proof,
                    reason: "denied by internal device trust command".to_string(),
                })
                .map_err(|error| error.to_string())?;
            Ok(DevicesCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Devices,
                generated_at,
                action: DeviceCommandAction::Deny,
                workspace_id: Some(workspace_id),
                local_device: None,
                devices: Vec::new(),
                revoked_devices: Vec::new(),
                pending_requests: Vec::new(),
                created_request: None,
                approved_device: None,
                denied_request: None,
                revoked_device: None,
                recovery_key: Some(RecoveryKeyState::missing()),
                next_actions: vec![SafeAction {
                    label: format!("Denied request {}", denial.request_id),
                    command: None,
                }],
            })
        }
        DevicesArgs::Revoke { device_id } => {
            let local_device_id = runtime::daemon_device_id(&workspace_id);
            let identity = key_store
                .load_or_create_device_identity()
                .map_err(|error| error.to_string())?;
            let revoked_by_device_proof = grants::device_authorization_proof(
                &identity,
                &workspace_id,
                &local_device_id,
                "revoke-device",
                &device_id,
            );
            let revoked = control_plane
                .revoke_device(DeviceRevocationInput {
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id,
                    revoked_by_device_id: local_device_id.as_str().to_string(),
                    revoked_by_device_proof,
                    reason: "revoked by bowline revoke".to_string(),
                })
                .map_err(|error| error.to_string())?;
            let revoked_device = RevokedDevice {
                id: DeviceId::new(revoked.device_id),
                name: revoked.device_name,
                workspace_id: workspace_id.clone(),
                platform: platform_from_str(&revoked.platform),
                device_fingerprint: DeviceFingerprint::new(revoked.device_fingerprint),
                revoked_at: revoked.revoked_at.to_string(),
                revoked_by_device_id: DeviceId::new(revoked.revoked_by_device_id),
                reason: revoked.reason,
            };
            Ok(DevicesCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Devices,
                generated_at,
                action: DeviceCommandAction::Revoke,
                workspace_id: Some(workspace_id),
                local_device: None,
                devices: Vec::new(),
                revoked_devices: vec![revoked_device.clone()],
                pending_requests: Vec::new(),
                created_request: None,
                approved_device: None,
                denied_request: None,
                revoked_device: Some(revoked_device),
                recovery_key: Some(RecoveryKeyState::missing()),
                next_actions: Vec::new(),
            })
        }
    }
}

fn local_request(
    request: bowline_control_plane::DeviceRequest,
) -> bowline_core::devices::DeviceApprovalRequest {
    bowline_core::devices::DeviceApprovalRequest {
        request_id: DeviceApprovalRequestId::new(request.request_id),
        workspace_id: bowline_core::ids::WorkspaceId::new(request.workspace_id),
        requester_device_id: DeviceId::new(request.device_id),
        device_name: request.device_name,
        platform: platform_from_str(&request.platform),
        device_public_key: bowline_core::devices::PublicDeviceKey::new(request.device_public_key),
        device_fingerprint: DeviceFingerprint::new(request.device_fingerprint),
        matching_code: request.matching_code,
        requested_at: request.requested_at.to_string(),
        expires_at: request.expires_at.to_string(),
        state: match request.state {
            bowline_control_plane::DeviceRequestState::Pending => {
                bowline_core::devices::DeviceApprovalRequestState::Pending
            }
            bowline_control_plane::DeviceRequestState::Approved => {
                bowline_core::devices::DeviceApprovalRequestState::Approved
            }
            bowline_control_plane::DeviceRequestState::Denied => {
                bowline_core::devices::DeviceApprovalRequestState::Denied
            }
            bowline_control_plane::DeviceRequestState::Expired => {
                bowline_core::devices::DeviceApprovalRequestState::Expired
            }
        },
        host: request.host,
        root: request.root,
    }
}

fn platform_from_str(value: &str) -> bowline_core::devices::DevicePlatform {
    match value {
        "macos" | "darwin" => bowline_core::devices::DevicePlatform::Macos,
        "linux" => bowline_core::devices::DevicePlatform::Linux,
        _ => bowline_core::devices::DevicePlatform::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::devices::DeviceApprovalRequestState;

    use super::request_is_awaiting_approval;

    #[test]
    fn only_pending_device_requests_are_awaiting_approval() {
        assert!(request_is_awaiting_approval(
            DeviceApprovalRequestState::Pending
        ));
        assert!(!request_is_awaiting_approval(
            DeviceApprovalRequestState::Approved
        ));
        assert!(!request_is_awaiting_approval(
            DeviceApprovalRequestState::Denied
        ));
        assert!(!request_is_awaiting_approval(
            DeviceApprovalRequestState::Expired
        ));
    }
}
