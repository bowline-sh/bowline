use super::*;

pub(crate) fn advance_device_trust(
    output: &mut bowline_core::commands::RootInitOutput,
    generated_at: &str,
) -> DeviceTrustAttachment {
    let message = match fetch_device_trust_context(output) {
        DeviceTrustFetch::ProbeDisabled => DeviceTrustMessage::LogInBeforeSync,
        DeviceTrustFetch::SecretStoreUnavailable => DeviceTrustMessage::CheckSecretStore,
        DeviceTrustFetch::LoginRequired => DeviceTrustMessage::LogInBeforeSync,
        DeviceTrustFetch::ControlPlaneUnavailable => DeviceTrustMessage::CheckControlPlane,
        DeviceTrustFetch::TrustUnavailable(error) => {
            DeviceTrustMessage::TrustSetupUnavailable(error)
        }
        DeviceTrustFetch::Ready(context) => {
            resolve_device_trust_state(output, generated_at, context)
        }
    };
    append_device_trust_message(output, message)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceTrustAttachment {
    NotReady,
    Ready,
    PendingApproval { request_id: String },
}

impl DeviceTrustAttachment {
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub(crate) fn pending_request_id(&self) -> Option<String> {
        match self {
            Self::NotReady | Self::Ready => None,
            Self::PendingApproval { request_id } => Some(request_id.clone()),
        }
    }
}

enum DeviceTrustFetch {
    ProbeDisabled,
    SecretStoreUnavailable,
    LoginRequired,
    ControlPlaneUnavailable,
    TrustUnavailable(String),
    Ready(DeviceTrustContext),
}

struct DeviceTrustContext {
    control_plane: Box<dyn bowline_control_plane::ControlPlaneClient>,
    key_store: Box<dyn bowline_local::device_keys::DeviceKeyStore>,
    trust: bowline_control_plane::DeviceApprovalRequestList,
    current_device_id: DeviceId,
    current_device_fingerprint: String,
    current_device_platform: bowline_core::devices::DevicePlatform,
}

fn fetch_device_trust_context(output: &bowline_core::commands::RootInitOutput) -> DeviceTrustFetch {
    if !runtime::passive_secret_store_probe_allowed() {
        return DeviceTrustFetch::ProbeDisabled;
    }
    let Ok(key_store) = runtime::key_store() else {
        return DeviceTrustFetch::SecretStoreUnavailable;
    };
    if !account_auth_available(&*key_store) {
        return DeviceTrustFetch::LoginRequired;
    }
    let Ok(control_plane) = runtime::control_plane() else {
        return DeviceTrustFetch::ControlPlaneUnavailable;
    };

    let trust = match control_plane.list_device_trust(&output.workspace_id) {
        Ok(trust) => trust,
        Err(error) => return DeviceTrustFetch::TrustUnavailable(error.to_string()),
    };
    if trust.authorized_devices.is_empty()
        && let Err(error) = control_plane.create_workspace_ref(&output.workspace_id)
    {
        return DeviceTrustFetch::TrustUnavailable(error.to_string());
    }
    let identity = match key_store.load_or_create_device_identity() {
        Ok(identity) => identity,
        Err(_) => return DeviceTrustFetch::SecretStoreUnavailable,
    };
    DeviceTrustFetch::Ready(DeviceTrustContext {
        control_plane,
        key_store,
        trust,
        current_device_id: runtime::daemon_device_id(&output.workspace_id),
        current_device_fingerprint: identity.fingerprint.as_str().to_string(),
        current_device_platform: runtime::platform(),
    })
}

fn account_auth_available(key_store: &dyn bowline_local::device_keys::DeviceKeyStore) -> bool {
    match key_store.load_account_tokens() {
        Ok(Some(_tokens)) => true,
        Ok(None) | Err(_) => {
            env_account_session_present()
                || env_workos_access_token_present()
                || env_control_plane_token_present()
                || env_bootstrap_token_present()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeviceTrustState {
    BootstrapFirstTrustedDevice,
    CurrentDeviceTrusted,
    AcceptApprovedRequest {
        request_id: DeviceApprovalRequestId,
    },
    WaitForApproval {
        request_id: String,
        device_name: String,
        matching_code: String,
    },
    RequestApprovalFromTrustedDevice,
}

fn device_trust_state(
    trust: &bowline_control_plane::DeviceApprovalRequestList,
    current_device_id: &DeviceId,
    current_device_fingerprint: &str,
    current_device_platform: bowline_core::devices::DevicePlatform,
) -> DeviceTrustState {
    if trust.authorized_devices.is_empty() {
        return DeviceTrustState::BootstrapFirstTrustedDevice;
    }
    if trust.authorized_devices.iter().any(|device| {
        device.device_id == current_device_id.as_str()
            && device.device_fingerprint == current_device_fingerprint
            && device.platform == device_platform_label(current_device_platform)
            && device.revoked_at.is_none()
    }) {
        return DeviceTrustState::CurrentDeviceTrusted;
    }

    // The first device creates the workspace key locally; every later device must
    // prove user presence on an already trusted device before accepting a grant.
    let Some(request) = trust.pending_requests.iter().find(|request| {
        request.device_id == current_device_id.as_str()
            && request.device_fingerprint == current_device_fingerprint
            && request.platform == device_platform_label(current_device_platform)
    }) else {
        return DeviceTrustState::RequestApprovalFromTrustedDevice;
    };
    match request.state {
        bowline_control_plane::DeviceRequestState::Approved => {
            DeviceTrustState::AcceptApprovedRequest {
                request_id: DeviceApprovalRequestId::new(request.request_id.clone()),
            }
        }
        bowline_control_plane::DeviceRequestState::Pending => DeviceTrustState::WaitForApproval {
            request_id: request.request_id.clone().into(),
            device_name: request.device_name.clone(),
            matching_code: request.matching_code.clone(),
        },
        bowline_control_plane::DeviceRequestState::Denied
        | bowline_control_plane::DeviceRequestState::Expired => {
            DeviceTrustState::RequestApprovalFromTrustedDevice
        }
    }
}

fn device_platform_label(platform: bowline_core::devices::DevicePlatform) -> &'static str {
    match platform {
        bowline_core::devices::DevicePlatform::Macos => "macos",
        bowline_core::devices::DevicePlatform::Linux => "linux",
        bowline_core::devices::DevicePlatform::Unknown => "unknown",
    }
}

fn resolve_device_trust_state(
    output: &bowline_core::commands::RootInitOutput,
    generated_at: &str,
    context: DeviceTrustContext,
) -> DeviceTrustMessage {
    match device_trust_state(
        &context.trust,
        &context.current_device_id,
        &context.current_device_fingerprint,
        context.current_device_platform,
    ) {
        DeviceTrustState::BootstrapFirstTrustedDevice => {
            ensure_first_trust_root(output, generated_at, &context)
        }
        DeviceTrustState::CurrentDeviceTrusted => DeviceTrustMessage::InspectStatus,
        DeviceTrustState::AcceptApprovedRequest { request_id } => {
            accept_device_grant(output, &context, &request_id)
        }
        DeviceTrustState::WaitForApproval {
            request_id,
            device_name,
            matching_code,
        } => DeviceTrustMessage::ApproveOnTrustedDevice {
            request_id,
            device_name,
            matching_code,
        },
        DeviceTrustState::RequestApprovalFromTrustedDevice => {
            create_trust_request(output, generated_at, &context)
        }
    }
}

fn accept_device_grant(
    output: &bowline_core::commands::RootInitOutput,
    context: &DeviceTrustContext,
    request_id: &DeviceApprovalRequestId,
) -> DeviceTrustMessage {
    match bowline_local::trust::accept_device_grant(
        &*context.control_plane,
        &*context.key_store,
        &output.workspace_id,
        request_id,
        &context.current_device_id,
    ) {
        Ok(_) => DeviceTrustMessage::InspectStatus,
        Err(error) => DeviceTrustMessage::DeviceGrantNotAccepted(error.to_string()),
    }
}

fn create_trust_request(
    output: &bowline_core::commands::RootInitOutput,
    generated_at: &str,
    context: &DeviceTrustContext,
) -> DeviceTrustMessage {
    match bowline_local::trust::create_device_request(
        &*context.control_plane,
        &*context.key_store,
        bowline_local::trust::DeviceRequestOptions {
            workspace_id: output.workspace_id.clone(),
            device_id: context.current_device_id.clone(),
            device_name: runtime::device_name(),
            platform: context.current_device_platform,
            host: None,
            lease_id: None,
            root: Some(output.root.clone()),
            runtime: None,
            generated_at: generated_at.to_string(),
        },
    ) {
        Ok(request) => DeviceTrustMessage::ApproveOnTrustedDevice {
            request_id: request.request_id.as_str().to_string(),
            device_name: request.device_name,
            matching_code: request.matching_code,
        },
        Err(error) => DeviceTrustMessage::DeviceApprovalRequestNotCreated(error.to_string()),
    }
}

fn ensure_first_trust_root(
    output: &bowline_core::commands::RootInitOutput,
    generated_at: &str,
    context: &DeviceTrustContext,
) -> DeviceTrustMessage {
    match bowline_local::trust::ensure_first_device_trust_root(
        &*context.control_plane,
        &*context.key_store,
        output.workspace_id.clone(),
        context.current_device_id.clone(),
        runtime::device_name(),
        context.current_device_platform,
        generated_at.to_string(),
    ) {
        Ok(_) => DeviceTrustMessage::CreateRecoveryKey,
        Err(error) => DeviceTrustMessage::TrustRootNotCreated(error.to_string()),
    }
}

enum DeviceTrustMessage {
    LogInBeforeSync,
    CheckSecretStore,
    CheckControlPlane,
    TrustSetupUnavailable(String),
    InspectStatus,
    DeviceGrantNotAccepted(String),
    ApproveOnTrustedDevice {
        request_id: String,
        device_name: String,
        matching_code: String,
    },
    DeviceApprovalRequestNotCreated(String),
    CreateRecoveryKey,
    TrustRootNotCreated(String),
}

fn append_device_trust_message(
    output: &mut bowline_core::commands::RootInitOutput,
    message: DeviceTrustMessage,
) -> DeviceTrustAttachment {
    let (label, command, attachment, mutates) = match message {
        DeviceTrustMessage::LogInBeforeSync => (
            "Log in before enabling workspace sync".to_string(),
            Some("bowline login".to_string()),
            DeviceTrustAttachment::NotReady,
            false,
        ),
        DeviceTrustMessage::CheckSecretStore => (
            "Check local secret store before enabling sync".to_string(),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NotReady,
            false,
        ),
        DeviceTrustMessage::CheckControlPlane => (
            "Check control-plane connectivity before enabling sync".to_string(),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NotReady,
            false,
        ),
        DeviceTrustMessage::TrustSetupUnavailable(error) => (
            format!("Trust setup unavailable: {error}"),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NotReady,
            false,
        ),
        DeviceTrustMessage::InspectStatus => (
            "Inspect workspace status".to_string(),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::Ready,
            false,
        ),
        DeviceTrustMessage::DeviceGrantNotAccepted(error) => (
            format!("Device grant not accepted: {error}"),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NotReady,
            false,
        ),
        DeviceTrustMessage::ApproveOnTrustedDevice {
            request_id,
            device_name,
            matching_code,
        } => (
            format!(
                "Approve {device_name} with code {} on a trusted device",
                bowline_core::devices::display_matching_code(&matching_code)
            ),
            None,
            DeviceTrustAttachment::PendingApproval { request_id },
            true,
        ),
        DeviceTrustMessage::DeviceApprovalRequestNotCreated(error) => (
            format!("Device approval request not created: {error}"),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NotReady,
            false,
        ),
        DeviceTrustMessage::CreateRecoveryKey => (
            "Create a Recovery Key".to_string(),
            Some("bowline recover create".to_string()),
            DeviceTrustAttachment::Ready,
            true,
        ),
        DeviceTrustMessage::TrustRootNotCreated(error) => (
            format!("Trust root not created: {error}"),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NotReady,
            false,
        ),
    };
    let action = if mutates {
        RepairCommand::mutating(label, command)
    } else {
        RepairCommand::inspect(label, command)
    };
    output.next_actions.push(action);
    attachment
}

fn status_command(root: &str) -> String {
    root_command("bowline status --root", root)
}

pub(super) fn root_command(prefix: &str, root: &str) -> String {
    format!("{prefix} {}", io_helpers::shell_word(root))
}

pub(crate) fn wait_for_device_grant(
    workspace_id: WorkspaceId,
    request_id: String,
) -> Result<(), String> {
    println!(
        "Waiting for approval. On a trusted device, run `bowline device approve --root <path> --request {request_id}`."
    );
    let control_plane = runtime::control_plane()?;
    let key_store = runtime::key_store()?;
    let request_id = DeviceApprovalRequestId::new(request_id);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match bowline_local::trust::accept_device_grant(
            &*control_plane,
            &*key_store,
            &workspace_id,
            &request_id,
            &runtime::device_id(),
        ) {
            Ok(_) => return Ok(()),
            Err(bowline_local::trust::TrustError::MissingPendingRequest(_)) => {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for device approval; run `bowline setup --root <path> --json` to leave the request pending".to_string());
                }
                thread::sleep(Duration::from_secs(2));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use bowline_control_plane::{
        AuthorizedDeviceRecord, ControlPlaneTimestamp, DeviceApprovalRequestList,
    };
    use bowline_core::{
        devices::DevicePlatform,
        ids::{DeviceId, WorkspaceId},
    };

    use super::{DeviceTrustState, device_trust_state};

    #[test]
    fn authorized_device_requires_matching_id_fingerprint_and_platform() {
        let trust = trust_with_authorized_device("device_shared", "fp_mac", "macos");
        let device_id = DeviceId::new("device_shared");

        assert_eq!(
            device_trust_state(&trust, &device_id, "fp_mac", DevicePlatform::Macos),
            DeviceTrustState::CurrentDeviceTrusted
        );
        assert_eq!(
            device_trust_state(&trust, &device_id, "fp_linux", DevicePlatform::Linux),
            DeviceTrustState::RequestApprovalFromTrustedDevice
        );
        assert_eq!(
            device_trust_state(&trust, &device_id, "fp_mac", DevicePlatform::Linux),
            DeviceTrustState::RequestApprovalFromTrustedDevice
        );
    }

    fn trust_with_authorized_device(
        device_id: &str,
        fingerprint: &str,
        platform: &str,
    ) -> DeviceApprovalRequestList {
        DeviceApprovalRequestList {
            pending_requests: Vec::new(),
            authorized_devices: vec![AuthorizedDeviceRecord {
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new(device_id),
                device_name: "Local device".to_string(),
                platform: platform.to_string(),
                device_fingerprint: fingerprint.to_string(),
                authorized_at: ControlPlaneTimestamp { tick: 1 },
                authorized_by_device_id: None,
                device_authorization_proof_verifier: Some("dapv_local".to_string()),
                revoked_at: None,
            }],
            revoked_devices: Vec::new(),
        }
    }
}
