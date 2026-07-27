//! The systemd user-manager supervisor: unit rendering, unit-value escaping,
//! and the `systemctl --user` calls. Everything above this file's platform
//! detail lives in [`crate::service_supervisor`].

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use bowline_core::fs_atomic::{AtomicWriteOptions, write_atomic};

use crate::{
    bootstrap::process::{ProcessOutput, ProcessRunner},
    service_runtime::{ServiceCommand, ServiceError, tolerate_nothing},
    service_supervisor::{
        ServiceConfig, ServiceOutcome, ServiceRegistration, ServiceState, ServiceSupervisor,
    },
};

pub const SERVICE_NAME: &str = "bowline.service";

const MISSING_DEFINITION_ERROR: &str =
    "systemd service has no unit to restore; refusing to stop it";
const USER_MANAGER_UNAVAILABLE: &str =
    "systemd user manager is unavailable; start a user session or enable lingering";

pub struct SystemdUserService<'a, R: ?Sized> {
    runner: &'a R,
    unit_dir: PathBuf,
}

impl<'a, R> SystemdUserService<'a, R>
where
    R: ProcessRunner + ?Sized,
{
    /// The supervisor for this user's systemd manager.
    pub fn for_current_user(runner: &'a R) -> Result<Self, ServiceError> {
        Ok(Self::in_unit_dir(runner, default_user_unit_dir()?))
    }

    /// The supervisor for an explicit unit directory, so systemd behaviour can
    /// be exercised on any host.
    pub fn in_unit_dir(runner: &'a R, unit_dir: PathBuf) -> Self {
        Self { runner, unit_dir }
    }

    fn systemctl(&self, args: &[&str]) -> Result<(), ServiceError> {
        self.systemctl_output(args).map(|_| ())
    }

    fn systemctl_output(&self, args: &[&str]) -> Result<ProcessOutput, ServiceError> {
        ServiceCommand {
            program: "systemctl",
            tolerate_failure: tolerate_nothing,
            unavailable: user_manager_unavailable,
            unavailable_message: USER_MANAGER_UNAVAILABLE,
        }
        .run(
            self.runner,
            ["--user"].into_iter().chain(args.iter().copied()),
        )
    }

    fn outcome(&self, state: ServiceState) -> ServiceOutcome {
        ServiceOutcome {
            service_name: SERVICE_NAME.to_string(),
            unit_path: self.definition_path(),
            state,
        }
    }

    fn apply_registration(&self, registration: ServiceRegistration) -> Result<(), ServiceError> {
        match registration {
            ServiceRegistration::Enabled => self.systemctl(&["enable", SERVICE_NAME]),
            ServiceRegistration::EnabledUntilReboot => {
                self.systemctl(&["disable", SERVICE_NAME])?;
                self.systemctl(&["enable", "--runtime", SERVICE_NAME])
            }
            ServiceRegistration::Disabled => self.systemctl(&["disable", SERVICE_NAME]),
        }
    }
}

