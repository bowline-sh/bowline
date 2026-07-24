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
