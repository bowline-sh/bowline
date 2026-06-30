#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt};

use bowline_control_plane::FakeControlPlaneClient;
use bowline_core::{
    ids::{ContentId, DeviceId, LeaseId, ProjectId, WorkspaceId},
    status::StatusItemKind,
    workspace_graph::{HydrationState, NamespaceEntryKind},
};
use bowline_local::{
    env::{
        EnvLineKind, EnvProviderRecord, EnvProviderRequest, EnvReadScope, EnvRecordFreshness,
        EnvRecordRestriction, parse_env_text, resolve_env_provider_request,
    },
    init::{InitOptions, initialize_root},
    metadata::{MetadataStore, ProjectedNodeRecord, SetupReceiptRecord},
    setup::{PackageManagerIdentity, collect_receipt_identity_inputs},
    setup::{PrewarmOptions, PrewarmState, prewarm_project},
    status::{StatusOptions, compose_status},
    sync::{SyncRunner, SyncRunnerOptions, SyncTickOutcome},
    workspace::TempWorkspace,
};
use bowline_storage::{LocalByteStore, StorageKey};

#[test]
fn two_device_phase8_env_setup_and_local_regeneration_path_just_works() {
    let source = TempWorkspace::new("phase8-device-a-code").expect("source workspace");
    source.create_project("app").expect("project");
    source
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    source
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    source
        .write_project_file("app", ".env.local", b"BOWLINE_PHASE8_SECRET=sync-me\n")
        .expect("env");
    source
        .write_project_file(
            "app",
            ".bowlinesetup",
            b"mkdir -p node_modules/react && printf ready > node_modules/react/.phase8-ready\n",
        )
        .expect("setup");
    source
        .create_generated_folder("app", "node_modules")
        .expect("source generated folder");

    let target = TempWorkspace::new("phase8-device-b-code").expect("target workspace");
    let state = TempWorkspace::new("phase8-shared-state").expect("state workspace");
    let target_state = TempWorkspace::new("phase8-device-b-state").expect("target state");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 28).expect("byte store");
    let storage_key = StorageKey::deterministic(28);
    let workspace_id = WorkspaceId::new("ws_code");
    let content_key = [28_u8; 32];

    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: state.root().join("device-a"),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: content_key,
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-25T10:00:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source upload"),
        SyncTickOutcome::Uploaded(_)
    ));

    let target_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: target.root().to_path_buf(),
            state_root: target_state.root().join("sync-state"),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: content_key,
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-25T10:00:01Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        target_runner.tick().expect("target import"),
        SyncTickOutcome::Imported(_)
    ));

    let target_env = target.root().join("app/.env.local");
    assert_eq!(
        fs::read(&target_env).expect("materialized env"),
        b"BOWLINE_PHASE8_SECRET=sync-me\n"
    );
    assert_eq!(
        fs::metadata(&target_env)
            .expect("env metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        !target.root().join("app/node_modules").exists(),
        "dependency folders should not sync across devices"
    );

    let db_path = target_state.root().join("metadata.sqlite3");
    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(target.root().display().to_string()),
        generated_at: "2026-06-25T10:00:02Z".to_string(),
    })
    .expect("target init imports local metadata");
    let store = MetadataStore::open(&db_path).expect("metadata store");
    let env_records = store
        .env_records(&workspace_id)
        .expect("env records imported");
    assert_eq!(env_records.len(), 1);
    assert_eq!(env_records[0].source_path, "app/.env.local");
    assert_eq!(env_records[0].key_name, "BOWLINE_PHASE8_SECRET");
    assert!(!format!("{env_records:?}").contains("sync-me"));
    assert!(!env_records[0].encrypted_locator_json.contains("sync-me"));

    let project = store
        .current_project_by_path("app")
        .expect("project lookup")
        .expect("project exists");
    let provider_records = provider_records_from_file(&project.id, &target_env);
    let provider_response = resolve_env_provider_request(
        &EnvProviderRequest {
            caller_device_id: Some(DeviceId::new("device-b")),
            lease_id: Some(LeaseId::new("lease-phase8")),
            project_id: project.id.clone(),
            read_scope: EnvReadScope::Lease,
            profile: "local".to_string(),
        },
        &provider_records,
    );
    assert_eq!(
        provider_response.values["BOWLINE_PHASE8_SECRET"].as_bytes(),
        b"sync-me"
    );
    assert!(
        !format!("{provider_response:?}").contains("sync-me"),
        "lease-facing provider debug output must not leak env values"
    );

    let blocked = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: false,
        trigger: "lease:lease-phase8".to_string(),
        generated_at: "2026-06-25T10:00:03Z".to_string(),
    })
    .expect("prewarm blocks for first-seen setup");
    assert_eq!(blocked.state, PrewarmState::SetupBlocked);
    assert!(
        !target.root().join("app/node_modules").exists(),
        "blocked setup must not create local dependency output"
    );

    let approved = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:lease-phase8".to_string(),
        generated_at: "2026-06-25T10:00:04Z".to_string(),
    })
    .expect("approved prewarm runs setup");
    assert_eq!(approved.state, PrewarmState::Hot);
    assert_eq!(
        fs::read(target.root().join("app/node_modules/react/.phase8-ready"))
            .expect("local regenerate output"),
        b"ready"
    );

    let status = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(target.root().join("app").display().to_string()),
        workspace_scope: false,
        generated_at: "2026-06-25T10:00:05Z".to_string(),
    })
    .expect("status composes");
    let status_json = serde_json::to_string(&status).expect("status json");
    assert!(
        status
            .items
            .iter()
            .any(|item| item.kind == StatusItemKind::Env)
    );
    assert!(
        status
            .items
            .iter()
            .any(|item| item.kind == StatusItemKind::Setup)
    );
    assert!(
        !status
            .status
            .attention_items
            .iter()
            .any(|item| item.contains("approval-required"))
    );
    assert!(!status_json.contains("sync-me"));
}

