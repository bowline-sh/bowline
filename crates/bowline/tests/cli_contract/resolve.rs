use super::*;

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
    assert_eq!(json["contractVersion"], 8);
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

    assert_eq!(output.status.code(), Some(3));
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
    assert_eq!(denied.status.code(), Some(3));
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
    create_conflict_bundle_with_workspace_root(&bundle, false, &project);

    let output = run_bowline_with_env(
        &["resolve", project.to_str().expect("project path"), "--json"],
        &[("BOWLINE_STATE_ROOT", state_root.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert_eq!(json["conflicts"].as_array().expect("conflicts").len(), 1);
    assert_eq!(json["conflicts"][0]["id"], "conflict_same_line");
    let actions = serde_json::to_string(&json["nextActions"]).expect("actions serialize");
    assert!(actions.contains(project.to_str().expect("project path")));
    assert!(!actions.contains("bowline/private --accept"));
}

#[test]
fn resolve_json_scopes_mixed_root_actions_per_conflict() {
    let temp = TempWorkspace::new("resolve-mixed-roots").expect("temp workspace");
    let requested_root = temp.root().join("Requested Root");
    let other_root = temp.root().join("Other Root");
    let state_root = temp.root().join("state");
    let local_bundle = requested_root.join(".bowline/conflicts/conflict_requested");
    create_conflict_bundle_manifest(
        &local_bundle,
        "conflict_requested",
        "apps/web/config.txt",
        false,
        None,
    );
    let remote_bundle = state_root.join("conflicts/conflict_other");
    create_conflict_bundle_manifest(
        &remote_bundle,
        "conflict_other",
        "apps/api/config.txt",
        false,
        Some(&other_root),
    );

    let output = run_bowline_with_env(
        &[
            "resolve",
            requested_root.to_str().expect("requested root"),
            "--json",
        ],
        &[("BOWLINE_STATE_ROOT", state_root.display().to_string())],
    );

    assert!(output.status.success());
    let json = parse_stdout_json(output);
    let actions = json["availableActions"].as_array().expect("actions");
    assert!(
        !actions.iter().any(|action| {
            action["command"].as_str().is_some_and(|command| {
                command.contains("--copy-prompt") || command.contains("--agent")
            })
        }),
        "a shared prompt command must not silently select one root"
    );
    for (conflict_id, root) in [
        ("conflict_requested", &requested_root),
        ("conflict_other", &other_root),
    ] {
        let commands = actions
            .iter()
            .filter(|action| {
                action["label"]
                    .as_str()
                    .is_some_and(|label| label.ends_with(conflict_id))
            })
            .map(|action| action["command"].as_str().expect("command"))
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), 3);
        assert!(commands.iter().all(|command| {
            command.starts_with(&format!("bowline resolve '{}'", root.display()))
        }));
    }
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
    let platform = if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    };
    let db_path = database_path_for_platform(platform, &home, None);
    let code_root = temp.root().join("Code");
    let project = code_root.join("app");
    fs::create_dir_all(&project).expect("project");
    seed_daemon_start_workspace(&db_path, &code_root);
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .upsert_workspace_sync_head(&bowline_local::metadata::WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 9,
                snapshot_id: SnapshotId::new("snap-9"),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 9 },
                updated_by_device_id: Some(DeviceId::new("device-a")),
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
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
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
    assert_eq!(operation.kind, SyncOperationKind::Reconcile);
    assert_eq!(operation.state, SyncOperationState::Queued);
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
fn resolve_accept_fails_when_resolution_sync_enqueue_fails() {
    let temp = TempWorkspace::new("resolve-accept-sync-queue-failure").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let platform = if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    };
    let db_path = database_path_for_platform(platform, &home, None);
    let code_root = temp.root().join("Code");
    let project = code_root.join("app");
    fs::create_dir_all(&project).expect("project");
    seed_daemon_start_workspace(&db_path, &code_root);
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .upsert_workspace_sync_head(&bowline_local::metadata::WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 9,
                snapshot_id: SnapshotId::new("snap-9"),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 9 },
                updated_by_device_id: Some(DeviceId::new("device-a")),
            },
            observed_at: "2026-06-24T11:59:00Z".to_string(),
        })
        .expect("head stored");
    store
        .enqueue_sync_operation(&bowline_local::metadata::SyncOperationRecord {
            id: "resolve:conflict_same_line:accept:2026_06_24T12_00_00Z".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "preexisting-different-key".to_string(),
            base_version: Some(9),
            base_snapshot_id: Some("snap-9".to_string()),
            target_snapshot_id: None,
            device_id: None,
            payload_json: "{}".to_string(),
            attempt_count: 0,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: "2026-06-24T11:59:00Z".to_string(),
            updated_at: "2026-06-24T11:59:00Z".to_string(),
        })
        .expect("preexisting operation");
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
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(!output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("status summary")
            .contains("resolve metadata update failed")
    );
    assert_eq!(
        fs::read(project.join("apps").join("web").join(".env.local")).expect("project file"),
        b"SECRET=resolved\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
    assert!(store_events(&db_path).is_empty());
}

