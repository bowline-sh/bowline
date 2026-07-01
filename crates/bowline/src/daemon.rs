use super::*;

pub(super) fn print_unknown_command(command: &str, json: bool) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Unknown,
            generated_at: generated_at(),
            status: CommandErrorStatus::UsageError,
            error: CommandError {
                code: "unknown_command".to_string(),
                message: format!("unknown command `{command}`"),
                recoverability: CommandRecoverability::UserAction,
                remediation: Some(
                    "Run `bowline help --json` to discover supported commands.".to_string(),
                ),
                details: Some(serde_json::json!({ "command": command })),
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: vec![SafeAction {
                label: "List bowline commands".to_string(),
                command: Some("bowline help --json".to_string()),
            }],
        });
    } else {
        eprintln!("bowline unknown command: {command}");
    }
}

pub(super) fn daemon_command_output(
    command: CommandName,
    generated_at: String,
    socket: &Path,
    state: &str,
    daemon_version: Option<&str>,
    pid: Option<u32>,
    include_protocol: bool,
) -> DaemonCommandOutput {
    DaemonCommandOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at,
        daemon: daemon_process_output(socket, state, daemon_version, pid, include_protocol),
    }
}

pub(super) fn daemon_process_output(
    socket: &Path,
    state: &str,
    daemon_version: Option<&str>,
    pid: Option<u32>,
    include_protocol: bool,
) -> DaemonProcessOutput {
    DaemonProcessOutput {
        state: state.to_string(),
        socket: socket.display().to_string(),
        protocol: include_protocol.then(|| PROTOCOL.to_string()),
        version: include_protocol.then_some(PROTOCOL_VERSION),
        daemon_version: daemon_version.map(str::to_string),
        pid,
    }
}

pub(super) fn daemon_service_state_from_status(status: &DaemonServiceStatus) -> DaemonServiceState {
    DaemonServiceState {
        state: status.state.clone(),
        name: None,
        unit_path: status.unit_path.display().to_string(),
        unavailable_because: status.unavailable_because.clone(),
    }
}

pub(super) fn daemon_service_state_from_outcome(
    outcome: &DaemonServiceOutcome,
) -> DaemonServiceState {
    DaemonServiceState {
        state: outcome.state.clone(),
        name: Some(outcome.service_name.clone()),
        unit_path: outcome.unit_path.display().to_string(),
        unavailable_because: None,
    }
}

pub(super) fn print_daemon_start(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let workspace_id =
        daemon_workspace_id_for_start().unwrap_or_else(|_| runtime::active_workspace_id());
    match handshake(socket) {
        Ok(handshake) => {
            if handshake_sync_workspace_ready_for_start(&handshake, workspace_id.as_str()) {
                if json {
                    print_json(&daemon_command_output(
                        CommandName::DaemonStart,
                        generated_at.clone(),
                        socket,
                        "running",
                        Some(&handshake.daemon_version),
                        None,
                        true,
                    ));
                } else {
                    println!("bowline daemon: already running");
                }
                return ExitCode::SUCCESS;
            }
            let _ = request_shutdown(socket);
            wait_for_daemon_socket_to_stop(socket, Duration::from_secs(3));
        }
        Err(error) => {
            remove_stale_daemon_socket_after_connect_error(socket, &error);
        }
    }

    match start_daemon_process(socket) {
        Ok(child_id) => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStart,
                    generated_at,
                    socket,
                    "starting",
                    None,
                    Some(child_id),
                    false,
                ));
            } else {
                println!("bowline daemon: starting (pid {child_id})");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_runtime_error(CommandName::DaemonStart, generated_at, &message, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn remove_stale_daemon_socket_after_connect_error(socket: &Path, error: &io::Error) {
    if error.kind() != io::ErrorKind::ConnectionRefused {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        if std::fs::symlink_metadata(socket)
            .map(|metadata| metadata.file_type().is_socket())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(socket);
        }
    }
}

pub(super) fn handshake_sync_workspace_ready_for_start(
    handshake: &Handshake,
    workspace_id: &str,
) -> bool {
    handshake.sync_json.as_deref().is_some_and(|sync| {
        extract_json_string(sync, "workspaceId").as_deref() == Some(workspace_id)
            && !matches!(
                extract_json_string(sync, "state").as_deref(),
                Some("limited" | "degraded")
            )
    })
}

pub(super) fn wait_for_daemon_socket_to_stop(socket: &Path, timeout: Duration) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if handshake(socket).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn print_daemon_stop(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match request_shutdown(socket) {
        Ok(()) => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStop,
                    generated_at,
                    socket,
                    "stopping",
                    None,
                    None,
                    false,
                ));
            } else {
                println!("bowline daemon: stopping");
            }
            ExitCode::SUCCESS
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStop,
                    generated_at,
                    socket,
                    "stopped",
                    None,
                    None,
                    false,
                ));
            } else {
                println!("bowline daemon: stopped");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::DaemonStop,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_diagnostics_collect(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let bundle = diagnostics_bundle_text(socket, &generated_at);
    let redacted = redact_setup_text(&bundle);
    if json {
        let output = DiagnosticsCollectCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::DiagnosticsCollect,
            generated_at,
            redaction_rules: redacted.rules,
            bundle: redacted.text,
        };
        print_json(&output);
        return ExitCode::SUCCESS;
    }
    println!("{}", redacted.text);
    if !redacted.rules.is_empty() {
        println!("redaction_rules={}", redacted.rules.join(","));
    }
    ExitCode::SUCCESS
}

pub(super) fn diagnostics_bundle_text(socket: &Path, generated_at: &str) -> String {
    let db_path = metadata_db_path_or_default();
    let state_root = db_path
        .as_ref()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("unavailable"));
    let db_path = db_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unavailable:{error}"));
    let service = daemon_service_status(&SystemProcessRunner)
        .map(|status| {
            let unavailable = status
                .unavailable_because
                .map(|message| format!(" unavailable={message}"))
                .unwrap_or_default();
            format!(
                "{} path={}{}",
                status.state,
                status.unit_path.display(),
                unavailable
            )
        })
        .unwrap_or_else(|| "unsupported".to_string());
    [
        "bowline diagnostics".to_string(),
        format!("generated_at={generated_at}"),
        format!("socket={}", socket.display()),
        format!("metadata_db={db_path}"),
        format!(
            "daemon_log={}",
            state_root.join("bowline-daemon.log").display()
        ),
        format!(
            "daemon_stdout={}",
            state_root.join("bowline-daemon.out.log").display()
        ),
        format!(
            "daemon_stderr={}",
            state_root.join("bowline-daemon.err.log").display()
        ),
        format!("service={service}"),
        "project_file_contents=excluded".to_string(),
    ]
    .join("\n")
}