#[test]
fn env_import_records_redacted_source_pack_metadata_without_secret_locators() {
    let workspace = TempWorkspace::new("phase8-env-locator").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", ".env.local", b"BOWLINE_PHASE8_SECRET=sync-me\n")
        .expect("env");
    let state = TempWorkspace::new("phase8-env-locator-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:00:00Z".to_string(),
    })
    .expect("init");
    let store = MetadataStore::open(&db_path).expect("store");
    let records = store
        .env_records(&WorkspaceId::new("ws_code"))
        .expect("records");

    assert_eq!(records.len(), 1);
    let locator = &records[0].encrypted_locator_json;
    assert!(locator.contains(r#""storage":"source-pack-file""#));
    assert!(!locator.contains("existing-pack-object"));
    assert!(!locator.contains("contentId"));
    assert!(!locator.contains("byteLen"));
    assert!(!locator.contains("sync-me"));
}

#[test]
fn prewarm_failed_setup_redacts_logs_and_marks_setup_blocked() {
    let workspace = TempWorkspace::new("phase8-setup-failure").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file(
            "app",
            ".env.local",
            b"BORING_SECRET=ordinary-secret-value\nMULTI_SECRET=ordinary secret value\nPIN=1234\n",
        )
        .expect("env");
    let long_secret = "z".repeat(70_000);
    fs::write(
        workspace.root().join("app/.env.truncation"),
        format!("LONG_SECRET={long_secret}\n"),
    )
    .expect("long env");
    workspace
        .write_project_file(
            "app",
            ".bowlinesetup",
            b"printf 'api key: ordinary-secret-value\\n'; sed -n 's/^PIN=//p' .env.local; printf 'MULTI_SECRET=ordinary secret value\\n'; sed -n 's/^LONG_SECRET=//p' .env.truncation; printf 'OPENAI_API_KEY=sk-phase8secret1234567890\\n'; printf 'ghp_phase8secretabcdefghijklmnopqrstuvwxyz\\n' >&2; exit 7\nmkdir -p node_modules/should-not-run\n",
        )
        .expect("setup");
    let state = TempWorkspace::new("phase8-setup-failure-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:10:00Z".to_string(),
    })
    .expect("init");
    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-failure".to_string(),
        generated_at: "2026-06-25T11:10:01Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::SetupBlocked);
    assert!(
        !workspace
            .root()
            .join("app/node_modules/should-not-run")
            .exists()
    );
    assert_eq!(project_hot_state(&db_path), "setup.blocked");

    let store = MetadataStore::open(&db_path).expect("store");
    let receipts = store
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts");
    let failed = receipts
        .iter()
        .find(|receipt| receipt.state == "failed")
        .expect("failed receipt");
    let output_path = failed.output_path.as_ref().expect("output path");
    let output = fs::read_to_string(output_path).expect("redacted setup log");
    assert!(!failed.command.contains("ordinary-secret-value"));
    assert!(!failed.command.contains("ordinary secret value"));
    assert!(!failed.command.contains("sk-phase8secret1234567890"));
    assert!(
        !failed
            .command
            .contains("ghp_phase8secretabcdefghijklmnopqrstuvwxyz")
    );
    assert!(!failed.receipt_json.contains("ordinary-secret-value"));
    assert!(!failed.receipt_json.contains("ordinary secret value"));
    assert!(!failed.receipt_json.contains("sk-phase8secret1234567890"));
    assert!(
        !failed
            .receipt_json
            .contains("ghp_phase8secretabcdefghijklmnopqrstuvwxyz")
    );
    assert!(!output.contains("ordinary-secret-value"));
    assert!(!output.contains("ordinary secret value"));
    assert!(!output.contains("secret value"));
    assert!(!output.contains("1234"));
    assert!(!output.contains(&long_secret[..1024]));
    assert!(!output.contains("sk-phase8secret1234567890"));
    assert!(!output.contains("ghp_phase8secretabcdefghijklmnopqrstuvwxyz"));
    assert!(output.contains("[redacted]"));

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(workspace.root().join("app").display().to_string()),
        workspace_scope: false,
        generated_at: "2026-06-25T11:10:02Z".to_string(),
    })
    .expect("status");
    let status_json = serde_json::to_string(&status).expect("status json");
    assert!(
        status
            .items
            .iter()
            .any(|item| item.kind == StatusItemKind::Setup)
    );
    assert!(!status_json.contains("sk-phase8secret1234567890"));
    assert!(!status_json.contains("ghp_phase8secretabcdefghijklmnopqrstuvwxyz"));

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:10:03Z".to_string(),
    })
    .expect("init refresh");
    let refreshed_status = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(workspace.root().join("app").display().to_string()),
        workspace_scope: false,
        generated_at: "2026-06-25T11:10:04Z".to_string(),
    })
    .expect("refreshed status");
    assert!(
        refreshed_status
            .status
            .attention_items
            .iter()
            .any(|item| item == "Setup for app needs attention: failed.")
    );
}

