use std::{error::Error, fmt};

use bowline_control_plane::{
    ControlPlaneError, DeviceDenialInput, DeviceRevocationInput, RejectionCode,
};
use bowline_core::{
    commands::{
        CONTRACT_VERSION, CommandRecoverability, DeviceCommandAction, DevicesCommandOutput,
    },
    devices::{
        DeviceApprovalRequestState, DeviceFingerprint, DeviceRecord, DeviceTrustState,
        RecoveryKeyState, RevokedDevice, display_matching_code,
    },
    ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId},
    status::RepairCommand,
};
use bowline_local::metadata::{MetadataStore, default_database_path};
use bowline_local::trust::{self, ApproveDeviceOptions, DeviceRequestOptions, grants};

use crate::{TrustRequestSelector, WorkspaceSelection, resolve_explicit_path, runtime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicesArgs {
    List {
        selection: WorkspaceSelection,
    },
    Request {
        selection: WorkspaceSelection,
    },
    Accept {
        selection: WorkspaceSelection,
        request_id: String,
    },
}

impl DevicesArgs {
    pub fn command_name(&self) -> bowline_core::commands::CommandName {
        match self {
            Self::List { .. } => bowline_core::commands::CommandName::Devices,
            Self::Request { .. } => bowline_core::commands::CommandName::DeviceRequest,
            Self::Accept { .. } => bowline_core::commands::CommandName::DeviceAccept,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRequestSelectorError {
    NoMatch,
    Ambiguous,
}

impl fmt::Display for DeviceRequestSelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMatch => formatter.write_str("No pending device request matches that code."),
            Self::Ambiguous => formatter
                .write_str("Multiple pending device requests match that code; use --request <id>."),
        }
    }
}

impl Error for DeviceRequestSelectorError {}

#[derive(Debug)]
pub enum DeviceCommandError {
    Runtime(String),
    Selector(DeviceRequestSelectorError),
    RequestRequiresAction(String),
    TrustRequiresAction(String),
    SafetyBlocked(String),
}

impl DeviceCommandError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Runtime(_) => "runtime_error",
            Self::Selector(DeviceRequestSelectorError::NoMatch) => "device_request_code_not_found",
            Self::Selector(DeviceRequestSelectorError::Ambiguous) => {
                "device_request_code_ambiguous"
            }
            Self::RequestRequiresAction(_) => "device_request_requires_action",
            Self::TrustRequiresAction(_) => "device_trust_requires_action",
            Self::SafetyBlocked(_) => "device_trust_blocked",
        }
    }

    pub fn recoverability(&self) -> CommandRecoverability {
        match self {
            Self::Runtime(_) => CommandRecoverability::Retry,
            Self::SafetyBlocked(_) => CommandRecoverability::Unsupported,
            _ => CommandRecoverability::UserAction,
        }
    }

    pub fn remediation(&self) -> Option<&'static str> {
        match self {
            Self::Selector(_) | Self::RequestRequiresAction(_) => Some(
                "Run `bowline device list --json`, then retry with `--request <id>` for a pending request.",
            ),
            Self::TrustRequiresAction(_) => Some(
                "Inspect device trust with `bowline device list --json` and complete the required trust action.",
            ),
            Self::SafetyBlocked(_) => Some(
                "Inspect device trust and control-plane capabilities before retrying this action.",
            ),
            Self::Runtime(_) => None,
        }
    }
}

impl fmt::Display for DeviceCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(message)
            | Self::RequestRequiresAction(message)
            | Self::TrustRequiresAction(message)
            | Self::SafetyBlocked(message) => formatter.write_str(message),
            Self::Selector(error) => error.fmt(formatter),
        }
    }
}

impl Error for DeviceCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Selector(error) => Some(error),
            _ => None,
        }
    }
}

impl From<String> for DeviceCommandError {
    fn from(message: String) -> Self {
        Self::Runtime(message)
    }
}

impl From<DeviceRequestSelectorError> for DeviceCommandError {
    fn from(error: DeviceRequestSelectorError) -> Self {
        Self::Selector(error)
    }
}

