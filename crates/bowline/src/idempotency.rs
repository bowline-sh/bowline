use super::*;

pub(super) fn print_dry_run(cli: Cli) -> ExitCode {
    let Some((command_name, target, would_change, risk)) = dry_run_plan(&cli.command) else {
        print_usage_error(
            command_name_for_command(&cli.command),
            "dry_run_unsupported",
            "--dry-run is not supported for this command",
            cli.json,
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let (apply_command, warnings) = dry_run_apply_command(&cli);
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
        next_actions: vec![SafeAction {
            label: "Run the command without --dry-run".to_string(),
            command: None,
        }],
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
    if cli.socket != Path::new(DEFAULT_SOCKET) {
        args.push("--socket".to_string());
        args.push(cli.socket.display().to_string());
    }
    let mut warnings = Vec::new();
    if let Some(key) = &cli.idempotency_key {
        if is_idempotent_mutation(&cli.command) {
            args.push("--json".to_string());
            args.push("--idempotency-key".to_string());
            args.push(key.clone());
        } else {
            warnings.push(
                "Omitted --idempotency-key from applyCommand because this command cannot be replayed safely."
                    .to_string(),
            );
        }
    }
    args.extend(command_args);
    (shell_join(args), warnings)
}

fn command_args_for_apply(command: &Command) -> Option<Vec<String>> {
    match command {
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
        _ => command_args_for_replay(command),
    }
}

pub(super) fn run_with_idempotency(mut cli: Cli) -> ExitCode {
    let key = cli
        .idempotency_key
        .take()
        .expect("idempotency key checked by caller");
    let command_name = command_name_for_command(&cli.command);
    if !cli.json {
        print_usage_error(
            command_name,
            "idempotency_requires_json",
            "--idempotency-key requires --json so the replayed result has a stable shape",
            false,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    if !is_idempotent_mutation(&cli.command) {
        print_usage_error(
            command_name,
            "idempotency_unsupported",
            "--idempotency-key is only supported for non-dry-run mutations",
            true,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let Some(command_args) = command_args_for_replay(&cli.command) else {
        print_usage_error(
            command_name,
            "idempotency_unsupported",
            "--idempotency-key is not available for this command",
            true,
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let request_cwd = idempotency_cwd_for_request(&cli.command, &cli.socket);
    let request_hash = idempotency_request_hash(
        command_name,
        &command_args,
        &cli.socket,
        request_cwd.as_deref(),
    );
    let (store, workspace_id) = match open_idempotency_store() {
        Ok(opened) => opened,
        Err(message) => {
            print_runtime_error(command_name, generated_at(), &message, true);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let now = generated_at();
    let expires_at = idempotency_expires_at_from(&now);
    let pending_record = CommandIdempotencyRecord {
        workspace_id: workspace_id.clone(),
        idempotency_key: key.clone(),
        command: command_name_token(command_name).to_string(),
        request_hash: request_hash.clone(),
        result_json: "{}".to_string(),
        status: "pending".to_string(),
        created_at: now.clone(),
        updated_at: now,
        expires_at,
    };
    loop {
        match store.try_insert_command_idempotency_record(&pending_record) {
            Ok(true) => break,
            Ok(false) => match store.command_idempotency_record(&workspace_id, &key) {
                Ok(Some(record))
                    if idempotency_record_expired(&record, &pending_record.created_at) =>
                {
                    if let Err(error) = store.delete_command_idempotency_record(
                        &workspace_id,
                        &key,
                        &record.request_hash,
                    ) {
                        print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                        return ExitCode::from(EXIT_RUNTIME);
                    }
                }
                Ok(Some(record)) if record.request_hash != request_hash => {
                    print_idempotency_conflict(command_name, &key);
                    return ExitCode::from(EXIT_USAGE);
                }
                Ok(Some(record)) if record.status == "success" => {
                    print_replayed_result(&record.result_json);
                    return ExitCode::SUCCESS;
                }
                Ok(Some(_record)) => {
                    print_idempotency_in_progress(command_name, &key);
                    return ExitCode::from(EXIT_RUNTIME);
                }
                Ok(None) => {
                    print_runtime_error(
                        command_name,
                        generated_at(),
                        "idempotency reservation disappeared before execution",
                        true,
                    );
                    return ExitCode::from(EXIT_RUNTIME);
                }
                Err(error) => {
                    print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                    return ExitCode::from(EXIT_RUNTIME);
                }
            },
            Err(error) => {
                print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }

    let mut child_args = Vec::new();
    child_args.push("--json".to_string());
    if cli.socket != Path::new(DEFAULT_SOCKET) {
        child_args.push("--socket".to_string());
        child_args.push(cli.socket.display().to_string());
    }
    child_args.extend(command_args.iter().cloned());

    let output = match env::current_exe().and_then(|exe| {
        ProcessCommand::new(exe)
            .args(&child_args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }) {
        Ok(output) => output,
        Err(error) => {
            let _ = store.delete_command_idempotency_record(&workspace_id, &key, &request_hash);
            print_runtime_error(command_name, generated_at(), &error.to_string(), true);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    if output.status.success() {
        let result_json = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if serde_json::from_str::<serde_json::Value>(&result_json).is_ok() {
            let now = generated_at();
            let expires_at = idempotency_expires_at_from(&now);
            let record = CommandIdempotencyRecord {
                workspace_id: workspace_id.clone(),
                idempotency_key: key.clone(),
                command: command_name_token(command_name).to_string(),
                request_hash: request_hash.clone(),
                result_json,
                status: "success".to_string(),
                created_at: now.clone(),
                updated_at: now,
                expires_at,
            };
            if let Err(error) = store.finish_command_idempotency_record(&record) {
                print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    } else {
        let _ = store.delete_command_idempotency_record(&workspace_id, &key, &request_hash);
    }

    let _ = io::stdout().write_all(&output.stdout);
    let _ = io::stderr().write_all(&output.stderr);
    ExitCode::from(output.status.code().unwrap_or(i32::from(EXIT_RUNTIME)) as u8)
}

fn is_idempotent_mutation(command: &Command) -> bool {
    matches!(
        command,
        Command::Approve(_)
            | Command::Revoke(_)
            | Command::Recovery(recovery::RecoveryArgs::Create)
            | Command::Recovery(recovery::RecoveryArgs::Rotate)
            | Command::Recovery(recovery::RecoveryArgs::Revoke { .. })
            | Command::BootstrapSsh(_)
            | Command::Workon(_)
            | Command::WorkAccept(_)
            | Command::WorkDiscard(_)
            | Command::WorkRestore(_)
            | Command::WorkCleanup(_)
            | Command::AgentLeaseCreate(_)
            | Command::AgentPublish(_)
            | Command::AgentComplete(_)
            | Command::AgentBudget(_)
            | Command::Daemon(DaemonCommand::Install)
            | Command::Daemon(DaemonCommand::Restart)
            | Command::Daemon(DaemonCommand::Uninstall)
    )
}

fn open_idempotency_store() -> Result<(MetadataStore, WorkspaceId), String> {
    let store =
        MetadataStore::open(metadata_db_path_or_default()?).map_err(|error| error.to_string())?;
    let workspace_id = store
        .current_workspace()
        .map_err(|error| error.to_string())?
        .map(|workspace| workspace.id)
        .unwrap_or_else(|| WorkspaceId::new("ws_local_uninitialized"));
    Ok((store, workspace_id))
}

fn idempotency_request_hash(
    command: CommandName,
    args: &[String],
    socket: &Path,
    cwd: Option<&Path>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(command_name_token(command).as_bytes());
    hasher.update(&[0]);
    hasher.update(b"socket");
    hasher.update(&[0]);
    hasher.update(socket.display().to_string().as_bytes());
    hasher.update(&[0]);
    if let Some(cwd) = cwd {
        hasher.update(b"cwd");
        hasher.update(&[0]);
        hasher.update(cwd.display().to_string().as_bytes());
        hasher.update(&[0]);
    }
    for arg in args {
        hasher.update(arg.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn idempotency_cwd_for_request(command: &Command, socket: &Path) -> Option<PathBuf> {
    (command_has_cwd_relative_target(command) || socket_depends_on_cwd(socket))
        .then(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

pub(super) fn command_has_cwd_relative_target(command: &Command) -> bool {
    match command {
        Command::Workon(args) => path_depends_on_cwd(&args.project_path),
        Command::AgentLeaseCreate(args) => path_depends_on_cwd(&args.project_path),
        Command::BootstrapSsh(args) => {
            path_depends_on_cwd(&args.root)
                || args.artifact.as_deref().is_some_and(path_depends_on_cwd)
                || args.project.as_deref().is_some_and(path_depends_on_cwd)
        }
        _ => false,
    }
}

pub(super) fn path_depends_on_cwd(path: &str) -> bool {
    if path == "~" || path.starts_with("~/") {
        return false;
    }
    !PathBuf::from(path).is_absolute()
}

fn socket_depends_on_cwd(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }
    path.to_str().is_none_or(path_depends_on_cwd)
}

fn idempotency_expires_at_from(generated_at: &str) -> String {
    let base =
        time::OffsetDateTime::parse(generated_at, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    (base + time::Duration::days(7))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC timestamp should format")
}

fn idempotency_record_expired(record: &CommandIdempotencyRecord, generated_at: &str) -> bool {
    let Ok(expires_at) = time::OffsetDateTime::parse(
        &record.expires_at,
        &time::format_description::well_known::Rfc3339,
    ) else {
        return true;
    };
    let now =
        time::OffsetDateTime::parse(generated_at, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    expires_at <= now
}

fn print_replayed_result(result_json: &str) {
    match serde_json::from_str::<serde_json::Value>(result_json) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert("replayed".to_string(), serde_json::Value::Bool(true));
            }
            print_json(&value);
        }
        Err(_) => println!("{result_json}"),
    }
}

fn print_idempotency_conflict(command: CommandName, key: &str) {
    print_json(&CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at: generated_at(),
        status: CommandErrorStatus::UsageError,
        error: CommandError {
            code: "idempotency_conflict".to_string(),
            message: "idempotency key was already used for a different request".to_string(),
            recoverability: CommandRecoverability::UserAction,
            remediation: Some(
                "Use the same request with this key, or choose a new key.".to_string(),
            ),
            details: Some(serde_json::json!({ "idempotencyKey": key })),
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions: Vec::new(),
    });
}

fn print_idempotency_in_progress(command: CommandName, key: &str) {
    print_json(&CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at: generated_at(),
        status: CommandErrorStatus::Failed,
        error: CommandError {
            code: "idempotency_in_progress".to_string(),
            message: "idempotency key is already executing".to_string(),
            recoverability: CommandRecoverability::Retry,
            remediation: Some(
                "Retry the same request after the in-flight command finishes.".to_string(),
            ),
            details: Some(serde_json::json!({ "idempotencyKey": key })),
            retry_after_seconds: Some(1),
            correlation_id: None,
        },
        next_actions: Vec::new(),
    });
}

fn dry_run_plan(command: &Command) -> Option<(CommandName, String, Vec<String>, String)> {
    match command {
        Command::Approve(args) => Some((
            CommandName::Approve,
            args.request_id
                .clone()
                .unwrap_or_else(|| "first pending approval request".to_string()),
            vec!["approve a pending device trust request".to_string()],
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
                "optionally create a remote agent handoff".to_string(),
            ],
            "remote-mutation".to_string(),
        )),
        Command::Workon(args) => Some((
            CommandName::Workon,
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
        Command::AgentLeaseCreate(args) => Some((
            CommandName::AgentStart,
            args.project_path.clone(),
            vec![
                "create an agent lease".to_string(),
                "reserve hydration budget".to_string(),
                "optionally create a work view".to_string(),
            ],
            "workspace-metadata".to_string(),
        )),
        Command::AgentPublish(args) => Some((
            CommandName::AgentPublish,
            args.lease_id.clone(),
            vec!["publish agent output for review".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::AgentComplete(args) => Some((
            CommandName::AgentComplete,
            args.lease_id.clone(),
            vec!["mark agent lease complete".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::AgentBudget(args) => Some((
            CommandName::AgentBudget,
            args.lease_id.clone(),
            vec!["grant additional hydration budget".to_string()],
            "workspace-metadata".to_string(),
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

fn command_name_for_command(command: &Command) -> CommandName {
    match command {
        Command::Help(_) => CommandName::Help,
        Command::Version => CommandName::Version,
        Command::Contract => CommandName::Contract,
        Command::Login(_) => CommandName::Login,
        Command::Approve(_) => CommandName::Approve,
        Command::Revoke(_) => CommandName::Revoke,
        Command::Init(_) => CommandName::Init,
        Command::Prewarm(_) => CommandName::Prewarm,
        Command::Setup(_) => CommandName::Setup,
        Command::Status(_) => CommandName::Status,
        Command::Actions(_) => CommandName::Actions,
        Command::Tui(_) => CommandName::Tui,
        Command::Search(_) => CommandName::Search,
        Command::Symbols(_) => CommandName::Symbols,
        Command::Explain(_) => CommandName::Explain,
        Command::Devices(_) => CommandName::Devices,
        Command::Recovery(_) => CommandName::Recover,
        Command::Resolve(_) => CommandName::Resolve,
        Command::Events(_) => CommandName::Events,
        Command::Workon(_) => CommandName::Workon,
        Command::Work(_) => CommandName::Work,
        Command::WorkDiff(_) => CommandName::Diff,
        Command::Review(_) => CommandName::Review,
        Command::WorkAccept(_) => CommandName::Accept,
        Command::WorkDiscard(_) => CommandName::Discard,
        Command::WorkRestore(_) => CommandName::Restore,
        Command::WorkCleanup(_) => CommandName::Cleanup,
        Command::AgentLeaseCreate(_) => CommandName::AgentStart,
        Command::AgentContext(_) => CommandName::AgentContext,
        Command::AgentPrompt(_) => CommandName::AgentPrompt,
        Command::AgentPublish(_) => CommandName::AgentPublish,
        Command::AgentComplete(_) => CommandName::AgentComplete,
        Command::AgentBudget(_) => CommandName::AgentBudget,
        Command::BootstrapSsh(_) => CommandName::Connect,
        Command::Daemon(DaemonCommand::Start) => CommandName::DaemonStart,
        Command::Daemon(DaemonCommand::Stop) => CommandName::DaemonStop,
        Command::Daemon(DaemonCommand::Status) => CommandName::DaemonStatus,
        Command::Daemon(DaemonCommand::Install) => CommandName::DaemonInstall,
        Command::Daemon(DaemonCommand::Restart) => CommandName::DaemonRestart,
        Command::Daemon(DaemonCommand::Uninstall) => CommandName::DaemonUninstall,
        Command::DiagnosticsCollect => CommandName::DiagnosticsCollect,
        Command::UsageError { command, .. } => *command,
        Command::DevCloudSpike(_) | Command::CommandUsageError(_) | Command::Unknown(_) => {
            CommandName::Unknown
        }
    }
}

fn command_args_for_replay(command: &Command) -> Option<Vec<String>> {
    match command {
        Command::Approve(args) => {
            let mut argv = vec!["approve".to_string()];
            if let Some(request_id) = &args.request_id {
                argv.push(request_id.clone());
            }
            if args.yes {
                argv.push("--yes".to_string());
            }
            Some(argv)
        }
        Command::Revoke(args) => Some(vec!["revoke".to_string(), args.device_id.clone()]),
        Command::Recovery(recovery::RecoveryArgs::Create) => {
            Some(vec!["recover".to_string(), "create".to_string()])
        }
        Command::Recovery(recovery::RecoveryArgs::Rotate) => {
            Some(vec!["recover".to_string(), "rotate".to_string()])
        }
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
            if let Some(project) = &args.project {
                argv.extend(["--project".to_string(), project.clone()]);
            }
            if let Some(task) = &args.task {
                argv.extend(["--task".to_string(), task.clone()]);
            }
            if let Some(agent) = &args.agent {
                argv.extend(["--agent".to_string(), agent.clone()]);
            }
            Some(argv)
        }
        Command::Workon(args) => Some(vec![
            "workon".to_string(),
            args.project_path.clone(),
            args.name.clone(),
        ]),
        Command::WorkAccept(args) => Some(vec!["accept".to_string(), args.selector.clone()]),
        Command::WorkDiscard(args) => Some(vec!["discard".to_string(), args.selector.clone()]),
        Command::WorkRestore(args) => Some(vec!["restore".to_string(), args.selector.clone()]),
        Command::WorkCleanup(args) => {
            let mut argv = vec!["cleanup".to_string()];
            if args.apply {
                argv.push("--apply".to_string());
            }
            Some(argv)
        }
        Command::AgentLeaseCreate(args) => {
            let mut argv = vec![
                "agent".to_string(),
                "start".to_string(),
                args.project_path.clone(),
                "--task".to_string(),
                args.task.clone(),
                "--base".to_string(),
                agent_base_token(args.base).to_string(),
                "--hydrate-budget".to_string(),
                args.hydrate_budget_bytes.to_string(),
            ];
            if args.work_view {
                argv.push("--work-view".to_string());
            }
            Some(argv)
        }
        Command::AgentPublish(args) => Some(vec![
            "agent".to_string(),
            "publish".to_string(),
            "--lease".to_string(),
            args.lease_id.clone(),
        ]),
        Command::AgentComplete(args) => Some(vec![
            "agent".to_string(),
            "complete".to_string(),
            "--lease".to_string(),
            args.lease_id.clone(),
        ]),
        Command::AgentBudget(args) => Some(vec![
            "agent".to_string(),
            "budget".to_string(),
            "--lease".to_string(),
            args.lease_id.clone(),
            "--add".to_string(),
            args.add_bytes.to_string(),
        ]),
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

fn agent_base_token(base: bowline_core::commands::AgentLeaseBase) -> &'static str {
    match base {
        bowline_core::commands::AgentLeaseBase::LatestWorkspace => "latest-workspace",
        bowline_core::commands::AgentLeaseBase::LatestMain => "latest:main",
    }
}

fn shell_join(args: impl IntoIterator<Item = String>) -> String {
    args.into_iter()
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./:=@".contains(character))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}
