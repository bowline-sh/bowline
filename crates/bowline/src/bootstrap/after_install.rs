use super::*;

pub(super) struct AfterInstallInput<'a, R>
where
    R: ProcessRunner,
{
    pub(super) runner: &'a R,
    pub(super) args: BootstrapSshArgs,
    pub(super) generated_at: String,
    pub(super) steps: Vec<BootstrapStep>,
    pub(super) install: RemoteBowlineInstall,
    pub(super) control_plane: &'a dyn ControlPlaneClient,
    pub(super) key_store: &'a dyn DeviceKeyStore,
    pub(super) workspace_id: bowline_core::ids::WorkspaceId,
    pub(super) device_id: DeviceId,
    pub(super) remote_secret_env: Vec<(String, String)>,
}

struct AfterInstallContext<'a, R>
where
    R: ProcessRunner,
{
    runner: &'a R,
    args: BootstrapSshArgs,
    generated_at: String,
    steps: Vec<BootstrapStep>,
    control_plane: &'a dyn ControlPlaneClient,
    key_store: &'a dyn DeviceKeyStore,
    workspace_id: bowline_core::ids::WorkspaceId,
    device_id: DeviceId,
    options: BootstrapSshOptions,
}

struct TrustedRemoteDevice {
    remote_request: Option<DeviceApprovalRequest>,
    verified_remote_device: DeviceRecord,
}

enum RemoteTrustStage {
    AlreadyTrusted(DeviceRecord),
    PendingRequest(DeviceApprovalRequest),
}

struct SyncProbe {
    remote_status: Option<WorkspaceStatus>,
    remote_status_items: Vec<StatusItem>,
    sync_ready: bool,
}

type StageResult<T> = Result<T, Box<BootstrapSshCommandOutput>>;

