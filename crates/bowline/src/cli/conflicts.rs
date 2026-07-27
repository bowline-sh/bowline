use bowline_core::commands::ConflictAction;

use super::*;
use crate::conflict_commands::{ConflictsArgs, ResolveArgs};
use crate::registry::ParsedValues;

pub(super) fn parse_conflicts_command(values: &ParsedValues) -> Result<Command, ParseError> {
    let selection = parse_selection_only(CommandName::Conflicts, "conflicts", values)
        .map_err(|error| *error)?;
    Ok(Command::Conflicts(ConflictsArgs { selection }))
}

pub(super) fn parse_resolve_command(values: &ParsedValues) -> Result<Command, ParseError> {
    let action = conflict_action(values)?;
    // The registry enforces the positional's presence before construction runs.
    let aside_path = values
        .positionals()
        .first()
        .expect("registry enforces <aside-path> on bowline resolve")
        .clone();
    let selection = ParsedSelection {
        root: values.option("--root").map(str::to_string),
        project: None,
    };
    let selection = selection
        .finish(CommandName::Resolve, "resolve")
        .map_err(|error| *error)?;
    Ok(Command::Resolve(ResolveArgs {
        root: selection.root,
        aside_path,
        action,
    }))
}

/// Exactly one of the three verbs. Defaulting would silently pick a side of the
/// user's own work, so absence and ambiguity are both usage errors.
fn conflict_action(values: &ParsedValues) -> Result<ConflictAction, ParseError> {
    let chosen = [
        ("--keep-local", ConflictAction::KeepLocal),
        ("--take-remote", ConflictAction::TakeRemote),
        ("--diff", ConflictAction::Diff),
    ]
    .into_iter()
    .filter(|(flag, _)| values.flag(flag))
    .map(|(_, action)| action)
    .collect::<Vec<_>>();

    match chosen.as_slice() {
        [action] => Ok(*action),
        [] => Err(parse_error(command_usage_error(
            CommandName::Resolve,
            "missing_required_option",
            "bowline resolve needs one of --keep-local, --take-remote, or --diff".to_string(),
            vec![RepairCommand::inspect(
                "Compare both versions first".to_string(),
                Some("bowline resolve <aside-path> --diff".to_string()),
            )],
        ))),
        _ => Err(parse_error(usage_error(
            CommandName::Resolve,
            "bowline resolve takes only one of --keep-local, --take-remote, or --diff",
        ))),
    }
}
