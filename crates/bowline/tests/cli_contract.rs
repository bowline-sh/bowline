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

#[test]
fn help_groups_commands_by_intent() {
    let output = run_bowline(&["help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");
    assert_eq!(stdout, include_str!("../../../tests/golden/cli/help.txt"));
    assert!(stdout.contains("Workspace:"));
    assert!(stdout.contains("bowline resolve [path] [--tui|--copy-prompt|--diff <conflict>]"));
    assert!(stdout.contains("bowline tui [path]"));
    assert!(stdout.contains("Trust:"));
    assert!(stdout.contains("Work:"));
    assert!(stdout.contains("Agent:"));
    assert!(stdout.contains("Daemon:"));
    assert!(stdout.contains("Support:"));
    assert!(stdout.contains("bowline diagnostics collect"));
}

#[test]
fn discovery_commands_emit_machine_contracts() {
    let version = run_bowline(&["version", "--json"]);
    assert!(version.status.success());
    let version_json = parse_stdout_json(version);
    assert_eq!(version_json["command"], "version");
    assert_eq!(version_json["contractVersion"], 3);
    assert_eq!(version_json["protocol"], "bowline.local");

    let short_version = run_bowline(&["--version"]);
    assert!(short_version.status.success());
    let short_version_stdout =
        String::from_utf8(short_version.stdout).expect("version stdout is utf8");
    assert!(short_version_stdout.starts_with("bowline "));

    let contract = run_bowline(&["contract", "--json"]);
    assert!(contract.status.success());
    let contract_json = parse_stdout_json(contract);
    assert_eq!(contract_json["command"], "contract");
    assert_eq!(
        contract_json["packageContractSource"],
        "packages/contracts/src/index.ts"
    );
    assert!(
        contract_json["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .any(|command| command["name"] == "search"
                && command["boundedOutput"]["cursorFormat"] == "v1:<offset>")
    );

    let schema = run_bowline(&["schema", "--json"]);
    assert!(schema.status.success());
    let schema_json = parse_stdout_json(schema);
    assert_eq!(schema_json["command"], "contract");
}

#[test]
fn topic_help_json_works_for_global_and_nested_commands() {
    for args in [
        &["status", "--help", "--json"][..],
        &["help", "status", "--json"][..],
        &["agent", "start", "--help", "--json"][..],
        &["daemon", "install", "--help", "--json"][..],
    ] {
        let output = run_bowline(args);
        assert!(output.status.success(), "{args:?}");
        let json = parse_stdout_json(output);
        assert_eq!(json["command"], "help");
        assert_eq!(json["commands"].as_array().expect("commands").len(), 1);
        let command_name = json["commands"][0]["name"].as_str().expect("command name");
        let groups = json["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0]["commands"]
                .as_array()
                .expect("group commands")
                .len(),
            1
        );
        assert_eq!(groups[0]["commands"][0], command_name);
    }
}

#[test]
fn unknown_command_json_uses_command_error_output() {
    let output = run_bowline(&["nope", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 3);
    assert_eq!(json["command"], "unknown");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "unknown_command");
}

#[test]
fn known_command_usage_errors_keep_command_name() {
    let output = run_bowline(&["events", "--limit", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "events");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "usage_error");
}

#[test]
fn dry_run_does_not_mask_parsed_usage_errors() {
    let output = run_bowline(&["workon", "--dry-run", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "workon");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "usage_error");
    assert_ne!(json["error"]["code"], "dry_run_unsupported");
}

#[test]
fn exploration_commands_reject_unbounded_or_malformed_controls() {
    let search = run_bowline(&["search", "needle", "--limit", "101", "--json"]);
    assert_eq!(search.status.code(), Some(2));
    let search_json = parse_stdout_json(search);
    assert_eq!(search_json["command"], "search");
    assert_eq!(search_json["status"], "usage-error");

    let symbols = run_bowline(&["symbols", "Target", "--cursor", "bad", "--json"]);
    assert_eq!(symbols.status.code(), Some(2));
    let symbols_json = parse_stdout_json(symbols);
    assert_eq!(symbols_json["command"], "symbols");
    assert_eq!(symbols_json["error"]["code"], "usage_error");

    let huge_cursor = run_bowline(&["search", "needle", "--cursor", "v1:1000000000", "--json"]);
    assert_eq!(huge_cursor.status.code(), Some(2));
    let huge_cursor_json = parse_stdout_json(huge_cursor);
    assert_eq!(huge_cursor_json["command"], "search");
    assert_eq!(huge_cursor_json["error"]["code"], "usage_error");
}

#[test]
fn recovery_verify_accepts_advertised_dry_run_contract() {
    let output = run_bowline(&["recover", "verify", "rk_123", "--dry-run", "--json"]);

    assert!(output.status.success(), "{output:?}");
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "recover");
    assert_eq!(json["status"], "dry-run");
    assert_eq!(json["target"], "rk_123");
    assert!(
        json["applyCommand"]
            .as_str()
            .expect("applyCommand string")
            .contains("recover verify rk_123")
    );

    let dry_with_idempotency = run_bowline(&[
        "recover",
        "verify",
        "rk_123",
        "--dry-run",
        "--idempotency-key",
        "recovery-key",
        "--json",
    ]);
    assert!(
        dry_with_idempotency.status.success(),
        "{dry_with_idempotency:?}"
    );
    let dry_with_idempotency_json = parse_stdout_json(dry_with_idempotency);
    let apply_command = dry_with_idempotency_json["applyCommand"]
        .as_str()
        .expect("applyCommand string");
    assert!(apply_command.contains("recover verify rk_123"));
    assert!(!apply_command.contains("--idempotency-key"));
    assert!(
        dry_with_idempotency_json["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains("Omitted --idempotency-key")))
    );

    let idempotent = run_bowline(&[
        "recover",
        "verify",
        "rk_123",
        "--idempotency-key",
        "recovery-key",
        "--json",
    ]);
    assert_eq!(idempotent.status.code(), Some(2));
    let idempotent_json = parse_stdout_json(idempotent);
    assert_eq!(idempotent_json["command"], "recover");
    assert_eq!(idempotent_json["error"]["code"], "idempotency_unsupported");
}

#[test]
fn workon_dry_run_and_idempotency_are_replay_safe() {
    let temp = TempWorkspace::new("cli-agent-use-workon").expect("temp workspace");
    let raw_code_root = temp.root().join("Code");
    let raw_project_path = raw_code_root.join("apps/web");
    fs::create_dir_all(raw_project_path.join("src")).expect("project src");
    fs::write(raw_project_path.join("src/index.ts"), "console.log('base')").expect("source");
    let code_root = raw_code_root.canonicalize().expect("canonical code root");
    let project_path = raw_project_path
        .canonicalize()
        .expect("canonical project path");
    let db_path = temp.root().join(".state/local.sqlite3");
    seed_workspace_for_work_views(&db_path, &code_root);
    let project_arg = project_path.display().to_string();
    let envs = [
        ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ("BOWLINE_GENERATED_AT", "2026-06-29T12:00:00Z".to_string()),
        ("BOWLINE_DEVICE_ID", "dev_cli_agent_use".to_string()),
    ];

    let dry_run = run_bowline_with_env(
        &["workon", &project_arg, "dry", "--dry-run", "--json"],
        &envs,
    );
    assert!(dry_run.status.success(), "{dry_run:?}");
    let dry_json = parse_stdout_json(dry_run);
    assert_eq!(dry_json["command"], "workon");
    assert_eq!(dry_json["status"], "dry-run");
    assert!(!code_root.join(".work/apps/web/dry").exists());

    let dry_socket_path = unique_socket("dry-run-socket");
    let dry_with_socket = run_bowline_with_env(
        &[
            "--socket",
            &dry_socket_path.display().to_string(),
            "workon",
            &project_arg,
            "dry-socket",
            "--dry-run",
            "--idempotency-key",
            "dry-key",
            "--json",
        ],
        &envs,
    );
    assert!(dry_with_socket.status.success(), "{dry_with_socket:?}");
    let dry_with_socket_json = parse_stdout_json(dry_with_socket);
    let apply_command = dry_with_socket_json["applyCommand"]
        .as_str()
        .expect("applyCommand is a string");
    assert!(apply_command.contains("--socket"));
    assert!(apply_command.contains(&dry_socket_path.display().to_string()));
    assert!(apply_command.contains("--json"));
    assert!(apply_command.contains("--idempotency-key"));
    assert!(apply_command.contains("dry-key"));

    let first = run_bowline_with_env(
        &[
            "workon",
            &project_arg,
            "idem",
            "--idempotency-key",
            "workon-key",
            "--json",
        ],
        &envs,
    );
    assert!(first.status.success(), "{first:?}");
    let first_json = parse_stdout_json(first);
    assert_eq!(first_json["command"], "workon");
    assert_eq!(first_json.get("replayed"), None);

    let replay = run_bowline_with_env(
        &[
            "workon",
            &project_arg,
            "idem",
            "--idempotency-key",
            "workon-key",
            "--json",
        ],
        &envs,
    );
    assert!(replay.status.success(), "{replay:?}");
    let replay_json = parse_stdout_json(replay);
    assert_eq!(
        replay_json["workView"]["name"],
        first_json["workView"]["name"]
    );
    assert_eq!(replay_json["replayed"], true);

    let cleanup_preview = run_bowline_with_env(
        &[
            "cleanup",
            "--idempotency-key",
            "cleanup-preview-key",
            "--json",
        ],
        &envs,
    );
    assert!(cleanup_preview.status.success(), "{cleanup_preview:?}");
    let cleanup_preview_json = parse_stdout_json(cleanup_preview);
    assert_eq!(cleanup_preview_json["command"], "cleanup");
    let cleanup_replay = run_bowline_with_env(
        &[
            "cleanup",
            "--idempotency-key",
            "cleanup-preview-key",
            "--json",
        ],
        &envs,
    );
    assert!(cleanup_replay.status.success(), "{cleanup_replay:?}");
    let cleanup_replay_json = parse_stdout_json(cleanup_replay);
    assert_eq!(cleanup_replay_json["replayed"], true);

    let replay_from_other_cwd = run_bowline_with_env_in_dir(
        &[
            "workon",
            &project_arg,
            "idem",
            "--idempotency-key",
            "workon-key",
            "--json",
        ],
        &envs,
        temp.root(),
    );
    assert!(
        replay_from_other_cwd.status.success(),
        "{replay_from_other_cwd:?}"
    );
    let replay_from_other_cwd_json = parse_stdout_json(replay_from_other_cwd);
    assert_eq!(replay_from_other_cwd_json["replayed"], true);

    let socket_conflict_path = unique_socket("idem-socket");
    let socket_conflict = run_bowline_with_env(
        &[
            "--socket",
            &socket_conflict_path.display().to_string(),
            "workon",
            &project_arg,
            "idem",
            "--idempotency-key",
            "workon-key",
            "--json",
        ],
        &envs,
    );
    assert_eq!(socket_conflict.status.code(), Some(2));
    let socket_conflict_json = parse_stdout_json(socket_conflict);
    assert_eq!(
        socket_conflict_json["error"]["code"],
        "idempotency_conflict"
    );

    let relative_socket_first = run_bowline_with_env_in_dir(
        &[
            "--socket",
            "bowline-relative.sock",
            "workon",
            &project_arg,
            "relative-socket",
            "--idempotency-key",
            "socket-cwd-key",
            "--json",
        ],
        &envs,
        &code_root,
    );
    assert!(
        relative_socket_first.status.success(),
        "{relative_socket_first:?}"
    );
    let relative_socket_conflict = run_bowline_with_env_in_dir(
        &[
            "--socket",
            "bowline-relative.sock",
            "workon",
            &project_arg,
            "relative-socket",
            "--idempotency-key",
            "socket-cwd-key",
            "--json",
        ],
        &envs,
        temp.root(),
    );
    assert_eq!(relative_socket_conflict.status.code(), Some(2));
    let relative_socket_conflict_json = parse_stdout_json(relative_socket_conflict);
    assert_eq!(
        relative_socket_conflict_json["error"]["code"],
        "idempotency_conflict"
    );

    let relative_first = run_bowline_with_env_in_dir(
        &[
            "workon",
            "apps/web",
            "relative",
            "--idempotency-key",
            "cwd-key",
            "--json",
        ],
        &envs,
        &code_root,
    );
    assert!(relative_first.status.success(), "{relative_first:?}");
    let relative_conflict = run_bowline_with_env_in_dir(
        &[
            "workon",
            "apps/web",
            "relative",
            "--idempotency-key",
            "cwd-key",
            "--json",
        ],
        &envs,
        temp.root(),
    );
    assert_eq!(relative_conflict.status.code(), Some(2));
    let relative_conflict_json = parse_stdout_json(relative_conflict);
    assert_eq!(
        relative_conflict_json["error"]["code"],
        "idempotency_conflict"
    );

    let conflict = run_bowline_with_env(
        &[
            "workon",
            &project_arg,
            "different",
            "--idempotency-key",
            "workon-key",
            "--json",
        ],
        &envs,
    );
    assert_eq!(conflict.status.code(), Some(2));
    let conflict_json = parse_stdout_json(conflict);
    assert_eq!(conflict_json["error"]["code"], "idempotency_conflict");

    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .try_insert_command_idempotency_record(&CommandIdempotencyRecord {
            workspace_id: WorkspaceId::new("ws_cli_phase9"),
            idempotency_key: "stale-key".to_string(),
            command: "workon".to_string(),
            request_hash: "stale-old-request".to_string(),
            result_json: "{}".to_string(),
            status: "pending".to_string(),
            created_at: "2026-06-01T12:00:00Z".to_string(),
            updated_at: "2026-06-01T12:00:00Z".to_string(),
            expires_at: "2026-06-08T12:00:00Z".to_string(),
        })
        .expect("stale reservation insert");
    let reclaimed = run_bowline_with_env(
        &[
            "workon",
            &project_arg,
            "stale",
            "--idempotency-key",
            "stale-key",
            "--json",
        ],
        &envs,
    );
    assert!(reclaimed.status.success(), "{reclaimed:?}");
    let reclaimed_json = parse_stdout_json(reclaimed);
    assert_eq!(reclaimed_json["command"], "workon");
    assert_eq!(reclaimed_json["workView"]["name"], "stale");
}

#[test]
fn status_json_reports_missing_metadata_without_creating_db() {
    let db_path = unique_db("missing-status");
    let output = run_bowline_with_env(
        &["status", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    assert!(!db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "status");
    assert_eq!(json["status"]["level"], "attention");
    assert_eq!(
        json["nextActions"][0]["label"],
        "Initialize ~/Code when ready"
    );
    assert!(json["nextActions"][0].get("command").is_none());
}

#[test]
fn init_json_creates_explicit_missing_root_without_project_files() {
    let temp = TempWorkspace::new("cli-init-missing-root").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let code_root = home.join("Code");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["init", code_root.to_str().expect("code root"), "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(code_root.is_dir());
    assert!(!code_root.join(".bowlineignore").exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "init");
    assert_eq!(json["root"], "~/Code");
    assert_eq!(json["rootChoice"], "explicit-created");
    assert_eq!(json["createdRoot"], true);
    assert_eq!(json["changedWorkspaceFiles"], false);
}

#[test]
fn login_root_no_poll_json_prepares_workspace_root() {
    let temp = TempWorkspace::new("cli-login-root-json").expect("temp workspace");
    let home = temp.root().join("home");
    let code_root = home.join("Code");
    fs::create_dir_all(&home).expect("home");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &[
            "login",
            "--root",
            code_root.to_str().expect("utf8 root"),
            "--no-poll",
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-27T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(code_root.is_dir());
    assert!(db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "login");
    assert_eq!(json["root"], "~/Code");
    assert_eq!(json["rootChoice"], "explicit-created");
}

#[test]
fn login_root_json_reports_workspace_errors_as_json() {
    let temp = TempWorkspace::new("cli-login-root-json-error").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let root_file = home.join("not-a-dir");
    fs::write(&root_file, "not a directory").expect("root file");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &[
            "login",
            "--root",
            root_file.to_str().expect("utf8 root"),
            "--no-poll",
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-27T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "login");
    assert_eq!(json["status"], "failed");
}

#[test]
fn bare_init_json_creates_code_when_no_likely_roots_exist() {
    let temp = TempWorkspace::new("cli-init-default-code").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["init", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(home.join("Code").is_dir());
    let json = parse_stdout_json(output);
    assert_eq!(json["root"], "~/Code");
    assert_eq!(json["rootChoice"], "default-selected");
    assert_eq!(json["createdRoot"], true);
}

#[test]
fn bare_init_json_rejects_existing_non_code_root_as_choice_needed() {
    let temp = TempWorkspace::new("cli-init-ambiguous-root").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(home.join("Projects")).expect("projects root");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["init", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(!home.join("Code").exists());
    assert!(!db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 3);
    assert_eq!(json["command"], "init");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "ambiguous_root");
    assert_eq!(json["error"]["recoverability"], "user-action");
    assert_eq!(
        json["nextActions"][0]["command"],
        "bowline login --root ~/Projects"
    );
}

#[test]
fn bare_init_json_rejects_code_plus_other_roots_as_choice_needed() {
    let temp = TempWorkspace::new("cli-init-code-plus-projects").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(home.join("Code")).expect("code root");
    fs::create_dir_all(home.join("Projects")).expect("projects root");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["init", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(!db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["error"]["code"], "ambiguous_root");
    assert_eq!(
        json["nextActions"][0]["command"],
        "bowline login --root ~/Code"
    );
    assert_eq!(
        json["nextActions"][1]["command"],
        "bowline login --root ~/Projects"
    );
}

#[test]
fn explain_json_usage_errors_use_command_contract() {
    let output = run_bowline(&["explain", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 3);
    assert_eq!(json["command"], "explain");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["recoverability"], "user-action");
    assert_eq!(json["nextActions"][0]["command"], "bowline explain <path>");
}

#[test]
fn work_view_cli_creates_lists_restores_and_cleans_without_copying_source() {
    let temp = TempWorkspace::new("cli-phase-9-work").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let project_path = code_root.join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source");
    let db_path = temp.root().join(".state/local.sqlite3");
    seed_workspace_for_work_views(&db_path, &code_root);
    let project_arg = project_path.display().to_string();
    let envs = [
        ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ("BOWLINE_GENERATED_AT", "2026-06-25T12:00:00Z".to_string()),
        ("BOWLINE_DEVICE_ID", "dev_cli_phase9".to_string()),
    ];

    let created = run_bowline_with_env(&["workon", &project_arg, "auth-fix", "--json"], &envs);
    assert!(created.status.success(), "{created:?}");
    let created_json = parse_stdout_json(created);
    assert_eq!(created_json["command"], "workon");
    assert_eq!(created_json["workView"]["name"], "auth-fix");
    let materialized = code_root.join(".work/apps/web/auth-fix");
    assert!(materialized.is_dir());
    assert!(materialized.join("src/index.ts").exists());

    let listed = run_bowline_with_env(&["work", "--json"], &envs);
    assert!(listed.status.success());
    let listed_json = parse_stdout_json(listed);
    assert_eq!(listed_json["workViews"].as_array().unwrap().len(), 1);

    let discarded = run_bowline_with_env(&["discard", "auth-fix", "--json"], &envs);
    assert!(discarded.status.success());
    let discarded_json = parse_stdout_json(discarded);
    assert_eq!(discarded_json["workView"]["lifecycle"], "discarded");

    let hidden_list = run_bowline_with_env(&["work", "--json"], &envs);
    assert!(hidden_list.status.success());
    let hidden_json = parse_stdout_json(hidden_list);
    assert!(hidden_json["workViews"].as_array().unwrap().is_empty());

    let restored = run_bowline_with_env(&["restore", "auth-fix", "--json"], &envs);
    assert!(restored.status.success());
    let restored_json = parse_stdout_json(restored);
    assert_eq!(restored_json["workView"]["lifecycle"], "active");

    let discarded = run_bowline_with_env(&["discard", "auth-fix", "--json"], &envs);
    assert!(discarded.status.success());
    let preview = run_bowline_with_env(&["cleanup", "--json"], &envs);
    assert!(preview.status.success());
    assert!(materialized.is_dir());

    let cleanup = run_bowline_with_env(&["cleanup", "--apply", "--json"], &envs);
    assert!(cleanup.status.success());
    let cleanup_json = parse_stdout_json(cleanup);
    assert_eq!(cleanup_json["deletedPaths"].as_array().unwrap().len(), 1);
    assert!(!materialized.exists());
}

#[test]
fn work_view_cli_uses_default_metadata_path_without_env_override() {
    let temp = TempWorkspace::new("cli-phase-9-default-db").expect("temp workspace");
    let home = temp.root().join("home");
    let code_root = home.join("Code");
    let project_path = code_root.join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source");

    let xdg_state_home = home.join(".local/state");
    let platform = if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    };
    let db_path = database_path_for_platform(platform, &home, Some(&xdg_state_home));
    seed_workspace_for_work_views(&db_path, &code_root);

    let project_arg = project_path.display().to_string();
    let envs = [
        ("HOME", home.display().to_string()),
        ("XDG_STATE_HOME", xdg_state_home.display().to_string()),
        ("BOWLINE_GENERATED_AT", "2026-06-25T12:00:00Z".to_string()),
        ("BOWLINE_DEVICE_ID", "dev_cli_default_db".to_string()),
    ];

    let created = run_bowline_with_env_removed(
        &["workon", &project_arg, "default-db", "--json"],
        &envs,
        &["BOWLINE_METADATA_DB"],
    );
    assert!(created.status.success(), "{created:?}");
    let created_json = parse_stdout_json(created);
    assert_eq!(created_json["command"], "workon");
    assert_eq!(created_json["workView"]["name"], "default-db");
    assert!(code_root.join(".work/apps/web/default-db").is_dir());

    let listed = run_bowline_with_env_removed(&["work", "--json"], &envs, &["BOWLINE_METADATA_DB"]);
    assert!(listed.status.success());
    let listed_json = parse_stdout_json(listed);
    assert_eq!(listed_json["workViews"].as_array().unwrap().len(), 1);
}

#[test]
fn init_json_rejects_unknown_single_flag_without_creating_root() {
    let temp = TempWorkspace::new("cli-init-unknown-flag").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["init", "--dry-run", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(!temp.root().join("--dry-run").exists());
    assert!(!db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["command"], "init");
    assert_eq!(json["error"]["code"], "dry_run_unsupported");
    assert_eq!(
        json["error"]["message"],
        "--dry-run is not supported for this command"
    );
}

#[test]
fn explain_json_rejects_unknown_single_flag_as_usage_error() {
    let output = run_bowline(&["explain", "--bad-option", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"], "usage-error");
    assert_eq!(
        json["error"]["message"],
        "unknown bowline explain option `--bad-option`"
    );
}

#[test]
fn init_status_and_explain_observe_existing_code_root() {
    let temp = TempWorkspace::new("cli-phase-2").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let web_dir = code_root.join("apps").join("web");
    fs::create_dir_all(web_dir.join("node_modules").join("react")).expect("node_modules");
    fs::write(web_dir.join("package.json"), b"{}").expect("package json");
    fs::write(web_dir.join(".env.local"), b"API_KEY=value\n").expect("env file");
    fs::create_dir_all(web_dir.join(".git").join("refs").join("heads")).expect("git dirs");
    fs::create_dir_all(
        web_dir
            .join(".git")
            .join("refs")
            .join("remotes")
            .join("origin"),
    )
    .expect("git remote dirs");
    fs::write(web_dir.join(".git").join("HEAD"), b"ref: refs/heads/main\n").expect("git head");
    fs::write(
        web_dir.join(".git").join("config"),
        b"[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n",
    )
    .expect("git config");
    fs::write(
        web_dir.join(".git").join("refs").join("heads").join("main"),
        b"aaaaaaaa\n",
    )
    .expect("local branch ref");
    fs::write(
        web_dir
            .join(".git")
            .join("refs")
            .join("remotes")
            .join("origin")
            .join("main"),
        b"bbbbbbbb\n",
    )
    .expect("remote tracking ref");
    let detector = WorkspaceMutationDetector::new(&code_root).expect("mutation detector");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let init = run_bowline_with_env(
        &["init", code_root.to_str().expect("code root"), "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(init.status.success());
    detector.assert_unchanged().expect("source tree unchanged");
    let init_json = parse_stdout_json(init);
    assert_eq!(init_json["command"], "init");
    assert_eq!(init_json["rootChoice"], "explicit-existing");
    assert_eq!(init_json["observedOnly"], true);
    assert_eq!(init_json["changedWorkspaceFiles"], false);
    assert_eq!(init_json["scanSummary"]["repoCount"], 1);
    assert_eq!(init_json["scanSummary"]["noRemoteRepoCount"], 0);
    assert_eq!(init_json["scanSummary"]["staleRemoteTrackingRepoCount"], 1);

    let status = run_bowline_with_env(
        &["status", code_root.to_str().expect("code root"), "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );
    assert!(status.status.success());
    let status_json = parse_stdout_json(status);
    assert_eq!(status_json["status"]["level"], "attention");
    assert_eq!(
        status_json["workspaceSummary"]["observed"]["staleRemoteTrackingRepoCount"],
        1
    );
    let status_text = serde_json::to_string(&status_json).expect("status json string");
    assert!(status_text.contains("Git observer is advisory"));
    assert!(status_text.contains("never fetches, commits, or uses Git as sync"));
    assert!(status_text.contains("local remote-tracking refs"));

    let explain = run_bowline_with_env(
        &[
            "explain",
            web_dir.join(".env.local").to_str().expect("env path"),
            "--json",
        ],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:01Z".to_string()),
        ],
    );
    assert!(explain.status.success());
    let explain_json = parse_stdout_json(explain);
    assert_eq!(explain_json["command"], "explain");
    assert_eq!(explain_json["mode"], "project-env");
    assert_eq!(explain_json["observedState"], "observed");
    assert!(
        !explain_json["summary"]
            .as_str()
            .expect("summary")
            .contains("API_KEY")
    );
}

#[test]
fn phase8_env_and_setup_prewarm_do_not_leak_or_sync_generated_state() {
    let temp = TempWorkspace::new("cli-phase-8").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let web_dir = code_root.join("apps").join("web");
    fs::create_dir_all(&web_dir).expect("web dir");
    fs::write(web_dir.join("package.json"), br#"{"name":"web"}"#).expect("package json");
    fs::write(
        web_dir.join(".env.local"),
        b"API_KEY=super-secret-value\nPUBLIC_URL=http://localhost:3000\n",
    )
    .expect("env file");
    fs::write(
        web_dir.join(".bowlinesetup"),
        "printf setup-complete > .setup-done\nmkdir -p node_modules/react\n",
    )
    .expect("setup recipe");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let init = run_bowline_with_env(
        &["init", code_root.to_str().expect("code root"), "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-25T12:00:00Z".to_string()),
        ],
    );
    assert!(init.status.success());
    let init_stdout = String::from_utf8(init.stdout).expect("init stdout");
    assert!(!init_stdout.contains("super-secret-value"));

    let status = run_bowline_with_env(
        &["status", web_dir.to_str().expect("web dir"), "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );
    assert!(status.status.success());
    let status_json = parse_stdout_json(status);
    let status_text = serde_json::to_string(&status_json).expect("status json serializes");
    assert!(status_text.contains("\"kind\":\"env\""));
    assert!(status_text.contains("values are redacted"));
    assert!(!status_text.contains("super-secret-value"));

    let blocked = run_bowline_with_env(
        &["prewarm", web_dir.to_str().expect("web dir"), "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-25T12:00:01Z".to_string()),
        ],
    );
    assert!(blocked.status.success());
    let blocked_json = parse_stdout_json(blocked);
    assert_eq!(blocked_json["command"], "prewarm");
    assert_eq!(blocked_json["outcome"]["state"], "setup-blocked");
    assert!(!web_dir.join(".setup-done").exists());

    let approved = run_bowline_with_env(
        &[
            "prewarm",
            web_dir.to_str().expect("web dir"),
            "--approve-setup",
            "--json",
        ],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-25T12:00:02Z".to_string()),
        ],
    );
    assert!(approved.status.success());
    let approved_json = parse_stdout_json(approved);
    assert_eq!(approved_json["outcome"]["state"], "hot");
    assert!(web_dir.join(".setup-done").exists());
    assert!(web_dir.join("node_modules").join("react").is_dir());
    assert!(
        !serde_json::to_string(&approved_json)
            .expect("approved serializes")
            .contains("super-secret-value")
    );

    let final_status = run_bowline_with_env(
        &["status", web_dir.to_str().expect("web dir"), "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );
    assert!(final_status.status.success());
    let final_json = parse_stdout_json(final_status);
    let final_text = serde_json::to_string(&final_json).expect("status serializes");
    assert!(final_text.contains("\"kind\":\"setup\""));
    assert!(!final_text.contains("super-secret-value"));
}

#[test]
fn status_watch_json_emits_initial_frame() {
    let db_path = unique_db("watch-status");
    let mut child = bowline()
        .args(["status", "--watch", "--json"])
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .stdout(Stdio::piped())
        .spawn()
        .expect("bowline status watch should start");
    let stdout = child.stdout.take().expect("watch stdout should be piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("initial watch frame should be readable");

    assert!(
        child.try_wait().expect("watch child status").is_none(),
        "watch should keep streaming after the initial frame"
    );
    let _ = child.kill();
    let _ = child.wait();

    let json: Value = serde_json::from_str(&line).expect("watch frame should be json");
    assert_eq!(json["type"], "status");
    assert_eq!(json["sequence"], 1);
    assert_eq!(json["status"]["command"], "status");
}

#[test]
fn status_watch_json_emits_sync_queue_change_frame() {
    let db_path = unique_db("watch-status-sync-queue");
    seed_sync_queue_workspace(&db_path);
    let mut child = bowline()
        .args(["status", "--watch", "--json"])
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .stdout(Stdio::piped())
        .spawn()
        .expect("bowline status watch should start");
    let stdout = child.stdout.take().expect("watch stdout should be piped");
    let mut reader = BufReader::new(stdout);
    let mut initial = String::new();
    reader
        .read_line(&mut initial)
        .expect("initial watch frame should be readable");

    enqueue_sync_queue_watch_change(&db_path);

    let mut changed = String::new();
    reader
        .read_line(&mut changed)
        .expect("changed watch frame should be readable");
    let _ = child.kill();
    let _ = child.wait();

    let json: Value = serde_json::from_str(&changed).expect("watch frame should be json");
    assert_eq!(json["type"], "status");
    assert_eq!(json["sequence"], 2);
    assert_eq!(json["status"]["syncQueue"]["waitingRetry"], 1);
    assert_eq!(
        json["status"]["status"]["attentionItems"],
        serde_json::json!(["Sync queue is waiting for retry."])
    );
}

#[test]
fn status_watch_human_emits_initial_frame() {
    let db_path = unique_db("watch-status-human");
    let mut child = bowline()
        .args(["status", "--watch"])
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .stdout(Stdio::piped())
        .spawn()
        .expect("bowline status watch should start");
    let stdout = child.stdout.take().expect("watch stdout should be piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("initial watch frame should be readable");

    assert!(
        child.try_wait().expect("watch child status").is_none(),
        "watch should keep streaming after the initial frame"
    );
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        line,
        include_str!("../../../tests/golden/cli/status-watch.txt")
    );
}

#[test]
fn status_json_reports_daemon_component_degradation_from_metadata() {
    let db_path = unique_db("daemon-component-status");
    seed_daemon_component_status(&db_path);

    let output = run_bowline_with_env(
        &["status", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "limited");
    assert_eq!(json["eventWatermarks"]["syncState"], "degraded");
    assert_eq!(json["eventWatermarks"]["watcherState"], "unavailable");
    assert_eq!(json["eventWatermarks"]["networkState"], "offline");
    let text = serde_json::to_string(&json).expect("status serializes");
    assert!(text.contains("Sync is degraded."), "{text}");
    assert!(text.contains("Native file watching is degraded."), "{text}");
    assert!(text.contains("Network is unavailable."), "{text}");
}

#[test]
fn status_json_derives_safe_actions_from_status() {
    let db_path = unique_db("actions-missing-status");
    let output = run_bowline_with_env(
        &["status", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 3);
    assert_eq!(json["command"], "status");
    assert_eq!(json["status"]["level"], "attention");
    assert_eq!(
        json["nextActions"][0]["label"],
        "Initialize ~/Code when ready"
    );
}

#[test]
fn tui_noninteractive_falls_back_to_actions_output() {
    let db_path = unique_db("tui-fallback");
    let output = run_bowline_with_env(
        &["tui"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("tui fallback should be utf8");
    assert!(stdout.contains("Actions: Attention"));
    assert!(stdout.contains("Initialize ~/Code when ready"));
}

#[test]
fn tui_json_reports_typed_usage_error() {
    let output = run_bowline(&["tui", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 3);
    assert_eq!(json["command"], "tui");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "usage_error");
    assert_eq!(json["nextActions"][0]["command"], "bowline status --json");
}

#[test]
fn resolve_tui_noninteractive_falls_back_to_resolve_output() {
    let temp = TempWorkspace::new("resolve-tui-fallback").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_tui");
    create_conflict_bundle_with_id(&bundle, "conflict_tui", "src/auth.ts", false);

    let output = run_bowline_with_env(
        &["resolve", project.to_str().expect("project path"), "--tui"],
        &[("BOWLINE_GENERATED_AT", "2026-06-25T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("resolve output should be utf8");
    assert!(stdout.contains("Resolve: 1 unresolved conflict bundle(s) found"));
    assert!(stdout.contains("conflict_tui"));
}

#[test]
fn events_json_reports_empty_history_for_missing_metadata() {
    let db_path = unique_db("missing-events");
    let output = run_bowline_with_env(
        &["events", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "events");
    assert_eq!(json["events"].as_array().expect("events array").len(), 0);
}

#[test]
fn events_json_filters_to_requested_project() {
    let db_path = unique_db("scoped-events");
    let home = std::env::temp_dir().join(format!("bowline-home-{}", unique_suffix()));
    let code_root = home.join("Code");
    let web_dir = code_root.join("apps").join("web");
    fs::create_dir_all(&web_dir).expect("web dir");
    let home = fs::canonicalize(home).expect("home canonicalizes");
    let code_root = home.join("Code");
    let web_dir = code_root.join("apps").join("web");
    seed_two_project_events_with_root(&db_path, &code_root.display().to_string());

    let output = run_bowline_with_env_in_dir(
        &["events", "src/index.ts", "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("HOME", home.display().to_string()),
        ],
        &web_dir,
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["projectId"], "proj_web");
    assert_eq!(json["scope"], "project");
    assert_eq!(json["requestedPath"], "~/Code/apps/web/src/index.ts");
    assert_eq!(json["events"].as_array().expect("events array").len(), 1);
    assert_eq!(json["events"][0]["id"], "evt_web");
}

#[test]
fn events_workspace_json_includes_all_projects() {
    let db_path = unique_db("workspace-events");
    seed_two_project_events(&db_path);

    let output = run_bowline_with_env(
        &["events", "--workspace", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["events"].as_array().expect("events array").len(), 2);
}

#[test]
fn events_default_path_uses_raw_cwd_for_absolute_root_project_scope() {
    let db_path = unique_db("cwd-scoped-events");
    let home = std::env::temp_dir().join(format!("bowline-home-{}", unique_suffix()));
    let code_root = home.join("Code");
    let web_dir = code_root.join("apps").join("web");
    fs::create_dir_all(&web_dir).expect("web dir");
    let home = fs::canonicalize(home).expect("home canonicalizes");
    let code_root = home.join("Code");
    let web_dir = code_root.join("apps").join("web");
    seed_two_project_events_with_root(&db_path, &code_root.display().to_string());

    let output = run_bowline_with_env_in_dir(
        &["events", "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("HOME", home.display().to_string()),
        ],
        &web_dir,
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["projectId"], "proj_web");
    assert_eq!(json["scope"], "project");
    assert_eq!(json["requestedPath"], "~/Code/apps/web");
    assert_eq!(json["events"].as_array().expect("events array").len(), 1);
    assert_eq!(json["events"][0]["id"], "evt_web");
}

#[test]
fn events_default_path_expands_tilde_root_for_project_scope() {
    let db_path = unique_db("cwd-tilde-scoped-events");
    let home = std::env::temp_dir().join(format!("bowline-home-{}", unique_suffix()));
    let code_root = home.join("Code");
    let web_dir = code_root.join("apps").join("web");
    fs::create_dir_all(&web_dir).expect("web dir");
    let home = fs::canonicalize(home).expect("home canonicalizes");
    let web_dir = home.join("Code").join("apps").join("web");
    seed_two_project_events(&db_path);

    let output = run_bowline_with_env_in_dir(
        &["events", "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("HOME", home.display().to_string()),
        ],
        &web_dir,
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["projectId"], "proj_web");
    assert_eq!(json["scope"], "project");
    assert_eq!(json["requestedPath"], "~/Code/apps/web");
    assert_eq!(json["events"].as_array().expect("events array").len(), 1);
    assert_eq!(json["events"][0]["id"], "evt_web");
}

#[test]
fn events_json_reports_corrupt_metadata_as_command_error() {
    let db_path = unique_db("corrupt-events");
    fs::create_dir_all(db_path.parent().expect("db parent")).expect("db parent");
    fs::write(&db_path, b"not sqlite").expect("corrupt db");

    let output = run_bowline_with_env(
        &["events", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 3);
    assert_eq!(json["command"], "events");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["code"], "runtime_error");
}

#[test]
fn resolve_json_lists_bundle_and_prints_redacted_prompt() {
    let temp = TempWorkspace::new("resolve-prompt").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, true);

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--copy-prompt",
            "--json",
        ],
        &[
            ("PATH", temp.root().join("empty-bin").display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 3);
    assert_eq!(json["command"], "resolve");
    assert_eq!(json["action"], "copy-prompt");
    assert_eq!(json["status"]["level"], "attention");
    assert_eq!(json["conflicts"][0]["id"], "conflict_same_line");
    assert_eq!(json["conflicts"][0]["containsSecrets"], true);
    assert_eq!(
        json["conflicts"][0]["affectedFiles"][0],
        "apps/web/.env.local"
    );
    assert_eq!(json["availableAgents"].as_array().expect("agents").len(), 0);
    let actions = serde_json::to_string(&json["availableActions"]).expect("actions serialize");
    assert!(actions.contains("--copy-prompt"));
    assert!(actions.contains("--diff conflict_same_line"));
    assert!(!actions.contains("--agent codex"));
    let prompt = json["prompt"]["text"].as_str().expect("prompt text");
    assert!(prompt.contains("Bundle path:"));
    assert!(prompt.contains("base/ contains the common ancestor bytes"));
    assert!(prompt.contains("local/ contains this device's version"));
    assert!(prompt.contains("remote/ contains the workspace-head version"));
    assert!(prompt.contains("resolution/ is the only place you may write"));
    assert!(prompt.contains("Do not use Git"));
    assert!(prompt.contains("Do not write outside the resolution overlay"));
    assert!(!prompt.contains("SECRET_VALUE"));
}

#[test]
fn resolve_json_surfaces_conflict_kind_and_spans() {
    let temp = TempWorkspace::new("resolve-typed-conflict").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_typed_span");
    create_typed_conflict_bundle(&bundle);

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--copy-prompt",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-26T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    let conflict = &json["conflicts"][0];
    assert_eq!(conflict["id"], "conflict_typed_span");
    assert_eq!(conflict["conflictKind"], "text");
    assert_eq!(conflict["spans"][0]["path"], "apps/web/src/auth.ts");
    assert_eq!(conflict["spans"][0]["localStartLine"], 14);
    assert_eq!(conflict["spans"][0]["remoteEndLine"], 22);
    let prompt = json["prompt"]["text"].as_str().expect("prompt text");
    assert!(prompt.contains("Conflict kind: text"));
    assert!(prompt.contains("apps/web/src/auth.ts base:14-22 local:14-22 remote:14-22"));
}

#[test]
fn resolve_json_prints_redacted_bundle_diff_paths() {
    let temp = TempWorkspace::new("resolve-diff").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, true);

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--diff",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["action"], "diff");
    assert_eq!(json["diff"]["conflictId"], "conflict_same_line");
    assert_eq!(json["diff"]["redaction"], "contents-not-printed");
    let diff = json["diff"]["text"].as_str().expect("diff text");
    assert!(diff.contains("Review paths:"));
    assert!(diff.contains("base:"));
    assert!(diff.contains("local:"));
    assert!(diff.contains("remote:"));
    assert!(diff.contains("resolution:"));
    assert!(diff.contains("apps/web/.env.local"));
    assert!(!diff.contains("SECRET_VALUE"));
}

#[test]
fn resolve_json_missing_diff_conflict_returns_failure() {
    let temp = TempWorkspace::new("resolve-missing-diff").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--diff",
            "missing_conflict",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["action"], "diff");
    assert!(json.get("diff").is_none());
    assert_eq!(json["status"]["level"], "attention");
    assert_eq!(
        json["status"]["summary"],
        "conflict `missing_conflict` was not found"
    );
}

#[test]
fn resolve_human_handles_canary_like_conflict_ids_without_panic() {
    let temp = TempWorkspace::new("resolve-canary-id").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("API_KEY=example");
    create_conflict_bundle_with_id(&bundle, "API_KEY=example", "apps/web/config.txt", false);

    let output = run_bowline(&["resolve", project.to_str().expect("project path")]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("resolve output");
    assert!(stdout.contains("API_KEY=example"));
}

#[test]
fn resolve_json_lists_only_available_agent_options() {
    let temp = TempWorkspace::new("resolve-agent").expect("temp workspace");
    let bin = temp.root().join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let codex = bin.join("codex");
    fs::write(&codex, "#!/bin/sh\nexit 0\n").expect("codex fake");
    let mut perms = fs::metadata(&codex).expect("codex metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&codex, perms).expect("codex executable");

    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_agent");
    create_conflict_bundle(&bundle, false);

    let output = run_bowline_with_env(
        &["resolve", project.to_str().expect("project path"), "--json"],
        &[
            ("PATH", bin.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["availableAgents"].as_array().expect("agents").len(), 1);
    assert_eq!(json["availableAgents"][0]["name"], "codex");
    assert_eq!(
        json["availableAgents"][0]["capability"]["supportsStdinLaunch"],
        true
    );
    assert_eq!(
        json["availableAgents"][0]["capability"]["supportsCwdSelection"],
        true
    );
    assert_eq!(
        json["availableAgents"][0]["capability"]["supportsNoninteractiveExecution"],
        true
    );
    assert_eq!(
        json["availableAgents"][0]["capability"]["supportsReceiptCapture"],
        true
    );
    assert_eq!(
        json["availableAgents"][0]["capability"]["degradedReason"],
        serde_json::Value::Null
    );
    let actions = serde_json::to_string(&json["availableActions"]).expect("actions serialize");
    assert!(actions.contains("--agent codex"));
    assert!(!actions.contains("--agent claude"));
    assert!(!actions.contains("--agent cursor"));
}

#[test]
fn resolve_agent_requires_secret_scope_for_secret_bearing_conflict() {
    let temp = TempWorkspace::new("resolve-agent-secret-scope").expect("temp workspace");
    let bin = temp.root().join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let codex = bin.join("codex");
    fs::write(&codex, "#!/bin/sh\nexit 0\n").expect("codex fake");
    let mut perms = fs::metadata(&codex).expect("codex metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&codex, perms).expect("codex executable");

    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_secret_agent");
    create_conflict_bundle_with_id(
        &bundle,
        "conflict_secret_agent",
        "apps/web/.env.local",
        true,
    );

    let denied = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--agent",
            "codex",
            "--json",
        ],
        &[
            ("PATH", bin.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );
    assert_eq!(denied.status.code(), Some(1));
    let json = parse_stdout_json(denied);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("secret-read scope")
    );
    assert!(
        json["prompt"]["text"]
            .as_str()
            .expect("prompt")
            .contains("redacted")
            || json["prompt"]["text"]
                .as_str()
                .expect("prompt")
                .contains("Do not print")
    );
    assert!(
        !serde_json::to_string(&json)
            .expect("json")
            .contains("SECRET_VALUE"),
        "agent prompt output must stay redacted"
    );

    let allowed = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--agent",
            "codex",
            "--json",
        ],
        &[
            ("PATH", bin.display().to_string()),
            ("BOWLINE_ALLOW_SECRET_CONFLICT_AGENT", "1".to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );
    assert!(allowed.status.success());
    let json = parse_stdout_json(allowed);
    assert_eq!(json["requestedAgent"], "codex");
}

#[test]
fn resolve_decision_with_extra_agent_flag_does_not_trigger_secret_scope_gate() {
    let temp = TempWorkspace::new("resolve-agent-flag-with-accept").expect("temp workspace");
    let bin = temp.root().join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let codex = bin.join("codex");
    fs::write(&codex, "#!/bin/sh\nexit 0\n").expect("codex fake");
    let mut perms = fs::metadata(&codex).expect("codex metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&codex, perms).expect("codex executable");

    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_secret_accept");
    create_conflict_bundle_with_id(
        &bundle,
        "conflict_secret_accept",
        "apps/web/.env.local",
        true,
    );
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution dir");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_secret_accept",
            "--agent",
            "codex",
            "--json",
        ],
        &[
            ("PATH", bin.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(
        output.status.success(),
        "accept action must not be reported as denied by the unrelated agent flag"
    );
    let json = parse_stdout_json(output);
    assert_eq!(json["action"], "accept");
    assert_eq!(json["status"]["level"], "healthy");
}

#[test]
fn resolve_json_quotes_available_action_paths() {
    let temp = TempWorkspace::new("resolve-action-quoting").expect("temp workspace");
    let project = temp.root().join("Code").join("My Project");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);

    let output = run_bowline_with_env(
        &["resolve", project.to_str().expect("project path"), "--json"],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    let actions = serde_json::to_string(&json["availableActions"]).expect("actions serialize");
    assert!(actions.contains("bowline resolve '"));
    assert!(actions.contains("My Project"));
    assert!(actions.contains("' --copy-prompt"));
}

#[test]
fn resolve_json_discovers_daemon_state_root_conflicts() {
    let temp = TempWorkspace::new("resolve-state-root").expect("temp workspace");
    let project = temp.root().join("Code");
    let state_root = temp.root().join("state");
    let bundle = state_root.join("conflicts").join("conflict_same_line");
    create_conflict_bundle(&bundle, false);

    let output = run_bowline_with_env(
        &["resolve", project.to_str().expect("project path"), "--json"],
        &[("BOWLINE_STATE_ROOT", state_root.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert_eq!(json["conflicts"].as_array().expect("conflicts").len(), 1);
    assert_eq!(json["conflicts"][0]["id"], "conflict_same_line");
}

#[test]
fn resolve_accept_applies_resolution_overlay_and_closes_bundle() {
    let temp = TempWorkspace::new("resolve-accept").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    fs::set_permissions(
        bundle.join("manifest.json"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("private manifest permissions");
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["action"], "accept");
    assert_eq!(json["selectedConflictId"], "conflict_same_line");
    assert_eq!(json["status"]["level"], "healthy");
    assert_eq!(json["conflicts"].as_array().expect("conflicts").len(), 0);
    assert_eq!(
        fs::read(project.join("apps").join("web").join(".env.local")).expect("applied"),
        b"SECRET=resolved\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(manifest.contains("\"state\": \"accepted\""));
    assert_eq!(
        fs::metadata(bundle.join("manifest.json"))
            .expect("manifest metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "resolve accept must not weaken private conflict manifest permissions"
    );
}

#[test]
fn resolve_accept_queues_upload_for_initialized_workspace() {
    let temp = TempWorkspace::new("resolve-accept-sync-queue").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = database_path_for_platform(Platform::Macos, &home, None);
    let code_root = temp.root().join("Code");
    let project = code_root.join("app");
    fs::create_dir_all(&project).expect("project");
    seed_daemon_start_workspace(&db_path, &code_root);
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .upsert_workspace_sync_head(&bowline_local::metadata::WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.as_str().to_string(),
                version: 9,
                snapshot_id: "snap-9".to_string(),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 9 },
                updated_by_device_id: Some("device-a".to_string()),
            },
            observed_at: "2026-06-24T11:59:00Z".to_string(),
        })
        .expect("head stored");
    drop(store);

    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    let store = MetadataStore::open(&db_path).expect("metadata reopens");
    let operations = store
        .sync_operations(&workspace_id)
        .expect("sync operations read");
    let operation = operations
        .iter()
        .find(|operation| {
            operation
                .id
                .starts_with("resolve:conflict_same_line:accept")
        })
        .expect("resolve accept queued sync");
    assert_eq!(operation.kind, "upload");
    assert_eq!(operation.state, "queued");
    assert_eq!(operation.base_version, Some(9));
    assert!(operation.payload_json.contains("\"decision\":\"accept\""));
    let events = store.list_events(20).expect("events read");
    let event = events
        .iter()
        .find(|event| event.name == EventName::ConflictResolutionAccepted)
        .expect("resolution accepted event");
    assert_eq!(
        event.subject.as_ref().expect("subject").id,
        "conflict_same_line"
    );
    assert_eq!(event.payload["decision"], "accept");
    assert_eq!(
        event.redaction.status,
        bowline_core::events::EventRedactionStatus::Applied
    );
    assert!(
        !serde_json::to_string(event)
            .expect("event json")
            .contains("SECRET=resolved"),
        "resolution event must not contain secret values"
    );
}

#[test]
fn resolve_accept_keeps_attention_when_other_conflicts_remain() {
    let temp = TempWorkspace::new("resolve-accept-partial").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let first = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    let second = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_other");
    create_conflict_bundle(&first, false);
    create_conflict_bundle_with_id(&second, "conflict_other", "apps/api/.env.local", false);
    for (bundle, value) in [
        (&first, b"SECRET=first\n".as_slice()),
        (&second, b"SECRET=second\n".as_slice()),
    ] {
        fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
        fs::write(
            bundle
                .join("resolution")
                .join("apps")
                .join("web")
                .join(".env.local"),
            value,
        )
        .expect("resolution file");
    }

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["action"], "accept");
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("1 unresolved conflict remain")
    );
    assert_eq!(json["conflicts"].as_array().expect("conflicts").len(), 1);
    assert_eq!(json["conflicts"][0]["id"], "conflict_other");
}

#[test]
fn resolve_accept_does_not_partially_apply_when_later_file_is_invalid() {
    let temp = TempWorkspace::new("resolve-accept-no-partial").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle_with_paths(
        &bundle,
        "conflict_same_line",
        &["apps/web/.env.local", "apps/web/missing.env"],
        false,
    );
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=first\n",
    )
    .expect("first resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("missing.env")
    );
    assert!(
        !project.join("apps").join("web").join(".env.local").exists(),
        "valid earlier files must not be applied when a later path fails validation"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_accept_preflights_all_destinations_before_applying_files() {
    let temp = TempWorkspace::new("resolve-accept-destination-preflight").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle_with_paths(
        &bundle,
        "conflict_same_line",
        &["apps/web/.env.local", "apps/web/existing-dir"],
        false,
    );
    let resolution_root = bundle.join("resolution").join("apps").join("web");
    fs::create_dir_all(&resolution_root).expect("resolution");
    fs::write(resolution_root.join(".env.local"), b"SECRET=first\n")
        .expect("first resolution file");
    fs::write(resolution_root.join("existing-dir"), b"second\n").expect("second resolution file");
    fs::create_dir_all(project.join("apps").join("web").join("existing-dir"))
        .expect("existing destination directory");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        !project.join("apps").join("web").join(".env.local").exists(),
        "valid earlier files must not be applied when a later destination is not replaceable"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_accept_allows_missing_resolution_for_delete_edit_conflict() {
    let temp = TempWorkspace::new("resolve-accept-delete").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_delete_edit");
    create_conflict_bundle_with_paths(
        &bundle,
        "conflict_delete_edit",
        &["apps/web/obsolete.env"],
        false,
    );
    let manifest_path = bundle.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).expect("manifest"))
            .expect("manifest json");
    manifest["reason"] = Value::String("delete-versus-edit conflict".to_string());
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("manifest bytes"),
    )
    .expect("manifest write");
    fs::create_dir_all(project.join("apps").join("web")).expect("project dir");
    fs::write(
        project.join("apps").join("web").join("obsolete.env"),
        b"SECRET=local\n",
    )
    .expect("local file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_delete_edit",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["action"], "accept");
    assert!(
        !project
            .join("apps")
            .join("web")
            .join("obsolete.env")
            .exists(),
        "delete-vs-edit conflicts can be resolved by omitting the path from resolution/"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_accept_rejects_direct_state_root_bundle_path() {
    let temp = TempWorkspace::new("resolve-direct-state-bundle").expect("temp workspace");
    let state_root = temp.root().join("state");
    let bundle = state_root.join("conflicts").join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            bundle.to_str().expect("bundle path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
            ("BOWLINE_STATE_ROOT", state_root.display().to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("workspace root")
    );
    assert!(!state_root.join("conflicts").join("apps").exists());
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_accept_applies_state_root_bundle_only_to_recorded_workspace_root() {
    let temp = TempWorkspace::new("resolve-state-root-workspace").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    fs::create_dir_all(&project).expect("project");
    let state_root = temp.root().join("state");
    let bundle = state_root.join("conflicts").join("conflict_same_line");
    create_conflict_bundle_with_workspace_root(&bundle, false, &project);
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
            ("BOWLINE_STATE_ROOT", state_root.display().to_string()),
        ],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "healthy");
    assert_eq!(
        fs::read(project.join("apps").join("web").join(".env.local")).expect("applied"),
        b"SECRET=resolved\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_accept_applies_state_root_bundle_from_requested_project_path() {
    let temp = TempWorkspace::new("resolve-state-root-project-accept").expect("temp workspace");
    let workspace_root = temp.root().join("Code");
    let project = workspace_root.join("apps").join("web");
    fs::create_dir_all(&project).expect("project");
    let state_root = temp.root().join("state");
    let bundle = state_root.join("conflicts").join("conflict_same_line");
    create_conflict_bundle_with_workspace_root(&bundle, false, &workspace_root);
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
            ("BOWLINE_STATE_ROOT", state_root.display().to_string()),
        ],
    );

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "healthy");
    assert_eq!(
        fs::read(project.join(".env.local")).expect("applied"),
        b"SECRET=resolved\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_accept_rejects_state_root_bundle_for_wrong_project_root() {
    let temp = TempWorkspace::new("resolve-state-root-wrong-project").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let wrong_project = temp.root().join("Code").join("wrong");
    fs::create_dir_all(&project).expect("project");
    fs::create_dir_all(&wrong_project).expect("wrong project");
    let state_root = temp.root().join("state");
    let bundle = state_root.join("conflicts").join("conflict_same_line");
    create_conflict_bundle_with_workspace_root(&bundle, false, &project);
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            wrong_project.to_str().expect("wrong path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
            ("BOWLINE_STATE_ROOT", state_root.display().to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("belongs to")
    );
    assert!(!wrong_project.join("apps").exists());
    assert!(!project.join("apps").exists());
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_reject_rejects_state_root_bundle_for_wrong_project_root() {
    let temp = TempWorkspace::new("resolve-reject-state-root-wrong-project").expect("temp");
    let project = temp.root().join("Code").join("app");
    let wrong_project = temp.root().join("Code").join("wrong");
    fs::create_dir_all(&project).expect("project");
    fs::create_dir_all(&wrong_project).expect("wrong project");
    let state_root = temp.root().join("state");
    let bundle = state_root.join("conflicts").join("conflict_same_line");
    create_conflict_bundle_with_workspace_root(&bundle, false, &project);

    let output = run_bowline_with_env(
        &[
            "resolve",
            wrong_project.to_str().expect("wrong path"),
            "--reject",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
            ("BOWLINE_STATE_ROOT", state_root.display().to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("belongs to")
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"rejected\""));
}

#[test]
fn resolve_reject_applies_state_root_bundle_from_requested_project_path() {
    let temp = TempWorkspace::new("resolve-state-root-project-reject").expect("temp workspace");
    let workspace_root = temp.root().join("Code");
    let project = workspace_root.join("apps").join("web");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join(".env.local"), b"SECRET=local\n").expect("local");
    let state_root = temp.root().join("state");
    let bundle = state_root.join("conflicts").join("conflict_same_line");
    create_conflict_bundle_with_workspace_root(&bundle, false, &workspace_root);
    fs::write(
        bundle
            .join("remote")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=remote\n",
    )
    .expect("remote file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--reject",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
            ("BOWLINE_STATE_ROOT", state_root.display().to_string()),
        ],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "healthy");
    assert_eq!(
        fs::read(project.join(".env.local")).expect("rejected remote side"),
        b"SECRET=remote\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(manifest.contains("\"state\": \"rejected\""));
}

#[test]
fn resolve_accept_rejects_private_state_targets() {
    for (index, target) in [".bowline/conflicts/manifest.json"].into_iter().enumerate() {
        let temp =
            TempWorkspace::new(&format!("resolve-private-roots-{index}")).expect("temp workspace");
        let project = temp.root().join("Code").join("app");
        let conflict_id = format!("conflict_private_{index}");
        let bundle = project
            .join(".bowline")
            .join("conflicts")
            .join(&conflict_id);
        create_conflict_bundle_with_id(&bundle, &conflict_id, target, false);

        let output = run_bowline_with_env(
            &[
                "resolve",
                project.to_str().expect("project path"),
                "--accept",
                &conflict_id,
                "--json",
            ],
            &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
        );

        assert_eq!(output.status.code(), Some(1));
        let json = parse_stdout_json(output);
        assert_eq!(json["status"]["level"], "attention");
        assert!(
            json["status"]["summary"]
                .as_str()
                .expect("summary")
                .contains("unsafe"),
            "{target} should be rejected as private state"
        );
        assert!(!project.join(target).exists());
        let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
        assert!(!manifest.contains("\"state\": \"accepted\""));
    }
}

#[cfg(unix)]
#[test]
fn resolve_accept_rejects_symlinked_resolution_overlay() {
    use std::os::unix::fs::symlink;

    let temp = TempWorkspace::new("resolve-source-symlink").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    let outside = temp.root().join("outside-secret.txt");
    fs::write(&outside, b"SECRET=outside\n").expect("outside file");
    let resolution_file = bundle
        .join("resolution")
        .join("apps")
        .join("web")
        .join(".env.local");
    fs::create_dir_all(resolution_file.parent().expect("resolution parent"))
        .expect("resolution parent");
    symlink(&outside, &resolution_file).expect("resolution symlink");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("unsafe")
    );
    assert!(!project.join("apps").join("web").join(".env.local").exists());
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
}

#[cfg(unix)]
#[test]
fn resolve_accept_rejects_symlinked_project_destination() {
    use std::os::unix::fs::symlink;

    let temp = TempWorkspace::new("resolve-destination-symlink").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    let resolution_file = bundle
        .join("resolution")
        .join("apps")
        .join("web")
        .join(".env.local");
    fs::create_dir_all(resolution_file.parent().expect("resolution parent"))
        .expect("resolution parent");
    fs::write(&resolution_file, b"SECRET=resolved\n").expect("resolution");

    let outside = temp.root().join("outside-live.txt");
    fs::write(&outside, b"SECRET=outside\n").expect("outside file");
    let destination = project.join("apps").join("web").join(".env.local");
    fs::create_dir_all(destination.parent().expect("destination parent"))
        .expect("destination parent");
    symlink(&outside, &destination).expect("destination symlink");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("unsafe")
    );
    assert_eq!(
        fs::read(&outside).expect("outside unchanged"),
        b"SECRET=outside\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
}

#[test]
fn resolve_reject_closes_bundle_without_applying_resolution() {
    let temp = TempWorkspace::new("resolve-reject").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    fs::create_dir_all(project.join("apps").join("web")).expect("project dir");
    fs::write(
        project.join("apps").join("web").join(".env.local"),
        b"SECRET=local unresolved\n",
    )
    .expect("local file");
    fs::write(
        bundle
            .join("remote")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=remote accepted\n",
    )
    .expect("remote side");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--reject",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["action"], "reject");
    assert_eq!(json["status"]["level"], "healthy");
    assert_eq!(json["conflicts"].as_array().expect("conflicts").len(), 0);
    assert_eq!(
        fs::read(project.join("apps").join("web").join(".env.local")).expect("project file"),
        b"SECRET=remote accepted\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(manifest.contains("\"state\": \"rejected\""));
}

#[test]
fn resolve_reject_queues_upload_for_initialized_workspace() {
    let temp = TempWorkspace::new("resolve-reject-sync-queue").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = database_path_for_platform(Platform::Macos, &home, None);
    let code_root = temp.root().join("Code");
    let project = code_root.join("app");
    fs::create_dir_all(&project).expect("project");
    seed_daemon_start_workspace(&db_path, &code_root);
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .upsert_workspace_sync_head(&bowline_local::metadata::WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.as_str().to_string(),
                version: 10,
                snapshot_id: "snap-10".to_string(),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 10 },
                updated_by_device_id: Some("device-b".to_string()),
            },
            observed_at: "2026-06-24T11:59:00Z".to_string(),
        })
        .expect("head stored");
    drop(store);

    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    fs::write(
        bundle
            .join("remote")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=remote\n",
    )
    .expect("remote side");
    fs::create_dir_all(project.join("apps").join("web")).expect("project dirs");
    fs::write(
        project.join("apps").join("web").join(".env.local"),
        b"SECRET=local unresolved\n",
    )
    .expect("local file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--reject",
            "conflict_same_line",
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    let store = MetadataStore::open(&db_path).expect("metadata reopens");
    let operations = store
        .sync_operations(&workspace_id)
        .expect("sync operations read");
    let operation = operations
        .iter()
        .find(|operation| {
            operation
                .id
                .starts_with("resolve:conflict_same_line:reject")
        })
        .expect("resolve reject queued sync");
    assert_eq!(operation.kind, "upload");
    assert_eq!(operation.state, "queued");
    assert_eq!(operation.base_version, Some(10));
    assert!(operation.payload_json.contains("\"decision\":\"reject\""));
    let events = store.list_events(20).expect("events read");
    let event = events
        .iter()
        .find(|event| event.name == EventName::ConflictResolutionRejected)
        .expect("resolution rejected event");
    assert_eq!(
        event.subject.as_ref().expect("subject").id,
        "conflict_same_line"
    );
    assert_eq!(event.payload["decision"], "reject");
    assert!(
        !serde_json::to_string(event)
            .expect("event json")
            .contains("SECRET=remote"),
        "resolution event must not contain secret values"
    );
}

#[test]
fn resolve_reject_refuses_missing_remote_side_for_non_delete_conflict() {
    let temp = TempWorkspace::new("resolve-reject-missing-remote").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_kind");
    fs::create_dir_all(bundle.join("base").join("apps").join("web")).expect("base dir");
    fs::create_dir_all(bundle.join("local").join("apps").join("web")).expect("local dir");
    fs::create_dir_all(bundle.join("remote").join("apps").join("web")).expect("remote dir");
    fs::create_dir_all(bundle.join("resolution")).expect("resolution dir");
    fs::write(
        bundle.join("manifest.json"),
        r#"{"conflictId":"conflict_kind","affectedFiles":["apps/web/link"],"activeView":"local","reason":"path kind conflict","containsSecrets":false}"#,
    )
    .expect("manifest");
    fs::create_dir_all(project.join("apps").join("web")).expect("project dir");
    fs::write(project.join("apps").join("web").join("link"), b"local\n").expect("local file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--reject",
            "conflict_kind",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("summary")
            .contains("missing")
    );
    assert_eq!(
        fs::read(project.join("apps").join("web").join("link")).expect("local file"),
        b"local\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"rejected\""));
}

#[cfg(unix)]
#[test]
fn resolve_accept_writes_owner_only_resolution_files() {
    let temp = TempWorkspace::new("resolve-owner-only").expect("temp workspace");
    let project = temp.root().join("Code").join("app");
    let bundle = project
        .join(".bowline")
        .join("conflicts")
        .join("conflict_same_line");
    create_conflict_bundle(&bundle, false);
    fs::create_dir_all(bundle.join("resolution").join("apps").join("web")).expect("resolution");
    fs::write(
        bundle
            .join("resolution")
            .join("apps")
            .join("web")
            .join(".env.local"),
        b"SECRET=resolved\n",
    )
    .expect("resolution file");

    let output = run_bowline_with_env(
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--accept",
            "conflict_same_line",
            "--json",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let mode = fs::metadata(project.join("apps").join("web").join(".env.local"))
        .expect("resolved file metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "resolved files must not be group/world readable"
    );
}

#[test]
fn events_limit_rejects_unbounded_requests() {
    let output = run_bowline(&["events", "--limit", "999999", "--json"]);

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
    let output = bowline()
        .args(["devices", "--json"])
        .current_dir(temp.root())
        .env(
            "BOWLINE_METADATA_DB",
            temp.root().join("state").join("local.sqlite3"),
        )
        .env_remove("CONVEX_URL")
        .env_remove("BOWLINE_CONTROL_PLANE_TOKEN")
        .env_remove("BOWLINE_USE_FAKE_CONTROL_PLANE")
        .env_remove("BOWLINE_WORKOS_ACCESS_TOKEN")
        .env_remove("BOWLINE_WORKOS_REFRESH_TOKEN")
        .env_remove("BOWLINE_WORKSPACE_ID")
        .output()
        .expect("bowline should run");

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "devices");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["code"], "runtime_error");
    let message = json["error"]["message"]
        .as_str()
        .expect("error message is a string");
    assert!(message.contains("control-plane configuration is missing"));
    assert!(message.contains("CONVEX_URL"));
    assert!(message.contains("BOWLINE_CONTROL_PLANE_TOKEN"));
}

#[test]
fn agent_start_json_reports_missing_workspace() {
    let db_path = unique_db("agent-start-missing-workspace");
    let output = run_bowline_with_env(
        &[
            "agent",
            "start",
            "/tmp/project",
            "--task",
            "fix auth callback race",
            "--json",
        ],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "agent start");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["code"], "runtime_error");
    assert_eq!(
        json["error"]["message"],
        "no bowline workspace is initialized"
    );
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

    assert_eq!(output.status.code(), Some(1));
    let json = parse_stdout_json(output);
    let expected_root = code_root.display().to_string();
    assert_eq!(json["command"], "connect");
    assert_eq!(json["root"], expected_root);
    assert!(
        json["nextActions"]
            .as_array()
            .expect("next actions")
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
fn dev_cloud_spike_is_hidden_from_help_but_fake_json_runs() {
    let help = run_bowline(&["help"]);
    assert!(help.status.success());
    let help_stdout = String::from_utf8(help.stdout).expect("help output should be utf8");
    assert!(!help_stdout.contains("cloud-spike"));

    let help_json = parse_stdout_json(run_bowline(&["help", "--json"]));
    let help_json_text = serde_json::to_string(&help_json).expect("help json should serialize");
    assert!(!help_json_text.contains("cloud-spike"));
    assert!(!help_json_text.contains("dev cloud-spike"));

    let output = run_bowline(&["dev", "cloud-spike", "--json"]);
    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "dev cloud-spike");
    assert_eq!(json["provider"], "fake");
    assert_eq!(json["advancedVersion"], 1);
    assert_eq!(json["staleRefDetected"], true);
    assert_eq!(json["deviceApprovalHarnessOnly"], true);
}

#[test]
fn hosted_cloud_spike_json_skips_without_env() {
    let temp = TempWorkspace::new("hosted-cloud-spike-missing-env").expect("temp workspace");
    let output = run_bowline_without_env_in_dir(
        &["dev", "cloud-spike", "--provider", "hosted", "--json"],
        &[
            "CONVEX_URL",
            "BOWLINE_CONTROL_PLANE_TOKEN",
            "CLOUDFLARE_ACCOUNT_ID",
            "BOWLINE_R2_BUCKET",
            "R2_ACCESS_KEY_ID",
            "R2_SECRET_ACCESS_KEY",
        ],
        temp.root(),
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "dev cloud-spike");
    assert_eq!(json["provider"], "hosted");
    assert_eq!(json["skipped"], true);
    assert!(
        !json["missingEnv"]
            .as_array()
            .expect("missing env")
            .is_empty()
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

    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for CLI handshake"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("accepted stream should become blocking");

        let request = read_line(&mut stream).expect("handshake request should be readable");
        assert_eq!(
            request,
            "{\"type\":\"hello\",\"protocol\":\"bowline.local\",\"version\":1}"
        );
        stream
            .write_all(
                b"{\"type\":\"hello_ack\",\"protocol\":\"bowline.local\",\"version\":1,\"daemonVersion\":\"test-daemon\",\"status\":\"ok\"}\n",
            )
            .expect("handshake response should write");
    });

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
    assert_eq!(json["daemon"]["protocol"], "bowline.local");
    assert_eq!(json["daemon"]["version"], 1);
    assert_eq!(json["daemon"]["daemonVersion"], "test-daemon");
    if let Some(service) = json.get("service") {
        assert!(service["unitPath"].as_str().is_some_and(|path| {
            path.ends_with("bowline.service") || path.ends_with("io.bowline.daemon.plist")
        }));
    }
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

struct ProcessKillGuard(u32);

impl Drop for ProcessKillGuard {
    fn drop(&mut self) {
        kill_process(self.0);
    }
}

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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-26T12:00:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-26T12:00:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-26T12:00:00Z")
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
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-23T12:00:00Z")
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
