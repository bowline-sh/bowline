use super::*;

pub(super) fn parse_device_command(
    command_name: CommandName,
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(value) = values.positionals().first() {
        return unexpected_argument(command_name, "device", value);
    }
    let selection = parse_selection_only(command_name, "device", values).map_err(|error| *error)?;
    parse_device_action(
        command_name,
        selection,
        values.option("--request").map(str::to_string),
    )
}

fn parse_device_action(
    command_name: CommandName,
    selection: WorkspaceSelection,
    request_id: Option<String>,
) -> Result<Command, ParseError> {
    match command_name {
        CommandName::Devices => Ok(Command::Devices(devices::DevicesArgs::List { selection })),
        CommandName::DeviceRequest => Ok(Command::Devices(devices::DevicesArgs::Request {
            selection,
        })),
        CommandName::DeviceAccept => match request_id {
            Some(request_id) => Ok(Command::Devices(devices::DevicesArgs::Accept {
                selection,
                request_id,
            })),
            None => command_usage_error(
                command_name,
                "usage_error",
                "bowline device accept requires --request <id>".to_string(),
                devices_usage_actions(),
            ),
        },
        _ => Err(ParseError::Unknown(command_name.token().to_string())),
    }
}

/// `device key-status` is a purely local custody question, so it takes the
/// workspace id directly instead of resolving a workspace root: the caller that
/// needs it most is a bootstrap probe over SSH, which knows the id and has no
/// materialized root to select from yet.
pub(super) fn parse_device_key_status_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(value) = values.positionals().first() {
        return unexpected_argument(CommandName::DeviceKeyStatus, "device key-status", value);
    }
    let Some(workspace_id) = values
        .option("--workspace")
        .filter(|value| !value.is_empty())
    else {
        return command_usage_error(
            CommandName::DeviceKeyStatus,
            "usage_error",
            "bowline device key-status requires --workspace <id>".to_string(),
            devices_usage_actions(),
        );
    };
    Ok(Command::DeviceKeyStatus(devices::DeviceKeyStatusArgs {
        workspace_id: WorkspaceId::new(workspace_id),
    }))
}

pub(super) fn devices_usage_actions() -> Vec<RepairCommand> {
    vec![
        RepairCommand::inspect(
            "Inspect workspace status".to_string(),
            Some("bowline status".to_string()),
        ),
        RepairCommand::mutating(
            "Approve a pending device".to_string(),
            Some("bowline device approve --request <id>".to_string()),
        ),
    ]
}
