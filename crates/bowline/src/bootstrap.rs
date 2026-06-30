use std::{env, thread, time::Duration};

use bowline_control_plane::{BootstrapSessionInput, ControlPlaneClient};
use bowline_core::{
    commands::{
        AgentWriteTargetMode, BootstrapSecretStore, BootstrapSshCommandOutput, BootstrapStep,
        BootstrapStepState, BootstrapSyncState, CONTRACT_VERSION, DevicesCommandOutput,
        StatusCommandOutput,
    },
    devices::{DeviceApprovalRequest, DeviceFingerprint, DeviceRecord, DeviceTrustState},
    ids::DeviceId,
    status::{SafeAction, StatusLevel, WorkspaceStatus},
};
use bowline_local::bootstrap::{
    install::{self, BootstrapInstallOptions, RemoteBowlineInstall},
    process::{ProcessRunner, SystemProcessRunner},
    ssh::{self, BootstrapSshOptions},
};
use bowline_local::device_keys::DeviceKeyStore;

use crate::runtime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSshArgs {
    pub host: String,
    pub root: String,
    pub artifact: Option<String>,
    pub project: Option<String>,
    pub task: Option<String>,
    pub agent: Option<String>,
}

struct BootstrapOutputBase {
    host: String,
    root: String,
    generated_at: String,
    steps: Vec<BootstrapStep>,
    agent_handoff: Option<BootstrapAgentHandoff>,
}

struct BootstrapAgentHandoff {
    project: String,
    task: String,
    agent: Option<String>,
    lease_id: Option<String>,
    write_target_mode: Option<AgentWriteTargetMode>,
    write_target_path: Option<String>,
    work_view_id: Option<String>,
    work_view_path: Option<String>,
    launched: bool,
    accepted: bool,
}

struct RemoteAgentHandoffLease {
    lease_id: String,
    write_target_mode: AgentWriteTargetMode,
    write_target_path: String,
    work_view_id: Option<String>,
    work_view_path: Option<String>,
}