pub(super) fn run_after_install<R>(input: AfterInstallInput<'_, R>) -> BootstrapSshCommandOutput
where
    R: ProcessRunner,
{
    let mut context = match create_bootstrap_context(input) {
        Ok(context) => context,
        Err(output) => return *output,
    };
    if let Err(output) = precheck_remote_auth(&mut context) {
        return *output;
    }
    if let Err(output) = prepare_remote_root(&mut context) {
        return *output;
    }

    let remote_trust_stage = match resolve_remote_trust_stage(&mut context) {
        Ok(stage) => stage,
        Err(output) => return *output,
    };
    let trusted_remote = match complete_remote_trust(&mut context, remote_trust_stage) {
        Ok(trusted) => trusted,
        Err(output) => return *output,
    };

    if let Err(output) = publish_default_metadata(&mut context, &trusted_remote) {
        return *output;
    }
    if let Err(output) = start_remote_daemon(&mut context, &trusted_remote) {
        return *output;
    }
    let sync_probe = match probe_final_remote_status(&mut context, &trusted_remote) {
        Ok(probe) => probe,
        Err(output) => return *output,
    };

    finish_after_install(context, trusted_remote, sync_probe)
}

fn create_bootstrap_context<R>(
    input: AfterInstallInput<'_, R>,
) -> StageResult<AfterInstallContext<'_, R>>
where
    R: ProcessRunner,
{
    let bootstrap_session =
        match input
            .control_plane
            .create_bootstrap_session(BootstrapSessionInput {
                workspace_id: input.workspace_id.clone(),
                host: Some(input.args.host.clone()),
                lease_handoff_digest: None,
                lease_id: None,
                root: Some(input.args.root.clone()),
                runtime: None,
                setup_receipts_digest: None,
                expires_in_ticks: 600,
            }) {
            Ok(session) => session,
            Err(error) => {
                let mut steps = input.steps;
                steps.push(step(
                    BootstrapStepName::AuthorizeBootstrap,
                    BootstrapStepState::Blocked,
                    format!("Could not create remote bootstrap session: {error}"),
                ));
                return Err(Box::new(bootstrap_output(
                    output_base(&input.args, &input.generated_at, steps),
                    None,
                    None,
                    false,
                    None,
                )));
            }
        };

    let mut steps = input.steps;
    steps.push(step(
        BootstrapStepName::AuthorizeBootstrap,
        BootstrapStepState::Completed,
        "Created a short-lived remote bootstrap session.",
    ));

    let options = BootstrapSshOptions {
        host: input.args.host.clone(),
        root: input.args.root.clone(),
        remote_binary: Some(input.install.remote_binary),
        remote_platform: Some(input.install.platform.os),
        remote_workspace_id: Some(input.workspace_id.as_str().to_string()),
        remote_env: remote_bootstrap_env(&input.args.host),
        remote_secret_env: input.remote_secret_env,
        bootstrap_token: Some(bootstrap_session.token),
    };

    Ok(AfterInstallContext {
        runner: input.runner,
        args: input.args,
        generated_at: input.generated_at,
        steps,
        control_plane: input.control_plane,
        key_store: input.key_store,
        workspace_id: input.workspace_id,
        device_id: input.device_id,
        options,
    })
}

fn precheck_remote_auth<R>(context: &mut AfterInstallContext<'_, R>) -> StageResult<()>
where
    R: ProcessRunner,
{
    if !remote_bootstrap_auth_error(&context.options.remote_secret_env) {
        return Ok(());
    }

    context.steps.push(step(
        BootstrapStepName::RemoteAuth,
        BootstrapStepState::Blocked,
        "Remote bootstrap needs an bowline account session for durable daemon auth; refusing to create a short-lived WorkOS-only remote.",
    ));
    Err(context.output(None, None, false, None))
}

fn prepare_remote_root<R>(context: &mut AfterInstallContext<'_, R>) -> StageResult<()>
where
    R: ProcessRunner,
{
    match ssh::prepare_remote_root(context.runner, &context.options) {
        Ok(_) => {
            context.steps.push(step(
                BootstrapStepName::PrepareRoot,
                BootstrapStepState::Completed,
                "Remote real directory root is initialized and accepted.",
            ));
            Ok(())
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::PrepareRoot,
                BootstrapStepState::Blocked,
                format!("Remote root preparation failed: {error}"),
            ));
            Err(context.output(None, None, false, None))
        }
    }
}

fn resolve_remote_trust_stage<R>(
    context: &mut AfterInstallContext<'_, R>,
) -> StageResult<RemoteTrustStage>
where
    R: ProcessRunner,
{
    if let Some(device) = existing_remote_device_with_key(context) {
        context.steps.push(step(
            BootstrapStepName::Request,
            BootstrapStepState::Completed,
            format!("Remote device {} is already trusted.", device.name),
        ));
        context.steps.push(step(
            BootstrapStepName::Trust,
            BootstrapStepState::Completed,
            format!("Remote device {} is trusted.", device.name),
        ));
        return Ok(RemoteTrustStage::AlreadyTrusted(device));
    }

    let request_probe = match ssh::probe_remote(context.runner, &context.options) {
        Ok(probe) => {
            context.steps.push(step(
                BootstrapStepName::Request,
                BootstrapStepState::Completed,
                "Remote device approval request created.",
            ));
            probe
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::Request,
                BootstrapStepState::Blocked,
                format!("Remote request failed: {error}"),
            ));
            return Err(context.output(None, None, false, None));
        }
    };

    let remote_request = parse_remote_request(context, &request_probe.stdout)?;
    Ok(RemoteTrustStage::PendingRequest(remote_request))
}

fn existing_remote_device_with_key<R>(
    context: &mut AfterInstallContext<'_, R>,
) -> Option<DeviceRecord>
where
    R: ProcessRunner,
{
    let device =
        existing_trusted_remote_device(context.runner, &context.options, &context.workspace_id)?;
    if remote_workspace_key_available(context.runner, &context.options, &context.workspace_id) {
        return Some(device);
    }

    set_remote_device_id(
        &mut context.options,
        remote_rebootstrap_device_id(&context.args.host, &context.generated_at),
    );
    None
}

fn parse_remote_request<R>(
    context: &mut AfterInstallContext<'_, R>,
    stdout: &str,
) -> StageResult<DeviceApprovalRequest>
where
    R: ProcessRunner,
{
    let remote_devices: DevicesCommandOutput = match serde_json::from_str(stdout) {
        Ok(output) => output,
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::Parse,
                BootstrapStepState::Blocked,
                format!("Remote request output was not valid bowline JSON: {error}"),
            ));
            return Err(context.output(None, None, false, None));
        }
    };

    let Some(remote_request) = remote_devices.created_request else {
        context.steps.push(step(
            BootstrapStepName::Parse,
            BootstrapStepState::Blocked,
            "Remote request output did not include a created request.",
        ));
        return Err(context.output(None, None, false, None));
    };
    Ok(remote_request)
}

