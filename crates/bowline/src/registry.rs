use super::*;

mod model;

pub(crate) use model::{DefinitionFailure, DefinitionTarget, ParsedValues, resolve_definition};

#[derive(Clone, Copy)]
struct CommandSpec {
    group: &'static str,
    name: &'static str,
    summary: &'static str,
    usage: &'static str,
    options: &'static [OptionSpec],
    positionals: &'static [PositionalSpec],
    examples: &'static [ExampleSpec],
    json_output_type: &'static str,
    side_effect_level: &'static str,
    supports_json: bool,
    supports_dry_run: bool,
    bounded_output: Option<BoundedSpec>,
    related_commands: &'static [&'static str],
}

impl CommandSpec {
    fn command_name(self) -> CommandName {
        match self.name {
            "debug classify" => CommandName::Unknown,
            "internal handoff install-bundle" => CommandName::Handoff,
            name => CommandName::from_token(name)
                .unwrap_or_else(|| unreachable!("public command definition has a generated name")),
        }
    }

    fn target(self) -> DefinitionTarget {
        match self.name {
            "debug classify" => DefinitionTarget::DebugClassify,
            "internal handoff install-bundle" => DefinitionTarget::HandoffInstallBundle,
            _ => DefinitionTarget::Public(self.command_name()),
        }
    }
}

#[derive(Clone, Copy)]
struct OptionSpec {
    name: &'static str,
    value_name: Option<&'static str>,
    summary: &'static str,
    required: bool,
    repeatable: bool,
}

#[derive(Clone, Copy)]
struct PositionalSpec {
    name: &'static str,
    required: bool,
    repeatable: bool,
}

#[derive(Clone, Copy)]
struct ExampleSpec {
    command: &'static str,
    summary: &'static str,
}

#[derive(Clone, Copy)]
struct BoundedSpec {
    default_limit: u16,
    max_limit: u16,
    cursor_format: &'static str,
    path_prefix: bool,
}

