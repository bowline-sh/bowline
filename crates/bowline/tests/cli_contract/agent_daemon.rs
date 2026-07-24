use super::*;
use bowline_core::wire::generated::{
    DaemonClientHello, DaemonRpcRequest, DaemonRpcResponse, DaemonServerHello,
    MACHINE_CONTRACT_VERSION,
};
use bowline_daemon_rpc::{DAEMON_RPC_PROTOCOL, DAEMON_RPC_PROTOCOL_VERSION, FrameCodec};
use std::os::unix::net::UnixStream;

fn spawn_daemon_info_server(
    listener: UnixListener,
    status: Option<Value>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Harness accept-wait only, not a responsiveness assertion: under a
        // fully loaded gate run, spawning the freshly linked CLI binary can
        // stall well past 10s and the fake server must not give up first.
        // Solo the connect takes ~0.2s; every functional assertion is below.
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut stream =
            accept_test_client(&listener, deadline).expect("RPC client should connect");
        let codec = FrameCodec::default();
        codec
            .read_magic(&mut stream)
            .expect("RPC magic should read");
        let _: DaemonClientHello = codec.read(&mut stream).expect("client hello should read");
        codec
            .write(
                &mut stream,
                &DaemonServerHello {
                    protocol_version: DAEMON_RPC_PROTOCOL_VERSION,
                    contract_version: MACHINE_CONTRACT_VERSION,
                    schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH.to_string(),
                    daemon_version: "test-daemon".to_string(),
                    capabilities: vec!["daemon.info".to_string(), "status.snapshot".to_string()],
                    instance_id: "test-daemon-instance".to_string(),
                },
            )
            .expect("server hello should write");
        let request: DaemonRpcRequest = codec.read(&mut stream).expect("RPC request should read");
        assert_eq!(request.method, "daemon.info");
        codec
            .write(
                &mut stream,
                &DaemonRpcResponse {
                    request_id: request.request_id,
                    result: Some(serde_json::json!({
                        "daemonVersion": "test-daemon",
                    })),
                    error: None,
                },
            )
            .expect("daemon info response should write");
        let status_request: DaemonRpcRequest =
            codec.read(&mut stream).expect("status request should read");
        assert_eq!(status_request.method, "status.getSnapshot");
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("tests/contracts/status/healthy.json");
        let mut snapshot: Value = serde_json::from_str(
            &fs::read_to_string(fixture_path).expect("healthy status fixture"),
        )
        .expect("typed healthy status fixture");
        if let Some(Value::Object(overrides)) = status {
            let object = snapshot
                .as_object_mut()
                .expect("healthy status fixture object");
            object.extend(overrides);
        }
        codec
            .write(
                &mut stream,
                &DaemonRpcResponse {
                    request_id: status_request.request_id,
                    result: Some(serde_json::json!({
                        "instanceId": "test-daemon-instance",
                        "sequence": 1,
                        "snapshot": snapshot,
                    })),
                    error: None,
                },
            )
            .expect("status response should write");
    })
}

fn accept_test_client(listener: &UnixListener, deadline: Instant) -> io::Result<UnixStream> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("accepted stream should become blocking");
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for CLI handshake",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn events_limit_rejects_unbounded_requests() {
    let output = run_bowline(&["events", "--root", "~/Code", "--limit", "999999", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"], "usage-error");
    assert_eq!(
        json["error"]["message"],
        "expected --limit between 1 and 500"
    );
}

#[test]
fn trust_commands_fail_without_control_plane_config_instead_of_using_ephemeral_fake() {
    let temp = TempWorkspace::new("trust-missing-control-plane").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-26T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-26T12:00:00Z")
        .expect("root insert");
    let output = bowline()
        .args(["device", "list", "--root", "~/Code", "--json"])
        .current_dir(temp.root())
        .env("BOWLINE_METADATA_DB", db_path)
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_USE_FAKE_CONTROL_PLANE")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .env_remove("BOWLINE_WORKOS_REFRESH_TOKEN")
        .env_remove("BOWLINE_WORKSPACE_ID")
        .output()
        .expect("bowline should run");

    assert_eq!(output.status.code(), Some(4));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "device list");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["code"], "device_trust_requires_action");
    assert_eq!(json["error"]["recoverability"], "user-action");
    let message = json["error"]["message"]
        .as_str()
        .expect("error message is a string");
    assert!(!message.contains("fake control plane"));
}

