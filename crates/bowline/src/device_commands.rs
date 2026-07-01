use super::*;

pub(super) fn print_devices(args: devices::DevicesArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match devices::run(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_devices_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Devices, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_approve(args: ApproveArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let request_id = match args.request_id {
        Some(request_id) => request_id,
        None => match devices::pending_requests() {
            Ok(requests) if requests.is_empty() => {
                return print_approve_no_pending(json, generated_at);
            }
            Ok(requests) if requests.len() == 1 => {
                let request = &requests[0];
                if json && !args.yes {
                    print_command_usage_error(
                        CommandUsageError {
                            command: CommandName::Approve,
                            code: "request_required",
                            message: "JSON approval requires an explicit request id or --yes."
                                .to_string(),
                            next_actions: vec![SafeAction {
                                label: format!("Approve {}", request.device_name),
                                command: Some(format!(
                                    "bowline approve {} --yes",
                                    request.request_id.as_str()
                                )),
                            }],
                        },
                        generated_at,
                        true,
                    );
                    return ExitCode::from(EXIT_USAGE);
                }
                if !json && !args.yes {
                    println!(
                        "Approve {}? Matching code: {}",
                        request.device_name, request.matching_code
                    );
                    if !confirm_return("Approve?") {
                        return ExitCode::SUCCESS;
                    }
                }
                request.request_id.as_str().to_string()
            }
            Ok(requests) => {
                if json {
                    print_command_usage_error(
                        CommandUsageError {
                            command: CommandName::Approve,
                            code: "multiple_pending_devices",
                            message: "Multiple devices are waiting for approval.".to_string(),
                            next_actions: requests
                                .iter()
                                .map(|request| SafeAction {
                                    label: format!("Approve {}", request.device_name),
                                    command: Some(format!(
                                        "bowline approve {}",
                                        request.request_id.as_str()
                                    )),
                                })
                                .collect(),
                        },
                        generated_at,
                        json,
                    );
                } else {
                    println!("Multiple devices are waiting for approval:");
                    for request in requests {
                        println!(
                            "  {}  {}  {}",
                            request.request_id.as_str(),
                            request.device_name,
                            request.matching_code
                        );
                    }
                    println!("Run `bowline approve <request>`.");
                }
                return ExitCode::from(EXIT_USAGE);
            }
            Err(error) => {
                print_runtime_error(CommandName::Approve, generated_at, &error, json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        },
    };

    match devices::approve(request_id, generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Approve;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Approve;
            print!("{}", render_devices_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Approve, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_approve_no_pending(json: bool, generated_at: String) -> ExitCode {
    if json {
        print_command_usage_error(
            CommandUsageError {
                command: CommandName::Approve,
                code: "no_pending_device",
                message: "No device is waiting for approval.".to_string(),
                next_actions: vec![SafeAction {
                    label: "Inspect workspace status".to_string(),
                    command: Some("bowline status".to_string()),
                }],
            },
            generated_at,
            true,
        );
    } else {
        println!("No device is waiting for approval.\nNext: bowline status");
    }
    ExitCode::from(approve_no_pending_exit_code(json))
}

pub(super) fn approve_no_pending_exit_code(json: bool) -> u8 {
    if json { EXIT_USAGE } else { 0 }
}

pub(super) fn print_revoke(args: RevokeArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match devices::run(
        devices::DevicesArgs::Revoke {
            device_id: args.device_id,
        },
        generated_at.clone(),
    ) {
        Ok(mut output) if json => {
            output.command = CommandName::Revoke;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Revoke;
            print!("{}", render_devices_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Revoke, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
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
                    mutates: action
                        .command
                        .as_deref()
                        .map(|command| {
                            command.contains(" --accept ") || command.contains(" --reject ")
                        })
                        .unwrap_or(false),
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