const GLOBAL_JSON_OPTION: OptionSpec = OptionSpec {
    name: "--json",
    value_name: None,
    summary: "Return the command contract JSON on stdout.",
    required: false,
    repeatable: false,
};
const HUMAN_OPTION: OptionSpec = OptionSpec {
    name: "--human",
    value_name: None,
    summary: "Render human-readable output.",
    required: false,
    repeatable: false,
};
const CONTRACT_SUMMARY_OPTION: OptionSpec = OptionSpec {
    name: "--summary",
    value_name: None,
    summary: "Return the compact command discovery index.",
    required: false,
    repeatable: false,
};
const QUIET_OPTION: OptionSpec = OptionSpec {
    name: "--quiet",
    value_name: None,
    summary: "Print one primary identifier per line.",
    required: false,
    repeatable: false,
};
const DRY_RUN_OPTION: OptionSpec = OptionSpec {
    name: "--dry-run",
    value_name: None,
    summary: "Preview the mutation without changing local or daemon state.",
    required: false,
    repeatable: false,
};
const SOCKET_OPTION: OptionSpec = OptionSpec {
    name: "--socket",
    value_name: Some("path"),
    summary: "Use this local daemon socket.",
    required: false,
    repeatable: false,
};
const WORK_PATH_OPTION: OptionSpec = OptionSpec {
    name: "--path",
    value_name: Some("glob"),
    summary: "Limit the work-view diff or accept to matching project-relative paths.",
    required: false,
    repeatable: true,
};
const ROOT_OPTION: OptionSpec = OptionSpec {
    name: "--root",
    value_name: Some("path"),
    summary: "Select the workspace root.",
    required: true,
    repeatable: false,
};
const OPTIONAL_ROOT_OPTION: OptionSpec = OptionSpec {
    name: "--root",
    value_name: Some("path"),
    summary: "Select the workspace root.",
    required: false,
    repeatable: false,
};
const PROJECT_OPTION: OptionSpec = OptionSpec {
    name: "--project",
    value_name: Some("path"),
    summary: "Scope to a project under the selected root.",
    required: false,
    repeatable: false,
};
const REQUEST_OPTION: OptionSpec = OptionSpec {
    name: "--request",
    value_name: Some("id"),
    summary: "Select a pending device request.",
    required: false,
    repeatable: false,
};
const CODE_OPTION: OptionSpec = OptionSpec {
    name: "--code",
    value_name: Some("short-code"),
    summary: "Select the pending request with this short matching code.",
    required: false,
    repeatable: false,
};
const DEVICE_OPTION: OptionSpec = OptionSpec {
    name: "--device",
    value_name: Some("id"),
    summary: "Select a trusted device.",
    required: true,
    repeatable: false,
};
const LIMIT_OPTION: OptionSpec = OptionSpec {
    name: "--limit",
    value_name: Some("n"),
    summary: "Maximum results to return.",
    required: false,
    repeatable: false,
};
const CURSOR_OPTION: OptionSpec = OptionSpec {
    name: "--cursor",
    value_name: Some("cursor"),
    summary: "Opaque cursor from nextCursor.",
    required: false,
    repeatable: false,
};
const LEASE_ID_OPTION: OptionSpec = OptionSpec {
    name: "--lease",
    value_name: Some("id"),
    summary: "Select an agent lease.",
    required: true,
    repeatable: false,
};
const NO_POSITIONALS: &[PositionalSpec] = &[];
const HELP_POSITIONALS: &[PositionalSpec] = &[PositionalSpec {
    name: "topic",
    required: false,
    repeatable: true,
}];
const OPTIONAL_PROJECT_POSITIONAL: &[PositionalSpec] = &[PositionalSpec {
    name: "project",
    required: false,
    repeatable: false,
}];
const REQUIRED_PROJECT_POSITIONAL: &[PositionalSpec] = &[PositionalSpec {
    name: "project",
    required: true,
    repeatable: false,
}];
const RECOVERY_POSITIONALS: &[PositionalSpec] = &[
    PositionalSpec {
        name: "action",
        required: true,
        repeatable: false,
    },
    PositionalSpec {
        name: "arguments",
        required: false,
        repeatable: true,
    },
];
const HISTORY_POSITIONALS: &[PositionalSpec] = &[
    PositionalSpec {
        name: "mode-or-target",
        required: false,
        repeatable: false,
    },
    PositionalSpec {
        name: "target",
        required: false,
        repeatable: false,
    },
];
const WORK_CREATE_POSITIONALS: &[PositionalSpec] = &[
    PositionalSpec {
        name: "name-or-project",
        required: true,
        repeatable: false,
    },
    PositionalSpec {
        name: "name",
        required: false,
        repeatable: false,
    },
];
const OPTIONAL_TARGET_POSITIONAL: &[PositionalSpec] = &[PositionalSpec {
    name: "target",
    required: false,
    repeatable: false,
}];
const REQUIRED_TARGET_POSITIONAL: &[PositionalSpec] = &[PositionalSpec {
    name: "target",
    required: true,
    repeatable: false,
}];
const HISTORY_BOUND: BoundedSpec = BoundedSpec {
    default_limit: 50,
    max_limit: 500,
    cursor_format: "offset",
    path_prefix: false,
};
const HISTORY_OPTIONS: &[OptionSpec] = &[
    LIMIT_OPTION,
    CURSOR_OPTION,
    OptionSpec {
        name: "--since",
        value_name: Some("time"),
        summary: "Only include restore points at or after this timestamp.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--until",
        value_name: Some("time"),
        summary: "Only include restore points at or before this timestamp.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--from",
        value_name: Some("restore-point"),
        summary: "Select the older restore point for history diff mode.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--to",
        value_name: Some("restore-point"),
        summary: "Select the newer restore point for history diff mode.",
        required: false,
        repeatable: false,
    },
    GLOBAL_JSON_OPTION,
    QUIET_OPTION,
];
const HISTORY_EXAMPLES: &[ExampleSpec] = &[
    ExampleSpec {
        command: "bowline history apps/web --json",
        summary: "List restore points for a project.",
    },
    ExampleSpec {
        command: "bowline history path apps/web/src/App.tsx --json",
        summary: "List restore points that touched a path.",
    },
    ExampleSpec {
        command: "bowline history diff apps/web --from rp_snap_old --to rp_snap_new --json",
        summary: "Summarize changes between two restore points.",
    },
];
const WORK_CREATE_FROM_OPTION: OptionSpec = OptionSpec {
    name: "--from",
    value_name: Some("restore-point"),
    summary: "Use a restore point or snapshot id as the work view base.",
    required: false,
    repeatable: false,
};
const WORK_CREATE_OPTIONS: &[OptionSpec] =
    &[WORK_CREATE_FROM_OPTION, GLOBAL_JSON_OPTION, DRY_RUN_OPTION];
