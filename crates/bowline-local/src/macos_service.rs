//! The launchd user-agent supervisor: plist rendering, XML escaping, and the
//! `launchctl` calls. Everything above this file's platform detail lives in
//! [`crate::service_supervisor`].

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use bowline_core::fs_atomic::{AtomicWriteOptions, write_atomic};

use crate::{
    bootstrap::process::{ProcessOutput, ProcessRunner},
    daemon_logs,
    service_runtime::{ServiceCommand, ServiceError, tolerate_nothing},
    service_supervisor::{
        ServiceConfig, ServiceOutcome, ServiceRegistration, ServiceState, ServiceSupervisor,
    },
};

pub const SERVICE_LABEL: &str = "io.bowline.daemon";
pub const PLIST_NAME: &str = "io.bowline.daemon.plist";

const MISSING_DEFINITION_ERROR: &str =
    "launchd service has no plist to restore; refusing to unload it";
const LAUNCH_DOMAIN_UNAVAILABLE: &str =
    "macOS user launch domain is unavailable; sign in to a GUI session";

pub struct LaunchdUserAgent<'a, R: ?Sized> {
    runner: &'a R,
    launch_agents_dir: PathBuf,
    launch_domain: String,
}

impl<'a, R> LaunchdUserAgent<'a, R>
where
    R: ProcessRunner + ?Sized,
{
    /// The supervisor for this user's GUI launch domain.
    pub fn for_current_user(runner: &'a R) -> Result<Self, ServiceError> {
        Ok(Self::in_launch_domain(
            runner,
            default_launch_agents_dir()?,
            default_launch_domain()?,
        ))
    }

    /// The supervisor for an explicit agents directory and domain, so launchd
    /// behaviour can be exercised on any host.
    pub fn in_launch_domain(
        runner: &'a R,
        launch_agents_dir: PathBuf,
        launch_domain: String,
    ) -> Self {
        Self {
            runner,
            launch_agents_dir,
            launch_domain,
        }
    }

    fn service_target(&self) -> String {
        format!("{}/{SERVICE_LABEL}", self.launch_domain)
    }

    fn launchctl(&self, args: &[&str], tolerate_missing: bool) -> Result<(), ServiceError> {
        self.launchctl_output(args, tolerate_missing).map(|_| ())
    }

    fn launchctl_output(
        &self,
        args: &[&str],
        tolerate_missing: bool,
    ) -> Result<ProcessOutput, ServiceError> {
        ServiceCommand {
            program: "launchctl",
            tolerate_failure: if tolerate_missing {
                launchctl_missing_service
            } else {
                tolerate_nothing
            },
            unavailable: launchctl_domain_unavailable,
            unavailable_message: LAUNCH_DOMAIN_UNAVAILABLE,
        }
        .run(self.runner, args.iter().copied())
    }

    fn outcome(&self, state: ServiceState) -> ServiceOutcome {
        ServiceOutcome {
            service_name: SERVICE_LABEL.to_string(),
            unit_path: self.definition_path(),
            state,
        }
    }

    /// launchd picks up a changed plist only through a bootout/bootstrap pair,
    /// so every path that rewrites the file replays the same three calls.
    fn reload_agent(&self) -> Result<(), ServiceError> {
        let plist = self.definition_path().display().to_string();
        self.launchctl(&["bootout", &self.launch_domain, &plist], true)?;
        self.launchctl(&["bootstrap", &self.launch_domain, &plist], false)?;
        self.launchctl(&["kickstart", "-k", &self.service_target()], false)
    }
}