#[test]
fn prewarm_redacts_workspace_parent_env_values_from_setup_logs() {
    let workspace = TempWorkspace::new("phase8-setup-parent-env").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    fs::write(
        workspace.root().join(".env.local"),
        b"ROOT_SECRET=workspace-parent-secret\n",
    )
    .expect("parent env");
    workspace
        .write_project_file(
            "app",
            ".bowlinesetup",
            b"sed -n 's/^ROOT_SECRET=//p' ../.env.local; exit 7\n",
        )
        .expect("setup");
    let state = TempWorkspace::new("phase8-setup-parent-env-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:12:00Z".to_string(),
    })
    .expect("init");
    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-parent-env".to_string(),
        generated_at: "2026-06-25T11:12:01Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::SetupBlocked);
    let store = MetadataStore::open(&db_path).expect("store");
    let failed = store
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts")
        .into_iter()
        .find(|receipt| receipt.state == "failed")
        .expect("failed receipt");
    let output =
        fs::read_to_string(failed.output_path.expect("output path")).expect("redacted setup log");
    assert!(!output.contains("workspace-parent-secret"));
    assert!(output.contains("[redacted]"));
}

#[test]
fn prewarm_kills_commands_that_exceed_setup_output_limit() {
    let workspace = TempWorkspace::new("phase8-setup-output-limit").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", ".bowlinesetup", b"yes noisy-output\n")
        .expect("setup");
    let state = TempWorkspace::new("phase8-setup-output-limit-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:12:10Z".to_string(),
    })
    .expect("init");
    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-output-limit".to_string(),
        generated_at: "2026-06-25T11:12:11Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::SetupBlocked);
    let store = MetadataStore::open(&db_path).expect("store");
    let failed = store
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts")
        .into_iter()
        .find(|receipt| receipt.state == "failed")
        .expect("failed receipt");
    let output =
        fs::read_to_string(failed.output_path.expect("output path")).expect("redacted setup log");
    assert!(output.contains("output exceeded the local log limit"));
    assert!(output.len() < 1024);
}

#[cfg(unix)]
#[test]
fn prewarm_output_limit_kills_noisy_descendant_processes() {
    let workspace = TempWorkspace::new("phase8-setup-output-limit-child").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file(
            "app",
            ".bowlinesetup",
            b"sh -c 'yes child-noise & sleep 10'\n",
        )
        .expect("setup");
    let state = TempWorkspace::new("phase8-setup-output-limit-child-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:12:15Z".to_string(),
    })
    .expect("init");
    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-output-limit-child".to_string(),
        generated_at: "2026-06-25T11:12:16Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::SetupBlocked);
    let failed = MetadataStore::open(&db_path)
        .expect("store")
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts")
        .into_iter()
        .find(|receipt| receipt.state == "failed")
        .expect("failed receipt");
    let output =
        fs::read_to_string(failed.output_path.expect("output path")).expect("redacted setup log");
    assert!(output.contains("output exceeded the local log limit"));
}

