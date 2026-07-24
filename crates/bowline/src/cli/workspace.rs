use super::*;
use crate::registry::ParsedValues;

mod workspace_login;

pub(super) use workspace_login::parse_login_command;

pub(super) fn parse_approve_command(values: &ParsedValues) -> Result<Command, ParseError> {
    reject_positionals(values, CommandName::Approve, "device approve")?;
    let selector = trust_selector(values, CommandName::Approve, "device approve")?;
    let selection = parsed_selection(values)
        .finish_for_trust(CommandName::Approve, "device approve")
        .map_err(|error| *error)?;
    Ok(Command::Approve(ApproveArgs {
        selection,
        selector,
        yes: values.flag("--yes"),
    }))
}

pub(super) fn parse_deny_command(values: &ParsedValues) -> Result<Command, ParseError> {
    reject_positionals(values, CommandName::Deny, "device deny")?;
    let selector = trust_selector(values, CommandName::Deny, "device deny")?;
    let selection = parsed_selection(values)
        .finish_for_trust(CommandName::Deny, "device deny")
        .map_err(|error| *error)?;
    Ok(Command::Deny(ApproveArgs {
        selection,
        selector,
        yes: true,
    }))
}

pub(super) fn parse_revoke_command(values: &ParsedValues) -> Result<Command, ParseError> {
    reject_positionals(values, CommandName::Revoke, "device revoke")?;
    let Some(device_id) = values.option("--device") else {
        return command_usage_error(
            CommandName::Revoke,
            "usage_error",
            "bowline device revoke requires --device <id>".to_string(),
            trust_usage_actions("device revoke"),
        );
    };
    let selection = parsed_selection(values)
        .finish_for_trust(CommandName::Revoke, "device revoke")
        .map_err(|error| *error)?;
    Ok(Command::Revoke(RevokeArgs {
        selection,
        device_id: device_id.to_string(),
    }))
}

pub(super) fn parse_setup_command(values: &ParsedValues) -> Result<Command, ParseError> {
    let root = values.option("--root").map(str::to_string);
    let yes = values.flag("--yes");
    let project_path = optional_positional(values, CommandName::Setup, "setup")?;

    match (project_path, root, yes) {
        (Some(project_path), None, yes) => Ok(Command::Setup(SetupArgs {
            mode: SetupMode::Project { project_path, yes },
        })),
        (Some(_), Some(_), _) => command_usage_error(
            CommandName::Setup,
            "usage_error",
            "bowline setup <path> cannot be combined with --root <path>".to_string(),
            vec![RepairCommand::mutating(
                "Set up this machine".to_string(),
                Some("bowline setup --root <path>".to_string()),
            )],
        ),
        (None, root, false) => Ok(Command::Setup(SetupArgs {
            mode: SetupMode::Machine { root },
        })),
        (None, _, true) => command_usage_error(
            CommandName::Setup,
            "usage_error",
            "bowline setup --yes requires a project path".to_string(),
            vec![RepairCommand::mutating(
                "Set up the current project".to_string(),
                Some("bowline setup . --yes".to_string()),
            )],
        ),
    }
}

pub(super) fn parse_status_command(values: &ParsedValues) -> Result<Command, ParseError> {
    reject_positionals(values, CommandName::Status, "status")?;
    let selection = parsed_selection(values)
        .finish(CommandName::Status, "status")
        .map_err(|error| *error)?;
    Ok(Command::Status(StatusArgs {
        selection,
        watch: values.flag("--watch"),
        include_all: values.flag("--all"),
    }))
}

pub(super) fn parse_tui_command(values: &ParsedValues) -> Result<Command, ParseError> {
    let selection =
        parse_selection_only(CommandName::Tui, "tui", values).map_err(|error| *error)?;
    Ok(Command::Tui(TuiArgs { selection }))
}

pub(super) fn parse_forget_local_command(values: &ParsedValues) -> Result<Command, ParseError> {
    let project_path = required_positional(
        values,
        CommandName::ForgetLocal,
        "forget-local",
        "bowline forget-local requires a project",
    )?;
    Ok(Command::ForgetLocal(ForgetLocalArgs {
        project_path,
        yes: values.flag("--yes"),
    }))
}