#[test]
fn devices_and_trust_commands_infer_unambiguous_local_root() {
    let temp = TempWorkspace::new("trust-infer-root").expect("temp workspace");
    let code_root = temp.root().join("Code");
    fs::create_dir_all(&code_root).expect("code root");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-02T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-07-02T12:00:00Z",
        )
        .expect("root insert");

    for args in [
        &["device", "list", "--json"][..],
        &[
            "device",
            "approve",
            "--request",
            "device-request:ws_code:dev",
            "--yes",
            "--json",
        ][..],
    ] {
        let output = bowline()
            .args(args)
            .current_dir(&code_root)
            .env("BOWLINE_METADATA_DB", db_path.display().to_string())
            .env_remove("CONVEX_URL")
            .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
            .env_remove("BOWLINE_USE_FAKE_CONTROL_PLANE")
            .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
            .env_remove("BOWLINE_WORKOS_REFRESH_TOKEN")
            .env_remove("BOWLINE_WORKSPACE_ID")
            .output()
            .expect("bowline should run");

        assert_eq!(output.status.code(), Some(4), "{args:?}: {output:?}");
        let json = parse_stdout_json(output);
        assert_ne!(json["status"], "usage-error", "{args:?}");
        assert_eq!(json["error"]["recoverability"], "user-action", "{args:?}");
    }
}

#[test]
fn bare_trust_command_with_multiple_workspace_roots_lists_candidates() {
    let temp = TempWorkspace::new("trust-ambiguous-root").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let other_root = temp.root().join("OtherCode");
    fs::create_dir_all(&code_root).expect("code root");
    fs::create_dir_all(&other_root).expect("other root");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    for (workspace, root_id, root) in [
        ("ws_code", "root_code", &code_root),
        ("ws_other", "root_other", &other_root),
    ] {
        let workspace_id = WorkspaceId::new(workspace);
        store
            .insert_workspace(&workspace_id, "User Code", "2026-07-02T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root(
                root_id,
                &workspace_id,
                &root.display().to_string(),
                "2026-07-02T12:00:00Z",
            )
            .expect("root insert");
    }

    let output = bowline()
        .args([
            "device",
            "approve",
            "--request",
            "device-request:ws_code:dev",
            "--yes",
            "--json",
        ])
        .current_dir(temp.root())
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .output()
        .expect("bowline should run");

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "device approve");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "ambiguous_root");
    let message = json["error"]["message"].as_str().expect("message");
    assert!(message.contains(&code_root.display().to_string()));
    assert!(message.contains(&other_root.display().to_string()));
    let actions = json["nextActions"].as_array().expect("next actions");
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().all(|action| {
        action["command"]
            .as_str()
            .expect("command")
            .contains("--root")
    }));
}

#[test]
fn approve_without_yes_in_human_mode_does_not_mutate_from_noninteractive_shell() {
    let temp = TempWorkspace::new("approve-no-yes-confirmation").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-26T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-26T12:00:00Z")
        .expect("root insert");

    let output = bowline()
        .args([
            "device",
            "approve",
            "--root",
            "~/Code",
            "--request",
            "device-request:ws_code:dev-mac",
            "--human",
        ])
        .current_dir(temp.root())
        .env("BOWLINE_METADATA_DB", db_path)
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_USE_FAKE_CONTROL_PLANE")
        .output()
        .expect("bowline should run");

    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
}