impl<R> ServiceSupervisor for SystemdUserService<'_, R>
where
    R: ProcessRunner + ?Sized,
{
    fn service_name(&self) -> &'static str {
        SERVICE_NAME
    }

    fn definition_path(&self) -> PathBuf {
        unit_path(&self.unit_dir)
    }

    fn missing_definition_error(&self) -> &'static str {
        MISSING_DEFINITION_ERROR
    }

    fn render_definition(&self, config: &ServiceConfig) -> String {
        render_systemd_user_unit(config)
    }

    fn install_or_update(&self, config: &ServiceConfig) -> Result<ServiceOutcome, ServiceError> {
        fs::create_dir_all(&self.unit_dir)?;
        write_unit_file(
            &self.definition_path(),
            render_systemd_user_unit(config).as_bytes(),
        )?;
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["enable", SERVICE_NAME])?;
        self.systemctl(&["restart", SERVICE_NAME])?;
        Ok(self.outcome(ServiceState::Installed))
    }

    fn restart(&self) -> Result<ServiceOutcome, ServiceError> {
        self.systemctl(&["restart", SERVICE_NAME])?;
        Ok(self.outcome(ServiceState::Restarted))
    }

    fn stop(&self) -> Result<ServiceOutcome, ServiceError> {
        self.systemctl(&["stop", SERVICE_NAME])?;
        Ok(self.outcome(ServiceState::Inactive))
    }

    fn uninstall(&self) -> Result<ServiceOutcome, ServiceError> {
        match self.systemctl(&["disable", "--now", SERVICE_NAME]) {
            Ok(()) => {}
            Err(error) if missing_unit_error(&error) => {}
            Err(error) => return Err(error),
        }
        match fs::remove_file(self.definition_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.systemctl(&["daemon-reload"])?;
        Ok(self.outcome(ServiceState::Uninstalled))
    }

    fn status(&self) -> Result<ServiceOutcome, ServiceError> {
        let output = match self.systemctl_output(&[
            "show",
            SERVICE_NAME,
            "--property=ActiveState",
            "--value",
        ]) {
            Ok(output) => output,
            // A unit systemd has never seen is a first install, not a fault.
            Err(error) if missing_unit_error(&error) => {
                return Ok(self.outcome(ServiceState::Inactive));
            }
            Err(error) => return Err(error),
        };
        let state = match output.stdout.lines().next().unwrap_or("").trim() {
            // Transitions belong to the supervisor, so the daemon still counts
            // as owned while systemd moves it between states.
            "active" | "activating" | "deactivating" => ServiceState::Active,
            "inactive" | "failed" | "" => ServiceState::Inactive,
            other => ServiceState::Unknown(other.to_string()),
        };
        Ok(self.outcome(state))
    }

    fn registration(&self) -> Result<Option<ServiceRegistration>, ServiceError> {
        let output =
            self.systemctl_output(&["show", SERVICE_NAME, "--property=UnitFileState", "--value"])?;
        match output.stdout.lines().next().unwrap_or("").trim() {
            "enabled" => Ok(Some(ServiceRegistration::Enabled)),
            "enabled-runtime" => Ok(Some(ServiceRegistration::EnabledUntilReboot)),
            "disabled" | "static" | "indirect" => Ok(Some(ServiceRegistration::Disabled)),
            state => Err(ServiceError::Unavailable(format!(
                "systemd unit-file state `{state}` cannot be restored safely"
            ))),
        }
    }

    fn restore(
        &self,
        definition: &[u8],
        registration: Option<ServiceRegistration>,
    ) -> Result<ServiceOutcome, ServiceError> {
        // Restoring a systemd unit without knowing how it was registered would
        // silently re-enable a service the user had disabled.
        let Some(registration) = registration else {
            return Err(ServiceError::Unavailable(
                "previous systemd enablement is unavailable".to_string(),
            ));
        };
        fs::create_dir_all(&self.unit_dir)?;
        write_unit_file(&self.definition_path(), definition)?;
        self.systemctl(&["daemon-reload"])?;
        self.apply_registration(registration)?;
        self.systemctl(&["restart", SERVICE_NAME])?;
        Ok(self.outcome(ServiceState::Restarted))
    }
}

pub fn default_user_unit_dir() -> Result<PathBuf, ServiceError> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME")
        && !config_home.is_empty()
    {
        return Ok(PathBuf::from(config_home).join("systemd").join("user"));
    }
    let Some(home) = env::var_os("HOME") else {
        return Err(ServiceError::MissingHome);
    };
    Ok(PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user"))
}

pub fn unit_path(unit_dir: &Path) -> PathBuf {
    unit_dir.join(SERVICE_NAME)
}

/// The daemon reads `daemon.env` itself at startup, so the unit deliberately
/// carries no `EnvironmentFile=`: a second delivery channel would put session
/// ids and bearer tokens into the unit environment, where `systemctl --user
/// show` echoes them, and would give Linux a secret path macOS does not have.
///
/// The two stream settings are stated rather than left to systemd's default so
/// the growth bound is visible in the artifact: journald applies its own size
/// caps and vacuum policy, which is why Linux needs no equivalent of the
/// daemon's own log rotation (see `crate::daemon_logs`) and must never be
/// pointed at a plain file, which nothing here would rotate.
pub fn render_systemd_user_unit(config: &ServiceConfig) -> String {
    format!(
        "[Unit]\nDescription=bowline daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={} serve --socket {} --sync-root {} --sync-state-root {} --sync-workspace {} --sync-device {} --notify-approvals\nStandardOutput=journal\nStandardError=journal\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote_arg(&config.root),
        systemd_quote_arg(&config.daemon),
        systemd_quote_arg(&config.socket),
        systemd_quote_arg(&config.root),
        systemd_quote_arg(&config.state_root),
        systemd_quote_value(&config.workspace_id),
        systemd_quote_value(&config.device_id),
    )
}

