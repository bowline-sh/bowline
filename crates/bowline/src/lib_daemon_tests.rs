use crate::runtime;

use super::{
    ActionsArgs, Command, DEFAULT_AGENT_HYDRATE_BUDGET_BYTES, DaemonCommand, StatusArgs, TuiArgs,
    agent, approve_no_pending_exit_code, bootstrap::BootstrapSshArgs, devices::DevicesArgs, login,
    parse_args, recovery::RecoveryArgs, redact_setup_text, resolve,
};
use bowline_core::commands::{AgentLeaseBase, CommandName};
use std::path::{Path, PathBuf};

#[test]
fn daemon_start_reuses_only_usable_workspace_daemon() {
    let idle = super::Handshake {
        daemon_version: "test".to_string(),
        sync_json: Some(
            r#"{"state":"idle","workspaceId":"ws_code","snapshotId":"snap_1","version":1}"#
                .to_string(),
        ),
    };
    let limited = super::Handshake {
        daemon_version: "test".to_string(),
        sync_json: Some(
            r#"{"state":"limited","workspaceId":"ws_code","unavailableBecause":"missing token"}"#
                .to_string(),
        ),
    };
    let degraded = super::Handshake {
        daemon_version: "test".to_string(),
        sync_json: Some(r#"{"state":"degraded","workspaceId":"ws_code"}"#.to_string()),
    };

    assert!(super::handshake_sync_workspace_ready_for_start(
        &idle, "ws_code"
    ));
    assert!(!super::handshake_sync_workspace_ready_for_start(
        &idle, "ws_other"
    ));
    assert!(!super::handshake_sync_workspace_ready_for_start(
        &limited, "ws_code"
    ));
    assert!(!super::handshake_sync_workspace_ready_for_start(
        &degraded, "ws_code"
    ));
}

#[test]
fn daemon_start_removes_socket_only_after_connection_refused() {
    let temp = tempfile_dir("bowline-stale-daemon-socket");
    let socket = temp.join("daemon.sock");
    {
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
    }

    assert!(socket.exists());
    super::remove_stale_daemon_socket_after_connect_error(
        &socket,
        &std::io::Error::from(std::io::ErrorKind::TimedOut),
    );
    assert!(socket.exists());
    super::remove_stale_daemon_socket_after_connect_error(
        &socket,
        &std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
    );
    assert!(!socket.exists());

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
    assert_eq!(cli.command, Command::Daemon(DaemonCommand::Status));
}

#[test]
fn parses_daemon_service_lifecycle_commands() {
    assert_eq!(
        parse_args(["daemon", "install"]).command,
        Command::Daemon(DaemonCommand::Install)
    );
    assert_eq!(
        parse_args(["daemon", "restart"]).command,
        Command::Daemon(DaemonCommand::Restart)
    );
    assert_eq!(
        parse_args(["daemon", "uninstall"]).command,
        Command::Daemon(DaemonCommand::Uninstall)
    );
}

#[test]
fn parses_diagnostics_collect() {
    assert_eq!(
        parse_args(["diagnostics", "collect"]).command,
        Command::DiagnosticsCollect
    );
    assert!(matches!(
        parse_args(["diagnostics"]).command,
        Command::UsageError { .. }
    ));
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
fn daemon_service_launch_config_defaults_before_login() {
    let temp = tempfile_dir("bowline-daemon-service-default");
    let db_path = temp.join("state").join("local.sqlite3");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
    let daemon = temp.join("bowline-daemon");

    let launch = super::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &db_path,
        &store,
        daemon.clone(),
    )
    .expect("service launch config");

    assert!(!launch.workspace_id.as_str().is_empty());
    assert_eq!(launch.daemon, daemon);
    assert_eq!(launch.root, super::default_workspace_root());
    assert_eq!(launch.state_root, temp.join("state"));
    assert_eq!(
        store
            .accepted_roots(&launch.workspace_id)
            .expect("accepted roots"),
        vec!["~/Code".to_string()]
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_launch_uses_persisted_device_id() {
    let temp = tempfile_dir("bowline-daemon-persisted-device");
    let state = temp.join("state");
    let db_path = state.join("local.sqlite3");
    std::fs::create_dir_all(&state).expect("state dir");
    let workspace_id = runtime::active_workspace_id();
    std::fs::write(
            state.join("daemon.env"),
            format!(
                "BOWLINE_WORKSPACE_ID={}\nBOWLINE_DEVICE_ID=device_remote_box\nBOWLINE_WORKOS_REFRESH_TOKEN=stale-refresh\n",
                workspace_id.as_str()
            ),
        )
        .expect("daemon env");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
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
        "running",
        Some("daemon-test"),
        Some("{\"state\":\"ready\"}"),
        Some(&service),
    ))
    .expect("running status json");
    let stopped: serde_json::Value = serde_json::from_str(&super::daemon_status_json(
        Path::new("/tmp/bowline.sock"),
        "stopped",
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
