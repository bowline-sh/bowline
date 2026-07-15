use super::permissions::MaterializedFilePermissions;
use super::*;
use crate::{
    metadata::{
        LocalWriteLogRecord, MetadataStore, ProjectUpsert, SyncOperationRecord, SyncResourceKey,
        WorkspaceSyncHeadRecord,
    },
    sync::merge_plugins::{
        MergePluginApprovalRequest, MergePluginAuditRecord, MergePluginIdentity,
        ProjectMergePluginRegistry,
    },
    sync::rebuild_manifest_identity,
    workspace::TempWorkspace,
};
use bowline_control_plane::WorkspaceControlPlaneClient as _;
use bowline_core::{
    git_worktree_link::WORKSPACE_ROOT_MARKER,
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, FileExecutability, HydrationState,
        NamespaceEntry, SnapshotDraft, SnapshotKind,
    },
};
use std::{os::unix::fs::PermissionsExt, process::Command};
mod cached_pack_io;
mod env_records;
mod head_persistence;
mod observation_persistence;
mod plan_characterization;
mod symlink_security;
mod worktree_materialization;
#[test]
fn materialize_snapshot_replaces_symlink_parents_without_following_them() {
    let workspace = TempWorkspace::new("sync-materialize-symlink-parent").expect("workspace");
    let outside = TempWorkspace::new("sync-materialize-outside").expect("outside");
    let app = workspace.root().join("app");
    std::os::unix::fs::symlink(outside.root(), &app).expect("symlink parent");
    let snapshot = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "app/src/main.ts",
        b"export const value = 1;\n",
    );
    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    let outside_written = outside.root().join("src").join("main.ts").metadata();
    assert!(
        outside_written.is_err(),
        "materialization must not write through a local symlink parent"
    );
    assert_eq!(
        fs::read(app.join("src").join("main.ts")).expect("workspace file"),
        b"export const value = 1;\n"
    );
    assert!(
        !fs::symlink_metadata(app)
            .expect("app metadata")
            .file_type()
            .is_symlink(),
        "symlink parent should be replaced with a real directory"
    );
}

#[test]
fn clean_device_materializes_newer_remote_before_adopting_its_head() {
    let linux_workspace = TempWorkspace::new("sync-race-linux-workspace").expect("linux root");
    let mac_workspace = TempWorkspace::new("sync-race-mac-workspace").expect("mac root");
    let linux_state = TempWorkspace::new("sync-race-linux-state").expect("linux state");
    let mac_state = TempWorkspace::new("sync-race-mac-state").expect("mac state");
    let shared_objects = TempWorkspace::new("sync-race-shared-objects").expect("objects");
    let workspace_id = WorkspaceId::new("ws_two_device_race");
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let byte_store = bowline_storage::LocalByteStore::open(shared_objects.root().join("objects"))
        .expect("byte store");
    let relative_path = "project/state.txt";
    let linux_path = linux_workspace.root().join(relative_path);
    let mac_path = mac_workspace.root().join(relative_path);
    fs::create_dir_all(linux_path.parent().expect("linux parent")).expect("linux parent");
    fs::create_dir_all(mac_path.parent().expect("mac parent")).expect("mac parent");
    fs::write(&linux_path, b"old-linux-bytes\n").expect("linux old bytes");
    fs::write(&mac_path, b"old-linux-bytes\n").expect("mac old bytes");

    let linux_initial = device_runner(
        &control_plane,
        &byte_store,
        &linux_workspace,
        &linux_state,
        workspace_id.clone(),
        "device_linux",
        "2026-07-15T10:00:00Z",
    )
    .tick()
    .expect("linux initial upload");
    assert!(matches!(linux_initial, SyncTickOutcome::Uploaded(_)));
    let mac_initial = device_runner(
        &control_plane,
        &byte_store,
        &mac_workspace,
        &mac_state,
        workspace_id.clone(),
        "device_mac",
        "2026-07-15T10:01:00Z",
    )
    .tick()
    .expect("mac initial import");
    assert!(matches!(mac_initial, SyncTickOutcome::Imported(_)));

    fs::write(&mac_path, b"newer-mac-bytes-with-a-distinct-length\n").expect("mac new bytes");
    let mac_advanced = device_runner(
        &control_plane,
        &byte_store,
        &mac_workspace,
        &mac_state,
        workspace_id.clone(),
        "device_mac",
        "2026-07-15T10:02:00Z",
    )
    .tick()
    .expect("mac upload");
    assert!(matches!(mac_advanced, SyncTickOutcome::Uploaded(_)));
    let remote_after_mac = control_plane
        .get_workspace_ref(&workspace_id)
        .expect("remote lookup")
        .expect("remote ref");

    MetadataStore::open(linux_state.root().join(DEFAULT_DATABASE_FILE))
        .expect("linux store")
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write_settled_watcher_noise".to_string(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_linux"),
            project_id: None,
            path: relative_path.to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "watcher-import-noise".to_string(),
            settled_at: "2026-07-15T10:03:31Z".to_string(),
            created_at: "2026-07-15T10:03:30Z".to_string(),
        })
        .expect("settled watcher row");

    let linux_import = device_runner(
        &control_plane,
        &byte_store,
        &linux_workspace,
        &linux_state,
        workspace_id.clone(),
        "device_linux",
        "2026-07-15T10:03:00Z",
    )
    .tick()
    .expect("linux remote materialization");
    assert_eq!(
        linux_import,
        SyncTickOutcome::Imported(remote_after_mac.clone())
    );
    assert_eq!(
        fs::read(&linux_path).expect("linux materialized bytes"),
        b"newer-mac-bytes-with-a-distinct-length\n"
    );
    let linux_head = MetadataStore::open(linux_state.root().join(DEFAULT_DATABASE_FILE))
        .expect("linux store")
        .workspace_sync_head(&workspace_id)
        .expect("linux head lookup")
        .expect("linux head");
    assert_eq!(linux_head.workspace_ref, remote_after_mac);

    let linux_stable = device_runner(
        &control_plane,
        &byte_store,
        &linux_workspace,
        &linux_state,
        workspace_id.clone(),
        "device_linux",
        "2026-07-15T10:04:00Z",
    )
    .tick()
    .expect("linux stable tick");
    assert_eq!(linux_stable, SyncTickOutcome::NoChanges);
    assert_eq!(
        control_plane
            .get_workspace_ref(&workspace_id)
            .expect("remote lookup")
            .expect("remote ref"),
        remote_after_mac,
        "the stale Linux snapshot must not advance over the remote Mac snapshot"
    );
}
#[test]
fn materialize_snapshot_writes_secret_bearing_files_owner_only() {
    let workspace = TempWorkspace::new("sync-materialize-env-permissions").expect("workspace");
    let snapshot = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "app/.env.local",
        b"SECRET=value\n",
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    let mode = fs::metadata(workspace.root().join("app").join(".env.local"))
        .expect("env metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn materialize_snapshot_writes_executable_files_0o755() {
    let workspace =
        TempWorkspace::new("sync-materialize-executable-permissions").expect("workspace");
    let snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/bin/tool",
        b"#!/bin/sh\n",
        FileExecutability::Executable,
        true,
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        mode_for(workspace.root().join("app").join("bin").join("tool")),
        0o755
    );
}

