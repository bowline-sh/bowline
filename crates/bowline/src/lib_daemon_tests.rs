use crate::runtime;

use super::{Command, DaemonCommand, WorkspaceSelection, parse_args, redact_setup_text};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[test]
fn daemon_start_reuses_only_usable_workspace_daemon() {
    let idle = super::Handshake {
        daemon_version: "test".to_string(),
        status_json:
            r#"{"workspaceId":"ws_code","status":{"level":"healthy","attentionItems":[]}}"#
                .to_string(),
    };
    let limited = super::Handshake {
        daemon_version: "test".to_string(),
        status_json: r#"{"workspaceId":"ws_code","status":{"level":"limited","attentionItems":["missing token"]}}"#.to_string(),
    };
    let degraded = super::Handshake {
        daemon_version: "test".to_string(),
        status_json:
            r#"{"workspaceId":"ws_code","status":{"level":"attention","attentionItems":[]}}"#
                .to_string(),
    };

    assert_eq!(
        super::handshake_start_status(&idle, "ws_code"),
        super::DaemonStartHandshakeStatus::Ready
    );
    assert_eq!(
        super::handshake_start_status(&idle, "ws_other"),
        super::DaemonStartHandshakeStatus::WorkspaceMismatch
    );
    assert_eq!(
        super::handshake_start_status(&limited, "ws_code"),
        super::DaemonStartHandshakeStatus::Degraded {
            state: bowline_core::commands::DaemonSyncState::Limited,
            reason: "missing token".to_string()
        }
    );
    assert_eq!(
        super::handshake_start_status(&degraded, "ws_code"),
        super::DaemonStartHandshakeStatus::Degraded {
            state: bowline_core::commands::DaemonSyncState::Degraded,
            reason: "sync state is degraded".to_string()
        }
    );
    assert_eq!(
        super::handshake_start_status(
            &super::Handshake {
                daemon_version: "test".to_string(),
                status_json: "{}".to_string(),
            },
            "ws_code"
        ),
        super::DaemonStartHandshakeStatus::Degraded {
            state: bowline_core::commands::DaemonSyncState::Unclassified,
            reason: "workspace status is unavailable".to_string()
        }
    );
}

