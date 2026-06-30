use std::{
    env,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

use crate::bootstrap::process::{ProcessError, ProcessRunner};

pub const SERVICE_LABEL: &str = "io.bowline.daemon";
pub const PLIST_NAME: &str = "io.bowline.daemon.plist";
const LAUNCHER_NAME: &str = "bowline-daemon-launcher.sh";
const PERSISTED_ENV_KEYS: &[&str] = &[
    "CONVEX_URL",
    "BOWLINE_WORKSPACE_ID",
    "BOWLINE_DEVICE_ID",
    "BOWLINE_DEVICE_NAME",
    "BOWLINE_SECRET_STORE",
    "BOWLINE_ACCOUNT_SESSION_ID",
    "BOWLINE_CONTROL_PLANE_TOKEN",
    "BOWLINE_WORKOS_ACCESS_TOKEN",
    "BOWLINE_WORKOS_CLIENT_ID",
];

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

#[derive(Debug)]
pub enum MacosServiceError {
    MissingHome,
    MissingUserId,
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
    write_owner_only(
        &launcher_path(&options.config.state_root),
        &render_daemon_launcher(),
        0o700,
    )?;
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
    let output = runner.run(
        "launchctl",
        &[
            "print".to_string(),
            format!("{launch_domain}/{SERVICE_LABEL}"),
        ],
    )?;
    if output.status_code != 0 {
        if launchctl_missing_service(&output.stderr) {
            return Ok(outcome(
                plist_path(launch_agents_dir),
                MacosServiceState::Inactive,
            ));
        }
        return Err(launchctl_failure(
            "launchctl",
            output.status_code,
            output.stderr,
        ));
    }
    Ok(outcome(
        plist_path(launch_agents_dir),
        parse_launchctl_state(&output.stdout),
    ))
}

pub fn render_launch_agent_plist(config: &MacosServiceConfig) -> String {
    let launcher = launcher_path(&config.state_root);
    let daemon_env = config.state_root.join("daemon.env");
    let args = [
        "/bin/sh".to_string(),
        launcher.display().to_string(),
        daemon_env.display().to_string(),
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

pub fn launcher_path(state_root: &Path) -> PathBuf {
    state_root.join(LAUNCHER_NAME)
}

pub fn plist_path(launch_agents_dir: &Path) -> PathBuf {
    launch_agents_dir.join(PLIST_NAME)
}

fn outcome(unit_path: PathBuf, state: MacosServiceState) -> MacosServiceOutcome {
    MacosServiceOutcome {
        service_name: SERVICE_LABEL.to_string(),
        unit_path,
        state,
    }
}

fn run_launchctl<R>(
    runner: &R,
    args: &[&str],
    ignore_missing: bool,
) -> Result<(), MacosServiceError>
where
    R: ProcessRunner,
{
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let output = runner.run("launchctl", &args)?;
    if output.status_code == 0 || (ignore_missing && launchctl_missing_service(&output.stderr)) {
        return Ok(());
    }
    Err(launchctl_failure(
        "launchctl",
        output.status_code,
        output.stderr,
    ))
}

fn launchctl_failure(program: &str, status_code: i32, stderr: String) -> MacosServiceError {
    if launchctl_domain_unavailable(&stderr) {
        return MacosServiceError::Unavailable(
            "macOS user launch domain is unavailable; sign in to a GUI session".to_string(),
        );
    }
    MacosServiceError::CommandFailed {
        program: program.to_string(),
        status_code,
        stderr,
    }
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
    if lower.contains("state = running") || lower.contains("\"state\" => running") {
        return MacosServiceState::Active;
    }
    if lower.contains("state = exited") || lower.contains("state = not running") {
        return MacosServiceState::Inactive;
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

fn render_daemon_launcher() -> String {
    let cases = PERSISTED_ENV_KEYS.to_vec().join("|");
    format!(
        r#"#!/bin/sh
set -eu
env_file="$1"
shift
if [ -f "$env_file" ]; then
  while IFS='=' read -r key value || [ -n "$key" ]; do
    case "$key" in
      {cases})
        if [ -n "$value" ]; then
          export "$key=$value"
        fi
        ;;
    esac
  done < "$env_file"
fi
exec "$@"
"#
    )
}

fn write_owner_only(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    let temp_path = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(PLIST_NAME),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp_path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let write_result = (|| {
        let mut file = options.open(&temp_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
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

impl fmt::Display for MacosServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => formatter.write_str("HOME is unavailable"),
            Self::MissingUserId => formatter.write_str("macOS user id is unavailable"),
            Self::Io(error) => write!(formatter, "launch agent file operation failed: {error}"),
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

impl Error for MacosServiceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Process(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for MacosServiceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProcessError> for MacosServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, fs, path::PathBuf, rc::Rc};

    use crate::bootstrap::process::{ProcessError, ProcessOutput, ProcessRunner};

    use super::{
        MacosServiceConfig, MacosServiceOptions, MacosServiceState, install_or_update_service,
        launcher_path, plist_path, render_daemon_launcher, render_launch_agent_plist,
        restart_service, service_status, uninstall_service,
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
    fn rendered_plist_runs_daemon_through_safe_env_launcher() {
        let plist = render_launch_agent_plist(&config_with_spaces());

        assert!(plist.contains("<string>io.bowline.daemon</string>"));
        assert!(plist.contains("<string>/bin/sh</string>"));
        assert!(plist.contains("<string>/tmp/bowline state/bowline-daemon-launcher.sh</string>"));
        assert!(plist.contains("<string>/tmp/bowline state/daemon.env</string>"));
        assert!(plist.contains("<string>/tmp/bin/bowline-daemon</string>"));
        assert!(plist.contains("<string>--sync-root</string>"));
        assert!(plist.contains("<string>/tmp/Code Root</string>"));
        assert!(plist.contains("<string>--sync-state-root</string>"));
        assert!(plist.contains("<string>/tmp/bowline state</string>"));
        assert!(plist.contains("<string>--sync-workspace</string>"));
        assert!(plist.contains("<string>ws_code</string>"));
        assert!(plist.contains("<string>--sync-device</string>"));
        assert!(plist.contains("<string>device-mac</string>"));
        assert!(!plist.contains("BOWLINE_ACCOUNT_SESSION_ID"));
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[test]
    fn launcher_parses_allowlisted_environment_without_sourcing_shell() {
        let launcher = render_daemon_launcher();

        assert!(launcher.contains("BOWLINE_ACCOUNT_SESSION_ID"));
        assert!(launcher.contains("export \"$key=$value\""));
        assert!(!launcher.contains(". \"$"));
        assert!(!launcher.contains("source "));
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
        assert!(
            fs::read_to_string(launcher_path(&config.state_root))
                .expect("launcher")
                .contains("exec \"$@\"")
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
            let launcher_mode = fs::metadata(launcher_path(&config.state_root))
                .expect("launcher metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(launcher_mode, 0o700);
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