#[test]
fn materialize_snapshot_writes_encrypted_sync_executable_files_0o755() {
    let workspace =
        TempWorkspace::new("sync-materialize-encrypted-executable-permissions").expect("workspace");
    let mut snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/.git/hooks/pre-commit",
        b"#!/bin/sh\n",
        FileExecutability::Executable,
        true,
    );
    snapshot.mutate_entries_for_test(|entries| {
        entries[0].mode = MaterializationMode::EncryptedSync;
    });

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        mode_for(
            workspace
                .root()
                .join("app")
                .join(".git")
                .join("hooks")
                .join("pre-commit")
        ),
        0o755
    );
}

#[test]
fn materialize_snapshot_keeps_regular_encrypted_sync_files_0o600() {
    let workspace =
        TempWorkspace::new("sync-materialize-encrypted-regular-permissions").expect("workspace");
    let mut snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/.git/objects/pack/pack-main.pack",
        b"opaque git bytes\n",
        FileExecutability::Regular,
        true,
    );
    snapshot.mutate_entries_for_test(|entries| {
        entries[0].mode = MaterializationMode::EncryptedSync;
    });

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        mode_for(
            workspace
                .root()
                .join("app")
                .join(".git")
                .join("objects")
                .join("pack")
                .join("pack-main.pack")
        ),
        0o600
    );
}

#[test]
fn materialize_snapshot_overrides_restrictive_umask_for_executable_files() {
    let test_binary = std::env::current_exe().expect("current test binary");
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("umask 077; exec \"$1\" --exact sync::runner::tests::materialize_snapshot_restrictive_umask_child --ignored")
        .arg("sh")
        .arg(test_binary)
        .output()
        .expect("spawn umask child");

    assert!(
        output.status.success(),
        "umask child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run through materialize_snapshot_overrides_restrictive_umask_for_executable_files"]
fn materialize_snapshot_restrictive_umask_child() {
    let workspace = TempWorkspace::new("sync-materialize-restrictive-umask").expect("workspace");
    let snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/bin/tool",
        b"#!/bin/sh\n",
        FileExecutability::Executable,
        true,
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        mode_for(workspace.root().join("app").join("bin").join("tool")),
        0o755
    );
}

#[test]
fn materialize_snapshot_writes_regular_files_0o644() {
    let workspace = TempWorkspace::new("sync-materialize-regular-permissions").expect("workspace");
    let snapshot = snapshot_with_file(WorkspaceId::new("ws_code"), "app/README.md", b"hello\n");

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        mode_for(workspace.root().join("app").join("README.md")),
        0o644
    );
}

#[test]
fn materialize_snapshot_overrides_restrictive_umask_for_regular_files() {
    let test_binary = std::env::current_exe().expect("current test binary");
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("umask 077; exec \"$1\" --exact sync::runner::tests::materialize_snapshot_regular_umask_child --ignored")
        .arg("sh")
        .arg(test_binary)
        .output()
        .expect("spawn regular umask child");

    assert!(
        output.status.success(),
        "regular umask child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run through materialize_snapshot_overrides_restrictive_umask_for_regular_files"]
fn materialize_snapshot_regular_umask_child() {
    let workspace =
        TempWorkspace::new("sync-materialize-regular-restrictive-umask").expect("workspace");
    let snapshot = snapshot_with_file(WorkspaceId::new("ws_code"), "app/README.md", b"hello\n");

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        mode_for(workspace.root().join("app").join("README.md")),
        0o644
    );
}

#[test]
fn materialize_snapshot_forces_secret_bearing_executable_to_0o600() {
    let workspace =
        TempWorkspace::new("sync-materialize-executable-secret-permissions").expect("workspace");
    let snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/.env.local",
        b"SECRET=value\n",
        FileExecutability::Executable,
        true,
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        mode_for(workspace.root().join("app").join(".env.local")),
        0o600
    );
}

#[test]
fn materialize_snapshot_rejects_missing_required_bytes_without_mutation() {
    let workspace = TempWorkspace::new("sync-materialize-rechmod").expect("workspace");
    let path = workspace.root().join("app").join("bin").join("tool");
    fs::create_dir_all(path.parent().expect("path parent")).expect("path parent");
    fs::write(&path, b"existing bytes\n").expect("existing file");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod regular");
    let outside = TempWorkspace::new("sync-materialize-rechmod-outside").expect("outside");
    let outside_target = outside.root().join("target");
    fs::write(&outside_target, b"outside\n").expect("outside target");
    fs::set_permissions(&outside_target, fs::Permissions::from_mode(0o644)).expect("outside chmod");
    let symlink = workspace.root().join("link");
    std::os::unix::fs::symlink(&outside_target, &symlink).expect("symlink");
    let unreadable = workspace.root().join("unreadable");
    fs::write(&unreadable, b"unreadable\n").expect("unreadable file");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("unreadable chmod");
    let unreadable_parent = workspace.root().join("noaccess");
    fs::create_dir(&unreadable_parent).expect("unreadable parent");
    fs::set_permissions(&unreadable_parent, fs::Permissions::from_mode(0o000))
        .expect("unreadable parent chmod");
    let snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/bin/tool",
        b"expected bytes are unavailable\n",
        FileExecutability::Executable,
        false,
    );

    let error = materialize_snapshot(workspace.root(), None, &snapshot)
        .expect_err("required bytes must be present before a path can become ready");
    assert!(matches!(
        error,
        SyncRunnerError::MissingMaterializationContent(path) if path == "app/bin/tool"
    ));
    apply_materialized_permissions(
        workspace.root(),
        std::path::Path::new("missing"),
        MaterializedFilePermissions::for_entry(
            "missing",
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
            FileExecutability::Executable,
        ),
    )
    .expect("missing rechmod no-op");
    apply_materialized_permissions(
        workspace.root(),
        std::path::Path::new("link"),
        MaterializedFilePermissions::for_entry(
            "link",
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
            FileExecutability::Executable,
        ),
    )
    .expect("symlink rechmod no-op");
    apply_materialized_permissions(
        workspace.root(),
        std::path::Path::new("unreadable"),
        MaterializedFilePermissions::for_entry(
            "unreadable",
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
            FileExecutability::Executable,
        ),
    )
    .expect("unreadable rechmod does not abort materialization");
    apply_materialized_permissions(
        workspace.root(),
        std::path::Path::new("noaccess/tool"),
        MaterializedFilePermissions::for_entry(
            "noaccess/tool",
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
            FileExecutability::Executable,
        ),
    )
    .expect("unreadable parent rechmod does not abort materialization");

    assert_eq!(fs::read(&path).expect("file bytes"), b"existing bytes\n");
    assert_eq!(mode_for(&path), 0o644);
    assert_eq!(mode_for(&outside_target), 0o644);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644))
        .expect("restore unreadable permissions");
    fs::set_permissions(&unreadable_parent, fs::Permissions::from_mode(0o755))
        .expect("restore unreadable parent permissions");
}

