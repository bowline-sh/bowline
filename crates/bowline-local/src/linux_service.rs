use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    bootstrap::process::ProcessRunner,
    service_runtime::{self, ServiceRuntimeError},
};

pub const SERVICE_NAME: &str = "bowline.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxServiceConfig {
    pub daemon: PathBuf,
    pub root: PathBuf,
    pub state_root: PathBuf,
    pub socket: PathBuf,
    pub workspace_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxServiceOptions {
    pub unit_dir: PathBuf,
    pub config: LinuxServiceConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxServiceOutcome {
    pub service_name: String,
    pub unit_path: PathBuf,
    pub state: LinuxServiceState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxServiceState {
    Installed,
    Restarted,
    Uninstalled,
    Active,
    Inactive,
    Unknown(String),
}

service_runtime::service_error! {
    pub enum LinuxServiceError {
        MissingHome => "HOME is unavailable",
    }
    io_context: "service file operation failed",
}

service_runtime::service_outcome_parts!(LinuxServiceOutcome, LinuxServiceState);

pub fn current_platform_supported() -> bool {
    cfg!(target_os = "linux")
}

pub fn default_user_unit_dir() -> Result<PathBuf, LinuxServiceError> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME")
        && !config_home.is_empty()
    {
        return Ok(PathBuf::from(config_home).join("systemd").join("user"));
    }
    let Some(home) = env::var_os("HOME") else {
        return Err(LinuxServiceError::MissingHome);
    };
    Ok(PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user"))
}

pub fn install_or_update_service<R>(
    runner: &R,
    options: &LinuxServiceOptions,
) -> Result<LinuxServiceOutcome, LinuxServiceError>
where
    R: ProcessRunner,
{
    fs::create_dir_all(&options.unit_dir)?;
    let unit_path = unit_path(&options.unit_dir);
    fs::write(&unit_path, render_systemd_user_unit(&options.config))?;
    run_systemctl(runner, &["daemon-reload"])?;
    run_systemctl(runner, &["enable", SERVICE_NAME])?;
    run_systemctl(runner, &["restart", SERVICE_NAME])?;
    Ok(outcome(unit_path, LinuxServiceState::Installed))
}

pub fn restart_service<R>(
    runner: &R,
    unit_dir: &Path,
) -> Result<LinuxServiceOutcome, LinuxServiceError>
where
    R: ProcessRunner,
{
    run_systemctl(runner, &["restart", SERVICE_NAME])?;
    Ok(outcome(unit_path(unit_dir), LinuxServiceState::Restarted))
}

pub fn uninstall_service<R>(
    runner: &R,
    unit_dir: &Path,
) -> Result<LinuxServiceOutcome, LinuxServiceError>
where
    R: ProcessRunner,
{
    let path = unit_path(unit_dir);
    match run_systemctl(runner, &["disable", "--now", SERVICE_NAME]) {
        Ok(()) => {}
        Err(error) if missing_unit_error(&error) => {}
        Err(error) => return Err(error),
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    run_systemctl(runner, &["daemon-reload"])?;
    Ok(outcome(path, LinuxServiceState::Uninstalled))
}

pub fn service_status<R>(
    runner: &R,
    unit_dir: &Path,
) -> Result<LinuxServiceOutcome, LinuxServiceError>
where
    R: ProcessRunner,
{
    let output = service_runtime::run_service_command(
        runner,
        "systemctl",
        [
            "--user",
            "show",
            SERVICE_NAME,
            "--property=ActiveState",
            "--value",
        ],
        |_| false,
        systemctl_failure,
    )
    .map_err(LinuxServiceError::from)?;
    let active = output.stdout.lines().next().unwrap_or("").trim();
    let state = match active {
        "active" => LinuxServiceState::Active,
        "inactive" | "deactivating" | "activating" | "" => LinuxServiceState::Inactive,
        other => LinuxServiceState::Unknown(other.to_string()),
    };
    Ok(outcome(unit_path(unit_dir), state))
}

pub fn render_systemd_user_unit(config: &LinuxServiceConfig) -> String {
    let daemon_env = config.state_root.join("daemon.env");
    format!(
        "[Unit]\nDescription=bowline daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nEnvironmentFile=-{}\nWorkingDirectory={}\nExecStart={} serve --socket {} --sync-root {} --sync-state-root {} --sync-workspace {} --sync-device {} --notify-approvals\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote_arg(&daemon_env),
        systemd_quote_arg(&config.root),
        systemd_quote_arg(&config.daemon),
        systemd_quote_arg(&config.socket),
        systemd_quote_arg(&config.root),
        systemd_quote_arg(&config.state_root),
        systemd_quote_value(&config.workspace_id),
        systemd_quote_value(&config.device_id),
    )
}

pub fn unit_path(unit_dir: &Path) -> PathBuf {
    unit_dir.join(SERVICE_NAME)
}

fn outcome(unit_path: PathBuf, state: LinuxServiceState) -> LinuxServiceOutcome {
    service_runtime::service_outcome(SERVICE_NAME, unit_path, state)
}

fn run_systemctl<R>(runner: &R, args: &[&str]) -> Result<(), LinuxServiceError>
where
    R: ProcessRunner,
{
    service_runtime::run_service_command(
        runner,
        "systemctl",
        ["--user"].into_iter().chain(args.iter().copied()),
        |_| false,
        systemctl_failure,
    )
    .map(|_| ())
    .map_err(LinuxServiceError::from)
}

fn systemctl_failure(failure: service_runtime::CommandFailure) -> ServiceRuntimeError {
    service_runtime::classify_command_failure(
        failure,
        user_manager_unavailable,
        "systemd user manager is unavailable; start a user session or enable lingering",
    )
}

fn user_manager_unavailable(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("failed to connect to bus")
        || lower.contains("no medium found")
        || lower.contains("no such file or directory")
}

fn missing_unit_error(error: &LinuxServiceError) -> bool {
    let LinuxServiceError::CommandFailed { stderr, .. } = error else {
        return false;
    };
    let lower = stderr.to_ascii_lowercase();
    lower.contains("could not be found")
        || lower.contains("not loaded")
        || lower.contains("not found")
}

fn systemd_quote_arg(path: &Path) -> String {
    systemd_quote_value(&path.display().to_string())
}

fn systemd_quote_value(value: &str) -> String {
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':' | b'@')
    }) {
        return value.to_string();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            character if character.is_control() => {
                for byte in character.to_string().as_bytes() {
                    quoted.push_str(&format!("\\x{byte:02x}"));
                }
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

impl fmt::Display for LinuxServiceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Installed => formatter.write_str("installed"),
            Self::Restarted => formatter.write_str("restarted"),
            Self::Uninstalled => formatter.write_str("uninstalled"),
            Self::Active => formatter.write_str("active"),
            Self::Inactive => formatter.write_str("inactive"),
            Self::Unknown(state) => formatter.write_str(state),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::{
        bootstrap::process::ProcessOutput,
        service_runtime::test_support::{RecordingRunner, SequenceRunner},
    };

    use super::{
        LinuxServiceConfig, LinuxServiceOptions, LinuxServiceState, install_or_update_service,
        render_systemd_user_unit, restart_service, service_status, uninstall_service, unit_path,
    };

    #[test]
    fn rendered_unit_runs_daemon_serve_directly() {
        let unit = render_systemd_user_unit(&config_with_spaces());

        assert!(unit.contains("[Service]"));
        assert!(unit.contains("EnvironmentFile=-\"/tmp/bowline state/daemon.env\""));
        assert!(unit.contains("WorkingDirectory=\"/tmp/Code Root\""));
        assert!(unit.contains("ExecStart=/tmp/bin/bowline-daemon serve"));
        assert!(unit.contains("--socket /tmp/bowline.sock"));
        assert!(unit.contains("--sync-root \"/tmp/Code Root\""));
        assert!(unit.contains("--sync-state-root \"/tmp/bowline state\""));
        assert!(unit.contains("--sync-workspace ws_code"));
        assert!(unit.contains("--sync-device device-linux"));
        assert!(unit.contains("--notify-approvals"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn rendered_unit_escapes_systemd_specifiers() {
        let mut config = config_with_spaces();
        config.root = PathBuf::from("/tmp/Code%Root");
        config.device_id = "device-$USER".to_string();

        let unit = render_systemd_user_unit(&config);

        assert!(unit.contains("--sync-root \"/tmp/Code%%Root\""));
        assert!(unit.contains("--sync-device \"device-$$USER\""));
    }

    #[test]
    fn rendered_unit_escapes_control_characters() {
        let mut config = config_with_spaces();
        config.root = PathBuf::from("/tmp/Code\nExecStart=/bin/false");

        let unit = render_systemd_user_unit(&config);

        assert!(!unit.contains("\nExecStart=/bin/false"));
        assert!(unit.contains("--sync-root \"/tmp/Code\\x0aExecStart=/bin/false\""));
    }

    #[test]
    fn install_writes_unit_and_enables_service() {
        let temp = tempfile_dir("bowline-service-install");
        let runner = RecordingRunner::ok();
        let options = LinuxServiceOptions {
            unit_dir: temp.clone(),
            config: config_with_spaces(),
        };

        let outcome = install_or_update_service(&runner, &options).expect("install service");

        assert_eq!(outcome.state, LinuxServiceState::Installed);
        assert_eq!(outcome.unit_path, unit_path(&temp));
        assert!(
            fs::read_to_string(unit_path(&temp))
                .expect("unit")
                .contains("bowline daemon")
        );
        assert_eq!(
            *runner.calls.borrow(),
            vec![
                vec!["systemctl", "--user", "daemon-reload"],
                vec!["systemctl", "--user", "enable", "bowline.service"],
                vec!["systemctl", "--user", "restart", "bowline.service"],
            ]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn reinstall_overwrites_changed_unit() {
        let temp = tempfile_dir("bowline-service-reinstall");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "old").expect("old unit");
        let runner = RecordingRunner::ok();

        install_or_update_service(
            &runner,
            &LinuxServiceOptions {
                unit_dir: temp.clone(),
                config: config_with_spaces(),
            },
        )
        .expect("install service");

        assert!(
            fs::read_to_string(unit_path(&temp))
                .expect("unit")
                .contains("ExecStart=")
        );
        assert_eq!(runner.calls.borrow().len(), 3);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn status_preserves_failed_user_service_state() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 0,
            stdout: "failed\n".to_string(),
            stderr: String::new(),
        });

        let outcome =
            service_status(&runner, PathBuf::from("/tmp/units").as_path()).expect("status");

        assert_eq!(
            outcome.state,
            LinuxServiceState::Unknown("failed".to_string())
        );
    }

    #[test]
    fn restart_and_uninstall_call_user_service_only() {
        let temp = tempfile_dir("bowline-service-uninstall");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "unit").expect("unit");
        let runner = RecordingRunner::ok();

        let restarted = restart_service(&runner, &temp).expect("restart");
        assert_eq!(restarted.state, LinuxServiceState::Restarted);
        let uninstalled = uninstall_service(&runner, &temp).expect("uninstall");
        assert_eq!(uninstalled.state, LinuxServiceState::Uninstalled);
        assert!(!unit_path(&temp).exists());
        assert_eq!(
            *runner.calls.borrow(),
            vec![
                vec!["systemctl", "--user", "restart", "bowline.service"],
                vec!["systemctl", "--user", "disable", "--now", "bowline.service"],
                vec!["systemctl", "--user", "daemon-reload"],
            ]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn uninstall_returns_disable_failures() {
        let temp = tempfile_dir("bowline-service-disable-failure");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "unit").expect("unit");
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 1,
            stdout: String::new(),
            stderr: "permission denied".to_string(),
        });

        let error = uninstall_service(&runner, &temp).expect_err("disable failure");

        assert!(error.to_string().contains("permission denied"));
        assert!(unit_path(&temp).exists());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn uninstall_ignores_already_missing_systemd_unit() {
        let temp = tempfile_dir("bowline-service-missing-unit");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "unit").expect("unit");
        let runner = SequenceRunner::new(vec![
            ProcessOutput {
                status_code: 1,
                stdout: String::new(),
                stderr: "Unit bowline.service could not be found.".to_string(),
            },
            ProcessOutput {
                status_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);

        let outcome = uninstall_service(&runner, &temp).expect("uninstall missing unit");

        assert_eq!(outcome.state, LinuxServiceState::Uninstalled);
        assert!(!unit_path(&temp).exists());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn unavailable_user_manager_is_structured() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 1,
            stdout: String::new(),
            stderr: "Failed to connect to bus: No medium found".to_string(),
        });
        let error = service_status(&runner, PathBuf::from("/tmp/units").as_path())
            .expect_err("status should be unavailable");

        assert!(error.to_string().contains("enable lingering"));
    }

    fn config_with_spaces() -> LinuxServiceConfig {
        LinuxServiceConfig {
            daemon: PathBuf::from("/tmp/bin/bowline-daemon"),
            root: PathBuf::from("/tmp/Code Root"),
            state_root: PathBuf::from("/tmp/bowline state"),
            socket: PathBuf::from("/tmp/bowline.sock"),
            workspace_id: "ws_code".to_string(),
            device_id: "device-linux".to_string(),
        }
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        path
    }
}
