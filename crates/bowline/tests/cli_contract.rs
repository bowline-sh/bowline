// Integration-test crate: long end-to-end scenario tests are expected here.
#![allow(clippy::too_many_lines)]

use std::fs;
use std::io::{self, BufRead, BufReader};
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
        MetadataStore, Platform, SyncOperationKind, SyncOperationRecord, SyncOperationState,
        SyncResourceKey, database_path_for_platform,
    },
    sync::{ConflictRecord, ConflictSpan},
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
#[path = "cli_contract/resolve.rs"]
mod resolve;
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
    // at ~104 bytes and $TMPDIR is already long; uniqueness comes from the
    // suffix.
    let _ = label;
    let dir = std::env::temp_dir().join(format!("bl-{}", unique_suffix()));
    fs::create_dir_all(&dir).expect("socket dir");
    dir.join("s.sock")
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
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id),
            state: SyncOperationState::WaitingRetry,
            idempotency_key: "watch-queue-retry".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device_cli_watch")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: Some("2026-06-26T12:01:00Z".to_string()),
            result_json: None,
            last_error_code: None,
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
    let mut record = ConflictRecord::same_path(affected_file);
    record.id = conflict_id.to_string();
    record.bundle_path = Some(bundle.to_path_buf());
    record.workspace_root = workspace_root.map(Path::to_path_buf);
    record.base_snapshot_id = Some("snap_fixture_base".to_string());
    record.remote_snapshot_id = Some("snap_fixture_remote".to_string());
    record.contains_secrets = contains_secrets;
    write_conflict_manifest(bundle, &record);
    if contains_secrets {
        for side in ["base", "local", "remote"] {
            let path = bundle.join(side).join(affected_file);
            fs::create_dir_all(path.parent().expect("conflict file parent"))
                .expect("conflict file parent directory");
            fs::write(path, b"API_TOKEN=SECRET_VALUE\n").expect("secret-bearing conflict side");
        }
    }
}

fn create_typed_conflict_bundle(bundle: &Path) {
    fs::create_dir_all(bundle.join("base").join("apps").join("web").join("src")).expect("base dir");
    fs::create_dir_all(bundle.join("local").join("apps").join("web").join("src"))
        .expect("local dir");
    fs::create_dir_all(bundle.join("remote").join("apps").join("web").join("src"))
        .expect("remote dir");
    fs::create_dir_all(bundle.join("resolution")).expect("resolution dir");
    let path = "apps/web/src/auth.ts";
    let mut record = ConflictRecord::same_path_span(
        path,
        ConflictSpan {
            path: path.to_string(),
            base_start_line: 14,
            base_end_line: 22,
            local_start_line: 14,
            local_end_line: 22,
            remote_start_line: 14,
            remote_end_line: 22,
            base_context_hash: None,
            local_context_hash: None,
            remote_context_hash: None,
        },
    );
    record.id = "conflict_typed_span".to_string();
    record.bundle_path = Some(bundle.to_path_buf());
    record.base_snapshot_id = Some("snap_fixture_base".to_string());
    record.remote_snapshot_id = Some("snap_fixture_remote".to_string());
    write_conflict_manifest(bundle, &record);
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
    let first_path = affected_files.first().expect("at least one conflict path");
    let mut record = ConflictRecord::same_path(first_path);
    record.id = conflict_id.to_string();
    record.paths = affected_files
        .iter()
        .map(|path| (*path).to_string())
        .collect();
    record.bundle_path = Some(bundle.to_path_buf());
    record.base_snapshot_id = Some("snap_fixture_base".to_string());
    record.remote_snapshot_id = Some("snap_fixture_remote".to_string());
    record.contains_secrets = contains_secrets;
    write_conflict_manifest(bundle, &record);
}

fn write_conflict_manifest(bundle: &Path, record: &ConflictRecord) {
    fs::write(
        bundle.join("manifest.json"),
        serde_json::to_vec_pretty(record).expect("conflict record serializes"),
    )
    .expect("manifest");
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
    bowline_testkit::persist_project_snapshot_fixture(
        &mut store,
        &workspace_id,
        &project_id,
        code_root,
        "apps/web",
        db_path.parent().expect("metadata state root"),
        "2026-06-25T12:00:00Z",
    );
}

fn seed_additional_work_view_project(db_path: &Path, code_root: &Path, path: &str) {
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
    bowline_testkit::persist_project_snapshot_fixture(
        &mut store,
        &workspace_id,
        &project_id,
        code_root,
        path,
        db_path.parent().expect("metadata state root"),
        "2026-07-10T12:00:00Z",
    );
}

fn parse_stdout_json(output: Output) -> Value {
    let stdout = String::from_utf8(output.stdout).expect("json should be utf8");
    serde_json::from_str(&stdout).expect("stdout should be json")
}
