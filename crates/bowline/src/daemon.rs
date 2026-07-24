use super::*;
use bowline_core::commands::DaemonSyncState;

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
            next_actions: vec![RepairCommand::inspect(
                "List bowline commands".to_string(),
                Some("bowline help --json".to_string()),
            )],
        });
    } else {
        eprintln!("bowline unknown command: {command}");
    }
}

pub(super) fn daemon_command_output(
    command: CommandName,
    generated_at: String,
    socket: &Path,
    state: DaemonProcessState,
    daemon_version: Option<&str>,
    pid: Option<u32>,
    include_protocol: bool,
) -> DaemonCommandOutput {
    daemon_command_output_with_sync(DaemonCommandOutputParams {
        command,
        generated_at,
        socket,
        state,
        sync_state: None,
        unavailable_because: None,
        daemon_version,
        pid,
        include_protocol,
    })
}

struct DaemonCommandOutputParams<'a> {
    command: CommandName,
    generated_at: String,
    socket: &'a Path,
    state: DaemonProcessState,
    sync_state: Option<DaemonSyncState>,
    unavailable_because: Option<String>,
    daemon_version: Option<&'a str>,
    pid: Option<u32>,
    include_protocol: bool,
}

fn daemon_command_output_with_sync(params: DaemonCommandOutputParams<'_>) -> DaemonCommandOutput {
    DaemonCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: params.command,
        generated_at: params.generated_at,
        daemon: daemon_process_output_with_sync(
            params.socket,
            params.state,
            params.sync_state,
            params.unavailable_because,
            params.daemon_version,
            params.pid,
            params.include_protocol,
        ),
    }
}

pub(super) fn daemon_process_output(
    socket: &Path,
    state: DaemonProcessState,
    daemon_version: Option<&str>,
    pid: Option<u32>,
    include_protocol: bool,
) -> DaemonProcessOutput {
    daemon_process_output_with_sync(
        socket,
        state,
        None,
        None,
        daemon_version,
        pid,
        include_protocol,
    )
}

