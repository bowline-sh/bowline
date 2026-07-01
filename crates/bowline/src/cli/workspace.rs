use super::*;

pub(super) fn parse_login_command(args: &[String]) -> Command {
    let mut root = None;
    let mut headless = false;
    let mut no_poll = false;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    return command_usage_error(
                        CommandName::Login,
                        "usage_error",
                        "bowline login --root requires a path".to_string(),
                        vec![SafeAction {
                            label: "Log in and prepare ~/Code".to_string(),
                            command: Some("bowline login".to_string()),
                        }],
                    );
                };
                root = Some(value.to_string());
                index += 2;
            }
            "--headless" => {
                headless = true;
                index += 1;
            }
            "--no-poll" => {
                no_poll = true;
                index += 1;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Login,
                    "usage_error",
                    format!("unknown bowline login option `{flag}`"),
                    vec![SafeAction {
                        label: "Start login".to_string(),
                        command: Some("bowline login".to_string()),
                    }],
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Login,
                    "usage_error",
                    format!("unexpected bowline login argument `{value}`"),
                    vec![SafeAction {
                        label: "Start login".to_string(),
                        command: Some("bowline login".to_string()),
                    }],
                );
            }
        }
    }

    Command::Login(login::LoginArgs {
        root,
        no_poll,
        headless,
    })
}

pub(super) fn parse_approve_command(args: &[String]) -> Command {
    let mut request_id = None;
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" => yes = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Approve,
                    "usage_error",
                    format!("unknown bowline approve option `{flag}`"),
                    vec![SafeAction {
                        label: "Approve pending device".to_string(),
                        command: Some("bowline approve".to_string()),
                    }],
                );
            }
            value if request_id.is_none() => request_id = Some(value.to_string()),
            value => {
                return command_usage_error(
                    CommandName::Approve,
                    "usage_error",
                    format!("unexpected bowline approve argument `{value}`"),
                    vec![SafeAction {
                        label: "Approve pending device".to_string(),
                        command: Some("bowline approve".to_string()),
                    }],
                );
            }
        }
    }
    Command::Approve(ApproveArgs { request_id, yes })
}

pub(super) fn parse_revoke_command(args: &[String]) -> Command {
    match args {
        [device_id] => Command::Revoke(RevokeArgs {
            device_id: device_id.to_string(),
        }),
        [] => command_usage_error(
            CommandName::Revoke,
            "usage_error",
            "bowline revoke requires a device id".to_string(),
            vec![SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            }],
        ),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Revoke,
            "usage_error",
            format!("unknown bowline revoke option `{flag}`"),
            vec![SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            }],
        ),
        _ => command_usage_error(
            CommandName::Revoke,
            "usage_error",
            "bowline revoke accepts exactly one device id".to_string(),
            vec![SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            }],
        ),
    }
}

pub(super) fn parse_init_command(args: &[String]) -> Command {
    match args {
        [] => Command::Init(InitArgs { root: None }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Init,
            "usage_error",
            format!("unknown bowline login option `{flag}`"),
            vec![SafeAction {
                label: "Log in and choose a root".to_string(),
                command: Some("bowline login --root <path>".to_string()),
            }],
        ),
        [root] => Command::Init(InitArgs {
            root: Some(root.to_string()),
        }),
        _ => command_usage_error(
            CommandName::Init,
            "usage_error",
            "bowline login accepts at most one root path".to_string(),
            vec![SafeAction {
                label: "Log in and choose a root".to_string(),
                command: Some("bowline login --root <path>".to_string()),
            }],
        ),
    }
}

pub(super) fn parse_prewarm_command(args: &[String]) -> Command {
    let mut approve_setup = false;
    let mut project_path = None;

    for arg in args {
        match arg.as_str() {
            "--approve-setup" => approve_setup = true,
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Prewarm,
                    format!("unknown bowline setup option `{flag}`"),
                );
            }
            value if project_path.is_none() => project_path = Some(value.to_string()),
            _ => {
                return usage_error(
                    CommandName::Prewarm,
                    "bowline setup accepts exactly one path",
                );
            }
        }
    }

    match project_path {
        Some(project_path) => Command::Prewarm(PrewarmArgs {
            project_path,
            approve_setup,
        }),
        None => usage_error(
            CommandName::Prewarm,
            "bowline setup requires a project path",
        ),
    }
}

