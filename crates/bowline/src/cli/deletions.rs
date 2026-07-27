use super::*;
use crate::deletion_commands::DeletionsArgs;
use crate::registry::ParsedValues;

/// `--confirm` is the whole grammar: the batch is whatever the engine is
/// refusing, so there is nothing to select and nothing to default.
pub(super) fn parse_deletions_command(values: &ParsedValues) -> Result<Command, ParseError> {
    Ok(Command::Deletions(DeletionsArgs {
        confirm: values.flag("--confirm"),
    }))
}
