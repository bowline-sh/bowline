use super::*;

pub(super) fn parse_events_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return unexpected_argument(CommandName::Events, "events", unexpected);
    }
    let limit = match values.option("--limit") {
        Some(raw_limit) => match raw_limit.parse::<u32>() {
            Ok(parsed) if (1..=bowline_local::status::MAX_EVENTS_LIMIT).contains(&parsed) => parsed,
            _ => {
                return usage_error(
                    CommandName::Events,
                    format!(
                        "expected --limit between 1 and {}",
                        bowline_local::status::MAX_EVENTS_LIMIT
                    ),
                );
            }
        },
        None => 50,
    };
    let selection = ParsedSelection {
        root: values.option("--root").map(str::to_string),
        project: values.option("--project").map(str::to_string),
    };
    let selection = match selection.finish(CommandName::Events, "events") {
        Ok(selection) => selection,
        Err(error) => return Err(*error),
    };

    Ok(Command::Events(EventsArgs { selection, limit }))
}

pub(super) fn parse_history_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let limit = match values.option("--limit") {
        Some(raw_limit) => match raw_limit.parse::<u32>() {
            Ok(parsed) if (1..=bowline_local::history::MAX_HISTORY_LIMIT).contains(&parsed) => {
                parsed
            }
            _ => {
                return usage_error(
                    CommandName::History,
                    format!(
                        "expected --limit between 1 and {}",
                        bowline_local::history::MAX_HISTORY_LIMIT
                    ),
                );
            }
        },
        None => bowline_local::history::DEFAULT_HISTORY_LIMIT,
    };
    let page_cursor = match values.option("--cursor") {
        Some(raw_cursor) => match raw_cursor.parse::<usize>() {
            Ok(parsed) => Some(parsed),
            Err(_) => return usage_error(CommandName::History, "expected numeric --cursor"),
        },
        None => None,
    };
    let since = values.option("--since").map(str::to_string);
    let until = values.option("--until").map(str::to_string);
    let from = values.option("--from").map(str::to_string);
    let to = values.option("--to").map(str::to_string);
    let has_diff_bounds = from.is_some() || to.is_some();

    let (target_path, mode) = match values.positionals() {
        [subcommand, target] if subcommand == "path" => (target.clone(), HistoryArgMode::Path),
        [subcommand, target] if subcommand == "diff" => {
            let (Some(from), Some(to)) = (from, to) else {
                return usage_error(
                    CommandName::History,
                    "bowline history diff requires --from <snapshot> --to <snapshot>",
                );
            };
            (target.clone(), HistoryArgMode::Diff { from, to })
        }
        [target] => (target.clone(), HistoryArgMode::Timeline),
        [] => (current_dir_string(), HistoryArgMode::Timeline),
        _ => {
            return usage_error(
                CommandName::History,
                "bowline history accepts [path], `path <path>`, or `diff <project> --from <snapshot> --to <snapshot>`",
            );
        }
    };
    if !matches!(mode, HistoryArgMode::Diff { .. }) && has_diff_bounds {
        return usage_error(
            CommandName::History,
            "--from and --to are only valid with `bowline history diff`",
        );
    }

    Ok(Command::History(HistoryArgs {
        target_path,
        mode,
        limit,
        cursor: page_cursor,
        since,
        until,
    }))
}

pub(super) fn parse_work_create_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let from = values.option("--from").map(str::to_string);
    match values.positionals() {
        [project_path, name] => Ok(Command::WorkCreate(work::WorkCreateArgs {
            project_path: project_path.to_string(),
            name: name.to_string(),
            from,
        })),
        [name] => Ok(Command::WorkCreate(work::WorkCreateArgs {
            project_path: current_dir_string(),
            name: name.to_string(),
            from,
        })),
        [] => command_usage_error(
            CommandName::WorkCreate,
            "usage_error",
            "bowline work create requires a name".to_string(),
            work_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::WorkCreate,
            "usage_error",
            "bowline work create accepts [project-path] <name> [--from <restore-point>]"
                .to_string(),
            work_usage_actions(),
        ),
    }
}

pub(super) fn parse_review_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    parse_work_selector_args(
        CommandName::Review,
        values,
        WorkSelectorParseMode {
            allow_paths: true,
            default_to_cwd: false,
        },
    )
    .map(Command::Review)
}