#[test]
fn prewarm_runs_setup_for_root_level_project() {
    let workspace = TempWorkspace::new("phase8-root-project-setup").expect("workspace");
    workspace
        .write_file("package.json", br#"{"name":"root-app"}"#)
        .expect("package");
    workspace
        .write_file(".bowlinesetup", b"printf ready > .setup-ready\n")
        .expect("setup");
    let state = TempWorkspace::new("phase8-root-project-setup-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:12:20Z".to_string(),
    })
    .expect("init");
    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: ".".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-root-project".to_string(),
        generated_at: "2026-06-25T11:12:21Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::Hot);
    assert_eq!(
        fs::read_to_string(workspace.root().join(".setup-ready")).expect("setup output"),
        "ready"
    );
}

#[test]
fn approved_setup_clears_stale_approval_required_attention() {
    let workspace = TempWorkspace::new("phase8-setup-approval-clear").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", ".bowlinesetup", b"printf ready > .setup-ready\n")
        .expect("setup");
    let state = TempWorkspace::new("phase8-setup-approval-clear-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:13:00Z".to_string(),
    })
    .expect("init");
    let blocked = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: false,
        trigger: "lease:phase8-approval-clear".to_string(),
        generated_at: "2026-06-25T11:13:01Z".to_string(),
    })
    .expect("blocked prewarm");
    assert_eq!(blocked.state, PrewarmState::SetupBlocked);
    let approved = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-approval-clear".to_string(),
        generated_at: "2026-06-25T11:13:02Z".to_string(),
    })
    .expect("approved prewarm");
    assert_eq!(approved.state, PrewarmState::Hot);

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(workspace.root().join("app").display().to_string()),
        workspace_scope: false,
        generated_at: "2026-06-25T11:13:03Z".to_string(),
    })
    .expect("status");
    assert!(
        !status
            .status
            .attention_items
            .iter()
            .any(|item| item.contains("approval-required"))
    );
    let status_json = serde_json::to_string(&status).expect("status json");
    assert!(!status_json.contains("approval-required"));
    let receipts = MetadataStore::open(&db_path)
        .expect("store")
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts");
    assert!(
        receipts
            .iter()
            .any(|receipt| receipt.state == "approved" && receipt.approval_state == "approved")
    );
    assert!(
        !receipts
            .iter()
            .any(|receipt| receipt.state == "approval-required")
    );
}

#[test]
fn setup_attention_scans_receipts_beyond_rendered_status_items() {
    let workspace = TempWorkspace::new("phase8-setup-attention-window").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let state = TempWorkspace::new("phase8-setup-attention-window-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:14:00Z".to_string(),
    })
    .expect("init");
    let store = MetadataStore::open(&db_path).expect("store");
    let project = store
        .current_project_by_path("app")
        .expect("project lookup")
        .expect("project");
    store
        .set_project_hot_state(&workspace_id, &project.id, "setup.blocked")
        .expect("blocked hot state");
    for (id, state) in [
        ("setup_a", "completed"),
        ("setup_b", "completed"),
        ("setup_c", "completed"),
        ("setup_z", "failed"),
    ] {
        store
            .upsert_setup_receipt(&SetupReceiptRecord {
                id: id.to_string(),
                workspace_id: workspace_id.clone(),
                project_id: Some(project.id.clone()),
                command: format!("command {id}"),
                state: state.to_string(),
                recipe_hash: "recipe-window".to_string(),
                approval_state: "approved".to_string(),
                trigger: "test".to_string(),
                cwd: "app".to_string(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                env_profile: "default".to_string(),
                output_path: None,
                redacted_summary: format!("receipt {state}"),
                receipt_json: "{}".to_string(),
                updated_at: "2026-06-25T11:14:01Z".to_string(),
            })
            .expect("receipt");
    }

    let status = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(workspace.root().join("app").display().to_string()),
        workspace_scope: false,
        generated_at: "2026-06-25T11:14:02Z".to_string(),
    })
    .expect("status");

    assert!(
        status
            .status
            .attention_items
            .iter()
            .any(|item| item == "Setup for app needs attention: failed.")
    );
    assert!(
        !status.items.iter().any(|item| {
            item.subject
                .as_ref()
                .is_some_and(|subject| subject.id == "setup_z")
        }),
        "the failed receipt is intentionally outside the rendered receipt window"
    );
}

