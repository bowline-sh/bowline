use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn daemon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bowline-daemon"))
}

#[test]
fn stopped_status_json_is_stable() {
    let socket = unique_socket("stopped");
    let _ = fs::remove_file(&socket);

    let output = daemon()
        .args(["status", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline-daemon status should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("json should be utf8"),
        format!(
            "{{\"ok\":true,\"command\":\"status\",\"daemon\":{{\"state\":\"stopped\",\"socket\":{},\"protocol\":\"bowline.local\",\"version\":1}}}}\n",
            json_string(&socket.display().to_string())
        )
    );
}

#[test]
fn help_json_lists_only_real_daemon_commands() {
    let output = daemon()
        .args(["--json", "help"])
        .output()
        .expect("bowline-daemon help should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("json should be utf8"),
        "{\"ok\":true,\"command\":\"help\",\"phase\":\"0D\",\"commands\":[\"serve\",\"sync-once\",\"stop\",\"status\",\"version\"],\"socket\":{\"protocol\":\"bowline.local\",\"version\":1}}\n"
    );
}

#[test]
fn sync_once_defaults_to_file_secret_store_without_keychain_probe() {
    let temp = unique_temp_dir("daemon-secret-store");
    let home = temp.join("home");
    let state = temp.join("state");
    let root = temp.join("Code");
    let sync_state = temp.join("sync-state");
    fs::create_dir_all(&root).expect("root dir");
    fs::create_dir_all(&home).expect("home dir");
    fs::create_dir_all(&state).expect("state dir");

    let output = daemon()
        .env_clear()
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state)
        .env("CONVEX_URL", "http://127.0.0.1:9")
        .args(["--json", "sync-once", "--root"])
        .arg(&root)
        .args(["--state-root"])
        .arg(&sync_state)
        .output()
        .expect("bowline-daemon sync-once should run");

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("workspace key is missing"),
        "expected local secret-store failure, got: {stdout}"
    );
    assert!(
        !stdout.contains("OS keychain failed"),
        "daemon should not touch keychain by default: {stdout}"
    );
}

#[test]
fn serve_once_answers_version_handshake() {
    let socket = unique_socket("serve");
    let _ = fs::remove_file(&socket);

    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");

    wait_for_socket(&socket, &mut child);

    let output = daemon()
        .args(["status", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline-daemon status should run");

    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);

    assert!(serve_status.success());
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("json should be utf8"),
        format!(
            "{{\"ok\":true,\"command\":\"status\",\"daemon\":{{\"state\":\"running\",\"socket\":{},\"protocol\":\"bowline.local\",\"version\":1,\"daemonVersion\":\"0.0.0\"}}}}\n",
            json_string(&socket.display().to_string())
        )
    );
}

#[test]
fn default_serve_status_has_no_synthetic_mount_state() {
    let socket = unique_socket("serve-prepared");
    let _ = fs::remove_file(&socket);

    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");

    wait_for_socket(&socket, &mut child);

    let output = daemon()
        .args(["status", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline-daemon status should run");

    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);

    assert!(serve_status.success());
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("json should be utf8"),
        format!(
            "{{\"ok\":true,\"command\":\"status\",\"daemon\":{{\"state\":\"running\",\"socket\":{},\"protocol\":\"bowline.local\",\"version\":1,\"daemonVersion\":\"0.0.0\"}}}}\n",
            json_string(&socket.display().to_string())
        )
    );
}

#[test]
fn unsupported_socket_request_names_real_supported_types() {
    let socket = unique_socket("unsupported-request");
    let _ = fs::remove_file(&socket);

    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");
    wait_for_socket(&socket, &mut child);

    let response = socket_response(
        &socket,
        br#"{"type":"something.else","protocol":"bowline.local","version":1}
"#,
    );
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);

    assert!(serve_status.success());
    assert!(
        response.contains("\"code\":\"unsupported_request\""),
        "{response}"
    );
    assert!(
        response.contains("hello, shutdown, agent.tool.invoke"),
        "{response}"
    );
    assert!(!response.contains("only version handshake"), "{response}");
}