#[test]
fn materialize_snapshot_missing_bytes_preserves_existing_special_mode_bits() {
    let workspace = TempWorkspace::new("sync-materialize-rechmod-special-bits").expect("workspace");
    let path = workspace.root().join("app").join("bin").join("tool");
    fs::create_dir_all(path.parent().expect("path parent")).expect("path parent");
    fs::write(&path, b"existing bytes\n").expect("existing file");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o4755)).expect("chmod setuid");
    let snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/bin/tool",
        b"expected bytes are unavailable\n",
        FileExecutability::Executable,
        false,
    );

    let error = materialize_snapshot(workspace.root(), None, &snapshot)
        .expect_err("required bytes must be present before metadata changes");
    assert!(matches!(
        error,
        SyncRunnerError::MissingMaterializationContent(path) if path == "app/bin/tool"
    ));

    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o7777,
        0o4755
    );
}

#[test]
fn materialize_snapshot_missing_bytes_does_not_follow_symlink_parents() {
    let workspace =
        TempWorkspace::new("sync-materialize-rechmod-symlink-parent").expect("workspace");
    let outside = TempWorkspace::new("sync-materialize-rechmod-outside").expect("outside");
    let outside_file = outside.root().join("bin").join("tool");
    fs::create_dir_all(outside_file.parent().expect("outside parent")).expect("outside parent");
    fs::write(&outside_file, b"outside bytes\n").expect("outside file");
    fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o644)).expect("outside chmod");
    std::os::unix::fs::symlink(outside.root(), workspace.root().join("app"))
        .expect("symlink parent");
    let snapshot = snapshot_with_file_executability(
        WorkspaceId::new("ws_code"),
        "app/bin/tool",
        b"bytes are unavailable\n",
        FileExecutability::Executable,
        false,
    );

    let error = materialize_snapshot(workspace.root(), None, &snapshot)
        .expect_err("required bytes must be present before path traversal");
    assert!(matches!(
        error,
        SyncRunnerError::MissingMaterializationContent(path) if path == "app/bin/tool"
    ));

    assert_eq!(
        mode_for(&outside_file),
        0o644,
        "permission reconciliation must not chmod through a symlink parent"
    );
    assert!(
        fs::symlink_metadata(workspace.root().join("app"))
            .expect("app metadata")
            .file_type()
            .is_symlink(),
        "cold permission reconciliation must not replace symlink parents"
    );
}

#[test]
fn executable_bit_round_trips_scan_to_materialization() {
    let source = TempWorkspace::new("sync-executable-roundtrip-source").expect("source");
    let path = source.root().join("app").join("bin").join("tool");
    fs::create_dir_all(path.parent().expect("path parent")).expect("path parent");
    fs::write(&path, b"#!/bin/sh\n").expect("tool");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod executable");
    let base_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: WorkspaceId::new("ws_code"),
        version: 1,
        snapshot_id: SnapshotId::new("snap_base"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(DeviceId::new("device-peer")),
    };
    let candidate = crate::sync::coalesce_workspace_scan(
        source.root(),
        WorkspaceId::new("ws_code"),
        &base_ref,
        DeviceId::new("device-local"),
        [17_u8; 32],
        "2026-07-03T12:00:00Z",
    )
    .expect("coalesce");
    let destination =
        TempWorkspace::new("sync-executable-roundtrip-destination").expect("destination");

    materialize_snapshot(destination.root(), None, &candidate.snapshot).expect("materialize");

    assert_eq!(
        mode_for(destination.root().join("app").join("bin").join("tool")),
        0o755
    );
}

#[test]
fn invalid_merge_plugin_policy_degrades_to_builtin_registry() {
    let workspace = TempWorkspace::new("sync-invalid-merge-plugin-policy").expect("workspace");
    let state = TempWorkspace::new("sync-invalid-merge-plugin-state").expect("state");
    fs::write(
        workspace.root().join(".bowlinemerge.toml"),
        "schema = 999\n",
    )
    .expect("config");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "sync_invalid_merge_plugin_policy".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            kind: SyncOperationKind::Reconcile,
            resource_key: crate::metadata::SyncResourceKey::workspace_sync(WorkspaceId::new(
                "ws_code",
            )),
            state: SyncOperationState::Queued,
            idempotency_key: "sync_invalid_merge_plugin_policy".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device_local")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: "2026-06-29T04:01:00Z".to_string(),
            updated_at: "2026-06-29T04:01:00Z".to_string(),
        })
        .expect("operation");
    let sync_claim = store
        .claim_next_sync_operation(
            &WorkspaceId::new("ws_code"),
            "test-runner",
            "2026-06-29T04:01:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim operation")
        .expect("queued operation")
        .claim;
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T04:01:00Z".to_string(),
            sync_claim: Some(sync_claim),
            scan_scope: Default::default(),
        },
    );

    let plugins = runner
        .project_merge_plugins()
        .expect("invalid policy must not block sync");

    assert!(plugins.approval_requests.is_empty());
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let checkpoints = store
        .sync_operation_checkpoints("sync_invalid_merge_plugin_policy")
        .expect("checkpoints");
    assert_eq!(checkpoints[0].step, "merge-plugin-config-invalid");
    assert_eq!(checkpoints[0].state, "limited");
    // The checkpoint payload is an external surface: it carries the fixed
    // reason code, never the raw config-bearing error text (KTD-17).
    assert!(
        checkpoints[0]
            .payload_json
            .contains("merge-plugin-config-invalid")
    );
    assert!(
        !checkpoints[0]
            .payload_json
            .contains("unsupported merge plugin config schema 999")
    );
}

