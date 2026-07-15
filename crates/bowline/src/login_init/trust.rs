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
    NoPendingApproval,
    PendingApproval { request_id: String },
}

impl DeviceTrustAttachment {
    pub(crate) fn pending_request_id(&self) -> Option<String> {
        match self {
            Self::NoPendingApproval => None,
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
    DeviceTrustFetch::Ready(DeviceTrustContext {
        control_plane,
        key_store,
        trust,
        current_device_id: runtime::daemon_device_id(&output.workspace_id),
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
) -> DeviceTrustState {
    if trust.authorized_devices.is_empty() {
        return DeviceTrustState::BootstrapFirstTrustedDevice;
    }
    if trust
        .authorized_devices
        .iter()
        .any(|device| device.device_id == current_device_id.as_str())
    {
        return DeviceTrustState::CurrentDeviceTrusted;
    }

    // The first device creates the workspace key locally; every later device must
    // prove user presence on an already trusted device before accepting a grant.
    let Some(request) = trust
        .pending_requests
        .iter()
        .find(|request| request.device_id == current_device_id.as_str())
    else {
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

fn resolve_device_trust_state(
    output: &bowline_core::commands::RootInitOutput,
    generated_at: &str,
    context: DeviceTrustContext,
) -> DeviceTrustMessage {
    match device_trust_state(&context.trust, &context.current_device_id) {
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
            device_id: runtime::device_id(),
            device_name: runtime::device_name(),
            platform: runtime::platform(),
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
        runtime::device_id(),
        runtime::device_name(),
        runtime::platform(),
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
            DeviceTrustAttachment::NoPendingApproval,
            false,
        ),
        DeviceTrustMessage::CheckSecretStore => (
            "Check local secret store before enabling sync".to_string(),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NoPendingApproval,
            false,
        ),
        DeviceTrustMessage::CheckControlPlane => (
            "Check control-plane connectivity before enabling sync".to_string(),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NoPendingApproval,
            false,
        ),
        DeviceTrustMessage::TrustSetupUnavailable(error) => (
            format!("Trust setup unavailable: {error}"),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NoPendingApproval,
            false,
        ),
        DeviceTrustMessage::InspectStatus => (
            "Inspect workspace status".to_string(),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NoPendingApproval,
            false,
        ),
        DeviceTrustMessage::DeviceGrantNotAccepted(error) => (
            format!("Device grant not accepted: {error}"),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NoPendingApproval,
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
            DeviceTrustAttachment::NoPendingApproval,
            false,
        ),
        DeviceTrustMessage::CreateRecoveryKey => (
            "Create a Recovery Key".to_string(),
            Some("bowline recover create".to_string()),
            DeviceTrustAttachment::NoPendingApproval,
            true,
        ),
        DeviceTrustMessage::TrustRootNotCreated(error) => (
            format!("Trust root not created: {error}"),
            Some(status_command(&output.root)),
            DeviceTrustAttachment::NoPendingApproval,
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
    command: CommandName,
    workspace_id: WorkspaceId,
    request_id: String,
    generated_at: String,
) -> ExitCode {
    println!(
        "Waiting for approval. On a trusted device, run `bowline device approve --root <path> --request {request_id}`."
    );
    let control_plane = match runtime::control_plane() {
        Ok(control_plane) => control_plane,
        Err(error) => {
            print_runtime_error(command, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let key_store = match runtime::key_store() {
        Ok(key_store) => key_store,
        Err(error) => {
            print_runtime_error(command, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
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
            Ok(_) => {
                println!("Device approved. Workspace is ready.");
                return ExitCode::SUCCESS;
            }
            Err(bowline_local::trust::TrustError::MissingPendingRequest(_)) => {
                if Instant::now() >= deadline {
                    print_runtime_error(
                        command,
                        generated_at,
                        "timed out waiting for device approval; run `bowline setup --root <path> --json` to leave the request pending",
                        false,
                    );
                    return ExitCode::from(EXIT_RUNTIME);
                }
                thread::sleep(Duration::from_secs(2));
            }
            Err(error) => {
                print_runtime_error(command, generated_at, &error.to_string(), false);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
}
