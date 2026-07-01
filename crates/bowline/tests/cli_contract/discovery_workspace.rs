use super::*;

#[test]
fn help_groups_commands_by_intent() {
    let output = run_bowline(&["help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");
    assert_eq!(
        stdout,
        include_str!("../../../../tests/golden/cli/help.txt")
    );
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
