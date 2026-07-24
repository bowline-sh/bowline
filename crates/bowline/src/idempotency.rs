use super::*;

pub(super) fn print_dry_run(cli: Cli) -> ExitCode {
    let Some((command_name, target, would_change, risk)) = dry_run_plan(&cli.command) else {
        print_usage_error(
            cli.command.name(),
            "dry_run_unsupported",
            "--dry-run is not supported for this command",
            cli.json,
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let (apply_command, warnings) = dry_run_apply_command(&cli);
    let apply_action = RepairCommand::mutating(
        "Run the command without --dry-run".to_string(),
        Some(apply_command.clone()),
    );
    let output = DryRunCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: command_name,
        generated_at: generated_at(),
        status: DryRunStatus::DryRun,
        allowed: true,
        risk,
        target,
        would_change,
        warnings,
        apply_command,
        next_actions: vec![apply_action],
    };
    if cli.json {
        print_json(&output);
    } else {
        println!(
            "bowline dry-run: {}\nTarget: {}\nRisk: {}\nWould change:\n  {}",
            command_name_token(command_name),
            output.target,
            output.risk,
            output.would_change.join("\n  ")
        );
        println!("\nApply:\n  {}", output.apply_command);
    }
    ExitCode::SUCCESS
}

fn dry_run_apply_command(cli: &Cli) -> (String, Vec<String>) {
    let Some(command_args) = command_args_for_apply(&cli.command) else {
        return ("bowline".to_string(), Vec::new());
    };
    let mut args = vec!["bowline".to_string()];
    args.extend(command_args);
    if cli.socket != default_socket_path() {
        args.push("--socket".to_string());
        args.push(cli.socket.display().to_string());
    }
    (bowline_core::shell::quote_command(args), Vec::new())
}

fn command_args_for_apply(command: &Command) -> Option<Vec<String>> {
    if let Some(args) = work_command_args_for_apply(command) {
        return Some(args);
    }
    match command {
        Command::Approve(args) => {
            let mut argv = vec![
                "device".to_string(),
                "approve".to_string(),
                "--root".to_string(),
                args.selection.root.clone(),
            ];
            if let Some(project) = &args.selection.project {
                argv.extend(["--project".to_string(), project.clone()]);
            }
            argv.extend(trust_selector_argv(&args.selector));
            if args.yes {
                argv.push("--yes".to_string());
            }
            Some(argv)
        }
        Command::Deny(args) => {
            let mut argv = vec![
                "device".to_string(),
                "deny".to_string(),
                "--root".to_string(),
                args.selection.root.clone(),
            ];
            if let Some(project) = &args.selection.project {
                argv.extend(["--project".to_string(), project.clone()]);
            }
            argv.extend(trust_selector_argv(&args.selector));
            Some(argv)
        }
        Command::Revoke(args) => {
            let mut argv = vec![
                "device".to_string(),
                "revoke".to_string(),
                "--root".to_string(),
                args.selection.root.clone(),
                "--device".to_string(),
                args.device_id.clone(),
            ];
            if let Some(project) = &args.selection.project {
                argv.extend(["--project".to_string(), project.clone()]);
            }
            Some(argv)
        }
        Command::Recovery(recovery::RecoveryArgs::Create) => {
            Some(vec!["recover".to_string(), "create".to_string()])
        }
        Command::Recovery(recovery::RecoveryArgs::Rotate) => {
            Some(vec!["recover".to_string(), "rotate".to_string()])
        }
        Command::Recovery(recovery::RecoveryArgs::Verify { envelope_id }) => Some(vec![
            "recover".to_string(),
            "verify".to_string(),
            envelope_id.clone(),
        ]),
        Command::Recovery(recovery::RecoveryArgs::Use { envelope_id }) => Some(vec![
            "recover".to_string(),
            "use".to_string(),
            envelope_id.clone(),
        ]),
        Command::Recovery(recovery::RecoveryArgs::Revoke { envelope_id }) => Some(vec![
            "recover".to_string(),
            "revoke".to_string(),
            envelope_id.clone(),
        ]),
        Command::BootstrapSsh(args) => {
            let mut argv = vec![
                "connect".to_string(),
                args.host.clone(),
                "--root".to_string(),
                args.root.clone(),
            ];
            if let Some(artifact) = &args.artifact {
                argv.extend(["--binary".to_string(), artifact.clone()]);
            }
            Some(argv)
        }
        Command::WorkCreate(_)
        | Command::WorkAccept(_)
        | Command::WorkDiscard(_)
        | Command::WorkRestore(_)
        | Command::WorkCleanup(_) => None,
        Command::ForgetLocal(args) => {
            let mut argv = vec!["forget-local".to_string(), args.project_path.clone()];
            if args.yes {
                argv.push("--yes".to_string());
            }
            Some(argv)
        }
        Command::Archive(args) => {
            let mut argv = vec!["archive".to_string(), args.project_path.clone()];
            if args.restore {
                argv.push("--restore".to_string());
            }
            Some(argv)
        }
        Command::Purge(args) => {
            let mut argv = vec!["purge".to_string(), args.project_path.clone()];
            if args.cancel {
                argv.push("--cancel".to_string());
            }
            if let Some(grace_days) = args.grace_days {
                argv.extend(["--grace".to_string(), grace_days.to_string()]);
            }
            Some(argv)
        }
        Command::Daemon(DaemonCommand::Install) => {
            Some(vec!["daemon".to_string(), "install".to_string()])
        }
        Command::Daemon(DaemonCommand::Restart) => {
            Some(vec!["daemon".to_string(), "restart".to_string()])
        }
        Command::Daemon(DaemonCommand::Uninstall) => {
            Some(vec!["daemon".to_string(), "uninstall".to_string()])
        }
        _ => None,
    }
}

fn work_command_args_for_apply(command: &Command) -> Option<Vec<String>> {
    match command {
        Command::WorkCreate(args) => {
            let mut argv = vec![
                "work".to_string(),
                "create".to_string(),
                args.project_path.clone(),
                args.name.clone(),
            ];
            if let Some(from) = &args.from {
                argv.extend(["--from".to_string(), from.clone()]);
            }
            Some(argv)
        }
        Command::WorkAccept(args) => Some(work_selector_apply_args(CommandName::Accept, args)),
        Command::WorkDiscard(args) => Some(work_selector_apply_args(CommandName::Discard, args)),
        Command::WorkRestore(args) => Some(work_selector_apply_args(CommandName::Restore, args)),
        Command::WorkCleanup(_) => {
            let mut argv = vec!["work".to_string(), "cleanup".to_string()];
            argv.push("--apply".to_string());
            Some(argv)
        }
        _ => None,
    }
}

fn work_selector_apply_args(command: CommandName, args: &work::WorkSelectorArgs) -> Vec<String> {
    let mut argv = command
        .token()
        .split_whitespace()
        .map(str::to_string)
        .chain(std::iter::once(args.selector.clone()))
        .collect::<Vec<_>>();
    for path in &args.paths {
        argv.extend(["--path".to_string(), path.clone()]);
    }
    argv
}

pub(super) fn dry_run_plan(
    command: &Command,
) -> Option<(CommandName, String, Vec<String>, String)> {
    match command {
        Command::Approve(args) => Some((
            CommandName::Approve,
            trust_selector_label(&args.selector),
            vec!["approve a pending device trust request".to_string()],
            "trust-change".to_string(),
        )),
        Command::Deny(args) => Some((
            CommandName::Deny,
            trust_selector_label(&args.selector),
            vec!["deny a pending device trust request".to_string()],
            "trust-change".to_string(),
        )),
        Command::Revoke(args) => Some((
            CommandName::Revoke,
            args.device_id.clone(),
            vec!["revoke device trust".to_string()],
            "trust-change".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Create) => Some((
            CommandName::Recover,
            "current workspace recovery key".to_string(),
            vec!["create a new recovery key envelope".to_string()],
            "secret-material".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Rotate) => Some((
            CommandName::Recover,
            "current workspace recovery key".to_string(),
            vec!["rotate recovery key material".to_string()],
            "secret-material".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Verify { envelope_id }) => Some((
            CommandName::Recover,
            envelope_id.clone(),
            vec!["verify recovery words from stdin".to_string()],
            "secret-material".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Revoke { envelope_id }) => Some((
            CommandName::Recover,
            envelope_id.clone(),
            vec!["revoke recovery key envelope".to_string()],
            "trust-change".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Use { envelope_id }) => Some((
            CommandName::Recover,
            envelope_id.clone(),
            vec!["submit recovery words from stdin and create a device grant".to_string()],
            "secret-material".to_string(),
        )),
        Command::BootstrapSsh(args) => Some((
            CommandName::Connect,
            args.host.clone(),
            vec![
                "install or update remote bowline binaries".to_string(),
                "establish device trust so the remote host materializes the workspace".to_string(),
            ],
            "remote-mutation".to_string(),
        )),
        Command::WorkCreate(args) => Some((
            CommandName::WorkCreate,
            format!("{}:{}", args.project_path, args.name),
            vec!["create or reuse a work view".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::WorkAccept(args) => Some((
            CommandName::Accept,
            args.selector.clone(),
            vec!["apply work-view changes to the target project".to_string()],
            "filesystem-write".to_string(),
        )),
        Command::WorkDiscard(args) => Some((
            CommandName::Discard,
            args.selector.clone(),
            vec!["mark work view as discarded".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::WorkRestore(args) => Some((
            CommandName::Restore,
            args.selector.clone(),
            vec!["restore a discarded work view".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::WorkCleanup(args) => Some((
            CommandName::Cleanup,
            "retained work views".to_string(),
            if args.apply {
                vec!["remove cleanup-eligible work-view metadata and overlays".to_string()]
            } else {
                vec!["no changes; cleanup preview remains read-only".to_string()]
            },
            if args.apply {
                "workspace-metadata".to_string()
            } else {
                "none".to_string()
            },
        )),
        Command::ForgetLocal(args) => Some((
            CommandName::ForgetLocal,
            args.project_path.clone(),
            vec!["preview local bytes that would be removed on this device".to_string()],
            "local-filesystem-delete".to_string(),
        )),
        Command::Archive(args) => {
            Some((
                CommandName::Archive,
                args.project_path.clone(),
                if args.restore {
                    vec!["restore the project to active namespace listings".to_string()]
                } else {
                    vec!["hide the project from default namespace listings without deleting local bytes".to_string()]
                },
                "workspace-metadata".to_string(),
            ))
        }
        Command::Purge(args) => Some((
            CommandName::Purge,
            args.project_path.clone(),
            if args.cancel {
                vec!["cancel the purge grace window and keep the archive".to_string()]
            } else {
                vec!["mark archived project objects collectible after the grace window".to_string()]
            },
            "remote-destruction-scheduled".to_string(),
        )),
        Command::Daemon(DaemonCommand::Install) => Some((
            CommandName::DaemonInstall,
            "local OS service".to_string(),
            vec!["install or update daemon service files".to_string()],
            "service-mutation".to_string(),
        )),
        Command::Daemon(DaemonCommand::Restart) => Some((
            CommandName::DaemonRestart,
            "local OS service".to_string(),
            vec!["restart daemon service".to_string()],
            "service-mutation".to_string(),
        )),
        Command::Daemon(DaemonCommand::Uninstall) => Some((
            CommandName::DaemonUninstall,
            "local OS service".to_string(),
            vec!["uninstall daemon service files".to_string()],
            "service-mutation".to_string(),
        )),
        _ => None,
    }
}

fn trust_selector_argv(selector: &TrustRequestSelector) -> Vec<String> {
    match selector {
        TrustRequestSelector::Request(request_id) => {
            vec!["--request".to_string(), request_id.clone()]
        }
        TrustRequestSelector::Code(code) => vec!["--code".to_string(), code.clone()],
    }
}

fn trust_selector_label(selector: &TrustRequestSelector) -> String {
    match selector {
        TrustRequestSelector::Request(request_id) => request_id.clone(),
        TrustRequestSelector::Code(code) => format!("matching code {code}"),
    }
}
