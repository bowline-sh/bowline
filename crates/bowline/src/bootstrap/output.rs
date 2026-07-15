use super::*;
use bowline_core::status::RepairCommand;

pub(super) fn output_base(
    args: &BootstrapSshArgs,
    generated_at: &str,
    steps: Vec<BootstrapStep>,
) -> BootstrapOutputBase {
    BootstrapOutputBase {
        host: args.host.clone(),
        root: args.root.clone(),
        local_root: runtime::active_workspace_root(),
        generated_at: generated_at.to_string(),
        steps,
        remote_status_items: Vec::new(),
    }
}

pub(super) fn bootstrap_output(
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
    let repair_actions = bootstrap_repair_actions(&base, trusted, sync, &remote_status);

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
        repair_actions,
    }
}

// Bootstrap no longer launches or supervises the remote agent — the trusted host
// materializes the workspace on arrival. This surface is now purely trust/repair
// guidance: inspect / retry / verify-trust remedies for blocked bootstrap steps.
pub(super) fn bootstrap_repair_actions(
    base: &BootstrapOutputBase,
    trusted: bool,
    sync: BootstrapSyncState,
    remote_status: &WorkspaceStatus,
) -> Vec<RepairCommand> {
    let mut actions = Vec::new();
    let root = remote_path_arg(&base.root);
    let remote_status_command =
        ssh_command(&base.host, &format!("bowline status --root {root} --json"));

    if trusted {
        actions.push(RepairCommand::inspect(
            "Inspect remote status",
            Some(remote_status_command.clone()),
        ));
    }

    match sync {
        // Ready: the workspace materializes on the trusted host; no repair needed.
        BootstrapSyncState::Ready => {}
        BootstrapSyncState::Prepared => {
            actions.push(RepairCommand::mutating(
                "Start the remote daemon",
                Some(ssh_command(&base.host, "bowline daemon start --json")),
            ));
        }
        BootstrapSyncState::Blocked => {
            if let Some(blocked) = base
                .steps
                .iter()
                .rev()
                .find(|step| step.state == BootstrapStepState::Blocked)
            {
                actions.extend(blocked_repair_actions(base, blocked.name));
            } else if remote_status.needs_attention() {
                actions.push(RepairCommand::inspect(
                    "Inspect remote status",
                    Some(remote_status_command),
                ));
            }
        }
    }

    dedupe_repair_actions(actions)
}

pub(super) fn blocked_repair_actions(
    base: &BootstrapOutputBase,
    blocked_step: BootstrapStepName,
) -> Vec<RepairCommand> {
    let root = remote_path_arg(&base.root);
    let local_root = remote_path_arg(base.local_root.as_deref().unwrap_or("~/Code"));
    let retry = RepairCommand::mutating(
        "Retry remote bootstrap",
        Some(format!(
            "bowline connect {} --root {} --json",
            bowline_core::shell::quote_word(&base.host),
            bowline_core::shell::quote_word(&base.root)
        )),
    );
    match blocked_step {
        BootstrapStepName::Install
        | BootstrapStepName::AuthorizeBootstrap
        | BootstrapStepName::ControlPlane
        | BootstrapStepName::RemoteAuth => vec![retry],
        BootstrapStepName::Request
        | BootstrapStepName::Parse
        | BootstrapStepName::Compare
        | BootstrapStepName::Accept => vec![
            RepairCommand::inspect(
                "Inspect remote device requests",
                Some(ssh_command(
                    &base.host,
                    &format!("bowline status --root {root} --json"),
                )),
            ),
            retry,
        ],
        BootstrapStepName::Approve => vec![
            RepairCommand::inspect(
                "Inspect local device requests",
                Some(format!("bowline status --root {local_root} --json")),
            ),
            retry,
        ],
        BootstrapStepName::Trust => vec![
            RepairCommand::inspect(
                "Verify local device trust",
                Some(format!("bowline status --root {local_root} --json")),
            ),
            RepairCommand::inspect(
                "Verify remote device trust",
                Some(ssh_command(
                    &base.host,
                    &format!("bowline status --root {root} --json"),
                )),
            ),
            retry,
        ],
        BootstrapStepName::PrepareRoot => vec![
            RepairCommand::mutating(
                "Set up remote root",
                Some(ssh_command(
                    &base.host,
                    &format!("bowline setup --root {root} --json"),
                )),
            ),
            retry,
        ],
        BootstrapStepName::MetadataDefault => vec![
            RepairCommand::inspect(
                "Inspect remote status",
                Some(ssh_command(
                    &base.host,
                    &format!("bowline status --root {root} --json"),
                )),
            ),
            retry,
        ],
        BootstrapStepName::DaemonStart | BootstrapStepName::DaemonStatus => vec![
            RepairCommand::mutating(
                "Start remote daemon",
                Some(ssh_command(&base.host, "bowline daemon start --json")),
            ),
            RepairCommand::inspect(
                "Inspect remote daemon status",
                Some(ssh_command(&base.host, "bowline daemon status --json")),
            ),
            retry,
        ],
        BootstrapStepName::Sync => vec![
            RepairCommand::inspect(
                "Inspect remote daemon status",
                Some(ssh_command(&base.host, "bowline daemon status --json")),
            ),
            RepairCommand::inspect(
                "Inspect remote status",
                Some(ssh_command(
                    &base.host,
                    &format!("bowline status --root {root} --json"),
                )),
            ),
            retry,
        ],
        // The handoff lease is prepared, but bootstrap does not launch/complete/
        // accept the agent; a blocked handoff is repaired by resolving remote
        // conflicts (if any) and retrying, never by an agent-launch action.
        BootstrapStepName::AgentLease => {
            let mut actions = Vec::new();
            if remote_status_has_conflict(&base.remote_status_items) {
                actions.push(RepairCommand::inspect(
                    "Resolve remote conflicts",
                    Some(ssh_command(
                        &base.host,
                        &format!("bowline resolve {} --json", remote_path_arg(&base.root)),
                    )),
                ));
            }
            actions.push(retry);
            actions
        }
    }
}

pub(super) fn remote_status_has_conflict(items: &[StatusItem]) -> bool {
    items
        .iter()
        .any(|item| item.kind == StatusItemKind::Conflict)
}

pub(super) fn ssh_command(host: &str, remote_command: &str) -> String {
    format!(
        "ssh {} {}",
        bowline_core::shell::quote_word(host),
        bowline_core::shell::quote_word(&format!(
            "bash -lc {}",
            bowline_core::shell::quote_word(remote_command)
        ))
    )
}

pub(super) fn remote_path_arg(path: &str) -> String {
    if path == "~" {
        return "~".to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", bowline_core::shell::quote_word(rest));
    }
    bowline_core::shell::quote_word(path)
}

pub(super) fn dedupe_repair_actions(actions: Vec<RepairCommand>) -> Vec<RepairCommand> {
    let mut deduped: Vec<RepairCommand> = Vec::new();
    for action in actions {
        let already_present = deduped
            .iter()
            .any(|existing| existing.label == action.label && existing.command == action.command);
        if !already_present {
            deduped.push(action);
        }
    }
    deduped
}

pub(super) fn step(
    name: BootstrapStepName,
    state: BootstrapStepState,
    summary: impl Into<String>,
) -> BootstrapStep {
    BootstrapStep {
        name,
        state,
        summary: summary.into(),
    }
}
