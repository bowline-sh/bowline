use super::*;

pub(crate) fn current_dir_string() -> String {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

pub(crate) fn confirm_return(prompt: &str) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    print!("{prompt} Press Return to approve, or type no to cancel: ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok()
        && !matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no")
}

pub(super) fn parse_cleanup_command(args: &[String]) -> Command {
    let mut apply = false;
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Cleanup,
                    "usage_error",
                    format!("unknown bowline cleanup option `{flag}`"),
                    work_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Cleanup,
                    "usage_error",
                    format!("unexpected bowline cleanup argument `{value}`"),
                    work_usage_actions(),
                );
            }
        }
    }

    Command::WorkCleanup(work::WorkCleanupArgs { apply })
}

pub(super) fn parse_resolve_command(args: &[String]) -> Command {
    let mut project_or_path = None;
    let mut copy_prompt = false;
    let mut tui = false;
    let mut diff = None;
    let mut agent = None;
    let mut decision = None;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--copy-prompt" => {
                copy_prompt = true;
                index += 1;
            }
            "--tui" => {
                tui = true;
                index += 1;
            }
            "--diff" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --diff");
                };
                diff = Some(value.to_string());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --agent");
                };
                let Some(parsed) = resolve::parse_agent(value) else {
                    return usage_error(
                        CommandName::Resolve,
                        "expected --agent codex, --agent claude, or --agent cursor",
                    );
                };
                agent = Some(parsed);
                index += 2;
            }
            "--accept" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --accept");
                };
                if decision.is_some() {
                    return usage_error(
                        CommandName::Resolve,
                        "bowline resolve accepts only one --accept or --reject action",
                    );
                }
                decision = Some(resolve::ResolveDecision::Accept(value.to_string()));
                index += 2;
            }
            "--reject" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --reject");
                };
                if decision.is_some() {
                    return usage_error(
                        CommandName::Resolve,
                        "bowline resolve accepts only one --accept or --reject action",
                    );
                }
                decision = Some(resolve::ResolveDecision::Reject(value.to_string()));
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Resolve,
                    format!("unknown bowline resolve option `{flag}`"),
                );
            }
            value if project_or_path.is_none() => {
                project_or_path = Some(value.to_string());
                index += 1;
            }
            value => {
                return usage_error(
                    CommandName::Resolve,
                    format!("unexpected bowline resolve argument `{value}`"),
                );
            }
        }
    }

    let project_or_path = project_or_path.unwrap_or_else(current_dir_string);
    if diff.is_some() && decision.is_some() {
        return usage_error(
            CommandName::Resolve,
            "bowline resolve --diff cannot be combined with --accept or --reject",
        );
    }

    Command::Resolve(resolve::ResolveArgs {
        project_or_path,
        copy_prompt,
        tui,
        diff,
        agent,
        decision,
    })
}

pub(super) fn parse_connect_command(args: &[String]) -> Command {
    let Some(host) = args.first() else {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect requires a host".to_string(),
            vec![SafeAction {
                label: "Connect a host".to_string(),
                command: Some("bowline connect <host>".to_string()),
            }],
        );
    };
    let mut root = None;
    let mut artifact = None;
    let mut project = None;
    let mut task = None;
    let mut agent = None;
    let mut index = 1_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --root");
                };
                root = Some(value.to_string());
                index += 2;
            }
            "--binary" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --binary");
                };
                artifact = Some(value.to_string());
                index += 2;
            }
            "--project" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --project");
                };
                project = Some(value.to_string());
                index += 2;
            }
            "--task" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --task");
                };
                task = Some(value.to_string());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --agent");
                };
                agent = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Connect,
                    "usage_error",
                    format!("unknown bowline connect option `{flag}`"),
                    vec![SafeAction {
                        label: "Connect a host".to_string(),
                        command: Some(format!("bowline connect {host}")),
                    }],
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Connect,
                    "usage_error",
                    format!("unexpected bowline connect argument `{value}`"),
                    vec![SafeAction {
                        label: "Connect a host".to_string(),
                        command: Some(format!("bowline connect {host}")),
                    }],
                );
            }
        }
    }

    if project.is_some() != task.is_some() {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect agent handoff requires both --project <project> and --task <task>"
                .to_string(),
            vec![SafeAction {
                label: "Connect and start remote agent work".to_string(),
                command: Some(format!(
                    "bowline connect {host} --project <project> --task '<task>'"
                )),
            }],
        );
    }
    if agent.is_some() && project.is_none() {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect --agent requires --project <project> and --task <task>".to_string(),
            vec![SafeAction {
                label: "Connect and start remote agent work".to_string(),
                command: Some(format!(
                    "bowline connect {host} --project <project> --task '<task>' --agent codex"
                )),
            }],
        );
    }

    Command::BootstrapSsh(bootstrap::BootstrapSshArgs {
        host: host.to_string(),
        root: root
            .or_else(runtime::active_workspace_root)
            .unwrap_or_else(|| "~/Code".to_string()),
        artifact,
        project,
        task,
        agent,
    })
}

