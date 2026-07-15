use super::*;

pub(super) fn print_ambiguous_setup_root(
    candidates: Vec<PathBuf>,
    generated_at: String,
    json: bool,
) {
    let roots = candidates
        .iter()
        .map(|path| abbreviate_requested_path(&path.display().to_string()))
        .collect::<Vec<_>>();
    let message = format!(
        "bowline setup found multiple existing code roots; pass an explicit root: {}",
        roots.join(", ")
    );
    let next_actions = roots
        .iter()
        .map(|root| {
            RepairCommand::mutating(
                format!("Set up with {root}"),
                Some(format!("bowline setup --root {root}")),
            )
        })
        .collect::<Vec<_>>();

    print_command_usage_error(
        CommandUsageError {
            command: CommandName::Setup,
            code: "ambiguous_root",
            message,
            next_actions,
        },
        generated_at,
        json,
    );
}

pub(super) fn print_usage_error(
    command: CommandName,
    code: &str,
    message: &str,
    json: bool,
) -> CommandExitCode {
    let help_command = if command == CommandName::Unknown {
        "bowline help --json".to_string()
    } else {
        format!("bowline help {} --json", command.token())
    };
    let remediation = if command == CommandName::Unknown {
        "Inspect the command catalog and retry with a canonical command path.".to_string()
    } else {
        format!(
            "Inspect `bowline help {} --json` and retry with valid arguments.",
            command.token()
        )
    };
    let next_actions = vec![RepairCommand::inspect(
        "Inspect command help".to_string(),
        Some(help_command),
    )];
    let output = CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at: generated_at(),
        status: CommandErrorStatus::UsageError,
        error: CommandError {
            code: code.to_string(),
            message: message.to_string(),
            recoverability: CommandRecoverability::UserAction,
            remediation: Some(remediation.clone()),
            details: None,
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions,
    };
    if json {
        print_json(&output);
    } else {
        eprintln!("bowline usage error: {message}");
        print_human_error_guidance(Some(&remediation), &output.next_actions);
    }
    exit_code_for_error(&output)
}

pub(super) fn print_command_usage_error(
    error: CommandUsageError,
    generated_at: String,
    json: bool,
) -> CommandExitCode {
    let remediation = "Inspect command help and retry with valid arguments.";
    let output = CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command: error.command,
        generated_at,
        status: CommandErrorStatus::UsageError,
        error: CommandError {
            code: error.code.to_string(),
            message: error.message,
            recoverability: CommandRecoverability::UserAction,
            remediation: Some(remediation.to_string()),
            details: None,
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions: error.next_actions,
    };
    if json {
        print_json(&output);
    } else {
        eprintln!("bowline usage error: {}", output.error.message);
        print_human_error_guidance(Some(remediation), &output.next_actions);
    }
    exit_code_for_error(&output)
}

pub(super) fn print_runtime_error(
    command: CommandName,
    generated_at: String,
    message: &str,
    json: bool,
) -> CommandExitCode {
    let output = bowline_local::status::command_error_output(
        command,
        generated_at,
        "runtime_error",
        message,
        CommandRecoverability::Retry,
    );
    print_command_error_output(&output, json)
}

pub(super) fn print_user_action_error(
    command: CommandName,
    generated_at: String,
    code: &str,
    message: &str,
    remediation: &str,
    json: bool,
) -> CommandExitCode {
    let mut output = bowline_local::status::command_error_output(
        command,
        generated_at,
        code,
        message,
        CommandRecoverability::UserAction,
    );
    output.error.remediation = Some(remediation.to_string());
    print_command_error_output(&output, json)
}

pub(super) fn print_command_error_output(
    output: &CommandErrorOutput,
    json: bool,
) -> CommandExitCode {
    if json {
        print_json(&output);
    } else {
        eprintln!(
            "bowline {} failed: {}",
            output.command.token(),
            output.error.message
        );
        print_human_error_guidance(output.error.remediation.as_deref(), &output.next_actions);
    }
    exit_code_for_error(output)
}

fn exit_code_for_error(output: &CommandErrorOutput) -> CommandExitCode {
    CommandExitCode::for_error(output.status, output.error.recoverability)
}

fn print_human_error_guidance(remediation: Option<&str>, next_actions: &[RepairCommand]) {
    if remediation.is_none() && next_actions.is_empty() {
        return;
    }
    let pres = surface::style::Presentation::detect(false);
    if let Some(remediation) = remediation {
        eprintln!("  {remediation}");
    }
    if !next_actions.is_empty() {
        eprintln!("  {}", surface::style::section("Next", &pres));
        for action in next_actions {
            match &action.command {
                Some(command) => {
                    eprintln!(
                        "{}",
                        surface::style::next_action(command, &action.label, &pres)
                    );
                }
                None => eprintln!("  {}", action.label),
            }
        }
    }
}