#[test]
fn stop_shuts_down_running_daemon() {
    let socket = unique_socket("stop");
    let _ = fs::remove_file(&socket);

    let mut child = daemon()
        .arg("serve")
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");

    wait_for_socket(&socket, &mut child);

    let output = daemon()
        .args(["stop", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline-daemon stop should run");

    let serve_status = wait_for_child(&mut child);

    assert!(serve_status.success());
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("json should be utf8"),
        format!(
            "{{\"ok\":true,\"command\":\"stop\",\"daemon\":{{\"state\":\"stopping\",\"socket\":{}}}}}\n",
            json_string(&socket.display().to_string())
        )
    );
    assert!(!socket.exists());
}

#[test]
fn sync_once_json_requires_workspace_key_without_env() {
    let root = unique_temp_dir("sync-root");
    let state_root = unique_temp_dir("sync-state");
    fs::create_dir_all(root.join("app").join("src")).expect("project dirs");
    fs::write(root.join("app").join("package.json"), br#"{"name":"app"}"#).expect("package");
    fs::write(
        root.join("app").join("src").join("main.ts"),
        b"export const value = 1;\n",
    )
    .expect("source");

    let output = daemon()
        .arg("sync-once")
        .arg("--json")
        .arg("--root")
        .arg(&root)
        .arg("--state-root")
        .arg(&state_root)
        .arg("--workspace")
        .arg("ws_code")
        .arg("--device")
        .arg("device-test")
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .output()
        .expect("bowline-daemon sync-once should run");

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&state_root);

    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stdout = String::from_utf8(output.stdout).expect("json should be utf8");
    assert!(stdout.contains("\"ok\":false"));
    assert!(stdout.contains("\"command\":\"sync-once\""));
    assert!(stdout.contains("workspace key is missing"));
}

#[test]
fn serve_reports_continuous_sync_state_without_manual_sync_command() {
    let root = unique_temp_dir("continuous-sync-root");
    let state_root = unique_temp_dir("continuous-sync-state");
    fs::create_dir_all(root.join("app").join("src")).expect("project dirs");
    fs::write(root.join("app").join("package.json"), br#"{"name":"app"}"#).expect("package");
    fs::write(
        root.join("app").join("src").join("main.ts"),
        b"export const value = 1;\n",
    )
    .expect("source");

    let socket = unique_socket("continuous-sync");
    let _ = fs::remove_file(&socket);
    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .arg("--sync-root")
        .arg(&root)
        .arg("--sync-state-root")
        .arg(&state_root)
        .arg("--sync-workspace")
        .arg("ws_code")
        .arg("--sync-device")
        .arg("device-test")
        .arg("--sync-interval-ms")
        .arg("10")
        .arg("--sync-max-ticks")
        .arg("1")
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");
    wait_for_socket(&socket, &mut child);
    thread::sleep(Duration::from_millis(80));

    let output = daemon()
        .args(["status", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline-daemon status should run");

    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&state_root);

    assert!(serve_status.success());
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("json should be utf8");
    assert!(
        stdout.contains("\"sync\":{\"state\":\"limited\""),
        "{stdout}"
    );
    assert!(stdout.contains("\"tickCount\":1"), "{stdout}");
    assert!(
        stdout.contains("\"watcherState\":{\"state\":\"ready\"}"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "\"queueCounts\":{\"queued\":0,\"claimed\":0,\"waitingRetry\":0,\"blockedOffline\":0,\"attention\":1,\"completed\":0}"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("\"localHead\":null"), "{stdout}");
    assert!(stdout.contains("workspace key is missing"), "{stdout}");
    assert!(
        stdout.contains("\"blockedAction\":\"sync ~/Code\""),
        "{stdout}"
    );
}

#[test]
fn serve_once_handles_agent_tool_invoke_over_local_socket() {
    let temp = unique_temp_dir("agent-tool");
    let code_root = temp.join("Code");
    let project_path = code_root.join("apps/web");
    fs::create_dir_all(&project_path).expect("project dir");
    let db_path = temp.join(".state/local.sqlite3");
    seed_agent_lease(&db_path, &code_root, &project_path);

    let socket = unique_socket("agent-tool");
    let _ = fs::remove_file(&socket);
    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .env("BOWLINE_METADATA_DB", &db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");
    wait_for_socket(&socket, &mut child);

    let response = socket_response(
        &socket,
        br#"{"type": "agent.tool.invoke", "protocolVersion": 3, "requestId": "req_list_capabilities", "leaseId": "lease_test", "tool": "list_capabilities", "authority": {"transport": "local-daemon", "peerCredentialChecked": false, "noncePresented": true}, "arguments": {}}
"#,
    );
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    assert!(
        response.contains("\"type\":\"agent.tool.result\""),
        "{response}"
    );
    assert!(
        response.contains("\"tool\":\"list_capabilities\""),
        "{response}"
    );
    assert!(response.contains("\"outcome\":\"allowed\""), "{response}");
    assert!(!response.contains("noncePresented"), "{response}");
}

#[test]
fn serve_once_rejects_mcp_authority_booleans_over_local_socket() {
    let temp = unique_temp_dir("agent-tool-mcp");
    let code_root = temp.join("Code");
    let project_path = code_root.join("apps/web");
    fs::create_dir_all(&project_path).expect("project dir");
    let db_path = temp.join(".state/local.sqlite3");
    seed_agent_lease(&db_path, &code_root, &project_path);

    let socket = unique_socket("agent-tool-mcp");
    let _ = fs::remove_file(&socket);
    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .env("BOWLINE_METADATA_DB", &db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");
    wait_for_socket(&socket, &mut child);

    let response = socket_response(
        &socket,
        br#"{"type": "agent.tool.invoke", "protocolVersion": 3, "requestId": "req_mcp", "leaseId": "lease_test", "tool": "list_capabilities", "authority": {"transport": "mcp-adapter", "peerCredentialChecked": true, "noncePresented": true}, "arguments": {}}
"#,
    );
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    assert!(
        response.contains("\"type\":\"agent.tool.result\""),
        "{response}"
    );
    assert!(response.contains("\"outcome\":\"denied\""), "{response}");
    assert!(response.contains("transport-not-authorized"), "{response}");
}

#[test]
fn serve_once_rejects_agent_tool_protocol_mismatch() {
    let temp = unique_temp_dir("agent-tool-protocol");
    let code_root = temp.join("Code");
    let project_path = code_root.join("apps/web");
    fs::create_dir_all(&project_path).expect("project dir");
    let db_path = temp.join(".state/local.sqlite3");
    seed_agent_lease(&db_path, &code_root, &project_path);

    let socket = unique_socket("agent-tool-protocol");
    let _ = fs::remove_file(&socket);
    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .env("BOWLINE_METADATA_DB", &db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");
    wait_for_socket(&socket, &mut child);

    let response = socket_response(
        &socket,
        br#"{"type": "agent.tool.invoke", "protocolVersion": 999, "requestId": "req_protocol", "leaseId": "lease_test", "tool": "list_capabilities", "authority": {"transport": "local-daemon", "peerCredentialChecked": true, "noncePresented": true}, "arguments": {}}
"#,
    );
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    assert!(response.contains("\"type\":\"error\""), "{response}");
    assert!(
        response.contains("unsupported_agent_tool_protocol"),
        "{response}"
    );
    assert!(
        !response.contains("\"type\":\"agent.tool.result\""),
        "{response}"
    );
}

#[test]
fn serve_once_timestamps_agent_tool_mutations_with_request_time() {
    use bowline_core::ids::LeaseId;
    use bowline_local::metadata::MetadataStore;

    let temp = unique_temp_dir("agent-tool-timestamp");
    let code_root = temp.join("Code");
    let project_path = code_root.join("apps/web");
    fs::create_dir_all(&project_path).expect("project dir");
    let db_path = temp.join(".state/local.sqlite3");
    seed_agent_lease(&db_path, &code_root, &project_path);

    let socket = unique_socket("agent-tool-timestamp");
    let _ = fs::remove_file(&socket);
    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .env("BOWLINE_METADATA_DB", &db_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");
    wait_for_socket(&socket, &mut child);

    let response = socket_response(
        &socket,
        br##"{"type": "agent.tool.invoke", "protocolVersion": 3, "requestId": "req_write", "leaseId": "lease_test", "tool": "write_overlay_file", "authority": {"transport": "local-daemon", "peerCredentialChecked": false, "noncePresented": true}, "arguments": {"path": "README.md", "contents": "# Hello\n"}}
"##,
    );
    let serve_status = wait_for_child(&mut child);
    let store = MetadataStore::open(&db_path).expect("metadata");
    let lease = store
        .agent_lease_by_id(&LeaseId::new("lease_test"))
        .expect("lease query")
        .expect("lease");
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    assert!(
        response.contains("\"type\":\"agent.tool.result\""),
        "{response}"
    );
    assert!(response.contains("\"outcome\":\"allowed\""), "{response}");
    assert_ne!(lease.updated_at, "2026-06-25T00:00:00Z");
    assert_ne!(lease.updated_at, "2026-06-25T12:00:00Z");
    assert!(lease.updated_at.ends_with('Z'), "{}", lease.updated_at);
}

fn socket_response(socket: &Path, request: &[u8]) -> String {
    let mut stream = UnixStream::connect(socket).expect("socket connects");
    stream.write_all(request).expect("request writes");
    stream.flush().expect("flush");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("response reads");
    response
}

fn wait_for_socket(socket: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if socket.exists() {
            return;
        }
        if let Some(status) = child.try_wait().expect("serve status should be readable") {
            panic!("bowline-daemon serve exited before binding socket: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon socket"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_child(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("serve status should be readable") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("timed out waiting for bowline-daemon serve to exit");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bowline-daemon-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn unique_socket(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .subsec_nanos();
    PathBuf::from(format!(
        "/tmp/bowline-daemon-{label}-{}-{nanos}.sock",
        std::process::id()
    ))
}

fn json_string(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len() + 2);
    escaped.push('"');
    for character in input.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn seed_agent_lease(db_path: &Path, code_root: &Path, project_path: &Path) {
    use bowline_core::{
        commands::{AgentLeaseBase, AgentLeaseCreateCommandOutput},
        ids::{DeviceId, ProjectId, SnapshotId, WorkspaceId},
    };
    use bowline_local::{
        agents::{AgentLeaseCreateOptions, create_agent_lease},
        metadata::MetadataStore,
    };

    let store = MetadataStore::open(db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T00:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T00:00:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T00:00:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &project_id,
            &SnapshotId::new("snap_project_base"),
        )
        .expect("snapshot");
    drop(store);
    let output: AgentLeaseCreateCommandOutput = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.to_path_buf()),
        project_path: project_path.display().to_string(),
        task: "daemon tool".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: "2026-06-25T12:00:00Z".to_string(),
    })
    .expect("lease");
    let mut lease = output.lease;
    lease.id = bowline_core::ids::LeaseId::new("lease_test");
    let store = MetadataStore::open(db_path).expect("metadata");
    store.upsert_agent_lease(&lease).expect("stable lease id");
}