pub(super) fn parse_work_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return command_usage_error(
            CommandName::Work,
            "usage_error",
            format!("unexpected bowline work list argument `{unexpected}`"),
            work_usage_actions(),
        );
    }

    Ok(Command::Work(work::WorkListArgs {
        include_hidden: values.flag("--all"),
    }))
}

pub(super) fn parse_work_selector_command(
    command: CommandName,
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let allow_paths = matches!(command, CommandName::Diff | CommandName::Accept);
    match parse_work_selector_args(
        command,
        values,
        WorkSelectorParseMode {
            allow_paths,
            default_to_cwd: matches!(command, CommandName::Diff),
        },
    ) {
        Ok(args) => match command {
            CommandName::Diff => Ok(Command::WorkDiff(args)),
            CommandName::Accept => Ok(Command::WorkAccept(args)),
            CommandName::Discard => Ok(Command::WorkDiscard(args)),
            CommandName::Restore => Ok(Command::WorkRestore(args)),
            _ => command_usage_error(
                command,
                "usage_error",
                "unsupported work selector command".to_string(),
                work_usage_actions(),
            ),
        },
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
struct WorkSelectorParseMode {
    allow_paths: bool,
    default_to_cwd: bool,
}

fn parse_work_selector_args(
    command: CommandName,
    values: &crate::registry::ParsedValues,
    mode: WorkSelectorParseMode,
) -> Result<work::WorkSelectorArgs, ParseError> {
    let paths = values
        .options("--path")
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !mode.allow_paths && !paths.is_empty() {
        return Err(parse_error(command_usage_error(
            command,
            "usage_error",
            format!("bowline {} does not accept --path", command.token()),
            work_usage_actions(),
        )));
    }
    let selector = match values.positionals() {
        [selector] => selector.clone(),
        [_, ..] => {
            return Err(parse_error(command_usage_error(
                command,
                "usage_error",
                "work-view selector commands accept exactly one id or name".to_string(),
                work_usage_actions(),
            )));
        }
        [] if mode.default_to_cwd => current_dir_string(),
        [] => String::new(),
    };
    if selector.is_empty() {
        return Err(parse_error(command_usage_error(
            command,
            "usage_error",
            "missing selector".to_string(),
            work_usage_actions(),
        )));
    }
    Ok(work::WorkSelectorArgs { selector, paths })
}

pub(super) fn parse_agent_lease_create_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let mut base = agent::parse_base("latest-workspace").expect("default base is valid");
    if let Some(raw_base) = values.option("--base") {
        let Some(parsed) = agent::parse_base(raw_base) else {
            return command_usage_error(
                CommandName::AgentStart,
                "usage_error",
                "expected --base latest-workspace or --base latest:main".to_string(),
                agent_usage_actions(),
            );
        };
        base = parsed;
    }
    let project_path = match values.positionals() {
        [] => current_dir_string(),
        [project_path] => project_path.clone(),
        [_, unexpected, ..] => {
            return command_usage_error(
                CommandName::AgentStart,
                "usage_error",
                format!("unexpected bowline agent start argument `{unexpected}`"),
                agent_usage_actions(),
            );
        }
    };
    let Some(task) = values.option("--task").map(str::to_string) else {
        return command_usage_error(
            CommandName::AgentStart,
            "usage_error",
            "bowline agent start requires --task <task>".to_string(),
            agent_usage_actions(),
        );
    };
    let on_device = nonempty_agent_option(values, "--on")?;
    let remote_runtime = nonempty_agent_option(values, "--remote")?;
    let remote_root = nonempty_agent_option(values, "--remote-root")?;
    if on_device.is_some() && remote_runtime.is_some() {
        return command_usage_error(
            CommandName::AgentStart,
            "usage_error",
            "bowline agent start accepts either --on or --remote, not both".to_string(),
            agent_usage_actions(),
        );
    }

    Ok(Command::AgentLeaseCreate(agent::AgentLeaseCreateArgs {
        project_path,
        task,
        base,
        work_view: values.flag("--work-view") || on_device.is_some() || remote_runtime.is_some(),
        force_stale: values.flag("--force-stale"),
        on_device,
        remote_runtime,
        remote_root,
    }))
}

fn nonempty_agent_option(
    values: &crate::registry::ParsedValues,
    name: &str,
) -> Result<Option<String>, ParseError> {
    match values.option(name) {
        Some(value) if value.trim().is_empty() => Err(ParseError::Usage {
            command: CommandName::AgentStart,
            message: format!("{name} cannot be empty"),
        }),
        Some(value) => Ok(Some(value.to_string())),
        None => Ok(None),
    }
}

pub(super) fn parse_agent_start_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    parse_agent_lease_create_command(values)
}