#[test]
fn merge_plugin_approval_event_includes_policy_versions() {
    let workspace = TempWorkspace::new("sync-merge-plugin-approval-event").expect("workspace");
    let state = TempWorkspace::new("sync-merge-plugin-approval-state").expect("state");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&WorkspaceId::new("ws_code"), "Code", "2026-06-29T04:01:01Z")
        .expect("workspace");
    drop(store);
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T04:01:01Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    let plugins = ProjectMergePluginRegistry {
        registry: crate::sync::merge_plugins::MergePluginRegistry::built_in(),
        approval_requests: vec![MergePluginApprovalRequest {
            plugin: MergePluginIdentity {
                id: "notebooks".to_string(),
                version: "1.0.0".to_string(),
                digest: "blake3:abc".to_string(),
                matcher_version: "2+patterns:policy".to_string(),
                validator_version: "1".to_string(),
            },
            patterns: vec!["*.ipynb".to_string()],
            module: ".bowline/plugins/notebooks.wasm".to_string(),
        }],
        config_path: workspace.root().join(".bowlinemerge.toml"),
    };

    runner.append_merge_plugin_approval_events(&plugins);

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let event = store
        .list_events(10)
        .expect("events")
        .into_iter()
        .find(|event| event.name == EventName::PolicyNeedsApproval)
        .expect("approval event");
    assert_eq!(
        event
            .payload
            .get("matcherVersion")
            .and_then(serde_json::Value::as_str),
        Some("2+patterns:policy")
    );
    assert_eq!(
        event
            .payload
            .get("validatorVersion")
            .and_then(serde_json::Value::as_str),
        Some("1")
    );
}

#[test]
fn merge_plugin_applied_event_records_remote_snapshot_id() {
    let workspace = TempWorkspace::new("sync-merge-plugin-applied-event").expect("workspace");
    let state = TempWorkspace::new("sync-merge-plugin-applied-state").expect("state");
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&WorkspaceId::new("ws_code"), "Code", "2026-06-29T04:01:02Z")
        .expect("workspace");
    drop(store);
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T04:01:02Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    let remote_ref = WorkspaceRef {
        workspace_id: WorkspaceId::new("ws_code"),
        version: 4,
        snapshot_id: SnapshotId::new("snap_remote"),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 4 },
        updated_by_device_id: Some(DeviceId::new("device_remote")),
    };

    runner.append_merge_plugin_applied_events(
        &[MergePluginAuditRecord {
            path: "notebook.ipynb".to_string(),
            plugin: MergePluginIdentity {
                id: "notebooks".to_string(),
                version: "1.0.0".to_string(),
                digest: "blake3:abc".to_string(),
                matcher_version: "2+patterns:policy".to_string(),
                validator_version: "1".to_string(),
            },
            output_digest: "blake3:def".to_string(),
        }],
        &remote_ref,
    );

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let event = store
        .list_events(10)
        .expect("events")
        .into_iter()
        .find(|event| event.name == EventName::MergePluginApplied)
        .expect("applied event");
    assert_eq!(
        event
            .payload
            .get("remoteSnapshotId")
            .and_then(serde_json::Value::as_str),
        Some("snap_remote")
    );
}

#[test]
fn materialize_snapshot_replaces_files_with_atomic_temp_rename() {
    let workspace = TempWorkspace::new("sync-materialize-atomic-file").expect("workspace");
    let destination = workspace.root().join("app/src/index.ts");
    fs::create_dir_all(destination.parent().expect("destination parent")).expect("parent");
    fs::write(&destination, b"old bytes stay until rename\n").expect("old file");
    let stale_temp = materialization_temp_path(&destination).expect("temp path");
    fs::write(&stale_temp, b"crashed temp bytes\n").expect("stale temp");
    let snapshot = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "app/src/index.ts",
        b"new materialized bytes\n",
    );

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        fs::read(&destination).expect("destination bytes"),
        b"new materialized bytes\n"
    );
    assert!(
        fs::symlink_metadata(&stale_temp).is_err(),
        "stale materialization temp file should be removed"
    );
}

#[test]
fn materialize_snapshot_replaces_symlinks_with_atomic_temp_rename() {
    let workspace = TempWorkspace::new("sync-materialize-atomic-symlink").expect("workspace");
    let destination = workspace.root().join("app/current");
    fs::create_dir_all(destination.parent().expect("destination parent")).expect("parent");
    std::os::unix::fs::symlink("old-target", &destination).expect("old symlink");
    let stale_temp = materialization_temp_path(&destination).expect("temp path");
    std::os::unix::fs::symlink("crashed-target", &stale_temp).expect("stale temp symlink");
    let snapshot = snapshot_with_symlink(WorkspaceId::new("ws_code"), "app/current", "src");

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        fs::read_link(&destination).expect("destination symlink"),
        PathBuf::from("src")
    );
    assert!(
        fs::symlink_metadata(&stale_temp).is_err(),
        "stale materialization temp symlink should be removed"
    );
}

#[test]
fn file_materialization_blocked_by_nonempty_directory() {
    let workspace = TempWorkspace::new("sync-materialize-file-blocked-dir").expect("workspace");
    let destination = workspace.root().join("a");
    fs::create_dir(&destination).expect("destination dir");
    fs::write(destination.join("keep.txt"), b"local child\n").expect("local child");
    let snapshot = snapshot_with_file(WorkspaceId::new("ws_code"), "a", b"incoming file\n");

    let error =
        materialize_snapshot(workspace.root(), None, &snapshot).expect_err("blocked directory");

    assert!(matches!(
        error,
        SyncRunnerError::MaterializationBlockedByDirectory(_)
    ));
    assert!(
        destination.join("keep.txt").exists(),
        "local child must be preserved"
    );
    assert_no_materialization_temp(workspace.root());
}

