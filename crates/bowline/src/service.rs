use super::*;
use std::fmt;
use std::process::Stdio;

mod install;
pub(super) use install::daemon_service_install;
#[cfg(test)]
pub(super) use install::{
    daemon_service_active_from_status, install_daemon_service_with_takeover,
    previous_active_service_definition, stop_unmanaged_daemon, wait_for_stable_socket_absence,
};

pub(super) fn print_daemon_service_install(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match daemon_service_install(socket) {
        Ok(outcome) => {
            print_service_outcome(
                CommandName::DaemonInstall,
                "daemon install",
                &outcome,
                generated_at,
                json,
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_service_error(CommandName::DaemonInstall, "daemon install", &message, json);
            CommandExitCode::BlockedOrDegradedBySafety.into()
        }
    }
}

pub(super) fn print_daemon_service_restart(json: bool) -> ExitCode {
    let generated_at = generated_at();
    match daemon_service_restart() {
        Ok(outcome) => {
            print_service_outcome(
                CommandName::DaemonRestart,
                "daemon restart",
                &outcome,
                generated_at,
                json,
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_service_error(CommandName::DaemonRestart, "daemon restart", &message, json);
            CommandExitCode::BlockedOrDegradedBySafety.into()
        }
    }
}

pub(super) fn print_daemon_service_uninstall(json: bool) -> ExitCode {
    let generated_at = generated_at();
    match daemon_service_uninstall() {
        Ok(outcome) => {
            print_service_outcome(
                CommandName::DaemonUninstall,
                "daemon uninstall",
                &outcome,
                generated_at,
                json,
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_service_error(
                CommandName::DaemonUninstall,
                "daemon uninstall",
                &message,
                json,
            );
            CommandExitCode::BlockedOrDegradedBySafety.into()
        }
    }
}

pub(super) const UNSUPPORTED_PLATFORM_ERROR: &str =
    "daemon service commands are available only on Linux and macOS";

static SYSTEM_RUNNER: SystemProcessRunner = SystemProcessRunner;

/// The host's supervisor, bound to the real process runner. Every daemon
/// service command goes through here, so nothing above this line knows whether
/// launchd or systemd answered.
pub(super) fn platform_supervisor() -> Result<Box<dyn ServiceSupervisor + 'static>, String> {
    PlatformService::current()
        .ok_or_else(|| UNSUPPORTED_PLATFORM_ERROR.to_string())?
        .supervisor(&SYSTEM_RUNNER)
        .map_err(|error| error.to_string())
}

fn run_supervisor_command(
    operation: impl FnOnce(&dyn ServiceSupervisor) -> Result<ServiceOutcome, ServiceError>,
) -> Result<DaemonServiceOutcome, String> {
    let supervisor = platform_supervisor()?;
    operation(supervisor.as_ref())
        .map(DaemonServiceOutcome::from)
        .map_err(|error| error.to_string())
}

pub(super) fn daemon_service_restart() -> Result<DaemonServiceOutcome, String> {
    run_supervisor_command(|supervisor| supervisor.restart())
}

pub(super) fn daemon_service_uninstall() -> Result<DaemonServiceOutcome, String> {
    run_supervisor_command(|supervisor| supervisor.uninstall())
}

pub(super) fn print_service_outcome(
    command: CommandName,
    command_label: &str,
    outcome: &DaemonServiceOutcome,
    generated_at: String,
    json: bool,
) {
    if json {
        print_json(&DaemonServiceOutput {
            contract_version: CONTRACT_VERSION,
            command,
            generated_at,
            service: daemon_service_state_from_outcome(outcome),
        });
        return;
    }
    println!(
        "bowline {command_label}: {} ({})",
        outcome.state,
        outcome.unit_path.display()
    );
}

pub(super) fn print_service_error(
    command: CommandName,
    command_label: &str,
    message: &str,
    json: bool,
) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command,
            generated_at: generated_at(),
            status: CommandErrorStatus::Unsupported,
            error: CommandError {
                code: "service_unavailable".to_string(),
                message: message.to_string(),
                recoverability: CommandRecoverability::Unsupported,
                remediation: Some(
                    "Run `bowline daemon status --json` or retry on a supported OS.".to_string(),
                ),
                details: None,
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: vec![RepairCommand::inspect(
                "Inspect daemon status".to_string(),
                Some("bowline daemon status --json".to_string()),
            )],
        });
        return;
    }
    eprintln!("bowline {command_label} unavailable: {message}");
}

