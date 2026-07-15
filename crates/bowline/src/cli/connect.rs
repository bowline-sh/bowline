use super::*;

pub(super) fn parse_connect_command(
    values: &crate::registry::ParsedValues,
) -> Result<Command, ParseError> {
    let host = match values.positionals() {
        [host] => host,
        [] => {
            return command_usage_error(
                CommandName::Connect,
                "usage_error",
                "bowline connect requires a host".to_string(),
                connect_usage_actions("connect <host>"),
            );
        }
        [host, unexpected, ..] => {
            return command_usage_error(
                CommandName::Connect,
                "usage_error",
                format!("unexpected bowline connect argument `{unexpected}`"),
                connect_usage_actions(&format!("connect {host}")),
            );
        }
    };
    let root = values.option("--root").map(str::to_string);
    let artifact = values.option("--binary").map(str::to_string);
    let project = values.option("--project").map(str::to_string);
    let task = values.option("--task").map(str::to_string);
    let agent = values.option("--agent").map(str::to_string);

    if project.is_some() != task.is_some() {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect agent handoff requires both --project <project> and --task <task>"
                .to_string(),
            vec![RepairCommand::mutating(
                "Connect and start remote agent work".to_string(),
                Some(format!(
                    "bowline connect {host} --project <project> --task '<task>'"
                )),
            )],
        );
    }
    if agent.is_some() && project.is_none() {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect --agent requires --project <project> and --task <task>".to_string(),
            vec![RepairCommand::mutating(
                "Connect and start remote agent work".to_string(),
                Some(format!(
                    "bowline connect {host} --project <project> --task '<task>' --agent codex"
                )),
            )],
        );
    }

    Ok(Command::BootstrapSsh(bootstrap::BootstrapSshArgs {
        host: host.clone(),
        root: root
            .or_else(runtime::active_workspace_root)
            .unwrap_or_else(|| "~/Code".to_string()),
        artifact,
        project,
        task,
        agent,
    }))
}

fn connect_usage_actions(command: &str) -> Vec<RepairCommand> {
    vec![RepairCommand::mutating(
        "Connect a host".to_string(),
        Some(format!("bowline {command}")),
    )]
}
