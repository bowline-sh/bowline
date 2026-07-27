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
    use bowline_core::commands::DaemonSyncState;
    use bowline_core::status::StatusLevel;
    let workspace = bowline_core::ids::WorkspaceId::new("ws_code");
    let idle = handshake_with(&workspace, StatusLevel::Healthy, &[]);
    let limited = handshake_with(&workspace, StatusLevel::Limited, &["missing token"]);
    let degraded = handshake_with(&workspace, StatusLevel::Attention, &[]);

    assert_eq!(
        super::handshake_start_status(&idle, &workspace),
        super::DaemonStartHandshakeStatus::Ready
    );
    assert_eq!(
        super::handshake_start_status(&idle, &bowline_core::ids::WorkspaceId::new("ws_other")),
        super::DaemonStartHandshakeStatus::WorkspaceMismatch
    );
    assert_eq!(
        super::handshake_start_status(&limited, &workspace),
        super::DaemonStartHandshakeStatus::Degraded {
            state: DaemonSyncState::Limited,
            reason: "missing token".to_string()
        }
    );
    assert_eq!(
        super::handshake_start_status(&degraded, &workspace),
        super::DaemonStartHandshakeStatus::Degraded {
            state: DaemonSyncState::Degraded,
            reason: "sync state is degraded".to_string()
        }
    );
}

fn handshake_with(
    workspace_id: &bowline_core::ids::WorkspaceId,
    level: bowline_core::status::StatusLevel,
    attention_items: &[&str],
) -> super::Handshake {
    let mut status = crate::status_commands::tests::status_output();
    status.workspace_id = workspace_id.clone();
    status.status = bowline_core::status::WorkspaceStatus {
        level,
        attention_items: attention_items
            .iter()
            .map(|item| item.to_string())
            .collect(),
    };
    super::Handshake {
        daemon_version: "test".to_string(),
        status,
    }
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
        &transport_error(std::io::ErrorKind::TimedOut),
    )
    .expect("non-refused errors do not mutate the socket");
    assert!(socket.exists());
    super::remove_stale_daemon_socket_after_connect_error(
        &socket,
        &transport_error(std::io::ErrorKind::ConnectionRefused),
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
        state: super::ServiceSupervisorState::Unavailable,
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
fn daemon_service_status_json_includes_unavailable_reason() {
    let status = super::DaemonServiceStatus {
        state: super::ServiceSupervisorState::Unavailable,
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
        state: super::ServiceSupervisorState::Unrecognized("failed".to_string()),
        unit_path: PathBuf::from("/tmp/bowline.service"),
        unavailable_because: None,
    };
    let snapshot = crate::status_commands::tests::status_output();

    let running: serde_json::Value =
        serde_json::from_str(&super::daemon_status_json(super::DaemonStatusReport {
            socket: Path::new("/tmp/bowline.sock"),
            state: super::DaemonProcessState::Running,
            daemon_version: Some("daemon-test"),
            sync: Some(&snapshot),
            unavailable_because: None,
            service: Some(&service),
        }))
        .expect("running status json");
    let stopped: serde_json::Value =
        serde_json::from_str(&super::daemon_status_json(super::DaemonStatusReport {
            socket: Path::new("/tmp/bowline.sock"),
            state: super::DaemonProcessState::Stopped,
            daemon_version: None,
            sync: None,
            unavailable_because: None,
            service: Some(&service),
        }))
        .expect("stopped status json");

    assert_eq!(running["daemon"]["state"], "running");
    // Read the id from the shared fixture rather than restating it, so the
    // assertion cannot drift when the fixture changes.
    assert_eq!(
        running["sync"]["workspaceId"],
        snapshot.workspace_id.as_str()
    );
    assert_eq!(running["service"]["state"], "failed");
    assert!(running["daemon"]["service"].is_null());
    assert_eq!(stopped["daemon"]["state"], "stopped");
    assert!(stopped["sync"].is_null());
    assert_eq!(stopped["service"]["state"], "failed");
    assert!(stopped["daemon"]["service"].is_null());
}

/// A version-skewed daemon owns the control socket. `daemon status` must report
/// it as running with the mismatch, never as stopped.
#[test]
fn daemon_status_json_reports_a_skewed_daemon_as_running() {
    let report: serde_json::Value =
        serde_json::from_str(&super::daemon_status_json(super::DaemonStatusReport {
            socket: Path::new("/tmp/bowline.sock"),
            state: super::DaemonProcessState::Running,
            daemon_version: None,
            sync: None,
            unavailable_because: Some("machine contract version 6".to_string()),
            service: None,
        }))
        .expect("skewed status json");

    assert_eq!(report["daemon"]["state"], "running");
    assert_eq!(
        report["daemon"]["unavailableBecause"],
        "machine contract version 6"
    );
}

fn transport_error(kind: std::io::ErrorKind) -> crate::wire::DaemonRpcError {
    crate::wire::DaemonRpcError::from(bowline_daemon_rpc::ClientError::Io {
        operation: "connect",
        source: std::io::Error::from(kind),
    })
}

fn tempfile_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

mod launch_config_tests;
