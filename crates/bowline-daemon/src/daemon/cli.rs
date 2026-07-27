use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{env, io};

use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::metadata::default_control_socket_path;
use bowline_local::notifications::NotificationDedupe;

use crate::daemon::log_supervisor::spawn_log_cap_supervisor;
use crate::daemon::{
    ContinuousSyncRuntime, DEFAULT_SOCKET_FALLBACK, DaemonRuntime, EXIT_FAILURE, EXIT_USAGE,
    PROTOCOL, PROTOCOL_VERSION, SyncArgs, bind_daemon_state_root, metrics_snapshot,
    request_shutdown, serve, status_snapshot,
};

mod output;

use bowline_daemon_rpc::DaemonReachability;
use output::{
    DaemonProcess, ErrorCode, ErrorOutput, HelpOutput, MetricsOutput, SocketProtocol, StatusOutput,
    StopOutput, VersionOutput, print_json,
};

const CONTRACT_VERSION: u16 = bowline_core::wire::MACHINE_CONTRACT_VERSION;
const COMMAND_NAMES: &[&str] = &["serve", "stop", "status", "metrics", "version"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cli {
    pub(super) json: bool,
    pub(super) socket: PathBuf,
    pub(super) continuous_sync: Option<SyncArgs>,
    pub(super) notify_approvals: bool,
    pub(super) command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    Help,
    Serve { once: bool },
    Stop,
    Status,
    Metrics,
    Version,
    UsageError(String),
    Unknown(String),
}

pub(super) fn entrypoint() -> ExitCode {
    install_panic_hook();
    let cli = parse_args(env::args().skip(1));
    run(cli)
}

pub(super) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        eprintln!(
            "bowline-daemon hit an internal error. Run `bowline daemon status`; environment values were not printed."
        );
    }));
}

/// The four sync flags as parsed. They are one unit: a daemon configured with
/// some of them would bind its socket, answer status, and sync nothing.
#[derive(Debug, Default)]
struct SyncFlags {
    root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    workspace_id: Option<WorkspaceId>,
    device_id: Option<DeviceId>,
}

pub(super) fn parse_args<I, S>(args: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut json = false;
    let mut socket = default_socket_path();
    let mut once = false;
    let mut sync = SyncFlags::default();
    let mut notify_approvals = false;
    let mut positionals = Vec::new();
    let mut iter = args.into_iter().map(Into::into);

    while let Some(arg) = iter.next() {
        let value = match arg.as_str() {
            "--json" => {
                json = true;
                continue;
            }
            "--once" => {
                once = true;
                continue;
            }
            "--notify-approvals" => {
                notify_approvals = true;
                continue;
            }
            "-h" | "--help" => {
                positionals.push("help".to_string());
                continue;
            }
            "-V" | "--version" => {
                positionals.push("version".to_string());
                continue;
            }
            "--socket" | "--sync-root" | "--sync-state-root" | "--sync-workspace"
            | "--sync-device" => match iter.next() {
                Some(value) => value,
                None => {
                    return usage_error(
                        json,
                        socket,
                        notify_approvals,
                        format!("missing value for {arg}"),
                    );
                }
            },
            _ => {
                positionals.push(arg);
                continue;
            }
        };
        match arg.as_str() {
            "--socket" => socket = PathBuf::from(value),
            "--sync-root" => sync.root = Some(PathBuf::from(value)),
            "--sync-state-root" => sync.state_root = Some(PathBuf::from(value)),
            "--sync-workspace" => sync.workspace_id = Some(WorkspaceId::new(value)),
            "--sync-device" => sync.device_id = Some(DeviceId::new(value)),
            _ => unreachable!("only value-taking flags reach this match"),
        }
    }

    let continuous_sync = match continuous_sync_args(sync) {
        Ok(continuous_sync) => continuous_sync,
        Err(message) => return usage_error(json, socket, notify_approvals, message),
    };

    let command = match positionals.as_slice() {
        [] => Command::Help,
        [command] if command == "help" => Command::Help,
        [command] if command == "serve" => Command::Serve { once },
        [command] if command == "stop" => Command::Stop,
        [command] if command == "status" => Command::Status,
        [command] if command == "metrics" => Command::Metrics,
        [command] if command == "version" => Command::Version,
        [command, ..] => Command::Unknown(command.clone()),
    };

    Cli {
        json,
        socket,
        continuous_sync,
        notify_approvals,
        command,
    }
}