pub fn pending_requests(
    workspace_id: &WorkspaceId,
) -> Result<Vec<bowline_core::devices::DeviceApprovalRequest>, DeviceCommandError> {
    let control_plane = runtime::control_plane().map_err(DeviceCommandError::Runtime)?;
    let trust = control_plane
        .list_device_trust(workspace_id)
        .map_err(classify_control_plane_error)?;
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

pub fn request_id_for_selector(
    workspace_id: &WorkspaceId,
    selector: &TrustRequestSelector,
) -> Result<String, DeviceCommandError> {
    match selector {
        TrustRequestSelector::Request(request_id) => Ok(request_id.clone()),
        TrustRequestSelector::Code(code) => {
            let matches = pending_requests(workspace_id)?
                .into_iter()
                .filter(|request| request_matches_selector_code(request, code))
                .collect::<Vec<_>>();
            request_id_for_code_matches(&matches)
        }
    }
}

fn request_id_for_code_matches(
    matches: &[bowline_core::devices::DeviceApprovalRequest],
) -> Result<String, DeviceCommandError> {
    match matches {
        [request] => Ok(request.request_id.as_str().to_string()),
        [] => Err(DeviceRequestSelectorError::NoMatch.into()),
        _ => Err(DeviceRequestSelectorError::Ambiguous.into()),
    }
}

fn request_matches_selector_code(
    request: &bowline_core::devices::DeviceApprovalRequest,
    code: &str,
) -> bool {
    display_matching_code(&request.matching_code) == code
}

pub fn approve(
    workspace_id: WorkspaceId,
    request_id: String,
    generated_at: String,
) -> Result<DevicesCommandOutput, DeviceCommandError> {
    let control_plane = runtime::control_plane().map_err(DeviceCommandError::Runtime)?;
    let key_store = runtime::key_store().map_err(DeviceCommandError::Runtime)?;
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
    .map_err(classify_trust_error)
}

pub fn deny(
    workspace_id: WorkspaceId,
    request_id: String,
    generated_at: String,
) -> Result<DevicesCommandOutput, DeviceCommandError> {
    let control_plane = runtime::control_plane().map_err(DeviceCommandError::Runtime)?;
    let key_store = runtime::key_store().map_err(DeviceCommandError::Runtime)?;
    let local_device_id = runtime::daemon_device_id(&workspace_id);
    let identity = key_store
        .load_or_create_device_identity()
        .map_err(|error| DeviceCommandError::Runtime(error.to_string()))?;
    let denied_by_device_proof = grants::device_authorization_proof(
        &identity,
        &workspace_id,
        &local_device_id,
        "deny-device-request",
        &grants::device_request_proof_subject(&request_id),
    )
    .map_err(|error| DeviceCommandError::Runtime(error.to_string()))?;
    let denial = control_plane
        .deny_device_request(DeviceDenialInput {
            request_id: DeviceApprovalRequestId::new(request_id.clone()),
            denied_by_device_id: local_device_id.clone(),
            denied_by_device_proof,
            reason: "denied by bowline device deny".to_string(),
        })
        .map_err(classify_control_plane_error)?;
    Ok(DevicesCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Deny,
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
        next_actions: vec![RepairCommand::inspect(
            format!("Denied request {}", denial.request_id),
            None,
        )],
    })
}