fn complete_remote_trust<R>(
    context: &mut AfterInstallContext<'_, R>,
    stage: RemoteTrustStage,
) -> StageResult<TrustedRemoteDevice>
where
    R: ProcessRunner,
{
    match stage {
        RemoteTrustStage::AlreadyTrusted(device) => Ok(TrustedRemoteDevice {
            remote_request: None,
            verified_remote_device: device,
        }),
        RemoteTrustStage::PendingRequest(remote_request) => {
            verify_remote_request_against_trust(context, &remote_request)?;
            approve_remote_request(context, &remote_request)?;
            accept_remote_grant(context, &remote_request)?;
            let verified_remote_device = verify_accepted_remote_device(context, &remote_request)?;
            Ok(TrustedRemoteDevice {
                remote_request: Some(remote_request),
                verified_remote_device,
            })
        }
    }
}

fn verify_remote_request_against_trust<R>(
    context: &mut AfterInstallContext<'_, R>,
    remote_request: &DeviceApprovalRequest,
) -> StageResult<()>
where
    R: ProcessRunner,
{
    let trust = match context
        .control_plane
        .list_device_trust(&remote_request.workspace_id)
    {
        Ok(trust) => trust,
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::ControlPlane,
                BootstrapStepState::Blocked,
                format!("Could not fetch pending request from control plane: {error}"),
            ));
            return Err(context.output(Some(remote_request.clone()), None, false, None));
        }
    };

    let Some(cloud_request) = trust
        .pending_requests
        .iter()
        .find(|request| request.request_id == remote_request.request_id.as_str())
    else {
        context.steps.push(step(
            BootstrapStepName::Compare,
            BootstrapStepState::Blocked,
            "Remote request was not present in the control plane.",
        ));
        return Err(context.output(Some(remote_request.clone()), None, false, None));
    };
    if !request_matches_cloud(remote_request, cloud_request) {
        context.steps.push(step(
            BootstrapStepName::Compare,
            BootstrapStepState::Blocked,
            "Remote request did not match the control-plane request.",
        ));
        return Err(context.output(Some(remote_request.clone()), None, false, None));
    }

    context.steps.push(step(
        BootstrapStepName::Compare,
        BootstrapStepState::Completed,
        "Remote request matched the control-plane request.",
    ));
    Ok(())
}

fn approve_remote_request<R>(
    context: &mut AfterInstallContext<'_, R>,
    remote_request: &DeviceApprovalRequest,
) -> StageResult<()>
where
    R: ProcessRunner,
{
    match bowline_local::trust::approve_device_request(
        context.control_plane,
        context.key_store,
        bowline_local::trust::ApproveDeviceOptions {
            workspace_id: remote_request.workspace_id.clone(),
            request_id: remote_request.request_id.clone(),
            approver_device_id: context.device_id.clone(),
            generated_at: context.generated_at.clone(),
        },
    ) {
        Ok(_) => {
            context.steps.push(step(
                BootstrapStepName::Approve,
                BootstrapStepState::Completed,
                "Encrypted device grant uploaded.",
            ));
            Ok(())
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::Approve,
                BootstrapStepState::Blocked,
                format!("Local approval failed: {error}"),
            ));
            Err(context.output(Some(remote_request.clone()), None, false, None))
        }
    }
}

fn accept_remote_grant<R>(
    context: &mut AfterInstallContext<'_, R>,
    remote_request: &DeviceApprovalRequest,
) -> StageResult<()>
where
    R: ProcessRunner,
{
    match ssh::accept_remote_grant(
        context.runner,
        &context.options,
        remote_request.request_id.as_str(),
    ) {
        Ok(_) => {
            context.steps.push(step(
                BootstrapStepName::Accept,
                BootstrapStepState::Completed,
                "Remote device accepted and decrypted the grant.",
            ));
            Ok(())
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::Accept,
                BootstrapStepState::Blocked,
                format!("Remote grant acceptance failed: {error}"),
            ));
            Err(context.output(Some(remote_request.clone()), None, false, None))
        }
    }
}