/// The single writer for the systemd unit: the file is the supervisor contract,
/// so install and restore differ only in the bytes they hand it. A torn write
/// leaves systemd with a unit it refuses to load, and a planted symlink must not
/// be followed.
fn write_unit_file(path: &Path, definition: &[u8]) -> io::Result<()> {
    write_atomic(
        path,
        definition,
        AtomicWriteOptions {
            unix_mode: Some(0o644),
            reject_symlink: true,
            replace_existing: true,
        },
    )
}

fn user_manager_unavailable(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("failed to connect to bus")
        || lower.contains("no medium found")
        || lower.contains("no such file or directory")
}

fn missing_unit_error(error: &ServiceError) -> bool {
    let Some(stderr) = error.command_stderr() else {
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

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::{
        bootstrap::process::{ProcessOutput, ProcessRunner},
        service_runtime::test_support::{RecordingRunner, SequenceRunner},
        service_supervisor::{ServiceConfig, ServiceRegistration, ServiceState, ServiceSupervisor},
    };

    use super::{SystemdUserService, render_systemd_user_unit, unit_path};

    #[test]
    fn rendered_unit_runs_daemon_serve_directly() {
        let unit = render_systemd_user_unit(&config_with_spaces());

        assert!(unit.contains("[Service]"));
        assert!(!unit.contains("EnvironmentFile"));
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
    fn rendered_unit_sends_daemon_streams_to_journald() {
        let unit = render_systemd_user_unit(&config_with_spaces());

        assert!(unit.contains("StandardOutput=journal"));
        assert!(unit.contains("StandardError=journal"));
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

        let outcome = SystemdUserService::in_unit_dir(&runner, temp.clone())
            .install_or_update(&config_with_spaces())
            .expect("install service");

        assert_eq!(outcome.state, ServiceState::Installed);
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

        SystemdUserService::in_unit_dir(&runner, temp.clone())
            .install_or_update(&config_with_spaces())
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
    fn status_treats_terminal_failure_as_repairable_inactive() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 0,
            stdout: "failed\n".to_string(),
            stderr: String::new(),
        });

        let outcome = units_service(&runner).status().expect("status");

        assert_eq!(outcome.state, ServiceState::Inactive);
    }

    #[test]
    fn status_treats_missing_unit_as_inactive_first_install() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 4,
            stdout: String::new(),
            stderr: "Unit bowline.service could not be found.".to_string(),
        });

        let outcome = units_service(&runner).status().expect("status");

        assert_eq!(outcome.state, ServiceState::Inactive);
    }

    #[test]
    fn status_treats_transitions_as_supervisor_owned() {
        for state in ["activating", "deactivating"] {
            let runner = RecordingRunner::with_output(ProcessOutput {
                status_code: 0,
                stdout: format!("{state}\n"),
                stderr: String::new(),
            });

            let outcome = units_service(&runner).status().expect("status");

            assert_eq!(outcome.state, ServiceState::Active);
        }
    }

    #[test]
    fn restart_and_uninstall_call_user_service_only() {
        let temp = tempfile_dir("bowline-service-uninstall");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "unit").expect("unit");
        let runner = RecordingRunner::ok();
        let service = SystemdUserService::in_unit_dir(&runner, temp.clone());

        let restarted = service.restart().expect("restart");
        assert_eq!(restarted.state, ServiceState::Restarted);
        let uninstalled = service.uninstall().expect("uninstall");
        assert_eq!(uninstalled.state, ServiceState::Uninstalled);
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
    fn stop_retains_unit_and_registration() {
        let temp = tempfile_dir("bowline-service-stop");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "unit").expect("unit");
        let runner = RecordingRunner::ok();

        let stopped = SystemdUserService::in_unit_dir(&runner, temp.clone())
            .stop()
            .expect("stop");

        assert_eq!(stopped.state, ServiceState::Inactive);
        assert!(unit_path(&temp).exists());
        assert_eq!(
            *runner.calls.borrow(),
            vec![vec!["systemctl", "--user", "stop", "bowline.service"]]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restore_replaces_definition_before_reload_and_restart() {
        let temp = tempfile_dir("bowline-service-restore");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "broken").expect("broken unit");
        let runner = RecordingRunner::ok();

        let restored = SystemdUserService::in_unit_dir(&runner, temp.clone())
            .restore(b"previous unit", Some(ServiceRegistration::Enabled))
            .expect("restore");

        assert_eq!(restored.state, ServiceState::Restarted);
        assert_eq!(
            fs::read(unit_path(&temp)).expect("restored unit"),
            b"previous unit"
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
    fn restore_preserves_disabled_service_state() {
        let temp = tempfile_dir("bowline-service-restore-disabled");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "broken").expect("broken unit");
        let runner = RecordingRunner::ok();

        SystemdUserService::in_unit_dir(&runner, temp.clone())
            .restore(b"previous unit", Some(ServiceRegistration::Disabled))
            .expect("restore");

        assert_eq!(
            *runner.calls.borrow(),
            vec![
                vec!["systemctl", "--user", "daemon-reload"],
                vec!["systemctl", "--user", "disable", "bowline.service"],
                vec!["systemctl", "--user", "restart", "bowline.service"],
            ]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restore_preserves_runtime_only_enablement() {
        let temp = tempfile_dir("bowline-service-restore-runtime");
        fs::create_dir_all(&temp).expect("unit dir");
        fs::write(unit_path(&temp), "broken").expect("broken unit");
        let runner = RecordingRunner::ok();

        SystemdUserService::in_unit_dir(&runner, temp.clone())
            .restore(
                b"previous unit",
                Some(ServiceRegistration::EnabledUntilReboot),
            )
            .expect("restore");

        assert_eq!(
            *runner.calls.borrow(),
            vec![
                vec!["systemctl", "--user", "daemon-reload"],
                vec!["systemctl", "--user", "disable", "bowline.service"],
                vec![
                    "systemctl",
                    "--user",
                    "enable",
                    "--runtime",
                    "bowline.service"
                ],
                vec!["systemctl", "--user", "restart", "bowline.service"],
            ]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restore_without_a_previous_registration_is_refused() {
        let temp = tempfile_dir("bowline-service-restore-unknown");
        let runner = RecordingRunner::ok();

        let error = SystemdUserService::in_unit_dir(&runner, temp.clone())
            .restore(b"previous unit", None)
            .expect_err("restore needs the previous enablement");

        assert!(error.to_string().contains("enablement is unavailable"));
        assert!(runner.calls.borrow().is_empty());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn registration_reads_unit_file_state() {
        for (state, expected) in [
            ("enabled", ServiceRegistration::Enabled),
            ("enabled-runtime", ServiceRegistration::EnabledUntilReboot),
            ("disabled", ServiceRegistration::Disabled),
            ("static", ServiceRegistration::Disabled),
            ("indirect", ServiceRegistration::Disabled),
        ] {
            let runner = RecordingRunner::with_output(ProcessOutput {
                status_code: 0,
                stdout: format!("{state}\n"),
                stderr: String::new(),
            });

            assert_eq!(
                units_service(&runner)
                    .registration()
                    .expect("registration")
                    .expect("systemd reports an enablement"),
                expected
            );
        }
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

        let error = SystemdUserService::in_unit_dir(&runner, temp.clone())
            .uninstall()
            .expect_err("disable failure");

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

        let outcome = SystemdUserService::in_unit_dir(&runner, temp.clone())
            .uninstall()
            .expect("uninstall missing unit");

        assert_eq!(outcome.state, ServiceState::Uninstalled);
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

        let error = units_service(&runner)
            .status()
            .expect_err("status should be unavailable");

        assert!(error.to_string().contains("enable lingering"));
    }

    fn units_service<R>(runner: &R) -> SystemdUserService<'_, R>
    where
        R: ProcessRunner,
    {
        SystemdUserService::in_unit_dir(runner, PathBuf::from("/tmp/units"))
    }

    fn config_with_spaces() -> ServiceConfig {
        ServiceConfig {
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