#[test]
fn successful_setup_rerun_clears_stale_failed_receipt_attention() {
    let workspace = TempWorkspace::new("phase8-setup-stale-failure").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let state = TempWorkspace::new("phase8-setup-stale-failure-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:14:10Z".to_string(),
    })
    .expect("init");
    let store = MetadataStore::open(&db_path).expect("store");
    let project = store
        .current_project_by_path("app")
        .expect("project lookup")
        .expect("project");
    for (id, state, updated_at) in [
        ("setup_old_failed", "failed", "2026-06-25T11:14:11Z"),
        ("setup_new_completed", "completed", "2026-06-25T11:14:12Z"),
    ] {
        store
            .upsert_setup_receipt(&SetupReceiptRecord {
                id: id.to_string(),
                workspace_id: workspace_id.clone(),
                project_id: Some(project.id.clone()),
                command: "pnpm install".to_string(),
                state: state.to_string(),
                recipe_hash: "recipe-stale-failure".to_string(),
                approval_state: "approved".to_string(),
                trigger: "test".to_string(),
                cwd: "app".to_string(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                env_profile: "default".to_string(),
                output_path: None,
                redacted_summary: format!("receipt {state}"),
                receipt_json: "{}".to_string(),
                updated_at: updated_at.to_string(),
            })
            .expect("receipt");
    }
    store
        .set_project_hot_state(&workspace_id, &project.id, "hot")
        .expect("hot state");

    let status = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(workspace.root().join("app").display().to_string()),
        workspace_scope: false,
        generated_at: "2026-06-25T11:14:13Z".to_string(),
    })
    .expect("status");

    assert!(
        !status
            .status
            .attention_items
            .iter()
            .any(|item| item.contains("failed"))
    );
}

#[test]
fn init_refresh_purges_env_records_for_deleted_env_files() {
    let workspace = TempWorkspace::new("phase8-env-delete").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let env_path = workspace
        .write_project_file("app", ".env.local", b"STALE_SECRET=delete-me\n")
        .expect("env");
    let state = TempWorkspace::new("phase8-env-delete-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:15:00Z".to_string(),
    })
    .expect("first init");
    assert_eq!(
        MetadataStore::open(&db_path)
            .expect("store")
            .env_records(&WorkspaceId::new("ws_code"))
            .expect("records")
            .len(),
        1
    );

    fs::remove_file(env_path).expect("delete env");
    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:15:01Z".to_string(),
    })
    .expect("second init");

    assert!(
        MetadataStore::open(&db_path)
            .expect("store")
            .env_records(&WorkspaceId::new("ws_code"))
            .expect("records")
            .is_empty()
    );
}

#[test]
fn prewarm_does_not_rerun_completed_setup_receipts() {
    let workspace = TempWorkspace::new("phase8-setup-idempotent").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file(
            "app",
            ".bowlinesetup",
            b"printf run >> .setup-runs\nprintf run >> .setup-runs\n",
        )
        .expect("setup");
    let state = TempWorkspace::new("phase8-setup-idempotent-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:18:00Z".to_string(),
    })
    .expect("init");

    for timestamp in ["2026-06-25T11:18:01Z", "2026-06-25T11:18:02Z"] {
        let outcome = prewarm_project(PrewarmOptions {
            db_path: Some(db_path.clone()),
            project_path: "app".to_string(),
            approve_setup: true,
            trigger: "lease:phase8-idempotent".to_string(),
            generated_at: timestamp.to_string(),
        })
        .expect("prewarm");
        assert_eq!(outcome.state, PrewarmState::Hot);
    }

    assert_eq!(
        fs::read_to_string(workspace.root().join("app/.setup-runs")).expect("setup runs"),
        "runrun"
    );
}

#[test]
fn empty_setup_recipe_does_not_require_approval() {
    let workspace = TempWorkspace::new("phase8-empty-setup").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", ".bowlinesetup", b"# nothing to run\n\n")
        .expect("setup");
    let state = TempWorkspace::new("phase8-empty-setup-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:18:05Z".to_string(),
    })
    .expect("init");
    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: false,
        trigger: "lease:phase8-empty-setup".to_string(),
        generated_at: "2026-06-25T11:18:06Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::NoSetupNeeded);
    let receipts = MetadataStore::open(&db_path)
        .expect("store")
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts");
    assert!(receipts.is_empty());
}

#[test]
fn explicit_setup_reruns_after_lockfile_changes() {
    let workspace = TempWorkspace::new("phase8-explicit-lockfile").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "package.json",
            br#"{"name":"app","packageManager":"pnpm@9.0.0"}"#,
        )
        .expect("package");
    workspace
        .write_project_file(
            "app",
            "pnpm-lock.yaml",
            b"lockfileVersion: '9.0'\nfirst: true\n",
        )
        .expect("lockfile");
    workspace
        .write_project_file("app", ".bowlinesetup", b"printf run >> .setup-runs\n")
        .expect("setup");
    let state = TempWorkspace::new("phase8-explicit-lockfile-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:18:10Z".to_string(),
    })
    .expect("init");
    let first = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-explicit-lockfile".to_string(),
        generated_at: "2026-06-25T11:18:11Z".to_string(),
    })
    .expect("first prewarm");
    assert_eq!(first.state, PrewarmState::Hot);
    fs::write(
        workspace.root().join("app/pnpm-lock.yaml"),
        b"lockfileVersion: '9.0'\nsecond: true\n",
    )
    .expect("changed lockfile");
    let second = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-explicit-lockfile".to_string(),
        generated_at: "2026-06-25T11:18:12Z".to_string(),
    })
    .expect("second prewarm");

    assert_eq!(second.state, PrewarmState::Hot);
    assert_eq!(
        fs::read_to_string(workspace.root().join("app/.setup-runs")).expect("setup runs"),
        "runrun"
    );
    let completed = MetadataStore::open(&db_path)
        .expect("store")
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts")
        .into_iter()
        .filter(|receipt| receipt.command == "printf run >> .setup-runs")
        .filter(|receipt| receipt.state == "completed")
        .count();
    assert_eq!(completed, 2);
}