const RESOLVE_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        name: "--tui",
        value_name: None,
        summary: "Open the interactive conflict resolver when a terminal is available.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--copy-prompt",
        value_name: None,
        summary: "Include agent-ready conflict context for copying.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--diff",
        value_name: Some("conflict"),
        summary: "Show one conflict diff.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--agent",
        value_name: Some("agent"),
        summary: "Shape copied context for codex, claude, or cursor.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--accept",
        value_name: Some("conflict"),
        summary: "Accept one conflict resolution.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--reject",
        value_name: Some("conflict"),
        summary: "Reject one conflict resolution.",
        required: false,
        repeatable: false,
    },
    SOCKET_OPTION,
    GLOBAL_JSON_OPTION,
];
const CONNECT_OPTIONS: &[OptionSpec] = &[
    OPTIONAL_ROOT_OPTION,
    OptionSpec {
        name: "--binary",
        value_name: Some("path"),
        summary: "Use a local bowline binary artifact for the remote install.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--project",
        value_name: Some("project"),
        summary: "Project path for optional remote agent handoff.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--task",
        value_name: Some("task"),
        summary: "Task for optional remote agent handoff.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--agent",
        value_name: Some("agent"),
        summary: "Agent runtime for optional remote handoff.",
        required: false,
        repeatable: false,
    },
    GLOBAL_JSON_OPTION,
    DRY_RUN_OPTION,
];
const HANDOFF_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        name: "--agent",
        value_name: Some("agent"),
        summary: "Select codex or claude. Defaults to the newest supported local session.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--session",
        value_name: Some("id-or-path"),
        summary: "Resume a specific local agent session by id or transcript path.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--prompt",
        value_name: Some("text"),
        summary: "Start a fresh remote agent session from inline prompt text.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--prompt-file",
        value_name: Some("path"),
        summary: "Start a fresh remote agent session from a local prompt file.",
        required: false,
        repeatable: false,
    },
    OptionSpec {
        name: "--project",
        value_name: Some("path"),
        summary: "Use this project path instead of the current directory.",
        required: false,
        repeatable: false,
    },
    GLOBAL_JSON_OPTION,
    DRY_RUN_OPTION,
];
const WORK_LIST_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        name: "--all",
        value_name: None,
        summary: "Include hidden and retained work views.",
        required: false,
        repeatable: false,
    },
    GLOBAL_JSON_OPTION,
    QUIET_OPTION,
];

const EMPTY_EXAMPLES: &[ExampleSpec] = &[];

mod contract;
mod daemon_specs;
mod diagnostics_specs;
mod lease_specs;
mod specs;
#[cfg(test)]
mod tests;
mod work_specs;

pub(super) use contract::print_contract;

fn command_specs() -> impl Iterator<Item = &'static CommandSpec> {
    specs::COMMAND_REGISTRY
        .iter()
        .chain(lease_specs::LEASE_COMMAND_REGISTRY.iter())
        .chain(work_specs::COMMAND_REGISTRY.iter())
        .chain(daemon_specs::DAEMON_COMMAND_REGISTRY.iter())
        .chain(diagnostics_specs::COMMAND_REGISTRY.iter())
}

const INTERNAL_COMMAND_REGISTRY: &[CommandSpec] = &[
    CommandSpec {
        group: "Internal",
        name: "debug classify",
        summary: "Classify one path without exposing file contents.",
        usage: "bowline debug classify <path> [--json]",
        options: &[GLOBAL_JSON_OPTION],
        positionals: REQUIRED_TARGET_POSITIONAL,
        examples: EMPTY_EXAMPLES,
        json_output_type: "DebugClassifyCommandOutput",
        side_effect_level: "none",
        supports_json: true,
        supports_dry_run: false,
        bounded_output: None,
        related_commands: &[],
    },
    CommandSpec {
        group: "Internal",
        name: "internal handoff install-bundle",
        summary: "Install an authenticated handoff bundle from stdin.",
        usage: "bowline internal handoff install-bundle --json",
        options: &[GLOBAL_JSON_OPTION],
        positionals: NO_POSITIONALS,
        examples: EMPTY_EXAMPLES,
        json_output_type: "HandoffCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: false,
        bounded_output: None,
        related_commands: &[],
    },
];

fn all_command_specs() -> impl Iterator<Item = &'static CommandSpec> {
    command_specs().chain(INTERNAL_COMMAND_REGISTRY.iter())
}

pub(super) fn print_help(topic: Option<&[String]>, json: bool) {
    let topic_name = topic.map(|parts| parts.join(" "));
    let commands = command_descriptors_for_topic(topic_name.as_deref());
    if json {
        print_json(&HelpCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Help,
            generated_at: generated_at(),
            topic: topic_name,
            groups: command_groups_for_descriptors(&commands),
            commands,
        });
        return;
    }

    if let Some(topic_name) = topic_name.as_deref() {
        if commands.is_empty() {
            eprintln!("bowline help: no topic named `{topic_name}`");
            return;
        }
        for descriptor in commands {
            println!("{}", render_command_help(&descriptor));
        }
        return;
    }

    println!("bowline command shell\n");
    for group in command_groups() {
        println!("{}:", group.name);
        for command in group.commands {
            if let Some(spec) = command_specs().find(|spec| spec.name == command) {
                println!("  {}", spec.usage);
            }
        }
        println!();
    }
    println!(
        "Command options follow the complete command path. Inspect `bowline help <command>` for the exact definition."
    );
}

