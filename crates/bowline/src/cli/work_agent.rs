use super::*;

pub(super) fn parse_devices_command(args: &[String]) -> Command {
    match args {
        [] => Command::Devices(devices::DevicesArgs::List),
        [subcommand] if subcommand == "list" => Command::Devices(devices::DevicesArgs::List),
        [subcommand] if subcommand == "request" => {
            Command::Devices(devices::DevicesArgs::Request { root: None })
        }
        [subcommand, flag, root] if subcommand == "request" && flag == "--root" => {
            Command::Devices(devices::DevicesArgs::Request {
                root: Some(root.to_string()),
            })
        }
        [subcommand, flag] if subcommand == "request" && flag == "--root" => command_usage_error(
            CommandName::Devices,
            "usage_error",
            "bowline login --root requires a path".to_string(),
            devices_usage_actions(),
        ),
        [subcommand, flag, ..] if subcommand == "request" && flag.starts_with("--") => {
            command_usage_error(
                CommandName::Devices,
                "usage_error",
                format!("unknown bowline login option `{flag}`"),
                devices_usage_actions(),
            )
        }
        [subcommand, request_id] if subcommand == "approve" => {
            Command::Devices(devices::DevicesArgs::Approve {
                request_id: request_id.to_string(),
            })
        }
        [subcommand, request_id] if subcommand == "accept" => {
            Command::Devices(devices::DevicesArgs::Accept {
                request_id: request_id.to_string(),
            })
        }
        [subcommand, request_id] if subcommand == "deny" => {
            Command::Devices(devices::DevicesArgs::Deny {
                request_id: request_id.to_string(),
            })
        }
        [subcommand, device_id] if subcommand == "revoke" => {
            Command::Devices(devices::DevicesArgs::Revoke {
                device_id: device_id.to_string(),
            })
        }
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Devices,
            "usage_error",
            format!("unknown bowline trust option `{flag}`"),
            devices_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Devices,
            "usage_error",
            "expected `bowline approve [request]`, `bowline revoke <device>`, or `bowline login --root <path>`"
                .to_string(),
            devices_usage_actions(),
        ),
    }
}

pub(super) fn parse_recovery_command(args: &[String]) -> Command {
    match args {
        [] => Command::Recovery(recovery::RecoveryArgs::Status),
        [subcommand] if subcommand == "status" => Command::Recovery(recovery::RecoveryArgs::Status),
        [subcommand] if subcommand == "create" => Command::Recovery(recovery::RecoveryArgs::Create),
        [subcommand, envelope_id] if subcommand == "verify" => {
            Command::Recovery(recovery::RecoveryArgs::Verify {
                envelope_id: envelope_id.to_string(),
            })
        }
        [subcommand, _, words @ ..] if subcommand == "verify" && !words.is_empty() => {
            command_usage_error(
                CommandName::Recover,
                "usage_error",
                "Recovery Key words must be provided on stdin, not argv".to_string(),
                recovery_usage_actions(),
            )
        }
        [subcommand] if subcommand == "rotate" => Command::Recovery(recovery::RecoveryArgs::Rotate),
        [subcommand, envelope_id] if subcommand == "revoke" => {
            Command::Recovery(recovery::RecoveryArgs::Revoke {
                envelope_id: envelope_id.to_string(),
            })
        }
        [subcommand, envelope_id] if subcommand == "use" => {
            Command::Recovery(recovery::RecoveryArgs::Use {
                envelope_id: envelope_id.to_string(),
            })
        }
        [subcommand, _, words @ ..] if subcommand == "use" && !words.is_empty() => {
            command_usage_error(
                CommandName::Recover,
                "usage_error",
                "Recovery Key words must be provided on stdin, not argv".to_string(),
                recovery_usage_actions(),
            )
        }
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Recover,
            "usage_error",
            format!("unknown bowline recover option `{flag}`"),
            recovery_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Recover,
            "usage_error",
            "expected `bowline recover [status|create|verify <envelope-id>|rotate|revoke <envelope-id>|use <envelope-id>]`; Recovery Key words are read from stdin".to_string(),
            recovery_usage_actions(),
        ),
    }
}

