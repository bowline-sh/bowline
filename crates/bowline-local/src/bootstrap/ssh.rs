use std::{error::Error, fmt};

pub use bowline_core::shell::quote_word as shell_quote;

use crate::{
    bootstrap::process::{ProcessError, ProcessRunner},
    daemon_env,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSshOptions {
    pub host: String,
    pub root: String,
    pub remote_binary: Option<String>,
    pub remote_platform: Option<String>,
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
    InvalidWorkspaceId(String),
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
            "device request --root {} --json",
            remote_bowline_path_arg(options.root.as_str())
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
    let output = run_remote_bowline(
        runner,
        options,
        &format!(
            "device list --root {} --json",
            remote_bowline_path_arg(options.root.as_str())
        ),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Asks the remote host, through its own secret-store abstraction, whether it
/// holds workspace key material. The question must go through the CLI: the
/// custody backend is the host's choice (OS keychain on a desktop, a file on a
/// headless agent host), so reading any one backend's storage answers for the
/// wrong hosts, and a shell that names a secrets file puts secret-adjacent
/// material into the SSH command line every operator's process list can read.
pub fn workspace_key_status_remote<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    workspace_id: &str,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let workspace_id = validated_remote_state_id(workspace_id)?;
    let output = run_remote_bowline(
        runner,
        options,
        &format!(
            "device key-status --workspace {} --json",
            shell_quote(workspace_id)
        ),
    )?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
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
        &format!(
            "device accept --root {} --request {} --json",
            remote_shell_path(&options.root),
            shell_quote(request_id)
        ),
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
        &format!(
            "setup --root {} --json",
            remote_bowline_path_arg(options.root.as_str())
        ),
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
    let workspace_id = validated_remote_state_id(workspace_id)?;
    let workspace_db = remote_shell_path(&format!(
        "~/.local/share/bowline/workspaces/{workspace_id}/local.sqlite3"
    ));
    let workspace_state =
        remote_shell_path(&format!("~/.local/share/bowline/workspaces/{workspace_id}"));
    let daemon_env = remote_shell_path(&format!(
        "~/.local/share/bowline/workspaces/{workspace_id}/daemon.env"
    ));
    let command = format!(
        "set -e; case \"$(uname -s)\" in Darwin) dir=\"$HOME/Library/Application Support/bowline\" ;; Linux) dir=\"${{XDG_STATE_HOME:-$HOME/.local/state}}/bowline\" ;; *) dir=\"$HOME/.bowline\" ;; esac; mkdir -p \"$dir\" {workspace_state}; ln -sfn {workspace_db} \"$dir/local.sqlite3\"; umask 077; cat > {daemon_env}; chmod 600 {daemon_env}"
    );
    let output = run_remote_shell_with_stdin(runner, options, &command, &daemon_env_file(options))?;
    Ok(RemoteBootstrapProbe {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub fn install_remote_daemon_service<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<RemoteBootstrapProbe, BootstrapSshError>
where
    R: ProcessRunner,
{
    let output = run_remote_bowline(runner, options, "daemon install --json")?;
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
        &format!(
            "status --root {} --json",
            remote_bowline_path_arg(options.root.as_str())
        ),
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

fn run_remote_bowline_with_prefix<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    bowline_args: &str,
    command_prefix: &str,
) -> Result<crate::bootstrap::process::ProcessOutput, BootstrapSshError>
where
    R: ProcessRunner,
{
    run_remote_bowline_with_prefix_and_stdin(runner, options, bowline_args, command_prefix, "")
}

fn run_remote_bowline_with_prefix_and_stdin<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    bowline_args: &str,
    command_prefix: &str,
    extra_stdin: &str,
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
        remote_state_prefix(options)?,
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
    let mut stdin = remote_stdin_env_stdin(options);
    stdin.push_str(extra_stdin);
    let output = if stdin.is_empty() {
        runner.run("ssh", &args)?
    } else {
        runner.run_with_stdin("ssh", &args, &stdin)?
    };
    if output.status_code != 0 {
        return Err(BootstrapSshError::RemoteFailed {
            status_code: output.status_code,
            stderr: remote_failure_detail(&output.stdout, &output.stderr),
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

fn remote_bowline_path_arg(value: &str) -> String {
    shell_quote(&normalize_remote_home(value))
}

fn remote_failure_detail(stdout: &str, stderr: &str) -> String {
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    detail.chars().take(2_048).collect()
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

/// Selects the remote per-workspace metadata database. A workspace id that
/// cannot address remote state is refused rather than silently dropped: the
/// empty prefix would run every remote command against the host's *default*
/// database, which succeeds against the wrong state with nothing to diagnose.
fn remote_state_prefix(options: &BootstrapSshOptions) -> Result<String, BootstrapSshError> {
    let Some(workspace_id) = options.remote_workspace_id.as_deref() else {
        return Ok(String::new());
    };
    let workspace_id = validated_remote_state_id(workspace_id)?;
    let state_dir = remote_shell_path(&format!("~/.local/share/bowline/workspaces/{workspace_id}"));
    let db_path = remote_shell_path(&format!(
        "~/.local/share/bowline/workspaces/{workspace_id}/local.sqlite3"
    ));
    Ok(format!(
        "mkdir -p {state_dir}; BOWLINE_METADATA_DB={db_path}; export BOWLINE_METADATA_DB; "
    ))
}

fn validated_remote_state_id(workspace_id: &str) -> Result<&str, BootstrapSshError> {
    if valid_remote_state_id(workspace_id) {
        return Ok(workspace_id);
    }
    Err(BootstrapSshError::InvalidWorkspaceId(
        workspace_id.to_string(),
    ))
}

/// `sysexits.h` EX_CONFIG. The remote uses it for "these credentials can never
/// arrive on this invocation", which is a misconfigured call rather than work
/// that failed and could be retried.
const REMOTE_STDIN_EXIT_CODE: u8 = 78;

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
    // `read` on a terminal waits forever, and on a closed stdin leaves the
    // variable unset so the CLI reports a missing configuration it cannot
    // explain. Both are refused here, where the reason is still known.
    let mut prefix = format!(
        "if [ -t 0 ]; then echo 'bowline: remote bootstrap credentials must be piped on stdin; \
         run `bowline connect <host>` instead of this command by hand' >&2; \
         exit {REMOTE_STDIN_EXIT_CODE}; fi; "
    );
    for key in keys {
        prefix.push_str(&format!(
            "IFS= read -r {key} || {{ echo 'bowline: bootstrap credential {key} was not delivered \
             on stdin' >&2; exit {REMOTE_STDIN_EXIT_CODE}; }}; export {key}; "
        ));
    }
    prefix
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
            | "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN"
            | "BOWLINE_WORKOS_ACCESS_TOKEN"
            | "BOWLINE_CONTROL_PLANE_TOKEN"
    )
}

/// The remote `daemon.env` is rendered by the one writer that owns the format,
/// so the key set is not a fourth hand-maintained allow-list.
fn daemon_env_file(options: &BootstrapSshOptions) -> String {
    daemon_env::render(
        options
            .remote_env
            .iter()
            .chain(options.remote_secret_env.iter())
            .map(|(key, value)| (key.as_str(), value.as_str())),
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
            Self::InvalidWorkspaceId(workspace_id) => write!(
                formatter,
                "workspace id `{workspace_id}` cannot address remote state; \
                 expected only letters, digits, `-` and `_`"
            ),
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
            Self::InvalidHost(_) | Self::InvalidWorkspaceId(_) => None,
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
mod tests;