pub(super) fn print_version(json: bool) {
    if json {
        print_json(&VersionCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Version,
            generated_at: generated_at(),
            cli_version: CLI_VERSION.to_string(),
            protocol: PROTOCOL.to_string(),
            protocol_version: PROTOCOL_VERSION,
            default_socket: default_socket_path().display().to_string(),
            package: "bowline".to_string(),
        });
        return;
    }
    println!("bowline {CLI_VERSION}");
}

fn command_descriptors_for_topic(topic: Option<&str>) -> Vec<CliCommandDescriptor> {
    let Some(topic) = topic else {
        return command_descriptors();
    };
    let topic = topic.trim();
    if topic.is_empty() {
        return command_descriptors();
    }
    command_specs()
        .filter(|spec| spec.name == topic || spec.group.eq_ignore_ascii_case(topic))
        .map(command_descriptor)
        .collect()
}

fn command_descriptors() -> Vec<CliCommandDescriptor> {
    command_specs().map(command_descriptor).collect()
}

fn command_descriptor(spec: &CommandSpec) -> CliCommandDescriptor {
    CliCommandDescriptor {
        group: spec.group.to_string(),
        name: spec.name.to_string(),
        summary: spec.summary.to_string(),
        usage: spec.usage.to_string(),
        options: descriptor_options(spec),
        positionals: spec.positionals.iter().map(command_positional).collect(),
        examples: spec.examples.iter().map(command_example).collect(),
        json_output_type: spec.json_output_type.to_string(),
        side_effect_level: spec.side_effect_level.to_string(),
        supports_json: spec.supports_json,
        supports_dry_run: spec.supports_dry_run,
        bounded_output: spec.bounded_output.map(|bounded| BoundedOutputControls {
            default_limit: bounded.default_limit,
            max_limit: bounded.max_limit,
            cursor_format: bounded.cursor_format.to_string(),
            path_prefix: bounded.path_prefix,
        }),
        related_commands: spec
            .related_commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
    }
}

fn descriptor_options(spec: &CommandSpec) -> Vec<CliCommandOption> {
    let mut options = spec.options.iter().map(command_option).collect::<Vec<_>>();
    if spec.supports_json {
        options.push(command_option(&HUMAN_OPTION));
    }
    options
}

fn command_positional(positional: &PositionalSpec) -> CliCommandPositional {
    CliCommandPositional {
        name: positional.name.to_string(),
        required: positional.required,
        repeatable: positional.repeatable,
    }
}

fn command_option(option: &OptionSpec) -> CliCommandOption {
    CliCommandOption {
        name: option.name.to_string(),
        value_name: option.value_name.map(str::to_string),
        summary: option.summary.to_string(),
        required: option.required,
        repeatable: option.repeatable,
    }
}

fn command_example(example: &ExampleSpec) -> CliCommandExample {
    CliCommandExample {
        command: example.command.to_string(),
        summary: example.summary.to_string(),
    }
}

fn command_groups() -> Vec<CliCommandGroup> {
    command_groups_for_descriptors(&command_descriptors())
}

fn command_groups_for_descriptors(descriptors: &[CliCommandDescriptor]) -> Vec<CliCommandGroup> {
    let mut groups = Vec::<CliCommandGroup>::new();
    for descriptor in descriptors {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.name == descriptor.group)
        {
            group.commands.push(descriptor.name.clone());
        } else {
            groups.push(CliCommandGroup {
                name: descriptor.group.clone(),
                commands: vec![descriptor.name.clone()],
            });
        }
    }
    groups
}

fn render_command_help(descriptor: &CliCommandDescriptor) -> String {
    let mut output = format!(
        "{}\n  {}\n\nUsage:\n  {}\n\nJSON output:\n  {}\n\nSide effects:\n  {}",
        descriptor.name,
        descriptor.summary,
        descriptor.usage,
        descriptor.json_output_type,
        descriptor.side_effect_level
    );
    if !descriptor.options.is_empty() {
        output.push_str("\n\nOptions:");
        for option in &descriptor.options {
            let value = option
                .value_name
                .as_ref()
                .map(|value| format!(" <{value}>"))
                .unwrap_or_default();
            output.push_str(&format!("\n  {}{}  {}", option.name, value, option.summary));
        }
    }
    if let Some(bounded) = &descriptor.bounded_output {
        output.push_str(&format!(
            "\n\nBounds:\n  default limit {}, max {}, cursor {}",
            bounded.default_limit, bounded.max_limit, bounded.cursor_format
        ));
    }
    if !descriptor.related_commands.is_empty() {
        output.push_str(&format!(
            "\n\nRelated:\n  {}",
            descriptor.related_commands.join(", ")
        ));
    }
    output
}

