use super::*;
use std::process::Stdio;

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

pub(super) fn daemon_service_install(socket: &Path) -> Result<DaemonServiceOutcome, String> {
    if linux_service::current_platform_supported() {
        return daemon_linux_service_options(socket).and_then(|options| {
            linux_service::install_or_update_service(&SystemProcessRunner, &options)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_options(socket).and_then(|options| {
            macos_service::install_or_update_service(&SystemProcessRunner, &options)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    Err("daemon service commands are available only on Linux and macOS".to_string())
}

pub(super) fn daemon_service_restart() -> Result<DaemonServiceOutcome, String> {
    if linux_service::current_platform_supported() {
        return daemon_linux_unit_dir().and_then(|unit_dir| {
            linux_service::restart_service(&SystemProcessRunner, &unit_dir)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_location().and_then(|(launch_agents_dir, launch_domain)| {
            macos_service::restart_service(&SystemProcessRunner, &launch_agents_dir, &launch_domain)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    Err("daemon service commands are available only on Linux and macOS".to_string())
}

pub(super) fn daemon_service_uninstall() -> Result<DaemonServiceOutcome, String> {
    if linux_service::current_platform_supported() {
        return daemon_linux_unit_dir().and_then(|unit_dir| {
            linux_service::uninstall_service(&SystemProcessRunner, &unit_dir)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_location().and_then(|(launch_agents_dir, launch_domain)| {
            macos_service::uninstall_service(
                &SystemProcessRunner,
                &launch_agents_dir,
                &launch_domain,
            )
            .map(DaemonServiceOutcome::from)
            .map_err(|error| error.to_string())
        });
    }
    Err("daemon service commands are available only on Linux and macOS".to_string())
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

impl From<linux_service::LinuxServiceOutcome> for DaemonServiceOutcome {
    fn from(outcome: linux_service::LinuxServiceOutcome) -> Self {
        Self {
            service_name: outcome.service_name,
            unit_path: outcome.unit_path,
            state: outcome.state.to_string(),
        }
    }
}

impl From<macos_service::MacosServiceOutcome> for DaemonServiceOutcome {
    fn from(outcome: macos_service::MacosServiceOutcome) -> Self {
        Self {
            service_name: outcome.service_name,
            unit_path: outcome.unit_path,
            state: outcome.state.to_string(),
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
    pub(super) state: String,
    pub(super) unit_path: PathBuf,
    pub(super) unavailable_because: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DaemonServiceOutcome {
    pub(super) service_name: String,
    pub(super) unit_path: PathBuf,
    pub(super) state: String,
}

pub(super) fn daemon_launch_config(socket: &Path) -> Result<DaemonLaunchConfig, String> {
    let db_path = metadata_db_path_or_default()?;
    let state_root = db_path
        .parent()
        .ok_or_else(|| "metadata database path has no parent directory".to_string())?
        .to_path_buf();
    let store = MetadataStore::open(&db_path).map_err(|error| error.to_string())?;
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

pub(super) fn daemon_linux_service_options(socket: &Path) -> Result<LinuxServiceOptions, String> {
    ensure_linux_service_supported()?;
    let launch = daemon_service_launch_config(socket)?;
    std::fs::create_dir_all(&launch.root).map_err(|error| {
        format!(
            "failed to prepare daemon root {}: {error}",
            launch.root.display()
        )
    })?;
    let unit_dir = daemon_linux_unit_dir()?;
    Ok(LinuxServiceOptions {
        unit_dir,
        config: LinuxServiceConfig {
            daemon: launch.daemon,
            root: launch.root,
            state_root: launch.state_root,
            socket: launch.socket,
            workspace_id: launch.workspace_id.as_str().to_string(),
            device_id: launch.device_id.as_str().to_string(),
        },
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
    let state_root = db_path
        .parent()
        .ok_or_else(|| "metadata database path has no parent directory".to_string())?
        .to_path_buf();
    let workspace_id = daemon_workspace_id_for_store(store)?;
    let root = store
        .accepted_roots(&workspace_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .next()
        .map(|root| Ok(expand_home_path(&root)))
        .unwrap_or_else(|| seed_default_daemon_root(store, &workspace_id))?;
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

pub(super) fn seed_default_daemon_root(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> Result<PathBuf, String> {
    let now = generated_at();
    store
        .insert_workspace(workspace_id, "Code", &now)
        .map_err(|error| error.to_string())?;
    store
        .insert_root(
            &format!("root_{}", workspace_id.as_str()),
            workspace_id,
            "~/Code",
            &now,
        )
        .map_err(|error| error.to_string())?;
    Ok(default_workspace_root())
}

pub(super) fn daemon_linux_unit_dir() -> Result<PathBuf, String> {
    ensure_linux_service_supported()?;
    linux_service::default_user_unit_dir().map_err(|error| error.to_string())
}

pub(super) fn daemon_macos_service_options(socket: &Path) -> Result<MacosServiceOptions, String> {
    ensure_macos_service_supported()?;
    let launch = daemon_service_launch_config(socket)?;
    std::fs::create_dir_all(&launch.root).map_err(|error| {
        format!(
            "failed to prepare daemon root {}: {error}",
            launch.root.display()
        )
    })?;
    let (launch_agents_dir, launch_domain) = daemon_macos_service_location()?;
    Ok(MacosServiceOptions {
        launch_agents_dir,
        launch_domain,
        config: MacosServiceConfig {
            daemon: launch.daemon,
            root: launch.root,
            state_root: launch.state_root,
            socket: launch.socket,
            workspace_id: launch.workspace_id.as_str().to_string(),
            device_id: launch.device_id.as_str().to_string(),
        },
    })
}

pub(super) fn daemon_macos_service_location() -> Result<(PathBuf, String), String> {
    ensure_macos_service_supported()?;
    let launch_agents_dir =
        macos_service::default_launch_agents_dir().map_err(|error| error.to_string())?;
    let launch_domain =
        macos_service::default_launch_domain().map_err(|error| error.to_string())?;
    Ok((launch_agents_dir, launch_domain))
}

pub(super) fn ensure_linux_service_supported() -> Result<(), String> {
    if linux_service::current_platform_supported() {
        Ok(())
    } else {
        Err("Linux user service commands are available only on Linux".to_string())
    }
}

pub(super) fn ensure_macos_service_supported() -> Result<(), String> {
    if macos_service::current_platform_supported() {
        Ok(())
    } else {
        Err("macOS daemon service commands are available only on macOS".to_string())
    }
}

pub(super) fn persisted_daemon_env(state_root: &Path) -> Vec<(String, String)> {
    let Ok(contents) = std::fs::read_to_string(state_root.join("daemon.env")) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, value)| valid_persisted_daemon_env_key(key) && !value.is_empty())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
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

#[cfg(test)]
pub(super) fn persisted_daemon_env_value(state_root: &Path, name: &str) -> Option<String> {
    persisted_daemon_env(state_root)
        .into_iter()
        .find_map(|(key, value)| (key == name).then_some(value))
}

pub(super) fn valid_persisted_daemon_env_key(key: &str) -> bool {
    matches!(
        key,
        "CONVEX_URL"
            | "BOWLINE_WORKSPACE_ID"
            | "BOWLINE_DEVICE_ID"
            | "BOWLINE_DEVICE_NAME"
            | "BOWLINE_SECRET_STORE"
            | "BOWLINE_ACCOUNT_SESSION_ID"
            | "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN"
            | "BOWLINE_CONTROL_PLANE_TOKEN"
            | "BOWLINE_WORKOS_ACCESS_TOKEN"
            | "BOWLINE_WORKOS_CLIENT_ID"
    )
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

pub(super) fn default_workspace_root() -> PathBuf {
    expand_home_path("~/Code")
}

pub(super) fn print_daemon_status(socket: &Path, json: bool) {
    let service = daemon_service_status(&SystemProcessRunner);
    match handshake(socket) {
        Ok(handshake) => {
            if json {
                println!(
                    "{}",
                    daemon_status_json(
                        socket,
                        DaemonProcessState::Running,
                        Some(&handshake.daemon_version),
                        Some(&handshake.status_json),
                        service.as_ref()
                    )
                );
            } else {
                println!(
                    "bowline daemon: running ({PROTOCOL} v{PROTOCOL_VERSION}, daemon {})",
                    handshake.daemon_version
                );
                print_daemon_service_status_human(service.as_ref());
            }
        }
        Err(_) => {
            if json {
                println!(
                    "{}",
                    daemon_status_json(
                        socket,
                        DaemonProcessState::Stopped,
                        None,
                        None,
                        service.as_ref()
                    )
                );
            } else {
                println!("bowline daemon: stopped");
                print_daemon_service_status_human(service.as_ref());
            }
        }
    }
}

pub(super) fn daemon_status_json(
    socket: &Path,
    state: DaemonProcessState,
    daemon_version: Option<&str>,
    status_json: Option<&str>,
    service: Option<&DaemonServiceStatus>,
) -> String {
    serde_json::to_string(&DaemonStatusOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::DaemonStatus,
        generated_at: generated_at(),
        daemon: daemon_process_output(socket, state, daemon_version, None, true),
        sync: status_json.and_then(|status| serde_json::from_str(status).ok()),
        service: service.map(daemon_service_state_from_status),
    })
    .expect("daemon status output should serialize")
}

pub(super) fn daemon_service_status<R>(runner: &R) -> Option<DaemonServiceStatus>
where
    R: ProcessRunner,
{
    if linux_service::current_platform_supported() {
        return daemon_linux_service_status(runner);
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_status(runner);
    }
    None
}

pub(super) fn daemon_linux_service_status<R>(runner: &R) -> Option<DaemonServiceStatus>
where
    R: ProcessRunner,
{
    let unit_dir = match linux_service::default_user_unit_dir() {
        Ok(unit_dir) => unit_dir,
        Err(error) => {
            return Some(DaemonServiceStatus {
                state: "unavailable".to_string(),
                unit_path: PathBuf::from(linux_service::SERVICE_NAME),
                unavailable_because: Some(error.to_string()),
            });
        }
    };
    match linux_service::service_status(runner, &unit_dir) {
        Ok(outcome) => Some(DaemonServiceStatus {
            state: outcome.state.to_string(),
            unit_path: outcome.unit_path,
            unavailable_because: None,
        }),
        Err(error) => Some(DaemonServiceStatus {
            state: "unavailable".to_string(),
            unit_path: linux_service::unit_path(&unit_dir),
            unavailable_because: Some(error.to_string()),
        }),
    }
}

pub(super) fn daemon_macos_service_status<R>(runner: &R) -> Option<DaemonServiceStatus>
where
    R: ProcessRunner,
{
    let (launch_agents_dir, launch_domain) = match daemon_macos_service_location() {
        Ok(location) => location,
        Err(error) => {
            return Some(DaemonServiceStatus {
                state: "unavailable".to_string(),
                unit_path: PathBuf::from(macos_service::PLIST_NAME),
                unavailable_because: Some(error),
            });
        }
    };
    match macos_service::service_status(runner, &launch_agents_dir, &launch_domain) {
        Ok(outcome) => Some(DaemonServiceStatus {
            state: outcome.state.to_string(),
            unit_path: outcome.unit_path,
            unavailable_because: None,
        }),
        Err(error) => Some(DaemonServiceStatus {
            state: "unavailable".to_string(),
            unit_path: macos_service::plist_path(&launch_agents_dir),
            unavailable_because: Some(error.to_string()),
        }),
    }
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