#[test]
fn connect_uses_configured_metadata_db_active_root_by_default() {
    let temp = TempWorkspace::new("connect-active-root").expect("temp workspace");
    let code_root = temp.root().join("Code Projects");
    fs::create_dir_all(&code_root).expect("code root");
    let db_path = temp.root().join(".state/local.sqlite3");
    seed_daemon_start_workspace(&db_path, &code_root);

    let output = run_bowline_with_env(
        &["connect", "bad host", "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-26T12:00:00Z".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(3));
    let json = parse_stdout_json(output);
    let expected_root = code_root.display().to_string();
    assert_eq!(json["command"], "connect");
    assert_eq!(json["root"], expected_root);
    assert!(
        json["repairActions"]
            .as_array()
            .expect("repair actions")
            .iter()
            .any(|action| action["command"].as_str().is_some_and(|command| {
                command.contains("bowline connect 'bad host'")
                    && command.contains("--root '")
                    && command.contains(&expected_root)
            })),
        "connect retry action should preserve the configured active root: {json}"
    );
}

#[test]
fn daemon_status_exercises_socket_handshake() {
    let socket = unique_socket("cli");
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("test socket should bind");
    listener
        .set_nonblocking(true)
        .expect("test socket should become nonblocking");

    let server = spawn_daemon_info_server(listener, None);

    let output = bowline()
        .args(["daemon", "status", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline daemon status should run");

    server.join().expect("test daemon should finish");
    let _ = fs::remove_file(&socket);

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "daemon status");
    assert_eq!(json["daemon"]["state"], "running");
    assert_eq!(json["daemon"]["socket"], socket.display().to_string());
    assert_eq!(json["daemon"]["protocol"], DAEMON_RPC_PROTOCOL);
    assert_eq!(json["daemon"]["version"], DAEMON_RPC_PROTOCOL_VERSION);
    assert_eq!(json["daemon"]["daemonVersion"], "test-daemon");
    if let Some(service) = json.get("service") {
        assert!(service["unitPath"].as_str().is_some_and(|path| {
            path.ends_with("bowline.service") || path.ends_with("io.bowline.daemon.plist")
        }));
    }
}

#[test]
fn daemon_start_json_keeps_process_state_running_for_degraded_sync() {
    let socket = unique_socket("daemon-start-degraded-json");
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("test socket should bind");
    listener
        .set_nonblocking(true)
        .expect("test socket should become nonblocking");

    let server = spawn_daemon_info_server(
        listener,
        Some(serde_json::json!({
            "workspaceId": "ws_code",
            "status": {
                "level": "attention",
                "attentionItems": ["offline"],
            },
        })),
    );

    let output = bowline()
        .args(["daemon", "start", "--json", "--socket"])
        .arg(&socket)
        .env("BOWLINE_WORKSPACE_ID", "ws_code")
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .output()
        .expect("bowline daemon start should run");

    server.join().expect("test daemon should finish");
    let _ = fs::remove_file(&socket);

    assert!(output.status.success(), "{output:?}");
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "daemon start");
    assert_eq!(json["daemon"]["state"], "running");
    assert_eq!(json["daemon"]["syncState"], "degraded");
    assert_eq!(json["daemon"]["unavailableBecause"], "offline");
    assert_eq!(json["daemon"]["daemonVersion"], "test-daemon");
}

#[test]
fn daemon_start_spawns_real_daemon_for_initialized_root() {
    let temp = TempWorkspace::new("daemon-start").expect("temp workspace");
    let code_root = temp.root().join("Code");
    fs::create_dir_all(&code_root).expect("code root");
    fs::write(code_root.join("README.md"), "hello\n").expect("workspace file");
    let db_path = temp.root().join("state").join("local.sqlite3");
    seed_daemon_start_workspace(&db_path, &code_root);
    let socket = unique_socket("daemon-start");
    let output = bowline()
        .args(["daemon", "start", "--json", "--socket"])
        .arg(&socket)
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .env("BOWLINE_WORKSPACE_ID", "ws_code")
        .env("BOWLINE_SECRET_STORE_PATH", temp.root().join("secrets.v1"))
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .output()
        .expect("bowline daemon start should run");

    assert!(output.status.success(), "{output:?}");
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "daemon start");
    assert_eq!(json["daemon"]["state"], "starting");
    let pid = json["daemon"]["pid"].as_u64().expect("pid") as u32;
    let _guard = ProcessKillGuard(pid);

    let running = wait_for_daemon_status(&socket);
    let _ = fs::remove_file(&socket);

    assert_eq!(running["daemon"]["state"], "running");
}

