use super::*;

pub(crate) fn print_contract(mode: ContractMode, json: bool) -> CommandExitCode {
    match mode {
        ContractMode::Full => print_full_contract(json),
        ContractMode::Summary => print_contract_summary(json),
        ContractMode::Topic(topic) => print_scoped_contract(&topic, json),
    }
}

fn print_full_contract(json: bool) -> CommandExitCode {
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
        exit_codes: CommandExitCode::contract_table(),
        command_output_types: command_output_types(),
        commands: command_descriptors(),
        fixtures: contract_fixtures(),
    };
    if json {
        print_json(&output);
    } else {
        println!(
            "bowline contract v{}: {} commands, {} fixtures. Use `bowline contract --json` for the machine contract.",
            output.contract_version,
            output.commands.len(),
            output.fixtures.len()
        );
    }
    CommandExitCode::Success
}

fn print_contract_summary(json: bool) -> CommandExitCode {
    let output = ContractSummaryCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Contract,
        generated_at: generated_at(),
        cli_version: CLI_VERSION.to_string(),
        protocol: PROTOCOL.to_string(),
        protocol_version: PROTOCOL_VERSION,
        event_schema_version: EVENT_SCHEMA_VERSION,
        package: "bowline".to_string(),
        package_contract_source: PACKAGE_CONTRACT_SOURCE.to_string(),
        exit_codes: CommandExitCode::contract_table(),
        commands: command_specs().map(command_summary).collect(),
    };
    if json {
        print_json(&output);
    } else {
        println!(
            "bowline contract summary ({} commands)\n",
            output.commands.len()
        );
        for command in output.commands {
            println!("  {:<24} {}", command.name, command.summary);
        }
    }
    CommandExitCode::Success
}

fn print_scoped_contract(topic: &[String], json: bool) -> CommandExitCode {
    let requested_topic = topic.join(" ");
    let Some(spec) = command_spec_for_topic(topic) else {
        return print_command_usage_error(
            CommandUsageError {
                command: CommandName::Contract,
                code: "usage_error",
                message: format!("no contract topic named `{requested_topic}`"),
                next_actions: vec![RepairCommand::inspect(
                    "Inspect the full CLI contract".to_string(),
                    Some("bowline contract --json".to_string()),
                )],
            },
            generated_at(),
            json,
        );
    };
    let descriptor = command_descriptor(spec);
    if json {
        print_json(&ScopedContractCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Contract,
            generated_at: generated_at(),
            cli_version: CLI_VERSION.to_string(),
            protocol: PROTOCOL.to_string(),
            protocol_version: PROTOCOL_VERSION,
            event_schema_version: EVENT_SCHEMA_VERSION,
            package: "bowline".to_string(),
            package_contract_source: PACKAGE_CONTRACT_SOURCE.to_string(),
            exit_codes: CommandExitCode::contract_table(),
            descriptor,
        });
    } else {
        println!("{}", render_command_help(&descriptor));
    }
    CommandExitCode::Success
}

fn command_spec_for_topic(topic: &[String]) -> Option<&'static CommandSpec> {
    let canonical_topic = topic.join(" ");
    command_specs().find(|spec| spec.name == canonical_topic)
}

fn command_summary(spec: &CommandSpec) -> CliCommandSummary {
    CliCommandSummary {
        name: spec.name.to_string(),
        group: spec.group.to_string(),
        summary: spec.summary.to_string(),
        side_effect_level: spec.side_effect_level.to_string(),
        supports_json: spec.supports_json,
        supports_dry_run: spec.supports_dry_run,
    }
}