#[test]
fn resolve_accept_fails_when_existing_metadata_db_cannot_open() {
    let temp = TempWorkspace::new("resolve-accept-metadata-open-failure").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let platform = if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    };
    let db_path = database_path_for_platform(platform, &home, None);
    fs::create_dir_all(db_path.parent().expect("database parent")).expect("database parent");
    fs::create_dir_all(&db_path).expect("database path directory");

    let code_root = temp.root().join("Code");
    let project = code_root.join("app");
    fs::create_dir_all(&project).expect("project");
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
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(!output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("status summary")
            .contains("resolve metadata update failed")
    );
    assert_eq!(
        fs::read(project.join("apps").join("web").join(".env.local")).expect("project file"),
        b"SECRET=resolved\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"accepted\""));
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

    assert_eq!(output.status.code(), Some(3));
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

    assert_eq!(output.status.code(), Some(3));
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

    assert_eq!(output.status.code(), Some(3));
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

    assert_eq!(output.status.code(), Some(3));
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

    assert_eq!(output.status.code(), Some(3));
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

        assert_eq!(output.status.code(), Some(3));
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

    assert_eq!(output.status.code(), Some(3));
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

    assert_eq!(output.status.code(), Some(3));
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
    let platform = if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    };
    let db_path = database_path_for_platform(platform, &home, None);
    let code_root = temp.root().join("Code");
    let project = code_root.join("app");
    fs::create_dir_all(&project).expect("project");
    seed_daemon_start_workspace(&db_path, &code_root);
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .upsert_workspace_sync_head(&bowline_local::metadata::WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 10,
                snapshot_id: SnapshotId::new("snap-10"),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 10 },
                updated_by_device_id: Some(DeviceId::new("device-b")),
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
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
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
    assert_eq!(operation.kind, SyncOperationKind::Reconcile);
    assert_eq!(operation.state, SyncOperationState::Queued);
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
fn resolve_reject_fails_before_applying_when_resolution_sync_enqueue_fails() {
    let temp = TempWorkspace::new("resolve-reject-sync-queue-failure").expect("temp workspace");
    let home = temp.root().join("home");
    fs::create_dir_all(&home).expect("home");
    let platform = if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Other
    };
    let db_path = database_path_for_platform(platform, &home, None);
    let code_root = temp.root().join("Code");
    let project = code_root.join("app");
    fs::create_dir_all(&project).expect("project");
    seed_daemon_start_workspace(&db_path, &code_root);
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .upsert_workspace_sync_head(&bowline_local::metadata::WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 10,
                snapshot_id: SnapshotId::new("snap-10"),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 10 },
                updated_by_device_id: Some(DeviceId::new("device-b")),
            },
            observed_at: "2026-06-24T11:59:00Z".to_string(),
        })
        .expect("head stored");
    store
        .enqueue_sync_operation(&bowline_local::metadata::SyncOperationRecord {
            id: "resolve:conflict_same_line:reject:2026_06_24T12_00_00Z".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "preexisting-different-key".to_string(),
            base_version: Some(10),
            base_snapshot_id: Some("snap-10".to_string()),
            target_snapshot_id: None,
            device_id: None,
            payload_json: "{}".to_string(),
            attempt_count: 0,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: "2026-06-24T11:59:00Z".to_string(),
            updated_at: "2026-06-24T11:59:00Z".to_string(),
        })
        .expect("preexisting operation");
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
            ("BOWLINE_METADATA_DB", db_path.display().to_string()),
            ("BOWLINE_GENERATED_AT", "2026-06-24T12:00:00Z".to_string()),
        ],
    );

    assert!(!output.status.success());
    let json = parse_stdout_json(output);
    assert_eq!(json["status"]["level"], "attention");
    assert!(
        json["status"]["summary"]
            .as_str()
            .expect("status summary")
            .contains("resolve metadata update failed")
    );
    assert_eq!(
        fs::read(project.join("apps").join("web").join(".env.local")).expect("project file"),
        b"SECRET=remote\n"
    );
    let manifest = fs::read_to_string(bundle.join("manifest.json")).expect("manifest");
    assert!(!manifest.contains("\"state\": \"rejected\""));
    assert!(store_events(&db_path).is_empty());
}

fn store_events(db_path: &Path) -> Vec<bowline_core::events::WorkspaceEvent> {
    let store = MetadataStore::open(db_path).expect("metadata reopens");
    store.list_events(20).expect("events read")
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
    let mut record = bowline_local::sync::ConflictRecord::path_conflict("apps/web/link");
    record.id = "conflict_kind".to_string();
    record.bundle_path = Some(bundle.clone());
    record.base_snapshot_id = Some("snap_fixture_base".to_string());
    record.remote_snapshot_id = Some("snap_fixture_remote".to_string());
    write_conflict_manifest(&bundle, &record);
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

    assert_eq!(output.status.code(), Some(3));
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