fn assert_daemon_start_does_not_shutdown_degraded_daemon() {
    let temp = tempfile_dir("bowline-degraded-daemon-start");
    let socket = temp.join("daemon.sock");
    let workspace_id = super::daemon_workspace_id_for_start()
        .unwrap_or_else(|_| runtime::active_workspace_id())
        .as_str()
        .to_string();
    let ready = Arc::new(AtomicBool::new(false));
    let shutdown_seen = Arc::new(AtomicBool::new(false));
    let thread_ready = Arc::clone(&ready);
    let thread_shutdown_seen = Arc::clone(&shutdown_seen);
    let thread_socket = socket.clone();

    let handle = std::thread::spawn(move || {
        let listener = std::os::unix::net::UnixListener::bind(&thread_socket)
            .expect("bind fake daemon socket");
        listener
            .set_nonblocking(true)
            .expect("set fake daemon nonblocking");
        thread_ready.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let mut hello_seen = false;
        while started.elapsed() < Duration::from_secs(2) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("make accepted daemon socket blocking");
                    use bowline_core::wire::generated::{
                        DaemonClientHello, DaemonRpcRequest, DaemonRpcResponse, DaemonServerHello,
                        MACHINE_CONTRACT_VERSION,
                    };
                    let codec = bowline_daemon_rpc::FrameCodec::default();
                    codec
                        .read_magic(&mut stream)
                        .expect("read daemon RPC magic");
                    let _: DaemonClientHello = codec.read(&mut stream).expect("read client hello");
                    codec
                        .write(
                            &mut stream,
                            &DaemonServerHello {
                                protocol_version: bowline_daemon_rpc::DAEMON_RPC_PROTOCOL_VERSION,
                                contract_version: MACHINE_CONTRACT_VERSION,
                                schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH
                                    .to_string(),
                                daemon_version: "test".to_string(),
                                capabilities: vec![
                                    "daemon.info".to_string(),
                                    "daemon.shutdown".to_string(),
                                    "status.snapshot".to_string(),
                                ],
                                instance_id: "fake-daemon".to_string(),
                            },
                        )
                        .expect("write server hello");
                    let request: DaemonRpcRequest =
                        codec.read(&mut stream).expect("read daemon request");
                    if request.method == "daemon.shutdown" {
                        thread_shutdown_seen.store(true, Ordering::SeqCst);
                        codec
                            .write(
                                &mut stream,
                                &DaemonRpcResponse {
                                    request_id: request.request_id,
                                    result: Some(serde_json::json!({"status": "stopping"})),
                                    error: None,
                                },
                            )
                            .expect("write shutdown response");
                    } else {
                        assert_eq!(request.method, "daemon.info");
                        hello_seen = true;
                        let result = serde_json::json!({
                            "daemonVersion": "test",
                        });
                        codec
                            .write(
                                &mut stream,
                                &DaemonRpcResponse {
                                    request_id: request.request_id,
                                    result: Some(result),
                                    error: None,
                                },
                            )
                            .expect("write daemon info response");
                        let status_request: DaemonRpcRequest =
                            codec.read(&mut stream).expect("read status request");
                        assert_eq!(status_request.method, "status.getSnapshot");
                        let mut snapshot: serde_json::Value = serde_json::from_str(include_str!(
                            "../../../tests/contracts/status/limited.json"
                        ))
                        .expect("shared status fixture");
                        snapshot["workspaceId"] = serde_json::Value::String(workspace_id.clone());
                        codec
                            .write(
                                &mut stream,
                                &DaemonRpcResponse {
                                    request_id: status_request.request_id,
                                    result: Some(serde_json::json!({
                                        "instanceId": "fake-daemon",
                                        "sequence": 1,
                                        "snapshot": snapshot,
                                    })),
                                    error: None,
                                },
                            )
                            .expect("write status snapshot response");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if hello_seen && started.elapsed() > Duration::from_millis(250) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fake daemon accept failed: {error}"),
            }
        }
    });

    let wait_started = Instant::now();
    while (!ready.load(Ordering::SeqCst) || !socket.exists())
        && wait_started.elapsed() < Duration::from_secs(3)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.load(Ordering::SeqCst));
    assert!(socket.exists());

    let code = super::print_daemon_start(&socket, false);

    handle.join().expect("fake daemon thread");
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert!(!shutdown_seen.load(Ordering::SeqCst));
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_start_removes_socket_only_after_connection_refused() {
    assert_daemon_start_does_not_shutdown_degraded_daemon();

    let temp = tempfile_dir("bowline-stale-daemon-socket");
    let socket = temp.join("daemon.sock");
    {
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
    }

    assert!(socket.exists());
    super::remove_stale_daemon_socket_after_connect_error(
        &socket,
        &std::io::Error::from(std::io::ErrorKind::TimedOut),
    )
    .expect("non-refused errors do not mutate the socket");
    assert!(socket.exists());
    super::remove_stale_daemon_socket_after_connect_error(
        &socket,
        &std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
    )
    .expect("refused stale socket is removable");
    assert!(!socket.exists());

    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_takeover_tolerates_missing_and_stale_daemon_sockets() {
    let temp = tempfile_dir("bowline-service-takeover");
    let socket = temp.join("daemon.sock");

    super::stop_unmanaged_daemon(&socket).expect("missing daemon is already stopped");

    {
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind stale socket");
    }
    assert!(socket.exists());

    super::stop_unmanaged_daemon(&socket).expect("stale socket is removed");

    assert!(!socket.exists());
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_reinstall_stops_supervisor_before_socket_takeover() {
    let temp = tempfile_dir("bowline-service-reinstall");
    let socket = temp.join("daemon.sock");
    let listener = std::cell::RefCell::new(Some(
        std::os::unix::net::UnixListener::bind(&socket).expect("bind managed socket"),
    ));
    let restarted = std::cell::Cell::new(false);

    let outcome = super::install_daemon_service_with_takeover(
        &socket,
        true,
        || {
            drop(listener.borrow_mut().take());
            std::fs::remove_file(&socket).map_err(|error| error.to_string())
        },
        || Ok("installed"),
        || {
            restarted.set(true);
            Ok(())
        },
    )
    .expect("the supervisor stops its managed daemon before takeover");

    assert_eq!(outcome, "installed");
    assert!(!socket.exists());
    assert!(!restarted.get());
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_reinstall_restores_active_service_after_takeover_failure() {
    let temp = tempfile_dir("bowline-service-restore-after-takeover");
    let socket = temp.join("daemon.sock");
    std::fs::write(&socket, b"unsafe target").expect("unsafe target");
    let stopped = std::cell::Cell::new(false);
    let restarted = std::cell::Cell::new(false);

    let error = super::install_daemon_service_with_takeover(
        &socket,
        true,
        || {
            stopped.set(true);
            Ok(())
        },
        || Ok("must not install"),
        || {
            restarted.set(true);
            Ok(())
        },
    )
    .expect_err("unsafe target blocks takeover");

    assert!(stopped.get());
    assert!(restarted.get());
    assert!(!error.contains("could not restore"));
    assert_eq!(
        std::fs::read(&socket).expect("target remains"),
        b"unsafe target"
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_reinstall_restores_active_service_after_install_failure() {
    let temp = tempfile_dir("bw-reinstall-install");
    let socket = temp.join("daemon.sock");
    let restarted = std::cell::Cell::new(false);

    let error = super::install_daemon_service_with_takeover(
        &socket,
        true,
        || Ok(()),
        || Err::<(), _>("install failed".to_string()),
        || {
            restarted.set(true);
            Ok(())
        },
    )
    .expect_err("failed install restores prior service");

    assert_eq!(error, "install failed");
    assert!(restarted.get());
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_reinstall_restores_active_service_after_stop_failure() {
    let temp = tempfile_dir("bw-reinstall-stop");
    let socket = temp.join("daemon.sock");
    let restarted = std::cell::Cell::new(false);

    let error = super::install_daemon_service_with_takeover(
        &socket,
        true,
        || Err("stop failed after mutation".to_string()),
        || Ok("must not install"),
        || {
            restarted.set(true);
            Ok(())
        },
    )
    .expect_err("failed stop restores prior service");

    assert_eq!(error, "stop failed after mutation");
    assert!(restarted.get());
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_install_refuses_uncertain_supervisor_state() {
    let status = super::DaemonServiceStatus {
        state: "unavailable".to_string(),
        unit_path: PathBuf::from("bowline.service"),
        unavailable_because: Some("systemd user manager is unavailable".to_string()),
    };

    let error = super::daemon_service_active_from_status(Some(status))
        .expect_err("uncertain supervisor ownership blocks mutation");

    assert_eq!(error, "systemd user manager is unavailable");
}

#[test]
fn managed_service_install_allows_linux_repair_with_missing_definition() {
    let temp = tempfile_dir("bowline-service-missing-definition");

    let definition = super::previous_active_service_definition(true, &temp.join("missing.service"))
        .expect("missing definition is repairable");

    assert!(definition.is_none());
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_takeover_rejects_an_unsafe_socket_path() {
    let temp = tempfile_dir("bowline-service-takeover-unsafe");
    let socket = temp.join("daemon.sock");
    std::fs::write(&socket, b"not a socket").expect("write unsafe path");

    let _error = super::stop_unmanaged_daemon(&socket)
        .expect_err("a non-socket path must not be silently replaced");

    assert_eq!(
        std::fs::read(&socket).expect("unsafe path remains"),
        b"not a socket"
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn managed_service_takeover_requires_stable_socket_absence() {
    let temp = tempfile_dir("bowline-service-takeover-race");
    let socket = temp.join("daemon.sock");
    let racing_socket = socket.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(racing_socket, b"late owner").expect("create racing socket path");
    });

    let error = super::wait_for_stable_socket_absence(
        &socket,
        Duration::from_millis(100),
        Duration::from_millis(10),
    )
    .expect_err("a path appearing during the stable-absence window blocks takeover");

    writer.join().expect("racing writer");
    assert!(error.contains("cannot be safely replaced"));
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_stop_waits_for_socket_ownership_to_be_released() {
    let temp = tempfile_dir("bowline-daemon-stop-wait");
    let socket = temp.join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    assert!(
        !super::wait_for_daemon_socket_to_stop(&socket, Duration::from_millis(20)),
        "a failed or absent handshake is insufficient while the socket path remains owned"
    );

    drop(listener);
    std::fs::remove_file(&socket).expect("release socket path");

    assert!(super::wait_for_daemon_socket_to_stop(
        &socket,
        Duration::from_millis(20)
    ));
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn parses_daemon_status_socket() {
    let cli = parse_args([
        "daemon",
        "status",
        "--socket",
        "/tmp/bowline-test.sock",
        "--json",
    ]);

    assert!(cli.json);
    assert_eq!(cli.socket, PathBuf::from("/tmp/bowline-test.sock"));
    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Daemon(DaemonCommand::Status)
    );
}

#[test]
fn parses_daemon_service_lifecycle_commands() {
    assert_eq!(
        parse_args(["daemon", "install"])
            .command
            .expect("parsed command"),
        Command::Daemon(DaemonCommand::Install)
    );
    assert_eq!(
        parse_args(["daemon", "restart"])
            .command
            .expect("parsed command"),
        Command::Daemon(DaemonCommand::Restart)
    );
    assert_eq!(
        parse_args(["daemon", "uninstall"])
            .command
            .expect("parsed command"),
        Command::Daemon(DaemonCommand::Uninstall)
    );
}

#[test]
fn parses_diagnostics_collect() {
    assert_eq!(
        parse_args(["diagnostics", "collect", "--root", "~/Code"])
            .command
            .expect("parsed command"),
        Command::DiagnosticsCollect(WorkspaceSelection {
            root: "~/Code".to_string(),
            project: None,
        })
    );
    assert!(parse_args(["diagnostics"]).command.is_err());
}

#[test]
fn diagnostics_redaction_removes_home_paths_and_tokens() {
    let home_db = ["", "home", "user", ".bowline", "local.sqlite3"].join("/");
    let token = ["SECRET", "1234567890abcdef"].join("_");
    let redacted = redact_setup_text(&format!(
        "metadata_db={home_db} TOKEN_VALUE={token} project_file_contents=excluded"
    ));

    assert!(
        redacted
            .text
            .contains("metadata_db=~/.bowline/local.sqlite3")
    );
    assert!(redacted.text.contains("[redacted]"));
    assert!(!redacted.text.contains(&home_db));
    assert!(!redacted.text.contains(&token));
}

#[test]
fn diagnostics_bundle_includes_requested_workspace_selection() {
    let bundle = crate::daemon::diagnostics_bundle_text(
        std::path::Path::new("/tmp/bowline.sock"),
        "2026-06-30T00:00:00Z",
        &WorkspaceSelection {
            root: "/tmp/Custom Code".to_string(),
            project: Some("apps/web".to_string()),
        },
    );

    assert!(bundle.contains("requested_root=/tmp/Custom Code"));
    assert!(bundle.contains("requested_project=apps/web"));
}

#[test]
fn daemon_service_launch_config_refuses_before_setup_without_mutating_metadata() {
    let temp = tempfile_dir("bowline-daemon-service-default");
    let db_path = temp.join("state").join("local.sqlite3");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
    let daemon = temp.join("bowline-daemon");

    let error = match super::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &db_path,
        &store,
        daemon.clone(),
    ) {
        Ok(_) => panic!("service launch config should require authenticated setup"),
        Err(error) => error,
    };

    assert!(error.contains("run `bowline setup --root <path>` first"));
    assert!(
        store
            .current_workspace()
            .expect("current workspace")
            .is_none()
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_service_launch_config_uses_authenticated_accepted_root() {
    let temp = tempfile_dir("bowline-daemon-service-authenticated");
    let state = temp.join("state");
    let db_path = state.join("local.sqlite3");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_code_account");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_account",
            &workspace_id,
            "~/Projects/Bowline",
            "2026-07-15T12:00:00Z",
        )
        .expect("root");
    std::fs::write(
        state.join("daemon.env"),
        "BOWLINE_WORKSPACE_ID=ws_code_account\nBOWLINE_DEVICE_ID=device_fixture\n",
    )
    .expect("persisted daemon identity");
    let daemon = temp.join("bowline-daemon");

    let launch = super::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &db_path,
        &store,
        daemon.clone(),
    )
    .expect("authenticated service launch config");

    assert_eq!(launch.workspace_id, workspace_id);
    assert_eq!(launch.root, super::expand_home_path("~/Projects/Bowline"));
    assert_eq!(launch.daemon, daemon);
    assert_eq!(
        launch.state_root,
        std::fs::canonicalize(state).expect("canonical state root")
    );
    assert_eq!(launch.device_id.as_str(), "device_fixture");
    let _ = std::fs::remove_dir_all(temp);
}

#[cfg(unix)]
#[test]
fn daemon_service_launch_config_follows_the_workspace_database_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile_dir("bowline-daemon-service-symlink");
    let default_state = temp.join("default");
    let workspace_state = temp.join("workspace");
    std::fs::create_dir_all(&default_state).expect("default state");
    std::fs::create_dir_all(&workspace_state).expect("workspace state");
    let workspace_db = workspace_state.join("local.sqlite3");
    let store =
        bowline_local::metadata::MetadataStore::open(&workspace_db).expect("metadata store");
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_code_account");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_account",
            &workspace_id,
            "~/Code",
            "2026-07-15T12:00:00Z",
        )
        .expect("root");
    std::fs::write(
        workspace_state.join("daemon.env"),
        "BOWLINE_WORKSPACE_ID=ws_code_account\nBOWLINE_DEVICE_ID=device_remote\nBOWLINE_ACCOUNT_SESSION_ID=session_remote\n",
    )
    .expect("daemon env");
    let default_db = default_state.join("local.sqlite3");
    symlink(&workspace_db, &default_db).expect("default database symlink");

    let launch = super::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &default_db,
        &store,
        temp.join("bowline-daemon"),
    )
    .expect("service launch config");

    assert_eq!(
        launch.state_root,
        std::fs::canonicalize(&workspace_state).expect("canonical workspace state")
    );
    assert_eq!(launch.device_id.as_str(), "device_remote");
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_launch_uses_persisted_device_id() {
    let temp = tempfile_dir("bowline-daemon-persisted-device");
    let state = temp.join("state");
    let db_path = state.join("local.sqlite3");
    std::fs::create_dir_all(&state).expect("state dir");
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_code_account");
    std::fs::write(
            state.join("daemon.env"),
            format!(
                "BOWLINE_WORKSPACE_ID={}\nBOWLINE_DEVICE_ID=device_remote_box\nBOWLINE_WORKOS_REFRESH_TOKEN=stale-refresh\n",
                workspace_id.as_str()
            ),
        )
        .expect("daemon env");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_account",
            &workspace_id,
            "~/Code",
            "2026-07-15T12:00:00Z",
        )
        .expect("root");
    let daemon = temp.join("bowline-daemon");

    let launch = super::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &db_path,
        &store,
        daemon,
    )
    .expect("service launch config");

    assert_eq!(launch.device_id.as_str(), "device_remote_box");
    assert_eq!(
        super::persisted_daemon_env_value(&state, "BOWLINE_WORKOS_REFRESH_TOKEN"),
        None
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn persisted_daemon_device_id_is_workspace_bound() {
    let temp = tempfile_dir("bowline-daemon-persisted-device-workspace");
    let state = temp.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("daemon.env"),
        "BOWLINE_WORKSPACE_ID=ws_a\nBOWLINE_DEVICE_ID=device_a\n",
    )
    .expect("daemon env");

    assert_eq!(
        super::persisted_daemon_device_id_for_workspace(
            &state,
            &bowline_core::ids::WorkspaceId::new("ws_a")
        )
        .as_deref(),
        Some("device_a")
    );
    assert_eq!(
        super::persisted_daemon_device_id_for_workspace(
            &state,
            &bowline_core::ids::WorkspaceId::new("ws_b")
        ),
        None
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn persisted_daemon_env_excludes_refresh_tokens() {
    let temp = tempfile_dir("bowline-daemon-env-sanitized");
    std::fs::write(
            temp.join("daemon.env"),
            "BOWLINE_ACCOUNT_SESSION_ID=session\nBOWLINE_WORKOS_ACCESS_TOKEN=access\nBOWLINE_WORKOS_REFRESH_TOKEN=refresh\nBOWLINE_DEVICE_ID=device_remote\n",
        )
        .expect("daemon env");

    let env = super::persisted_daemon_env(&temp);

    assert!(env.contains(&(
        "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
        "session".to_string()
    )));
    assert!(env.contains(&(
        "BOWLINE_WORKOS_ACCESS_TOKEN".to_string(),
        "access".to_string()
    )));
    assert!(env.contains(&("BOWLINE_DEVICE_ID".to_string(), "device_remote".to_string())));
    assert!(
        !env.iter()
            .any(|(key, _)| key == "BOWLINE_WORKOS_REFRESH_TOKEN")
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_binary_path_requires_sibling_daemon() {
    let temp = tempfile_dir("bowline-daemon-missing");
    let error = super::daemon_binary_path_next_to(&temp.join("bowline"))
        .expect_err("missing daemon binary");

    assert!(error.contains("bowline-daemon binary is unavailable"));
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_binary_path_accepts_executable_sibling() {
    let temp = tempfile_dir("bowline-daemon-present");
    let daemon = temp.join(if cfg!(windows) {
        "bowline-daemon.exe"
    } else {
        "bowline-daemon"
    });
    std::fs::write(&daemon, b"daemon").expect("daemon file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&daemon)
            .expect("daemon metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&daemon, permissions).expect("daemon permissions");
    }

    assert_eq!(
        super::daemon_binary_path_next_to(&temp.join("bowline")).expect("daemon path"),
        daemon
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_binary_path_accepts_target_debug_fallback() {
    let temp = tempfile_dir("bowline-daemon-target-debug");
    let deps = temp.join("target").join("debug").join("deps");
    std::fs::create_dir_all(&deps).expect("debug deps dir");
    let daemon = temp.join("target").join("debug").join(if cfg!(windows) {
        "bowline-daemon.exe"
    } else {
        "bowline-daemon"
    });
    std::fs::write(&daemon, b"daemon").expect("daemon file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&daemon)
            .expect("daemon metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&daemon, permissions).expect("daemon permissions");
    }

    assert_eq!(
        super::daemon_binary_path_next_to(&deps.join("bowline")).expect("daemon path"),
        daemon
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_service_status_json_includes_unavailable_reason() {
    let status = super::DaemonServiceStatus {
        state: "unavailable".to_string(),
        unit_path: PathBuf::from("/tmp/bowline.service"),
        unavailable_because: Some("systemd user manager is unavailable".to_string()),
    };

    assert_eq!(
        super::daemon_service_status_json(&status),
        "{\"state\":\"unavailable\",\"unitPath\":\"/tmp/bowline.service\",\"unavailableBecause\":\"systemd user manager is unavailable\"}"
    );
}

#[test]
fn daemon_status_json_keeps_service_top_level() {
    let service = super::DaemonServiceStatus {
        state: "failed".to_string(),
        unit_path: PathBuf::from("/tmp/bowline.service"),
        unavailable_because: None,
    };

    let running: serde_json::Value = serde_json::from_str(&super::daemon_status_json(
        Path::new("/tmp/bowline.sock"),
        super::DaemonProcessState::Running,
        Some("daemon-test"),
        Some("{\"state\":\"ready\"}"),
        Some(&service),
    ))
    .expect("running status json");
    let stopped: serde_json::Value = serde_json::from_str(&super::daemon_status_json(
        Path::new("/tmp/bowline.sock"),
        super::DaemonProcessState::Stopped,
        None,
        None,
        Some(&service),
    ))
    .expect("stopped status json");

    assert_eq!(running["daemon"]["state"], "running");
    assert_eq!(running["service"]["state"], "failed");
    assert!(running["daemon"]["service"].is_null());
    assert_eq!(stopped["daemon"]["state"], "stopped");
    assert_eq!(stopped["service"]["state"], "failed");
    assert!(stopped["daemon"]["service"].is_null());
}

fn tempfile_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}