pub(super) fn parse_events_command(args: &[String]) -> Command {
    let mut workspace = false;
    let mut limit = 50;
    let mut path = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => workspace = true,
            "--limit" => {
                let Some(raw_limit) = iter.next() else {
                    return usage_error(CommandName::Events, "missing value for --limit");
                };
                match raw_limit.parse::<u32>() {
                    Ok(parsed)
                        if (1..=bowline_local::status::MAX_EVENTS_LIMIT).contains(&parsed) =>
                    {
                        limit = parsed;
                    }
                    _ => {
                        return usage_error(
                            CommandName::Events,
                            format!(
                                "expected --limit between 1 and {}",
                                bowline_local::status::MAX_EVENTS_LIMIT
                            ),
                        );
                    }
                }
            }
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Events,
                    format!("unknown bowline status --watch option `{flag}`"),
                );
            }
            value if path.is_none() => path = Some(value.to_string()),
            _ => {
                return usage_error(
                    CommandName::Events,
                    "bowline status --watch accepts at most one path",
                );
            }
        }
    }

    Command::Events(EventsArgs {
        path,
        workspace,
        limit,
    })
}

pub(super) fn parse_workon_command(args: &[String]) -> Command {
    match args {
        [project_path, name] => Command::Workon(work::WorkonArgs {
            project_path: project_path.to_string(),
            name: name.to_string(),
        }),
        [name] => Command::Workon(work::WorkonArgs {
            project_path: current_dir_string(),
            name: name.to_string(),
        }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Workon,
            "usage_error",
            format!("unknown bowline workon option `{flag}`"),
            work_usage_actions(),
        ),
        [] => command_usage_error(
            CommandName::Workon,
            "usage_error",
            "bowline workon requires a name".to_string(),
            work_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Workon,
            "usage_error",
            "bowline workon accepts [project-path] <name>".to_string(),
            work_usage_actions(),
        ),
    }
}

pub(super) fn parse_review_command(args: &[String]) -> Command {
    match args {
        [] => Command::Review(work::WorkSelectorArgs {
            selector: current_dir_string(),
        }),
        [target] => Command::Review(work::WorkSelectorArgs {
            selector: target.to_string(),
        }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Review,
            "usage_error",
            format!("unknown bowline review option `{flag}`"),
            work_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Review,
            "usage_error",
            "bowline review accepts at most one target".to_string(),
            work_usage_actions(),
        ),
    }
}

pub(super) fn parse_work_command(args: &[String]) -> Command {
    let mut include_hidden = false;
    for arg in args {
        match arg.as_str() {
            "--all" => include_hidden = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Work,
                    "usage_error",
                    format!("unknown bowline work option `{flag}`"),
                    work_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Work,
                    "usage_error",
                    format!("unexpected bowline work argument `{value}`"),
                    work_usage_actions(),
                );
            }
        }
    }

    Command::Work(work::WorkListArgs { include_hidden })
}

pub(super) fn parse_work_selector_command(
    command: CommandName,
    args: &[String],
    missing_message: &'static str,
) -> Command {
    match args {
        [selector] => {
            let args = work::WorkSelectorArgs {
                selector: selector.to_string(),
            };
            match command {
                CommandName::Diff => Command::WorkDiff(args),
                CommandName::Accept => Command::WorkAccept(args),
                CommandName::Discard => Command::WorkDiscard(args),
                CommandName::Restore => Command::WorkRestore(args),
                _ => unreachable!("unsupported work selector command"),
            }
        }
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            command,
            "usage_error",
            format!(
                "unknown bowline {} option `{flag}`",
                command_name_token(command)
            ),
            work_usage_actions(),
        ),
        [] if matches!(
            command,
            CommandName::Accept | CommandName::Discard | CommandName::Restore
        ) =>
        {
            let args = work::WorkSelectorArgs {
                selector: current_dir_string(),
            };
            match command {
                CommandName::Accept => Command::WorkAccept(args),
                CommandName::Discard => Command::WorkDiscard(args),
                CommandName::Restore => Command::WorkRestore(args),
                _ => unreachable!("unsupported work selector command"),
            }
        }
        [] => command_usage_error(
            command,
            "usage_error",
            missing_message.to_string(),
            work_usage_actions(),
        ),
        _ => command_usage_error(
            command,
            "usage_error",
            "work-view selector commands accept exactly one id or name".to_string(),
            work_usage_actions(),
        ),
    }
}

