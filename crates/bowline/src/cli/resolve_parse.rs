use super::*;

pub(super) fn parse_resolve_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let project_or_path = match values.positionals() {
        [] => current_dir_string(),
        [value] => value.to_string(),
        [_, unexpected, ..] => {
            return usage_error(
                CommandName::Resolve,
                format!("unexpected bowline resolve argument `{unexpected}`"),
            );
        }
    };
    let diff = values.option("--diff").map(str::to_string);
    let agent = values
        .option("--agent")
        .map(|value| {
            resolve::parse_agent(value).ok_or_else(|| {
                parse_error(usage_error(
                    CommandName::Resolve,
                    "expected --agent codex, --agent claude, or --agent cursor",
                ))
            })
        })
        .transpose()?;
    let decision = match (values.option("--accept"), values.option("--reject")) {
        (Some(conflict), None) => Some(resolve::ResolveDecision::Accept(conflict.to_string())),
        (None, Some(conflict)) => Some(resolve::ResolveDecision::Reject(conflict.to_string())),
        (None, None) => None,
        (Some(_), Some(_)) => {
            return usage_error(
                CommandName::Resolve,
                "bowline resolve accepts only one --accept or --reject action",
            );
        }
    };
    if diff.is_some() && decision.is_some() {
        return usage_error(
            CommandName::Resolve,
            "bowline resolve --diff cannot be combined with --accept or --reject",
        );
    }

    Ok(Command::Resolve(resolve::ResolveArgs {
        project_or_path,
        copy_prompt: values.flag("--copy-prompt"),
        tui: values.flag("--tui"),
        diff,
        agent,
        decision,
    }))
}