#[test]
fn file_materialization_replaces_empty_directory() {
    let workspace = TempWorkspace::new("sync-materialize-file-empty-dir").expect("workspace");
    let destination = workspace.root().join("a");
    fs::create_dir(&destination).expect("destination dir");
    let snapshot = snapshot_with_file(WorkspaceId::new("ws_code"), "a", b"incoming file\n");

    materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

    assert_eq!(
        fs::read(&destination).expect("destination file"),
        b"incoming file\n"
    );
    assert!(destination.is_file());
}

#[test]
fn symlink_materialization_blocked_by_nonempty_directory() {
    let workspace = TempWorkspace::new("sync-materialize-symlink-blocked-dir").expect("workspace");
    let destination = workspace.root().join("a");
    fs::create_dir(&destination).expect("destination dir");
    fs::write(destination.join("keep.txt"), b"local child\n").expect("local child");
    let snapshot = snapshot_with_symlink(WorkspaceId::new("ws_code"), "a", "target");

    let error =
        materialize_snapshot(workspace.root(), None, &snapshot).expect_err("blocked directory");

    assert!(matches!(
        error,
        SyncRunnerError::MaterializationBlockedByDirectory(_)
    ));
    assert!(
        destination.join("keep.txt").exists(),
        "local child must be preserved"
    );
    assert_no_materialization_temp(workspace.root());
}

#[test]
fn manifest_directory_removal_tolerates_nonempty() {
    let workspace = TempWorkspace::new("sync-materialize-removed-nonempty-dir").expect("workspace");
    let destination = workspace.root().join("a");
    fs::create_dir(&destination).expect("destination dir");
    fs::write(destination.join("keep.txt"), b"local child\n").expect("local child");
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_directory(workspace_id.clone(), "a");
    let target = empty_snapshot_content(workspace_id, SnapshotId::new("snap_target"), [7; 32])
        .expect("empty target");

    materialize_snapshot(workspace.root(), Some(&base), &target).expect("materialize");

    assert!(
        destination.join("keep.txt").exists(),
        "removed manifest directory keeps local children"
    );
}

#[test]
fn materialization_removes_base_only_git_index() {
    let workspace = TempWorkspace::new("sync-materialize-keep-git-index").expect("workspace");
    let git_index = workspace.root().join(".git/index");
    fs::create_dir_all(git_index.parent().expect("git parent")).expect("git parent");
    fs::write(&git_index, b"local index").expect("local index");
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(workspace_id.clone(), ".git/index", b"base index");
    let target = empty_snapshot_content(workspace_id, SnapshotId::new("snap_target"), [7; 32])
        .expect("empty target");

    materialize_snapshot(workspace.root(), Some(&base), &target).expect("materialize");

    assert!(!git_index.exists());
}

#[test]
fn materialization_writes_target_git_index() {
    let workspace = TempWorkspace::new("sync-materialize-skip-git-index").expect("workspace");
    let git_index = workspace.root().join(".git/index");
    fs::create_dir_all(git_index.parent().expect("git parent")).expect("git parent");
    fs::write(&git_index, b"local index").expect("local index");
    let target = snapshot_with_file(WorkspaceId::new("ws_code"), ".git/index", b"remote index");

    materialize_snapshot(workspace.root(), None, &target).expect("materialize");

    assert_eq!(fs::read(&git_index).expect("git index"), b"remote index");
}

#[test]
fn materialization_denormalizes_portable_worktree_gitlink() {
    let workspace = TempWorkspace::new("sync-materialize-worktree-gitlink").expect("workspace");
    let portable = format!("gitdir: {WORKSPACE_ROOT_MARKER}/repo/.git/worktrees/feat\n");
    let target = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "repo-wt/.git",
        portable.as_bytes(),
    );

    materialize_snapshot(workspace.root(), None, &target).expect("materialize");

    let expected = format!(
        "gitdir: {}/repo/.git/worktrees/feat\n",
        workspace.root().display()
    );
    assert_eq!(
        fs::read(workspace.root().join("repo-wt").join(".git")).expect("gitlink"),
        expected.as_bytes()
    );
}

#[test]
fn materialization_denormalizes_worktree_admin_gitdir() {
    let workspace = TempWorkspace::new("sync-materialize-worktree-admin").expect("workspace");
    let portable = format!("{WORKSPACE_ROOT_MARKER}/repo-wt/.git\n");
    let target = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "repo/.git/worktrees/feat/gitdir",
        portable.as_bytes(),
    );

    materialize_snapshot(workspace.root(), None, &target).expect("materialize");

    let expected = format!("{}/repo-wt/.git\n", workspace.root().display());
    assert_eq!(
        fs::read(
            workspace
                .root()
                .join("repo")
                .join(".git")
                .join("worktrees")
                .join("feat")
                .join("gitdir")
        )
        .expect("gitdir"),
        expected.as_bytes()
    );
}

#[test]
fn materialization_denormalizes_worktree_admin_commondir() {
    let workspace = TempWorkspace::new("sync-materialize-worktree-commondir").expect("workspace");
    let portable = format!("{WORKSPACE_ROOT_MARKER}/repo/.git\n");
    let target = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "repo/.git/worktrees/feat/commondir",
        portable.as_bytes(),
    );

    materialize_snapshot(workspace.root(), None, &target).expect("materialize");

    let expected = format!("{}/repo/.git\n", workspace.root().display());
    assert_eq!(
        fs::read(
            workspace
                .root()
                .join("repo")
                .join(".git")
                .join("worktrees")
                .join("feat")
                .join("commondir")
        )
        .expect("commondir"),
        expected.as_bytes()
    );
}

#[test]
fn materialization_deletes_removed_worktree_gitlink() {
    let workspace =
        TempWorkspace::new("sync-materialize-delete-worktree-gitlink").expect("workspace");
    let path = workspace.root().join("repo-wt").join(".git");
    fs::create_dir_all(path.parent().expect("gitlink parent")).expect("gitlink parent");
    fs::write(&path, b"gitdir: /old/root/repo/.git/worktrees/feat\n").expect("gitlink");
    let base = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "repo-wt/.git",
        b"gitdir: ${BOWLINE_WORKSPACE_ROOT}/repo/.git/worktrees/feat\n",
    );
    let target = snapshot_with_files(WorkspaceId::new("ws_code"), &[]);

    materialize_snapshot(workspace.root(), Some(&base), &target).expect("materialize");

    assert!(
        fs::symlink_metadata(&path).is_err(),
        "removed synced worktree gitlink should not be preserved as local volatile git state"
    );
}

