// Integration-test crate: helpers may panic (clippy only exempts #[test] fns).
#![allow(clippy::panic)]

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bowline_core::commands::CONTRACT_VERSION;
use bowline_core::wire::generated::{
    DaemonClientHello, DaemonRpcError, DaemonRpcErrorCode, DaemonRpcRequest, DaemonRpcResponse,
    DaemonServerHello,
};
use bowline_daemon_rpc::{CONNECTION_MAGIC, DAEMON_RPC_PROTOCOL, FrameCodec};
use serde_json::json;

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);
static DAEMON_SERVE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn daemon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bowline-daemon"))
}

fn isolated_daemon(state_root: &Path) -> Command {
    let mut command = daemon();
    command
        .env("HOME", state_root.join("home"))
        .env("XDG_STATE_HOME", state_root.join("xdg-state"))
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_ACCOUNT_SESSION_ID")
        .env_remove("BOWLINE_BOOTSTRAP_TOKEN")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .env_remove("BOWLINE_WORKOS_REFRESH_TOKEN")
        .env_remove("BOWLINE_METADATA_DB")
        .env_remove("BOWLINE_SECRET_STORE");
    command
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
            "{{\"ok\":true,\"command\":\"status\",\"daemon\":{{\"state\":\"stopped\",\"socket\":{},\"protocol\":\"bowline-daemon-v2\",\"version\":2}}}}\n",
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
        "{\"ok\":true,\"command\":\"help\",\"phase\":\"0D\",\"commands\":[\"serve\",\"sync-once\",\"stop\",\"status\",\"version\"],\"socket\":{\"protocol\":\"bowline-daemon-v2\",\"version\":2}}\n"
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
    let _serve_guard = daemon_serve_test_guard();
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
    let stdout = String::from_utf8(output.stdout).expect("json should be utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("status json parses");
    assert_eq!(parsed["daemon"]["state"], "running");
    assert_eq!(parsed["daemon"]["socket"], socket.display().to_string());
    assert_eq!(parsed["daemon"]["protocol"], "bowline-daemon-v2");
    assert_eq!(parsed["daemon"]["version"], 2);
    assert_eq!(parsed["daemon"]["daemonVersion"], "0.1.1");
    assert_eq!(parsed["snapshot"]["contractVersion"], 8);
    assert_eq!(parsed["snapshot"]["command"], "status");
    assert!(parsed["snapshot"]["status"]["level"].is_string());
}

#[test]
fn serve_once_accepts_fragmented_v2_magic_and_multiple_framed_messages() {
    let _serve_guard = daemon_serve_test_guard();
    let socket = unique_socket("serve-v2-fragmented");
    let _ = fs::remove_file(&socket);
    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon v2 serve should spawn");
    wait_for_socket(&socket, &mut child);

    let mut stream = UnixStream::connect(&socket).expect("connect v2");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("write timeout");
    for byte in CONNECTION_MAGIC {
        stream.write_all(&[byte]).expect("fragmented magic byte");
    }
    let codec = FrameCodec::default();
    codec
        .write(
            &mut stream,
            &DaemonClientHello {
                protocol: DAEMON_RPC_PROTOCOL.to_string(),
                protocol_version: 2,
                contract_version: CONTRACT_VERSION,
                schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH.to_string(),
                client_kind: "integration-test".to_string(),
                client_version: "1".to_string(),
                capabilities: vec!["daemon.ping".to_string(), "daemon.metrics".to_string()],
            },
        )
        .expect("hello frame");
    let hello: DaemonServerHello = codec.read(&mut stream).expect("server hello");
    assert_eq!(hello.protocol_version, 2);
    assert_eq!(hello.contract_version, CONTRACT_VERSION);
    assert_eq!(
        hello.schema_hash,
        bowline_core::wire::generated::WIRE_SCHEMA_HASH
    );

    codec
        .write(
            &mut stream,
            &DaemonRpcRequest {
                request_id: "request-ping".to_string(),
                method: "daemon.ping".to_string(),
                params: json!({}),
                deadline_ms: Some(500),
            },
        )
        .expect("ping frame");
    let response: DaemonRpcResponse = codec.read(&mut stream).expect("ping response");
    assert_eq!(response.request_id, "request-ping");
    assert_eq!(response.result, Some(json!({"ok": true})));
    assert!(response.error.is_none());
    codec
        .write(
            &mut stream,
            &DaemonRpcRequest {
                request_id: "request-metrics".to_string(),
                method: "daemon.metrics".to_string(),
                params: json!({}),
                deadline_ms: Some(500),
            },
        )
        .expect("metrics frame");
    let metrics: DaemonRpcResponse = codec.read(&mut stream).expect("metrics response");
    assert_eq!(metrics.request_id, "request-metrics");
    let metrics = metrics.result.expect("metrics payload");
    assert_eq!(metrics["coordinator"]["configuredWorkers"], 19);
    assert_eq!(metrics["rpc"]["configuredQueryWorkers"], 8);
    assert_eq!(metrics["shutdown"]["phase"], "running");
    drop(stream);

    assert!(wait_for_child(&mut child).success());
    let _ = fs::remove_file(&socket);
}

#[test]
fn serve_once_rejects_wrong_schema_hash_before_any_status_frame() {
    let _serve_guard = daemon_serve_test_guard();
    let socket = unique_socket("serve-v2-wrong-schema");
    let _ = fs::remove_file(&socket);
    let mut child = daemon()
        .arg("serve")
        .arg("--once")
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon v2 serve should spawn");
    wait_for_socket(&socket, &mut child);

    let mut stream = UnixStream::connect(&socket).expect("connect v2");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let codec = FrameCodec::default();
    codec.write_magic(&mut stream).expect("write magic");
    codec
        .write(
            &mut stream,
            &DaemonClientHello {
                protocol: DAEMON_RPC_PROTOCOL.to_string(),
                protocol_version: 2,
                contract_version: CONTRACT_VERSION,
                schema_hash: "different-schema".to_string(),
                client_kind: "integration-test".to_string(),
                client_version: "1".to_string(),
                capabilities: vec!["status.snapshot".to_string()],
            },
        )
        .expect("hello frame");
    let error: DaemonRpcError = codec.read(&mut stream).expect("structured mismatch error");
    assert_eq!(error.code, DaemonRpcErrorCode::UnsupportedVersion);
    assert!(error.message.contains("wire schema hash"));
    assert!(codec.read::<serde_json::Value, _>(&mut stream).is_err());

    drop(stream);
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    assert!(serve_status.success());
}

#[test]
fn default_serve_status_has_no_synthetic_mount_state() {
    let _serve_guard = daemon_serve_test_guard();
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
    let stdout = String::from_utf8(output.stdout).expect("json should be utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("status json parses");
    assert_eq!(parsed["daemon"]["state"], "running");
    assert_eq!(parsed["daemon"]["socket"], socket.display().to_string());
    assert_eq!(parsed["daemon"]["protocol"], "bowline-daemon-v2");
    assert_eq!(parsed["daemon"]["version"], 2);
    assert_eq!(parsed["daemon"]["daemonVersion"], "0.1.1");
    assert_eq!(parsed["snapshot"]["contractVersion"], 8);
    assert_eq!(parsed["snapshot"]["command"], "status");
    assert!(parsed["snapshot"]["status"]["level"].is_string());
}

#[test]
fn unsupported_framed_method_returns_structured_error() {
    let _serve_guard = daemon_serve_test_guard();
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

    let mut stream = UnixStream::connect(&socket).expect("connect daemon");
    let codec = FrameCodec::default();
    codec.write_magic(&mut stream).expect("write magic");
    codec
        .write(
            &mut stream,
            &DaemonClientHello {
                protocol: DAEMON_RPC_PROTOCOL.to_string(),
                protocol_version: 2,
                contract_version: CONTRACT_VERSION,
                schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH.to_string(),
                client_kind: "integration-test".to_string(),
                client_version: "1".to_string(),
                capabilities: vec![],
            },
        )
        .expect("write hello");
    let _: DaemonServerHello = codec.read(&mut stream).expect("server hello");
    codec
        .write(
            &mut stream,
            &DaemonRpcRequest {
                request_id: "request-unsupported".to_string(),
                method: "something.else".to_string(),
                params: json!({}),
                deadline_ms: Some(500),
            },
        )
        .expect("write unsupported request");
    let response: DaemonRpcResponse = codec.read(&mut stream).expect("error response");
    drop(stream);
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);

    assert!(serve_status.success());
    let error = response.error.expect("structured error");
    assert_eq!(error.code.to_string(), "method_not_found");
}

#[test]
fn stop_shuts_down_running_daemon() {
    let _serve_guard = daemon_serve_test_guard();
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

    let output = isolated_daemon(&state_root)
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
    let _serve_guard = daemon_serve_test_guard();
    let root = unique_temp_dir("continuous-sync-root");
    let state_root = unique_temp_dir("continuous-sync-state");
    fs::create_dir_all(root.join("app").join("src")).expect("project dirs");
    fs::write(root.join("app").join("package.json"), br#"{"name":"app"}"#).expect("package");
    fs::write(
        root.join("app").join("src").join("main.ts"),
        b"export const value = 1;\n",
    )
    .expect("source");
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_code");
    let db_path = state_root.join(bowline_local::metadata::DEFAULT_DATABASE_FILE);
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &root.display().to_string(),
            "2026-07-13T00:00:00Z",
        )
        .expect("root insert");
    drop(store);

    let socket = unique_socket("continuous-sync");
    let _ = fs::remove_file(&socket);
    let mut child = isolated_daemon(&state_root)
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
        .arg("5000")
        .arg("--sync-max-ticks")
        .arg("2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bowline-daemon serve should spawn");
    wait_for_socket(&socket, &mut child);
    wait_for_sync_attention(&db_path, &workspace_id, &mut child);

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
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("status json parses");
    assert_eq!(parsed["snapshot"]["workspaceId"], "ws_code");
    assert_eq!(
        parsed["snapshot"]["syncQueue"],
        json!({
            "queued": 0,
            "claimed": 0,
            "waitingRetry": 0,
            "blockedOffline": 0,
            "reconciliationRequired": 0,
            "attention": 1,
            "completed": 0,
        })
    );
    assert_eq!(parsed["snapshot"]["status"]["level"], "attention");
    assert_eq!(
        parsed["snapshot"]["statusSummary"]["primaryFactId"],
        "sync-queue-blocked"
    );
    assert_eq!(
        parsed["snapshot"]["eventWatermarks"]["syncState"],
        "degraded"
    );
    assert_eq!(
        parsed["snapshot"]["eventWatermarks"]["watcherState"],
        "ready"
    );
    assert_eq!(
        parsed["snapshot"]["eventWatermarks"]["networkState"],
        "degraded"
    );
    assert!(
        parsed["snapshot"]["limits"]
            .as_array()
            .is_some_and(|limits| limits.iter().any(|limit| {
                limit["capability"] == "sync"
                    && limit["unavailableBecause"] == "sync queue needs attention"
            })),
        "{stdout}"
    );
}

#[test]
fn serve_once_handles_agent_tool_invoke_over_local_socket() {
    let _serve_guard = daemon_serve_test_guard();
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

    let request = agent_tool_request(
        "req_list_capabilities",
        "list_capabilities",
        "local-daemon",
        false,
        json!({}),
    );
    let response = rpc_response(&socket, "agent.tool.invoke", request);
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    let result = response.result.expect("agent result");
    assert_eq!(result["tool"], "list_capabilities");
    assert_eq!(result["outcome"], "allowed");
    assert!(result.get("noncePresented").is_none());
}

#[test]
fn serve_once_rejects_mcp_authority_booleans_over_local_socket() {
    let _serve_guard = daemon_serve_test_guard();
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

    let request = agent_tool_request(
        "req_mcp",
        "list_capabilities",
        "mcp-adapter",
        true,
        json!({}),
    );
    let response = rpc_response(&socket, "agent.tool.invoke", request);
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    let result = response.result.expect("agent denial result");
    assert_eq!(result["outcome"], "denied");
    assert_eq!(result["denial"]["code"], "mcp-token-file-required");
}

#[test]
fn serve_once_rejects_agent_tool_protocol_mismatch() {
    let _serve_guard = daemon_serve_test_guard();
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

    let response = rpc_response(
        &socket,
        "agent.tool.invoke",
        json!({
            "type": "agent.tool.invoke",
            "protocolVersion": 999,
            "requestId": "req_protocol",
            "leaseId": "lease_test",
            "tool": "list_capabilities",
            "authority": {
                "transport": "local-daemon",
                "peerCredentialChecked": true,
                "noncePresented": true
            },
            "arguments": {}
        }),
    );
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    let error = response.error.expect("structured protocol error");
    assert_eq!(error.code.to_string(), "unsupported_version");
}

#[test]
fn serve_once_handles_read_only_agent_tool_over_socket() {
    let _serve_guard = daemon_serve_test_guard();
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

    // All mutating agent tools were removed with the supervisor stack; the daemon
    // now serves only read-only inspection tools over the socket.
    let request = agent_tool_request(
        "req_status",
        "workspace_status",
        "local-daemon",
        false,
        json!({}),
    );
    let response = rpc_response(&socket, "agent.tool.invoke", request);
    let serve_status = wait_for_child(&mut child);
    let store = MetadataStore::open(&db_path).expect("metadata");
    let lease = store
        .agent_lease_by_id(&LeaseId::new("lease_test"))
        .expect("lease query")
        .expect("lease");
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&temp);

    assert!(serve_status.success());
    assert_eq!(response.result.expect("agent result")["outcome"], "allowed");
    // A read-only tool does not mutate the lease record's updated_at.
    assert!(lease.updated_at.ends_with('Z'), "{}", lease.updated_at);
}

