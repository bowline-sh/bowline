use super::*;

pub(super) fn parse_lease_join_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    if let Some(value) = values.positionals().first() {
        return unexpected_argument(CommandName::LeaseJoin, "lease join", value);
    }
    let Some(root) = values.option("--root") else {
        return usage_error(
            CommandName::LeaseJoin,
            "bowline lease join requires --root <path>",
        );
    };
    Ok(Command::LeaseJoin(lease::LeaseJoinArgs {
        root: root.to_string(),
        lease_id: values.option("--lease").map(str::to_string),
        runtime: values.option("--runtime").map(str::to_string),
        request_id: values.option("--request").map(str::to_string),
        token_env: values.option("--token-env").map(str::to_string),
        lease_json_env: values
            .option("--lease-json-env")
            .unwrap_or("BOWLINE_AGENT_LEASE_JSON")
            .to_string(),
    }))
}
