use super::*;

use bowline_core::commands::RecoveryCommandAction;
use bowline_core::ids::RecoveryEnvelopeId;

/// The action comes from the registry — one spec per `recover <action>` — so
/// this only binds the envelope-id positional the spec already validated.
pub(super) fn parse_recovery_command(
    action: RecoveryCommandAction,
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let envelope_id = |action: RecoveryCommandAction| match values.positionals() {
        [envelope_id] => Ok(RecoveryEnvelopeId::new(envelope_id.clone())),
        _ => Err(command_usage_parse_error(
            CommandName::Recover,
            "missing_envelope_id",
            format!(
                "bowline recover {} requires <envelope-id>; Recovery Key words are read from stdin",
                action_token(action)
            ),
            recovery_usage_actions(),
        )),
    };
    let args = match action {
        RecoveryCommandAction::Status => recovery::RecoveryArgs::Status,
        RecoveryCommandAction::Create => recovery::RecoveryArgs::Create,
        RecoveryCommandAction::Rotate => recovery::RecoveryArgs::Rotate,
        RecoveryCommandAction::Verify => recovery::RecoveryArgs::Verify {
            envelope_id: envelope_id(action)?,
        },
        RecoveryCommandAction::Revoke => recovery::RecoveryArgs::Revoke {
            envelope_id: envelope_id(action)?,
        },
        RecoveryCommandAction::Use => recovery::RecoveryArgs::Use {
            envelope_id: envelope_id(action)?,
        },
    };
    Ok(Command::Recovery(args))
}

fn action_token(action: RecoveryCommandAction) -> &'static str {
    match action {
        RecoveryCommandAction::Status => "status",
        RecoveryCommandAction::Create => "create",
        RecoveryCommandAction::Verify => "verify",
        RecoveryCommandAction::Rotate => "rotate",
        RecoveryCommandAction::Revoke => "revoke",
        RecoveryCommandAction::Use => "use",
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
