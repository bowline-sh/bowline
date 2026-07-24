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
        CommandName::Tui => parse_tui_command(values),
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
        CommandName::Doctor => parse_doctor_command(values),
        CommandName::Connect => parse_connect_command(values),
        CommandName::Unknown => Err(ParseError::Unknown(command.token().to_string())),
    }
}

fn parse_doctor_command(values: &crate::registry::ParsedValues) -> Result<Command, ParseError> {
    if let [unexpected, ..] = values.positionals() {
        return usage_error(
            CommandName::Doctor,
            format!("unexpected bowline doctor argument `{unexpected}`"),
        );
    }
    let engine = match values.option("--engine") {
        None | Some("manifest") => bowline_core::commands::DoctorEngine::Manifest,
        Some(other) => {
            return usage_error(
                CommandName::Doctor,
                format!("unsupported engine `{other}`; the only engine is `manifest`"),
            );
        }
    };
    Ok(Command::Doctor(DoctorArgs { engine }))
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
