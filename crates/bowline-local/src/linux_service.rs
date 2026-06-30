use std::{
    env,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::bootstrap::process::{ProcessError, ProcessRunner};

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

#[derive(Debug)]
pub enum LinuxServiceError {
    MissingHome,
    Io(io::Error),
    Process(ProcessError),
    Unavailable(String),
    CommandFailed {
        program: String,
        status_code: i32,
        stderr: String,
    },
}

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
    let output = runner.run(
        "systemctl",
        &[
            "--user".to_string(),
            "show".to_string(),
            SERVICE_NAME.to_string(),
            "--property=ActiveState".to_string(),
            "--value".to_string(),
        ],
    )?;
    if output.status_code != 0 {
        return Err(systemctl_failure(
            "systemctl",
            output.status_code,
            output.stderr,
        ));
    }
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
    LinuxServiceOutcome {
        service_name: SERVICE_NAME.to_string(),
        unit_path,
        state,
    }
}

fn run_systemctl<R>(runner: &R, args: &[&str]) -> Result<(), LinuxServiceError>
where
    R: ProcessRunner,
{
    let mut full_args = vec!["--user".to_string()];
    full_args.extend(args.iter().map(|arg| arg.to_string()));
    let output = runner.run("systemctl", &full_args)?;
    if output.status_code != 0 {
        return Err(systemctl_failure(
            "systemctl",
            output.status_code,
            output.stderr,
        ));
    }
    Ok(())
}

fn systemctl_failure(program: &str, status_code: i32, stderr: String) -> LinuxServiceError {
    if user_manager_unavailable(&stderr) {
        return LinuxServiceError::Unavailable(
            "systemd user manager is unavailable; start a user session or enable lingering"
                .to_string(),
        );
    }
    LinuxServiceError::CommandFailed {
        program: program.to_string(),
        status_code,
        stderr,
    }
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

impl fmt::Display for LinuxServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => formatter.write_str("HOME is unavailable"),
            Self::Io(error) => write!(formatter, "service file operation failed: {error}"),
            Self::Process(error) => error.fmt(formatter),
            Self::Unavailable(message) => formatter.write_str(message),
            Self::CommandFailed {
                program,
                status_code,
                stderr,
            } => write!(
                formatter,
                "`{program}` failed with status {status_code}: {stderr}"
            ),
        }
    }
}

impl Error for LinuxServiceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Process(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LinuxServiceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProcessError> for LinuxServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, fs, path::PathBuf, rc::Rc};

    use crate::bootstrap::process::{ProcessError, ProcessOutput, ProcessRunner};

    use super::{
        LinuxServiceConfig, LinuxServiceOptions, LinuxServiceState, install_or_update_service,
        render_systemd_user_unit, restart_service, service_status, uninstall_service, unit_path,
    };

    #[derive(Clone)]
    struct RecordingRunner {
        calls: Rc<RefCell<Vec<Vec<String>>>>,
        output: ProcessOutput,
    }

    impl RecordingRunner {
        fn ok() -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                output: ProcessOutput {
                    status_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            }
        }

        fn with_output(output: ProcessOutput) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                output,
            }
        }
    }

    impl ProcessRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
            let mut call = vec![program.to_string()];
            call.extend(args.iter().cloned());
            self.calls.borrow_mut().push(call);
            Ok(self.output.clone())
        }
    }

    #[derive(Clone)]
    struct SequenceRunner {
        calls: Rc<RefCell<Vec<Vec<String>>>>,
        outputs: Rc<RefCell<VecDeque<ProcessOutput>>>,
    }

    impl SequenceRunner {
        fn new(outputs: Vec<ProcessOutput>) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                outputs: Rc::new(RefCell::new(outputs.into())),
            }
        }
    }

    impl ProcessRunner for SequenceRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
            let mut call = vec![program.to_string()];
            call.extend(args.iter().cloned());
            self.calls.borrow_mut().push(call);
            Ok(self
                .outputs
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| ProcessOutput {
                    status_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }
    }

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

        let unit = render_systemd_user_unit(&config);

        assert!(unit.contains("--sync-root \"/tmp/Code%%Root\""));
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
