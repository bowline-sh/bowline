// Integration-test crate: long end-to-end scenario tests are expected here.
#![allow(clippy::too_many_lines)]

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bowline_core::{
    events::{EventName, EventSeverity, WorkspaceEvent},
    ids::{EventId, ProjectId, WorkspaceId},
};
use bowline_local::{
    metadata::{MetadataStore, Platform, database_path_for_platform},
    workspace::{TempWorkspace, WorkspaceMutationDetector},
};
use serde_json::Value;

fn bowline() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bowline"));
    command.env(
        "BOWLINE_SECRET_STORE_PATH",
        unique_secret_store("cli-contract").display().to_string(),
    );
    command.env(
        "BOWLINE_METADATA_DB",
        unique_db("cli-contract").display().to_string(),
    );
    command.env(
        "BOWLINE_STATE_ROOT",
        unique_state_root("cli-contract").display().to_string(),
    );
    command
}

fn run_bowline(args: &[&str]) -> Output {
    bowline().args(args).output().expect("bowline should run")
}

fn run_bowline_with_env(args: &[&str], envs: &[(&str, String)]) -> Output {
    let mut command = bowline();
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("bowline should run")
}

#[test]
fn usage_error_stderr_renders_next_actions() {
    let output = run_bowline(&["status", "--root", "--human"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("usage stderr should be utf8");
    assert!(
        stderr.contains("bowline usage error: bowline status --root requires a value"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Inspect `bowline help status --json` and retry with valid arguments."),
        "{stderr}"
    );
    assert!(stderr.contains("bowline help status"), "{stderr}");
}

#[test]
fn non_root_usage_error_points_to_help_not_root_retry() {
    let output = run_bowline(&["status", "--root", "~/Code", "--bogus", "--human"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("usage stderr should be utf8");
    assert!(
        stderr.contains("bowline usage error: unknown bowline status option `--bogus`"),
        "{stderr}"
    );
    assert!(stderr.contains("bowline help status"), "{stderr}");
    assert!(!stderr.contains("bowline status --root ~/Code"), "{stderr}");
}

fn run_bowline_with_env_removed(
    args: &[&str],
    envs: &[(&str, String)],
    removed_envs: &[&str],
) -> Output {
    let mut command = bowline();
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    for key in removed_envs {
        command.env_remove(key);
    }
    command.output().expect("bowline should run")
}

fn run_bowline_with_env_in_dir(
    args: &[&str],
    envs: &[(&str, String)],
    current_dir: &Path,
) -> Output {
    let mut command = bowline();
    command.args(args).current_dir(current_dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("bowline should run")
}

#[path = "cli_contract/agent_daemon.rs"]
mod agent_daemon;
#[path = "cli_contract/discovery_workspace.rs"]
mod discovery_workspace;
#[path = "cli_contract/lifecycle_errors.rs"]
mod lifecycle_errors;
#[path = "cli_contract/output_mode_errors.rs"]
mod output_mode_errors;
#[path = "cli_contract/status_events.rs"]
mod status_events;

fn wait_for_daemon_status(socket: &Path) -> Value {
    // 30s, not 10s: real-daemon startup is load-sensitive when the full gate
    // runs test binaries in parallel on the same machine.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = bowline()
            .args(["daemon", "status", "--json", "--socket"])
            .arg(socket)
            .output()
            .expect("bowline daemon status should run");
        if output.status.success() {
            let json = parse_stdout_json(output);
            if json["daemon"]["state"] == "running" {
                return json;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon status"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_daemon_stopped(socket: &Path) -> Value {
    // See wait_for_daemon_status on the 30s deadline.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = bowline()
            .args(["daemon", "status", "--json", "--socket"])
            .arg(socket)
            .output()
            .expect("bowline daemon status should run");
        if output.status.success() {
            let json = parse_stdout_json(output);
            if json["daemon"]["state"] == "stopped" {
                return json;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon stop"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn kill_process(pid: u32) {
    let _ = Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn unique_socket(label: &str) -> PathBuf {
    // prepare_socket refuses parents not owned by the current uid, so sockets
    // must live under a per-user dir, not directly in world-writable /tmp.
    // The label is dropped from the path because macOS caps unix socket paths
    // at ~104 bytes and $TMPDIR is already long. Reserve the directory
    // atomically: timestamp-only suffixes can repeat across second boundaries,
    // and a stale directory from a reused PID must not collide with this run.
    let _ = label;
    reserve_unique_socket_dir()
        .expect("socket directory reservation should succeed")
        .join("s.sock")
}

fn reserve_unique_socket_dir() -> io::Result<PathBuf> {
    static SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    loop {
        let sequence = SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("bl-{}-{sequence}", std::process::id()));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn unique_socket_reserves_distinct_directories() {
    let first = unique_socket("first");
    let second = unique_socket("second");

    assert_ne!(first, second);
    assert!(first.parent().is_some_and(Path::is_dir));
    assert!(second.parent().is_some_and(Path::is_dir));

    fs::remove_dir_all(first.parent().expect("first socket parent")).expect("remove first socket");
    fs::remove_dir_all(second.parent().expect("second socket parent"))
        .expect("remove second socket");
}

fn accept_cli_test_client(listener: &UnixListener, deadline: Instant) -> io::Result<UnixStream> {
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
                        "timed out waiting for CLI test client",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn unique_db(label: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("bowline-{label}-{}", unique_suffix()))
        .join("local.sqlite3")
}

fn unique_secret_store(label: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("bowline-{label}-{}", unique_suffix()))
        .join("secrets.v1")
}

fn unique_state_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("bowline-{label}-{}", unique_suffix()))
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .subsec_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn seed_two_project_events(db_path: &PathBuf) {
    seed_two_project_events_with_root(db_path, "~/Code");
}

fn seed_daemon_start_workspace(db_path: &Path, code_root: &Path) {
    seed_daemon_start_workspace_with_id(db_path, code_root, "ws_code");
}

fn seed_daemon_start_workspace_with_id(db_path: &Path, code_root: &Path, workspace_id: &str) {
    let workspace_id = WorkspaceId::new(workspace_id);
    let store = MetadataStore::open(db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-26T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-26T12:00:00Z",
        )
        .expect("root insert");
}

fn seed_two_project_events_with_root(db_path: &PathBuf, root_path: &str) {
    let workspace_id = WorkspaceId::new("ws_code");
    let web_id = ProjectId::new("proj_web");
    let backend_id = ProjectId::new("proj_backend");
    let store = MetadataStore::open(db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            root_path,
            "2026-06-23T12:00:00Z",
        )
        .expect("root insert");
    store
        .insert_project(
            &web_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-23T12:00:00Z",
        )
        .expect("web project insert");
    store
        .insert_project(
            &backend_id,
            &workspace_id,
            "root_code",
            "apps/backend",
            "2026-06-23T12:00:00Z",
        )
        .expect("backend project insert");
    store
        .append_event(project_event(
            "evt_web",
            &workspace_id,
            &web_id,
            "apps/web/src/index.ts",
            "Web event.",
        ))
        .expect("web event append");
    store
        .append_event(project_event(
            "evt_backend",
            &workspace_id,
            &backend_id,
            "apps/backend/src/main.rs",
            "Backend event.",
        ))
        .expect("backend event append");
}

fn project_event(
    id: &str,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    path: &str,
    summary: &str,
) -> WorkspaceEvent {
    let mut event = WorkspaceEvent::new(
        EventId::new(id),
        EventName::SourceStale,
        "2026-06-23T12:00:00Z",
        EventSeverity::Attention,
        summary,
        workspace_id.clone(),
    );
    event.project_id = Some(project_id.clone());
    event.path = Some(path.to_string());
    event
}

fn seed_workspace_for_work_views(db_path: &Path, code_root: &Path) {
    let mut store = MetadataStore::open(db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_cli_phase9");
    let project_id = ProjectId::new("proj_cli_web");
    store
        .insert_workspace(&workspace_id, "CLI Phase 9", "2026-06-25T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_cli_phase9",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T12:00:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_cli_phase9",
            "apps/web",
            "2026-06-25T12:00:00Z",
        )
        .expect("project");
    let _ = &mut store;
}

fn seed_additional_work_view_project(db_path: &Path, _code_root: &Path, path: &str) {
    let mut store = MetadataStore::open(db_path).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_cli_phase9");
    let project_id = ProjectId::new(format!("proj_cli_{}", path.replace('/', "_")));
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_cli_phase9",
            path,
            "2026-07-10T12:00:00Z",
        )
        .expect("additional project");
    let _ = &mut store;
}

fn parse_stdout_json(output: Output) -> Value {
    let stdout = String::from_utf8(output.stdout).expect("json should be utf8");
    serde_json::from_str(&stdout).expect("stdout should be json")
}

/// A scripted daemon socket for `work create` CLI-contract tests: it answers
/// the version handshake and `work.create` by mirroring the real daemon's
/// effect — materializing the project tree directly into the project-scoped
/// view directory — and returning a fixed base manifest key. Engine behavior is covered by the
/// bowline-daemon `work_views` tests; these tests pin the CLI's wire contract.
fn spawn_work_create_server(listener: UnixListener, creates: usize) -> thread::JoinHandle<()> {
    spawn_work_rpc_server(listener, creates)
}

/// A scripted daemon socket handling one work RPC per connection: `work.create`
/// materializes the view directory (mirroring the daemon's pull), `work.review`
/// reports one captured added file, `work.accept` reports a published merged
/// head. Fixed keys keep the CLI-side wire assertions deterministic.
fn spawn_work_rpc_server(listener: UnixListener, connections: usize) -> thread::JoinHandle<()> {
    use bowline_core::wire::generated::{
        DaemonClientHello, DaemonRpcRequest, DaemonRpcResponse, DaemonServerHello,
        MACHINE_CONTRACT_VERSION,
    };
    use bowline_daemon_rpc::{DAEMON_RPC_PROTOCOL_VERSION, FrameCodec};
    thread::spawn(move || {
        for _ in 0..connections {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut stream =
                accept_cli_test_client(&listener, deadline).expect("work RPC client connects");
            let codec = FrameCodec::default();
            codec.read_magic(&mut stream).expect("RPC magic reads");
            let _: DaemonClientHello = codec.read(&mut stream).expect("client hello reads");
            codec
                .write(
                    &mut stream,
                    &DaemonServerHello {
                        protocol_version: DAEMON_RPC_PROTOCOL_VERSION,
                        contract_version: MACHINE_CONTRACT_VERSION,
                        schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH.to_string(),
                        daemon_version: "test-daemon".to_string(),
                        capabilities: vec!["work.create".to_string()],
                        instance_id: "test-work-daemon".to_string(),
                    },
                )
                .expect("server hello writes");
            let request: DaemonRpcRequest = codec.read(&mut stream).expect("work request reads");
            assert!(
                matches!(
                    request.method.as_str(),
                    "work.create" | "work.review" | "work.accept"
                ),
                "unexpected work RPC method: {}",
                request.method
            );
            let result = match request.method.as_str() {
                "work.create" => {
                    let view_dir =
                        PathBuf::from(request.params["viewDir"].as_str().expect("viewDir param"));
                    materialize_fake_view(&view_dir);
                    serde_json::json!({ "baseManifestKey": "m_cli_contract_base" })
                }
                "work.review" => serde_json::json!({
                    "overlayManifestKey": "m_cli_contract_overlay",
                    "changes": [
                        { "path": "apps/web/src/feature.ts", "kind": "added" },
                    ],
                }),
                "work.accept" => serde_json::json!({
                    "overlayManifestKey": "m_cli_contract_overlay",
                    "baseManifestKey": "m_cli_contract_rebased",
                    "publishedManifestKey": "m_cli_contract_head2",
                    "conflictAsides": [],
                    "acceptedPaths": ["src/feature.ts"],
                }),
                _ => serde_json::Value::Null, // unreachable: asserted above
            };
            codec
                .write(
                    &mut stream,
                    &DaemonRpcResponse {
                        request_id: request.request_id,
                        result: Some(result),
                        error: None,
                    },
                )
                .expect("work response writes");
        }
    })
}

/// Mirror the daemon's project-scoped materialization effect.
fn materialize_fake_view(view_dir: &Path) {
    let components: Vec<std::ffi::OsString> =
        view_dir.iter().map(std::ffi::OsStr::to_os_string).collect();
    let work_index = components
        .iter()
        .rposition(|component| component == ".work")
        .expect("view dir lives under .work");
    let root: PathBuf = components[..work_index].iter().collect();
    let project_rel: PathBuf = components[work_index + 1..components.len() - 1]
        .iter()
        .collect();
    copy_tree(&root.join(&project_rel), view_dir);
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("copy target dir");
    for entry in fs::read_dir(source).expect("copy source dir") {
        let entry = entry.expect("copy source entry");
        let file_type = entry.file_type().expect("copy source file type");
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), &destination).expect("copy source file");
        }
    }
}
