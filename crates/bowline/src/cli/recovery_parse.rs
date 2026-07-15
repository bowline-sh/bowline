use super::*;

pub(super) fn parse_recovery_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    match values.positionals() {
        [subcommand] if subcommand == "status" => {
            Ok(Command::Recovery(recovery::RecoveryArgs::Status))
        }
        [subcommand] if subcommand == "create" => {
            Ok(Command::Recovery(recovery::RecoveryArgs::Create))
        }
        [subcommand, envelope_id] if subcommand == "verify" => {
            Ok(Command::Recovery(recovery::RecoveryArgs::Verify {
                envelope_id: envelope_id.to_string(),
            }))
        }
        [subcommand, _, words @ ..] if subcommand == "verify" && !words.is_empty() => {
            command_usage_error(
                CommandName::Recover,
                "usage_error",
                "Recovery Key words must be provided on stdin, not argv".to_string(),
                recovery_usage_actions(),
            )
        }
        [subcommand] if subcommand == "rotate" => {
            Ok(Command::Recovery(recovery::RecoveryArgs::Rotate))
        }
        [subcommand, envelope_id] if subcommand == "revoke" => {
            Ok(Command::Recovery(recovery::RecoveryArgs::Revoke {
                envelope_id: envelope_id.to_string(),
            }))
        }
        [subcommand, envelope_id] if subcommand == "use" => {
            Ok(Command::Recovery(recovery::RecoveryArgs::Use {
                envelope_id: envelope_id.to_string(),
            }))
        }
        [subcommand, _, words @ ..] if subcommand == "use" && !words.is_empty() => {
            command_usage_error(
                CommandName::Recover,
                "usage_error",
                "Recovery Key words must be provided on stdin, not argv".to_string(),
                recovery_usage_actions(),
            )
        }
        _ => command_usage_error(
            CommandName::Recover,
            "usage_error",
            "expected `bowline recover status|create|verify <envelope-id>|rotate|revoke <envelope-id>|use <envelope-id>`; Recovery Key words are read from stdin".to_string(),
            recovery_usage_actions(),
        ),
    }
}

pub(super) fn recovery_usage_actions() -> Vec<RepairCommand> {
    vec![
        RepairCommand::inspect(
            "Show Recovery Key status".to_string(),
            Some("bowline recover status".to_string()),
        ),
        RepairCommand::mutating(
            "Create a Recovery Key".to_string(),
            Some("bowline recover create".to_string()),
        ),
    ]
}