#[test]
fn daemon_start_uses_current_metadata_workspace_when_env_is_unset() {
    let temp = TempWorkspace::new("daemon-start-current-workspace").expect("temp workspace");
    let code_root = temp.root().join("Code");
    fs::create_dir_all(&code_root).expect("code root");
    fs::write(code_root.join("README.md"), "hello\n").expect("workspace file");
    let db_path = temp.root().join("state").join("local.sqlite3");
    seed_daemon_start_workspace_with_id(&db_path, &code_root, "ws_bootstrapped");
    let socket = unique_socket("daemon-start-current-workspace");
    let output = bowline()
        .args(["daemon", "start", "--json", "--socket"])
        .arg(&socket)
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .env("BOWLINE_SECRET_STORE_PATH", temp.root().join("secrets.v1"))
        .env_remove("BOWLINE_WORKSPACE_ID")
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .output()
        .expect("bowline daemon start should run");

    assert!(output.status.success(), "{output:?}");
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "daemon start");
    let pid = json["daemon"]["pid"].as_u64().expect("pid") as u32;
    let _guard = ProcessKillGuard(pid);

    let running = wait_for_daemon_status(&socket);
    let _ = fs::remove_file(&socket);

    assert_eq!(running["daemon"]["state"], "running");
}

#[test]
fn daemon_stop_shuts_down_started_daemon() {
    let temp = TempWorkspace::new("daemon-stop").expect("temp workspace");
    let code_root = temp.root().join("Code");
    fs::create_dir_all(&code_root).expect("code root");
    fs::write(code_root.join("README.md"), "hello\n").expect("workspace file");
    let db_path = temp.root().join("state").join("local.sqlite3");
    seed_daemon_start_workspace(&db_path, &code_root);
    let socket = unique_socket("daemon-stop");
    let start = bowline()
        .args(["daemon", "start", "--json", "--socket"])
        .arg(&socket)
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .env("BOWLINE_WORKSPACE_ID", "ws_code")
        .env("BOWLINE_SECRET_STORE_PATH", temp.root().join("secrets.v1"))
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .output()
        .expect("bowline daemon start should run");

    assert!(start.status.success(), "{start:?}");
    let start_json = parse_stdout_json(start);
    let pid = start_json["daemon"]["pid"].as_u64().expect("pid") as u32;
    let _guard = ProcessKillGuard(pid);
    let _running = wait_for_daemon_status(&socket);

    let stop = bowline()
        .args(["daemon", "stop", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline daemon stop should run");

    assert!(stop.status.success(), "{stop:?}");
    let stop_json = parse_stdout_json(stop);
    assert_eq!(stop_json["command"], "daemon stop");
    assert_eq!(stop_json["daemon"]["state"], "stopping");
    let stopped = wait_for_daemon_stopped(&socket);
    assert_eq!(stopped["daemon"]["state"], "stopped");
}

#[test]
fn daemon_stop_is_idempotent_before_the_first_start() {
    let socket = unique_socket("daemon-never-started");
    let stop = bowline()
        .args(["daemon", "stop", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline daemon stop should run");

    assert!(stop.status.success(), "{stop:?}");
    let stop_json = parse_stdout_json(stop);
    assert_eq!(stop_json["command"], "daemon stop");
    assert_eq!(stop_json["daemon"]["state"], "stopped");
}

struct ProcessKillGuard(u32);

impl Drop for ProcessKillGuard {
    fn drop(&mut self) {
        kill_process(self.0);
    }
}