pub(super) fn parse_agent_selector_command(
    command: CommandName,
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return command_usage_error(
            command,
            "usage_error",
            format!(
                "unexpected bowline {} argument `{unexpected}`",
                command.token()
            ),
            agent_usage_actions(),
        );
    }
    let Some(lease_id) = values.option("--lease").map(str::to_string) else {
        return command_usage_error(
            command,
            "usage_error",
            format!("bowline {} requires --lease <id>", command.token()),
            agent_usage_actions(),
        );
    };
    let args = agent::AgentLeaseSelectorArgs { lease_id };
    match command {
        CommandName::AgentContext => Ok(Command::AgentContext(args)),
        CommandName::AgentPrompt => Ok(Command::AgentPrompt(args)),
        CommandName::AgentComplete => Ok(Command::AgentComplete(args)),
        CommandName::AgentCancel => Ok(Command::AgentCancel(args)),
        _ => command_usage_error(
            command,
            "usage_error",
            "unsupported agent selector command".to_string(),
            agent_usage_actions(),
        ),
    }
}

pub(super) fn parse_agent_extend_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return unexpected_argument(CommandName::AgentExtend, "agent extend", unexpected);
    }
    let Some(lease_id) = values.option("--lease").map(str::to_string) else {
        return usage_error(
            CommandName::AgentExtend,
            "bowline agent extend requires --lease <id>",
        );
    };
    let Some(raw_hours) = values.option("--hours") else {
        return usage_error(
            CommandName::AgentExtend,
            "bowline agent extend requires --hours <1..168>",
        );
    };
    let Some(hours) = raw_hours.parse::<u16>().ok().filter(|value| {
        (1..=bowline_local::agents::MAX_AGENT_LEASE_EXTENSION_HOURS).contains(value)
    }) else {
        return usage_error(
            CommandName::AgentExtend,
            format!(
                "expected --hours between 1 and {}",
                bowline_local::agents::MAX_AGENT_LEASE_EXTENSION_HOURS
            ),
        );
    };
    Ok(Command::AgentExtend(agent::AgentLeaseExtendArgs {
        lease_id,
        hours,
    }))
}

pub(super) fn parse_agent_mcp_token_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return command_usage_error(
            CommandName::AgentMcpToken,
            "usage_error",
            format!("unexpected bowline agent mcp-token argument `{unexpected}`"),
            agent_usage_actions(),
        );
    }
    let Some(lease_id) = values.option("--lease").map(str::to_string) else {
        return command_usage_error(
            CommandName::AgentMcpToken,
            "usage_error",
            "bowline agent mcp-token requires --lease <id>".to_string(),
            agent_usage_actions(),
        );
    };
    // The MCP bridge exposes only read-only tools, so every token carries the
    // single Read scope (packet 065, Decision 2). No --grant selection remains.
    Ok(Command::AgentMcpToken(agent::AgentMcpTokenArgs {
        lease_id,
        grants: vec![bowline_core::commands::AgentMcpGrant::Read],
    }))
}

pub(super) fn parse_cleanup_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return command_usage_error(
            CommandName::Cleanup,
            "usage_error",
            format!("unexpected bowline work cleanup argument `{unexpected}`"),
            work_usage_actions(),
        );
    }

    Ok(Command::WorkCleanup(work::WorkCleanupArgs {
        apply: values.flag("--apply"),
    }))
}

pub(super) fn work_usage_actions() -> Vec<RepairCommand> {
    vec![
        RepairCommand::mutating(
            "Start a work view".to_string(),
            Some("bowline work create <name>".to_string()),
        ),
        RepairCommand::inspect(
            "Review work".to_string(),
            Some("bowline work review <target>".to_string()),
        ),
    ]
}

pub(super) fn agent_usage_actions() -> Vec<RepairCommand> {
    vec![
        RepairCommand::mutating(
            "Start agent work".to_string(),
            Some("bowline agent start <project> --task <task> --base latest-workspace".to_string()),
        ),
        RepairCommand::inspect(
            "Inspect an agent work".to_string(),
            Some("bowline agent context --lease <id>".to_string()),
        ),
    ]
}
