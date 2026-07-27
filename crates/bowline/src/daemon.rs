use super::*;
use bowline_core::commands::DaemonSyncState;
use bowline_core::status::StatusLevel;

/// Report an unresolvable command path. Human output goes through the same
/// printer as every other usage error so the remediation and next action reach
/// the audience that needs them most.
pub(super) fn print_unknown_command(command: &str, json: bool) -> CommandExitCode {
    print_usage_error(
        CommandName::Unknown,
        "unknown_command",
        &format!("unknown command `{command}`"),
        json,
    )
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
    /// A daemon owns the socket and speaks a wire generation this build cannot.
    VersionSkew,
    /// The socket exists but this process could not talk to it. Not `stopped`:
    /// a daemon may well be running.
    Unreachable,
}

impl DaemonProcessState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Starting => "starting",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::VersionSkew => "version-skew",
            Self::Unreachable => "unreachable",
        }
    }

    /// Render a failed connection as a process state, reusing the one shared
    /// classification rather than re-deciding what a failure means.
    pub(super) fn from_reachability(reachability: &bowline_daemon_rpc::DaemonReachability) -> Self {
        use bowline_daemon_rpc::DaemonReachability;
        match reachability {
            DaemonReachability::NotRunning => Self::Stopped,
            DaemonReachability::VersionSkew(_) => Self::VersionSkew,
            DaemonReachability::Unreachable => Self::Unreachable,
        }
    }
}

pub(super) fn daemon_service_state_from_status(status: &DaemonServiceStatus) -> DaemonServiceState {
    DaemonServiceState {
        state: status.state.to_string(),
        name: None,
        unit_path: status.unit_path.display().to_string(),
        unavailable_because: status.unavailable_because.clone(),
    }
}

pub(super) fn daemon_service_state_from_outcome(
    outcome: &DaemonServiceOutcome,
) -> DaemonServiceState {
    DaemonServiceState {
        state: outcome.state.to_string(),
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
        Ok(handshake) => match handshake_start_status(&handshake, &workspace_id) {
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
            if let Some(skew) = error.version_skew() {
                return replace_skewed_daemon(socket, &skew, generated_at, json);
            }
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

/// How long a supervisor-replaced daemon gets to rebind the control socket and
/// answer a handshake before `daemon start` reports the replacement as failed.
const SKEWED_DAEMON_REPLACEMENT_TIMEOUT: Duration = Duration::from_secs(15);
const SKEWED_DAEMON_PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// A daemon from another Bowline build owns the control socket. Spawning a
/// second process would lose the bind race and exit, so the only honest moves
/// are to replace the running daemon through the supervisor that owns it, or to
/// say plainly that a foreign daemon holds the socket.
fn replace_skewed_daemon(
    socket: &Path,
    skew: &bowline_daemon_rpc::VersionSkew,
    generated_at: String,
    json: bool,
) -> ExitCode {
    if !daemon_service_is_active() {
        return print_user_action_error(
            CommandName::DaemonStart,
            generated_at,
            "daemon_version_skew",
            &format!("{skew} and no managed service owns {}", socket.display()),
            "Stop the running daemon, then run `bowline daemon install` so Bowline manages it.",
            json,
        )
        .into();
    }
    if let Err(message) = daemon_service_restart() {
        return print_runtime_error(
            CommandName::DaemonStart,
            generated_at,
            &format!("{skew} and the managed service could not be restarted: {message}"),
            json,
        )
        .into();
    }
    match wait_for_compatible_daemon(socket, SKEWED_DAEMON_REPLACEMENT_TIMEOUT) {
        Some(handshake) => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStart,
                    generated_at,
                    socket,
                    DaemonProcessState::Running,
                    Some(&handshake.daemon_version),
                    None,
                    true,
                ));
            } else {
                println!(
                    "bowline daemon: replaced a mismatched daemon, now running (daemon {})",
                    handshake.daemon_version
                );
            }
            ExitCode::SUCCESS
        }
        None => print_runtime_error(
            CommandName::DaemonStart,
            generated_at,
            &format!(
                "{skew} and the restarted service did not answer on {}",
                socket.display()
            ),
            json,
        )
        .into(),
    }
}

