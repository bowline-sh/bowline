use super::*;

pub(super) fn run_after_install<R>(
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