pub fn revoke(
    workspace_id: WorkspaceId,
    device_id: String,
    generated_at: String,
) -> Result<DevicesCommandOutput, DeviceCommandError> {
    let control_plane = runtime::control_plane().map_err(DeviceCommandError::Runtime)?;
    let key_store = runtime::key_store().map_err(DeviceCommandError::Runtime)?;
    let local_device_id = runtime::daemon_device_id(&workspace_id);
    let identity = key_store
        .load_or_create_device_identity()
        .map_err(|error| DeviceCommandError::Runtime(error.to_string()))?;
    let revoked_by_device_proof = grants::device_authorization_proof(
        &identity,
        &workspace_id,
        &local_device_id,
        "revoke-device",
        &grants::device_revocation_proof_subject(&device_id),
    )
    .map_err(|error| DeviceCommandError::Runtime(error.to_string()))?;
    let revoked = control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new(device_id),
            revoked_by_device_id: local_device_id.clone(),
            revoked_by_device_proof,
            reason: "revoked by bowline device revoke".to_string(),
        })
        .map_err(classify_control_plane_error)?;
    let revoked_device_id = DeviceId::new(revoked.device_id);
    let revoked_at = revoked.revoked_at.to_string();
    if let Err(error) =
        revoke_local_mcp_tokens_for_device(&workspace_id, &revoked_device_id, &revoked_at)
    {
        eprintln!(
            "bowline device revoke failed to revoke local MCP tokens for {}: {error}",
            revoked_device_id.as_str()
        );
    }
    let revoked_device = RevokedDevice {
        id: revoked_device_id,
        name: revoked.device_name,
        workspace_id: workspace_id.clone(),
        platform: platform_from_str(&revoked.platform),
        device_fingerprint: DeviceFingerprint::new(revoked.device_fingerprint),
        revoked_at,
        revoked_by_device_id: DeviceId::new(revoked.revoked_by_device_id),
        reason: revoked.reason,
    };
    Ok(DevicesCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Revoke,
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

fn revoke_local_mcp_tokens_for_device(
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    revoked_at: &str,
) -> Result<(), String> {
    let db_path = crate::io_helpers::metadata_db_path()
        .or_else(|| default_database_path().ok())
        .ok_or_else(|| "metadata database path is unavailable".to_string())?;
    let store = MetadataStore::open(db_path).map_err(|error| error.to_string())?;
    for lease in store
        .agent_leases(workspace_id)
        .map_err(|error| error.to_string())?
    {
        if &lease.device_id == device_id {
            store
                .revoke_agent_mcp_tokens_for_lease(&lease.id, revoked_at)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub fn run(
    args: DevicesArgs,
    generated_at: String,
) -> Result<DevicesCommandOutput, DeviceCommandError> {
    let control_plane = runtime::control_plane().map_err(DeviceCommandError::Runtime)?;
    let key_store = runtime::key_store().map_err(DeviceCommandError::Runtime)?;

    match args {
        DevicesArgs::List { selection } => {
            let workspace_id =
                workspace_id_for_selection(&selection).map_err(DeviceCommandError::Runtime)?;
            let local_device_id = runtime::daemon_device_id(&workspace_id);
            let trust = control_plane
                .list_device_trust(&workspace_id)
                .map_err(classify_control_plane_error)?;
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
        DevicesArgs::Request { selection } => {
            let workspace_id =
                workspace_id_for_selection(&selection).map_err(DeviceCommandError::Runtime)?;
            let request = trust::create_device_request(
                &*control_plane,
                &*key_store,
                DeviceRequestOptions {
                    workspace_id: workspace_id.clone(),
                    device_id: runtime::device_id(),
                    device_name: runtime::device_name(),
                    platform: runtime::platform(),
                    host: None,
                    lease_id: None,
                    root: Some(selection.root),
                    runtime: None,
                    generated_at: generated_at.clone(),
                },
            )
            .map_err(classify_trust_error)?;
            let mut output = trust::devices_output_for_request(generated_at, request);
            output.command = bowline_core::commands::CommandName::DeviceRequest;
            Ok(output)
        }
        DevicesArgs::Accept {
            selection,
            request_id,
        } => {
            let workspace_id =
                workspace_id_for_selection(&selection).map_err(DeviceCommandError::Runtime)?;
            let grant = trust::accept_device_grant(
                &*control_plane,
                &*key_store,
                &workspace_id,
                &DeviceApprovalRequestId::new(request_id),
                &runtime::device_id(),
            )
            .map_err(classify_trust_error)?;
            let identity = key_store
                .load_or_create_device_identity()
                .map_err(|error| DeviceCommandError::Runtime(error.to_string()))?;
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
                command: bowline_core::commands::CommandName::DeviceAccept,
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
    }
}

fn classify_trust_error(error: trust::TrustError) -> DeviceCommandError {
    match error {
        error @ trust::TrustError::MissingPendingRequest(_) => {
            DeviceCommandError::RequestRequiresAction(error.to_string())
        }
        error @ trust::TrustError::MissingWorkspaceKey(_) => {
            DeviceCommandError::TrustRequiresAction(error.to_string())
        }
        trust::TrustError::ControlPlane(error) => classify_control_plane_error(error),
        error @ trust::TrustError::DeviceKeys(_) => DeviceCommandError::Runtime(error.to_string()),
        error @ trust::TrustError::Grant(_) => DeviceCommandError::SafetyBlocked(error.to_string()),
    }
}

fn classify_control_plane_error(error: ControlPlaneError) -> DeviceCommandError {
    let message = error.to_string();
    match error {
        ControlPlaneError::DeviceRequestMissing { .. } => {
            DeviceCommandError::RequestRequiresAction(message)
        }
        ControlPlaneError::Conflict { .. } => DeviceCommandError::TrustRequiresAction(message),
        ControlPlaneError::Rejected {
            code:
                RejectionCode::DeviceNotTrusted
                | RejectionCode::InvalidRequest
                | RejectionCode::Unauthorized
                | RejectionCode::WorkspaceMembershipRequired
                | RejectionCode::WorkspaceOwnerRequired,
            ..
        }
        | ControlPlaneError::WorkspaceMissing { .. } => {
            DeviceCommandError::TrustRequiresAction(message)
        }
        ControlPlaneError::Limited { .. } | ControlPlaneError::Unsupported { .. } => {
            DeviceCommandError::SafetyBlocked(message)
        }
        _ => DeviceCommandError::Runtime(message),
    }
}

fn workspace_id_for_selection(selection: &WorkspaceSelection) -> Result<WorkspaceId, String> {
    runtime::workspace_id_for_root(&resolve_explicit_path(selection.root.clone()))
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
        lease_id: request.lease_id.map(String::from),
        lease_handoff_digest: request.lease_handoff_digest,
        root: request.root,
        setup_receipts_digest: request.setup_receipts_digest,
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
    use bowline_core::{
        commands::{CommandErrorStatus, CommandExitCode},
        devices::{
            DeviceApprovalRequest, DeviceApprovalRequestState, DeviceFingerprint, DevicePlatform,
            PublicDeviceKey,
        },
        ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId},
    };

    use super::*;

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

    #[test]
    fn selector_code_matches_display_token_not_full_binding_code() {
        let request = device_request("device-request:ws_code:dev-mac");

        assert!(request_matches_selector_code(&request, "0123-4567"));
        assert!(!request_matches_selector_code(
            &request,
            "bowline-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn selector_code_no_match_requires_user_action_exit() {
        let error = request_id_for_code_matches(&[]).expect_err("selector should not match");

        assert_eq!(error.code(), "device_request_code_not_found");
        assert_eq!(error.recoverability(), CommandRecoverability::UserAction);
        assert_eq!(
            CommandExitCode::for_error(CommandErrorStatus::Failed, error.recoverability()),
            CommandExitCode::UserActionRequired
        );
    }

    #[test]
    fn selector_code_multiple_matches_require_user_action_exit() {
        let requests = vec![
            device_request("device-request:ws_code:dev-mac"),
            device_request("device-request:ws_code:dev-linux"),
        ];
        let error = request_id_for_code_matches(&requests).expect_err("selector is ambiguous");

        assert_eq!(error.code(), "device_request_code_ambiguous");
        assert_eq!(error.recoverability(), CommandRecoverability::UserAction);
        assert_eq!(
            CommandExitCode::for_error(CommandErrorStatus::Failed, error.recoverability()),
            CommandExitCode::UserActionRequired
        );
    }

    #[test]
    fn control_plane_state_errors_require_action_while_transport_errors_retry() {
        let missing = classify_control_plane_error(ControlPlaneError::DeviceRequestMissing {
            request_id: DeviceApprovalRequestId::new("missing-request"),
        });
        let stale = classify_control_plane_error(ControlPlaneError::Conflict {
            resource: "device request",
            reason: "only pending requests can be denied",
        });
        let timeout = classify_control_plane_error(ControlPlaneError::Timeout {
            capability: "device-trust",
        });
        let blocked = classify_control_plane_error(ControlPlaneError::Unsupported {
            capability: "device-trust",
            reason: "trust mutation is disabled",
        });

        assert_eq!(missing.recoverability(), CommandRecoverability::UserAction);
        assert_eq!(stale.recoverability(), CommandRecoverability::UserAction);
        assert_eq!(timeout.recoverability(), CommandRecoverability::Retry);
        assert_eq!(blocked.recoverability(), CommandRecoverability::Unsupported);
        assert_eq!(
            CommandExitCode::for_error(CommandErrorStatus::Failed, timeout.recoverability()),
            CommandExitCode::RetryableRuntimeError
        );
        assert_eq!(
            CommandExitCode::for_error(CommandErrorStatus::Failed, blocked.recoverability()),
            CommandExitCode::BlockedOrDegradedBySafety
        );
    }

    fn device_request(request_id: &str) -> DeviceApprovalRequest {
        DeviceApprovalRequest {
            request_id: DeviceApprovalRequestId::new(request_id),
            workspace_id: WorkspaceId::new("ws_code"),
            requester_device_id: DeviceId::new("dev_mac"),
            device_name: "Dev-Mac".to_string(),
            platform: DevicePlatform::Macos,
            device_public_key: PublicDeviceKey::new("age1public"),
            device_fingerprint: DeviceFingerprint::new("fp_mac"),
            matching_code:
                "bowline-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(),
            requested_at: "2026-07-09T12:00:00Z".to_string(),
            expires_at: "2026-07-09T12:10:00Z".to_string(),
            state: DeviceApprovalRequestState::Pending,
            host: None,
            lease_id: None,
            lease_handoff_digest: None,
            root: Some("~/Code".to_string()),
            setup_receipts_digest: None,
        }
    }
}
