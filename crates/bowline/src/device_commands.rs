use super::*;

pub(super) fn print_devices(args: devices::DevicesArgs, json: bool, quiet: bool) -> ExitCode {
    let generated_at = generated_at();
    let command_name = args.command_name();
    let output = devices::run(args, generated_at.clone()).map(|mut output| {
        output.command = command_name;
        output
    });
    match output {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) if quiet => {
            write_human_or_exit(command_name, generated_at, &render_devices_quiet(&output))
        }
        Ok(output) => {
            print!("{}", render_devices_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_device_error(command_name, generated_at, &error, json).into(),
    }
}

pub(super) fn print_approve(args: ApproveArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let root = resolve_explicit_path(args.selection.root.clone());
    let workspace_id = match runtime::workspace_id_for_root(&root) {
        Ok(workspace_id) => workspace_id,
        Err(error) => {
            return print_runtime_error(CommandName::Approve, generated_at, &error, json).into();
        }
    };
    let request_id = match devices::request_id_for_selector(&workspace_id, &args.selector) {
        Ok(request_id) => request_id,
        Err(error) => {
            return print_device_error(CommandName::Approve, generated_at, &error, json).into();
        }
    };

    if !json && !args.yes && !confirm_return("Approve device request?") {
        return ExitCode::SUCCESS;
    }

    match devices::approve(workspace_id, request_id, generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Approve;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Approve;
            print!("{}", render_devices_human_for_root(&output, &root));
            ExitCode::SUCCESS
        }
        Err(error) => print_device_error(CommandName::Approve, generated_at, &error, json).into(),
    }
}

pub(super) fn print_deny(args: ApproveArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let root = resolve_explicit_path(args.selection.root);
    let workspace_id = match runtime::workspace_id_for_root(&root) {
        Ok(workspace_id) => workspace_id,
        Err(error) => {
            return print_runtime_error(CommandName::Deny, generated_at, &error, json).into();
        }
    };
    let request_id = match devices::request_id_for_selector(&workspace_id, &args.selector) {
        Ok(request_id) => request_id,
        Err(error) => {
            return print_device_error(CommandName::Deny, generated_at, &error, json).into();
        }
    };

    match devices::deny(workspace_id, request_id, generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Deny;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Deny;
            print!("{}", render_devices_human_for_root(&output, &root));
            ExitCode::SUCCESS
        }
        Err(error) => print_device_error(CommandName::Deny, generated_at, &error, json).into(),
    }
}

pub(super) fn print_revoke(args: RevokeArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let root = resolve_explicit_path(args.selection.root);
    let workspace_id = match runtime::workspace_id_for_root(&root) {
        Ok(workspace_id) => workspace_id,
        Err(error) => {
            print_runtime_error(CommandName::Revoke, generated_at, &error, json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    match devices::revoke(workspace_id, args.device_id, generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Revoke;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Revoke;
            print!("{}", render_devices_human_for_root(&output, &root));
            ExitCode::SUCCESS
        }
        Err(error) => print_device_error(CommandName::Revoke, generated_at, &error, json).into(),
    }
}

fn print_device_error(
    command: CommandName,
    generated_at: String,
    error: &devices::DeviceCommandError,
    json: bool,
) -> CommandExitCode {
    let mut output = bowline_local::status::command_error_output(
        command,
        generated_at,
        error.code(),
        error.to_string(),
        error.recoverability(),
    );
    output.error.remediation = error.remediation().map(str::to_string);
    output.next_actions = device_error_next_actions(error);
    print_command_error_output(&output, json)
}

fn device_error_next_actions(error: &devices::DeviceCommandError) -> Vec<RepairCommand> {
    match error {
        devices::DeviceCommandError::Selector(_)
        | devices::DeviceCommandError::RequestRequiresAction(_) => vec![RepairCommand::inspect(
            "List pending device requests".to_string(),
            Some("bowline device list --json".to_string()),
        )],
        devices::DeviceCommandError::TrustRequiresAction(_)
        | devices::DeviceCommandError::SafetyBlocked(_) => vec![RepairCommand::inspect(
            "Inspect device trust".to_string(),
            Some("bowline device list --json".to_string()),
        )],
        devices::DeviceCommandError::Runtime(_) => Vec::new(),
    }
}

fn render_devices_human_for_root(
    output: &bowline_core::commands::DevicesCommandOutput,
    root: &str,
) -> String {
    let mut human = render_devices_human(output);
    human.push_str(&format!("Workspace  {root}\n"));
    human
}

pub(super) fn print_recovery(args: recovery::RecoveryArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match recovery::run(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output.output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_recovery_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Recover, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}