#[test]
fn inferred_setup_reruns_after_lockfile_changes() {
    let workspace = TempWorkspace::new("phase8-inferred-lockfile").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "Cargo.toml",
            b"[package]\nname = \"bowline_phase8_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("cargo toml");
    workspace
        .write_project_file("app", "src/lib.rs", b"pub fn fixture() {}\n")
        .expect("cargo source");
    workspace
        .write_project_file(
            "app",
            "Cargo.lock",
            b"# first\nversion = 4\n\n[[package]]\nname = \"bowline_phase8_fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("cargo lock");
    let state = TempWorkspace::new("phase8-inferred-lockfile-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:19:00Z".to_string(),
    })
    .expect("init");
    for timestamp in ["2026-06-25T11:19:01Z", "2026-06-25T11:19:02Z"] {
        let outcome = prewarm_project(PrewarmOptions {
            db_path: Some(db_path.clone()),
            project_path: "app".to_string(),
            approve_setup: true,
            trigger: "lease:phase8-inferred-lockfile".to_string(),
            generated_at: timestamp.to_string(),
        })
        .expect("prewarm");
        assert_eq!(outcome.state, PrewarmState::Hot);
        fs::write(
            workspace.root().join("app/Cargo.lock"),
            b"# second\nversion = 4\n\n[[package]]\nname = \"bowline_phase8_fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("changed cargo lock");
    }

    let completed = MetadataStore::open(&db_path)
        .expect("store")
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts")
        .into_iter()
        .filter(|receipt| receipt.recipe_hash == "inferred:Cargo.lock")
        .filter(|receipt| receipt.state == "completed")
        .count();
    assert_eq!(completed, 2);
}

#[test]
fn inferred_setup_reruns_after_toolchain_changes_without_lockfile_changes() {
    let workspace = TempWorkspace::new("phase8-inferred-toolchain").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "Cargo.toml",
            b"[package]\nname = \"bowline_phase8_toolchain_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("cargo toml");
    workspace
        .write_project_file("app", "src/lib.rs", b"pub fn fixture() {}\n")
        .expect("cargo source");
    workspace
        .write_project_file(
            "app",
            "Cargo.lock",
            b"version = 4\n\n[[package]]\nname = \"bowline_phase8_toolchain_fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("cargo lock");
    workspace
        .write_project_file("app", ".node-version", b"24\n")
        .expect("toolchain");
    let state = TempWorkspace::new("phase8-inferred-toolchain-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:19:10Z".to_string(),
    })
    .expect("init");
    let first = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-inferred-toolchain".to_string(),
        generated_at: "2026-06-25T11:19:11Z".to_string(),
    })
    .expect("first prewarm");
    assert_eq!(first.state, PrewarmState::Hot);
    fs::write(workspace.root().join("app/.node-version"), b"25\n").expect("changed toolchain");
    let second = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-inferred-toolchain".to_string(),
        generated_at: "2026-06-25T11:19:12Z".to_string(),
    })
    .expect("second prewarm");
    assert_eq!(second.state, PrewarmState::Hot);

    let completed = MetadataStore::open(&db_path)
        .expect("store")
        .setup_receipts(&WorkspaceId::new("ws_code"))
        .expect("receipts")
        .into_iter()
        .filter(|receipt| receipt.recipe_hash == "inferred:Cargo.lock")
        .filter(|receipt| receipt.state == "completed")
        .count();
    assert_eq!(completed, 2);
}

