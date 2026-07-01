use super::*;

pub(super) fn run(cli: Cli) -> ExitCode {
    let parsed_error = matches!(
        cli.command,
        Command::CommandUsageError(_) | Command::UsageError { .. } | Command::Unknown(_)
    );
    if !parsed_error {
        if cli.dry_run {
            return idempotency::print_dry_run(cli);
        }
        if cli.idempotency_key.is_some() {
            return idempotency::run_with_idempotency(cli);
        }
    }
    match cli.command {
        Command::Help(topic) => {
            print_help(topic.as_deref(), cli.json);
            ExitCode::SUCCESS
        }
        Command::Version => {
            print_version(cli.json);
            ExitCode::SUCCESS
        }
        Command::Contract => {
            print_contract(cli.json);
            ExitCode::SUCCESS
        }
        Command::Login(args) => print_login(args, cli.json),
        Command::Approve(args) => print_approve(args, cli.json),
        Command::Revoke(args) => print_revoke(args, cli.json),
        Command::Init(args) => print_init(args, cli.json),
        Command::Prewarm(args) => print_prewarm(args, cli.json),
        Command::Setup(args) => print_setup(args, cli.json),
        Command::Status(args) => print_status(args, cli.json),
        Command::Actions(args) => print_actions(args, cli.json),
        Command::Tui(args) => print_tui(args, cli.json, &cli.socket),
        Command::Search(args) => print_search(args, cli.json),
        Command::Symbols(args) => print_symbols(args, cli.json),
        Command::Explain(args) => print_explain(args, cli.json),
        Command::Devices(args) => print_devices(args, cli.json),
        Command::Recovery(args) => print_recovery(args, cli.json),
        Command::Resolve(args) => print_resolve(args, cli.json, &cli.socket),
        Command::Events(args) => print_events(args, cli.json),
        Command::Workon(args) => print_workon(args, cli.json),
        Command::Work(args) => print_work(args, cli.json),
        Command::WorkDiff(args) => print_work_diff(args, cli.json),
        Command::Review(args) => print_work_review(args, cli.json),
        Command::WorkAccept(args) => print_work_lifecycle(CommandName::Accept, args, cli.json),
        Command::WorkDiscard(args) => print_work_lifecycle(CommandName::Discard, args, cli.json),
        Command::WorkRestore(args) => print_work_lifecycle(CommandName::Restore, args, cli.json),
        Command::WorkCleanup(args) => print_work_cleanup(args, cli.json),
        Command::AgentLeaseCreate(args) => print_agent_lease_create(args, cli.json),
        Command::AgentContext(args) => print_agent_context(args, cli.json),
        Command::AgentPrompt(args) => print_agent_prompt(args, cli.json),
        Command::AgentPublish(args) => {
            print_agent_tool_action(CommandName::AgentPublish, args, cli.json)
        }
        Command::AgentComplete(args) => {
            print_agent_tool_action(CommandName::AgentComplete, args, cli.json)
        }
        Command::AgentBudget(args) => print_agent_budget(args, cli.json),
        Command::BootstrapSsh(args) => print_bootstrap_ssh(args, cli.json),
        Command::DevCloudSpike(args) => print_dev_cloud_spike(args, cli.json),
        Command::Daemon(DaemonCommand::Start) => print_daemon_start(&cli.socket, cli.json),
        Command::Daemon(DaemonCommand::Stop) => print_daemon_stop(&cli.socket, cli.json),
        Command::Daemon(DaemonCommand::Status) => {
            print_daemon_status(&cli.socket, cli.json);
            ExitCode::SUCCESS
        }
        Command::Daemon(DaemonCommand::Install) => {
            print_daemon_service_install(&cli.socket, cli.json)
        }
        Command::Daemon(DaemonCommand::Restart) => print_daemon_service_restart(cli.json),
        Command::Daemon(DaemonCommand::Uninstall) => print_daemon_service_uninstall(cli.json),
        Command::DiagnosticsCollect => print_diagnostics_collect(&cli.socket, cli.json),
        Command::CommandUsageError(error) => {
            print_command_usage_error(error, generated_at(), cli.json);
            ExitCode::from(EXIT_USAGE)
        }
        Command::UsageError { command, message } => {
            print_usage_error(command, "usage_error", &message, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
        Command::Unknown(command) => {
            print_unknown_command(&command, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
    }
}
