// Integration-test crate: helpers may panic (clippy only exempts #[test] fns).
#![allow(clippy::panic)]

use std::fs;
use std::io::{self, Write};
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
        "{\"ok\":true,\"command\":\"help\",\"phase\":\"0D\",\"commands\":[\"serve\",\"stop\",\"status\",\"metrics\",\"version\"],\"socket\":{\"protocol\":\"bowline-daemon-v2\",\"version\":2}}\n"
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
    assert_eq!(parsed["daemon"]["daemonVersion"], env!("CARGO_PKG_VERSION"));
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
    let mut stream = connect_to_socket(&socket, &mut child);
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
    assert_eq!(metrics["coordinator"]["configuredWorkers"], 5);
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
    let mut stream = connect_to_socket(&socket, &mut child);
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
    assert_eq!(parsed["daemon"]["daemonVersion"], env!("CARGO_PKG_VERSION"));
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

    let mut stream = connect_to_socket(&socket, &mut child);
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
fn serve_reports_v8_status_without_legacy_convergence_journal() {
    // Plan 111 Step 1: status now flows from the manifest engine snapshot, not the
    // old convergence journal. Without a workspace key the manifest driver does not
    // start, so a served daemon reports a coherent v8 status with no fabricated
    // convergence state — the journal no longer drives the status snapshot. The
    // engine-snapshot-to-v8 mapping is proven in-process (see the daemon-crate
    // `manifest_engine_status_*` tests); a spawned binary cannot inject a fake
    // transport to reach `ready` without real hosted infrastructure.
    let _serve_guard = daemon_serve_test_guard();
    let root = unique_temp_dir("continuous-sync-root");
    let state_root = unique_temp_dir("continuous-sync-state");
    fs::create_dir_all(root.join("app").join("src")).expect("project dirs");
    fs::write(root.join("app").join("package.json"), br#"{"name":"app"}"#).expect("package");
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
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status json parses");

    let stop = daemon()
        .args(["stop", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("bowline-daemon stop should run");
    let serve_status = wait_for_child(&mut child);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&state_root);

    assert!(stop.status.success());
    assert!(serve_status.success());
    assert_eq!(parsed["snapshot"]["contractVersion"], 8);
    assert_eq!(parsed["snapshot"]["workspaceId"], "ws_code");
    // A configured workspace whose manifest driver cannot build (no workspace
    // key here) must surface a truthful engine-host `limited` state — never
    // silently fall back to the legacy journal. Host snapshots publish in the
    // disjoint 1<<60 revision namespace, which is the proof the convergence
    // block is engine-host-driven rather than journal-driven.
    let convergence = &parsed["snapshot"]["convergence"];
    assert_eq!(
        convergence["state"], "limited",
        "engine host must report limited while the driver is pending: {parsed}"
    );
    let revision = convergence["revision"]
        .as_u64()
        .expect("convergence revision is a u64");
    assert!(
        revision >= 1 << 60,
        "convergence revision must come from the engine-host namespace, \
         not the legacy journal: {parsed}"
    );
    // The legacy convergence journal is deleted: no journal-derived items or
    // facts may appear. The engine host snapshot is the only convergence
    // authority in the status output.
    let snapshot_text = parsed["snapshot"].to_string();
    assert!(
        !snapshot_text.contains("Recovering"),
        "journal-derived recovery items must not appear: {parsed}"
    );
    let facts = parsed["snapshot"]["facts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        facts.iter().all(|fact| {
            fact["id"]
                .as_str()
                .is_none_or(|id| !id.starts_with("sync-attempt"))
        }),
        "journal attempt facts must not appear: {parsed}"
    );
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

fn connect_to_socket(socket: &Path, child: &mut Child) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => panic!("failed to connect to daemon socket: {error}"),
        }
        if let Some(status) = child.try_wait().expect("serve status should be readable") {
            panic!("bowline-daemon serve exited before accepting connections: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon socket connection"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn daemon_serve_test_guard() -> MutexGuard<'static, ()> {
    DAEMON_SERVE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Poll the daemon status RPC until its snapshot reports `expected_queue`, then
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