pub fn run(args: BootstrapSshArgs, generated_at: String) -> BootstrapSshCommandOutput {
    let args = normalize_remote_root(args);
    let runner = SystemProcessRunner;
    let mut steps = Vec::new();
    let install = match install::install_or_update_bowline(
        &runner,
        &BootstrapInstallOptions {
            host: args.host.clone(),
            root: args.root.clone(),
            artifact: args.artifact.clone().map(Into::into),
        },
    ) {
        Ok(install) => {
            steps.push(step(
                "install",
                BootstrapStepState::Completed,
                format!(
                    "Installed bowline and bowline-daemon for {} with artifacts {} / {}.",
                    install.platform.label(),
                    &install.artifact_sha256[..16],
                    &install.daemon_artifact_sha256[..16]
                ),
            ));
            install
        }
        Err(error) => {
            steps.push(step(
                "install",
                BootstrapStepState::Blocked,
                format!("Remote install failed: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        }
    };
    let control_plane = match runtime::control_plane() {
        Ok(control_plane) => control_plane,
        Err(error) => {
            steps.push(step(
                "control-plane",
                BootstrapStepState::Blocked,
                format!("Local control-plane client unavailable: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        }
    };
    let key_store = match runtime::key_store() {
        Ok(key_store) => key_store,
        Err(error) => {
            steps.push(step(
                "approve",
                BootstrapStepState::Blocked,
                format!("Local secret store unavailable: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        }
    };
    let workspace_id = runtime::active_workspace_id();
    let approving_device_id = runtime::daemon_device_id(&workspace_id);
    run_after_install(
        &runner,
        args,
        generated_at,
        steps,
        install,
        &*control_plane,
        &*key_store,
        workspace_id,
        approving_device_id,
        remote_bootstrap_secret_env(),
    )
}

fn normalize_remote_root(mut args: BootstrapSshArgs) -> BootstrapSshArgs {
    if let Ok(home) = env::var("HOME") {
        args.root = normalize_remote_root_for_home(&args.root, &home);
    }
    args
}

fn normalize_remote_root_for_home(root: &str, home: &str) -> String {
    if root == home {
        return "~".to_string();
    }
    root.strip_prefix(&format!("{home}/"))
        .map(|rest| format!("~/{rest}"))
        .unwrap_or_else(|| root.to_string())
}

#[allow(clippy::too_many_arguments)]
fn run_after_install<R>(
    runner: &R,
    args: BootstrapSshArgs,
    generated_at: String,
    mut steps: Vec<BootstrapStep>,
    install: RemoteBowlineInstall,
    control_plane: &dyn ControlPlaneClient,
    key_store: &dyn DeviceKeyStore,
    workspace_id: bowline_core::ids::WorkspaceId,
    device_id: DeviceId,
    remote_secret_env: Vec<(String, String)>,
) -> BootstrapSshCommandOutput
where
    R: ProcessRunner,
{
    let bootstrap_session = match control_plane.create_bootstrap_session(BootstrapSessionInput {
        workspace_id: workspace_id.as_str().to_string(),
        host: Some(args.host.clone()),
        root: Some(args.root.clone()),
        expires_in_ticks: 600,
    }) {
        Ok(session) => {
            steps.push(step(
                "authorize-bootstrap",
                BootstrapStepState::Completed,
                "Created a short-lived remote bootstrap session.",
            ));
            session
        }
        Err(error) => {
            steps.push(step(
                "authorize-bootstrap",
                BootstrapStepState::Blocked,
                format!("Could not create remote bootstrap session: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        }
    };
    let mut options = BootstrapSshOptions {
        host: args.host.clone(),
        root: args.root.clone(),
        remote_binary: Some(install.remote_binary),
        remote_workspace_id: Some(workspace_id.as_str().to_string()),
        remote_env: remote_bootstrap_env(&args.host),
        remote_secret_env,
        bootstrap_token: Some(bootstrap_session.token),
    };
    if remote_bootstrap_auth_error(&options.remote_secret_env) {
        steps.push(step(
            "remote-auth",
            BootstrapStepState::Blocked,
            "Remote bootstrap needs an bowline account session for durable daemon auth; refusing to create a short-lived WorkOS-only remote.",
        ));
        return bootstrap_output(
            output_base(&args, &generated_at, steps),
            None,
            None,
            false,
            None,
        );
    }

    let mut existing_remote_device =
        existing_trusted_remote_device(runner, &options, &workspace_id);
    if existing_remote_device.is_some()
        && !remote_workspace_key_available(runner, &options, &workspace_id)
    {
        set_remote_device_id(
            &mut options,
            remote_rebootstrap_device_id(&args.host, &generated_at),
        );
        existing_remote_device = None;
    }
    let (remote_request, verified_remote_device) = if let Some(device) = existing_remote_device {
        steps.push(step(
            "request",
            BootstrapStepState::Completed,
            format!("Remote device {} is already trusted.", device.name),
        ));
        steps.push(step(
            "trust",
            BootstrapStepState::Completed,
            format!("Remote device {} is trusted.", device.name),
        ));
        (None, device)
    } else {
        let request_probe = match ssh::probe_remote(runner, &options) {
            Ok(probe) => {
                steps.push(step(
                    "request",
                    BootstrapStepState::Completed,
                    "Remote device approval request created.",
                ));
                probe
            }
            Err(error) => {
                steps.push(step(
                    "request",
                    BootstrapStepState::Blocked,
                    format!("Remote request failed: {error}"),
                ));
                return bootstrap_output(
                    output_base(&args, &generated_at, steps),
                    None,
                    None,
                    false,
                    None,
                );
            }
        };

        let remote_devices: DevicesCommandOutput = match serde_json::from_str(&request_probe.stdout)
        {
            Ok(output) => output,
            Err(error) => {
                steps.push(step(
                    "parse",
                    BootstrapStepState::Blocked,
                    format!("Remote request output was not valid bowline JSON: {error}"),
                ));
                return bootstrap_output(
                    output_base(&args, &generated_at, steps),
                    None,
                    None,
                    false,
                    None,
                );
            }
        };
        let Some(remote_request) = remote_devices.created_request.clone() else {
            steps.push(step(
                "parse",
                BootstrapStepState::Blocked,
                "Remote request output did not include a created request.",
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        };

        let trust = match control_plane.list_device_trust(remote_request.workspace_id.as_str()) {
            Ok(trust) => trust,
            Err(error) => {
                steps.push(step(
                    "control-plane",
                    BootstrapStepState::Blocked,
                    format!("Could not fetch pending request from control plane: {error}"),
                ));
                return bootstrap_output(
                    output_base(&args, &generated_at, steps),
                    Some(remote_request),
                    None,
                    false,
                    None,
                );
            }
        };
        let Some(cloud_request) = trust
            .pending_requests
            .iter()
            .find(|request| request.request_id == remote_request.request_id.as_str())
        else {
            steps.push(step(
                "compare",
                BootstrapStepState::Blocked,
                "Remote request was not present in the control plane.",
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                Some(remote_request.clone()),
                None,
                false,
                None,
            );
        };
        if !request_matches_cloud(&remote_request, cloud_request) {
            steps.push(step(
                "compare",
                BootstrapStepState::Blocked,
                "Remote request did not match the control-plane request.",
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                Some(remote_request.clone()),
                None,
                false,
                None,
            );
        }
        steps.push(step(
            "compare",
            BootstrapStepState::Completed,
            "Remote request matched the control-plane request.",
        ));

        let _approval = match bowline_local::trust::approve_device_request(
            control_plane,
            key_store,
            bowline_local::trust::ApproveDeviceOptions {
                workspace_id: remote_request.workspace_id.clone(),
                request_id: remote_request.request_id.clone(),
                approver_device_id: device_id,
                generated_at: generated_at.clone(),
            },
        ) {
            Ok(output) => {
                steps.push(step(
                    "approve",
                    BootstrapStepState::Completed,
                    "Encrypted device grant uploaded.",
                ));
                output
            }
            Err(error) => {
                steps.push(step(
                    "approve",
                    BootstrapStepState::Blocked,
                    format!("Local approval failed: {error}"),
                ));
                return bootstrap_output(
                    output_base(&args, &generated_at, steps),
                    Some(remote_request),
                    None,
                    false,
                    None,
                );
            }
        };

        match ssh::accept_remote_grant(runner, &options, remote_request.request_id.as_str()) {
            Ok(_) => steps.push(step(
                "accept",
                BootstrapStepState::Completed,
                "Remote device accepted and decrypted the grant.",
            )),
            Err(error) => {
                steps.push(step(
                    "accept",
                    BootstrapStepState::Blocked,
                    format!("Remote grant acceptance failed: {error}"),
                ));
                return bootstrap_output(
                    output_base(&args, &generated_at, steps),
                    Some(remote_request),
                    None,
                    false,
                    None,
                );
            }
        }

        let verified_remote_device =
            match verify_remote_device_trust(control_plane, &remote_request) {
                Ok(device) => {
                    steps.push(step(
                        "trust",
                        BootstrapStepState::Completed,
                        format!("Remote device {} is trusted.", device.name),
                    ));
                    device
                }
                Err(error) => {
                    steps.push(step("trust", BootstrapStepState::Blocked, error));
                    return bootstrap_output(
                        output_base(&args, &generated_at, steps),
                        Some(remote_request),
                        None,
                        false,
                        None,
                    );
                }
            };
        (Some(remote_request), verified_remote_device)
    };

    match ssh::prepare_remote_root(runner, &options) {
        Ok(_) => steps.push(step(
            "prepare-root",
            BootstrapStepState::Completed,
            "Remote real directory root is initialized and accepted.",
        )),
        Err(error) => {
            steps.push(step(
                "prepare-root",
                BootstrapStepState::Blocked,
                format!("Remote root preparation failed: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                remote_request.clone(),
                Some(verified_remote_device),
                true,
                None,
            );
        }
    }

    match ssh::publish_default_metadata(runner, &options) {
        Ok(_) => steps.push(step(
            "metadata-default",
            BootstrapStepState::Completed,
            "Remote bowline commands now use this workspace by default.",
        )),
        Err(error) => {
            steps.push(step(
                "metadata-default",
                BootstrapStepState::Blocked,
                format!("Remote default metadata setup failed: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                remote_request.clone(),
                Some(verified_remote_device),
                true,
                None,
            );
        }
    }

    match ssh::start_remote_daemon(runner, &options) {
        Ok(_) => steps.push(step(
            "daemon-start",
            BootstrapStepState::Completed,
            "Remote daemon start requested for the accepted root.",
        )),
        Err(error) => {
            steps.push(step(
                "daemon-start",
                BootstrapStepState::Blocked,
                format!("Remote daemon start failed: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                remote_request.clone(),
                Some(verified_remote_device),
                true,
                None,
            );
        }
    }

    let daemon_probe = match wait_for_remote_daemon(runner, &options) {
        Ok(probe) if remote_daemon_is_running(&probe.stdout) => {
            steps.push(step(
                "daemon-status",
                BootstrapStepState::Completed,
                "Remote daemon is running.",
            ));
            probe
        }
        Ok(probe) => {
            steps.push(step(
                "daemon-status",
                BootstrapStepState::Blocked,
                remote_daemon_status_summary(&probe.stdout),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                remote_request.clone(),
                Some(verified_remote_device),
                true,
                None,
            );
        }
        Err(error) => {
            steps.push(step(
                "daemon-status",
                BootstrapStepState::Blocked,
                format!("Remote daemon status failed: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                remote_request.clone(),
                Some(verified_remote_device),
                true,
                None,
            );
        }
    };

    let (remote_status, sync_ready) = if remote_daemon_sync_is_ready(&daemon_probe.stdout) {
        steps.push(step(
            "sync",
            BootstrapStepState::Completed,
            "Remote daemon has completed sync for this real directory root.",
        ));
        (Some(WorkspaceStatus::healthy()), true)
    } else {
        match ssh::status_remote(runner, &options) {
            Ok(probe) => match serde_json::from_str::<StatusCommandOutput>(&probe.stdout) {
                Ok(output) => {
                    let sync_ready = remote_sync_is_ready(&output.status);
                    steps.push(step(
                        "sync",
                        if sync_ready {
                            BootstrapStepState::Completed
                        } else {
                            BootstrapStepState::Blocked
                        },
                        if sync_ready {
                            "Sync is ready for this real directory root.".to_string()
                        } else {
                            remote_status_attention_summary(&output.status)
                        },
                    ));
                    (Some(output.status), sync_ready)
                }
                Err(error) => {
                    let status = WorkspaceStatus {
                        level: StatusLevel::Limited,
                        attention_items: vec![format!(
                            "Remote status output was not valid bowline JSON: {error}"
                        )],
                    };
                    steps.push(step(
                        "sync",
                        BootstrapStepState::Blocked,
                        status.attention_items[0].clone(),
                    ));
                    (Some(status), false)
                }
            },
            Err(error) => {
                let status = WorkspaceStatus {
                    level: StatusLevel::Limited,
                    attention_items: vec![format!("Remote status check failed: {error}")],
                };
                steps.push(step(
                    "sync",
                    BootstrapStepState::Blocked,
                    status.attention_items[0].clone(),
                ));
                (Some(status), false)
            }
        }
    };

    let agent_handoff = if sync_ready {
        create_agent_handoff_if_requested(runner, &options, &args, &mut steps)
    } else {
        requested_agent_handoff(&args)
    };
    let mut base = output_base(&args, &generated_at, steps);
    base.agent_handoff = agent_handoff;

    bootstrap_output(
        base,
        remote_request,
        Some(verified_remote_device),
        true,
        remote_status,
    )
}

fn wait_for_remote_daemon<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<ssh::RemoteBootstrapProbe, ssh::BootstrapSshError>
where
    R: ProcessRunner,
{
    let mut last_probe = None;
    for attempt in 0..20 {
        let probe = ssh::daemon_status_remote(runner, options)?;
        if remote_daemon_sync_is_ready(&probe.stdout) {
            return Ok(probe);
        }
        last_probe = Some(probe);
        if attempt < 19 {
            thread::sleep(Duration::from_millis(250));
        }
    }
    Ok(last_probe.expect("readiness loop always performs at least one probe"))
}

fn existing_trusted_remote_device<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> Option<DeviceRecord>
where
    R: ProcessRunner,
{
    let mut options = options.clone();
    options.bootstrap_token = None;
    let probe = ssh::list_remote_devices(runner, &options).ok()?;
    let output = serde_json::from_str::<DevicesCommandOutput>(&probe.stdout).ok()?;
    output.devices.into_iter().find(|device| {
        device.is_current_device
            && device.trust_state == DeviceTrustState::Trusted
            && device.workspace_id.as_str() == workspace_id.as_str()
    })
}

fn remote_workspace_key_available<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> bool
where
    R: ProcessRunner,
{
    ssh::server_local_workspace_key_available(runner, options, workspace_id.as_str())
        .unwrap_or(false)
}

fn set_remote_device_id(options: &mut BootstrapSshOptions, device_id: String) {
    if let Some((_, value)) = options
        .remote_env
        .iter_mut()
        .find(|(key, _)| key == "BOWLINE_DEVICE_ID")
    {
        *value = device_id;
        return;
    }
    options
        .remote_env
        .push(("BOWLINE_DEVICE_ID".to_string(), device_id));
}

fn request_matches_cloud(
    remote: &DeviceApprovalRequest,
    cloud: &bowline_control_plane::DeviceRequest,
) -> bool {
    remote.request_id.as_str() == cloud.request_id
        && remote.workspace_id.as_str() == cloud.workspace_id
        && remote.requester_device_id.as_str() == cloud.device_id
        && remote.device_public_key.as_str() == cloud.device_public_key
        && remote.device_fingerprint.as_str() == cloud.device_fingerprint
        && remote.matching_code == cloud.matching_code
}

fn remote_bootstrap_env(host: &str) -> Vec<(String, String)> {
    let mut values = Vec::new();
    if let Some(convex_url) = runtime::hosted_convex_url() {
        values.push(("CONVEX_URL".to_string(), convex_url));
    }
    values.push((
        "BOWLINE_WORKOS_CLIENT_ID".to_string(),
        runtime::hosted_workos_client_id(),
    ));
    values.push((
        "BOWLINE_WORKSPACE_ID".to_string(),
        runtime::active_workspace_id().as_str().to_string(),
    ));
    values.push(("BOWLINE_DEVICE_ID".to_string(), remote_device_id(host)));
    values.push(("BOWLINE_DEVICE_NAME".to_string(), remote_device_name(host)));
    values.push((
        "BOWLINE_SECRET_STORE".to_string(),
        "server-local".to_string(),
    ));
    values
}

fn remote_device_id(host: &str) -> String {
    format!("device_{}", sanitize_remote_device_id_part(host))
}

fn remote_rebootstrap_device_id(host: &str, seed: &str) -> String {
    let suffix = blake3::hash(seed.as_bytes()).to_hex()[..8].to_string();
    format!("device_{}_{}", sanitize_remote_device_id_part(host), suffix)
}

fn sanitize_remote_device_id_part(value: &str) -> String {
    let mut id = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            id.push(character.to_ascii_lowercase());
        } else {
            id.push('_');
        }
    }
    while id.contains("__") {
        id = id.replace("__", "_");
    }
    id.trim_matches('_').to_string()
}

fn remote_device_name(host: &str) -> String {
    format!("bowline-remote-{}", sanitize_remote_device_id_part(host))
}

fn remote_bootstrap_secret_env() -> Vec<(String, String)> {
    let account_session_id = runtime::key_store().ok().and_then(|store| {
        let _ =
            runtime::ensure_durable_account_session(&*store, Some(&runtime::active_workspace_id()));
        runtime::account_session_id(&*store)
    });
    let control_plane_token = env::var("BOWLINE_CONTROL_PLANE_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    remote_bootstrap_secret_env_from(account_session_id, control_plane_token)
}

fn remote_bootstrap_secret_env_from(
    account_session_id: Option<String>,
    control_plane_token: Option<String>,
) -> Vec<(String, String)> {
    let mut values = Vec::new();
    if let Some(session_id) = account_session_id {
        values.push(("BOWLINE_ACCOUNT_SESSION_ID".to_string(), session_id));
    }
    if let Some(token) = control_plane_token {
        values.push(("BOWLINE_CONTROL_PLANE_TOKEN".to_string(), token));
    }
    if values.is_empty() {
        values.push((
            "BOWLINE_REMOTE_AUTH_ERROR".to_string(),
            "missing-durable-account-session".to_string(),
        ));
    }
    values
}

fn remote_bootstrap_auth_error(values: &[(String, String)]) -> bool {
    values
        .iter()
        .any(|(key, value)| key == "BOWLINE_REMOTE_AUTH_ERROR" && !value.is_empty())
}

fn verify_remote_device_trust(
    control_plane: &dyn bowline_control_plane::ControlPlaneClient,
    remote_request: &DeviceApprovalRequest,
) -> Result<DeviceRecord, String> {
    let trust = control_plane
        .list_device_trust(remote_request.workspace_id.as_str())
        .map_err(|error| format!("Could not verify remote device trust: {error}"))?;
    let Some(device) = trust.authorized_devices.into_iter().find(|device| {
        device.device_id == remote_request.requester_device_id.as_str()
            && device.device_fingerprint == remote_request.device_fingerprint.as_str()
    }) else {
        return Err(format!(
            "Remote device {} accepted its grant but is not authorized in the control plane.",
            remote_request.device_name
        ));
    };

    Ok(DeviceRecord {
        id: DeviceId::new(device.device_id),
        name: device.device_name,
        workspace_id: remote_request.workspace_id.clone(),
        platform: platform_from_str(&device.platform),
        trust_state: DeviceTrustState::Trusted,
        device_fingerprint: DeviceFingerprint::new(device.device_fingerprint),
        authorized_at: Some(device.authorized_at.to_string()),
        updated_at: device.authorized_at.to_string(),
        is_current_device: false,
        limitation_reason: None,
    })
}

fn remote_sync_is_ready(status: &WorkspaceStatus) -> bool {
    !status.needs_attention()
}

fn remote_daemon_is_running(stdout: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(stdout).is_ok_and(|value| {
        value
            .pointer("/daemon/state")
            .and_then(|state| state.as_str())
            == Some("running")
    })
}

fn remote_daemon_sync_is_ready(stdout: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(stdout).is_ok_and(|value| {
        let sync = &value["sync"];
        let local_head = &sync["localHead"];
        let remote_head = &sync["remoteHead"];
        sync["state"].as_str() == Some("idle")
            && local_head["workspaceId"].as_str().is_some()
            && local_head["snapshotId"].as_str().is_some()
            && local_head["version"].as_u64().is_some()
            && local_head["workspaceId"] == remote_head["workspaceId"]
            && local_head["snapshotId"] == remote_head["snapshotId"]
            && local_head["version"] == remote_head["version"]
    })
}

fn remote_daemon_status_summary(stdout: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(stdout) {
        Ok(value) => {
            let state = value
                .pointer("/daemon/state")
                .and_then(|state| state.as_str())
                .unwrap_or("unknown");
            format!("Remote daemon is {state}, not running.")
        }
        Err(error) => format!("Remote daemon status output was not valid bowline JSON: {error}"),
    }
}

fn platform_from_str(value: &str) -> bowline_core::devices::DevicePlatform {
    match value {
        "macos" | "darwin" => bowline_core::devices::DevicePlatform::Macos,
        "linux" => bowline_core::devices::DevicePlatform::Linux,
        _ => bowline_core::devices::DevicePlatform::Unknown,
    }
}

fn remote_status_attention_summary(status: &WorkspaceStatus) -> String {
    status
        .attention_items
        .first()
        .cloned()
        .unwrap_or_else(|| "Remote status is not healthy.".to_string())
}

fn output_base(
    args: &BootstrapSshArgs,
    generated_at: &str,
    steps: Vec<BootstrapStep>,
) -> BootstrapOutputBase {
    BootstrapOutputBase {
        host: args.host.clone(),
        root: args.root.clone(),
        generated_at: generated_at.to_string(),
        steps,
        agent_handoff: requested_agent_handoff(args),
    }
}

fn requested_agent_handoff(args: &BootstrapSshArgs) -> Option<BootstrapAgentHandoff> {
    Some(BootstrapAgentHandoff {
        project: args.project.clone()?,
        task: args.task.clone()?,
        agent: args.agent.clone(),
        lease_id: None,
        write_target_mode: None,
        write_target_path: None,
        work_view_id: None,
        work_view_path: None,
        launched: false,
        accepted: false,
    })
}

fn create_agent_handoff_if_requested<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    args: &BootstrapSshArgs,
    steps: &mut Vec<BootstrapStep>,
) -> Option<BootstrapAgentHandoff>
where
    R: ProcessRunner,
{
    let mut handoff = requested_agent_handoff(args)?;
    match ssh::create_remote_agent_lease(runner, options, &handoff.project, &handoff.task) {
        Ok(probe) => match extract_agent_handoff(&probe.stdout) {
            Ok(remote_lease) => {
                handoff.lease_id = Some(remote_lease.lease_id.clone());
                handoff.write_target_mode = Some(remote_lease.write_target_mode);
                handoff.write_target_path = Some(remote_lease.write_target_path.clone());
                handoff.work_view_id = remote_lease.work_view_id.clone();
                handoff.work_view_path = remote_lease.work_view_path.clone();
                steps.push(step(
                    "agent-lease",
                    BootstrapStepState::Completed,
                    format!("Started remote agent work {}.", remote_lease.lease_id),
                ));
                if handoff.agent.as_deref() == Some("codex") {
                    run_requested_remote_codex(runner, options, &mut handoff, steps);
                } else if let Some(agent) = handoff.agent.as_deref() {
                    steps.push(step(
                        "agent-run",
                        BootstrapStepState::Blocked,
                        format!("Remote agent `{agent}` is not launchable by bootstrap yet."),
                    ));
                }
                Some(handoff)
            }
            Err(error) => {
                steps.push(step(
                    "agent-lease",
                    BootstrapStepState::Blocked,
                    format!("Remote agent start output was not valid bowline JSON: {error}"),
                ));
                Some(handoff)
            }
        },
        Err(error) => {
            steps.push(step(
                "agent-lease",
                BootstrapStepState::Blocked,
                format!("Remote agent start failed: {error}"),
            ));
            Some(handoff)
        }
    }
}

fn run_requested_remote_codex<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    handoff: &mut BootstrapAgentHandoff,
    steps: &mut Vec<BootstrapStep>,
) where
    R: ProcessRunner,
{
    let Some(lease_id) = handoff.lease_id.as_deref() else {
        return;
    };
    let Some(write_target_mode) = handoff.write_target_mode else {
        return;
    };
    let Some(write_target_path) = handoff.write_target_path.as_deref() else {
        return;
    };
    match ssh::launch_remote_codex_agent(runner, options, lease_id, write_target_path) {
        Ok(_) => steps.push(step(
            "agent-run",
            BootstrapStepState::Completed,
            format!("Codex finished remote lease {lease_id}."),
        )),
        Err(error) => {
            steps.push(step(
                "agent-run",
                BootstrapStepState::Blocked,
                format!("Codex launch failed for remote lease {lease_id}: {error}"),
            ));
            return;
        }
    }
    handoff.launched = true;

    if write_target_mode == AgentWriteTargetMode::Direct {
        match ssh::complete_remote_agent_lease(runner, options, lease_id) {
            Ok(probe) => match completed_direct_lease_summary(&probe.stdout, lease_id) {
                Ok(summary) => {
                    steps.push(step(
                        "agent-complete",
                        BootstrapStepState::Completed,
                        summary,
                    ));
                    handoff.accepted = true;
                }
                Err(summary) => {
                    steps.push(step("agent-complete", BootstrapStepState::Blocked, summary))
                }
            },
            Err(error) => steps.push(step(
                "agent-complete",
                BootstrapStepState::Blocked,
                format!("Remote agent complete failed for {lease_id}: {error}"),
            )),
        }
        return;
    }

    let Some(work_view_id) = handoff.work_view_id.as_deref() else {
        steps.push(step(
            "agent-accept",
            BootstrapStepState::Blocked,
            format!("Remote work-view lease {lease_id} did not include a work view id."),
        ));
        return;
    };
    match ssh::accept_remote_work_view(runner, options, work_view_id) {
        Ok(probe) => match accepted_work_view_summary(&probe.stdout) {
            Ok(summary) => {
                steps.push(step("agent-accept", BootstrapStepState::Completed, summary));
                handoff.accepted = true;
            }
            Err(summary) => steps.push(step("agent-accept", BootstrapStepState::Blocked, summary)),
        },
        Err(error) => steps.push(step(
            "agent-accept",
            BootstrapStepState::Blocked,
            format!("Remote work-view accept failed for {work_view_id}: {error}"),
        )),
    }
}

fn completed_direct_lease_summary(stdout: &str, lease_id: &str) -> Result<String, String> {
    let output = serde_json::from_str::<serde_json::Value>(stdout).map_err(|error| {
        format!("Remote agent complete output was not valid bowline JSON: {error}")
    })?;
    match output
        .pointer("/outcome")
        .and_then(|outcome| outcome.as_str())
    {
        Some("allowed") => Ok(format!(
            "Completed direct remote lease {lease_id}; edits remain in the real project path."
        )),
        Some("denied")
            if output
                .pointer("/denial/code")
                .and_then(|code| code.as_str())
                == Some("lease-not-active") =>
        {
            Ok(format!(
                "Direct remote lease {lease_id} was already completed."
            ))
        }
        Some(outcome) => Err(format!(
            "Remote agent complete returned unexpected outcome {outcome}."
        )),
        None => Err("Remote agent complete output did not include an outcome.".to_string()),
    }
}

fn accepted_work_view_summary(stdout: &str) -> Result<String, String> {
    let output = serde_json::from_str::<serde_json::Value>(stdout).map_err(|error| {
        format!("Remote work-view accept output was not valid bowline JSON: {error}")
    })?;
    let work_view_id = output
        .pointer("/workView/id")
        .and_then(|id| id.as_str())
        .unwrap_or("unknown");
    match output.pointer("/action").and_then(|action| action.as_str()) {
        Some("accepted") => Ok(format!(
            "Accepted remote work view {work_view_id} into the real project."
        )),
        Some("review-ready") => Err(output
            .pointer("/status/attentionItems/0")
            .and_then(|item| item.as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                format!("Remote work view {work_view_id} still needs review before accepting.")
            })),
        Some(action) => Err(format!(
            "Remote work-view accept returned unexpected action {action}."
        )),
        None => Err("Remote work-view accept output did not include an action.".to_string()),
    }
}

fn extract_agent_handoff(stdout: &str) -> Result<RemoteAgentHandoffLease, String> {
    let value =
        serde_json::from_str::<serde_json::Value>(stdout).map_err(|error| error.to_string())?;
    let lease_id = value
        .pointer("/lease/id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "missing lease.id".to_string())?
        .to_string();
    let work_view_id = value
        .pointer("/lease/workViewId")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);
    let write_target_mode = match value
        .pointer("/lease/writeTargetMode")
        .and_then(|mode| mode.as_str())
    {
        Some("direct") => AgentWriteTargetMode::Direct,
        Some("work-view") => AgentWriteTargetMode::WorkView,
        Some(mode) => return Err(format!("unsupported lease.writeTargetMode {mode}")),
        None if work_view_id.is_some() => AgentWriteTargetMode::WorkView,
        None => AgentWriteTargetMode::Direct,
    };
    let work_view_path = value
        .pointer("/lease/workViewPath")
        .and_then(|path| path.as_str())
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned);
    let write_target_path = value
        .pointer("/lease/writeTargetPath")
        .and_then(|path| path.as_str())
        .filter(|path| !path.is_empty())
        .or_else(|| {
            value
                .pointer("/lease/outputTarget/path")
                .and_then(|path| path.as_str())
                .filter(|path| !path.is_empty())
        })
        .or(work_view_path.as_deref())
        .ok_or_else(|| "missing lease.writeTargetPath".to_string())?
        .to_string();
    if write_target_mode == AgentWriteTargetMode::WorkView && work_view_id.is_none() {
        return Err("missing lease.workViewId for work-view lease".to_string());
    }
    Ok(RemoteAgentHandoffLease {
        lease_id,
        write_target_mode,
        write_target_path,
        work_view_id,
        work_view_path,
    })
}

fn bootstrap_output(
    base: BootstrapOutputBase,
    device_request: Option<DeviceApprovalRequest>,
    authorized_device: Option<bowline_core::devices::DeviceRecord>,
    trusted: bool,
    remote_status: Option<WorkspaceStatus>,
) -> BootstrapSshCommandOutput {
    let has_blocked_step = base
        .steps
        .iter()
        .any(|step| step.state == BootstrapStepState::Blocked);
    let remote_status = remote_status.unwrap_or_else(|| {
        if trusted && !has_blocked_step {
            WorkspaceStatus::healthy()
        } else {
            WorkspaceStatus {
                level: if trusted {
                    StatusLevel::Attention
                } else {
                    StatusLevel::Limited
                },
                attention_items: vec![if trusted {
                    "Remote device is trusted, but bootstrap did not finish preparing sync."
                        .to_string()
                } else {
                    "Remote bootstrap did not complete.".to_string()
                }],
            }
        }
    });
    let sync = if has_blocked_step {
        BootstrapSyncState::Blocked
    } else if remote_sync_is_ready(&remote_status) {
        BootstrapSyncState::Ready
    } else {
        BootstrapSyncState::Blocked
    };
    let next_actions = bootstrap_next_actions(&base, trusted, sync, &remote_status);

    BootstrapSshCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Connect,
        generated_at: base.generated_at,
        workspace_id: device_request
            .as_ref()
            .map(|request| request.workspace_id.clone()),
        project_id: None,
        host: base.host,
        root: base.root,
        steps: base.steps,
        remote_device_fingerprint: device_request
            .as_ref()
            .map(|request| request.device_fingerprint.clone()),
        device_request,
        authorized_device,
        trusted,
        secret_store: BootstrapSecretStore::ServerLocal,
        sync,
        next_required_phase: None,
        remote_status,
        next_actions,
    }
}

fn bootstrap_next_actions(
    base: &BootstrapOutputBase,
    trusted: bool,
    sync: BootstrapSyncState,
    remote_status: &WorkspaceStatus,
) -> Vec<SafeAction> {
    let mut actions = Vec::new();
    let root = remote_path_arg(&base.root);
    let remote_status_command = ssh_command(&base.host, &format!("bowline status {root} --json"));

    if trusted {
        actions.push(SafeAction {
            label: "Inspect remote status".to_string(),
            command: Some(remote_status_command.clone()),
        });
        actions.push(SafeAction {
            label: "Inspect remote next actions".to_string(),
            command: Some(remote_status_command.clone()),
        });
    }

    match sync {
        BootstrapSyncState::Ready => {
            if let Some(handoff) = &base.agent_handoff {
                actions.extend(agent_handoff_actions(base, handoff));
            } else {
                actions.push(SafeAction {
                    label: "Start agent work in a project".to_string(),
                    command: Some(ssh_command(
                        &base.host,
                        &format!(
                            "cd {root}/<project> && bowline agent start . --task '<task>' --base latest-workspace --hydrate-budget 512MiB --json"
                        ),
                    )),
                });
            }
        }
        BootstrapSyncState::Prepared => {
            actions.push(SafeAction {
                label: "Start the remote daemon".to_string(),
                command: Some(ssh_command(&base.host, "bowline daemon start --json")),
            });
        }
        BootstrapSyncState::Blocked => {
            if let Some(blocked) = base
                .steps
                .iter()
                .rev()
                .find(|step| step.state == BootstrapStepState::Blocked)
            {
                actions.extend(blocked_step_actions(
                    base,
                    blocked.name.as_str(),
                    remote_status,
                ));
            } else if remote_status.needs_attention() {
                actions.push(SafeAction {
                    label: "Inspect remote status".to_string(),
                    command: Some(remote_status_command),
                });
            }
        }
    }

    dedupe_actions(actions)
}

fn blocked_step_actions(
    base: &BootstrapOutputBase,
    blocked_step: &str,
    remote_status: &WorkspaceStatus,
) -> Vec<SafeAction> {
    let root = remote_path_arg(&base.root);
    let retry = SafeAction {
        label: "Retry remote bootstrap".to_string(),
        command: Some(format!(
            "bowline connect {} --root {} --json",
            shell_quote(&base.host),
            shell_quote(&base.root)
        )),
    };
    match blocked_step {
        "install" | "authorize-bootstrap" | "control-plane" => vec![retry],
        "request" | "parse" | "compare" | "accept" => vec![
            SafeAction {
                label: "Inspect remote device requests".to_string(),
                command: Some(ssh_command(&base.host, "bowline status --json")),
            },
            retry,
        ],
        "approve" => vec![
            SafeAction {
                label: "Inspect local device requests".to_string(),
                command: Some("bowline status --json".to_string()),
            },
            retry,
        ],
        "trust" => vec![
            SafeAction {
                label: "Verify local device trust".to_string(),
                command: Some("bowline status --json".to_string()),
            },
            SafeAction {
                label: "Verify remote device trust".to_string(),
                command: Some(ssh_command(&base.host, "bowline status --json")),
            },
            retry,
        ],
        "prepare-root" => vec![
            SafeAction {
                label: "Log in on remote root".to_string(),
                command: Some(ssh_command(
                    &base.host,
                    &format!("bowline login --root {root} --no-poll --json"),
                )),
            },
            retry,
        ],
        "daemon-start" | "daemon-status" => vec![
            SafeAction {
                label: "Start remote daemon".to_string(),
                command: Some(ssh_command(&base.host, "bowline daemon start --json")),
            },
            SafeAction {
                label: "Inspect remote daemon status".to_string(),
                command: Some(ssh_command(&base.host, "bowline daemon status --json")),
            },
            retry,
        ],
        "sync" => vec![
            SafeAction {
                label: "Inspect remote daemon status".to_string(),
                command: Some(ssh_command(&base.host, "bowline daemon status --json")),
            },
            SafeAction {
                label: "Inspect remote status".to_string(),
                command: Some(ssh_command(
                    &base.host,
                    &format!("bowline status {root} --json"),
                )),
            },
            retry,
        ],
        "agent-lease" | "agent-run" | "agent-complete" | "agent-accept" => base
            .agent_handoff
            .as_ref()
            .map(|handoff| {
                let mut actions = Vec::new();
                if remote_status_mentions_conflict(remote_status) {
                    actions.push(SafeAction {
                        label: "Resolve remote conflicts".to_string(),
                        command: Some(ssh_command(
                            &base.host,
                            &format!("bowline resolve {} --json", remote_path_arg(&base.root)),
                        )),
                    });
                }
                actions.push(agent_lease_create_action(base, handoff));
                actions.push(retry.clone());
                actions
            })
            .unwrap_or_else(|| vec![retry]),
        _ => vec![retry],
    }
}

fn remote_status_mentions_conflict(status: &WorkspaceStatus) -> bool {
    status
        .attention_items
        .iter()
        .any(|item| item.to_ascii_lowercase().contains("conflict"))
}

fn agent_handoff_actions(
    base: &BootstrapOutputBase,
    handoff: &BootstrapAgentHandoff,
) -> Vec<SafeAction> {
    if handoff.accepted {
        return Vec::new();
    }
    let Some(lease_id) = handoff.lease_id.as_deref() else {
        return vec![agent_lease_create_action(base, handoff)];
    };
    let mut actions = Vec::new();
    if let Some(path) = handoff.write_target_path.as_deref() {
        actions.push(SafeAction {
            label: match handoff.write_target_mode {
                Some(AgentWriteTargetMode::Direct) => "Open remote agent project".to_string(),
                Some(AgentWriteTargetMode::WorkView) => "Open remote agent work view".to_string(),
                None => "Open remote agent target".to_string(),
            },
            command: Some(ssh_command(
                &base.host,
                &format!("cd {}", remote_path_arg(path)),
            )),
        });
    }
    actions.push(SafeAction {
        label: "Inspect remote agent context".to_string(),
        command: Some(ssh_command(
            &base.host,
            &format!(
                "bowline agent context --lease {} --json",
                shell_quote(lease_id)
            ),
        )),
    });
    if !handoff.launched
        && handoff.agent.as_deref() == Some("codex")
        && let Some(path) = handoff.write_target_path.as_deref()
    {
        actions.push(SafeAction {
            label: match handoff.write_target_mode {
                Some(AgentWriteTargetMode::Direct) => "Launch Codex on remote project".to_string(),
                Some(AgentWriteTargetMode::WorkView) => {
                    "Launch Codex on remote work view".to_string()
                }
                None => "Launch Codex on remote target".to_string(),
            },
            command: Some(ssh_command(
                &base.host,
                &format!(
                    "export PATH=\"$HOME/.local/bin:$PATH\"; ~/.local/bin/bowline agent prompt --lease {} | codex exec --cd {} --sandbox workspace-write --add-dir ~/.local/share/bowline --add-dir ~/.local/state/bowline --add-dir ~/.local/state/bowline --add-dir \"$HOME/Library/Application Support/bowline\" --skip-git-repo-check -",
                    shell_quote(lease_id),
                    remote_path_arg(path),
                ),
            )),
        });
    }
    actions.push(SafeAction {
        label: match handoff.agent.as_deref() {
            Some(agent) => format!("Copy prompt for {agent}"),
            None => "Copy remote agent prompt".to_string(),
        },
        command: Some(ssh_command(
            &base.host,
            &format!("bowline agent prompt --lease {}", shell_quote(lease_id)),
        )),
    });
    actions
}

fn agent_lease_create_action(
    base: &BootstrapOutputBase,
    handoff: &BootstrapAgentHandoff,
) -> SafeAction {
    let root = remote_path_arg(&base.root);
    let project = remote_path_arg(&handoff.project);
    SafeAction {
        label: "Start remote agent work".to_string(),
        command: Some(ssh_command(
            &base.host,
            &format!(
                "cd {root} && bowline agent start {project} --task {} --base latest-workspace --hydrate-budget 512MiB --json",
                shell_quote(&handoff.task),
            ),
        )),
    }
}

fn ssh_command(host: &str, remote_command: &str) -> String {
    format!(
        "ssh {} {}",
        shell_quote(host),
        shell_quote(&format!("bash -lc {}", shell_quote(remote_command)))
    )
}

fn remote_path_arg(path: &str) -> String {
    if path == "~" {
        return "~".to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        if shell_safe_path(rest) {
            return format!("~/{rest}");
        }
        return format!("~/{}", shell_quote(rest));
    }
    if shell_safe_path(path) {
        return path.to_string();
    }
    shell_quote(path)
}

fn shell_safe_path(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '.' | '_' | '-' | ':' | '@' | '+')
        })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

fn dedupe_actions(actions: Vec<SafeAction>) -> Vec<SafeAction> {
    let mut deduped = Vec::new();
    for action in actions {
        let already_present = deduped.iter().any(|existing: &SafeAction| {
            existing.label == action.label && existing.command == action.command
        });
        if !already_present {
            deduped.push(action);
        }
    }
    deduped
}

fn step(
    name: impl Into<String>,
    state: BootstrapStepState,
    summary: impl Into<String>,
) -> BootstrapStep {
    BootstrapStep {
        name: name.into(),
        state,
        summary: summary.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    use bowline_control_plane::FakeControlPlaneClient;
    use bowline_core::{
        commands::{CONTRACT_VERSION, DeviceCommandAction, DevicesCommandOutput},
        devices::{DevicePlatform, RecoveryKeyState},
        ids::{DeviceApprovalRequestId, WorkspaceId},
    };
    use bowline_local::{
        bootstrap::{
            install::{RemoteBowlineInstall, RemotePlatform},
            process::{ProcessError, ProcessOutput, ProcessRunner},
        },
        fakes::FakeKeychain,
    };

    #[derive(Clone)]
    struct FakeBootstrapRunner {
        control_plane: FakeControlPlaneClient,
        remote_keychain: FakeKeychain,
        workspace_id: WorkspaceId,
        request_id: Rc<RefCell<Option<DeviceApprovalRequestId>>>,
    }

    impl ProcessRunner for FakeBootstrapRunner {
        fn run(&self, _program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
            self.run_with_stdin(_program, args, "")
        }

        fn run_with_stdin(
            &self,
            _program: &str,
            args: &[String],
            _stdin: &str,
        ) -> Result<ProcessOutput, ProcessError> {
            let command = args.last().cloned().unwrap_or_default();
            if command.contains("devices request") && command.contains("--json") {
                let request = bowline_local::trust::create_device_request(
                    &self.control_plane,
                    &self.remote_keychain,
                    bowline_local::trust::DeviceRequestOptions {
                        workspace_id: self.workspace_id.clone(),
                        device_id: DeviceId::new("remote-linux"),
                        device_name: "Remote Linux".to_string(),
                        platform: DevicePlatform::Linux,
                        host: Some("linux-box".to_string()),
                        root: Some("~/Code".to_string()),
                        generated_at: "2026-06-26T12:00:00Z".to_string(),
                    },
                )
                .expect("remote request");
                *self.request_id.borrow_mut() = Some(request.request_id.clone());
                return Ok(json_output(&DevicesCommandOutput {
                    contract_version: CONTRACT_VERSION,
                    command: bowline_core::commands::CommandName::Devices,
                    generated_at: "2026-06-26T12:00:00Z".to_string(),
                    action: DeviceCommandAction::Request,
                    workspace_id: Some(self.workspace_id.clone()),
                    local_device: None,
                    devices: Vec::new(),
                    revoked_devices: Vec::new(),
                    pending_requests: vec![request.clone()],
                    created_request: Some(request),
                    approved_device: None,
                    denied_request: None,
                    revoked_device: None,
                    recovery_key: Some(RecoveryKeyState::missing()),
                    next_actions: Vec::new(),
                }));
            }
            if command.contains("devices accept") {
                let request_id = self
                    .request_id
                    .borrow()
                    .clone()
                    .expect("request id exists before accept");
                bowline_local::trust::accept_device_grant(
                    &self.control_plane,
                    &self.remote_keychain,
                    &self.workspace_id,
                    &request_id,
                    &DeviceId::new("remote-linux"),
                )
                .expect("remote accepts grant");
                return Ok(json_output(&serde_json::json!({"ok": true})));
            }
            if command.contains("daemon status --json") {
                return Ok(json_output(&serde_json::json!({
                    "daemon": {"state": "running"},
                    "sync": {
                        "state": "idle",
                        "lastOutcome": "no-changes",
                        "localHead": {
                            "workspaceId": self.workspace_id.as_str(),
                            "snapshotId": "snap-ready",
                            "version": 1
                        },
                        "remoteHead": {
                            "workspaceId": self.workspace_id.as_str(),
                            "snapshotId": "snap-ready",
                            "version": 1
                        }
                    }
                })));
            }
            if command.contains("init ")
                || command.contains("daemon start --json")
                || command.contains("ln -sfn")
                || command.contains("daemon.env")
            {
                return Ok(json_output(&serde_json::json!({"ok": true})));
            }
            if command.contains("agent start") {
                return Ok(json_output(&serde_json::json!({
                    "lease": {
                        "id": "lease-remote-codex",
                        "writeTargetMode": "direct",
                        "writeTargetPath": "~/Code/foo",
                        "outputTarget": {
                            "kind": "real-project",
                            "path": "~/Code/foo"
                        }
                    }
                })));
            }
            if command.contains("codex exec") {
                return Ok(ProcessOutput {
                    status_code: 0,
                    stdout: "codex completed\n".to_string(),
                    stderr: String::new(),
                });
            }
            if command.contains("agent complete --lease")
                && command.contains("lease-remote-codex")
                && command.contains("--json")
            {
                return Ok(json_output(&serde_json::json!({
                    "requestId": "tool-complete",
                    "leaseId": "lease-remote-codex",
                    "tool": "complete-task",
                    "outcome": "allowed",
                    "summary": "task completed"
                })));
            }
            if command.contains("accept")
                && command.contains("work_view_remote_codex")
                && command.contains("--json")
            {
                return Ok(json_output(&serde_json::json!({
                    "action": "accepted",
                    "workView": {"id": "work_view_remote_codex"},
                    "status": {"level": "healthy", "attentionItems": []}
                })));
            }
            Ok(json_output(&serde_json::json!({})))
        }
    }

    fn json_output<T: serde::Serialize>(value: &T) -> ProcessOutput {
        ProcessOutput {
            status_code: 0,
            stdout: serde_json::to_string(value).expect("json") + "\n",
            stderr: String::new(),
        }
    }

    #[test]
    fn remote_sync_ready_requires_healthy_without_attention() {
        assert!(remote_sync_is_ready(&WorkspaceStatus::healthy()));
        assert!(!remote_sync_is_ready(&WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items: Vec::new(),
        }));
        assert!(!remote_sync_is_ready(&WorkspaceStatus {
            level: StatusLevel::Healthy,
            attention_items: vec!["device trust has not settled".to_string()],
        }));
        assert!(!remote_sync_is_ready(&WorkspaceStatus {
            level: StatusLevel::Limited,
            attention_items: vec!["remote daemon unavailable".to_string()],
        }));
    }

    #[test]
    fn remote_daemon_sync_ready_requires_matching_local_and_remote_heads() {
        let ready = r#"{
          "daemon": {"state": "running"},
          "sync": {
            "state": "idle",
            "lastOutcome": "no-changes",
            "localHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3},
            "remoteHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3}
          }
        }"#;
        let stale = r#"{
          "daemon": {"state": "running"},
          "sync": {
            "state": "idle",
            "lastOutcome": "no-changes",
            "localHead": {"workspaceId": "ws", "snapshotId": "snap-new", "version": 4},
            "remoteHead": {"workspaceId": "ws", "snapshotId": "snap-old", "version": 3}
          }
        }"#;
        let just_advanced = r#"{
          "daemon": {"state": "running"},
          "sync": {
            "state": "idle",
            "lastOutcome": "advanced",
            "localHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3},
            "remoteHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3}
          }
        }"#;

        assert!(remote_daemon_sync_is_ready(ready));
        assert!(remote_daemon_sync_is_ready(just_advanced));
        assert!(!remote_daemon_sync_is_ready(stale));
        assert!(!remote_daemon_sync_is_ready(
            r#"{"daemon":{"state":"running"}}"#
        ));
    }

    #[test]
    fn bootstrap_root_unexpands_local_home_for_remote_hosts() {
        assert_eq!(
            normalize_remote_root_for_home("/workspace/theo/Code", "/workspace/theo"),
            "~/Code"
        );
        assert_eq!(
            normalize_remote_root_for_home("/srv/Code", "/workspace/theo"),
            "/srv/Code"
        );
    }

    #[test]
    fn bootstrap_output_marks_sync_blocked_when_bootstrap_did_not_complete() {
        let output = bootstrap_output(
            BootstrapOutputBase {
                host: "linux-box".to_string(),
                root: "~/Code".to_string(),
                generated_at: "2026-06-24T12:00:00Z".to_string(),
                steps: vec![step(
                    "install",
                    BootstrapStepState::Blocked,
                    "install failed",
                )],
                agent_handoff: None,
            },
            None,
            None,
            false,
            None,
        );

        assert_eq!(output.sync, BootstrapSyncState::Blocked);
        assert_eq!(output.next_required_phase, None);
        assert!(output.remote_status.needs_attention());
        assert_eq!(
            output.next_actions,
            vec![SafeAction {
                label: "Retry remote bootstrap".to_string(),
                command: Some("bowline connect 'linux-box' --root '~/Code' --json".to_string()),
            }]
        );
    }

    #[test]
    fn bootstrap_output_keeps_trust_separate_from_sync_status() {
        let output = bootstrap_output(
            BootstrapOutputBase {
                host: "linux-box".to_string(),
                root: "~/Code".to_string(),
                generated_at: "2026-06-24T12:00:00Z".to_string(),
                steps: vec![step(
                    "sync",
                    BootstrapStepState::Blocked,
                    "daemon unavailable",
                )],
                agent_handoff: None,
            },
            None,
            None,
            true,
            Some(WorkspaceStatus {
                level: StatusLevel::Limited,
                attention_items: vec!["daemon unavailable".to_string()],
            }),
        );

        assert!(output.trusted);
        assert_eq!(output.sync, BootstrapSyncState::Blocked);
        assert_eq!(output.next_required_phase, None);
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Inspect remote daemon status"
                && action.command.as_deref()
                    == Some(ssh_command("linux-box", "bowline daemon status --json").as_str())
        }));
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Inspect remote status"
                && action.command.as_deref()
                    == Some(ssh_command("linux-box", "bowline status ~/Code --json").as_str())
        }));
    }

    #[test]
    fn bootstrap_output_returns_agent_handoff_actions_when_ready() {
        let output = bootstrap_output(
            BootstrapOutputBase {
                host: "linux-box".to_string(),
                root: "~/Code".to_string(),
                generated_at: "2026-06-24T12:00:00Z".to_string(),
                steps: vec![step("sync", BootstrapStepState::Completed, "sync ready")],
                agent_handoff: None,
            },
            None,
            None,
            true,
            Some(WorkspaceStatus::healthy()),
        );

        assert_eq!(output.sync, BootstrapSyncState::Ready);
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Inspect remote status"
                && action.command.as_deref()
                    == Some(ssh_command("linux-box", "bowline status ~/Code --json").as_str())
        }));
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Inspect remote next actions"
                && action.command.as_deref()
                    == Some(ssh_command("linux-box", "bowline status ~/Code --json").as_str())
        }));
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Start agent work in a project"
                && action.command.as_deref()
                    == Some(ssh_command(
                        "linux-box",
                        "cd ~/Code/<project> && bowline agent start . --task '<task>' --base latest-workspace --hydrate-budget 512MiB --json",
                    ).as_str())
        }));
    }

    #[test]
    fn blocked_remote_agent_handoff_points_at_conflict_resolution() {
        let output = bootstrap_output(
            BootstrapOutputBase {
                host: "linux-box".to_string(),
                root: "~/Code".to_string(),
                generated_at: "2026-06-24T12:00:00Z".to_string(),
                steps: vec![step(
                    "agent-lease",
                    BootstrapStepState::Blocked,
                    "Remote agent start failed: conflicts need attention",
                )],
                agent_handoff: Some(BootstrapAgentHandoff {
                    project: "foo".to_string(),
                    task: "implement the thing".to_string(),
                    agent: Some("codex".to_string()),
                    lease_id: None,
                    write_target_mode: None,
                    write_target_path: None,
                    work_view_id: None,
                    work_view_path: None,
                    launched: false,
                    accepted: false,
                }),
            },
            None,
            None,
            true,
            Some(WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec!["1 unresolved conflict needs attention".to_string()],
            }),
        );

        assert_eq!(output.sync, BootstrapSyncState::Blocked);
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Resolve remote conflicts"
                && action.command.as_deref()
                    == Some(ssh_command("linux-box", "bowline resolve ~/Code --json").as_str())
        }));
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Start remote agent work"
                && action.command.as_deref().is_some_and(|command| {
                    command.contains("bowline agent start foo")
                        && command.contains("implement the thing")
                })
        }));
    }

    #[test]
    fn bootstrap_output_returns_local_approval_recovery_action() {
        let output = bootstrap_output(
            BootstrapOutputBase {
                host: "linux box".to_string(),
                root: "/workspace/theo/Code Projects".to_string(),
                generated_at: "2026-06-24T12:00:00Z".to_string(),
                steps: vec![step(
                    "approve",
                    BootstrapStepState::Blocked,
                    "key store locked",
                )],
                agent_handoff: None,
            },
            None,
            None,
            false,
            None,
        );

        assert_eq!(output.sync, BootstrapSyncState::Blocked);
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Inspect local device requests"
                && action.command.as_deref() == Some("bowline status --json")
        }));
        assert!(output.next_actions.iter().any(|action| {
            action.label == "Retry remote bootstrap"
                && action.command.as_deref()
                    == Some(
                        "bowline connect 'linux box' --root '/workspace/theo/Code Projects' --json",
                    )
        }));
    }

    #[test]
    fn remote_path_arg_preserves_remote_tilde_expansion() {
        assert_eq!(remote_path_arg("~/Code"), "~/Code");
        assert_eq!(remote_path_arg("~/Code Projects"), "~/'Code Projects'");
        assert_eq!(
            remote_path_arg("/workspace/theo/Code Projects"),
            "'/workspace/theo/Code Projects'"
        );
    }

    #[test]
    fn remote_bootstrap_pins_sanitized_device_id() {
        let env = remote_bootstrap_env("linux-box");

        assert!(env.iter().any(|(key, _)| key == "BOWLINE_DEVICE_NAME"));
        assert!(
            env.iter()
                .any(|(key, value)| key == "BOWLINE_DEVICE_ID" && value == "device_linux_box")
        );
        assert!(env.iter().any(
            |(key, value)| key == "BOWLINE_DEVICE_NAME" && value == "bowline-remote-linux_box"
        ));
    }

    #[test]
    fn remote_rebootstrap_device_id_uses_fresh_suffix() {
        assert_eq!(remote_device_id("mac-mini.local"), "device_mac_mini_local");
        assert_ne!(
            remote_rebootstrap_device_id("mac-mini.local", "first"),
            remote_rebootstrap_device_id("mac-mini.local", "second")
        );
        assert!(
            remote_rebootstrap_device_id("mac-mini.local", "first")
                .starts_with("device_mac_mini_local_")
        );
    }

    #[test]
    fn remote_bootstrap_secrets_require_durable_account_session() {
        let without_any_durable_auth = remote_bootstrap_secret_env_from(None, None);
        assert!(remote_bootstrap_auth_error(&without_any_durable_auth));

        let with_session =
            remote_bootstrap_secret_env_from(Some("bowline-session".to_string()), None);
        assert!(!remote_bootstrap_auth_error(&with_session));
        assert!(with_session.contains(&(
            "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
            "bowline-session".to_string()
        )));
        assert!(
            !with_session
                .iter()
                .any(|(key, _)| key == "BOWLINE_WORKOS_ACCESS_TOKEN")
        );

        let with_control = remote_bootstrap_secret_env_from(
            Some("bowline-session".to_string()),
            Some("durable-control".to_string()),
        );

        assert!(with_control.contains(&(
            "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
            "bowline-session".to_string()
        )));
        assert!(with_control.contains(&(
            "BOWLINE_CONTROL_PLANE_TOKEN".to_string(),
            "durable-control".to_string()
        )));
        assert!(
            !with_control
                .iter()
                .any(|(key, _)| key == "BOWLINE_WORKOS_REFRESH_TOKEN")
        );
        assert!(!remote_bootstrap_auth_error(&with_control));
    }

    #[test]
    fn fake_ssh_bootstrap_completes_device_trust_runs_agent_and_completes_direct_lease() {
        let control_plane = FakeControlPlaneClient::default();
        let workspace_id = WorkspaceId::new("ws_agent_native_fake_bootstrap");
        control_plane.create_workspace(workspace_id.as_str());
        let local_keychain = FakeKeychain::default();
        bowline_local::trust::ensure_first_device_trust_root(
            &control_plane,
            &local_keychain,
            workspace_id.clone(),
            DeviceId::new("local-codex"),
            "Local Codex".to_string(),
            DevicePlatform::Macos,
            "2026-06-26T12:00:00Z",
        )
        .expect("local device trusted");
        let runner = FakeBootstrapRunner {
            control_plane: control_plane.clone(),
            remote_keychain: FakeKeychain::default(),
            workspace_id: workspace_id.clone(),
            request_id: Rc::new(RefCell::new(None)),
        };
        let output = run_after_install(
            &runner,
            BootstrapSshArgs {
                host: "linux-box".to_string(),
                root: "~/Code".to_string(),
                artifact: None,
                project: Some("foo".to_string()),
                task: Some("implement the thing".to_string()),
                agent: Some("codex".to_string()),
            },
            "2026-06-26T12:00:00Z".to_string(),
            vec![step(
                "install",
                BootstrapStepState::Completed,
                "Installed fake bowline artifacts.",
            )],
            RemoteBowlineInstall {
                platform: RemotePlatform {
                    os: "linux".to_string(),
                    arch: "x86_64".to_string(),
                },
                remote_binary: "~/.local/bin/bowline".to_string(),
                remote_daemon_binary: "~/.local/bin/bowline-daemon".to_string(),
                artifact_sha256: "0123456789abcdef".repeat(4),
                daemon_artifact_sha256: "fedcba9876543210".repeat(4),
            },
            &control_plane,
            &local_keychain,
            workspace_id.clone(),
            DeviceId::new("local-codex"),
            Vec::new(),
        );

        assert!(output.trusted);
        assert_eq!(output.sync, BootstrapSyncState::Ready);
        assert!(
            output
                .steps
                .iter()
                .all(|step| step.state == BootstrapStepState::Completed)
        );
        assert_eq!(
            output
                .authorized_device
                .as_ref()
                .expect("authorized remote")
                .id
                .as_str(),
            "remote-linux"
        );
        assert!(output.steps.iter().any(|step| {
            step.name == "agent-lease"
                && step.state == BootstrapStepState::Completed
                && step.summary.contains("lease-remote-codex")
        }));
        assert!(output.steps.iter().any(|step| {
            step.name == "agent-run"
                && step.state == BootstrapStepState::Completed
                && step.summary.contains("Codex finished")
        }));
        assert!(output.steps.iter().any(|step| {
            step.name == "agent-complete"
                && step.state == BootstrapStepState::Completed
                && step.summary.contains("Completed direct remote lease")
        }));
        assert!(
            !output
                .next_actions
                .iter()
                .any(|action| action.label.contains("Launch Codex"))
        );
        assert!(
            !output
                .next_actions
                .iter()
                .any(|action| action.label.contains("Copy prompt"))
        );

        let trust = control_plane
            .list_device_trust(workspace_id.as_str())
            .expect("trust list");
        assert!(trust.pending_requests.is_empty());
        assert!(trust.authorized_devices.iter().any(|device| {
            device.device_id == "remote-linux" && device.device_name == "Remote Linux"
        }));
    }

    #[test]
    fn remote_device_trust_requires_exact_authorized_device() {
        let control_plane = FakeControlPlaneClient::default();
        let workspace_id = bowline_core::ids::WorkspaceId::new("ws_bootstrap_trust");
        control_plane.create_workspace(workspace_id.as_str());
        let trusted_keychain = FakeKeychain::default();
        bowline_local::trust::ensure_first_device_trust_root(
            &control_plane,
            &trusted_keychain,
            workspace_id.clone(),
            DeviceId::new("trusted-device"),
            "Trusted Mac",
            bowline_core::devices::DevicePlatform::Macos,
            "2026-06-24T12:00:00Z",
        )
        .expect("first trusted device");

        let remote_keychain = FakeKeychain::default();
        let request = bowline_local::trust::create_device_request(
            &control_plane,
            &remote_keychain,
            bowline_local::trust::DeviceRequestOptions {
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("remote-device"),
                device_name: "Linux Server".to_string(),
                platform: bowline_core::devices::DevicePlatform::Linux,
                host: Some("linux-server".to_string()),
                root: Some("~/Code".to_string()),
                generated_at: "2026-06-24T12:00:00Z".to_string(),
            },
        )
        .expect("request created");

        let before_accept = verify_remote_device_trust(&control_plane, &request)
            .expect_err("pending request is not trusted yet");
        assert!(before_accept.contains("not authorized"));

        bowline_local::trust::approve_device_request(
            &control_plane,
            &trusted_keychain,
            bowline_local::trust::ApproveDeviceOptions {
                workspace_id: workspace_id.clone(),
                request_id: request.request_id.clone(),
                approver_device_id: DeviceId::new("trusted-device"),
                generated_at: "2026-06-24T12:00:01Z".to_string(),
            },
        )
        .expect("request approved");
        bowline_local::trust::accept_device_grant(
            &control_plane,
            &remote_keychain,
            &workspace_id,
            &request.request_id,
            &request.requester_device_id,
        )
        .expect("grant accepted");

        let verified =
            verify_remote_device_trust(&control_plane, &request).expect("remote device trusted");
        assert_eq!(verified.id.as_str(), "remote-device");
        assert_eq!(verified.trust_state, DeviceTrustState::Trusted);
    }
}
