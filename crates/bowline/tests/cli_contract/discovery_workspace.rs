use super::*;

#[test]
fn help_groups_commands_by_intent() {
    let output = run_bowline(&["help", "--human"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");
    assert_eq!(
        stdout,
        include_str!("../../../../tests/golden/cli/help.txt")
    );
    assert!(stdout.contains("Workspace:"));
    assert!(stdout.contains("bowline resolve [path] [--tui] [--copy-prompt] [--diff <conflict>]"));
    assert!(stdout.contains("bowline tui [--root <path>] [--project <path>]"));
    assert!(stdout.contains("Trust:"));
    assert!(stdout.contains("Remote:"));
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
    assert_eq!(version_json["contractVersion"], 8);
    assert_eq!(version_json["protocol"], "bowline-daemon-v2");

    let short_version = run_bowline(&["--version", "--human"]);
    assert_eq!(short_version.status.code(), Some(2));

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
            .any(|command| command["name"] == "status"
                && command["jsonOutputType"] == "StatusCommandOutput")
    );
}

#[test]
fn topic_help_json_works_for_canonical_command_paths() {
    for args in [
        &["help", "status", "--json"][..],
        &["help", "agent", "start", "--json"][..],
        &["help", "daemon", "install", "--json"][..],
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
fn setup_help_marks_root_optional_and_login_uses_default_root() {
    let setup = run_bowline(&["help", "setup", "--json"]);
    assert!(setup.status.success());
    let setup_json = parse_stdout_json(setup);
    let options = setup_json["commands"][0]["options"]
        .as_array()
        .expect("options");
    let root = options
        .iter()
        .find(|option| option["name"] == "--root")
        .expect("root option");
    assert_eq!(root["required"], false);

    let login = run_bowline(&["help", "login", "--json"]);
    assert!(login.status.success());
    let login_json = parse_stdout_json(login);
    assert_eq!(
        login_json["commands"][0]["jsonOutputType"],
        "LoginCommandOutput | SetupCommandOutput"
    );
    assert!(
        !login_json["commands"][0]["options"]
            .as_array()
            .expect("options")
            .iter()
            .any(|option| option["name"] == "--root")
    );
}

#[test]
fn unknown_command_json_uses_command_error_output() {
    let output = run_bowline(&["nope", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 8);
    assert_eq!(json["command"], "unknown");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "unknown_command");
}

#[test]
fn known_command_usage_errors_keep_command_name() {
    let output = run_bowline(&["events", "--root", "~/Code", "--limit", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "events");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "usage_error");
}

#[test]
fn dry_run_does_not_mask_parsed_usage_errors() {
    let output = run_bowline(&["work", "create", "--dry-run", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "work create");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "usage_error");
    assert_ne!(json["error"]["code"], "dry_run_unsupported");
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
    assert_eq!(json["nextActions"][0]["command"], json["applyCommand"]);
    assert_eq!(json["nextActions"][0]["mutates"], true);
}

#[test]
fn work_cleanup_dry_run_points_to_applying_cleanup() {
    let output = run_bowline(&["work", "cleanup", "--dry-run", "--json"]);

    assert!(output.status.success(), "{output:?}");
    let json = parse_stdout_json(output);
    assert_eq!(json["applyCommand"], "bowline work cleanup --apply");
    assert_eq!(json["nextActions"][0]["command"], json["applyCommand"]);
    assert_eq!(json["nextActions"][0]["mutates"], true);
}

#[test]
fn lease_commands_accept_advertised_dry_run_contract() {
    let join = run_bowline(&[
        "lease",
        "join",
        "--root",
        "/tmp/bowline-remote",
        "--dry-run",
        "--json",
    ]);
    assert!(join.status.success(), "{join:?}");
    let join_json = parse_stdout_json(join);
    assert_eq!(join_json["command"], "lease join");
    assert_eq!(join_json["status"], "dry-run");

    // `lease run` was removed with the agent-supervisor stack; only `lease join`
    // survives and the parser rejects the removed subcommand.
    let run = run_bowline(&["lease", "run", "--lease", "lease_remote"]);
    assert!(!run.status.success(), "{run:?}");
}

#[test]
fn work_create_dry_run_reports_apply_command_without_mutating() {
    let temp = TempWorkspace::new("cli-agent-use-work_create").expect("temp workspace");
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
        &["work", "create", &project_arg, "dry", "--dry-run", "--json"],
        &envs,
    );
    assert!(dry_run.status.success(), "{dry_run:?}");
    let dry_json = parse_stdout_json(dry_run);
    assert_eq!(dry_json["command"], "work create");
    assert_eq!(dry_json["status"], "dry-run");
    assert!(!code_root.join(".work/apps/web/dry").exists());
}

#[test]
fn work_create_resolves_relative_project_from_active_workspace_root() {
    let temp = TempWorkspace::new("cli-work_create-active-root").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let project_path = code_root.join("acme/web");
    let unrelated_cwd = temp.root().join("shell-cwd");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::create_dir_all(&unrelated_cwd).expect("unrelated cwd");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source");
    let code_root = code_root.canonicalize().expect("canonical code root");
    let db_path = temp.root().join(".state/local.sqlite3");
    seed_workspace_for_work_views(&db_path, &code_root);
    seed_additional_work_view_project(&db_path, &code_root, "acme/web");
    let envs = [
        ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ("BOWLINE_GENERATED_AT", "2026-07-10T12:00:00Z".to_string()),
        ("BOWLINE_DEVICE_ID", "dev_cli_active_root".to_string()),
    ];

    let created = run_bowline_with_env_in_dir(
        &["work", "create", "acme/web", "root-relative", "--json"],
        &envs,
        &unrelated_cwd,
    );

    assert!(created.status.success(), "{created:?}");
    let created_json = parse_stdout_json(created);
    assert_eq!(created_json["workView"]["projectPath"], "acme/web");
    assert!(code_root.join(".work/acme/web/root-relative").is_dir());
    assert!(!unrelated_cwd.join("acme/web").exists());
}

#[test]
fn explicit_dot_project_paths_stay_relative_to_shell_cwd() {
    let temp = TempWorkspace::new("cli-explicit-dot-project").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let project_path = code_root.join("acme/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source");
    let code_root = code_root.canonicalize().expect("canonical code root");
    let project_path = project_path.canonicalize().expect("canonical project");
    let db_path = temp.root().join(".state/local.sqlite3");
    seed_workspace_for_work_views(&db_path, &code_root);
    seed_additional_work_view_project(&db_path, &code_root, "acme/web");
    let envs = [
        ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ("BOWLINE_GENERATED_AT", "2026-07-10T12:00:00Z".to_string()),
        ("BOWLINE_DEVICE_ID", "dev_cli_explicit_dot".to_string()),
    ];

    for (selector, name) in [(".", "dot-work"), ("./src", "dot-child-work")] {
        let output = run_bowline_with_env_in_dir(
            &["work", "create", selector, name, "--json"],
            &envs,
            &project_path,
        );
        assert!(output.status.success(), "{selector}: {output:?}");
        let json = parse_stdout_json(output);
        assert_eq!(json["workView"]["projectPath"], "acme/web");
    }

    for selector in [".", "./src"] {
        let task = format!("dot path {selector}");
        let output = run_bowline_with_env_in_dir(
            &["agent", "start", selector, "--task", &task, "--json"],
            &envs,
            &project_path,
        );
        assert!(output.status.success(), "{selector}: {output:?}");
        let json = parse_stdout_json(output);
        assert_eq!(
            json["lease"]["writeTargetPath"],
            project_path.display().to_string()
        );
    }
}

#[test]
fn agent_sessions_complete_for_direct_and_isolated_targets() {
    let temp = TempWorkspace::new("cli-agent-complete").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let project_path = code_root.join("acme/web");
    let unrelated_cwd = temp.root().join("shell-cwd");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::create_dir_all(&unrelated_cwd).expect("unrelated cwd");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source");
    let code_root = code_root.canonicalize().expect("canonical code root");
    let db_path = temp.root().join(".state/local.sqlite3");
    seed_workspace_for_work_views(&db_path, &code_root);
    seed_additional_work_view_project(&db_path, &code_root, "acme/web");
    let envs = [
        ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ("BOWLINE_GENERATED_AT", "2026-07-10T12:00:00Z".to_string()),
        ("BOWLINE_DEVICE_ID", "dev_cli_agent_complete".to_string()),
    ];

    let direct = run_bowline_with_env_in_dir(
        &[
            "agent",
            "start",
            "acme/web",
            "--task",
            "direct task",
            "--json",
        ],
        &envs,
        &unrelated_cwd,
    );
    assert!(direct.status.success(), "{direct:?}");
    let direct_json = parse_stdout_json(direct);
    let direct_lease = direct_json["lease"]["id"]
        .as_str()
        .expect("direct lease id");
    let direct_completed = run_bowline_with_env(
        &["agent", "complete", "--lease", direct_lease, "--json"],
        &envs,
    );
    assert!(direct_completed.status.success(), "{direct_completed:?}");
    let direct_completed = parse_stdout_json(direct_completed);
    assert_eq!(direct_completed["command"], "agent complete");
    assert_eq!(direct_completed["lease"]["sessionState"], "completed");
    assert_eq!(direct_completed["status"]["level"], "healthy");
    assert_eq!(
        direct_completed["nextActions"][0]["command"],
        format!(
            "bowline status --root {} --project acme/web",
            code_root.display()
        )
    );
    let direct_status = run_bowline_with_env(
        &[
            "status",
            "--root",
            code_root.to_str().expect("code root"),
            "--project",
            "acme/web",
            "--json",
        ],
        &envs,
    );
    assert!(direct_status.status.success(), "{direct_status:?}");
    let direct_status = parse_stdout_json(direct_status);
    assert_eq!(direct_status["status"]["level"], "healthy");
    let direct_attention = direct_status["status"]["attentionItems"]
        .as_array()
        .expect("attention items");
    assert!(direct_attention.is_empty());
    assert!(direct_status["items"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("completed; inspect synced project state"))
        })
    }));

    let isolated = run_bowline_with_env_in_dir(
        &[
            "agent",
            "start",
            "acme/web",
            "--task",
            "isolated task",
            "--work-view",
            "--json",
        ],
        &envs,
        &unrelated_cwd,
    );
    assert!(isolated.status.success(), "{isolated:?}");
    let isolated_json = parse_stdout_json(isolated);
    let isolated_target = isolated_json["lease"]["writeTargetPath"]
        .as_str()
        .expect("isolated target")
        .to_string();
    let isolated_lease = isolated_json["lease"]["id"]
        .as_str()
        .expect("isolated lease id");
    let isolated_completed = run_bowline_with_env(
        &["agent", "complete", "--lease", isolated_lease, "--json"],
        &envs,
    );
    assert!(
        isolated_completed.status.success(),
        "{isolated_completed:?}"
    );
    let isolated_completed = parse_stdout_json(isolated_completed);
    assert_eq!(isolated_completed["lease"]["sessionState"], "completed");
    assert_eq!(isolated_completed["status"]["level"], "attention");
    assert!(
        isolated_completed["status"]["attentionItems"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|item| item.contains("review-ready"))))
    );
    assert_eq!(
        isolated_completed["nextActions"][0]["command"],
        format!("bowline work review {isolated_target}")
    );
    let discarded = run_bowline_with_env(&["work", "discard", &isolated_target, "--json"], &envs);
    assert!(discarded.status.success(), "{discarded:?}");
    let after_discard = run_bowline_with_env(
        &[
            "status",
            "--root",
            code_root.to_str().expect("code root"),
            "--project",
            "acme/web",
            "--json",
        ],
        &envs,
    );
    assert!(after_discard.status.success(), "{after_discard:?}");
    let after_discard = parse_stdout_json(after_discard);
    assert_eq!(after_discard["status"]["level"], "healthy");
    assert!(
        after_discard["status"]["attentionItems"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
}

#[test]
fn status_json_reports_missing_metadata_without_creating_db() {
    let db_path = unique_db("missing-status");
    let output = run_bowline_with_env(
        &["status", "--root", "~/Code", "--json"],
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
fn read_only_workspace_commands_infer_project_from_cwd() {
    let temp = TempWorkspace::new("infer-root-read-only").expect("temp workspace");
    let code_root = temp.root().join("Code");
    let project = code_root.join("apps/web");
    fs::create_dir_all(&project).expect("project dir");
    let project_display = project.display().to_string();
    let project = project.canonicalize().expect("project canonicalizes");
    let db_path = temp.root().join(".state/local.sqlite3");
    seed_two_project_events_with_root(&db_path, &code_root.display().to_string());
    let envs = [
        ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
    ];

    for args in [&["status", "--json"][..], &["events", "--json"][..]] {
        let output = run_bowline_with_env_in_dir(args, &envs, &project);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let json = parse_stdout_json(output);
        assert_ne!(json["status"], "usage-error", "{args:?}");
        assert_eq!(json["requestedPath"], project_display, "{args:?}");
        assert_eq!(json["scope"], "project");
        assert_eq!(json["projectId"], "proj_web");
    }
}

#[test]
fn setup_json_creates_explicit_missing_root_without_project_files() {
    let temp = TempWorkspace::new("cli-init-missing-root").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let code_root = home.join("Code");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &[
            "setup",
            "--root",
            code_root.to_str().expect("code root"),
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(code_root.is_dir());
    assert!(
        !db_path.exists(),
        "pending hosted login must not persist metadata under fallback workspace"
    );
    assert!(!code_root.join(".bowlineignore").exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["root"], "~/Code");
    assert_eq!(json["rootChoice"], "explicit-created");
    assert_eq!(json["login"]["status"], "login-pending");
    let next_actions = json["nextActions"].as_array().expect("next actions");
    assert!(next_actions.iter().any(|action| {
        action["command"]
            .as_str()
            .is_some_and(|command| command == "bowline setup --root ~/Code")
    }));
}

#[test]
fn login_rejects_workspace_options_owned_by_setup() {
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

    assert_eq!(output.status.code(), Some(2));
    assert!(!code_root.exists());
    assert!(!db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "login");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(
        json["error"]["message"],
        "unknown bowline login option `--root`"
    );
}

#[test]
fn setup_json_onboards_machine_with_default_root() {
    let temp = TempWorkspace::new("cli-setup-default-json").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let code_root = home.join("Code");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["setup", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(code_root.is_dir());
    assert!(db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["root"], "~/Code");
    assert_eq!(json["rootChoice"], "default-selected");
    assert_eq!(json["login"]["status"], "not-logged-in");
    assert!(json["nextActions"].is_array());
}

#[test]
fn setup_root_json_uses_explicit_root() {
    let temp = TempWorkspace::new("cli-setup-explicit-json").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let code_root = temp.root().join("CustomCode");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &[
            "setup",
            "--root",
            code_root.to_str().expect("utf8 root"),
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(code_root.is_dir());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["root"], code_root.display().to_string());
    assert_eq!(json["rootChoice"], "explicit-created");
}

#[test]
fn setup_json_uses_environment_control_plane_credentials() {
    let temp = TempWorkspace::new("cli-setup-env-token-json").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let code_root = temp.root().join("EnvCode");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &[
            "setup",
            "--root",
            code_root.to_str().expect("utf8 root"),
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
            ("BOWLINE_WORKSPACE_ID", "ws_env_setup".to_string()),
            ("BOWLINE_CONTROL_PLANE_TOKEN", "control-token".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(code_root.is_dir());
    assert!(db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["workspaceId"], "ws_env_setup");
    assert_eq!(json["root"], code_root.display().to_string());
    assert_eq!(json["rootChoice"], "explicit-created");
    assert_eq!(json["login"]["status"], "not-logged-in");
    let next_actions = json["nextActions"].as_array().expect("next actions");
    assert!(!next_actions.iter().any(|action| {
        action["label"]
            .as_str()
            .is_some_and(|label| label.contains("verification URL"))
    }));
    assert!(next_actions.iter().any(|action| {
        action["command"]
            .as_str()
            .is_some_and(|command| command == "bowline login")
    }));
}

#[test]
fn setup_json_reports_ambiguous_default_root() {
    let temp = TempWorkspace::new("cli-setup-ambiguous-json").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(home.join("Code")).expect("code root");
    fs::create_dir_all(home.join("Projects")).expect("projects root");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["setup", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["error"]["code"], "ambiguous_root");
    assert_eq!(
        json["nextActions"][0]["command"],
        "bowline setup --root ~/Code"
    );
}

#[test]
fn bare_login_no_poll_json_is_auth_only() {
    let temp = TempWorkspace::new("cli-login-default-json").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["login", "--no-poll", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert!(output.status.success());
    assert!(!db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "login");
    assert!(json.get("account").is_some());
    assert!(json.get("root").is_none());
}

#[test]
fn setup_project_json_uses_setup_project_output_contract() {
    let temp = TempWorkspace::new("cli-setup-project-json").expect("temp workspace");
    let home = temp.root().join("home");
    let code_root = home.join("Code");
    let project_path = code_root.join("app");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/index.ts"), "console.log('ready')").expect("source");
    fs::write(project_path.join("package.json"), r#"{"name":"app"}"#).expect("package");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let init = run_bowline_with_env(
        &[
            "setup",
            "--root",
            code_root.to_str().expect("utf8 root"),
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );
    assert!(init.status.success());

    let output = run_bowline_with_env(
        &[
            "setup",
            project_path.to_str().expect("utf8 project"),
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-07-02T12:00:00Z".to_string()),
        ],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert!(json.get("outcome").is_some());
    assert_eq!(json["outcome"]["projectPath"], "app");
}

#[test]
fn setup_root_json_reports_workspace_errors_as_json() {
    let temp = TempWorkspace::new("cli-login-root-json-error").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let root_file = home.join("not-a-dir");
    fs::write(&root_file, "not a directory").expect("root file");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &[
            "setup",
            "--root",
            root_file.to_str().expect("utf8 root"),
            "--json",
        ],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-27T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(3));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["status"], "failed");
}

#[test]
fn setup_json_creates_code_when_explicit_root_is_missing() {
    let temp = TempWorkspace::new("cli-init-default-code").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &[
            "setup",
            "--root",
            home.join("Code").to_str().expect("code root"),
            "--json",
        ],
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
    assert_eq!(json["rootChoice"], "explicit-created");
}

#[test]
fn bare_setup_json_creates_code_on_fresh_home() {
    let temp = TempWorkspace::new("cli-init-bare-default-code").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["setup", "--json"],
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
}

#[test]
fn bare_setup_json_requires_explicit_root_when_non_code_root_exists() {
    let temp = TempWorkspace::new("cli-init-ambiguous-root").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(home.join("Projects")).expect("projects root");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["setup", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(!home.join("Code").exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 8);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "ambiguous_root");
    assert_eq!(json["error"]["recoverability"], "user-action");
    assert_eq!(
        json["error"]["message"],
        "bowline setup found multiple existing code roots; pass an explicit root: ~/Projects"
    );
    assert_eq!(
        json["nextActions"][0]["command"],
        "bowline setup --root ~/Projects"
    );
}

#[test]
fn bare_setup_json_requires_explicit_root_when_code_plus_other_roots_exist() {
    let temp = TempWorkspace::new("cli-init-code-plus-projects").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(home.join("Code")).expect("code root");
    fs::create_dir_all(home.join("Projects")).expect("projects root");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["setup", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["command"], "setup");
    assert_eq!(json["error"]["code"], "ambiguous_root");
    assert_eq!(
        json["error"]["message"],
        "bowline setup found multiple existing code roots; pass an explicit root: ~/Code, ~/Projects"
    );
    assert_eq!(
        json["nextActions"][0]["command"],
        "bowline setup --root ~/Code"
    );
    assert_eq!(
        json["nextActions"][1]["command"],
        "bowline setup --root ~/Projects"
    );
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

    let created = run_bowline_with_env(
        &["work", "create", &project_arg, "auth-fix", "--json"],
        &envs,
    );
    assert!(created.status.success(), "{created:?}");
    let created_json = parse_stdout_json(created);
    assert_eq!(created_json["command"], "work create");
    assert_eq!(created_json["workView"]["name"], "auth-fix");
    let materialized = code_root.join(".work/apps/web/auth-fix");
    assert!(materialized.is_dir());
    assert!(materialized.join("src/index.ts").exists());

    let reused = run_bowline_with_env(
        &["work", "create", &project_arg, "auth-fix", "--json"],
        &envs,
    );
    assert!(reused.status.success(), "{reused:?}");
    let reused_json = parse_stdout_json(reused);
    assert_eq!(reused_json["action"], "reused");
    assert_eq!(
        reused_json["workView"]["id"],
        created_json["workView"]["id"]
    );

    let listed = run_bowline_with_env(&["work", "list", "--json"], &envs);
    assert!(listed.status.success());
    let listed_json = parse_stdout_json(listed);
    assert_eq!(listed_json["workViews"].as_array().unwrap().len(), 1);

    let discarded = run_bowline_with_env(&["work", "discard", "auth-fix", "--json"], &envs);
    assert!(discarded.status.success());
    let discarded_json = parse_stdout_json(discarded);
    assert_eq!(discarded_json["workView"]["lifecycle"], "discarded");

    let hidden_list = run_bowline_with_env(&["work", "list", "--json"], &envs);
    assert!(hidden_list.status.success());
    let hidden_json = parse_stdout_json(hidden_list);
    assert!(hidden_json["workViews"].as_array().unwrap().is_empty());

    let restored = run_bowline_with_env(&["work", "restore", "auth-fix", "--json"], &envs);
    assert!(restored.status.success());
    let restored_json = parse_stdout_json(restored);
    assert_eq!(restored_json["workView"]["lifecycle"], "active");

    let discarded = run_bowline_with_env(&["work", "discard", "auth-fix", "--json"], &envs);
    assert!(discarded.status.success());
    let preview = run_bowline_with_env(&["work", "cleanup", "--json"], &envs);
    assert!(preview.status.success());
    assert!(materialized.is_dir());

    let cleanup = run_bowline_with_env(&["work", "cleanup", "--apply", "--json"], &envs);
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
        &["work", "create", &project_arg, "default-db", "--json"],
        &envs,
        &["BOWLINE_METADATA_DB"],
    );
    assert!(created.status.success(), "{created:?}");
    let created_json = parse_stdout_json(created);
    assert_eq!(created_json["command"], "work create");
    assert_eq!(created_json["workView"]["name"], "default-db");
    assert!(code_root.join(".work/apps/web/default-db").is_dir());

    let listed =
        run_bowline_with_env_removed(&["work", "list", "--json"], &envs, &["BOWLINE_METADATA_DB"]);
    assert!(listed.status.success());
    let listed_json = parse_stdout_json(listed);
    assert_eq!(listed_json["workViews"].as_array().unwrap().len(), 1);
}

#[test]
fn init_json_is_not_a_public_command() {
    let temp = TempWorkspace::new("cli-init-unknown-flag").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let db_path = temp.root().join(".state").join("local.sqlite3");

    let output = run_bowline_with_env(
        &["init", "--json"],
        &[
            ("HOME", home.display().to_string()),
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(!db_path.exists());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["command"], "unknown");
    assert_eq!(json["error"]["code"], "unknown_command");
}

#[test]
fn setup_and_status_observe_existing_code_root() {
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

    let setup = run_bowline_with_env(
        &[
            "setup",
            "--root",
            code_root.to_str().expect("code root"),
            "--json",
        ],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );

    assert!(setup.status.success());
    detector.assert_unchanged().expect("source tree unchanged");
    let setup_json = parse_stdout_json(setup);
    assert_eq!(setup_json["command"], "setup");
    assert_eq!(setup_json["rootChoice"], "explicit-existing");

    let status = run_bowline_with_env(
        &[
            "status",
            "--root",
            code_root.to_str().expect("code root"),
            "--json",
        ],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );
    assert!(status.status.success());
    let status_json = parse_stdout_json(status);
    assert_eq!(status_json["status"]["level"], "healthy");
    assert_eq!(
        status_json["workspaceSummary"]["observed"]["staleRemoteTrackingRepoCount"],
        1
    );
    let status_text = serde_json::to_string(&status_json).expect("status json string");
    assert!(status_text.contains("local branches ahead of their tracking refs"));
}
