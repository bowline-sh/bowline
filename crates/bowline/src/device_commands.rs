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

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MergePluginApprovalOutput {
    contract_version: u16,
    command: CommandName,
    generated_at: String,
    workspace_id: WorkspaceId,
    plugin_id: String,
    plugin_version: String,
    digest: String,
    matcher_version: String,
    validator_version: String,
    approved_by_device_id: DeviceId,
}

pub(super) fn print_approve_merge_plugin(args: MergePluginApproveArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    if !args.digest.starts_with("blake3:") {
        print_usage_error(
            CommandName::Approve,
            "usage_error",
            "merge plugin digest must start with `blake3:`",
            json,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let root = resolve_explicit_path(args.selection.root.clone());
    let workspace_id = match runtime::workspace_id_for_root(&root) {
        Ok(workspace_id) => workspace_id,
        Err(error) => {
            print_runtime_error(CommandName::Approve, generated_at, &error, json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let approvable = match bowline_local::sync::merge_plugins::declared_approvable_merge_plugins(
        std::path::Path::new(&root),
    ) {
        Ok(approvable) => approvable,
        Err(error) => {
            print_runtime_error(CommandName::Approve, generated_at, &error.to_string(), json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let Some(request) = approvable.iter().find(|request| {
        request.plugin.id == args.id
            && request.plugin.version == args.version
            && request.plugin.digest == args.digest
    }) else {
        print_usage_error(
            CommandName::Approve,
            "usage_error",
            &format!(
                "merge plugin approval must match a declaration in .bowlinemerge.toml: {}",
                render_approvable_merge_plugins(&approvable)
            ),
            json,
        );
        return ExitCode::from(EXIT_USAGE);
    };
    if let Some(matcher_version) = &args.matcher_version
        && matcher_version != &request.plugin.matcher_version
    {
        print_usage_error(
            CommandName::Approve,
            "usage_error",
            &format!(
                "merge plugin matcher version mismatch: declaration uses `{}`, but `--matcher-version` was `{matcher_version}`",
                request.plugin.matcher_version
            ),
            json,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    if let Some(validator_version) = &args.validator_version
        && validator_version != &request.plugin.validator_version
    {
        print_usage_error(
            CommandName::Approve,
            "usage_error",
            &format!(
                "merge plugin validator version mismatch: declaration uses `{}`, but `--validator-version` was `{validator_version}`",
                request.plugin.validator_version
            ),
            json,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    if !json
        && !args.yes
        && !confirm_return(&format!(
            "Approve merge plugin `{}` {} for this device?\n  digest:   {}\n  module:   {}\n  matches:  {}",
            request.plugin.id,
            request.plugin.version,
            request.plugin.digest,
            request.module,
            request.patterns.join(", ")
        ))
    {
        return ExitCode::SUCCESS;
    }

    let db_path = match runtime::selected_metadata_database_path() {
        Some(path) => path,
        None => {
            print_runtime_error(
                CommandName::Approve,
                generated_at,
                &format!("no local metadata database; run `bowline setup --root {root}`"),
                json,
            );
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let store = match MetadataStore::open(db_path) {
        Ok(store) => store,
        Err(error) => {
            print_runtime_error(CommandName::Approve, generated_at, &error.to_string(), json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let approved_by_device_id = runtime::device_id();
    let plugin = request.plugin.clone();
    if let Err(error) = store.approve_merge_plugin(
        &bowline_local::sync::merge_plugins::MergePluginApprovalInput {
            workspace_id: workspace_id.clone(),
            plugin: plugin.clone(),
            approved_by_device_id: approved_by_device_id.clone(),
            approved_at: generated_at.clone(),
        },
    ) {
        print_runtime_error(CommandName::Approve, generated_at, &error.to_string(), json);
        return ExitCode::from(EXIT_RUNTIME);
    }

    let output = MergePluginApprovalOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Approve,
        generated_at,
        workspace_id,
        plugin_id: plugin.id,
        plugin_version: plugin.version,
        digest: plugin.digest,
        matcher_version: plugin.matcher_version,
        validator_version: plugin.validator_version,
        approved_by_device_id,
    };
    if json {
        print_json(&output);
    } else {
        println!(
            "Approved merge plugin `{}` {} ({}) for this device.",
            output.plugin_id, output.plugin_version, output.digest
        );
    }
    ExitCode::SUCCESS
}

fn render_approvable_merge_plugins(
    requests: &[bowline_local::sync::merge_plugins::MergePluginApprovalRequest],
) -> String {
    if requests.is_empty() {
        return "no approvable merge plugin declarations found".to_string();
    }
    requests
        .iter()
        .map(|request| {
            format!(
                "{} {} {} matcher={} validator={}",
                request.plugin.id,
                request.plugin.version,
                request.plugin.digest,
                request.plugin.matcher_version,
                request.plugin.validator_version
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
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

pub(super) fn print_resolve(args: resolve::ResolveArgs, json: bool, socket: &Path) -> ExitCode {
    let generated_at = generated_at();
    let use_tui = args.tui;
    let args = resolve::ResolveArgs {
        project_or_path: resolve_explicit_path(args.project_or_path),
        ..args
    };
    let output = resolve::run(args, generated_at);

    let command_failed = output.command_failed;
    if json {
        print_json(&output);
    } else if use_tui && io::stdin().is_terminal() && io::stdout().is_terminal() {
        let model = surface::tui::TuiModel::from_resolve(
            output.status.summary.clone(),
            surface::tui::TuiTone::from_status_label(output.status.level),
            output
                .available_actions
                .iter()
                .map(|action| surface::tui::TuiAction {
                    label: action.label.clone(),
                    command: action.command.clone(),
                    mutates: action.mutates,
                })
                .collect(),
            output
                .conflicts
                .iter()
                .map(|conflict| {
                    if conflict.contains_secrets {
                        format!(
                            "{}: secret-bearing conflict at {}",
                            conflict.id, conflict.bundle_path
                        )
                    } else {
                        format!("{}: {}", conflict.id, conflict.affected_files.join(", "))
                    }
                })
                .collect(),
        );
        match surface::tui::run_app(model) {
            Ok(Some(command)) => return run_confirmed_tui_command(&command, socket),
            Ok(None) => {}
            Err(error) => {
                print_runtime_error(
                    CommandName::Resolve,
                    output.generated_at.clone(),
                    &error.to_string(),
                    false,
                );
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    } else {
        let human = resolve::render_human(&output);
        print!("{human}");
    }

    if command_failed {
        return ExitCode::from(EXIT_RUNTIME);
    }

    ExitCode::SUCCESS
}