fn usage_error(json: bool, socket: PathBuf, notify_approvals: bool, message: String) -> Cli {
    Cli {
        json,
        socket,
        continuous_sync: None,
        notify_approvals,
        command: Command::UsageError(message),
    }
}

pub(super) fn default_socket_path() -> PathBuf {
    default_control_socket_path().unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET_FALLBACK))
}

/// Accept the sync flags only as a complete set. A partial set used to yield
/// `None` silently, producing a daemon that answers RPCs and syncs nothing;
/// there are no default identities to fall back on, because a placeholder
/// workspace id makes every later error say "different workspace" instead of
/// "misconfigured at launch".
fn continuous_sync_args(flags: SyncFlags) -> Result<Option<SyncArgs>, String> {
    let SyncFlags {
        root,
        state_root,
        workspace_id,
        device_id,
    } = flags;
    let present = [
        root.is_some(),
        state_root.is_some(),
        workspace_id.is_some(),
        device_id.is_some(),
    ];
    if present.iter().all(|flag| !flag) {
        return Ok(None);
    }
    match (root, state_root, workspace_id, device_id) {
        (Some(root), Some(state_root), Some(workspace_id), Some(device_id)) => {
            Ok(Some(SyncArgs {
                root,
                state_root,
                workspace_id,
                device_id,
            }))
        }
        _ => Err(
            "--sync-root, --sync-state-root, --sync-workspace and --sync-device must be given together"
                .to_string(),
        ),
    }
}