#[test]
fn materialization_leaves_unportable_worktree_gitlink_verbatim() {
    let workspace = TempWorkspace::new("sync-materialize-worktree-unportable").expect("workspace");
    let target = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "repo-wt/.git",
        b"gitdir: /opt/other/repo/.git/worktrees/feat\n",
    );

    materialize_snapshot(workspace.root(), None, &target).expect("materialize");

    assert_eq!(
        fs::read(workspace.root().join("repo-wt").join(".git")).expect("gitlink"),
        b"gitdir: /opt/other/repo/.git/worktrees/feat\n"
    );
}

#[test]
fn materialization_leaves_normal_files_with_marker_verbatim() {
    let workspace = TempWorkspace::new("sync-materialize-normal-marker").expect("workspace");
    let bytes = format!("root={WORKSPACE_ROOT_MARKER}\n");
    let target = snapshot_with_file(WorkspaceId::new("ws_code"), "src/main.rs", bytes.as_bytes());

    materialize_snapshot(workspace.root(), None, &target).expect("materialize");

    assert_eq!(
        fs::read(workspace.root().join("src").join("main.rs")).expect("source"),
        bytes.as_bytes()
    );
}

#[test]
fn linked_git_worktree_round_trips_to_second_root() {
    let source = TempWorkspace::new("sync-linked-worktree-source").expect("source");
    let destination = TempWorkspace::new("sync-linked-worktree-destination").expect("destination");
    let repo = source.root().join("repo");
    let worktree = source.root().join("repo-wt");
    fs::create_dir_all(&repo).expect("repo dir");
    assert_command_success(
        Command::new("git").arg("init").arg(&repo).output(),
        "git init",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.email=bowline@example.test"])
            .args(["-c", "user.name=Bowline Test"])
            .args(["commit", "--allow-empty", "-m", "initial"])
            .output(),
        "git commit",
    );
    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg("feat")
            .arg(&worktree)
            .output(),
        "git worktree add",
    );

    let workspace_id = WorkspaceId::new("ws_code");
    let candidate = super::super::coalescer::coalesce_workspace_scan(
        source.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        [41_u8; 32],
        "2026-07-04T10:30:00Z",
    )
    .expect("coalesce linked worktree");

    materialize_snapshot(destination.root(), None, &candidate.snapshot).expect("materialize");

    assert_command_success(
        Command::new("git")
            .arg("-C")
            .arg(destination.root().join("repo-wt"))
            .arg("status")
            .arg("--short")
            .output(),
        "git status in materialized worktree",
    );
}

#[test]
fn imported_hydration_includes_git_index_but_skips_derivable_git_entries() {
    let git_index = snapshot_with_file(WorkspaceId::new("ws_code"), ".git/index", b"remote index");
    let git_head = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        ".git/HEAD",
        b"ref: refs/heads/main\n",
    );
    let gitlink = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "repo-wt/.git",
        b"gitdir: ${BOWLINE_WORKSPACE_ROOT}/repo/.git/worktrees/feat\n",
    );
    let mut generated = snapshot_with_file(
        WorkspaceId::new("ws_code"),
        "app/target/output.bin",
        b"machine-local output",
    );
    generated.mutate_entries_for_test(|entries| {
        entries[0].mode = MaterializationMode::LocalRegenerate;
    });

    let git_index_entries = git_index.entries_for_test();
    let git_head_entries = git_head.entries_for_test();
    let gitlink_entries = gitlink.entries_for_test();
    let generated_entries = generated.entries_for_test();

    assert!(should_hydrate_imported_entry(
        &git_index_entries[0],
        &ImportedHydrationSelection::AllFiles
    ));
    assert!(should_hydrate_imported_entry(
        &git_head_entries[0],
        &ImportedHydrationSelection::AllFiles
    ));
    assert!(should_hydrate_imported_entry(
        &gitlink_entries[0],
        &ImportedHydrationSelection::AllFiles
    ));
    assert!(!should_hydrate_imported_entry(
        &generated_entries[0],
        &ImportedHydrationSelection::RequiredFiles
    ));
    assert!(should_hydrate_imported_entry(
        &git_head_entries[0],
        &ImportedHydrationSelection::RequiredFiles
    ));
    let selected = ImportedHydrationSelection::Paths(BTreeSet::from([".git/index".to_string()]));
    assert!(should_hydrate_imported_entry(
        &git_index_entries[0],
        &selected
    ));
    assert!(!should_hydrate_imported_entry(
        &git_head_entries[0],
        &selected
    ));
}

#[test]
fn materialization_skips_target_derivable_git_directories() {
    let workspace = TempWorkspace::new("sync-materialize-skip-git-logs").expect("workspace");
    let target = snapshot_with_directory(WorkspaceId::new("ws_code"), ".git/logs");

    materialize_snapshot(workspace.root(), None, &target).expect("materialize");

    assert!(
        !workspace.root().join(".git/logs").exists(),
        "remote derivable git directories must not be recreated"
    );
}

#[test]
fn cancelled_claim_before_local_head_preparation_performs_no_hosted_reads() {
    assert_rejected_claim_before_preparation_performs_no_hosted_reads(true);
}

#[test]
fn lost_claim_before_local_head_preparation_performs_no_hosted_reads() {
    assert_rejected_claim_before_preparation_performs_no_hosted_reads(false);
}

#[test]
fn cancellation_after_first_metadata_load_stops_import_before_local_head_commit() {
    assert_mid_import_authority_loss(true);
}

#[test]
fn ownership_loss_after_first_pack_head_stops_import_before_local_head_commit() {
    assert_mid_import_authority_loss(false);
}