fn verify_accepted_remote_device<R>(
    context: &mut AfterInstallContext<'_, R>,
    remote_request: &DeviceApprovalRequest,
) -> StageResult<DeviceRecord>
where
    R: ProcessRunner,
{
    match verify_remote_device_trust(context.control_plane, remote_request) {
        Ok(device) => {
            context.steps.push(step(
                BootstrapStepName::Trust,
                BootstrapStepState::Completed,
                format!("Remote device {} is trusted.", device.name),
            ));
            Ok(device)
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::Trust,
                BootstrapStepState::Blocked,
                error,
            ));
            Err(context.output(Some(remote_request.clone()), None, false, None))
        }
    }
}

fn publish_default_metadata<R>(
    context: &mut AfterInstallContext<'_, R>,
    trusted: &TrustedRemoteDevice,
) -> StageResult<()>
where
    R: ProcessRunner,
{
    match ssh::publish_default_metadata(context.runner, &context.options) {
        Ok(_) => {
            context.steps.push(step(
                BootstrapStepName::MetadataDefault,
                BootstrapStepState::Completed,
                "Remote bowline commands now use this workspace by default.",
            ));
            Ok(())
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::MetadataDefault,
                BootstrapStepState::Blocked,
                format!("Remote default metadata setup failed: {error}"),
            ));
            Err(context.trusted_output(trusted, true, None))
        }
    }
}