impl<R> ServiceSupervisor for LaunchdUserAgent<'_, R>
where
    R: ProcessRunner + ?Sized,
{
    fn service_name(&self) -> &'static str {
        SERVICE_LABEL
    }

    fn definition_path(&self) -> PathBuf {
        plist_path(&self.launch_agents_dir)
    }

    fn missing_definition_error(&self) -> &'static str {
        MISSING_DEFINITION_ERROR
    }

    fn render_definition(&self, config: &ServiceConfig) -> String {
        render_launch_agent_plist(config)
    }

    fn install_or_update(&self, config: &ServiceConfig) -> Result<ServiceOutcome, ServiceError> {
        fs::create_dir_all(&self.launch_agents_dir)?;
        fs::create_dir_all(&config.state_root)?;
        write_plist(
            &self.definition_path(),
            render_launch_agent_plist(config).as_bytes(),
        )?;
        self.reload_agent()?;
        Ok(self.outcome(ServiceState::Installed))
    }

    fn restart(&self) -> Result<ServiceOutcome, ServiceError> {
        self.launchctl(&["kickstart", "-k", &self.service_target()], false)?;
        Ok(self.outcome(ServiceState::Restarted))
    }

    fn stop(&self) -> Result<ServiceOutcome, ServiceError> {
        let plist = self.definition_path().display().to_string();
        self.launchctl(&["bootout", &self.launch_domain, &plist], true)?;
        Ok(self.outcome(ServiceState::Inactive))
    }

    fn uninstall(&self) -> Result<ServiceOutcome, ServiceError> {
        let path = self.definition_path();
        self.launchctl(
            &["bootout", &self.launch_domain, &path.display().to_string()],
            true,
        )?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(self.outcome(ServiceState::Uninstalled))
    }

    fn status(&self) -> Result<ServiceOutcome, ServiceError> {
        let output = self.launchctl_output(&["print", &self.service_target()], true)?;
        if output.status_code != 0 {
            return Ok(self.outcome(ServiceState::Inactive));
        }
        Ok(self.outcome(parse_launchctl_state(&output.stdout)))
    }

    /// launchd keeps no registration apart from the plist itself, so there is
    /// nothing to capture before a takeover and nothing to reapply after one.
    fn registration(&self) -> Result<Option<ServiceRegistration>, ServiceError> {
        Ok(None)
    }

    fn restore(
        &self,
        definition: &[u8],
        _registration: Option<ServiceRegistration>,
    ) -> Result<ServiceOutcome, ServiceError> {
        fs::create_dir_all(&self.launch_agents_dir)?;
        write_plist(&self.definition_path(), definition)?;
        self.reload_agent()?;
        Ok(self.outcome(ServiceState::Restarted))
    }
}

pub fn default_launch_agents_dir() -> Result<PathBuf, ServiceError> {
    let Some(home) = env::var_os("HOME") else {
        return Err(ServiceError::MissingHome);
    };
    Ok(PathBuf::from(home).join("Library").join("LaunchAgents"))
}

pub fn default_launch_domain() -> Result<String, ServiceError> {
    if let Ok(uid) = env::var("UID")
        && !uid.trim().is_empty()
    {
        return Ok(format!("gui/{}", uid.trim()));
    }
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(ServiceError::Io)?;
    if !output.status.success() {
        return Err(ServiceError::MissingUserId);
    }
    let uid = String::from_utf8_lossy(&output.stdout);
    let uid = uid.trim();
    if uid.is_empty() {
        return Err(ServiceError::MissingUserId);
    }
    Ok(format!("gui/{uid}"))
}

pub fn plist_path(launch_agents_dir: &Path) -> PathBuf {
    launch_agents_dir.join(PLIST_NAME)
}

pub fn render_launch_agent_plist(config: &ServiceConfig) -> String {
    let args = [
        config.daemon.display().to_string(),
        "serve".to_string(),
        "--socket".to_string(),
        config.socket.display().to_string(),
        "--sync-root".to_string(),
        config.root.display().to_string(),
        "--sync-state-root".to_string(),
        config.state_root.display().to_string(),
        "--sync-workspace".to_string(),
        config.workspace_id.clone(),
        "--sync-device".to_string(),
        config.device_id.clone(),
        "--notify-approvals".to_string(),
    ];
    let args = args
        .iter()
        .map(|arg| format!("      <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
{}
    </array>
    <key>WorkingDirectory</key>
    <string>{}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
      <key>SuccessfulExit</key>
      <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
  </dict>
</plist>
"#,
        SERVICE_LABEL,
        args,
        xml_escape(&config.root.display().to_string()),
        // The daemon caps and rotates these two files itself (see
        // `crate::daemon_logs`); launchd only opens them. Naming them anywhere
        // but that module would let the plist and the rotation drift apart, and
        // launchd would then append forever to a file nothing rotates.
        xml_escape(
            &daemon_logs::stdout_log_path(&config.state_root)
                .display()
                .to_string()
        ),
        xml_escape(
            &daemon_logs::stderr_log_path(&config.state_root)
                .display()
                .to_string()
        )
    )
}

/// The single writer for the launch agent plist: install and restore differ
/// only in the bytes they hand it. The plist is the launchd contract, so an
/// intentional symlink there is never something bowline should clobber, and a
/// torn write leaves launchd with a plist it refuses to load.
fn write_plist(path: &Path, definition: &[u8]) -> io::Result<()> {
    write_atomic(
        path,
        definition,
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: true,
        },
    )
}

