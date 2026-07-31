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

pub(super) fn print_device_key_status(args: devices::DeviceKeyStatusArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match devices::key_status(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_device_key_status_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_device_error(CommandName::DeviceKeyStatus, generated_at, &error, json).into()
        }
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
    let request_id = match resolve_approval(&workspace_id, &args, &generated_at, json) {
        Ok(request_id) => request_id,
        Err(exit_code) => return exit_code,
    };

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

/// Approval hands the requesting device the workspace key and every plaintext
/// secret in it, so the gate is `--yes` alone: an output mode can never stand in
/// for consent, and a terminal that cannot ask must refuse rather than approve.
fn resolve_approval(
    workspace_id: &WorkspaceId,
    args: &ApproveArgs,
    generated_at: &str,
    json: bool,
) -> Result<DeviceApprovalRequestId, ExitCode> {
    let approval_error = |error: &devices::DeviceCommandError| {
        ExitCode::from(print_device_error(
            CommandName::Approve,
            generated_at.to_string(),
            error,
            json,
        ))
    };
    if args.yes {
        return devices::request_id_for_selector(workspace_id, &args.selector)
            .map_err(|error| approval_error(&error));
    }
    let request = devices::pending_request_for_selector(workspace_id, &args.selector)
        .map_err(|error| approval_error(&error))?;
    match confirm_device_approval(&request) {
        DeviceApprovalConsent::Approved => Ok(request.request_id.clone()),
        DeviceApprovalConsent::Declined => {
            eprintln!("Not approved.");
            Err(ExitCode::from(CommandExitCode::UserActionRequired))
        }
        DeviceApprovalConsent::CannotAsk => Err(print_approval_confirmation_required(
            generated_at.to_string(),
            &args.selection.root,
            &request,
            json,
        )
        .into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceApprovalConsent {
    Approved,
    Declined,
    CannotAsk,
}

/// Shows the requesting device and its matching code, then requires an explicit
/// affirmative. EOF, an empty line, or anything else is a decline: the default
/// answer to "hand this machine your workspace key" is no.
fn confirm_device_approval(
    request: &bowline_core::devices::DeviceApprovalRequest,
) -> DeviceApprovalConsent {
    if !io::stdin().is_terminal() {
        return DeviceApprovalConsent::CannotAsk;
    }
    print!("{}", render_pending_approval(request));
    print!("Approve this device? Type yes to approve: ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return DeviceApprovalConsent::Declined;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => DeviceApprovalConsent::Approved,
        _ => DeviceApprovalConsent::Declined,
    }
}

fn render_pending_approval(request: &bowline_core::devices::DeviceApprovalRequest) -> String {
    let pres = surface::style::Presentation::detect(false);
    let mut lines = vec![
        format!(
            "{}  {}",
            surface::style::section("Device", &pres),
            request.device_name
        ),
        format!(
            "{}  {}",
            surface::style::section("Platform", &pres),
            surface::style::kebab(&request.platform)
        ),
        format!(
            "{}  {}",
            surface::style::section("Code", &pres),
            bowline_core::devices::display_matching_code(&request.matching_code)
        ),
        format!(
            "{}  {}",
            surface::style::section("Request", &pres),
            request.request_id.as_str()
        ),
    ];
    if let Some(host) = &request.host {
        lines.push(format!(
            "{}  {host}",
            surface::style::section("Host", &pres)
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn print_approval_confirmation_required(
    generated_at: String,
    root: &str,
    request: &bowline_core::devices::DeviceApprovalRequest,
    json: bool,
) -> CommandExitCode {
    let code = bowline_core::devices::display_matching_code(&request.matching_code);
    let command = format!(
        "bowline device approve --root {} --code {code} --yes",
        io_helpers::shell_word(root)
    );
    let mut output = bowline_local::status::command_error_output(
        CommandName::Approve,
        generated_at,
        "needs_confirmation",
        format!(
            "approving {} ({}) grants it the workspace key; confirm from a terminal or pass --yes",
            request.device_name, code
        ),
        CommandRecoverability::UserAction,
    );
    output.error.remediation =
        Some("Check the code shown on the requesting device before approving.".to_string());
    output.next_actions = vec![RepairCommand::mutating(
        format!("Approve {} after checking the code", request.device_name),
        Some(command),
    )];
    print_command_error_output(&output, json)
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
        devices::DeviceCommandError::ClientOutOfDate(_) => vec![RepairCommand::mutating(
            "Install a client that matches this control plane".to_string(),
            Some("bowline update".to_string()),
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
        Ok(output) => emit_recovery(&output, json),
        Err(error) => {
            print_runtime_error(CommandName::Recover, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

/// The envelope is already published by the time we get here, so a failed write
/// would destroy the only copy of the words. Flush explicitly and, if stdout is
/// unusable, fall back to stderr rather than exiting 0 with the secret gone.
fn emit_recovery(output: &recovery::RecoveryRunOutput, json: bool) -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    emit_recovery_to(output, json, &mut stdout.lock(), &mut stderr.lock())
}

fn emit_recovery_to(
    output: &recovery::RecoveryRunOutput,
    json: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> ExitCode {
    let written = if json {
        write_json_line_to(stdout, &output.json_payload())
    } else {
        stdout
            .write_all(render_recovery_human(output).as_bytes())
            .and_then(|()| stdout.flush())
    };
    let Err(error) = written else {
        return ExitCode::SUCCESS;
    };
    let _ = writeln!(
        stderr,
        "bowline recover could not write its output to stdout: {error}"
    );
    if let Some(words) = output.generated_words.as_deref() {
        let _ = writeln!(
            stderr,
            "Store these Recovery Key words now; bowline cannot print them again:\n{words}"
        );
    }
    ExitCode::from(EXIT_RUNTIME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::commands::{RecoveryCommandAction, RecoveryCommandOutput};
    use bowline_core::devices::{RecoveryKeyLifecycle, RecoveryKeyState};
    use bowline_core::ids::RecoveryEnvelopeId;

    struct FailAfter {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Write for FailAfter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.bytes.len() >= self.limit {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected broken pipe",
                ));
            }
            let written = buffer.len().min(self.limit - self.bytes.len());
            self.bytes.extend_from_slice(&buffer[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn human_recovery_write_failure_preserves_words_and_returns_failure() {
        let output = recovery::RecoveryRunOutput {
            output: RecoveryCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: CommandName::Recover,
                generated_at: "2026-07-30T12:00:00Z".to_string(),
                action: RecoveryCommandAction::Create,
                workspace_id: Some(WorkspaceId::new("workspace-recovery-output")),
                recovery_key: RecoveryKeyState {
                    lifecycle: RecoveryKeyLifecycle::GeneratedUnverified,
                    envelope_id: Some(RecoveryEnvelopeId::new("recovery-output")),
                    fingerprint: Some("rkp_output".to_string()),
                    created_at: Some("2026-07-30T12:00:00Z".to_string()),
                    verified_at: None,
                    rotated_at: None,
                    revoked_at: None,
                },
                device_request: None,
                encrypted_grant: None,
                next_actions: Vec::new(),
            },
            generated_words: Some("alpha beta gamma delta".to_string()),
        };
        let mut stdout = FailAfter {
            bytes: Vec::new(),
            limit: 16,
        };
        let mut stderr = Vec::new();

        let exit = emit_recovery_to(&output, false, &mut stdout, &mut stderr);

        assert_eq!(exit, ExitCode::from(EXIT_RUNTIME));
        assert!(
            !String::from_utf8(stdout.bytes)
                .expect("stdout remains UTF-8")
                .contains("alpha beta gamma delta")
        );
        let fallback = String::from_utf8(stderr).expect("stderr remains UTF-8");
        assert!(fallback.contains("injected broken pipe"));
        assert!(fallback.contains("alpha beta gamma delta"));
    }
}
