use super::*;

pub(super) fn print_login(mut args: login::LoginArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let root = args.root.clone();
    let wait_for_trust = !json && !args.no_poll && !args.headless;
    if json && args.no_poll && root.is_some() && can_finish_login_workspace_without_auth() {
        return print_login_workspace(root, generated_at, false, true);
    }
    args = login_args_for_output(args, json);
    if !json && !args.no_poll && !args.headless {
        return print_polling_login(root, generated_at, wait_for_trust);
    }
    match login::run(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_login_human(&output));
            print_login_workspace(root, generated_at, wait_for_trust, false)
        }
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn can_finish_login_workspace_without_auth() -> bool {
    runtime::control_plane().is_ok()
}

pub(super) fn print_polling_login(
    root: Option<String>,
    generated_at: String,
    wait_for_trust: bool,
) -> ExitCode {
    let (authorization, pending_output) = match login::start(generated_at.clone()) {
        Ok(started) => started,
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    print!("{}", render_login_human(&pending_output));
    let _ = io::stdout().flush();

    match login::finish(authorization, generated_at.clone()) {
        Ok(output) => {
            print!("{}", render_login_human(&output));
            print_login_workspace(root, generated_at, wait_for_trust, false)
        }
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_login_workspace(
    root: Option<String>,
    generated_at: String,
    wait_for_trust: bool,
    json: bool,
) -> ExitCode {
    let options = InitOptions {
        db_path: metadata_db_path(),
        requested_root: root
            .or_else(runtime::active_workspace_root)
            .map(resolve_explicit_path),
        generated_at: generated_at.clone(),
    };
    match bowline_local::init::initialize_root_with_workspace(
        options,
        runtime::active_workspace_id(),
    ) {
        Ok(mut output) => {
            output.command = CommandName::Login;
            let pending_request =
                attach_first_device_trust_if_available(&mut output, &generated_at);
            let workspace_id = output.workspace_id.clone();
            if json {
                print_json(&output);
            } else {
                print!("{}", render_init_human(&output));
            }
            if wait_for_trust && let Some(request_id) = pending_request {
                return wait_for_device_grant(workspace_id, request_id, generated_at);
            }
            ExitCode::SUCCESS
        }
        Err(LocalInitError::AmbiguousDefaultRoot(candidates)) => {
            print_ambiguous_init_root(candidates, generated_at, json);
            ExitCode::from(EXIT_USAGE)
        }
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn login_args_for_output(mut args: login::LoginArgs, json: bool) -> login::LoginArgs {
    if json {
        args.no_poll = true;
    }
    args
}

pub(super) fn print_init(args: InitArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = InitOptions {
        db_path: metadata_db_path(),
        requested_root: args.root.map(resolve_explicit_path),
        generated_at: generated_at.clone(),
    };

    match bowline_local::init::initialize_root_with_workspace(
        options,
        runtime::active_workspace_id_without_local_metadata_probe(),
    ) {
        Ok(mut output) if json => {
            attach_first_device_trust_if_available(&mut output, &generated_at);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            attach_first_device_trust_if_available(&mut output, &generated_at);
            print!("{}", render_init_human(&output));
            ExitCode::SUCCESS
        }
        Err(LocalInitError::AmbiguousDefaultRoot(candidates)) => {
            print_ambiguous_init_root(candidates, generated_at, json);
            ExitCode::from(EXIT_USAGE)
        }
        Err(error) => {
            print_runtime_error(CommandName::Init, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_prewarm(args: PrewarmArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let outcome = prewarm_project(PrewarmOptions {
        db_path: metadata_db_path(),
        project_path: resolve_explicit_path(args.project_path),
        approve_setup: args.approve_setup,
        trigger: if args.approve_setup {
            "cli-approved-setup".to_string()
        } else {
            "cli-setup".to_string()
        },
        generated_at: generated_at.clone(),
    });

    match outcome {
        Ok(outcome) if json => {
            print_json(&PrewarmCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: CommandName::Prewarm,
                generated_at,
                outcome: PrewarmCommandOutcome {
                    workspace_id: outcome.workspace_id,
                    project_id: outcome.project_id,
                    project_path: outcome.project_path,
                    state: match outcome.state {
                        bowline_local::setup::PrewarmState::Hot => PrewarmCommandState::Hot,
                        bowline_local::setup::PrewarmState::SetupBlocked => {
                            PrewarmCommandState::SetupBlocked
                        }
                        bowline_local::setup::PrewarmState::NoSetupNeeded => {
                            PrewarmCommandState::NoSetupNeeded
                        }
                    },
                    receipt_ids: outcome.receipt_ids,
                    redacted_summary: outcome.redacted_summary,
                },
            });
            ExitCode::SUCCESS
        }
        Ok(outcome) => {
            println!("Prewarm {:?}: {}", outcome.state, outcome.redacted_summary);
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_prewarm_error(error, generated_at, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_setup(args: SetupArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let project_path = args.project_path.unwrap_or_else(current_dir_string);
    let mut approve_setup = args.yes;

    loop {
        let outcome = prewarm_project(PrewarmOptions {
            db_path: metadata_db_path(),
            project_path: resolve_explicit_path(project_path.clone()),
            approve_setup,
            trigger: if approve_setup {
                "cli-approved-setup".to_string()
            } else {
                "cli-setup".to_string()
            },
            generated_at: generated_at.clone(),
        });

        match outcome {
            Ok(outcome)
                if !json
                    && !approve_setup
                    && outcome.state == bowline_local::setup::PrewarmState::SetupBlocked =>
            {
                println!("Setup needs approval: {}", outcome.redacted_summary);
                if !confirm_return("Approve setup?") {
                    return ExitCode::SUCCESS;
                }
                approve_setup = true;
            }
            Ok(outcome) if json => {
                print_json(&PrewarmCommandOutput {
                    contract_version: CONTRACT_VERSION,
                    command: CommandName::Setup,
                    generated_at,
                    outcome: PrewarmCommandOutcome {
                        workspace_id: outcome.workspace_id,
                        project_id: outcome.project_id,
                        project_path: outcome.project_path,
                        state: match outcome.state {
                            bowline_local::setup::PrewarmState::Hot => PrewarmCommandState::Hot,
                            bowline_local::setup::PrewarmState::SetupBlocked => {
                                PrewarmCommandState::SetupBlocked
                            }
                            bowline_local::setup::PrewarmState::NoSetupNeeded => {
                                PrewarmCommandState::NoSetupNeeded
                            }
                        },
                        receipt_ids: outcome.receipt_ids,
                        redacted_summary: outcome.redacted_summary,
                    },
                });
                return ExitCode::SUCCESS;
            }
            Ok(outcome) => {
                println!("Setup {:?}: {}", outcome.state, outcome.redacted_summary);
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                print_runtime_error(CommandName::Setup, generated_at, &error.to_string(), json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
}

pub(super) fn print_prewarm_error(error: SetupRunError, generated_at: String, json: bool) {
    print_runtime_error(CommandName::Prewarm, generated_at, &error.to_string(), json);
}

pub(super) fn attach_first_device_trust_if_available(
    output: &mut bowline_core::commands::InitCommandOutput,
    generated_at: &str,
) -> Option<String> {
    if !runtime::passive_secret_store_probe_allowed() {
        output.next_actions.push(SafeAction {
            label: "Log in before enabling workspace sync".to_string(),
            command: Some("bowline login".to_string()),
        });
        return None;
    }

    let Ok(key_store) = runtime::key_store() else {
        output.next_actions.push(SafeAction {
            label: "Check local secret store before enabling sync".to_string(),
            command: Some("bowline status".to_string()),
        });
        return None;
    };

    match key_store.load_account_tokens() {
        Ok(Some(_tokens)) => {}
        Ok(None) | Err(_)
            if env_account_session_id_present()
                || env_workos_access_token_present()
                || env_control_plane_token_present() => {}
        Ok(None) | Err(_) => {
            output.next_actions.push(SafeAction {
                label: "Log in before enabling workspace sync".to_string(),
                command: Some("bowline login".to_string()),
            });
            return None;
        }
    }

    let Ok(control_plane) = runtime::control_plane() else {
        output.next_actions.push(SafeAction {
            label: "Check control-plane connectivity before enabling sync".to_string(),
            command: Some("bowline status".to_string()),
        });
        return None;
    };

    let _ = control_plane.create_workspace_ref(output.workspace_id.as_str());
    let trust = match control_plane.list_device_trust(output.workspace_id.as_str()) {
        Ok(trust) => trust,
        Err(error) => {
            output.next_actions.push(SafeAction {
                label: format!("Trust setup unavailable: {error}"),
                command: Some("bowline status".to_string()),
            });
            return None;
        }
    };

    let current_device_id = runtime::daemon_device_id(&output.workspace_id);
    if !trust.authorized_devices.is_empty() {
        if trust
            .authorized_devices
            .iter()
            .any(|device| device.device_id == current_device_id.as_str())
        {
            output.next_actions.push(SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            });
            return None;
        }
        if let Some(request) = trust
            .pending_requests
            .iter()
            .find(|request| request.device_id == current_device_id.as_str())
        {
            match request.state {
                bowline_control_plane::DeviceRequestState::Approved => {
                    let request_id = DeviceApprovalRequestId::new(request.request_id.clone());
                    match bowline_local::trust::accept_device_grant(
                        &*control_plane,
                        &*key_store,
                        &output.workspace_id,
                        &request_id,
                        &current_device_id,
                    ) {
                        Ok(_) => {
                            output.next_actions.push(SafeAction {
                                label: "Inspect workspace status".to_string(),
                                command: Some("bowline status".to_string()),
                            });
                        }
                        Err(error) => {
                            output.next_actions.push(SafeAction {
                                label: format!("Device grant not accepted: {error}"),
                                command: Some("bowline status".to_string()),
                            });
                        }
                    }
                    return None;
                }
                bowline_control_plane::DeviceRequestState::Pending => {
                    output.next_actions.push(SafeAction {
                        label: format!(
                            "Approve {} with code {} on a trusted device",
                            request.device_name, request.matching_code
                        ),
                        command: Some(format!("bowline approve {}", request.request_id)),
                    });
                    return Some(request.request_id.clone());
                }
                bowline_control_plane::DeviceRequestState::Denied
                | bowline_control_plane::DeviceRequestState::Expired => {}
            }
        }
        match bowline_local::trust::create_device_request(
            &*control_plane,
            &*key_store,
            bowline_local::trust::DeviceRequestOptions {
                workspace_id: output.workspace_id.clone(),
                device_id: runtime::device_id(),
                device_name: runtime::device_name(),
                platform: runtime::platform(),
                host: None,
                root: Some(output.root.clone()),
                generated_at: generated_at.to_string(),
            },
        ) {
            Ok(request) => {
                let request_id = request.request_id.as_str().to_string();
                output.next_actions.push(SafeAction {
                    label: format!(
                        "Approve {} with code {} on a trusted device",
                        request.device_name, request.matching_code
                    ),
                    command: Some(format!("bowline approve {}", request.request_id.as_str())),
                });
                return Some(request_id);
            }
            Err(error) => {
                output.next_actions.push(SafeAction {
                    label: format!("Device approval request not created: {error}"),
                    command: Some("bowline status".to_string()),
                });
            }
        }
        return None;
    }

    match bowline_local::trust::ensure_first_device_trust_root(
        &*control_plane,
        &*key_store,
        output.workspace_id.clone(),
        runtime::device_id(),
        runtime::device_name(),
        runtime::platform(),
        generated_at.to_string(),
    ) {
        Ok(_) => {
            output.next_actions.push(SafeAction {
                label: "Create a Recovery Key".to_string(),
                command: Some("bowline recover create".to_string()),
            });
        }
        Err(error) => {
            output.next_actions.push(SafeAction {
                label: format!("Trust root not created: {error}"),
                command: Some("bowline status".to_string()),
            });
        }
    }
    None
}

pub(super) fn wait_for_device_grant(
    workspace_id: WorkspaceId,
    request_id: String,
    generated_at: String,
) -> ExitCode {
    println!("Waiting for approval. On a trusted device, run `bowline approve`.");
    let control_plane = match runtime::control_plane() {
        Ok(control_plane) => control_plane,
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let key_store = match runtime::key_store() {
        Ok(key_store) => key_store,
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let request_id = DeviceApprovalRequestId::new(request_id);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match bowline_local::trust::accept_device_grant(
            &*control_plane,
            &*key_store,
            &workspace_id,
            &request_id,
            &runtime::device_id(),
        ) {
            Ok(_) => {
                println!("Device approved. Workspace is ready.");
                return ExitCode::SUCCESS;
            }
            Err(bowline_local::trust::TrustError::MissingPendingRequest(_)) => {
                if Instant::now() >= deadline {
                    print_runtime_error(
                        CommandName::Login,
                        generated_at,
                        "timed out waiting for device approval; run `bowline login --no-poll` to leave the request pending",
                        false,
                    );
                    return ExitCode::from(EXIT_RUNTIME);
                }
                thread::sleep(Duration::from_secs(2));
            }
            Err(error) => {
                print_runtime_error(CommandName::Login, generated_at, &error.to_string(), false);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
}

pub(super) fn env_workos_access_token_present() -> bool {
    env::var("BOWLINE_WORKOS_ACCESS_TOKEN")
        .ok()
        .is_some_and(|value| !value.is_empty())
}

pub(super) fn env_account_session_id_present() -> bool {
    env::var("BOWLINE_ACCOUNT_SESSION_ID")
        .ok()
        .is_some_and(|value| !value.is_empty())
}

pub(super) fn env_control_plane_token_present() -> bool {
    env::var("BOWLINE_CONTROL_PLANE_TOKEN")
        .ok()
        .is_some_and(|value| !value.is_empty())
}
