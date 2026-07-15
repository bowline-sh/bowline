use std::{
    env,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::bootstrap::{
    process::{ProcessError, ProcessRunner},
    ssh::{remote_shell_path, shell_quote, validate_ssh_host},
};

const REMOTE_INSTALL_PATH: &str = "~/.local/bin/bowline";
const REMOTE_DAEMON_INSTALL_PATH: &str = "~/.local/bin/bowline-daemon";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapInstallOptions {
    pub host: String,
    pub root: String,
    pub artifact: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlatform {
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBowlineInstall {
    pub platform: RemotePlatform,
    pub remote_binary: String,
    pub remote_daemon_binary: String,
    pub artifact_sha256: String,
    pub daemon_artifact_sha256: String,
}

#[derive(Debug)]
pub enum BootstrapInstallError {
    InvalidHost(String),
    Process(ProcessError),
    Io(io::Error),
    CurrentExecutable(io::Error),
    ProbeFailed { status_code: i32, stderr: String },
    UploadFailed { status_code: i32, stderr: String },
    InstallFailed { status_code: i32, stderr: String },
    RemoteBuildFailed { status_code: i32, stderr: String },
    MissingArtifact(PathBuf),
    UnsupportedDefaultArtifact { local: String, remote: String },
}

pub fn install_or_update_bowline<R>(
    runner: &R,
    options: &BootstrapInstallOptions,
) -> Result<RemoteBowlineInstall, BootstrapInstallError>
where
    R: ProcessRunner,
{
    validate_ssh_host(options.host.as_str()).map_err(|reason| {
        BootstrapInstallError::InvalidHost(format!("invalid SSH host `{}`: {reason}", options.host))
    })?;
    let platform = detect_remote_platform(runner, &options.host)?;
    if options.artifact.is_none() && local_platform_label() != platform.label() {
        return install_or_update_from_remote_source(runner, options, platform);
    }
    let artifacts = choose_artifacts(options.artifact.as_deref(), &platform)?;
    let artifact_sha256 = sha256_hex(&artifacts.cli)?;
    let daemon_artifact_sha256 = sha256_hex(&artifacts.daemon)?;
    let remote_temp = format!("~/.local/bin/.bowline-bootstrap-{}", &artifact_sha256[..16]);
    let remote_daemon_temp = format!(
        "~/.local/bin/.bowline-daemon-bootstrap-{}",
        &daemon_artifact_sha256[..16]
    );

    run_remote_setup(runner, options)?;
    upload_artifact(runner, &options.host, &artifacts.cli, &remote_temp)?;
    upload_artifact(
        runner,
        &options.host,
        &artifacts.daemon,
        &remote_daemon_temp,
    )?;
    verify_and_install(
        runner,
        &options.host,
        &remote_temp,
        &artifact_sha256,
        REMOTE_INSTALL_PATH,
    )?;
    verify_and_install(
        runner,
        &options.host,
        &remote_daemon_temp,
        &daemon_artifact_sha256,
        REMOTE_DAEMON_INSTALL_PATH,
    )?;

    Ok(RemoteBowlineInstall {
        platform,
        remote_binary: REMOTE_INSTALL_PATH.to_string(),
        remote_daemon_binary: REMOTE_DAEMON_INSTALL_PATH.to_string(),
        artifact_sha256,
        daemon_artifact_sha256,
    })
}

fn install_or_update_from_remote_source<R>(
    runner: &R,
    options: &BootstrapInstallOptions,
    platform: RemotePlatform,
) -> Result<RemoteBowlineInstall, BootstrapInstallError>
where
    R: ProcessRunner,
{
    let source_root = env::current_dir()?;
    let remote_source = "~/.cache/bowline/bootstrap-source";
    run_remote_setup(runner, options)?;
    sync_source_to_remote(runner, &options.host, &source_root, remote_source)?;
    let (artifact_sha256, daemon_artifact_sha256) =
        build_and_install_remote_source(runner, &options.host, remote_source)?;

    Ok(RemoteBowlineInstall {
        platform,
        remote_binary: REMOTE_INSTALL_PATH.to_string(),
        remote_daemon_binary: REMOTE_DAEMON_INSTALL_PATH.to_string(),
        artifact_sha256,
        daemon_artifact_sha256,
    })
}

fn detect_remote_platform<R>(
    runner: &R,
    host: &str,
) -> Result<RemotePlatform, BootstrapInstallError>
where
    R: ProcessRunner,
{
    let output = runner.run(
        "ssh",
        &ssh_args(host, "printf '%s\\n%s\\n' \"$(uname -s)\" \"$(uname -m)\""),
    )?;
    if output.status_code != 0 {
        return Err(BootstrapInstallError::ProbeFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    let mut lines = output.stdout.lines();
    let os = lines
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase();
    let arch = lines
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase();
    Ok(RemotePlatform { os, arch })
}

struct BootstrapArtifacts {
    cli: PathBuf,
    daemon: PathBuf,
}

fn choose_artifacts(
    override_artifact: Option<&Path>,
    platform: &RemotePlatform,
) -> Result<BootstrapArtifacts, BootstrapInstallError> {
    if let Some(path) = override_artifact {
        if !path.is_file() {
            return Err(BootstrapInstallError::MissingArtifact(path.to_path_buf()));
        }
        let daemon = daemon_artifact_for(path);
        if !daemon.is_file() {
            return Err(BootstrapInstallError::MissingArtifact(daemon));
        }
        return Ok(BootstrapArtifacts {
            cli: path.to_path_buf(),
            daemon,
        });
    }

    let local = local_platform_label();
    let remote = platform.label();
    if local != remote {
        return Err(BootstrapInstallError::UnsupportedDefaultArtifact { local, remote });
    }
    let cli = env::current_exe().map_err(BootstrapInstallError::CurrentExecutable)?;
    let daemon = daemon_artifact_for(&cli);
    if !daemon.is_file() {
        return Err(BootstrapInstallError::MissingArtifact(daemon));
    }
    Ok(BootstrapArtifacts { cli, daemon })
}

fn daemon_artifact_for(cli: &Path) -> PathBuf {
    let Some(file_name) = cli.file_name().and_then(|name| name.to_str()) else {
        return cli.with_file_name("bowline-daemon");
    };
    if file_name == "bowline" {
        return cli.with_file_name("bowline-daemon");
    }
    if file_name == "bowline.exe" {
        return cli.with_file_name("bowline-daemon.exe");
    }
    if let Some(prefix) = file_name.strip_suffix("-bowline") {
        return cli.with_file_name(format!("{prefix}-bowline-daemon"));
    }
    cli.with_file_name(format!("{file_name}-daemon"))
}

fn run_remote_setup<R>(
    runner: &R,
    options: &BootstrapInstallOptions,
) -> Result<(), BootstrapInstallError>
where
    R: ProcessRunner,
{
    let command = format!(
        "mkdir -p \"$HOME/.local/bin\" {}",
        remote_shell_path(options.root.as_str())
    );
    let output = runner.run("ssh", &ssh_args(&options.host, &command))?;
    if output.status_code != 0 {
        return Err(BootstrapInstallError::InstallFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    Ok(())
}

fn sync_source_to_remote<R>(
    runner: &R,
    host: &str,
    source_root: &Path,
    remote_source: &str,
) -> Result<(), BootstrapInstallError>
where
    R: ProcessRunner,
{
    let mkdir = format!("mkdir -p {}", remote_shell_path(remote_source));
    let setup = runner.run("ssh", &ssh_args(host, &mkdir))?;
    if setup.status_code != 0 {
        return Err(BootstrapInstallError::InstallFailed {
            status_code: setup.status_code,
            stderr: setup.stderr,
        });
    }

    let mut source = source_root.display().to_string();
    if !source.ends_with('/') {
        source.push('/');
    }
    let target = format!("{host}:{remote_source}/");
    let args = vec![
        "-az".to_string(),
        "--delete".to_string(),
        "--exclude".to_string(),
        "target".to_string(),
        "--exclude".to_string(),
        ".git".to_string(),
        "--exclude".to_string(),
        "node_modules".to_string(),
        "--exclude".to_string(),
        ".tmp".to_string(),
        source,
        target,
    ];
    let output = runner.run("rsync", &args)?;
    if output.status_code != 0 {
        return Err(BootstrapInstallError::UploadFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    Ok(())
}

fn build_and_install_remote_source<R>(
    runner: &R,
    host: &str,
    remote_source: &str,
) -> Result<(String, String), BootstrapInstallError>
where
    R: ProcessRunner,
{
    let source = remote_shell_path(remote_source);
    let cli_install = remote_shell_path(REMOTE_INSTALL_PATH);
    let daemon_install = remote_shell_path(REMOTE_DAEMON_INSTALL_PATH);
    let command = format!(
        "set -e; cd {source}; cargo build -p bowline -p bowline-daemon; install -m 755 target/debug/bowline {cli_install}; install -m 755 target/debug/bowline-daemon {daemon_install}; sha256sum {cli_install} {daemon_install}"
    );
    let output = runner.run("ssh", &ssh_args(host, &command))?;
    if output.status_code != 0 {
        return Err(BootstrapInstallError::RemoteBuildFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    let hashes = output
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|hash| hash.len() == 64 && hash.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let Some(cli_hash) = hashes.first().cloned() else {
        return Err(BootstrapInstallError::RemoteBuildFailed {
            status_code: 0,
            stderr: "remote build did not print the bowline checksum".to_string(),
        });
    };
    let Some(daemon_hash) = hashes.get(1).cloned() else {
        return Err(BootstrapInstallError::RemoteBuildFailed {
            status_code: 0,
            stderr: "remote build did not print the bowline-daemon checksum".to_string(),
        });
    };
    Ok((cli_hash, daemon_hash))
}

fn upload_artifact<R>(
    runner: &R,
    host: &str,
    artifact: &Path,
    remote_temp: &str,
) -> Result<(), BootstrapInstallError>
where
    R: ProcessRunner,
{
    let target = format!("{host}:{remote_temp}");
    let args = vec![
        "-az".to_string(),
        "--partial".to_string(),
        artifact.display().to_string(),
        target,
    ];
    let output = runner.run("rsync", &args)?;
    if output.status_code != 0 {
        return Err(BootstrapInstallError::UploadFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    Ok(())
}

fn verify_and_install<R>(
    runner: &R,
    host: &str,
    remote_temp: &str,
    expected_sha256: &str,
    remote_install_path: &str,
) -> Result<(), BootstrapInstallError>
where
    R: ProcessRunner,
{
    let remote_temp = remote_shell_path(remote_temp);
    let install_path = remote_shell_path(remote_install_path);
    let command = format!(
        "set -e; if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum {remote_temp} | awk '{{print $1}}'); else actual=$(shasum -a 256 {remote_temp} | awk '{{print $1}}'); fi; [ \"$actual\" = {} ] || {{ echo 'bowline bootstrap checksum mismatch' >&2; exit 42; }}; chmod 755 {remote_temp}; mv {remote_temp} {install_path}",
        shell_quote(expected_sha256),
    );
    let output = runner.run("ssh", &ssh_args(host, &command))?;
    if output.status_code != 0 {
        return Err(BootstrapInstallError::InstallFailed {
            status_code: output.status_code,
            stderr: output.stderr,
        });
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String, BootstrapInstallError> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(bytes);
    Ok(format!("{digest:x}"))
}

fn ssh_args(host: &str, command: &str) -> Vec<String> {
    let mut args = ssh_options();
    args.push(host.to_string());
    args.push(command.to_string());
    args
}

fn ssh_options() -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
    ]
}

fn local_platform_label() -> String {
    format!(
        "{}-{}",
        match env::consts::OS {
            "macos" => "darwin",
            other => other,
        },
        env::consts::ARCH
    )
}

impl RemotePlatform {
    pub fn label(&self) -> String {
        format!(
            "{}-{}",
            match self.os.as_str() {
                "darwin" => "darwin",
                "linux" => "linux",
                other => other,
            },
            match self.arch.as_str() {
                "x86_64" | "amd64" => "x86_64",
                "aarch64" | "arm64" => "aarch64",
                other => other,
            }
        )
    }
}

impl fmt::Display for BootstrapInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHost(error) => formatter.write_str(error),
            Self::Process(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "bootstrap artifact read failed: {error}"),
            Self::CurrentExecutable(error) => {
                write!(
                    formatter,
                    "could not locate the local bowline executable: {error}"
                )
            }
            Self::ProbeFailed {
                status_code,
                stderr,
            } => write!(
                formatter,
                "remote platform probe failed with status {status_code}: {stderr}"
            ),
            Self::UploadFailed {
                status_code,
                stderr,
            } => write!(
                formatter,
                "remote bowline upload failed with status {status_code}: {stderr}"
            ),
            Self::InstallFailed {
                status_code,
                stderr,
            } => write!(
                formatter,
                "remote bowline install failed with status {status_code}: {stderr}"
            ),
            Self::RemoteBuildFailed {
                status_code,
                stderr,
            } => write!(
                formatter,
                "remote bowline build failed with status {status_code}: {stderr}"
            ),
            Self::MissingArtifact(path) => {
                write!(
                    formatter,
                    "bootstrap artifact does not exist: {}",
                    path.display()
                )
            }
            Self::UnsupportedDefaultArtifact { local, remote } => write!(
                formatter,
                "local bowline binary is for {local}, but remote host is {remote}; pass --binary <remote-compatible-bowline-artifact>"
            ),
        }
    }
}

impl Error for BootstrapInstallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Process(error) => Some(error),
            Self::Io(error) | Self::CurrentExecutable(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProcessError> for BootstrapInstallError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<io::Error> for BootstrapInstallError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::PathBuf, rc::Rc};

    use super::{RemotePlatform, local_platform_label, verify_and_install};
    use crate::bootstrap::process::{ProcessError, ProcessOutput, ProcessRunner};
    use crate::bootstrap::ssh::remote_shell_path;

    #[derive(Clone)]
    struct RecordingRunner {
        args: Rc<RefCell<Vec<String>>>,
    }

    impl ProcessRunner for RecordingRunner {
        fn run(&self, _program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
            *self.args.borrow_mut() = args.to_vec();
            Ok(ProcessOutput {
                status_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn platform_label_normalizes_common_uname_values() {
        assert_eq!(
            RemotePlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
            }
            .label(),
            "linux-x86_64"
        );
        assert_eq!(
            RemotePlatform {
                os: "darwin".to_string(),
                arch: "arm64".to_string(),
            }
            .label(),
            "darwin-aarch64"
        );
        assert!(local_platform_label().contains('-'));
    }

    #[test]
    fn remote_shell_path_preserves_remote_home_for_local_home_expansion() {
        let home = std::env::var("HOME").expect("HOME exists");
        let local_code = PathBuf::from(home).join("Code");

        assert_eq!(
            remote_shell_path(local_code.to_str().expect("utf8 path")),
            "$HOME/Code"
        );
        assert_eq!(remote_shell_path("~/Code"), "$HOME/Code");
    }

    #[test]
    fn verify_install_uses_command_substitution_for_checksum() {
        let args = Rc::new(RefCell::new(Vec::new()));
        let runner = RecordingRunner { args: args.clone() };

        verify_and_install(
            &runner,
            "linux-box",
            "~/.local/bin/.bowline-bootstrap-test",
            "abc123",
            "~/.local/bin/bowline",
        )
        .expect("install command succeeds");

        let captured = args.borrow();
        let command = captured.last().expect("ssh command is recorded");
        assert!(command.contains("actual=$(sha256sum"));
        assert!(command.contains("else actual=$(shasum -a 256"));
        assert!(!command.contains("actual=$(("));
        assert!(command.contains("bowline bootstrap checksum mismatch"));
    }
}