pub(super) fn parse_archive_command(values: &ParsedValues) -> Result<Command, ParseError> {
    let project_path = required_positional(
        values,
        CommandName::Archive,
        "archive",
        "bowline archive requires a project",
    )?;
    Ok(Command::Archive(ArchiveArgs {
        project_path,
        restore: values.flag("--restore"),
    }))
}

pub(super) fn parse_purge_command(values: &ParsedValues) -> Result<Command, ParseError> {
    let project_path = required_positional(
        values,
        CommandName::Purge,
        "purge",
        "bowline purge requires a project",
    )?;
    let cancel = values.flag("--cancel");
    let grace_days = values
        .option("--grace")
        .map(|value| {
            value.parse::<u32>().map_err(|_| ParseError::Usage {
                command: CommandName::Purge,
                message: "purge --grace must be a whole number of days".to_string(),
            })
        })
        .transpose()?;
    if cancel && grace_days.is_some() {
        return usage_error(
            CommandName::Purge,
            "bowline purge accepts only one of --grace <days> or --cancel",
        );
    }
    Ok(Command::Purge(PurgeArgs {
        project_path,
        cancel,
        grace_days,
    }))
}

#[derive(Default)]
pub(super) struct ParsedSelection {
    pub(super) root: Option<String>,
    pub(super) project: Option<String>,
}

impl ParsedSelection {
    pub(super) fn finish(
        self,
        command: CommandName,
        name: &str,
    ) -> Result<WorkspaceSelection, Box<ParseError>> {
        let resolved = WorkspaceRootSelection::current(self.root)
            .resolve()
            .map_err(|error| Box::new(parse_error(workspace_root_error(command, name, error))))?;
        Ok(WorkspaceSelection {
            root: resolved.root,
            project: self.project,
        })
    }

    pub(super) fn finish_for_trust(
        self,
        command: CommandName,
        name: &str,
    ) -> Result<WorkspaceSelection, Box<ParseError>> {
        let resolved = WorkspaceRootSelection::current(self.root)
            .resolve_for_trust()
            .map_err(|error| Box::new(parse_error(workspace_root_error(command, name, error))))?;
        Ok(WorkspaceSelection {
            root: resolved.root,
            project: self.project,
        })
    }
}

pub(super) fn parse_selection_only(
    command: CommandName,
    name: &str,
    values: &ParsedValues,
) -> Result<WorkspaceSelection, Box<ParseError>> {
    reject_positionals(values, command, name).map_err(Box::new)?;
    parsed_selection(values).finish(command, name)
}

fn parsed_selection(values: &ParsedValues) -> ParsedSelection {
    ParsedSelection {
        root: values.option("--root").map(str::to_string),
        project: values.option("--project").map(str::to_string),
    }
}

fn trust_selector(
    values: &ParsedValues,
    command: CommandName,
    name: &str,
) -> Result<TrustRequestSelector, ParseError> {
    match (values.option("--request"), values.option("--code")) {
        (Some(request), None) => Ok(TrustRequestSelector::Request(request.to_string())),
        (None, Some(code)) => Ok(TrustRequestSelector::Code(code.to_string())),
        _ => Err(parse_error(trust_selector_error(command, name))),
    }
}

fn trust_selector_error(command: CommandName, name: &str) -> Result<Command, ParseError> {
    command_usage_error(
        command,
        "usage_error",
        format!("bowline {name} requires exactly one of --request <id> or --code <short-code>"),
        trust_usage_actions(name),
    )
}

fn reject_positionals(
    values: &ParsedValues,
    command: CommandName,
    name: &str,
) -> Result<(), ParseError> {
    match values.positionals().first() {
        Some(value) => Err(parse_error(unexpected_argument(command, name, value))),
        None => Ok(()),
    }
}

