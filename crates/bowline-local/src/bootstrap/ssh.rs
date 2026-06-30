use std::{error::Error, fmt};

use crate::bootstrap::process::{ProcessError, ProcessRunner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSshOptions {
    pub host: String,
    pub root: String,
    pub remote_binary: Option<String>,
    pub remote_workspace_id: Option<String>,
    pub remote_env: Vec<(String, String)>,
    pub remote_secret_env: Vec<(String, String)>,
    pub bootstrap_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBootstrapProbe {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum BootstrapSshError {
    InvalidHost(String),
    Process(ProcessError),
    RemoteFailed { status_code: i32, stderr: String },
}

pub fn probe_remote<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(
        runner,
        options,
        &format!(
            "devices request --root {} --json",
            remote_shell_path(options.root.as_str())
        ),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn list_remote_devices<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(runner, options, "devices list --json")?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn server_local_workspace_key_available<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    workspace_id: &str,
) -> Result<bool, BootstrapSshError>
where
    R: ProcessRunner,
{
    let command = format!(
        "workspace_id={}; secret_file=\"${{XDG_STATE_HOME:-$HOME/.local/state}}/bowline/secrets.v1\"; if [ -f \"$secret_file\" ] && grep -F \"\\\"workspaceId\\\": \\\"$workspace_id\\\"\" \"$secret_file\" >/dev/null 2>&1; then printf yes; else printf no; fi",
        shell_quote(workspace_id),
    );
    let output = run_remote_shell_with_stdin(runner, options, &command, "")?;
    Ok(output.stdout.trim() == "yes")
}

pub fn accept_remote_grant<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    request_id: &str,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(
        runner,
        options,
        &format!("devices accept {} --json", shell_quote(request_id)),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn prepare_remote_root<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(
        runner,
        options,
        &format!("init {} --json", remote_shell_path(options.root.as_str())),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn publish_default_metadata<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let Some(workspace_id) = options.remote_workspace_id.as_deref() else {
        return Ok(RemoteBootstrapProbe {
            stdout: String::new(),
            stderr: String::new(),
        });
    };
    let workspace_db = remote_shell_path(&format!(
        "~/.local/share/bowline/workspaces/{workspace_id}/local.sqlite3"
    ));
    let command = format!(
        "set -e; case \"$(uname -s)\" in Darwin) dir=\"$HOME/Library/Application Support/bowline\" ;; Linux) dir=\"${{XDG_STATE_HOME:-$HOME/.local/state}}/bowline\" ;; *) dir=\"$HOME/.bowline\" ;; esac; mkdir -p \"$dir\"; ln -sfn {workspace_db} \"$dir/local.sqlite3\"; umask 077; cat > \"$dir/daemon.env\"; chmod 600 \"$dir/daemon.env\""
    );
    let output = run_remote_shell_with_stdin(runner, options, &command, &daemon_env_file(options))?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn start_remote_daemon<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(runner, options, "daemon start --json")?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn daemon_status_remote<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(runner, options, "daemon status --json")?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn status_remote<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(
        runner,
        options,
        &format!("status {} --json", remote_shell_path(options.root.as_str())),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn create_remote_agent_lease<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    project: &str,
    task: &str,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline_in_root(
        runner,
        options,
        &format!(
            "agent start {} --task {} --base latest-workspace --hydrate-budget 512MiB --json",
            shell_quote(project),
            shell_quote(task),
        ),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn launch_remote_codex_agent<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    lease_id: &str,
    write_target_path: &str,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let bowline_command = options
        .remote_binary
        .as_ref()
        .map(|binary| remote_shell_path(binary))
        .unwrap_or_else(|| "bowline".to_string());
    let login_command = format!(
        "{}export PATH=\"$HOME/.local/bin:$PATH\"; {}{} agent prompt --lease {} | codex exec --cd {} --sandbox workspace-write --add-dir ~/.local/share/bowline --add-dir ~/.local/state/bowline --add-dir ~/.local/state/bowline --add-dir \"$HOME/Library/Application Support/bowline\" --skip-git-repo-check -",
        remote_state_prefix(options),
        remote_env_prefix(&options.remote_env),
        bowline_command,
        shell_quote(lease_id),
        remote_shell_path(write_target_path),
    );
    let output = run_remote_login_shell_with_stdin(
        runner,
        options,
        &login_command,
        &remote_stdin_env_stdin(options),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn complete_remote_agent_lease<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    lease_id: &str,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(
        runner,
        options,
        &format!("agent complete --lease {} --json", shell_quote(lease_id)),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn accept_remote_work_view<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    work_view_id: &str,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(
        runner,
        options,
        &format!("accept {} --json", shell_quote(work_view_id)),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn run_remote_bowline<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    bowline_args: &str,
) -> Result<crate::bootstrap::process::ProcessOutput, BootstrapSshError>
where
    R: ProcessRunner,
{
    run_remote_bowline_with_prefix(runner, options, bowline_args, "")
}

fn run_remote_bowline_in_root<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    bowline_args: &str,
) -> Result<crate::bootstrap::process::ProcessOutput, BootstrapSshError>
where
    R: ProcessRunner,
{
    run_remote_bowline_with_prefix(
        runner,
        options,
        bowline_args,
        &format!("cd {} && ", remote_shell_path(options.root.as_str())),
    )
}

fn run_remote_bowline_with_prefix<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    bowline_args: &str,
    command_prefix: &str,
) -> Result<crate::bootstrap::process::ProcessOutput, BootstrapSshError>
where
    R: ProcessRunner,
{
    validate_ssh_host(options.host.as_str()).map_err(|reason| {
        BootstrapSshError::InvalidHost(format!("invalid SSH host `{}`: {reason}", options.host))
    })?;
    let bowline_command = options
        .remote_binary
        .as_ref()
        .map(|binary| format!("{} {bowline_args}", remote_shell_path(binary)))
        .unwrap_or_else(|| format!("bowline {bowline_args}"));
    let remote_command = format!(
        "{}{}{}{}{}",
        remote_state_prefix(options),
        remote_stdin_env_prefix(options),
        command_prefix,
        remote_env_prefix(&options.remote_env),
        bowline_command,
    );
    let args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
        options.host.clone(),
        remote_command,
    ];
    let stdin = remote_stdin_env_stdin(options);
    let output = if stdin.is_empty() {
        runner.run("ssh", &args)?
    } else {
        runner.run_with_stdin("ssh", &args, &stdin)?
    };
    if output.status_code != 0 {
        return Err(BootstrapSshError::RemoteFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    Ok(output)
}

fn run_remote_login_shell_with_stdin<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    login_command: &str,
    stdin: &str,
) -> Result<crate::bootstrap::process::ProcessOutput, BootstrapSshError>
where
    R: ProcessRunner,
{
    validate_ssh_host(options.host.as_str()).map_err(|reason| {
        BootstrapSshError::InvalidHost(format!("invalid SSH host `{}`: {reason}", options.host))
    })?;
    let remote_command = format!(
        "{}bash -lc {}",
        remote_stdin_env_prefix(options),
        shell_quote(login_command)
    );
    let args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
        options.host.clone(),
        remote_command,
    ];
    let output = if stdin.is_empty() {
        runner.run("ssh", &args)?
    } else {
        runner.run_with_stdin("ssh", &args, stdin)?
    };
    if output.status_code != 0 {
        return Err(BootstrapSshError::RemoteFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    Ok(output)
}

fn run_remote_shell_with_stdin<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    remote_command: &str,
    stdin: &str,
) -> Result<crate::bootstrap::process::ProcessOutput, BootstrapSshError>
where
    R: ProcessRunner,
{
    validate_ssh_host(options.host.as_str()).map_err(|reason| {
        BootstrapSshError::InvalidHost(format!("invalid SSH host `{}`: {reason}", options.host))
    })?;
    let args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
        options.host.clone(),
        remote_command.to_string(),
    ];
    let output = if stdin.is_empty() {
        runner.run("ssh", &args)?
    } else {
        runner.run_with_stdin("ssh", &args, stdin)?
    };
    if output.status_code != 0 {
        return Err(BootstrapSshError::RemoteFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    Ok(output)
}

pub fn remote_shell_path(value: &str) -> String {
    let normalized = normalize_remote_home(value);
    if normalized == "~" {
        return "$HOME".to_string();
    }
    if let Some(rest) = normalized.strip_prefix("~/") {
        if rest.is_empty() {
            return "$HOME".to_string();
        }
        return format!("$HOME/{}", shell_quote(rest));
    }
    shell_quote(&normalized)
}

fn normalize_remote_home(value: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return value.to_string();
    };
    if home.is_empty() {
        return value.to_string();
    }
    if value == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    value
        .strip_prefix(&prefix)
        .map(|rest| format!("~/{rest}"))
        .unwrap_or_else(|| value.to_string())
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

pub fn validate_ssh_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("host is empty");
    }
    if host.starts_with('-') {
        return Err("host must not start with '-'");
    }
    if host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@'))
    {
        return Ok(());
    }
    Err(
        "host must be an SSH alias or user@host using only letters, numbers, '.', '-', '_', and '@'",
    )
}

fn remote_env_prefix(env: &[(String, String)]) -> String {
    if env.is_empty() {
        return String::new();
    }
    let assignments = env
        .iter()
        .map(|(key, value)| format!("{key}={}", shell_quote(value)))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{assignments} ")
}

fn remote_state_prefix(options: &BootstrapSshOptions) -> String {
    let Some(workspace_id) = options.remote_workspace_id.as_deref() else {
        return String::new();
    };
    if !valid_remote_state_id(workspace_id) {
        return String::new();
    }
    let state_dir = remote_shell_path(&format!("~/.local/share/bowline/workspaces/{workspace_id}"));
    let db_path = remote_shell_path(&format!(
        "~/.local/share/bowline/workspaces/{workspace_id}/local.sqlite3"
    ));
    format!("mkdir -p {state_dir}; BOWLINE_METADATA_DB={db_path}; export BOWLINE_METADATA_DB; ")
}

fn remote_stdin_env_prefix(options: &BootstrapSshOptions) -> String {
    let mut keys = Vec::new();
    if options.bootstrap_token.is_some() {
        keys.push("BOWLINE_BOOTSTRAP_TOKEN");
    }
    keys.extend(
        options
            .remote_secret_env
            .iter()
            .filter_map(|(key, _)| valid_remote_env_key(key).then_some(key.as_str())),
    );
    if keys.is_empty() {
        return String::new();
    }
    keys.into_iter()
        .map(|key| format!("IFS= read -r {key}; export {key}; "))
        .collect::<String>()
}

fn remote_stdin_env_stdin(options: &BootstrapSshOptions) -> String {
    let mut values = Vec::new();
    if let Some(token) = options.bootstrap_token.as_deref() {
        values.push(token);
    }
    values.extend(
        options
            .remote_secret_env
            .iter()
            .filter_map(|(key, value)| valid_remote_env_key(key).then_some(value.as_str())),
    );
    if values.is_empty() {
        return String::new();
    }
    format!("{}\n", values.join("\n"))
}

fn valid_remote_env_key(key: &str) -> bool {
    matches!(
        key,
        "BOWLINE_ACCOUNT_SESSION_ID"
            | "BOWLINE_WORKOS_ACCESS_TOKEN"
            | "BOWLINE_CONTROL_PLANE_TOKEN"
    )
}

fn daemon_env_file(options: &BootstrapSshOptions) -> String {
    options
        .remote_env
        .iter()
        .chain(options.remote_secret_env.iter())
        .filter(|(key, value)| {
            valid_daemon_env_key(key) && !value.is_empty() && !value.contains('\n')
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn valid_daemon_env_key(key: &str) -> bool {
    matches!(
        key,
        "CONVEX_URL"
            | "BOWLINE_WORKSPACE_ID"
            | "BOWLINE_DEVICE_ID"
            | "BOWLINE_DEVICE_NAME"
            | "BOWLINE_SECRET_STORE"
            | "BOWLINE_ACCOUNT_SESSION_ID"
            | "BOWLINE_CONTROL_PLANE_TOKEN"
            | "BOWLINE_WORKOS_ACCESS_TOKEN"
            | "BOWLINE_WORKOS_CLIENT_ID"
    )
}

fn valid_remote_state_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

impl fmt::Display for BootstrapSshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHost(error) => formatter.write_str(error),
            Self::Process(error) => error.fmt(formatter),
            Self::RemoteFailed {
                status_code,
                stderr,
            } => write!(
                formatter,
                "remote bootstrap command failed with status {status_code}: {stderr}"
            ),
        }
    }
}

impl Error for BootstrapSshError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidHost(_) => None,
            Self::Process(error) => Some(error),
            Self::RemoteFailed { .. } => None,
        }
    }
}

impl From<ProcessError> for BootstrapSshError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use crate::bootstrap::{
        process::{ProcessError, ProcessOutput, ProcessRunner},
        ssh::{
            BootstrapSshError, BootstrapSshOptions, accept_remote_grant, create_remote_agent_lease,
            daemon_status_remote, launch_remote_codex_agent, prepare_remote_root, probe_remote,
            start_remote_daemon, status_remote,
        },
    };

    #[derive(Clone)]
    struct RecordingRunner {
        args: Rc<RefCell<Vec<String>>>,
        stdin: Rc<RefCell<String>>,
    }

    impl ProcessRunner for RecordingRunner {
        fn run(&self, _program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
            *self.args.borrow_mut() = args.to_vec();
            *self.stdin.borrow_mut() = String::new();
            Ok(ProcessOutput {
                status_code: 0,
                stdout: "{}".to_string(),
                stderr: String::new(),
            })
        }

        fn run_with_stdin(
            &self,
            _program: &str,
            args: &[String],
            stdin: &str,
        ) -> Result<ProcessOutput, ProcessError> {
            *self.args.borrow_mut() = args.to_vec();
            *self.stdin.borrow_mut() = stdin.to_string();
            Ok(ProcessOutput {
                status_code: 0,
                stdout: "{}".to_string(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn remote_probe_prefixes_only_non_secret_bootstrap_environment() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let stdin = Rc::new(RefCell::new(String::new()));
        let runner = RecordingRunner {
            args: args.clone(),
            stdin: stdin.clone(),
        };
        let options = BootstrapSshOptions {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            remote_binary: Some("~/.local/bin/bowline".to_string()),
            remote_workspace_id: Some("ws_code".to_string()),
            remote_env: vec![
                (
                    "CONVEX_URL".to_string(),
                    "https://example.convex.cloud".to_string(),
                ),
                ("BOWLINE_WORKSPACE_ID".to_string(), "ws_code".to_string()),
                (
                    "BOWLINE_SECRET_STORE".to_string(),
                    "server-local".to_string(),
                ),
            ],
            remote_secret_env: vec![
                (
                    "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
                    "bowline account session".to_string(),
                ),
                (
                    "BOWLINE_WORKOS_REFRESH_TOKEN".to_string(),
                    "workos refresh token".to_string(),
                ),
            ],
            bootstrap_token: Some("scoped bootstrap token".to_string()),
        };

        probe_remote(&runner, &options).expect("probe succeeds");

        let captured = args.borrow();
        let remote_command = captured.last().expect("ssh command is recorded");
        assert!(remote_command.contains("CONVEX_URL='https://example.convex.cloud'"));
        assert!(remote_command.contains("BOWLINE_WORKSPACE_ID='ws_code'"));
        assert!(remote_command.contains("BOWLINE_SECRET_STORE='server-local'"));
        assert!(remote_command.contains(
            "BOWLINE_METADATA_DB=$HOME/'.local/share/bowline/workspaces/ws_code/local.sqlite3'"
        ));
        assert!(remote_command.contains("IFS= read -r BOWLINE_BOOTSTRAP_TOKEN"));
        assert!(!remote_command.contains("BOWLINE_CONTROL_PLANE_TOKEN"));
        assert!(!remote_command.contains("scoped bootstrap token"));
        assert!(!remote_command.contains("control token"));
        assert!(!remote_command.contains("bowline account session"));
        assert!(!remote_command.contains("workos refresh token"));
        assert!(remote_command.contains("IFS= read -r BOWLINE_ACCOUNT_SESSION_ID"));
        assert!(!remote_command.contains("IFS= read -r BOWLINE_WORKOS_ACCESS_TOKEN"));
        assert!(!remote_command.contains("IFS= read -r BOWLINE_WORKOS_REFRESH_TOKEN"));
        assert!(remote_command.contains("devices request --root $HOME/'Code' --json"));
        assert_eq!(
            stdin.borrow().as_str(),
            "scoped bootstrap token\nbowline account session\n"
        );
    }

    #[test]
    fn remote_accept_quotes_request_id_before_ssh_execution() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let stdin = Rc::new(RefCell::new(String::new()));
        let runner = RecordingRunner {
            args: args.clone(),
            stdin,
        };
        let options = BootstrapSshOptions {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            remote_binary: Some("~/.local/bin/bowline".to_string()),
            remote_workspace_id: None,
            remote_env: Vec::new(),
            remote_secret_env: Vec::new(),
            bootstrap_token: None,
        };

        accept_remote_grant(&runner, &options, "req_1; touch /tmp/pwn").expect("accept succeeds");

        let captured = args.borrow();
        let remote_command = captured.last().expect("ssh command is recorded");
        assert!(remote_command.contains("devices accept 'req_1; touch /tmp/pwn' --json"));
        assert!(!remote_command.contains("devices accept req_1;"));
    }

    #[test]
    fn remote_probe_rejects_option_like_host_before_ssh_execution() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let stdin = Rc::new(RefCell::new(String::new()));
        let runner = RecordingRunner {
            args: args.clone(),
            stdin,
        };
        let options = BootstrapSshOptions {
            host: "-oProxyCommand=touch /tmp/pwn".to_string(),
            root: "~/Code".to_string(),
            remote_binary: Some("~/.local/bin/bowline".to_string()),
            remote_workspace_id: None,
            remote_env: Vec::new(),
            remote_secret_env: Vec::new(),
            bootstrap_token: None,
        };

        let error = probe_remote(&runner, &options).expect_err("host is rejected");

        assert!(matches!(error, BootstrapSshError::InvalidHost(_)));
        assert!(args.borrow().is_empty());
    }

    #[test]
    fn remote_prepare_and_daemon_commands_use_installed_binary_and_root() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let stdin = Rc::new(RefCell::new(String::new()));
        let runner = RecordingRunner {
            args: args.clone(),
            stdin,
        };
        let options = BootstrapSshOptions {
            host: "linux-box".to_string(),
            root: "~/Code/agent project".to_string(),
            remote_binary: Some("~/.local/bin/bowline".to_string()),
            remote_workspace_id: Some("ws_agent".to_string()),
            remote_env: vec![(
                "BOWLINE_SECRET_STORE".to_string(),
                "server-local".to_string(),
            )],
            remote_secret_env: Vec::new(),
            bootstrap_token: None,
        };

        prepare_remote_root(&runner, &options).expect("prepare succeeds");
        let prepare_args = args.borrow().clone();
        let prepare_command = prepare_args.last().expect("ssh command is recorded");
        assert!(!prepare_command.starts_with("cd "));
        assert!(prepare_command.contains("$HOME/'Code/agent project'"));
        assert!(prepare_command.contains(
            "BOWLINE_METADATA_DB=$HOME/'.local/share/bowline/workspaces/ws_agent/local.sqlite3'"
        ));
        assert!(
            prepare_command
                .contains("$HOME/'.local/bin/bowline' init $HOME/'Code/agent project' --json")
        );
        assert!(prepare_command.contains("BOWLINE_SECRET_STORE='server-local'"));

        start_remote_daemon(&runner, &options).expect("daemon start succeeds");
        let start_args = args.borrow().clone();
        let start_command = start_args.last().expect("ssh command is recorded");
        assert!(!start_command.starts_with("cd "));
        assert!(start_command.contains("$HOME/'.local/bin/bowline' daemon start --json"));

        daemon_status_remote(&runner, &options).expect("daemon status succeeds");
        let daemon_status_args = args.borrow().clone();
        let daemon_status_command = daemon_status_args.last().expect("ssh command is recorded");
        assert!(!daemon_status_command.starts_with("cd "));
        assert!(daemon_status_command.contains("$HOME/'.local/bin/bowline' daemon status --json"));
    }

    #[test]
    fn remote_status_uses_explicit_root_without_requiring_root_cwd() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let stdin = Rc::new(RefCell::new(String::new()));
        let runner = RecordingRunner {
            args: args.clone(),
            stdin,
        };
        let options = BootstrapSshOptions {
            host: "linux-box".to_string(),
            root: "~/Code/new machine".to_string(),
            remote_binary: Some("~/.local/bin/bowline".to_string()),
            remote_workspace_id: Some("ws_agent".to_string()),
            remote_env: Vec::new(),
            remote_secret_env: Vec::new(),
            bootstrap_token: None,
        };

        status_remote(&runner, &options).expect("status succeeds");

        let captured = args.borrow();
        let remote_command = captured.last().expect("ssh command is recorded");
        assert!(!remote_command.starts_with("cd "));
        assert!(
            remote_command
                .contains("$HOME/'.local/bin/bowline' status $HOME/'Code/new machine' --json")
        );
    }

    #[test]
    fn remote_agent_lease_runs_from_root_with_env_on_bowline_command() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let stdin = Rc::new(RefCell::new(String::new()));
        let runner = RecordingRunner {
            args: args.clone(),
            stdin,
        };
        let options = BootstrapSshOptions {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            remote_binary: Some("~/.local/bin/bowline".to_string()),
            remote_workspace_id: Some("ws_agent".to_string()),
            remote_env: vec![(
                "BOWLINE_SECRET_STORE".to_string(),
                "server-local".to_string(),
            )],
            remote_secret_env: Vec::new(),
            bootstrap_token: None,
        };

        create_remote_agent_lease(&runner, &options, "foo", "fix auth")
            .expect("lease command succeeds");

        let captured = args.borrow();
        let remote_command = captured.last().expect("ssh command is recorded");
        assert!(remote_command.contains(
            "cd $HOME/'Code' && BOWLINE_SECRET_STORE='server-local' \
             $HOME/'.local/bin/bowline' agent start 'foo' --task 'fix auth'"
        ));
    }

    #[test]
    fn remote_codex_launch_exports_path_before_bowline_env_prefix() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let stdin = Rc::new(RefCell::new(String::new()));
        let runner = RecordingRunner {
            args: args.clone(),
            stdin,
        };
        let options = BootstrapSshOptions {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            remote_binary: Some("~/.local/bin/bowline".to_string()),
            remote_workspace_id: Some("ws_agent".to_string()),
            remote_env: vec![
                ("BOWLINE_WORKSPACE_ID".to_string(), "ws_agent".to_string()),
                (
                    "BOWLINE_SECRET_STORE".to_string(),
                    "server-local".to_string(),
                ),
            ],
            remote_secret_env: Vec::new(),
            bootstrap_token: None,
        };

        launch_remote_codex_agent(&runner, &options, "lease_123", "~/Code/app")
            .expect("codex launch command succeeds");

        let captured = args.borrow();
        let remote_command = captured.last().expect("ssh command is recorded");
        let path_export = remote_command
            .find("export PATH=\"$HOME/.local/bin:$PATH\";")
            .expect("PATH export is present");
        let env_prefix = remote_command
            .find("BOWLINE_WORKSPACE_ID=")
            .expect("workspace env prefix is present");
        let agent_prompt = remote_command
            .find("agent prompt --lease")
            .expect("agent prompt command is present");
        assert!(
            path_export < env_prefix && env_prefix < agent_prompt,
            "{remote_command}"
        );
        assert!(!remote_command.contains("BOWLINE_SECRET_STORE='server-local' export PATH"));
    }
}