pub(super) fn parse_agent_lease_create_command(args: &[String]) -> Command {
    let mut project_path = None;
    let mut task = None;
    let mut base = agent::parse_base("latest-workspace").expect("default base is valid");
    let mut hydrate_budget_bytes = DEFAULT_AGENT_HYDRATE_BUDGET_BYTES;
    let mut work_view = false;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--work-view" => {
                work_view = true;
                index += 1;
            }
            "--task" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentStart, "missing value for --task");
                };
                task = Some(value.to_string());
                index += 2;
            }
            "--base" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentStart, "missing value for --base");
                };
                let Some(parsed) = agent::parse_base(value) else {
                    return command_usage_error(
                        CommandName::AgentStart,
                        "usage_error",
                        "expected --base latest-workspace or --base latest:main".to_string(),
                        agent_usage_actions(),
                    );
                };
                base = parsed;
                index += 2;
            }
            "--hydrate-budget" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(
                        CommandName::AgentStart,
                        "missing value for --hydrate-budget",
                    );
                };
                let Some(parsed) = parse_byte_budget(value) else {
                    return command_usage_error(
                        CommandName::AgentStart,
                        "usage_error",
                        "expected --hydrate-budget as bytes, KiB, MiB, or GiB".to_string(),
                        agent_usage_actions(),
                    );
                };
                hydrate_budget_bytes = parsed;
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::AgentStart,
                    "usage_error",
                    format!("unknown bowline agent start option `{flag}`"),
                    agent_usage_actions(),
                );
            }
            value if project_path.is_none() => {
                project_path = Some(value.to_string());
                index += 1;
            }
            value => {
                return command_usage_error(
                    CommandName::AgentStart,
                    "usage_error",
                    format!("unexpected bowline agent start argument `{value}`"),
                    agent_usage_actions(),
                );
            }
        }
    }

    let project_path = project_path.unwrap_or_else(current_dir_string);
    let Some(task) = task else {
        return command_usage_error(
            CommandName::AgentStart,
            "usage_error",
            "bowline agent start requires --task <task>".to_string(),
            agent_usage_actions(),
        );
    };

    Command::AgentLeaseCreate(agent::AgentLeaseCreateArgs {
        project_path,
        task,
        base,
        hydrate_budget_bytes,
        work_view,
    })
}

pub(super) fn parse_agent_start_command(args: &[String]) -> Command {
    parse_agent_lease_create_command(args)
}

pub(super) fn parse_agent_selector_command(command: CommandName, args: &[String]) -> Command {
    let mut lease_id = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--lease" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(command, "missing value for --lease");
                };
                lease_id = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    command,
                    "usage_error",
                    format!(
                        "unknown bowline {} option `{flag}`",
                        command_name_token(command)
                    ),
                    agent_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    command,
                    "usage_error",
                    format!(
                        "unexpected bowline {} argument `{value}`",
                        command_name_token(command)
                    ),
                    agent_usage_actions(),
                );
            }
        }
    }
    let Some(lease_id) = lease_id else {
        return command_usage_error(
            command,
            "usage_error",
            format!(
                "bowline {} requires --lease <id>",
                command_name_token(command)
            ),
            agent_usage_actions(),
        );
    };
    let args = agent::AgentLeaseSelectorArgs { lease_id };
    match command {
        CommandName::AgentContext => Command::AgentContext(args),
        CommandName::AgentPrompt => Command::AgentPrompt(args),
        CommandName::AgentPublish => Command::AgentPublish(args),
        CommandName::AgentComplete => Command::AgentComplete(args),
        _ => unreachable!("unsupported agent selector command"),
    }
}

pub(super) fn parse_agent_budget_command(args: &[String]) -> Command {
    let mut lease_id = None;
    let mut add_bytes = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--lease" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentBudget, "missing value for --lease");
                };
                lease_id = Some(value.to_string());
                index += 2;
            }
            "--add" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentBudget, "missing value for --add");
                };
                let Some(parsed) = parse_byte_budget(value) else {
                    return command_usage_error(
                        CommandName::AgentBudget,
                        "usage_error",
                        "expected --add as bytes, KiB, MiB, or GiB".to_string(),
                        agent_usage_actions(),
                    );
                };
                add_bytes = Some(parsed);
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::AgentBudget,
                    "usage_error",
                    format!("unknown bowline agent budget option `{flag}`"),
                    agent_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    CommandName::AgentBudget,
                    "usage_error",
                    format!("unexpected bowline agent budget argument `{value}`"),
                    agent_usage_actions(),
                );
            }
        }
    }
    let Some(lease_id) = lease_id else {
        return command_usage_error(
            CommandName::AgentBudget,
            "usage_error",
            "bowline agent budget requires --lease <id>".to_string(),
            agent_usage_actions(),
        );
    };
    let Some(add_bytes) = add_bytes else {
        return command_usage_error(
            CommandName::AgentBudget,
            "usage_error",
            "bowline agent budget requires --add <bytes>".to_string(),
            agent_usage_actions(),
        );
    };
    Command::AgentBudget(agent::AgentBudgetArgs {
        lease_id,
        add_bytes,
    })
}
