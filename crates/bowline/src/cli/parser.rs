use super::*;

pub(super) fn parse_positionals(args: &[String]) -> Command {
    match args {
        [] => Command::Help(None),
        [command] if command == "help" => Command::Help(None),
        [command, rest @ ..] if command == "help" => Command::Help(Some(rest.to_vec())),
        [command] if command == "version" => Command::Version,
        [command] if command == "contract" => Command::Contract,
        [command, rest @ ..] if command == "login" => parse_login_command(rest),
        [command] if command == "logout" => Command::Logout,
        [command, rest @ ..] if command == "logout" => usage_error(
            CommandName::Logout,
            format!("unexpected bowline logout argument `{}`", rest[0]),
        ),
        [command, rest @ ..] if command == "approve" => parse_approve_command(rest),
        [command, rest @ ..] if command == "revoke" => parse_revoke_command(rest),
        [command, rest @ ..] if command == "recover" => parse_recovery_command(rest),
        [command, rest @ ..] if command == "init" => parse_init_command(rest),
        [command, rest @ ..] if command == "setup" => parse_setup_command(rest),
        [command, rest @ ..] if command == "prewarm" => parse_prewarm_command(rest),
        [command, rest @ ..] if command == "status" => parse_status_command(rest),
        [command, rest @ ..] if command == "actions" => parse_actions_command(rest),
        [command, rest @ ..] if command == "tui" => parse_tui_command(rest),
        [command, rest @ ..] if command == "search" => parse_search_command(rest),
        [command, rest @ ..] if command == "symbols" => parse_symbols_command(rest),
        [command, rest @ ..] if command == "explain" => parse_explain_command(rest),
        [command, rest @ ..] if command == "devices" => parse_devices_command(rest),
        [command, rest @ ..] if command == "events" => parse_events_command(rest),
        [command, rest @ ..] if command == "workon" => parse_workon_command(rest),
        [command, rest @ ..] if command == "work" => parse_work_command(rest),
        [command, rest @ ..] if command == "review" => parse_review_command(rest),
        [command, rest @ ..] if command == "diff" => parse_work_selector_command(
            CommandName::Diff,
            rest,
            "bowline diff requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "accept" => parse_work_selector_command(
            CommandName::Accept,
            rest,
            "bowline accept requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "discard" => parse_work_selector_command(
            CommandName::Discard,
            rest,
            "bowline discard requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "restore" => parse_work_selector_command(
            CommandName::Restore,
            rest,
            "bowline restore requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "cleanup" => parse_cleanup_command(rest),
        [command, subcommand, rest @ ..] if command == "dev" && subcommand == "cloud-spike" => {
            parse_dev_cloud_spike_command(rest)
        }
        [command, ..] if command == "dev" => {
            usage_error(CommandName::Unknown, "expected `bowline dev cloud-spike`")
        }
        [command, rest @ ..] if command == "connect" => parse_connect_command(rest),
        [command, ..] if command == "bootstrap" => {
            usage_error(CommandName::Connect, "expected `bowline connect <host>`")
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "start" => {
            parse_agent_start_command(rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "context" => {
            parse_agent_selector_command(CommandName::AgentContext, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "prompt" => {
            parse_agent_selector_command(CommandName::AgentPrompt, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "publish" => {
            parse_agent_selector_command(CommandName::AgentPublish, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "complete" => {
            parse_agent_selector_command(CommandName::AgentComplete, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "budget" => {
            parse_agent_budget_command(rest)
        }
        [command, ..] if command == "agent" => usage_error(
            CommandName::AgentStart,
            "expected `bowline agent start ...`, `bowline agent context ...`, `bowline agent prompt ...`, `bowline agent publish ...`, `bowline agent complete ...`, or `bowline agent budget ...`",
        ),
        [command, rest @ ..] if command == "resolve" => parse_resolve_command(rest),
        [command, subcommand] if command == "daemon" && subcommand == "start" => {
            Command::Daemon(DaemonCommand::Start)
        }
        [command, subcommand] if command == "daemon" && subcommand == "stop" => {
            Command::Daemon(DaemonCommand::Stop)
        }
        [command, subcommand] if command == "daemon" && subcommand == "status" => {
            Command::Daemon(DaemonCommand::Status)
        }
        [command, subcommand] if command == "daemon" && subcommand == "install" => {
            Command::Daemon(DaemonCommand::Install)
        }
        [command, subcommand] if command == "daemon" && subcommand == "restart" => {
            Command::Daemon(DaemonCommand::Restart)
        }
        [command, subcommand] if command == "daemon" && subcommand == "uninstall" => {
            Command::Daemon(DaemonCommand::Uninstall)
        }
        [command, ..] if command == "daemon" => usage_error(
            CommandName::DaemonStatus,
            "expected `bowline daemon start`, `bowline daemon stop`, `bowline daemon status`, `bowline daemon install`, `bowline daemon restart`, or `bowline daemon uninstall`",
        ),
        [command, subcommand] if command == "diagnostics" && subcommand == "collect" => {
            Command::DiagnosticsCollect
        }
        [command, ..] if command == "diagnostics" => usage_error(
            CommandName::DiagnosticsCollect,
            "expected `bowline diagnostics collect`",
        ),
        [command, ..] => Command::Unknown(command.clone()),
    }
}

pub(super) fn parse_dev_cloud_spike_command(args: &[String]) -> Command {
    let mut provider = CloudSpikeProvider::Fake;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--provider" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Unknown, "missing value for --provider");
                };
                provider = match value.as_str() {
                    "fake" => CloudSpikeProvider::Fake,
                    "hosted" => CloudSpikeProvider::Hosted,
                    _ => {
                        return usage_error(
                            CommandName::Unknown,
                            "expected --provider fake or --provider hosted",
                        );
                    }
                };
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Unknown,
                    format!("unknown bowline dev cloud-spike option `{flag}`"),
                );
            }
            value => {
                return usage_error(
                    CommandName::Unknown,
                    format!("unexpected bowline dev cloud-spike argument `{value}`"),
                );
            }
        }
    }

    Command::DevCloudSpike(CloudSpikeArgs { provider })
}