pub(super) fn devices_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Inspect workspace status".to_string(),
            command: Some("bowline status".to_string()),
        },
        SafeAction {
            label: "Approve a pending device".to_string(),
            command: Some("bowline approve".to_string()),
        },
    ]
}

pub(super) fn recovery_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Show Recovery Key status".to_string(),
            command: Some("bowline recover status".to_string()),
        },
        SafeAction {
            label: "Create a Recovery Key".to_string(),
            command: Some("bowline recover create".to_string()),
        },
    ]
}

pub(super) fn work_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Start a work view".to_string(),
            command: Some("bowline workon <name>".to_string()),
        },
        SafeAction {
            label: "Review work".to_string(),
            command: Some("bowline review".to_string()),
        },
    ]
}

pub(super) fn agent_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Start agent work".to_string(),
            command: Some(
                "bowline agent start <project> --task <task> --base latest-workspace".to_string(),
            ),
        },
        SafeAction {
            label: "Inspect an agent work".to_string(),
            command: Some("bowline agent context --lease <id>".to_string()),
        },
        SafeAction {
            label: "Publish an agent work for review".to_string(),
            command: Some("bowline agent publish --lease <id>".to_string()),
        },
        SafeAction {
            label: "Increase agent hydration budget".to_string(),
            command: Some("bowline agent budget --lease <id> --add 64MiB".to_string()),
        },
    ]
}

pub(super) fn parse_byte_budget(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    let (number, multiplier) = if let Some(number) = trimmed.strip_suffix("GiB") {
        (number, 1024_u64 * 1024 * 1024)
    } else if let Some(number) = trimmed.strip_suffix("MiB") {
        (number, 1024_u64 * 1024)
    } else if let Some(number) = trimmed.strip_suffix("KiB") {
        (number, 1024_u64)
    } else if let Some(number) = trimmed.strip_suffix("GB") {
        (number, 1_000_000_000_u64)
    } else if let Some(number) = trimmed.strip_suffix("MB") {
        (number, 1_000_000_u64)
    } else if let Some(number) = trimmed.strip_suffix("KB") {
        (number, 1_000_u64)
    } else {
        (trimmed, 1_u64)
    };
    number.trim().parse::<u64>().ok()?.checked_mul(multiplier)
}

pub(crate) fn command_name_token(command: CommandName) -> &'static str {
    match command {
        CommandName::Help => "help",
        CommandName::Version => "version",
        CommandName::Contract => "contract",
        CommandName::Unknown => "unknown",
        CommandName::Login => "login",
        CommandName::Approve => "approve",
        CommandName::Revoke => "revoke",
        CommandName::Recover => "recover",
        CommandName::Init => "init",
        CommandName::Setup => "setup",
        CommandName::Prewarm => "prewarm",
        CommandName::Status => "status",
        CommandName::Search => "search",
        CommandName::Symbols => "symbols",
        CommandName::Explain => "explain",
        CommandName::Devices => "devices",
        CommandName::Events => "events",
        CommandName::Actions => "actions",
        CommandName::Tui => "tui",
        CommandName::Resolve => "resolve",
        CommandName::Workon => "workon",
        CommandName::Review => "review",
        CommandName::Work => "work",
        CommandName::Diff => "diff",
        CommandName::Accept => "accept",
        CommandName::Discard => "discard",
        CommandName::Restore => "restore",
        CommandName::Cleanup => "cleanup",
        CommandName::AgentContext => "agent context",
        CommandName::AgentStart => "agent start",
        CommandName::AgentPrompt => "agent prompt",
        CommandName::AgentPublish => "agent publish",
        CommandName::AgentComplete => "agent complete",
        CommandName::AgentBudget => "agent budget",
        CommandName::DaemonStart => "daemon start",
        CommandName::DaemonStop => "daemon stop",
        CommandName::DaemonStatus => "daemon status",
        CommandName::DaemonInstall => "daemon install",
        CommandName::DaemonRestart => "daemon restart",
        CommandName::DaemonUninstall => "daemon uninstall",
        CommandName::DiagnosticsCollect => "diagnostics collect",
        CommandName::Connect => "connect",
    }
}

pub(super) fn command_usage_error(
    command: CommandName,
    code: &'static str,
    message: String,
    next_actions: Vec<SafeAction>,
) -> Command {
    Command::CommandUsageError(CommandUsageError {
        command,
        code,
        message,
        next_actions,
    })
}
