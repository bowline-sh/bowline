use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use bowline_core::fs_atomic::{AtomicWriteOptions, write_atomic};

use crate::{
    bootstrap::process::ProcessRunner,
    service_runtime::{self, ServiceRuntimeError},
};

pub const SERVICE_LABEL: &str = "io.bowline.daemon";
pub const PLIST_NAME: &str = "io.bowline.daemon.plist";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosServiceConfig {
    pub daemon: PathBuf,
    pub root: PathBuf,
    pub state_root: PathBuf,
    pub socket: PathBuf,
    pub workspace_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosServiceOptions {
    pub launch_agents_dir: PathBuf,
    pub launch_domain: String,
    pub config: MacosServiceConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosServiceOutcome {
    pub service_name: String,
    pub unit_path: PathBuf,
    pub state: MacosServiceState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacosServiceState {
    Installed,
    Restarted,
    Uninstalled,
    Active,
    Inactive,
    Unknown(String),
}

service_runtime::service_error! {
    pub enum MacosServiceError {
        MissingHome => "HOME is unavailable",
        MissingUserId => "macOS user id is unavailable",
    }
    io_context: "launch agent file operation failed",
}

service_runtime::service_outcome_parts!(MacosServiceOutcome, MacosServiceState);

pub fn current_platform_supported() -> bool {
    cfg!(target_os = "macos")
}

pub fn default_launch_agents_dir() -> Result<PathBuf, MacosServiceError> {
    let Some(home) = env::var_os("HOME") else {
        return Err(MacosServiceError::MissingHome);
    };
    Ok(PathBuf::from(home).join("Library").join("LaunchAgents"))
}

pub fn default_launch_domain() -> Result<String, MacosServiceError> {
    if let Ok(uid) = env::var("UID")
        && !uid.trim().is_empty()
    {
        return Ok(format!("gui/{}", uid.trim()));
    }
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(MacosServiceError::Io)?;
    if !output.status.success() {
        return Err(MacosServiceError::MissingUserId);
    }
    let uid = String::from_utf8_lossy(&output.stdout);
    let uid = uid.trim();
    if uid.is_empty() {
        return Err(MacosServiceError::MissingUserId);
    }
    Ok(format!("gui/{uid}"))
}

pub fn install_or_update_service<R>(
    runner: &R,
    options: &MacosServiceOptions,
) -> Result<MacosServiceOutcome, MacosServiceError>
where
    R: ProcessRunner,
{
    fs::create_dir_all(&options.launch_agents_dir)?;
    fs::create_dir_all(&options.config.state_root)?;
    let plist_path = plist_path(&options.launch_agents_dir);
    write_owner_only(
        &plist_path,
        &render_launch_agent_plist(&options.config),
        0o600,
    )?;
    run_launchctl(
        runner,
        &[
            "bootout",
            &options.launch_domain,
            &plist_path.display().to_string(),
        ],
        true,
    )?;
    run_launchctl(
        runner,
        &[
            "bootstrap",
            &options.launch_domain,
            &plist_path.display().to_string(),
        ],
        false,
    )?;
    run_launchctl(
        runner,
        &[
            "kickstart",
            "-k",
            &format!("{}/{}", options.launch_domain, SERVICE_LABEL),
        ],
        false,
    )?;
    Ok(outcome(plist_path, MacosServiceState::Installed))
}

pub fn restart_service<R>(
    runner: &R,
    launch_agents_dir: &Path,
    launch_domain: &str,
) -> Result<MacosServiceOutcome, MacosServiceError>
where
    R: ProcessRunner,
{
    run_launchctl(
        runner,
        &[
            "kickstart",
            "-k",
            &format!("{launch_domain}/{SERVICE_LABEL}"),
        ],
        false,
    )?;
    Ok(outcome(
        plist_path(launch_agents_dir),
        MacosServiceState::Restarted,
    ))
}

pub fn stop_service<R>(
    runner: &R,
    launch_agents_dir: &Path,
    launch_domain: &str,
) -> Result<MacosServiceOutcome, MacosServiceError>
where
    R: ProcessRunner,
{
    let path = plist_path(launch_agents_dir);
    run_launchctl(
        runner,
        &["bootout", launch_domain, &path.display().to_string()],
        true,
    )?;
    Ok(outcome(path, MacosServiceState::Inactive))
}

pub fn restore_service<R>(
    runner: &R,
    launch_agents_dir: &Path,
    launch_domain: &str,
    definition: &[u8],
) -> Result<MacosServiceOutcome, MacosServiceError>
where
    R: ProcessRunner,
{
    fs::create_dir_all(launch_agents_dir)?;
    let path = plist_path(launch_agents_dir);
    write_atomic(
        &path,
        definition,
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: true,
        },
    )?;
    run_launchctl(
        runner,
        &["bootout", launch_domain, &path.display().to_string()],
        true,
    )?;
    run_launchctl(
        runner,
        &["bootstrap", launch_domain, &path.display().to_string()],
        false,
    )?;
    run_launchctl(
        runner,
        &[
            "kickstart",
            "-k",
            &format!("{launch_domain}/{SERVICE_LABEL}"),
        ],
        false,
    )?;
    Ok(outcome(path, MacosServiceState::Restarted))
}

pub fn uninstall_service<R>(
    runner: &R,
    launch_agents_dir: &Path,
    launch_domain: &str,
) -> Result<MacosServiceOutcome, MacosServiceError>
where
    R: ProcessRunner,
{
    let path = plist_path(launch_agents_dir);
    run_launchctl(
        runner,
        &["bootout", launch_domain, &path.display().to_string()],
        true,
    )?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(outcome(path, MacosServiceState::Uninstalled))
}

pub fn service_status<R>(
    runner: &R,
    launch_agents_dir: &Path,
    launch_domain: &str,
) -> Result<MacosServiceOutcome, MacosServiceError>
where
    R: ProcessRunner,
{
    let output = service_runtime::run_service_command(
        runner,
        "launchctl",
        [
            "print".to_string(),
            format!("{launch_domain}/{SERVICE_LABEL}"),
        ],
        launchctl_missing_service,
        launchctl_failure,
    );
    let output = output?;
    if output.status_code != 0 {
        return Ok(outcome(
            plist_path(launch_agents_dir),
            MacosServiceState::Inactive,
        ));
    }
    Ok(outcome(
        plist_path(launch_agents_dir),
        parse_launchctl_state(&output.stdout),
    ))
}

pub fn render_launch_agent_plist(config: &MacosServiceConfig) -> String {
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
        xml_escape(
            &config
                .state_root
                .join("bowline-daemon.out.log")
                .display()
                .to_string()
        ),
        xml_escape(
            &config
                .state_root
                .join("bowline-daemon.err.log")
                .display()
                .to_string()
        )
    )
}

pub fn plist_path(launch_agents_dir: &Path) -> PathBuf {
    launch_agents_dir.join(PLIST_NAME)
}

fn outcome(unit_path: PathBuf, state: MacosServiceState) -> MacosServiceOutcome {
    service_runtime::service_outcome(SERVICE_LABEL, unit_path, state)
}

fn run_launchctl<R>(
    runner: &R,
    args: &[&str],
    ignore_missing: bool,
) -> Result<(), MacosServiceError>
where
    R: ProcessRunner,
{
    service_runtime::run_service_command(
        runner,
        "launchctl",
        args.iter().copied(),
        |stderr| ignore_missing && launchctl_missing_service(stderr),
        launchctl_failure,
    )
    .map(|_| ())
    .map_err(MacosServiceError::from)
}

fn launchctl_failure(failure: service_runtime::CommandFailure) -> ServiceRuntimeError {
    service_runtime::classify_command_failure(
        failure,
        launchctl_domain_unavailable,
        "macOS user launch domain is unavailable; sign in to a GUI session",
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

fn parse_launchctl_state(stdout: &str) -> MacosServiceState {
    let lower = stdout.to_ascii_lowercase();
    if lower.contains("state = exited")
        || lower.contains("state = not running")
        || lower.contains("\"state\" => exited")
        || lower.contains("\"state\" => not running")
    {
        return MacosServiceState::Inactive;
    }
    if lower.contains("state = ") || lower.contains("\"state\" => ") {
        return MacosServiceState::Active;
    }
    MacosServiceState::Unknown("unknown".to_string())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn write_owner_only(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    write_atomic(
        path,
        contents.as_bytes(),
        AtomicWriteOptions {
            unix_mode: Some(mode),
            reject_symlink: false,
            replace_existing: true,
        },
    )
}

impl fmt::Display for MacosServiceState {
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
        MacosServiceConfig, MacosServiceOptions, MacosServiceState, install_or_update_service,
        plist_path, render_launch_agent_plist, restart_service, restore_service, service_status,
        stop_service, uninstall_service,
    };

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

        let outcome = install_or_update_service(
            &runner,
            &MacosServiceOptions {
                launch_agents_dir: temp.clone(),
                launch_domain: "gui/501".to_string(),
                config: config.clone(),
            },
        )
        .expect("install service");

        assert_eq!(outcome.state, MacosServiceState::Installed);
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

        let restarted = restart_service(&runner, &temp, "gui/501").expect("restart");
        let uninstalled = uninstall_service(&runner, &temp, "gui/501").expect("uninstall");

        assert_eq!(restarted.state, MacosServiceState::Restarted);
        assert_eq!(uninstalled.state, MacosServiceState::Uninstalled);
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

        let stopped = stop_service(&runner, &temp, "gui/501").expect("stop");

        assert_eq!(stopped.state, MacosServiceState::Inactive);
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

        let restored =
            restore_service(&runner, &temp, "gui/501", b"previous plist").expect("restore");

        assert_eq!(restored.state, MacosServiceState::Restarted);
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
    fn status_parses_running_launch_agent() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 0,
            stdout: "state = running\n".to_string(),
            stderr: String::new(),
        });

        let outcome = service_status(&runner, PathBuf::from("/tmp/agents").as_path(), "gui/501")
            .expect("status");

        assert_eq!(outcome.state, MacosServiceState::Active);
    }

    #[test]
    fn status_treats_waiting_launch_agent_as_supervisor_owned() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 0,
            stdout: "state = waiting\n".to_string(),
            stderr: String::new(),
        });

        let outcome = service_status(&runner, PathBuf::from("/tmp/agents").as_path(), "gui/501")
            .expect("status");

        assert_eq!(outcome.state, MacosServiceState::Active);
    }

    #[test]
    fn missing_launch_agent_reports_inactive() {
        let runner = RecordingRunner::with_output(ProcessOutput {
            status_code: 3,
            stdout: String::new(),
            stderr: "Could not find service".to_string(),
        });

        let outcome = service_status(&runner, PathBuf::from("/tmp/agents").as_path(), "gui/501")
            .expect("status");

        assert_eq!(outcome.state, MacosServiceState::Inactive);
    }

    fn config_with_spaces() -> MacosServiceConfig {
        MacosServiceConfig {
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
