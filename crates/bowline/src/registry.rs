use super::*;

#[derive(Clone, Copy)]
struct CommandSpec {
    group: &'static str,
    name: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    usage: &'static str,
    options: &'static [OptionSpec],
    examples: &'static [ExampleSpec],
    json_output_type: &'static str,
    side_effect_level: &'static str,
    supports_json: bool,
    supports_dry_run: bool,
    supports_idempotency_key: bool,
    bounded_output: Option<BoundedSpec>,
    related_commands: &'static [&'static str],
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
const DRY_RUN_OPTION: OptionSpec = OptionSpec {
    name: "--dry-run",
    value_name: None,
    summary: "Preview the mutation without changing local or daemon state.",
    required: false,
    repeatable: false,
};
const IDEMPOTENCY_OPTION: OptionSpec = OptionSpec {
    name: "--idempotency-key",
    value_name: Some("key"),
    summary: "Replay-safe key for a non-dry-run mutation.",
    required: false,
    repeatable: false,
};
const RECOVERY_IDEMPOTENCY_OPTION: OptionSpec = OptionSpec {
    name: "--idempotency-key",
    value_name: Some("key"),
    summary: "Replay-safe key for recover create, rotate, and revoke; recover verify and use read stdin and reject it.",
    required: false,
    repeatable: false,
};
const SEARCH_BOUND: BoundedSpec = BoundedSpec {
    default_limit: 20,
    max_limit: 100,
    cursor_format: "v1:<offset>",
    path_prefix: true,
};

const EMPTY_OPTIONS: &[OptionSpec] = &[];
const EMPTY_EXAMPLES: &[ExampleSpec] = &[];

mod specs;

use specs::COMMAND_REGISTRY;

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
            if let Some(spec) = COMMAND_REGISTRY.iter().find(|spec| spec.name == command) {
                println!("  {}", spec.usage);
            }
        }
        println!();
    }
    println!(
        "Global options:\n  --json\n  --socket <path>\n  --dry-run\n  --idempotency-key <key>"
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
            default_socket: DEFAULT_SOCKET.to_string(),
            package: "bowline".to_string(),
        });
        return;
    }
    println!("bowline {CLI_VERSION}");
}

pub(super) fn print_contract(json: bool) {
    let output = ContractCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Contract,
        generated_at: generated_at(),
        cli_version: CLI_VERSION.to_string(),
        protocol: PROTOCOL.to_string(),
        protocol_version: PROTOCOL_VERSION,
        event_schema_version: EVENT_SCHEMA_VERSION,
        package: "bowline".to_string(),
        package_contract_source: PACKAGE_CONTRACT_SOURCE.to_string(),
        command_output_types: command_output_types(),
        commands: command_descriptors(),
        fixtures: contract_fixtures(),
    };
    if json {
        print_json(&output);
        return;
    }
    println!(
        "bowline contract v{}: {} commands, {} fixtures. Use `bowline contract --json` for the machine contract.",
        output.contract_version,
        output.commands.len(),
        output.fixtures.len()
    );
}

fn command_descriptors_for_topic(topic: Option<&str>) -> Vec<CliCommandDescriptor> {
    let Some(topic) = topic else {
        return command_descriptors();
    };
    let topic = topic.trim();
    if topic.is_empty() {
        return command_descriptors();
    }
    COMMAND_REGISTRY
        .iter()
        .filter(|spec| {
            spec.name == topic
                || spec.aliases.contains(&topic)
                || spec.group.eq_ignore_ascii_case(topic)
        })
        .map(command_descriptor)
        .collect()
}

fn command_descriptors() -> Vec<CliCommandDescriptor> {
    COMMAND_REGISTRY.iter().map(command_descriptor).collect()
}

fn command_descriptor(spec: &CommandSpec) -> CliCommandDescriptor {
    CliCommandDescriptor {
        group: spec.group.to_string(),
        name: spec.name.to_string(),
        aliases: spec
            .aliases
            .iter()
            .map(|alias| (*alias).to_string())
            .collect(),
        summary: spec.summary.to_string(),
        usage: spec.usage.to_string(),
        options: spec.options.iter().map(command_option).collect(),
        examples: spec.examples.iter().map(command_example).collect(),
        json_output_type: spec.json_output_type.to_string(),
        side_effect_level: spec.side_effect_level.to_string(),
        supports_json: spec.supports_json,
        supports_dry_run: spec.supports_dry_run,
        supports_idempotency_key: spec.supports_idempotency_key,
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
    if !descriptor.aliases.is_empty() {
        output.push_str(&format!(
            "\n\nAliases:\n  {}",
            descriptor.aliases.join(", ")
        ));
    }
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
    COMMAND_REGISTRY
        .iter()
        .map(|spec| spec.json_output_type)
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
            "dry-run",
            "tests/contracts/commands/dry-run.json",
            "DryRunCommandOutput",
        ),
        (
            "explain-env",
            "tests/contracts/commands/explain-env.json",
            "ExplainCommandOutput",
        ),
        (
            "help",
            "tests/contracts/commands/help.json",
            "HelpCommandOutput",
        ),
        (
            "setup-blocked",
            "tests/contracts/commands/setup-blocked.json",
            "PrewarmCommandOutput",
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
            "workon-created",
            "tests/contracts/commands/workon-created.json",
            "WorkonCommandOutput",
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
