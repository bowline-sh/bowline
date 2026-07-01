use super::*;

pub(super) fn output_base(
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

pub(super) fn bootstrap_next_actions(
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

pub(super) fn blocked_step_actions(
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

pub(super) fn remote_status_mentions_conflict(status: &WorkspaceStatus) -> bool {
    status
        .attention_items
        .iter()
        .any(|item| item.to_ascii_lowercase().contains("conflict"))
}

pub(super) fn agent_handoff_actions(
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

pub(super) fn agent_lease_create_action(
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

pub(super) fn ssh_command(host: &str, remote_command: &str) -> String {
    format!(
        "ssh {} {}",
        shell_quote(host),
        shell_quote(&format!("bash -lc {}", shell_quote(remote_command)))
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

pub(super) fn shell_safe_path(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '.' | '_' | '-' | ':' | '@' | '+')
        })
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

pub(super) fn dedupe_actions(actions: Vec<SafeAction>) -> Vec<SafeAction> {
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

pub(super) fn step(
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