#[test]
fn completed_inferred_setup_with_lifecycle_hooks_does_not_block_again() {
    let workspace = TempWorkspace::new("phase8-inferred-approved").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file(
            "app",
            "package.json",
            br#"{"name":"app","scripts":{"postinstall":"node build.js"}}"#,
        )
        .expect("package");
    workspace
        .write_project_file("app", "pnpm-lock.yaml", b"lockfileVersion: '9.0'\n")
        .expect("lockfile");
    let state = TempWorkspace::new("phase8-inferred-approved-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:21:00Z".to_string(),
    })
    .expect("init");
    let store = MetadataStore::open(&db_path).expect("store");
    let project = store
        .current_project_by_path("app")
        .expect("project lookup")
        .expect("project");
    let command_text = "pnpm install --frozen-lockfile --ignore-scripts";
    let receipt_key = inferred_receipt_key_for_test(
        workspace.root().join("app").as_path(),
        "pnpm-lock.yaml",
        command_text,
        PackageManagerIdentity {
            name: "pnpm".to_string(),
            command: "pnpm".to_string(),
            declared: None,
            resolved_path: None,
            version: None,
        },
    );
    let receipt_id = setup_receipt_id_for_test(
        &workspace_id,
        &project.id,
        "inferred:pnpm-lock.yaml",
        &receipt_key,
    );
    store
        .upsert_setup_receipt(&SetupReceiptRecord {
            id: receipt_id,
            workspace_id: workspace_id.clone(),
            project_id: Some(project.id),
            command: command_text.to_string(),
            state: "completed".to_string(),
            recipe_hash: "inferred:pnpm-lock.yaml".to_string(),
            approval_state: "approved".to_string(),
            trigger: "test".to_string(),
            cwd: "app".to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            env_profile: "default".to_string(),
            output_path: None,
            redacted_summary: "already completed".to_string(),
            receipt_json: "{}".to_string(),
            updated_at: "2026-06-25T11:21:01Z".to_string(),
        })
        .expect("receipt");

    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path),
        project_path: "app".to_string(),
        approve_setup: false,
        trigger: "lease:phase8-inferred-approved".to_string(),
        generated_at: "2026-06-25T11:21:02Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::Hot);
}

#[test]
fn prewarm_infrastructure_error_leaves_project_setup_blocked() {
    let workspace = TempWorkspace::new("phase8-prewarm-infra-error").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", b"{ not json")
        .expect("package");
    let state = TempWorkspace::new("phase8-prewarm-infra-error-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:20:00Z".to_string(),
    })
    .expect("init");
    let error = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-infra-error".to_string(),
        generated_at: "2026-06-25T11:20:01Z".to_string(),
    })
    .expect_err("invalid inferred setup metadata should fail");

    assert!(error.to_string().contains("setup inference"));
    assert_eq!(project_hot_state(&db_path), "setup.blocked");
}

#[cfg(unix)]
#[test]
fn prewarm_rejects_project_root_replaced_by_symlink() {
    use std::os::unix::fs::symlink;

    let workspace = TempWorkspace::new("phase8-prewarm-symlink-root").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", ".bowlinesetup", b"printf should-not-run > outside\n")
        .expect("setup");
    let outside = TempWorkspace::new("phase8-prewarm-symlink-outside").expect("outside");
    fs::write(
        outside.root().join(".bowlinesetup"),
        b"printf owned > pwned\n",
    )
    .expect("outside setup");
    let state = TempWorkspace::new("phase8-prewarm-symlink-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:30:00Z".to_string(),
    })
    .expect("init");
    fs::remove_dir_all(workspace.root().join("app")).expect("remove app");
    symlink(outside.root(), workspace.root().join("app")).expect("project root symlink");

    let error = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-symlink".to_string(),
        generated_at: "2026-06-25T11:30:01Z".to_string(),
    })
    .expect_err("symlinked project root should be rejected");

    assert!(error.to_string().contains("not a normal directory"));
    assert!(!outside.root().join("pwned").exists());
}

#[cfg(unix)]
#[test]
fn prewarm_rejects_symlinked_setup_recipe() {
    use std::os::unix::fs::symlink;

    let workspace = TempWorkspace::new("phase8-prewarm-symlink-recipe").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let outside = TempWorkspace::new("phase8-prewarm-symlink-recipe-outside").expect("outside");
    fs::write(
        outside.root().join("payload.bowlinesetup"),
        b"printf owned > pwned\n",
    )
    .expect("outside setup");
    symlink(
        outside.root().join("payload.bowlinesetup"),
        workspace.root().join("app/.bowlinesetup"),
    )
    .expect("setup symlink");
    let state = TempWorkspace::new("phase8-prewarm-symlink-recipe-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");

    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-25T11:31:00Z".to_string(),
    })
    .expect("init");
    let error = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: true,
        trigger: "lease:phase8-symlink-recipe".to_string(),
        generated_at: "2026-06-25T11:31:01Z".to_string(),
    })
    .expect_err("symlinked setup recipe should be rejected");

    assert!(error.to_string().contains("not a normal directory"));
    assert!(!workspace.root().join("app/pwned").exists());
}