pub(super) fn parse_setup_command(args: &[String]) -> Command {
    let mut yes = false;
    let mut project_path = None;

    for arg in args {
        match arg.as_str() {
            "--yes" => yes = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Setup,
                    "usage_error",
                    format!("unknown bowline setup option `{flag}`"),
                    vec![SafeAction {
                        label: "Prepare the current project".to_string(),
                        command: Some("bowline setup".to_string()),
                    }],
                );
            }
            value if project_path.is_none() => project_path = Some(value.to_string()),
            value => {
                return command_usage_error(
                    CommandName::Setup,
                    "usage_error",
                    format!("unexpected bowline setup argument `{value}`"),
                    vec![SafeAction {
                        label: "Prepare the current project".to_string(),
                        command: Some("bowline setup".to_string()),
                    }],
                );
            }
        }
    }

    Command::Setup(SetupArgs { project_path, yes })
}

pub(super) fn parse_status_command(args: &[String]) -> Command {
    let mut watch = false;
    let mut workspace = false;
    let mut path = None;

    for arg in args {
        match arg.as_str() {
            "--watch" => watch = true,
            "--workspace" | "--all" => workspace = true,
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Status,
                    format!("unknown bowline status option `{flag}`"),
                );
            }
            value if path.is_none() => path = Some(value.to_string()),
            _ => {
                return usage_error(
                    CommandName::Status,
                    "bowline status accepts at most one path",
                );
            }
        }
    }

    Command::Status(StatusArgs {
        path,
        watch,
        workspace,
    })
}

pub(super) fn parse_actions_command(args: &[String]) -> Command {
    let mut workspace = false;
    let mut path = None;

    for arg in args {
        match arg.as_str() {
            "--workspace" => workspace = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Actions,
                    "usage_error",
                    format!("unknown bowline status option `{flag}`"),
                    vec![SafeAction {
                        label: "Inspect workspace status".to_string(),
                        command: Some("bowline status [path] --json".to_string()),
                    }],
                );
            }
            value if path.is_none() => path = Some(value.to_string()),
            _ => {
                return command_usage_error(
                    CommandName::Actions,
                    "usage_error",
                    "bowline status accepts at most one path".to_string(),
                    vec![SafeAction {
                        label: "Inspect workspace status".to_string(),
                        command: Some("bowline status [path] --json".to_string()),
                    }],
                );
            }
        }
    }

    Command::Actions(ActionsArgs { path, workspace })
}

pub(super) fn parse_tui_command(args: &[String]) -> Command {
    match args {
        [] => Command::Tui(TuiArgs { path: None }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Tui,
            "usage_error",
            format!("unknown bowline tui option `{flag}`"),
            vec![SafeAction {
                label: "Open the terminal UI".to_string(),
                command: Some("bowline tui [path]".to_string()),
            }],
        ),
        [path] => Command::Tui(TuiArgs {
            path: Some(path.to_string()),
        }),
        _ => command_usage_error(
            CommandName::Tui,
            "usage_error",
            "bowline tui accepts at most one path".to_string(),
            vec![SafeAction {
                label: "Open the terminal UI".to_string(),
                command: Some("bowline tui [path]".to_string()),
            }],
        ),
    }
}

pub(super) fn parse_search_command(args: &[String]) -> Command {
    let mut values = Vec::new();
    let mut limit = DEFAULT_EXPLORATION_LIMIT;
    let mut cursor = None;
    let mut path_prefix = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --limit requires a number",
                    );
                };
                let Some(parsed) = parse_exploration_limit(value) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --limit must be between 1 and 100",
                    );
                };
                limit = parsed;
                index += 2;
            }
            "--cursor" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --cursor requires a cursor",
                    );
                };
                let Some(parsed) = parse_exploration_cursor(value) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --cursor must be opaque cursor format v1:<offset> with offset at most 10000",
                    );
                };
                cursor = Some(parsed);
                index += 2;
            }
            "--path-prefix" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --path-prefix requires a prefix",
                    );
                };
                path_prefix = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Search,
                    "usage_error",
                    format!("unknown bowline search option `{flag}`"),
                    vec![SafeAction {
                        label: "Search a project".to_string(),
                        command: Some("bowline search <query> [path]".to_string()),
                    }],
                );
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    match values.as_slice() {
        [query] => Command::Search(SearchArgs {
            query: query.to_string(),
            path: None,
            limit,
            cursor,
            path_prefix,
        }),
        [query, path] => Command::Search(SearchArgs {
            query: query.to_string(),
            path: Some(path.to_string()),
            limit,
            cursor,
            path_prefix,
        }),
        [] => command_usage_error(
            CommandName::Search,
            "usage_error",
            "bowline search requires a query".to_string(),
            vec![SafeAction {
                label: "Search a project".to_string(),
                command: Some("bowline search <query> [path]".to_string()),
            }],
        ),
        _ => command_usage_error(
            CommandName::Search,
            "usage_error",
            "bowline search accepts <query> and an optional path".to_string(),
            vec![SafeAction {
                label: "Search a project".to_string(),
                command: Some("bowline search <query> [path]".to_string()),
            }],
        ),
    }
}