/// The daemon's state under whichever OS supervisor owns it, normalized across
/// launchd and systemd. Supervisor states cross module boundaries as this enum
/// and are serialized only at the JSON edge, so no consumer re-parses a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ServiceSupervisorState {
    Active,
    Inactive,
    Installed,
    Restarted,
    Uninstalled,
    /// A supervisor owns the daemon but could not be queried; the reason travels
    /// alongside in `DaemonServiceStatus::unavailable_because`.
    Unavailable,
    /// No supported supervisor exists on this host.
    Unsupported,
    /// A supervisor label this build does not model, preserved verbatim.
    Unrecognized(String),
}

impl fmt::Display for ServiceSupervisorState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Installed => "installed",
            Self::Restarted => "restarted",
            Self::Uninstalled => "uninstalled",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::Unrecognized(label) => label,
        })
    }
}

impl From<ServiceState> for ServiceSupervisorState {
    fn from(state: ServiceState) -> Self {
        match state {
            ServiceState::Active => Self::Active,
            ServiceState::Inactive => Self::Inactive,
            ServiceState::Installed => Self::Installed,
            ServiceState::Restarted => Self::Restarted,
            ServiceState::Uninstalled => Self::Uninstalled,
            ServiceState::Unknown(label) => Self::Unrecognized(label),
        }
    }
}

impl From<ServiceOutcome> for DaemonServiceOutcome {
    fn from(outcome: ServiceOutcome) -> Self {
        Self {
            service_name: outcome.service_name,
            unit_path: outcome.unit_path,
            state: outcome.state.into(),
        }
    }
}