fn launchctl_domain_unavailable(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("domain does not support")
        || lower.contains("could not find domain")
        || lower.contains("bootstrap failed")
}

fn launchctl_missing_service(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("could not find service")
        || lower.contains("service is not loaded")
        || lower.contains("no such process")
        || lower.contains("does not exist")
        || lower.contains("input/output error")
}

fn parse_launchctl_state(stdout: &str) -> ServiceState {
    let lower = stdout.to_ascii_lowercase();
    if lower.contains("state = exited")
        || lower.contains("state = not running")
        || lower.contains("\"state\" => exited")
        || lower.contains("\"state\" => not running")
    {
        return ServiceState::Inactive;
    }
    if lower.contains("state = ") || lower.contains("\"state\" => ") {
        return ServiceState::Active;
    }
    ServiceState::Unknown("unknown".to_string())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::{
        bootstrap::process::{ProcessOutput, ProcessRunner},
        service_runtime::test_support::{RecordingRunner, SequenceRunner},
        service_supervisor::{ServiceConfig, ServiceState, ServiceSupervisor},
    };

    use super::{LaunchdUserAgent, plist_path, render_launch_agent_plist};

    #[test]
    fn rendered_plist_runs_daemon_directly() {
        let plist = render_launch_agent_plist(&config_with_spaces());

        assert!(plist.contains("<string>io.bowline.daemon</string>"));
        assert!(plist.contains("<string>/tmp/bin/bowline-daemon</string>"));
        assert!(plist.contains("<string>--sync-root</string>"));
        assert!(plist.contains("<string>/tmp/Code Root</string>"));
        assert!(plist.contains("<string>--sync-state-root</string>"));
        assert!(plist.contains("<string>/tmp/bowline state</string>"));
        assert!(plist.contains("<string>--sync-workspace</string>"));
        assert!(plist.contains("<string>ws_code</string>"));
        assert!(plist.contains("<string>--sync-device</string>"));
        assert!(plist.contains("<string>device-mac</string>"));
        assert!(!plist.contains("<string>/bin/sh</string>"));
        assert!(!plist.contains("BOWLINE_ACCOUNT_SESSION_ID"));
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[test]
    fn rendered_plist_escapes_xml() {
        let mut config = config_with_spaces();
        config.root = PathBuf::from("/tmp/Code & <Root>");

        let plist = render_launch_agent_plist(&config);

        assert!(plist.contains("/tmp/Code &amp; &lt;Root&gt;"));
        assert!(!plist.contains("/tmp/Code & <Root>"));
    }

    #[test]
    fn install_bootstraps_and_kickstarts_launch_agent() {
        let temp = tempfile_dir("bowline-macos-service-install");
        let mut config = config_with_spaces();
        config.state_root = temp.join("state");
        let runner = SequenceRunner::new(vec![
            ProcessOutput {
                status_code: 3,
                stdout: String::new(),
                stderr: "Could not find service".to_string(),
            },
            ok_output(),
            ok_output(),
        ]);

        let outcome = agent(&runner, temp.clone())
            .install_or_update(&config)
            .expect("install service");

        assert_eq!(outcome.state, ServiceState::Installed);
        assert_eq!(outcome.unit_path, plist_path(&temp));
        assert!(
            fs::read_to_string(plist_path(&temp))
                .expect("plist")
                .contains("RunAtLoad")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(plist_path(&temp))
                .expect("plist metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(
            *runner.calls.borrow(),
            vec![
                vec![
                    "launchctl",
                    "bootout",
                    "gui/501",
                    &plist_path(&temp).display().to_string()
                ],
                vec![
                    "launchctl",
                    "bootstrap",
                    "gui/501",
                    &plist_path(&temp).display().to_string()
                ],
                vec!["launchctl", "kickstart", "-k", "gui/501/io.bowline.daemon"],
            ]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restart_and_uninstall_do_not_touch_project_files() {
        let temp = tempfile_dir("bowline-macos-service-uninstall");
        let project = temp.join("Code").join("app").join("package.json");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(&project, "{}").expect("project file");
        fs::write(plist_path(&temp), "plist").expect("plist");
        let runner = RecordingRunner::ok();
        let service = agent(&runner, temp.clone());

        let restarted = service.restart().expect("restart");
        let uninstalled = service.uninstall().expect("uninstall");

        assert_eq!(restarted.state, ServiceState::Restarted);
        assert_eq!(uninstalled.state, ServiceState::Uninstalled);
        assert!(!plist_path(&temp).exists());
        assert_eq!(fs::read_to_string(project).expect("project file"), "{}");
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn stop_retains_launch_agent_configuration() {
        let temp = tempfile_dir("bowline-macos-service-stop");
        fs::create_dir_all(&temp).expect("launch agents dir");
        fs::write(plist_path(&temp), "plist").expect("plist");
        let runner = RecordingRunner::ok();

        let stopped = agent(&runner, temp.clone()).stop().expect("stop");

        assert_eq!(stopped.state, ServiceState::Inactive);
        assert!(plist_path(&temp).exists());
        assert_eq!(
            *runner.calls.borrow(),
            vec![vec![
                "launchctl",
                "bootout",
                "gui/501",
                &plist_path(&temp).display().to_string()
            ]]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restore_replaces_plist_before_bootstrap_and_kickstart() {
        let temp = tempfile_dir("bowline-macos-service-restore");
        fs::create_dir_all(&temp).expect("launch agents dir");
        fs::write(plist_path(&temp), "broken").expect("broken plist");
        let runner = RecordingRunner::ok();

        let restored = agent(&runner, temp.clone())
            .restore(b"previous plist", None)
            .expect("restore");

        assert_eq!(restored.state, ServiceState::Restarted);
        assert_eq!(
            fs::read(plist_path(&temp)).expect("restored plist"),
            b"previous plist"
        );
        assert_eq!(
            *runner.calls.borrow(),
            vec![
                vec![
                    "launchctl",
                    "bootout",
                    "gui/501",
                    &plist_path(&temp).display().to_string()
                ],
                vec![
                    "launchctl",
                    "bootstrap",
                    "gui/501",
                    &plist_path(&temp).display().to_string()
                ],
                vec!["launchctl", "kickstart", "-k", "gui/501/io.bowline.daemon"],
            ]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn launchd_reports_no_registration_to_preserve() {
        let runner = RecordingRunner::ok();

        let registration = agent(&runner, PathBuf::from("/tmp/agents"))
            .registration()
            .expect("registration");

        assert_eq!(registration, None);
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn status_parses_running_launch_agent() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 0,
            stdout: "state = running\n".to_string(),
            stderr: String::new(),
        });

        let outcome = agent(&runner, PathBuf::from("/tmp/agents"))
            .status()
            .expect("status");

        assert_eq!(outcome.state, ServiceState::Active);
    }

    #[test]
    fn status_treats_waiting_launch_agent_as_supervisor_owned() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 0,
            stdout: "state = waiting\n".to_string(),
            stderr: String::new(),
        });

        let outcome = agent(&runner, PathBuf::from("/tmp/agents"))
            .status()
            .expect("status");

        assert_eq!(outcome.state, ServiceState::Active);
    }

    #[test]
    fn missing_launch_agent_reports_inactive() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 3,
            stdout: String::new(),
            stderr: "Could not find service".to_string(),
        });

        let outcome = agent(&runner, PathBuf::from("/tmp/agents"))
            .status()
            .expect("status");

        assert_eq!(outcome.state, ServiceState::Inactive);
    }

    fn agent<R>(runner: &R, launch_agents_dir: PathBuf) -> LaunchdUserAgent<'_, R>
    where
        R: ProcessRunner,
    {
        LaunchdUserAgent::in_launch_domain(runner, launch_agents_dir, "gui/501".to_string())
    }

    fn config_with_spaces() -> ServiceConfig {
        ServiceConfig {
            daemon: PathBuf::from("/tmp/bin/bowline-daemon"),
            root: PathBuf::from("/tmp/Code Root"),
            state_root: PathBuf::from("/tmp/bowline state"),
            socket: PathBuf::from("/tmp/bowline.sock"),
            workspace_id: "ws_code".to_string(),
            device_id: "device-mac".to_string(),
        }
    }

    fn ok_output() -> ProcessOutput {
        ProcessOutput {
            status_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        path
    }
}
