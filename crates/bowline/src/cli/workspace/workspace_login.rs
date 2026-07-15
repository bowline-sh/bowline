use super::*;

pub(in crate::cli) fn parse_login_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(value) = values.positionals().first() {
        return command_usage_error(
            CommandName::Login,
            "usage_error",
            format!("unexpected bowline login argument `{value}`"),
            vec![RepairCommand::inspect(
                "Start login".to_string(),
                Some("bowline login".to_string()),
            )],
        );
    }
    Ok(Command::Login(login::LoginArgs {
        no_poll: values.flag("--no-poll"),
        headless: values.flag("--headless"),
    }))
}