#[test]
fn prewarm_queues_and_settles_hot_project_prefetch_for_local_project_bytes() {
    let workspace = TempWorkspace::new("phase8-prewarm-hot-prefetch").expect("workspace");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    let state = TempWorkspace::new("phase8-prewarm-hot-prefetch-state").expect("state");
    let db_path = state.root().join("metadata.sqlite3");
    initialize_root(InitOptions {
        db_path: Some(db_path.clone()),
        requested_root: Some(workspace.root().display().to_string()),
        generated_at: "2026-06-26T13:00:00Z".to_string(),
    })
    .expect("init");

    let store = MetadataStore::open(&db_path).expect("store");
    let workspace_record = store
        .current_workspace()
        .expect("workspace lookup")
        .expect("workspace");
    let project = store
        .current_project_by_path("app")
        .expect("project lookup")
        .expect("project");
    let existing_node = store
        .projected_node_by_path(&workspace_record.id, "app/src/main.ts")
        .expect("node lookup")
        .expect("projected node");
    store
        .upsert_projected_node(&ProjectedNodeRecord {
            workspace_id: workspace_record.id.clone(),
            node_id: existing_node.node_id,
            project_id: Some(project.id.clone()),
            parent_node_id: existing_node.parent_node_id,
            path: "app/src/main.ts".to_string(),
            kind: NamespaceEntryKind::File,
            content_id: Some(ContentId::new("cid_hot_source")),
            hydration_state: HydrationState::Cold,
            updated_at: "2026-06-26T13:00:01Z".to_string(),
        })
        .expect("make source cold");

    let outcome = prewarm_project(PrewarmOptions {
        db_path: Some(db_path.clone()),
        project_path: "app".to_string(),
        approve_setup: false,
        trigger: "cli-prewarm".to_string(),
        generated_at: "2026-06-26T13:00:02Z".to_string(),
    })
    .expect("prewarm");

    assert_eq!(outcome.state, PrewarmState::NoSetupNeeded);
    assert!(
        outcome
            .redacted_summary
            .contains("Hot project prefetch queued 1 file(s); 1 already local."),
        "{}",
        outcome.redacted_summary
    );
    assert_eq!(project_hot_state(&db_path), "hot");
    let queue = store
        .hydration_queue(&workspace_record.id)
        .expect("hydration queue");
    let hot_prefetch = queue
        .iter()
        .find(|record| record.path == "app/src/main.ts")
        .expect("hot prefetch row");
    assert_eq!(hot_prefetch.priority, "hot-project-prefetch");
    assert_eq!(hot_prefetch.cause, "hot-project-prefetch");
    assert_eq!(hot_prefetch.state, "completed");
}

fn provider_records_from_file(
    project_id: &ProjectId,
    path: &std::path::Path,
) -> Vec<EnvProviderRecord> {
    let bytes = fs::read(path).expect("env bytes");
    let parsed = parse_env_text("app/.env.local", "local", &bytes);
    parsed
        .lines
        .into_iter()
        .filter_map(|line| match line.kind {
            EnvLineKind::KeyValue(value) => Some(EnvProviderRecord {
                project_id: project_id.clone(),
                source_path: parsed.source_path.clone(),
                profile: parsed.profile.clone(),
                key: value.key,
                occurrence_index: value.occurrence_index,
                value: value.value,
                restriction: EnvRecordRestriction::Inherited,
                freshness: EnvRecordFreshness::Fresh,
            }),
            EnvLineKind::Blank | EnvLineKind::Comment | EnvLineKind::Opaque(_) => None,
        })
        .collect()
}

fn project_hot_state(db_path: &std::path::Path) -> String {
    let connection = rusqlite::Connection::open(db_path).expect("db");
    connection
        .query_row(
            "SELECT hot_state FROM projects WHERE path = 'app'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("project hot state")
}

fn setup_receipt_id_for_test(
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    recipe_hash: &str,
    receipt_key: &str,
) -> String {
    let input = format!(
        "{}:{}:{}:{}",
        workspace_id.as_str(),
        project_id.as_str(),
        recipe_hash,
        receipt_key
    );
    format!("setup_{}", blake3::hash(input.as_bytes()).to_hex())
}

fn inferred_receipt_key_for_test(
    project_root: &std::path::Path,
    lockfile: &str,
    command_text: &str,
    package_manager: PackageManagerIdentity,
) -> String {
    let identity = collect_receipt_identity_inputs(
        project_root,
        "default",
        Some(format!("inferred:{lockfile}")),
        Some(package_manager),
    )
    .expect("identity");
    let identity_json = serde_json::to_string(&identity).expect("identity json");
    let identity_hash = blake3::hash(identity_json.as_bytes());
    format!(
        "lockfile:{lockfile}:identity:{}:{command_text}",
        identity_hash.to_hex()
    )
}
