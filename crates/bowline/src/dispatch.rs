use super::*;

pub(super) fn run(invocation: ParsedInvocation) -> ExitCode {
    let output_mode = resolve_output_mode(&invocation, io::stdout().is_terminal());
    let command = match invocation.command {
        Ok(command) => command,
        Err(error) => return render_parse_error(error, output_mode == OutputMode::Json),
    };
    let cli = Cli {
        json: output_mode == OutputMode::Json,
        quiet: output_mode == OutputMode::Quiet,
        socket: invocation.socket,
        dry_run: invocation.dry_run,
        command,
    };
    if cli.dry_run {
        return idempotency::print_dry_run(cli);
    }
    match cli.command {
        Command::Help(topic) => run_help(topic.as_deref(), cli.json),
        Command::Version => run_version(cli.json, &cli.socket),
        Command::Contract(mode) => run_contract(mode, cli.json),
        Command::Update(args) => print_update(args, cli.json),
        Command::Login(args) => print_login(args, cli.json, &cli.socket),
        Command::Logout => logout::print_logout(cli.json),
        Command::Approve(args) => print_approve(args, cli.json),
        Command::Deny(args) => print_deny(args, cli.json),
        Command::Revoke(args) => print_revoke(args, cli.json),
        Command::Setup(args) => print_setup(args, cli.json, &cli.socket),
        Command::Status(args) => print_status(args, cli.json, &cli.socket),
        Command::Tui(args) => print_tui(args, cli.json, &cli.socket),
        Command::SyncWait(args) => sync_wait::print_sync_wait(args, cli.json, &cli.socket),
        Command::SyncAttention => sync_attention::print_sync_attention(cli.json, &cli.socket),
        Command::SyncRetry(selector) => {
            sync_attention::print_sync_retry(selector, cli.json, &cli.socket)
        }
        Command::SyncDismiss(operation_id) => {
            sync_attention::print_sync_dismiss(operation_id, cli.json, &cli.socket)
        }
        Command::DebugClassify(args) => print_debug_classify(args, cli.json),
        Command::Devices(args) => print_devices(args, cli.json, cli.quiet),
        Command::Recovery(args) => print_recovery(args, cli.json),
        Command::Events(args) => print_events(args, cli.json, cli.quiet),
        Command::WorkCreate(args) => print_work_create(args, cli.json),
        Command::Work(args) => print_work(args, cli.json, cli.quiet),
        Command::WorkDiff(args) => print_work_diff(args, cli.json),
        Command::Review(args) => print_work_review(args, cli.json),
        Command::WorkAccept(args) => {
            print_work_lifecycle(work::WorkLifecycle::Accept, args, cli.json)
        }
        Command::WorkDiscard(args) => {
            print_work_lifecycle(work::WorkLifecycle::Discard, args, cli.json)
        }
        Command::WorkRestore(args) => {
            print_work_lifecycle(work::WorkLifecycle::Restore, args, cli.json)
        }
        Command::WorkCleanup(args) => print_work_cleanup(args, cli.json),
        Command::ForgetLocal(args) => print_forget_local(args, cli.json),
        Command::Archive(args) => print_archive(args, cli.json),
        Command::Purge(args) => print_purge(args, cli.json),
        Command::BootstrapSsh(args) => print_bootstrap_ssh(args, cli.json),
        Command::Daemon(DaemonCommand::Start) => print_daemon_start(&cli.socket, cli.json),
        Command::Daemon(DaemonCommand::Stop) => print_daemon_stop(&cli.socket, cli.json),
        Command::Daemon(DaemonCommand::Status) => run_daemon_status(&cli.socket, cli.json),
        Command::Daemon(DaemonCommand::Install) => {
            print_daemon_service_install(&cli.socket, cli.json)
        }
        Command::Daemon(DaemonCommand::Restart) => print_daemon_service_restart(cli.json),
        Command::Daemon(DaemonCommand::Uninstall) => print_daemon_service_uninstall(cli.json),
        Command::DiagnosticsCollect(selection) => {
            print_diagnostics_collect(selection, &cli.socket, cli.json)
        }
        Command::Doctor(args) => doctor::run_doctor(args, cli.json, &cli.socket),
    }
}

pub(super) fn resolve_output_mode(
    invocation: &ParsedInvocation,
    stdout_is_terminal: bool,
) -> OutputMode {
    if invocation.command.is_err() {
        return resolve_error_output_mode(invocation, stdout_is_terminal);
    }
    if invocation.quiet {
        return OutputMode::Quiet;
    }
    if invocation.json {
        return OutputMode::Json;
    }
    if invocation.human {
        return OutputMode::Human;
    }
    if matches!(invocation.command, Ok(Command::Tui(_))) || stdout_is_terminal {
        OutputMode::Human
    } else {
        OutputMode::Json
    }
}

fn resolve_error_output_mode(
    invocation: &ParsedInvocation,
    stdout_is_terminal: bool,
) -> OutputMode {
    if invocation.json {
        OutputMode::Json
    } else if invocation.human || stdout_is_terminal {
        OutputMode::Human
    } else {
        OutputMode::Json
    }
}

fn render_parse_error(error: ParseError, json: bool) -> ExitCode {
    let exit_code = match error {
        ParseError::Command(error) => print_command_usage_error(error, generated_at(), json),
        ParseError::Usage { command, message } => {
            print_usage_error(command, "usage_error", &message, json)
        }
        ParseError::Unknown(command) => {
            print_unknown_command(&command, json);
            CommandExitCode::UsageError
        }
    };
    exit_code.into()
}

fn run_help(topic: Option<&[String]>, json: bool) -> ExitCode {
    print_help(topic, json);
    ExitCode::SUCCESS
}

fn run_version(json: bool, socket: &Path) -> ExitCode {
    print_version(json, socket);
    ExitCode::SUCCESS
}

fn run_contract(mode: ContractMode, json: bool) -> ExitCode {
    print_contract(mode, json).into()
}

fn run_daemon_status(socket: &Path, json: bool) -> ExitCode {
    print_daemon_status(socket, json);
    ExitCode::SUCCESS
}
