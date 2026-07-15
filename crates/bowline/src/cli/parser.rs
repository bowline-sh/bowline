use super::*;

pub(super) fn construct_command(
    command: CommandName,
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    match command {
        CommandName::Help => Ok(Command::Help(
            (!values.positionals().is_empty()).then(|| values.positionals().to_vec()),
        )),
        CommandName::Version => no_argument_command(command, values, Command::Version),
        CommandName::Contract => parse_contract_command(values),
        CommandName::Mcp => parse_mcp_command(values),
        CommandName::Update => parse_update_command(values),
        CommandName::Login => parse_login_command(values),
        CommandName::Logout => no_argument_command(command, values, Command::Logout),
        CommandName::Approve => parse_approve_command(values),
        CommandName::Deny => parse_deny_command(values),
        CommandName::Revoke => parse_revoke_command(values),
        CommandName::Recover => parse_recovery_command(values),
        CommandName::Setup => parse_setup_command(values),
        CommandName::Status => parse_status_command(values),
        CommandName::Devices | CommandName::DeviceRequest | CommandName::DeviceAccept => {
            parse_device_command(command, values)
        }
        CommandName::Events => parse_events_command(values),
        CommandName::History => parse_history_command(values),
        CommandName::Tui => parse_tui_command(values),
        CommandName::Resolve => parse_resolve_command(values),
        CommandName::ForgetLocal => parse_forget_local_command(values),
        CommandName::Archive => parse_archive_command(values),
        CommandName::Purge => parse_purge_command(values),
        CommandName::WorkCreate => parse_work_create_command(values),
        CommandName::Review => parse_review_command(values),
        CommandName::Work => parse_work_command(values),
        CommandName::Diff | CommandName::Accept | CommandName::Discard | CommandName::Restore => {
            parse_work_selector_command(command, values)
        }
        CommandName::Cleanup => parse_cleanup_command(values),
        CommandName::AgentContext
        | CommandName::AgentPrompt
        | CommandName::AgentComplete
        | CommandName::AgentCancel => parse_agent_selector_command(command, values),
        CommandName::AgentStart => parse_agent_start_command(values),
        CommandName::AgentExtend => parse_agent_extend_command(values),
        CommandName::AgentMcpToken => parse_agent_mcp_token_command(values),
        CommandName::LeaseJoin => parse_lease_join_command(values),
        CommandName::DaemonStart => {
            no_argument_command(command, values, Command::Daemon(DaemonCommand::Start))
        }
        CommandName::DaemonStop => {
            no_argument_command(command, values, Command::Daemon(DaemonCommand::Stop))
        }
        CommandName::DaemonStatus => {
            no_argument_command(command, values, Command::Daemon(DaemonCommand::Status))
        }
        CommandName::DaemonInstall => {
            no_argument_command(command, values, Command::Daemon(DaemonCommand::Install))
        }
        CommandName::DaemonRestart => {
            no_argument_command(command, values, Command::Daemon(DaemonCommand::Restart))
        }
        CommandName::DaemonUninstall => {
            no_argument_command(command, values, Command::Daemon(DaemonCommand::Uninstall))
        }
        CommandName::DiagnosticsCollect => {
            match parse_selection_only(command, command.token(), values) {
                Ok(selection) => Ok(Command::DiagnosticsCollect(selection)),
                Err(error) => Err(*error),
            }
        }
        CommandName::Connect => parse_connect_command(values),
        CommandName::Handoff => parse_handoff_command(values),
        CommandName::Unknown => Err(ParseError::Unknown(command.token().to_string())),
    }
}

fn no_argument_command(
    command_name: CommandName,
    values: &crate::registry::ParsedValues,
    command: Command,
) -> Result<Command, ParseError> {
    match values.positionals() {
        [] => Ok(command),
        [unexpected, ..] => usage_error(
            command_name,
            format!(
                "unexpected bowline {} argument `{unexpected}`",
                command_name.token()
            ),
        ),
    }
}

fn parse_contract_command(values: &crate::registry::ParsedValues) -> Result<Command, ParseError> {
    let topic = values.positionals();
    if values.flag("--summary") {
        if topic.is_empty() {
            Ok(Command::Contract(ContractMode::Summary))
        } else {
            usage_error(
                CommandName::Contract,
                format!(
                    "--summary cannot be combined with contract topic `{}`",
                    topic.join(" ")
                ),
            )
        }
    } else if topic.is_empty() {
        Ok(Command::Contract(ContractMode::Full))
    } else {
        Ok(Command::Contract(ContractMode::Topic(topic.to_vec())))
    }
}

fn parse_mcp_command(values: &crate::registry::ParsedValues) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return usage_error(
            CommandName::Mcp,
            format!("unexpected bowline mcp argument `{unexpected}`"),
        );
    }

    Ok(Command::Mcp(McpArgs {
        lease_id: values.option("--lease").map(str::to_string),
        token_file: values.option("--token-file").map(str::to_string),
    }))
}

pub(super) fn parse_handoff_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let Some(target) = values.positionals().first() else {
        return command_usage_error(
            CommandName::Handoff,
            "usage_error",
            "bowline handoff requires a target".to_string(),
            handoff_usage_actions(),
        );
    };
    if let Some(unexpected) = values.positionals().get(1) {
        return command_usage_error(
            CommandName::Handoff,
            "usage_error",
            format!("unexpected bowline handoff argument `{unexpected}`"),
            handoff_usage_actions(),
        );
    }
    let agent = match values.option("--agent") {
        Some(value) => match parse_handoff_agent(value) {
            Some(parsed) => Some(parsed),
            None => {
                return command_usage_error(
                    CommandName::Handoff,
                    "unsupported_agent",
                    format!("bowline handoff --agent must be codex or claude, got `{value}`"),
                    handoff_usage_actions(),
                );
            }
        },
        None => None,
    };
    let session = values.option("--session").map(str::to_string);
    let prompt = values.option("--prompt").map(str::to_string);
    let prompt_file = values.option("--prompt-file").map(str::to_string);
    let project = values.option("--project").map(str::to_string);

    if prompt.is_some() && prompt_file.is_some() {
        return command_usage_error(
            CommandName::Handoff,
            "usage_error",
            "bowline handoff cannot combine --prompt and --prompt-file".to_string(),
            handoff_usage_actions(),
        );
    }
    if session.is_some() && (prompt.is_some() || prompt_file.is_some()) {
        return command_usage_error(
            CommandName::Handoff,
            "usage_error",
            "bowline handoff cannot combine --session with prompt launch mode".to_string(),
            handoff_usage_actions(),
        );
    }

    Ok(Command::Handoff(HandoffArgs {
        target: target.to_string(),
        agent,
        session,
        prompt,
        prompt_file,
        project,
    }))
}

fn parse_handoff_agent(value: &str) -> Option<HandoffAgent> {
    match value {
        "codex" => Some(HandoffAgent::Codex),
        "claude" => Some(HandoffAgent::Claude),
        _ => None,
    }
}

fn handoff_usage_actions() -> Vec<RepairCommand> {
    vec![RepairCommand::inspect(
        "See handoff usage",
        Some("bowline help handoff".to_string()),
    )]
}

pub(super) fn parse_update_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return usage_error(
            CommandName::Update,
            format!("unexpected bowline update argument `{unexpected}`"),
        );
    }

    Ok(Command::Update(UpdateArgs {
        check: values.flag("--check"),
        version: values.option("--version").map(str::to_string),
    }))
}