fn daemon_process_output_with_sync(
    socket: &Path,
    state: DaemonProcessState,
    sync_state: Option<DaemonSyncState>,
    unavailable_because: Option<String>,
    daemon_version: Option<&str>,
    pid: Option<u32>,
    include_protocol: bool,
) -> DaemonProcessOutput {
    DaemonProcessOutput {
        state: state.as_str().to_string(),
        socket: socket.display().to_string(),
        sync_state,
        unavailable_because,
        protocol: include_protocol.then(|| PROTOCOL.to_string()),
        version: include_protocol.then_some(PROTOCOL_VERSION),
        daemon_version: daemon_version.map(str::to_string),
        pid,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DaemonProcessState {
    Running,
    Starting,
    Stopping,
    Stopped,
}

impl DaemonProcessState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Starting => "starting",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
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
        Ok(handshake) => match handshake_start_status(&handshake, workspace_id.as_str()) {
            DaemonStartHandshakeStatus::Ready => {
                if json {
                    print_json(&daemon_command_output(
                        CommandName::DaemonStart,
                        generated_at.clone(),
                        socket,
                        DaemonProcessState::Running,
                        Some(&handshake.daemon_version),
                        None,
                        true,
                    ));
                } else {
                    println!("bowline daemon: already running");
                }
                return ExitCode::SUCCESS;
            }
            DaemonStartHandshakeStatus::Degraded { state, reason } => {
                if json {
                    print_json(&daemon_command_output_with_sync(
                        DaemonCommandOutputParams {
                            command: CommandName::DaemonStart,
                            generated_at: generated_at.clone(),
                            socket,
                            state: DaemonProcessState::Running,
                            sync_state: Some(state),
                            unavailable_because: Some(reason.clone()),
                            daemon_version: Some(&handshake.daemon_version),
                            pid: None,
                            include_protocol: true,
                        },
                    ));
                } else {
                    println!("bowline daemon: running, {}: {reason}", state.as_str());
                    println!("Next: bowline status");
                    println!("Restart explicitly: bowline daemon restart");
                }
                return ExitCode::SUCCESS;
            }
            DaemonStartHandshakeStatus::WorkspaceMismatch => {
                let _ = request_shutdown(socket);
                let _ = wait_for_daemon_socket_to_stop(socket, Duration::from_secs(3));
            }
        },
        Err(error) => {
            let _ = remove_stale_daemon_socket_after_connect_error(socket, &error);
        }
    }

    match start_daemon_process(socket) {
        Ok(child_id) => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStart,
                    generated_at,
                    socket,
                    DaemonProcessState::Starting,
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

pub(super) fn remove_stale_daemon_socket_after_connect_error(
    socket: &Path,
    error: &io::Error,
) -> io::Result<bool> {
    if error.kind() != io::ErrorKind::ConnectionRefused {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        match std::fs::symlink_metadata(socket) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                std::fs::remove_file(socket)?;
                return Ok(true);
            }
            Ok(_) => {}
            Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {}
            Err(metadata_error) => return Err(metadata_error),
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DaemonStartHandshakeStatus {
    Ready,
    Degraded {
        state: DaemonSyncState,
        reason: String,
    },
    WorkspaceMismatch,
}

pub(super) fn handshake_start_status(
    handshake: &Handshake,
    workspace_id: &str,
) -> DaemonStartHandshakeStatus {
    let status = handshake.status_json.as_str();
    let Some(sync_workspace_id) = json_string_field(status, "workspaceId") else {
        return DaemonStartHandshakeStatus::Degraded {
            state: DaemonSyncState::Unclassified,
            reason: "workspace status is unavailable".to_string(),
        };
    };
    if sync_workspace_id != workspace_id {
        return DaemonStartHandshakeStatus::WorkspaceMismatch;
    }
    match json_pointer_string(status, "/status/level").as_deref() {
        Some("limited") => DaemonStartHandshakeStatus::Degraded {
            state: DaemonSyncState::Limited,
            reason: daemon_degraded_reason(status, DaemonSyncState::Limited),
        },
        Some("attention") => DaemonStartHandshakeStatus::Degraded {
            state: DaemonSyncState::Degraded,
            reason: daemon_degraded_reason(status, DaemonSyncState::Degraded),
        },
        _ => DaemonStartHandshakeStatus::Ready,
    }
}

fn daemon_degraded_reason(status: &str, state: DaemonSyncState) -> String {
    json_pointer_string(status, "/status/attentionItems/0")
        .unwrap_or_else(|| format!("sync state is {}", state.as_str()))
}

fn json_pointer_string(input: &str, pointer: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()?
        .pointer(pointer)?
        .as_str()
        .map(ToOwned::to_owned)
}

fn json_string_field(input: &str, field: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()?
        .get(field)?
        .as_str()
        .map(ToOwned::to_owned)
}

pub(super) fn wait_for_daemon_socket_to_stop(socket: &Path, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !socket.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
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
                    DaemonProcessState::Stopping,
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
                    DaemonProcessState::Stopped,
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

pub(super) fn print_diagnostics_collect(
    selection: WorkspaceSelection,
    socket: &Path,
    json: bool,
) -> ExitCode {
    let generated_at = generated_at();
    let bundle = diagnostics_bundle_text(socket, &generated_at, &selection);
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

pub(super) fn diagnostics_bundle_text(
    socket: &Path,
    generated_at: &str,
    selection: &WorkspaceSelection,
) -> String {
    let db_path = metadata_db_path_or_default();
    let state_root = db_path
        .as_ref()
        .ok()
        .and_then(|path| runtime::metadata_state_root(path))
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
        format!(
            "requested_root={}",
            resolve_explicit_path(selection.root.clone())
        ),
        format!(
            "requested_project={}",
            selection.project.as_deref().unwrap_or("unscoped")
        ),
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
