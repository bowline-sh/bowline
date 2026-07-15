use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cli {
    pub(super) json: bool,
    pub(super) socket: PathBuf,
    pub(super) continuous_sync: Option<ContinuousSyncOptions>,
    pub(super) notify_approvals: bool,
    pub(super) command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    Help,
    Serve { once: bool },
    SyncOnce(SyncOnceArgs),
    Stop,
    Status,
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
    let mut sync_interval = DEFAULT_SYNC_INTERVAL;
    let mut sync_max_ticks = None;
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
            "--sync-interval-ms" => match iter.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(ms) if ms > 0 => sync_interval = Duration::from_millis(ms),
                    _ => {
                        return Cli {
                            json,
                            socket,
                            continuous_sync: None,
                            notify_approvals,
                            command: Command::UsageError(
                                "--sync-interval-ms must be a positive integer".to_string(),
                            ),
                        };
                    }
                },
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-interval-ms".to_string(),
                        ),
                    };
                }
            },
            "--sync-max-ticks" => match iter.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(ticks) => sync_max_ticks = Some(ticks),
                    _ => {
                        return Cli {
                            json,
                            socket,
                            continuous_sync: None,
                            notify_approvals,
                            command: Command::UsageError(
                                "--sync-max-ticks must be an integer".to_string(),
                            ),
                        };
                    }
                },
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-max-ticks".to_string(),
                        ),
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
        [command, rest @ ..] if command == "sync-once" => parse_sync_once_command(rest),
        [command] if command == "stop" => Command::Stop,
        [command] if command == "status" => Command::Status,
        [command] if command == "version" => Command::Version,
        [command, ..] => Command::Unknown(command.clone()),
    };

    Cli {
        json,
        socket,
        continuous_sync: continuous_sync_options(
            sync_root,
            sync_state_root,
            sync_workspace_id,
            sync_device_id,
            sync_interval,
            sync_max_ticks,
        ),
        notify_approvals,
        command,
    }
}

pub(super) fn default_socket_path() -> PathBuf {
    default_control_socket_path().unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET_FALLBACK))
}

pub(super) fn continuous_sync_options(
    root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    workspace_id: String,
    device_id: String,
    interval: Duration,
    max_ticks: Option<u64>,
) -> Option<ContinuousSyncOptions> {
    Some(ContinuousSyncOptions {
        args: SyncOnceArgs {
            root: root?,
            state_root: state_root?,
            workspace_id,
            device_id,
            sync_claim: None,
            scan_scope: Default::default(),
        },
        interval,
        max_ticks,
    })
}

pub(super) fn parse_sync_once_command(args: &[String]) -> Command {
    let mut root = None;
    let mut state_root = None;
    let mut workspace_id = "ws_code".to_string();
    let mut device_id = "device-daemon".to_string();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --root".to_string());
                };
                root = Some(PathBuf::from(value));
                index += 2;
            }
            "--state-root" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --state-root".to_string());
                };
                state_root = Some(PathBuf::from(value));
                index += 2;
            }
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --workspace".to_string());
                };
                workspace_id = value.to_string();
                index += 2;
            }
            "--device" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --device".to_string());
                };
                device_id = value.to_string();
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return Command::UsageError(format!("unknown sync-once option `{flag}`"));
            }
            value => {
                return Command::UsageError(format!("unexpected sync-once argument `{value}`"));
            }
        }
    }

    let Some(root) = root else {
        return Command::UsageError("sync-once requires --root <path>".to_string());
    };
    let Some(state_root) = state_root else {
        return Command::UsageError("sync-once requires --state-root <path>".to_string());
    };
    Command::SyncOnce(SyncOnceArgs {
        root,
        state_root,
        workspace_id,
        device_id,
        sync_claim: None,
        scan_scope: Default::default(),
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
                load_persisted_daemon_env(&sync.args.state_root);
            }
            match serve(
                &cli.socket,
                once,
                DaemonRuntime {
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
        Command::SyncOnce(args) => {
            load_persisted_daemon_env(&args.state_root);
            print_sync_once(args, cli.json)
        }
        Command::Stop => print_stop(&cli.socket, cli.json),
        Command::Status => {
            print_status(&cli.socket, cli.json);
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
            "{{\"ok\":true,\"command\":\"help\",\"phase\":\"{PHASE}\",\"commands\":[\"serve\",\"sync-once\",\"stop\",\"status\",\"version\"],\"socket\":{{\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}}}"
        );
        return;
    }
    println!(
        "bowline daemon\n\nCommands:\n  bowline-daemon serve [--sync-root <path> --sync-state-root <path>] [--notify-approvals]\n  bowline-daemon sync-once --root <path> --state-root <path>\n  bowline-daemon stop\n  bowline-daemon status\n  bowline-daemon version\n\nGlobal options:\n  --json\n  --socket <path>"
    );
}

pub(super) fn print_sync_once(args: SyncOnceArgs, json: bool) -> ExitCode {
    match run_sync_once(args) {
        Ok(summary) => {
            if json {
                let scan_json = serde_json::to_string(&summary.scan).unwrap_or_else(|_| {
                    "{\"mode\":\"full\",\"fullReason\":\"cli-requested\",\"filesHashed\":0,\"statHits\":0,\"futureMtimePaths\":0,\"divergenceCount\":0,\"rehashReasons\":[]}".to_string()
                });
                println!(
                    "{{\"ok\":true,\"command\":\"sync-once\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"snapshotRootManifestId\":{},\"manifestObjectKey\":{},\"namespaceRootId\":{},\"stale\":{},\"merged\":{},\"conflictCount\":{},\"scan\":{}}}",
                    json_string(&summary.workspace_id),
                    json_string(&summary.snapshot_id),
                    summary.version,
                    json_string(summary.snapshot_root_manifest_id_label()),
                    json_string(summary.manifest_object_key_label()),
                    json_string(
                        summary
                            .namespace_root_id
                            .as_deref()
                            .unwrap_or("not-applicable")
                    ),
                    summary.stale(),
                    summary.merged(),
                    summary.conflict_count,
                    scan_json,
                );
            } else {
                println!(
                    "sync-once: workspace {} at snapshot {} (version {}, manifest {}, stale: {}, merged: {}, conflicts: {})",
                    summary.workspace_id,
                    summary.snapshot_id,
                    summary.version,
                    summary.snapshot_root_manifest_id_label(),
                    summary.stale(),
                    summary.merged(),
                    summary.conflict_count
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if json {
                println!(
                    "{{\"ok\":false,\"command\":\"sync-once\",\"status\":\"error\",\"error\":{{\"code\":\"sync_once_failed\",\"message\":{}}}}}",
                    json_string(&error.to_string())
                );
            } else {
                eprintln!("bowline-daemon sync-once failed: {error}");
            }
            ExitCode::from(EXIT_FAILURE)
        }
    }
}
pub(super) fn print_status(socket: &Path, json: bool) {
    match status_snapshot(socket) {
        Ok(status) => {
            if json {
                println!(
                    "{}",
                    daemon_json(&serde_json::json!({
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
                    }))
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