fn wait_for_compatible_daemon(socket: &Path, timeout: Duration) -> Option<Handshake> {
    let started = Instant::now();
    loop {
        match handshake(socket) {
            Ok(handshake) => return Some(handshake),
            Err(_) if started.elapsed() < timeout => {
                std::thread::sleep(SKEWED_DAEMON_PROBE_INTERVAL);
            }
            Err(_) => return None,
        }
    }
}

pub(super) fn remove_stale_daemon_socket_after_connect_error(
    socket: &Path,
    error: &DaemonRpcError,
) -> io::Result<bool> {
    if error.io_kind() != Some(io::ErrorKind::ConnectionRefused) {
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

/// Decide whether a live daemon can serve this invocation, reading the decoded
/// snapshot directly. The status level is matched exhaustively: a level this
/// build does not recognise is a compile error here, never a silent `Ready`.
pub(super) fn handshake_start_status(
    handshake: &Handshake,
    workspace_id: &WorkspaceId,
) -> DaemonStartHandshakeStatus {
    let status = &handshake.status;
    if status.workspace_id != *workspace_id {
        return DaemonStartHandshakeStatus::WorkspaceMismatch;
    }
    let state = match status.status.level {
        StatusLevel::Healthy => return DaemonStartHandshakeStatus::Ready,
        StatusLevel::Limited => DaemonSyncState::Limited,
        StatusLevel::Attention => DaemonSyncState::Degraded,
    };
    DaemonStartHandshakeStatus::Degraded {
        reason: daemon_degraded_reason(status, state),
        state,
    }
}

fn daemon_degraded_reason(status: &StatusCommandOutput, state: DaemonSyncState) -> String {
    status
        .status
        .attention_items
        .first()
        .cloned()
        .unwrap_or_else(|| format!("sync state is {}", state.as_str()))
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
        Err(error) if error.daemon_is_absent() => {
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
        Err(error) => match error.version_skew() {
            Some(skew) => stop_skewed_daemon(socket, &skew, generated_at, json),
            None => print_runtime_error(
                CommandName::DaemonStop,
                generated_at,
                &error.to_string(),
                json,
            )
            .into(),
        },
    }
}

/// A daemon this build cannot speak to still has to be stoppable. The shutdown
/// RPC is unreachable across a contract skew, so fall through to the supervisor
/// that owns the process; an unmanaged foreign daemon is reported as such
/// instead of being mislabelled `stopped`.
fn stop_skewed_daemon(
    socket: &Path,
    skew: &bowline_daemon_rpc::VersionSkew,
    generated_at: String,
    json: bool,
) -> ExitCode {
    if !daemon_service_is_active() {
        return print_user_action_error(
            CommandName::DaemonStop,
            generated_at,
            "daemon_version_skew",
            &format!("{skew} and no managed service owns {}", socket.display()),
            "Stop that daemon process directly, then run `bowline daemon install` so Bowline manages it.",
            json,
        )
        .into();
    }
    if let Err(message) = daemon_service_stop() {
        return print_runtime_error(
            CommandName::DaemonStop,
            generated_at,
            &format!("{skew} and the managed service could not be stopped: {message}"),
            json,
        )
        .into();
    }
    let stopped = wait_for_daemon_socket_to_stop(socket, Duration::from_secs(3));
    let state = if stopped {
        DaemonProcessState::Stopped
    } else {
        DaemonProcessState::Stopping
    };
    if json {
        print_json(&daemon_command_output(
            CommandName::DaemonStop,
            generated_at,
            socket,
            state,
            None,
            None,
            false,
        ));
    } else {
        println!("bowline daemon: {} a mismatched daemon", state.as_str());
    }
    ExitCode::SUCCESS
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
