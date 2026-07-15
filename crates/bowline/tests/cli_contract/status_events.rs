use super::*;

#[test]
fn phase8_env_and_project_setup_do_not_leak_or_sync_generated_state() {
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

    let setup = run_bowline_with_env(
        &[
            "setup",
            "--root",
            code_root.to_str().expect("code root"),
            "--json",
        ],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-25T12:00:00Z".to_string()),
            ("BOWLINE_USE_FAKE_CONTROL_PLANE", "1".to_string()),
        ],
    );
    assert!(setup.status.success());
    let setup_stdout = String::from_utf8(setup.stdout).expect("setup stdout");
    assert!(!setup_stdout.contains("super-secret-value"));

    let status = run_bowline_with_env(
        &[
            "status",
            "--root",
            code_root.to_str().expect("code root"),
            "--project",
            "apps/web",
            "--json",
        ],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );
    assert!(status.status.success());
    let status_json = parse_stdout_json(status);
    let status_text = serde_json::to_string(&status_json).expect("status json serializes");
    assert!(status_text.contains("\"kind\":\"env\""));
    assert!(status_text.contains("values are redacted"));
    assert!(!status_text.contains("super-secret-value"));

    let blocked = run_bowline_with_env(
        &["setup", web_dir.to_str().expect("web dir"), "--json"],
        &[
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-25T12:00:01Z".to_string()),
        ],
    );
    assert!(blocked.status.success());
    let blocked_json = parse_stdout_json(blocked);
    assert_eq!(blocked_json["command"], "setup");
    assert_eq!(blocked_json["outcome"]["state"], "setup-blocked");
    assert!(!web_dir.join(".setup-done").exists());

    let approved = run_bowline_with_env(
        &[
            "setup",
            web_dir.to_str().expect("web dir"),
            "--yes",
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
        &[
            "status",
            "--root",
            code_root.to_str().expect("code root"),
            "--project",
            "apps/web",
            "--json",
        ],
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
        .args(["status", "--root", "~/Code", "--watch", "--json"])
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
        .args(["status", "--root", "~/Code", "--watch", "--json"])
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
        .args(["status", "--root", "~/Code", "--watch", "--human"])
        .env("BOWLINE_METADATA_DB", db_path.display().to_string())
        .env("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z")
        .env("TERM", "xterm-256color")
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
        include_str!("../../../../tests/golden/cli/status-watch.txt")
    );
}

#[test]
fn status_json_reports_daemon_component_degradation_from_metadata() {
    let db_path = unique_db("daemon-component-status");
    seed_daemon_component_status(&db_path);

    let output = run_bowline_with_env(
        &["status", "--root", "~/Code", "--json"],
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
        &["status", "--root", "~/Code", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 8);
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
        &["tui", "--root", "~/Code"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("tui fallback should be utf8");
    assert!(stdout.contains("NEEDS YOU"));
    assert!(stdout.contains("Initialize ~/Code when ready"));
}

#[test]
fn tui_rejects_json_output_mode() {
    let output = run_bowline(&["tui", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 8);
    assert_eq!(json["command"], "tui");
    assert_eq!(json["status"], "usage-error");
    assert_eq!(json["error"]["code"], "usage_error");
    let command = json["nextActions"][0]["command"]
        .as_str()
        .expect("next action command");
    assert_eq!(command, "bowline help tui --json");
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
        &[
            "resolve",
            project.to_str().expect("project path"),
            "--tui",
            "--human",
        ],
        &[("BOWLINE_GENERATED_AT", "2026-06-25T12:00:00Z".to_string())],
    );

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("resolve output should be utf8");
    assert!(stdout.contains("Resolve"));
    assert!(stdout.contains("1 unresolved conflict bundle(s) found"));
    assert!(stdout.contains("conflict_tui"));
}

#[test]
fn events_json_reports_empty_history_for_missing_metadata() {
    let db_path = unique_db("missing-events");
    let output = run_bowline_with_env(
        &["events", "--root", "~/Code", "--json"],
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
        &[
            "events",
            "--root",
            code_root.to_str().expect("code root"),
            "--project",
            "apps/web/src/index.ts",
            "--json",
        ],
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
        &["events", "--root", "~/Code", "--json"],
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
        &[
            "events",
            "--root",
            code_root.to_str().expect("code root"),
            "--project",
            "apps/web",
            "--json",
        ],
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
        &[
            "events",
            "--root",
            "~/Code",
            "--project",
            "apps/web",
            "--json",
        ],
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
        &["events", "--root", "~/Code", "--json"],
        &[("BOWLINE_METADATA_DB", db_path.display().to_string())],
    );

    assert_eq!(output.status.code(), Some(3));
    let json = parse_stdout_json(output);
    assert_eq!(json["contractVersion"], 8);
    assert_eq!(json["command"], "events");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"]["code"], "runtime_error");
}