fn command_output_types() -> Vec<String> {
    command_specs()
        .flat_map(|spec| spec.json_output_type.split(" | "))
        .filter(|output_type| *output_type != "none")
        .fold(Vec::<String>::new(), |mut output_types, output_type| {
            if !output_types.iter().any(|existing| existing == output_type) {
                output_types.push(output_type.to_string());
            }
            output_types
        })
}

fn contract_fixtures() -> Vec<ContractFixtureDescriptor> {
    [
        (
            "agent-context",
            "tests/contracts/commands/agent-context.json",
            "AgentContextCommandOutput",
        ),
        (
            "agent-lease-create",
            "tests/contracts/commands/agent-lease-create.json",
            "AgentLeaseCreateCommandOutput",
        ),
        (
            "agent-prompt",
            "tests/contracts/commands/agent-prompt.json",
            "AgentPromptCommandOutput",
        ),
        (
            "contract",
            "tests/contracts/commands/contract.json",
            "ContractCommandOutput",
        ),
        (
            "contract-summary",
            "tests/contracts/commands/contract-summary.json",
            "ContractSummaryCommandOutput",
        ),
        (
            "contract-work-diff",
            "tests/contracts/commands/contract-work-diff.json",
            "ScopedContractCommandOutput",
        ),
        (
            "dry-run",
            "tests/contracts/commands/dry-run.json",
            "DryRunCommandOutput",
        ),
        (
            "help",
            "tests/contracts/commands/help.json",
            "HelpCommandOutput",
        ),
        (
            "history",
            "tests/contracts/commands/history.json",
            "HistoryCommandOutput",
        ),
        (
            "handoff-dry-run",
            "tests/contracts/commands/handoff-dry-run.json",
            "HandoffCommandOutput",
        ),
        (
            "handoff-confirmation-required",
            "tests/contracts/commands/handoff-confirmation-required.json",
            "HandoffCommandOutput",
        ),
        (
            "handoff-receipt",
            "tests/contracts/commands/handoff-receipt.json",
            "HandoffCommandOutput",
        ),
        (
            "handoff-no-supported-session",
            "tests/contracts/commands/handoff-no-supported-session.json",
            "HandoffCommandOutput",
        ),
        (
            "handoff-target-not-trusted",
            "tests/contracts/commands/handoff-target-not-trusted.json",
            "HandoffCommandOutput",
        ),
        (
            "handoff-trust-stale",
            "tests/contracts/commands/handoff-trust-stale.json",
            "HandoffCommandOutput",
        ),
        (
            "handoff-tmux-missing",
            "tests/contracts/commands/handoff-tmux-missing.json",
            "HandoffCommandOutput",
        ),
        (
            "setup-blocked",
            "tests/contracts/commands/setup-blocked.json",
            "SetupProjectOutput",
        ),
        (
            "setup-machine",
            "tests/contracts/commands/setup-machine.json",
            "SetupCommandOutput",
        ),
        (
            "version",
            "tests/contracts/commands/version.json",
            "VersionCommandOutput",
        ),
        (
            "work-accept-review-ready",
            "tests/contracts/commands/work-accept-review-ready.json",
            "WorkDiffCommandOutput",
        ),
        (
            "work-accept",
            "tests/contracts/commands/work-accept.json",
            "WorkLifecycleCommandOutput",
        ),
        (
            "work-discard",
            "tests/contracts/commands/work-discard.json",
            "WorkLifecycleCommandOutput",
        ),
        (
            "work-review",
            "tests/contracts/commands/work-review.json",
            "WorkDiffCommandOutput",
        ),
        (
            "work-create-created",
            "tests/contracts/commands/work-create-created.json",
            "WorkCreateCommandOutput",
        ),
        (
            "work-create-reused",
            "tests/contracts/commands/work-create-reused.json",
            "WorkCreateCommandOutput",
        ),
    ]
    .into_iter()
    .map(|(name, path, output_type)| ContractFixtureDescriptor {
        name: name.to_string(),
        path: path.to_string(),
        output_type: output_type.to_string(),
    })
    .collect()
}