fn assert_mid_import_authority_loss(cancel_after_metadata: bool) {
    use crate::sync::download::{
        DownloadError, ImportCheckpointPoint, import_snapshot_by_id_with_checkpoints,
    };

    let workspace = TempWorkspace::new("sync-mid-import-authority-workspace").expect("workspace");
    let state = TempWorkspace::new("sync-mid-import-authority-state").expect("state");
    fs::write(workspace.root().join("README.md"), b"hosted import\n").expect("workspace file");
    let workspace_id = WorkspaceId::new("ws_mid_import_authority");
    let device_id = DeviceId::new("device_local");
    let generated_at = "2026-07-15T17:00:00Z";
    let storage_key = StorageKey::from_bytes([8_u8; 32]);
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace(workspace_id.as_str());
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let candidate = super::super::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        device_id.clone(),
        [7_u8; 32],
        generated_at,
    )
    .expect("candidate");
    let uploaded = crate::sync::upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("upload hosted snapshot fixture");
    let workspace_ref = match uploaded {
        UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
        UploadOutcome::Stale { .. } => panic!("fixture upload should advance the workspace ref"),
    };

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "sync_mid_import_authority".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "sync-mid-import-authority".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: Some(workspace_ref.snapshot_id.as_str().to_string()),
            device_id: Some(device_id.clone()),
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
            created_at: generated_at.to_string(),
            updated_at: generated_at.to_string(),
        })
        .expect("operation");
    let claim = store
        .claim_next_sync_operation(
            &workspace_id,
            "test-runner",
            "2026-07-15T17:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("queued operation")
        .claim;
    drop(store);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id,
            workspace_content_key: [7_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: generated_at.to_string(),
            sync_claim: Some(claim.clone()),
            scan_scope: Default::default(),
        },
    );
    let metadata_loads = Cell::new(0_u64);
    let pack_heads = Cell::new(0_u64);
    let invalidated = Cell::new(false);
    let exact_error = RefCell::new(None);
    let checkpoint = |point| {
        if let Err(error) = runner.check_claim_before_domain_boundary() {
            *exact_error.borrow_mut() = Some(error);
            return Err(DownloadError::CancellationRequested);
        }
        match point {
            ImportCheckpointPoint::MetadataRecordLoaded => {
                metadata_loads.set(metadata_loads.get().saturating_add(1));
                if cancel_after_metadata && !invalidated.replace(true) {
                    MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE))
                        .expect("open cancellation store")
                        .request_sync_operation_cancellation(
                            claim.operation_id(),
                            "2026-07-15T17:00:02Z",
                        )
                        .expect("request mid-import cancellation");
                }
            }
            ImportCheckpointPoint::PackHeadCompleted => {
                pack_heads.set(pack_heads.get().saturating_add(1));
                if !cancel_after_metadata && !invalidated.replace(true) {
                    MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE))
                        .expect("open ownership store")
                        .requeue_claimed_sync_operation_after_dispatch_failure(
                            &claim,
                            "test_claim_lost",
                            "replacement worker owns the operation",
                            "2026-07-15T17:00:02Z",
                        )
                        .expect("invalidate mid-import claim");
                }
            }
            ImportCheckpointPoint::Boundary => {}
        }
        Ok(())
    };

    let error = import_snapshot_by_id_with_checkpoints(
        &workspace_id,
        &SnapshotId::new(workspace_ref.snapshot_id.clone()),
        &control_plane,
        &byte_store,
        storage_key,
        crate::sync::namespace::MetadataIdentityKey::derive(&workspace_id, [7_u8; 32]),
        &checkpoint,
    )
    .expect_err("mid-import authority loss must stop import");
    assert!(matches!(error, DownloadError::CancellationRequested));
    let exact_error = exact_error
        .into_inner()
        .expect("runner claim error should be retained");
    if cancel_after_metadata {
        assert!(matches!(
            exact_error,
            SyncRunnerError::SyncOperationCancellationRequested
        ));
        assert_eq!(metadata_loads.get(), 1);
    } else {
        assert!(matches!(
            exact_error,
            SyncRunnerError::SyncClaimOwnershipLost
        ));
        assert_eq!(pack_heads.get(), 1);
    }
    assert!(
        MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE))
            .expect("store")
            .workspace_sync_head(&workspace_id)
            .expect("local head")
            .is_none()
    );
}

fn assert_rejected_claim_before_preparation_performs_no_hosted_reads(cancel: bool) {
    let workspace = TempWorkspace::new("sync-local-head-authority-workspace").expect("workspace");
    let state = TempWorkspace::new("sync-local-head-authority-state").expect("state");
    fs::write(workspace.root().join("README.md"), b"claim fenced\n").expect("workspace file");
    let workspace_id = WorkspaceId::new("ws_local_head_authority");
    let generated_at = "2026-07-15T16:00:00Z";
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .enqueue_sync_operation(&SyncOperationRecord {
            id: "sync_local_head_authority".to_string(),
            workspace_id: workspace_id.clone(),
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
            state: SyncOperationState::Queued,
            idempotency_key: "sync-local-head-authority".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device_local")),
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
            created_at: generated_at.to_string(),
            updated_at: generated_at.to_string(),
        })
        .expect("operation");
    let claim = store
        .claim_next_sync_operation(
            &workspace_id,
            "test-runner",
            "2026-07-15T16:00:01Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("claim")
        .expect("queued operation")
        .claim;
    if cancel {
        store
            .request_sync_operation_cancellation(claim.operation_id(), "2026-07-15T16:00:02Z")
            .expect("request cancellation");
    } else {
        store
            .requeue_claimed_sync_operation_after_dispatch_failure(
                &claim,
                "test_claim_lost",
                "replacement worker owns the operation",
                "2026-07-15T16:00:02Z",
            )
            .expect("release original claim");
    }
    drop(store);

    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    control_plane.set_offline(true);
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let base_ref = empty_workspace_ref(workspace_id.clone());
    let candidate = super::super::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device_local"),
        [7_u8; 32],
        generated_at,
    )
    .expect("candidate");
    let accepted = accepted_ref(
        &workspace_id,
        candidate.snapshot.manifest.snapshot_id.as_str(),
    );
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id,
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: generated_at.to_string(),
            sync_claim: Some(claim),
            scan_scope: Default::default(),
        },
    );

    let error = runner
        .complete_local_head(
            &accepted,
            LocalHeadMetadataUpdate::FreshScan {
                bound_snapshot: None,
            },
        )
        .expect_err("stale claim must fence local-head preparation");
    if cancel {
        assert!(matches!(
            error,
            SyncRunnerError::SyncOperationCancellationRequested
        ));
    } else {
        assert!(matches!(error, SyncRunnerError::SyncClaimOwnershipLost));
    }
    let metrics = byte_store.metrics();
    assert_eq!(metrics.full_read_count, 0);
    assert_eq!(metrics.range_read_count, 0);
    assert_eq!(metrics.head_count, 0);
}