pub(super) fn start_daemon_process(socket: &Path) -> Result<u32, String> {
    let launch = daemon_launch_config(socket)?;
    let log_path = launch.state_root.join("bowline-daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| format!("failed to open daemon log {}: {error}", log_path.display()))?;
    let err = log
        .try_clone()
        .map_err(|error| format!("failed to clone daemon log handle: {error}"))?;
    let mut command = ProcessCommand::new(launch.daemon);
    command
        .envs(persisted_daemon_env(&launch.state_root))
        .arg("serve")
        .arg("--socket")
        .arg(&launch.socket)
        .arg("--sync-root")
        .arg(&launch.root)
        .arg("--sync-state-root")
        .arg(&launch.state_root)
        .arg("--sync-workspace")
        .arg(launch.workspace_id.as_str())
        .arg("--sync-device")
        .arg(launch.device_id.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("failed to start bowline-daemon: {error}"))?;
    Ok(child.id())
}

pub(super) struct DaemonLaunchConfig {
    pub(super) state_root: PathBuf,
    pub(super) workspace_id: bowline_core::ids::WorkspaceId,
    pub(super) root: PathBuf,
    pub(super) daemon: PathBuf,
    pub(super) socket: PathBuf,
    pub(super) device_id: bowline_core::ids::DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DaemonServiceStatus {
    pub(super) state: ServiceSupervisorState,
    pub(super) unit_path: PathBuf,
    pub(super) unavailable_because: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DaemonServiceOutcome {
    pub(super) service_name: String,
    pub(super) unit_path: PathBuf,
    pub(super) state: ServiceSupervisorState,
}

pub(super) fn daemon_launch_config(socket: &Path) -> Result<DaemonLaunchConfig, String> {
    let db_path = metadata_db_path_or_default()?;
    let store = MetadataStore::open(&db_path).map_err(|error| error.to_string())?;
    let state_root = runtime::metadata_state_root(&db_path)
        .ok_or_else(|| "metadata database path could not be resolved".to_string())?;
    let workspace_id = daemon_workspace_id_for_store(&store)?;
    let root = store
        .accepted_roots(&workspace_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .next()
        .ok_or_else(|| {
            "no accepted workspace root; run `bowline setup --root <path>` first".to_string()
        })?;
    let root = expand_home_path(&root);
    let daemon = daemon_binary_path()?;
    let device_id = daemon_device_id_for_launch(&state_root, &workspace_id);
    Ok(DaemonLaunchConfig {
        state_root,
        workspace_id,
        root,
        daemon,
        socket: socket.to_path_buf(),
        device_id,
    })
}

/// What the host's supervisor should run, prepared once for every platform.
pub(super) fn daemon_service_config(socket: &Path) -> Result<ServiceConfig, String> {
    let launch = daemon_service_launch_config(socket)?;
    std::fs::create_dir_all(&launch.root).map_err(|error| {
        format!(
            "failed to prepare daemon root {}: {error}",
            launch.root.display()
        )
    })?;
    Ok(ServiceConfig {
        daemon: launch.daemon,
        root: launch.root,
        state_root: launch.state_root,
        socket: launch.socket,
        workspace_id: launch.workspace_id.as_str().to_string(),
        device_id: launch.device_id.as_str().to_string(),
    })
}

pub(super) fn daemon_service_launch_config(socket: &Path) -> Result<DaemonLaunchConfig, String> {
    let db_path = metadata_db_path_or_default()?;
    let store = MetadataStore::open(&db_path).map_err(|error| error.to_string())?;
    daemon_service_launch_config_for_store(socket, &db_path, &store, daemon_binary_path()?)
}

pub(super) fn daemon_service_launch_config_for_store(
    socket: &Path,
    db_path: &Path,
    store: &MetadataStore,
    daemon: PathBuf,
) -> Result<DaemonLaunchConfig, String> {
    let state_root = runtime::metadata_state_root(db_path)
        .ok_or_else(|| "metadata database path could not be resolved".to_string())?;
    let workspace_id = daemon_workspace_id_for_store(store)?;
    if workspace_id.as_str() == "ws_code" {
        return Err(
            "daemon service setup requires an authenticated workspace; run `bowline setup --root <path>` first"
                .to_string(),
        );
    }
    let root = store
        .accepted_roots(&workspace_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .next()
        .map(|root| expand_home_path(&root))
        .ok_or_else(|| {
            "daemon service setup requires an accepted workspace root; run `bowline setup --root <path>` first"
                .to_string()
        })?;
    let device_id = daemon_device_id_for_launch(&state_root, &workspace_id);
    Ok(DaemonLaunchConfig {
        state_root,
        workspace_id,
        root,
        daemon,
        socket: socket.to_path_buf(),
        device_id,
    })
}

pub(super) fn persisted_daemon_env(
    state_root: &Path,
) -> std::collections::BTreeMap<String, String> {
    bowline_local::daemon_env::read(state_root)
}

pub(super) fn daemon_device_id_for_launch(
    state_root: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> bowline_core::ids::DeviceId {
    persisted_daemon_device_id_for_workspace(state_root, workspace_id)
        .map(bowline_core::ids::DeviceId::new)
        .unwrap_or_else(|| runtime::daemon_device_id(workspace_id))
}

pub(super) fn persisted_daemon_device_id_for_workspace(
    state_root: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> Option<String> {
    runtime::persisted_daemon_device_id_for_workspace(state_root, workspace_id)
}

pub(super) fn daemon_workspace_id_for_start() -> Result<bowline_core::ids::WorkspaceId, String> {
    let db_path = metadata_db_path_or_default()?;
    let store = MetadataStore::open(&db_path).map_err(|error| error.to_string())?;
    daemon_workspace_id_for_store(&store)
}

pub(super) fn daemon_workspace_id_for_store(
    store: &MetadataStore,
) -> Result<bowline_core::ids::WorkspaceId, String> {
    let active = runtime::active_workspace_id();
    if !store
        .accepted_roots(&active)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Ok(active);
    }
    if std::env::var("BOWLINE_WORKSPACE_ID")
        .ok()
        .is_some_and(|value| !value.is_empty())
    {
        return Ok(active);
    }
    if let Some(workspace) = store
        .current_workspace()
        .map_err(|error| error.to_string())?
        && !store
            .accepted_roots(&workspace.id)
            .map_err(|error| error.to_string())?
            .is_empty()
    {
        return Ok(workspace.id);
    }
    Ok(active)
}

pub(super) fn metadata_db_path_or_default() -> Result<PathBuf, String> {
    metadata_db_path()
        .or_else(|| default_database_path().ok())
        .ok_or_else(|| "metadata database path is unavailable".to_string())
}

pub(super) fn daemon_binary_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("BOWLINE_DAEMON_BIN") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }
    let current = env::current_exe().map_err(|error| error.to_string())?;
    daemon_binary_path_next_to(&current)
}

pub(super) fn daemon_binary_path_next_to(current: &Path) -> Result<PathBuf, String> {
    let daemon_name = if cfg!(windows) {
        "bowline-daemon.exe"
    } else {
        "bowline-daemon"
    };
    let sibling = current.with_file_name(daemon_name);
    if sibling.exists() {
        return validate_daemon_binary_path(sibling);
    }
    if let Some(debug_dir) = current.parent().and_then(Path::parent) {
        let target_debug = debug_dir.join(daemon_name);
        if target_debug.exists() {
            return validate_daemon_binary_path(target_debug);
        }
    }
    validate_daemon_binary_path(sibling)
}

pub(super) fn validate_daemon_binary_path(daemon: PathBuf) -> Result<PathBuf, String> {
    let metadata = std::fs::metadata(&daemon).map_err(|error| {
        format!(
            "bowline-daemon binary is unavailable at {}: {error}",
            daemon.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "bowline-daemon binary is unavailable at {}: not a file",
            daemon.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "bowline-daemon binary is unavailable at {}: not executable",
                daemon.display()
            ));
        }
    }
    Ok(daemon)
}

pub(super) fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

pub(super) fn print_daemon_status(socket: &Path, json: bool) {
    let service = daemon_service_status(&SystemProcessRunner);
    match handshake(socket) {
        Ok(handshake) => {
            if json {
                println!(
                    "{}",
                    daemon_status_json(DaemonStatusReport {
                        socket,
                        state: DaemonProcessState::Running,
                        daemon_version: Some(&handshake.daemon_version),
                        sync: Some(&handshake.status),
                        unavailable_because: None,
                        service: service.as_ref(),
                    })
                );
            } else {
                println!(
                    "bowline daemon: running ({PROTOCOL} v{PROTOCOL_VERSION}, daemon {})",
                    handshake.daemon_version
                );
                print_daemon_service_status_human(service.as_ref());
            }
        }
        // Reporting `stopped` for every failed connection is what made version
        // skew invisible: a skewed or unresponsive daemon is running and owns
        // the socket. Report what the shared classification actually says.
        Err(error) => {
            let reachability = error.reachability();
            let state = DaemonProcessState::from_reachability(&reachability);
            if json {
                println!(
                    "{}",
                    daemon_status_json(DaemonStatusReport {
                        socket,
                        state,
                        daemon_version: None,
                        sync: None,
                        unavailable_because: (state != DaemonProcessState::Stopped)
                            .then(|| reachability.to_string()),
                        service: service.as_ref(),
                    })
                );
            } else {
                println!("bowline daemon: {}", state.as_str());
                if state != DaemonProcessState::Stopped {
                    println!("  {reachability}");
                    println!("Next: {}", reachability.remediation());
                }
                print_daemon_service_status_human(service.as_ref());
            }
        }
    }
}

pub(super) struct DaemonStatusReport<'a> {
    pub(super) socket: &'a Path,
    pub(super) state: DaemonProcessState,
    pub(super) daemon_version: Option<&'a str>,
    pub(super) sync: Option<&'a StatusCommandOutput>,
    pub(super) unavailable_because: Option<String>,
    pub(super) service: Option<&'a DaemonServiceStatus>,
}

pub(super) fn daemon_status_json(report: DaemonStatusReport<'_>) -> String {
    let mut daemon = daemon_process_output(
        report.socket,
        report.state,
        report.daemon_version,
        None,
        true,
    );
    daemon.unavailable_because = report.unavailable_because;
    serde_json::to_string(&DaemonStatusOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::DaemonStatus,
        generated_at: generated_at(),
        daemon,
        sync: report
            .sync
            .and_then(|status| crate::wire::status_snapshot_wire_value(status).ok()),
        service: report.service.map(daemon_service_state_from_status),
    })
    .expect("daemon status output should serialize")
}

/// Which OS service supervisor owns the daemon on this host. Platform support is
/// the single source of truth for the manager label.
pub(super) fn daemon_service_manager() -> bowline_core::introspection::ServiceManager {
    PlatformService::current().map_or(
        bowline_core::introspection::ServiceManager::None,
        PlatformService::manager,
    )
}

/// Compact service view for `bowline status --json`. Always returns a value: an
/// unsupported host reports `manager: none` with an honest `unsupported` state.
pub(super) fn daemon_service_introspection() -> bowline_core::introspection::ServiceIntrospection {
    daemon_service_introspection_from(daemon_service_status(&SystemProcessRunner))
}

fn daemon_service_introspection_from(
    status: Option<DaemonServiceStatus>,
) -> bowline_core::introspection::ServiceIntrospection {
    bowline_core::introspection::ServiceIntrospection {
        state: status
            .map_or(ServiceSupervisorState::Unsupported, |status| status.state)
            .to_string(),
        manager: daemon_service_manager(),
    }
}

/// Whether an OS supervisor currently owns a running daemon. This is the only
/// lever the CLI has over a daemon it cannot speak to over the control socket.
pub(super) fn daemon_service_is_active() -> bool {
    daemon_service_status(&SystemProcessRunner)
        .is_some_and(|status| status.state == ServiceSupervisorState::Active)
}

pub(super) fn daemon_service_stop() -> Result<DaemonServiceOutcome, String> {
    run_supervisor_command(|supervisor| supervisor.stop())
}

pub(super) fn daemon_service_status<R>(runner: &R) -> Option<DaemonServiceStatus>
where
    R: ProcessRunner,
{
    let platform = PlatformService::current()?;
    let supervisor = match platform.supervisor(runner) {
        Ok(supervisor) => supervisor,
        // The supervisor exists but its session does not, so there is no
        // definition path to report — only the name it would have had.
        Err(error) => {
            return Some(DaemonServiceStatus {
                state: ServiceSupervisorState::Unavailable,
                unit_path: PathBuf::from(platform.service_name()),
                unavailable_because: Some(error.to_string()),
            });
        }
    };
    Some(match supervisor.status() {
        Ok(outcome) => DaemonServiceStatus {
            state: outcome.state.into(),
            unit_path: outcome.unit_path,
            unavailable_because: None,
        },
        Err(error) => DaemonServiceStatus {
            state: ServiceSupervisorState::Unavailable,
            unit_path: supervisor.definition_path(),
            unavailable_because: Some(error.to_string()),
        },
    })
}

#[cfg(test)]
pub(super) fn daemon_service_status_json(status: &DaemonServiceStatus) -> String {
    serde_json::to_string(&daemon_service_state_from_status(status))
        .expect("daemon service status should serialize")
}

pub(super) fn print_daemon_service_status_human(status: Option<&DaemonServiceStatus>) {
    let Some(status) = status else {
        return;
    };
    match &status.unavailable_because {
        Some(message) => println!(
            "bowline service: unavailable ({}, {})",
            status.unit_path.display(),
            message
        ),
        None => println!(
            "bowline service: {} ({})",
            status.state,
            status.unit_path.display()
        ),
    }
}
