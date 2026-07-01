use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bowline_core::{
    events::{EventName, EventSeverity, WorkspaceEvent},
    ids::{DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId},
};
use bowline_local::{
    metadata::{
        CommandIdempotencyRecord, MetadataStore, Platform, SyncOperationRecord,
        database_path_for_platform,
    },
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

fn run_bowline_without_env_in_dir(
    args: &[&str],
    removed_envs: &[&str],
    current_dir: &Path,
) -> Output {
    let mut command = bowline();
    command.args(args).current_dir(current_dir);
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
#[path = "cli_contract/resolve.rs"]
mod resolve;
#[path = "cli_contract/status_events.rs"]
mod status_events;

fn wait_for_daemon_status(socket: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let deadline = Instant::now() + Duration::from_secs(10);
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

fn read_line(stream: &mut impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) if one[0] == b'\n' => break,
            Ok(_) => bytes.push(one[0]),
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn unique_socket(label: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/bowline-{label}-{}.sock", unique_suffix()))
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

fn seed_daemon_component_status(db_path: &Path) {
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-26T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-26T12:00:00Z")
        .expect("root insert");
    store
        .set_component_state("sync", "degraded", "2026-06-26T12:00:01Z")
        .expect("sync state");
    store
        .set_component_state("watcher", "unavailable", "2026-06-26T12:00:01Z")
        .expect("watcher state");
    store
        .set_component_state("network", "offline", "2026-06-26T12:00:01Z")
        .expect("network state");
}

fn seed_sync_queue_workspace(db_path: &Path) {
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-26T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-26T12:00:00Z")
        .expect("root insert");
}

fn enqueue_sync_queue_watch_change(db_path: &Path) {
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(db_path).expect("metadata opens");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "watch_queue_retry".to_string(),
            workspace_id,
            kind: "daemon-reconcile".to_string(),
            state: "waiting_retry".to_string(),
            idempotency_key: "watch-queue-retry".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device_cli_watch")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: Some("2026-06-26T12:01:00Z".to_string()),
            last_error: Some("retry later".to_string()),
            created_at: "2026-06-26T12:00:01Z".to_string(),
            updated_at: "2026-06-26T12:00:01Z".to_string(),
        })
        .expect("sync operation enqueue");
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

fn create_conflict_bundle(bundle: &Path, contains_secrets: bool) {
    create_conflict_bundle_with_id(
        bundle,
        "conflict_same_line",
        "apps/web/.env.local",
        contains_secrets,
    );
}

fn create_conflict_bundle_with_workspace_root(
    bundle: &Path,
    contains_secrets: bool,
    workspace_root: &Path,
) {
    create_conflict_bundle_manifest(
        bundle,
        "conflict_same_line",
        "apps/web/.env.local",
        contains_secrets,
        Some(workspace_root),
    );
}

fn create_conflict_bundle_with_id(
    bundle: &Path,
    conflict_id: &str,
    affected_file: &str,
    contains_secrets: bool,
) {
    create_conflict_bundle_manifest(bundle, conflict_id, affected_file, contains_secrets, None);
}

fn create_conflict_bundle_manifest(
    bundle: &Path,
    conflict_id: &str,
    affected_file: &str,
    contains_secrets: bool,
    workspace_root: Option<&Path>,
) {
    fs::create_dir_all(bundle.join("base").join("apps").join("web")).expect("base dir");
    fs::create_dir_all(bundle.join("local").join("apps").join("web")).expect("local dir");
    fs::create_dir_all(bundle.join("remote").join("apps").join("web")).expect("remote dir");
    fs::create_dir_all(bundle.join("resolution")).expect("resolution dir");
    let workspace_root_field = workspace_root
        .map(|root| {
            format!(
                ",\"workspaceRoot\":{}",
                json_string(&root.display().to_string())
            )
        })
        .unwrap_or_default();
    fs::write(
        bundle.join("manifest.json"),
        format!(
            "{{\"conflictId\":\"{conflict_id}\",\"affectedFiles\":[\"{affected_file}\"],\"activeView\":\"local\",\"containsSecrets\":{contains_secrets},\"ignoredValue\":\"SECRET_VALUE\"{workspace_root_field}}}"
        ),
    )
    .expect("manifest");
}

fn create_typed_conflict_bundle(bundle: &Path) {
    fs::create_dir_all(bundle.join("base").join("apps").join("web").join("src")).expect("base dir");
    fs::create_dir_all(bundle.join("local").join("apps").join("web").join("src"))
        .expect("local dir");
    fs::create_dir_all(bundle.join("remote").join("apps").join("web").join("src"))
        .expect("remote dir");
    fs::create_dir_all(bundle.join("resolution")).expect("resolution dir");
    fs::write(
        bundle.join("manifest.json"),
        r#"{
  "conflictId": "conflict_typed_span",
  "conflictKind": "text",
  "affectedFiles": ["apps/web/src/auth.ts"],
  "activeView": "local",
  "containsSecrets": false,
  "state": "unresolved",
  "spans": [
    {
      "path": "apps/web/src/auth.ts",
      "baseStartLine": 14,
      "baseEndLine": 22,
      "localStartLine": 14,
      "localEndLine": 22,
      "remoteStartLine": 14,
      "remoteEndLine": 22
    }
  ]
}"#,
    )
    .expect("manifest");
}

fn create_conflict_bundle_with_paths(
    bundle: &Path,
    conflict_id: &str,
    affected_files: &[&str],
    contains_secrets: bool,
) {
    fs::create_dir_all(bundle.join("base").join("apps").join("web")).expect("base dir");
    fs::create_dir_all(bundle.join("local").join("apps").join("web")).expect("local dir");
    fs::create_dir_all(bundle.join("remote").join("apps").join("web")).expect("remote dir");
    fs::create_dir_all(bundle.join("resolution")).expect("resolution dir");
    let affected = affected_files
        .iter()
        .map(|path| json_string(path))
        .collect::<Vec<_>>()
        .join(",");
    fs::write(
        bundle.join("manifest.json"),
        format!(
            "{{\"conflictId\":\"{conflict_id}\",\"affectedFiles\":[{affected}],\"activeView\":\"local\",\"containsSecrets\":{contains_secrets},\"ignoredValue\":\"SECRET_VALUE\"}}"
        ),
    )
    .expect("manifest");
}

fn seed_workspace_for_work_views(db_path: &Path, code_root: &Path) {
    let store = MetadataStore::open(db_path).expect("metadata opens");
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
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &project_id,
            &SnapshotId::new("snap_cli_phase9_base"),
        )
        .expect("project latest snapshot");
}

fn parse_stdout_json(output: Output) -> Value {
    let stdout = String::from_utf8(output.stdout).expect("json should be utf8");
    serde_json::from_str(&stdout).expect("stdout should be json")
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