fn rpc_response(socket: &Path, method: &str, params: serde_json::Value) -> DaemonRpcResponse {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("socket connects: {error}"),
        }
    };
    let codec = FrameCodec::default();
    codec.write_magic(&mut stream).expect("write RPC magic");
    codec
        .write(
            &mut stream,
            &DaemonClientHello {
                protocol: DAEMON_RPC_PROTOCOL.to_string(),
                protocol_version: 2,
                contract_version: CONTRACT_VERSION,
                schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH.to_string(),
                client_kind: "integration-test".to_string(),
                client_version: "1".to_string(),
                capabilities: vec!["agent.tool.invoke".to_string()],
            },
        )
        .expect("write client hello");
    let _: DaemonServerHello = codec.read(&mut stream).expect("read server hello");
    codec
        .write(
            &mut stream,
            &DaemonRpcRequest {
                request_id: "integration-request".to_string(),
                method: method.to_string(),
                params,
                deadline_ms: Some(5_000),
            },
        )
        .expect("write RPC request");
    codec.read(&mut stream).expect("read RPC response")
}

fn agent_tool_request(
    request_id: &str,
    tool: &str,
    transport: &str,
    peer_credential_checked: bool,
    arguments: serde_json::Value,
) -> serde_json::Value {
    json!({
        "type": "agent.tool.invoke",
        "protocolVersion": CONTRACT_VERSION,
        "requestId": request_id,
        "leaseId": "lease_test",
        "tool": tool,
        "authority": {
            "transport": transport,
            "peerCredentialChecked": peer_credential_checked,
            "noncePresented": true
        },
        "arguments": arguments
    })
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

fn daemon_serve_test_guard() -> MutexGuard<'static, ()> {
    DAEMON_SERVE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_for_sync_attention(
    db_path: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
    child: &mut Child,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let attention = bowline_local::metadata::MetadataStore::open(db_path)
            .and_then(|store| store.sync_operation_counts(workspace_id))
            .is_ok_and(|counts| counts.attention > 0);
        if attention {
            return;
        }
        if let Some(status) = child.try_wait().expect("serve status should be readable") {
            panic!("bowline-daemon serve exited before sync reached attention: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for continuous sync attention state"
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
    // prepare_socket refuses parents not owned by the current uid, so sockets
    // must live under a per-user dir, not directly in world-writable /tmp.
    // The label is dropped from the path because macOS caps unix socket paths
    // at ~104 bytes and $TMPDIR is already long. A process-local counter avoids
    // clock-resolution collisions between concurrently running contract tests.
    let _ = label;
    let socket_id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bld-{}-{socket_id}", std::process::id()));
    fs::create_dir_all(&dir).expect("socket dir");
    dir.join("s.sock")
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
        ids::{DeviceId, ProjectId, WorkspaceId},
    };
    use bowline_local::{
        agents::{AgentLeaseCreateOptions, create_agent_lease},
        metadata::MetadataStore,
    };

    let mut store = MetadataStore::open(db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
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
    bowline_testkit::persist_project_snapshot_fixture(
        &mut store,
        &workspace_id,
        &project_id,
        code_root,
        "apps/web",
        db_path.parent().expect("metadata state root"),
        "2026-06-25T00:00:00Z",
    );
    drop(store);
    let output: AgentLeaseCreateCommandOutput = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.to_path_buf()),
        project_path: project_path.display().to_string(),
        task: "daemon tool".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: "2999-06-25T12:00:00Z".to_string(),
    })
    .expect("lease");
    let mut lease = output.lease;
    lease.id = bowline_core::ids::LeaseId::new("lease_test");
    let store = MetadataStore::open(db_path).expect("metadata");
    store.upsert_agent_lease(&lease).expect("stable lease id");
}