fn start_remote_daemon<R>(
    context: &mut AfterInstallContext<'_, R>,
    trusted: &TrustedRemoteDevice,
) -> StageResult<()>
where
    R: ProcessRunner,
{
    let mut stop_error = None;
    for attempt in 0..3 {
        match ssh::stop_remote_daemon(context.runner, &context.options) {
            Ok(_) => {
                stop_error = None;
                break;
            }
            Err(error) => {
                stop_error = Some(error);
                if attempt < 2 {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    if let Some(error) = stop_error {
        context.steps.push(step(
            BootstrapStepName::DaemonStart,
            BootstrapStepState::Blocked,
            format!("Remote daemon stop before update failed: {error}"),
        ));
        return Err(context.trusted_output(trusted, true, None));
    }
    let mut stopped = false;
    for attempt in 0..30 {
        match ssh::daemon_status_remote(context.runner, &context.options) {
            Ok(probe) if remote_daemon_is_stopped(&probe.stdout) => {
                stopped = true;
                break;
            }
            Ok(_) => {}
            Err(error) => {
                context.steps.push(step(
                    BootstrapStepName::DaemonStart,
                    BootstrapStepState::Blocked,
                    format!("Remote daemon stop verification failed: {error}"),
                ));
                return Err(context.trusted_output(trusted, true, None));
            }
        }
        if attempt < 29 {
            thread::sleep(Duration::from_millis(100));
        }
    }
    if !stopped {
        context.steps.push(step(
            BootstrapStepName::DaemonStart,
            BootstrapStepState::Blocked,
            "Remote daemon did not stop after the installed binary changed.",
        ));
        return Err(context.trusted_output(trusted, true, None));
    }

    match ssh::start_remote_daemon(context.runner, &context.options) {
        Ok(_) => {
            context.steps.push(step(
                BootstrapStepName::DaemonStart,
                BootstrapStepState::Completed,
                "Remote daemon restarted with the installed binary for the accepted root.",
            ));
            Ok(())
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::DaemonStart,
                BootstrapStepState::Blocked,
                format!("Remote daemon start failed: {error}"),
            ));
            Err(context.trusted_output(trusted, true, None))
        }
    }
}

fn probe_final_remote_status<R>(
    context: &mut AfterInstallContext<'_, R>,
    trusted: &TrustedRemoteDevice,
) -> StageResult<SyncProbe>
where
    R: ProcessRunner,
{
    let daemon_probe = wait_for_running_remote_daemon(context, trusted)?;

    if remote_daemon_sync_is_ready(&daemon_probe.stdout) {
        context.steps.push(step(
            BootstrapStepName::Sync,
            BootstrapStepState::Completed,
            "Remote daemon has completed sync for this real directory root.",
        ));
        return Ok(SyncProbe {
            remote_status: Some(WorkspaceStatus::healthy()),
            remote_status_items: Vec::new(),
            sync_ready: true,
        });
    }

    Ok(probe_remote_status(context))
}

fn wait_for_running_remote_daemon<R>(
    context: &mut AfterInstallContext<'_, R>,
    trusted: &TrustedRemoteDevice,
) -> StageResult<ssh::RemoteBootstrapProbe>
where
    R: ProcessRunner,
{
    match wait_for_remote_daemon(context.runner, &context.options) {
        Ok(probe) if remote_daemon_is_running(&probe.stdout) => {
            context.steps.push(step(
                BootstrapStepName::DaemonStatus,
                BootstrapStepState::Completed,
                "Remote daemon is running.",
            ));
            Ok(probe)
        }
        Ok(probe) => {
            context.steps.push(step(
                BootstrapStepName::DaemonStatus,
                BootstrapStepState::Blocked,
                remote_daemon_status_summary(&probe.stdout),
            ));
            Err(context.trusted_output(trusted, true, None))
        }
        Err(error) => {
            context.steps.push(step(
                BootstrapStepName::DaemonStatus,
                BootstrapStepState::Blocked,
                format!("Remote daemon status failed: {error}"),
            ));
            Err(context.trusted_output(trusted, true, None))
        }
    }
}

fn probe_remote_status<R>(context: &mut AfterInstallContext<'_, R>) -> SyncProbe
where
    R: ProcessRunner,
{
    match ssh::status_remote(context.runner, &context.options) {
        Ok(probe) => remote_status_from_stdout(context, &probe.stdout),
        Err(error) => {
            let status = WorkspaceStatus {
                level: StatusLevel::Limited,
                attention_items: vec![format!("Remote status check failed: {error}")],
            };
            context.steps.push(step(
                BootstrapStepName::Sync,
                BootstrapStepState::Blocked,
                status.attention_items[0].clone(),
            ));
            SyncProbe {
                remote_status: Some(status),
                remote_status_items: Vec::new(),
                sync_ready: false,
            }
        }
    }
}

fn remote_status_from_stdout<R>(context: &mut AfterInstallContext<'_, R>, stdout: &str) -> SyncProbe
where
    R: ProcessRunner,
{
    match serde_json::from_str::<StatusCommandOutput>(stdout) {
        Ok(output) => {
            let sync_ready = remote_sync_is_ready(&output.status);
            context.steps.push(step(
                BootstrapStepName::Sync,
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
            SyncProbe {
                remote_status: Some(output.status),
                remote_status_items: output.items,
                sync_ready,
            }
        }
        Err(error) => {
            let status = WorkspaceStatus {
                level: StatusLevel::Limited,
                attention_items: vec![format!(
                    "Remote status output was not valid bowline JSON: {error}"
                )],
            };
            context.steps.push(step(
                BootstrapStepName::Sync,
                BootstrapStepState::Blocked,
                status.attention_items[0].clone(),
            ));
            SyncProbe {
                remote_status: Some(status),
                remote_status_items: Vec::new(),
                sync_ready: false,
            }
        }
    }
}

fn finish_after_install<R>(
    mut context: AfterInstallContext<'_, R>,
    trusted: TrustedRemoteDevice,
    sync_probe: SyncProbe,
) -> BootstrapSshCommandOutput
where
    R: ProcessRunner,
{
    if sync_probe.sync_ready {
        create_agent_handoff_if_requested(
            context.runner,
            &context.options,
            &context.args,
            &mut context.steps,
        );
    }
    let mut base = output_base(&context.args, &context.generated_at, context.steps);
    base.remote_status_items = sync_probe.remote_status_items;

    bootstrap_output(
        base,
        trusted.remote_request,
        Some(trusted.verified_remote_device),
        true,
        sync_probe.remote_status,
    )
}

impl<R> AfterInstallContext<'_, R>
where
    R: ProcessRunner,
{
    fn output(
        &mut self,
        device_request: Option<DeviceApprovalRequest>,
        authorized_device: Option<DeviceRecord>,
        trusted: bool,
        remote_status: Option<WorkspaceStatus>,
    ) -> Box<BootstrapSshCommandOutput> {
        Box::new(bootstrap_output(
            output_base(
                &self.args,
                &self.generated_at,
                std::mem::take(&mut self.steps),
            ),
            device_request,
            authorized_device,
            trusted,
            remote_status,
        ))
    }

    fn trusted_output(
        &mut self,
        trusted: &TrustedRemoteDevice,
        is_trusted: bool,
        remote_status: Option<WorkspaceStatus>,
    ) -> Box<BootstrapSshCommandOutput> {
        self.output(
            trusted.remote_request.clone(),
            Some(trusted.verified_remote_device.clone()),
            is_trusted,
            remote_status,
        )
    }
}
