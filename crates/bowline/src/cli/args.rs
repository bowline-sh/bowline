use super::*;

pub(super) fn missing_value(
    command: CommandName,
    name: &str,
    flag: &str,
) -> Result<Command, ParseError> {
    command_usage_error(
        command,
        "usage_error",
        format!("bowline {name} {flag} requires a value"),
        trust_usage_actions(name),
    )
}

pub(super) fn unexpected_argument(
    command: CommandName,
    name: &str,
    value: &str,
) -> Result<Command, ParseError> {
    command_usage_error(
        command,
        "usage_error",
        format!("unexpected bowline {name} argument `{value}`"),
        trust_usage_actions(name),
    )
}

pub(super) fn trust_usage_actions(name: &str) -> Vec<RepairCommand> {
    vec![RepairCommand::inspect(
        format!("See {name} usage"),
        Some(format!("bowline help {name}")),
    )]
}

pub(super) fn workspace_root_error(
    command: CommandName,
    name: &str,
    error: WorkspaceRootSelectionError,
) -> Result<Command, ParseError> {
    match error {
        WorkspaceRootSelectionError::ExplicitRootRequired => command_usage_error(
            command,
            "usage_error",
            format!("bowline {name} requires --root <path>"),
            missing_root_actions(command, name),
        ),
        WorkspaceRootSelectionError::AmbiguousRoots(roots) => {
            let candidates = roots
                .iter()
                .map(|root| abbreviate_requested_path(root))
                .collect::<Vec<_>>();
            let next_actions = candidates
                .iter()
                .map(|root| {
                    root_action(command)(
                        format!("Run {name} for {root}"),
                        Some(format!("bowline {name} --root {}", shell_word(root))),
                    )
                })
                .collect::<Vec<_>>();
            command_usage_error(
                command,
                "ambiguous_root",
                format!(
                    "bowline {name} found multiple workspace roots; pass --root <path>: {}",
                    candidates.join(", ")
                ),
                next_actions,
            )
        }
        WorkspaceRootSelectionError::MetadataUnavailable(message) => command_usage_error(
            command,
            "usage_error",
            format!("bowline {name} could not infer a workspace root: {message}"),
            trust_usage_actions(name),
        ),
    }
}

fn missing_root_actions(command: CommandName, name: &str) -> Vec<RepairCommand> {
    vec![
        root_action(command)(
            format!("Run {name} with an explicit root"),
            Some(format!("bowline {name} --root <path>")),
        ),
        RepairCommand::inspect(
            format!("See {name} usage"),
            Some(format!("bowline help {name}")),
        ),
    ]
}

fn root_action(command: CommandName) -> fn(String, Option<String>) -> RepairCommand {
    match command {
        CommandName::Approve
        | CommandName::Deny
        | CommandName::Revoke
        | CommandName::DeviceRequest
        | CommandName::DeviceAccept => RepairCommand::mutating,
        _ => RepairCommand::inspect,
    }
}

pub(super) fn command_usage_error(
    command: CommandName,
    code: &'static str,
    message: String,
    next_actions: Vec<RepairCommand>,
) -> Result<Command, ParseError> {
    Err(command_usage_parse_error(
        command,
        code,
        message,
        next_actions,
    ))
}

pub(super) fn parse_error(result: Result<Command, ParseError>) -> ParseError {
    match result {
        Err(error) => error,
        Ok(_) => unreachable!("parser error helper returned a command"),
    }
}

fn command_usage_parse_error(
    command: CommandName,
    code: &'static str,
    message: String,
    next_actions: Vec<RepairCommand>,
) -> ParseError {
    ParseError::Command(CommandUsageError {
        command,
        code,
        message,
        next_actions,
    })
}
