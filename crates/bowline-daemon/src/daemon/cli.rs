use super::*;

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

pub(super) fn parse_args<I, S>(args: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut json = false;
    let mut socket = default_socket_path();
    let mut once = false;
    let mut sync_root = None;
    let mut sync_state_root = None;
    let mut sync_workspace_id = "ws_code".to_string();
    let mut sync_device_id = "device-daemon".to_string();
    let mut notify_approvals = false;
    let mut positionals = Vec::new();
    let mut iter = args.into_iter().map(Into::into);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--once" => once = true,
            "--notify-approvals" => notify_approvals = true,
            "--socket" => match iter.next() {
                Some(path) => socket = PathBuf::from(path),
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError("missing value for --socket".to_string()),
                    };
                }
            },
            "--sync-root" => match iter.next() {
                Some(path) => sync_root = Some(PathBuf::from(path)),
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError("missing value for --sync-root".to_string()),
                    };
                }
            },
            "--sync-state-root" => match iter.next() {
                Some(path) => sync_state_root = Some(PathBuf::from(path)),
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-state-root".to_string(),
                        ),
                    };
                }
            },
            "--sync-workspace" => match iter.next() {
                Some(value) => sync_workspace_id = value,
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-workspace".to_string(),
                        ),
                    };
                }
            },
            "--sync-device" => match iter.next() {
                Some(value) => sync_device_id = value,
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError("missing value for --sync-device".to_string()),
                    };
                }
            },
            "-h" | "--help" => positionals.push("help".to_string()),
            "-V" | "--version" => positionals.push("version".to_string()),
            _ => positionals.push(arg),
        }
    }

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
        continuous_sync: continuous_sync_args(
            sync_root,
            sync_state_root,
            sync_workspace_id,
            sync_device_id,
        ),
        notify_approvals,
        command,
    }
}

pub(super) fn default_socket_path() -> PathBuf {
    default_control_socket_path().unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET_FALLBACK))
}

pub(super) fn continuous_sync_args(
    root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    workspace_id: String,
    device_id: String,
) -> Option<SyncArgs> {
    Some(SyncArgs {
        root: root?,
        state_root: state_root?,
        workspace_id,
        device_id,
    })
}

pub(super) fn run(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Help => {
            print_help(cli.json);
            ExitCode::SUCCESS
        }
        Command::Serve { once } => {
            if let Some(sync) = &cli.continuous_sync {
                load_persisted_daemon_env(&sync.state_root);
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
        Command::Status => {
            print_status(&cli.socket, cli.json);
            ExitCode::SUCCESS
        }
        Command::Metrics => {
            print_metrics(&cli.socket, cli.json);
            ExitCode::SUCCESS
        }
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
        println!(
            "{{\"ok\":true,\"command\":\"help\",\"phase\":\"{PHASE}\",\"commands\":[\"serve\",\"stop\",\"status\",\"metrics\",\"version\"],\"socket\":{{\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}}}"
        );
        return;
    }
    println!(
        "bowline daemon\n\nCommands:\n  bowline-daemon serve [--sync-root <path> --sync-state-root <path>] [--notify-approvals]\n  bowline-daemon stop\n  bowline-daemon status\n  bowline-daemon version\n\nGlobal options:\n  --json\n  --socket <path>"
    );
}

pub(super) fn print_status(socket: &Path, json: bool) {
    match status_snapshot(socket) {
        Ok(status) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "command": "status",
                        "daemon": {
                            "state": "running",
                            "socket": socket.display().to_string(),
                            "protocol": PROTOCOL,
                            "version": PROTOCOL_VERSION,
                            "daemonVersion": status.daemon_version,
                        },
                        "snapshot": status.snapshot,
                    })
                );
            } else {
                println!(
                    "bowline-daemon: running ({PROTOCOL} v{PROTOCOL_VERSION}, daemon {})",
                    status.daemon_version
                );
            }
        }
        Err(_) => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"status\",\"daemon\":{{\"state\":\"stopped\",\"socket\":{},\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}}}",
                    json_string(&socket.display().to_string())
                );
            } else {
                println!("bowline-daemon: stopped");
            }
        }
    }
}

pub(super) fn print_metrics(socket: &Path, json: bool) {
    match metrics_snapshot(socket) {
        Ok(metrics) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "command": "metrics",
                        "metrics": metrics,
                    })
                );
            } else {
                println!("bowline-daemon metrics: {metrics}");
            }
        }
        Err(_) => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"metrics\",\"daemon\":{{\"state\":\"stopped\"}},\"metrics\":null}}"
                );
            } else {
                println!("bowline-daemon: stopped");
            }
        }
    }
}

pub(super) fn print_stop(socket: &Path, json: bool) -> ExitCode {
    match request_shutdown(socket) {
        Ok(()) => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"stop\",\"daemon\":{{\"state\":\"stopping\",\"socket\":{}}}}}",
                    json_string(&socket.display().to_string())
                );
            } else {
                println!("bowline-daemon: stopping");
            }
            ExitCode::SUCCESS
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"stop\",\"daemon\":{{\"state\":\"stopped\",\"socket\":{}}}}}",
                    json_string(&socket.display().to_string())
                );
            } else {
                println!("bowline-daemon: stopped");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error("stop", &error, json);
            ExitCode::from(EXIT_FAILURE)
        }
    }
}
pub(super) fn print_version(json: bool) {
    if json {
        println!(
            "{{\"ok\":true,\"command\":\"version\",\"daemonVersion\":{},\"socket\":{{\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}}}",
            json_string(env!("CARGO_PKG_VERSION"))
        );
    } else {
        println!(
            "bowline-daemon {} ({PROTOCOL} v{PROTOCOL_VERSION})",
            env!("CARGO_PKG_VERSION")
        );
    }
}

pub(super) fn print_usage_error(message: &str, json: bool) {
    if json {
        println!(
            "{{\"ok\":false,\"status\":\"usage_error\",\"error\":{{\"code\":\"usage_error\",\"message\":{},\"phase\":\"{PHASE}\"}}}}",
            json_string(message)
        );
    } else {
        eprintln!("bowline-daemon usage error: {message}");
    }
}

pub(super) fn print_unknown_command(command: &str, json: bool) {
    if json {
        println!(
            "{{\"ok\":false,\"command\":{},\"status\":\"usage_error\",\"error\":{{\"code\":\"unknown_command\",\"message\":{},\"phase\":\"{PHASE}\"}}}}",
            json_string(command),
            json_string("unknown command")
        );
    } else {
        eprintln!("bowline-daemon unknown command: {command}");
    }
}

pub(super) fn print_runtime_error(command: &str, error: &io::Error, json: bool) {
    if json {
        println!(
            "{{\"ok\":false,\"command\":{},\"status\":\"error\",\"error\":{{\"code\":\"daemon_error\",\"message\":{},\"phase\":\"{PHASE}\"}}}}",
            json_string(command),
            json_string(&error.to_string())
        );
    } else {
        eprintln!("bowline-daemon {command} failed: {error}");
    }
}
