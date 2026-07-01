use super::*;

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