pub(super) fn parse_symbols_command(args: &[String]) -> Command {
    let mut values = Vec::new();
    let mut limit = DEFAULT_EXPLORATION_LIMIT;
    let mut cursor = None;
    let mut path_prefix = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --limit requires a number",
                    );
                };
                let Some(parsed) = parse_exploration_limit(value) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --limit must be between 1 and 100",
                    );
                };
                limit = parsed;
                index += 2;
            }
            "--cursor" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --cursor requires a cursor",
                    );
                };
                let Some(parsed) = parse_exploration_cursor(value) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --cursor must be opaque cursor format v1:<offset> with offset at most 10000",
                    );
                };
                cursor = Some(parsed);
                index += 2;
            }
            "--path-prefix" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --path-prefix requires a prefix",
                    );
                };
                path_prefix = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Symbols,
                    "usage_error",
                    format!("unknown bowline symbols option `{flag}`"),
                    vec![SafeAction {
                        label: "Look up symbols".to_string(),
                        command: Some("bowline symbols <name> [path]".to_string()),
                    }],
                );
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    match values.as_slice() {
        [query] => Command::Symbols(SymbolsArgs {
            query: query.to_string(),
            path: None,
            limit,
            cursor,
            path_prefix,
        }),
        [query, path] => Command::Symbols(SymbolsArgs {
            query: query.to_string(),
            path: Some(path.to_string()),
            limit,
            cursor,
            path_prefix,
        }),
        [] => command_usage_error(
            CommandName::Symbols,
            "usage_error",
            "bowline symbols requires a name".to_string(),
            vec![SafeAction {
                label: "Look up symbols".to_string(),
                command: Some("bowline symbols <name> [path]".to_string()),
            }],
        ),
        _ => command_usage_error(
            CommandName::Symbols,
            "usage_error",
            "bowline symbols accepts <name> and an optional path".to_string(),
            vec![SafeAction {
                label: "Look up symbols".to_string(),
                command: Some("bowline symbols <name> [path]".to_string()),
            }],
        ),
    }
}

pub(super) fn parse_exploration_limit(value: &str) -> Option<usize> {
    let limit = value.parse::<usize>().ok()?;
    (1..=MAX_EXPLORATION_LIMIT)
        .contains(&limit)
        .then_some(limit)
}

pub(super) fn parse_exploration_cursor(value: &str) -> Option<usize> {
    let offset = value.strip_prefix("v1:")?.parse::<usize>().ok()?;
    (offset <= MAX_EXPLORATION_CURSOR_OFFSET).then_some(offset)
}

pub(super) fn exploration_usage_error(command: CommandName, message: &str) -> Command {
    command_usage_error(
        command,
        "usage_error",
        message.to_string(),
        vec![SafeAction {
            label: "Inspect command help".to_string(),
            command: Some(format!(
                "bowline help {} --json",
                match command {
                    CommandName::Search => "search",
                    CommandName::Symbols => "symbols",
                    _ => "help",
                }
            )),
        }],
    )
}

pub(super) fn parse_explain_command(args: &[String]) -> Command {
    match args {
        [] => command_usage_error(
            CommandName::Explain,
            "usage_error",
            "bowline explain requires a path".to_string(),
            vec![SafeAction {
                label: "Explain a path".to_string(),
                command: Some("bowline explain <path>".to_string()),
            }],
        ),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Explain,
            "usage_error",
            format!("unknown bowline explain option `{flag}`"),
            vec![SafeAction {
                label: "Explain a path".to_string(),
                command: Some("bowline explain <path>".to_string()),
            }],
        ),
        [path] => Command::Explain(ExplainArgs {
            path: path.to_string(),
        }),
        _ => command_usage_error(
            CommandName::Explain,
            "usage_error",
            "bowline explain accepts exactly one path".to_string(),
            vec![SafeAction {
                label: "Explain a path".to_string(),
                command: Some("bowline explain <path>".to_string()),
            }],
        ),
    }
}