#[test]
fn remote_ref_history_skips_unavailable_old_snapshots_without_blocking_sync() {
    let workspace = TempWorkspace::new("sync-history-skip-missing").expect("workspace");
    let state = TempWorkspace::new("sync-history-skip-missing-state").expect("state");
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    control_plane
        .create_workspace_ref(&workspace_id)
        .expect("workspace ref");
    let old_snapshot = SnapshotId::new("snap_missing_old");
    let device_id = DeviceId::new("device-a");
    control_plane
        .compare_and_swap_workspace_ref(&workspace_id, 0, &old_snapshot, &device_id)
        .expect("history row");
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T04:01:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );

    runner
        .import_remote_ref_history("snap_current")
        .expect("missing old history snapshot should not block sync");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    assert!(
        store
            .sync_operations(&workspace_id)
            .expect("sync operations")
            .is_empty()
    );
}

fn snapshot_with_file(workspace_id: WorkspaceId, path: &str, bytes: &[u8]) -> SnapshotContent {
    snapshot_with_file_executability(workspace_id, path, bytes, FileExecutability::Regular, true)
}

fn snapshot_with_file_executability(
    workspace_id: WorkspaceId,
    path: &str,
    bytes: &[u8],
    executability: FileExecutability,
    include_bytes: bool,
) -> SnapshotContent {
    let content_id = bowline_core::workspace_graph::workspace_content_id([3_u8; 32], bytes);
    let content = if include_bytes {
        [(content_id.clone(), bytes.to_vec())].into_iter().collect()
    } else {
        BTreeMap::new()
    };
    snapshot_from_entries(
        workspace_id,
        vec![NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            content_id: Some(content_id.clone()),
            content_layout: None,
            symlink_target: None,
            byte_len: Some(bytes.len() as u64),
            executability,
            hydration_state: HydrationState::Local,
        }],
        content,
    )
}

pub(super) fn snapshot_with_files(
    workspace_id: WorkspaceId,
    files: &[(&str, &[u8])],
) -> SnapshotContent {
    let mut content = BTreeMap::new();
    let entries = files
        .iter()
        .map(|(path, bytes)| {
            let content_id = bowline_core::workspace_graph::workspace_content_id([3_u8; 32], bytes);
            content.insert(content_id.clone(), (*bytes).to_vec());
            NamespaceEntry {
                path: (*path).to_string(),
                kind: NamespaceEntryKind::File,
                classification: PathClassification::WorkspaceSync,
                mode: MaterializationMode::WorkspaceSync,
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                content_id: Some(content_id.clone()),
                content_layout: None,
                symlink_target: None,
                byte_len: Some(bytes.len() as u64),
                executability: bowline_core::workspace_graph::FileExecutability::Regular,
                hydration_state: HydrationState::Local,
            }
        })
        .collect::<Vec<_>>();
    snapshot_from_entries(workspace_id, entries, content)
}

fn snapshot_with_directory(workspace_id: WorkspaceId, path: &str) -> SnapshotContent {
    snapshot_from_entries(
        workspace_id,
        vec![NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::Directory,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: None,
            executability: bowline_core::workspace_graph::FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }],
        Default::default(),
    )
}

fn snapshot_with_symlink(workspace_id: WorkspaceId, path: &str, target: &str) -> SnapshotContent {
    snapshot_from_entries(
        workspace_id,
        vec![NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::Symlink,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            content_id: None,
            content_layout: None,
            symlink_target: Some(target.to_string()),
            byte_len: None,
            executability: bowline_core::workspace_graph::FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }],
        Default::default(),
    )
}

fn snapshot_from_entries(
    workspace_id: WorkspaceId,
    entries: Vec<NamespaceEntry>,
    content: BTreeMap<ContentId, Vec<u8>>,
) -> SnapshotContent {
    let snapshot_id = rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id,
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: Vec::new(),
        },
        content,
        [7; 32],
    )
    .expect("page-backed test snapshot")
}

fn assert_no_materialization_temp(root: &Path) {
    for entry in fs::read_dir(root).expect("root entries") {
        let entry = entry.expect("root entry");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(
            !(name.starts_with(".bowline-materialize-") && name.ends_with(".tmp")),
            "materialization temp file should be removed: {}",
            entry.path().display()
        );
    }
}

fn mode_for(path: impl AsRef<Path>) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn assert_command_success(output: std::io::Result<std::process::Output>, action: &str) {
    let output = output.unwrap_or_else(|error| panic!("{action} failed to spawn: {error}"));
    assert!(
        output.status.success(),
        "{action} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn test_runner(
    workspace: &TempWorkspace,
    state: &TempWorkspace,
    workspace_id: WorkspaceId,
) -> SyncRunner<'static> {
    let control_plane = Box::leak(Box::new(
        bowline_control_plane::FakeControlPlaneClient::default(),
    ));
    control_plane.create_workspace(workspace_id.as_str());
    let byte_store = Box::leak(Box::new(
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store"),
    ));
    SyncRunner::new(
        control_plane,
        byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id,
            device_id: DeviceId::new("device_local"),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-07-03T10:01:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    )
}

fn device_runner<'a>(
    control_plane: &'a bowline_control_plane::FakeControlPlaneClient,
    byte_store: &'a bowline_storage::LocalByteStore,
    workspace: &TempWorkspace,
    state: &TempWorkspace,
    workspace_id: WorkspaceId,
    device_id: &str,
    generated_at: &str,
) -> SyncRunner<'a> {
    SyncRunner::new(
        control_plane,
        byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id,
            device_id: DeviceId::new(device_id),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: generated_at.to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    )
}

fn accepted_ref(workspace_id: &WorkspaceId, snapshot_id: &str) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: SnapshotId::new(snapshot_id),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(DeviceId::new("device_local")),
    }
}

fn add_bound_locator(snapshot: &mut SnapshotContent, path: &str, pack_id: &str) {
    snapshot.mutate_entries_for_test(|entries| {
        let entry = entries
            .iter_mut()
            .find(|entry| entry.path == path)
            .expect("entry");
        let content_id = entry.content_id.clone().expect("content id");
        entry.content_layout = Some(
            ContentLayout::single_segment(ContentLocator {
                content_id,
                storage: ContentStorage::Packed,
                raw_size: entry.byte_len.unwrap_or(0),
                pack_id: Some(bowline_core::ids::PackId::new(pack_id)),
                offset: Some(0),
                length: entry.byte_len,
            })
            .expect("test layout"),
        );
        entry.hydration_state = HydrationState::Cold;
    });
}