pub(super) fn run(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Help => {
            print_help(cli.json);
            ExitCode::SUCCESS
        }
        Command::Serve { once } => {
            if let Some(sync) = &cli.continuous_sync {
                bind_daemon_state_root(&sync.state_root);
                // The supervisor opened this daemon's log files before exec and
                // will never rotate them, so the cap starts with the daemon.
                spawn_log_cap_supervisor(sync.state_root.clone());
            }
            match serve(
                &cli.socket,
                once,
                DaemonRuntime {
                    // Engine construction reads the secret store and refreshes
                    // remote trust. Leave it pending for the scheduler's first
                    // drive so the control socket is available while that I/O
                    // runs on the background scheduler.
                    sync: cli.continuous_sync.map(ContinuousSyncRuntime::new),
                    notify_approvals: cli.notify_approvals,
                    notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
                    next_notification_poll: Instant::now(),
                    pending_notification_status: None,
                },
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    print_runtime_error("serve", &error, cli.json);
                    ExitCode::from(EXIT_FAILURE)
                }
            }
        }
        Command::Stop => print_stop(&cli.socket, cli.json),
        Command::Status => print_status(&cli.socket, cli.json),
        Command::Metrics => print_metrics(&cli.socket, cli.json),
        Command::Version => {
            print_version(cli.json);
            ExitCode::SUCCESS
        }
        Command::UsageError(message) => {
            print_usage_error(&message, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
        Command::Unknown(command) => {
            print_unknown_command(&command, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
    }
}

pub(super) fn print_help(json: bool) {
    if json {
        print_json(&HelpOutput {
            ok: true,
            command: "help",
            contract_version: CONTRACT_VERSION,
            commands: COMMAND_NAMES,
            socket: SocketProtocol::current(),
        });
        return;
    }
    println!(
        "bowline daemon\n\nCommands:\n  bowline-daemon serve [--sync-root <path> --sync-state-root <path> --sync-workspace <id> --sync-device <id>] [--notify-approvals]\n  bowline-daemon stop\n  bowline-daemon status\n  bowline-daemon metrics\n  bowline-daemon version\n\nGlobal options:\n  --json\n  --socket <path>"
    );
}

pub(super) fn print_status(socket: &Path, json: bool) -> ExitCode {
    match status_snapshot(socket) {
        Ok(status) => {
            let daemon = DaemonProcess::running(socket, status.daemon_version.clone());
            if json {
                print_json(&StatusOutput {
                    ok: true,
                    command: "status",
                    contract_version: CONTRACT_VERSION,
                    daemon,
                    snapshot: Some(status.snapshot),
                });
            } else {
                println!(
                    "bowline-daemon: running ({PROTOCOL} v{PROTOCOL_VERSION}, daemon {})",
                    status.daemon_version
                );
            }
            ExitCode::SUCCESS
        }
        Err(reachability) => {
            let daemon = DaemonProcess::unavailable(socket, &reachability);
            let exit = daemon.state.exit_code();
            if json {
                print_json(&StatusOutput {
                    ok: daemon.state.is_expected(),
                    command: "status",
                    contract_version: CONTRACT_VERSION,
                    daemon,
                    snapshot: None,
                });
            } else {
                print_unavailable_line(&reachability);
            }
            exit
        }
    }
}

pub(super) fn print_metrics(socket: &Path, json: bool) -> ExitCode {
    match metrics_snapshot(socket) {
        Ok(snapshot) => {
            if json {
                print_json(&MetricsOutput {
                    ok: true,
                    command: "metrics",
                    contract_version: CONTRACT_VERSION,
                    daemon: DaemonProcess::running(socket, snapshot.daemon_version),
                    metrics: Some(snapshot.metrics),
                });
            } else {
                println!("bowline-daemon metrics: {}", snapshot.metrics);
            }
            ExitCode::SUCCESS
        }
        Err(reachability) => {
            let daemon = DaemonProcess::unavailable(socket, &reachability);
            let exit = daemon.state.exit_code();
            if json {
                print_json(&MetricsOutput {
                    ok: daemon.state.is_expected(),
                    command: "metrics",
                    contract_version: CONTRACT_VERSION,
                    daemon,
                    metrics: None,
                });
            } else {
                print_unavailable_line(&reachability);
            }
            exit
        }
    }
}

fn print_unavailable_line(reachability: &DaemonReachability) {
    match reachability {
        DaemonReachability::NotRunning => println!("bowline-daemon: stopped"),
        other => println!("bowline-daemon: {other} ({})", other.remediation()),
    }
}

pub(super) fn print_stop(socket: &Path, json: bool) -> ExitCode {
    match request_shutdown(socket) {
        Ok(()) => {
            if json {
                print_json(&StopOutput {
                    ok: true,
                    command: "stop",
                    contract_version: CONTRACT_VERSION,
                    daemon: DaemonProcess::stopping(socket),
                });
            } else {
                println!("bowline-daemon: stopping");
            }
            ExitCode::SUCCESS
        }
        Err(reachability) if reachability.is_absent() => {
            if json {
                print_json(&StopOutput {
                    ok: true,
                    command: "stop",
                    contract_version: CONTRACT_VERSION,
                    daemon: DaemonProcess::unavailable(socket, &reachability),
                });
            } else {
                println!("bowline-daemon: stopped");
            }
            ExitCode::SUCCESS
        }
        Err(reachability) => {
            let daemon = DaemonProcess::unavailable(socket, &reachability);
            let exit = daemon.state.exit_code();
            if json {
                print_json(&StopOutput {
                    ok: false,
                    command: "stop",
                    contract_version: CONTRACT_VERSION,
                    daemon,
                });
            } else {
                eprintln!(
                    "bowline-daemon stop failed: {reachability} ({})",
                    reachability.remediation()
                );
            }
            exit
        }
    }
}

pub(super) fn print_version(json: bool) {
    if json {
        print_json(&VersionOutput {
            ok: true,
            command: "version",
            contract_version: CONTRACT_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION"),
            socket: SocketProtocol::current(),
        });
    } else {
        println!(
            "bowline-daemon {} ({PROTOCOL} v{PROTOCOL_VERSION})",
            env!("CARGO_PKG_VERSION")
        );
    }
}

pub(super) fn print_usage_error(message: &str, json: bool) {
    if json {
        print_json(&ErrorOutput::new(
            None,
            ErrorCode::UsageError,
            message.to_string(),
        ));
    } else {
        eprintln!("bowline-daemon usage error: {message}");
    }
}

pub(super) fn print_unknown_command(command: &str, json: bool) {
    if json {
        print_json(&ErrorOutput::new(
            Some(command.to_string()),
            ErrorCode::UnknownCommand,
            "unknown command".to_string(),
        ));
    } else {
        eprintln!("bowline-daemon unknown command: {command}");
    }
}

pub(super) fn print_runtime_error(command: &str, error: &io::Error, json: bool) {
    if json {
        print_json(&ErrorOutput::new(
            Some(command.to_string()),
            ErrorCode::DaemonError,
            error.to_string(),
        ));
    } else {
        eprintln!("bowline-daemon {command} failed: {error}");
    }
}