fn optional_positional(
    values: &ParsedValues,
    command: CommandName,
    name: &str,
) -> Result<Option<String>, ParseError> {
    match values.positionals() {
        [] => Ok(None),
        [value] => Ok(Some(value.to_string())),
        [_, unexpected, ..] => Err(parse_error(unexpected_argument(command, name, unexpected))),
    }
}

fn required_positional(
    values: &ParsedValues,
    command: CommandName,
    name: &str,
    missing_message: &str,
) -> Result<String, ParseError> {
    optional_positional(values, command, name)?.map_or_else(
        || Err(parse_error(usage_error(command, missing_message))),
        Ok,
    )
}

pub(super) fn parse_sync_wait_command(values: &ParsedValues) -> Result<Command, ParseError> {
    use bowline_core::introspection::WorkspaceReadiness;
    if let Some(unexpected) = values.positionals().first() {
        return usage_error(
            CommandName::Unknown,
            format!("unexpected bowline sync wait argument `{unexpected}`"),
        );
    }
    let Some(workspace_id) = values
        .option("--workspace")
        .filter(|value| !value.is_empty())
    else {
        return usage_error(
            CommandName::Unknown,
            "bowline sync wait requires --workspace <id>",
        );
    };
    let Some(state) = values.option("--state") else {
        return usage_error(
            CommandName::Unknown,
            "bowline sync wait requires --state <state>",
        );
    };
    let Some(target_state) = WorkspaceReadiness::from_token(state) else {
        return usage_error(
            CommandName::Unknown,
            format!(
                "unknown bowline sync wait --state `{state}`; expected one of {}",
                readiness_state_list()
            ),
        );
    };
    let timeout = match values.option("--timeout") {
        Some(raw) => match crate::sync_wait::parse_timeout(raw) {
            Ok(duration) => duration,
            Err(message) => return usage_error(CommandName::Unknown, message),
        },
        None => crate::sync_wait::DEFAULT_TIMEOUT,
    };
    Ok(Command::SyncWait(SyncWaitArgs {
        workspace_id: workspace_id.to_string(),
        target_state,
        timeout,
    }))
}

pub(super) fn parse_sync_retry_command(values: &ParsedValues) -> Result<Command, ParseError> {
    use crate::sync_attention::RetrySelector;
    if let Some(unexpected) = values.positionals().first() {
        return usage_error(
            CommandName::Unknown,
            format!("unexpected bowline sync retry argument `{unexpected}`"),
        );
    }
    let operation = values
        .option("--operation")
        .filter(|value| !value.is_empty());
    let all = values.flag("--all");
    // Require exactly one selector so a caller can never re-queue the whole lane
    // by forgetting the operation id.
    match (operation, all) {
        (Some(incident), false) => Ok(Command::SyncRetry(RetrySelector::Incident(
            incident.to_string(),
        ))),
        (None, true) => Ok(Command::SyncRetry(RetrySelector::All)),
        _ => usage_error(
            CommandName::Unknown,
            "bowline sync retry requires exactly one of --operation <id> or --all",
        ),
    }
}

pub(super) fn parse_sync_dismiss_command(values: &ParsedValues) -> Result<Command, ParseError> {
    if let Some(unexpected) = values.positionals().first() {
        return usage_error(
            CommandName::Unknown,
            format!("unexpected bowline sync dismiss argument `{unexpected}`"),
        );
    }
    let Some(operation) = values
        .option("--operation")
        .filter(|value| !value.is_empty())
    else {
        return usage_error(
            CommandName::Unknown,
            "bowline sync dismiss requires --operation <id>",
        );
    };
    Ok(Command::SyncDismiss(operation.to_string()))
}

fn readiness_state_list() -> String {
    bowline_core::introspection::WorkspaceReadiness::ALL
        .iter()
        .map(|state| state.token())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn parse_debug_classify_command(values: &ParsedValues) -> Result<Command, ParseError> {
    match values.positionals() {
        [path] => Ok(Command::DebugClassify(DebugClassifyArgs {
            path: path.to_string(),
        })),
        _ => usage_error(
            CommandName::Unknown,
            "expected `bowline debug classify <path>`",
        ),
    }
}
