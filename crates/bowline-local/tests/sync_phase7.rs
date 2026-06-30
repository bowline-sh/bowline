use std::{
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

use bowline_control_plane::{
    CompactEvent, CompareAndSwapError, ConflictMetadataPublish, ConflictMetadataRecord,
    ConflictResolutionMark, ControlPlaneClient, ControlPlaneResult, DeleteIntent,
    DeleteIntentRequest, DeviceRequest, DeviceRequestInput, DownloadIntent, DownloadIntentRequest,
    FakeControlPlaneClient, ObjectManifestCommit, ObjectManifestRecord, ObjectRetentionStateUpdate,
    UploadIntent, UploadIntentRequest, UploadVerificationIntentRequest, WorkspaceRef,
};
use bowline_core::{
    events::EventName,
    ids::{DeviceId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentStorage, HydrationState, NamespaceEntry, NamespaceEntryKind, SnapshotManifest,
        workspace_content_id,
    },
};
use bowline_local::metadata::{HydrationQueueRecord, SyncOperationRecord, WorkspaceSyncHeadRecord};
use bowline_local::{
    metadata::MetadataStore,
    status::{StatusOptions, compose_status},
    sync::{
        ConflictBundleError, ConflictFile, ConflictRecord, ConflictSpan, DownloadError,
        MergeOutcome, SyncRunner, SyncRunnerOptions, SyncTickOutcome, coalesce_workspace_scan,
        create_conflict_bundle, import_snapshot_by_id, merge_snapshots, upload_snapshot_candidate,
    },
    workspace::TempWorkspace,
};
use bowline_storage::{ByteStore, LocalByteStore, StorageKey};
use bowline_storage::{LocalContentCache, ObjectKey, RangeHydrationRequest};

#[test]
fn dirty_untracked_workspace_uploads_packed_snapshot_and_imports_structure_first() {
    let workspace = TempWorkspace::new("phase7-source").expect("source root");
    workspace.create_project("app").expect("project");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    workspace
        .write_project_file("app", ".env.local", b"SECRET=value\n")
        .expect("env");
    workspace
        .create_generated_folder("app", "node_modules")
        .expect("generated");

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let state = TempWorkspace::new("phase7-state").expect("state root");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 10).expect("byte store");
    let workspace_id = WorkspaceId::new("ws_code");
    let storage_key = StorageKey::deterministic(7);
    let content_key = [7_u8; 32];

    let candidate = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-linux"),
        content_key,
        "2026-06-24T12:00:00Z",
    )
    .expect("candidate");

    assert!(
        candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/src/main.ts")
    );
    assert!(
        candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/.env.local")
    );
    assert!(
        !candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path.contains("node_modules"))
    );

    let outcome =
        upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
            .expect("upload");
    let object_manifest = match outcome {
        bowline_local::sync::UploadOutcome::Advanced {
            workspace_ref,
            object_manifest,
        } => {
            assert_eq!(
                workspace_ref.snapshot_id,
                candidate.snapshot.manifest.snapshot_id.as_str()
            );
            object_manifest
        }
        bowline_local::sync::UploadOutcome::Stale { .. } => {
            panic!("first writer should advance")
        }
    };

    assert_eq!(object_manifest.pack_objects.len(), 1);
    assert!(
        object_manifest
            .manifest_object
            .object_key
            .starts_with("manifests_mf_")
    );
    assert!(
        object_manifest.pack_objects[0]
            .object_key
            .starts_with("packs_pk_")
    );
    assert!(!object_manifest.pack_objects[0].object_key.contains("main"));

    let imported = import_snapshot_by_id(
        &workspace_id,
        &candidate.snapshot.manifest.snapshot_id,
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import");

    assert_eq!(
        imported.manifest.snapshot_id,
        candidate.snapshot.manifest.snapshot_id
    );
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == NamespaceEntryKind::File)
            .all(|entry| entry.locator.is_some())
    );
    assert_eq!(byte_store.metrics().full_read_count, 1);
    assert_eq!(byte_store.metrics().range_read_count, 0);

    let retry = upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
        .expect("committed object retry");
    match retry {
        bowline_local::sync::UploadOutcome::Advanced { .. } => {
            panic!("retrying the same base should observe the advanced workspace ref")
        }
        bowline_local::sync::UploadOutcome::Stale {
            stale,
            object_manifest: retried_manifest,
        } => {
            assert_eq!(
                stale.current.snapshot_id,
                candidate.snapshot.manifest.snapshot_id.as_str()
            );
            assert_eq!(retried_manifest, object_manifest);
        }
    }
}

#[test]
fn coalesced_snapshot_id_is_stable_for_unchanged_workspace_content() {
    let workspace = TempWorkspace::new("phase7-stable-snapshot-id").expect("workspace");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    let workspace_id = WorkspaceId::new("ws_code");
    let first_base = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_base_a".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let first = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &first_base,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:00:00Z",
    )
    .expect("first candidate");
    let second_base = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: first.snapshot.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    };
    let second = coalesce_workspace_scan(
        workspace.root(),
        workspace_id,
        &second_base,
        DeviceId::new("device-b"),
        [3_u8; 32],
        "2026-06-24T12:01:00Z",
    )
    .expect("second candidate");

    assert_eq!(
        second.snapshot.manifest.snapshot_id, first.snapshot.manifest.snapshot_id,
        "unchanged workspace content must not create a new snapshot ID just because the base ref or device changed"
    );
}

#[test]
fn retry_after_manifest_commit_reuses_side_effects_and_advances_ref() {
    let workspace = TempWorkspace::new("phase7-commit-before-cas-workspace").expect("workspace");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");

    let workspace_id = WorkspaceId::new("ws_code");
    let inner = FakeControlPlaneClient::default();
    let base_ref = inner.create_workspace("ws_code");
    let control_plane = CasFailsOnceControlPlane::new(inner);
    let state = TempWorkspace::new("phase7-commit-before-cas-state").expect("state");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 30).expect("byte store");
    let storage_key = StorageKey::deterministic(30);
    let candidate = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-a"),
        [30_u8; 32],
        "2026-06-24T12:09:00Z",
    )
    .expect("candidate");

    let first_error =
        upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
            .expect_err("first CAS fails after object manifest commit");
    assert!(
        first_error.to_string().contains("injected CAS failure"),
        "unexpected first error: {first_error}"
    );
    assert_eq!(
        control_plane
            .get_workspace_ref("ws_code")
            .expect("workspace ref")
            .expect("workspace exists")
            .version,
        base_ref.version,
        "the failed attempt must not claim the workspace advanced"
    );
    let committed_manifest = control_plane
        .get_snapshot_manifest_pointer("ws_code", candidate.snapshot.manifest.snapshot_id.as_str())
        .expect("manifest lookup")
        .expect("manifest committed before CAS failure");
    let first_put_count = byte_store.metrics().put_count;
    assert!(first_put_count > 0, "first attempt should upload objects");

    let retry = upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
        .expect("retry succeeds");
    let bowline_local::sync::UploadOutcome::Advanced {
        workspace_ref,
        object_manifest,
    } = retry
    else {
        panic!("retry should advance the original local edit after transient CAS failure");
    };
    assert_eq!(
        workspace_ref.snapshot_id,
        candidate.snapshot.manifest.snapshot_id.as_str()
    );
    assert_eq!(workspace_ref.version, base_ref.version + 1);
    assert_eq!(object_manifest, committed_manifest);
    assert_eq!(
        byte_store.metrics().put_count,
        first_put_count,
        "retry should reuse committed pack and manifest objects without re-uploading"
    );
}

#[test]
fn resolved_conflict_metadata_is_not_cleared_before_failed_upload_is_durable() {
    let workspace = TempWorkspace::new("phase7-resolved-clear-after-upload").expect("workspace");
    let state = TempWorkspace::new("phase7-resolved-clear-after-upload-state").expect("state");
    workspace
        .write_project_file("app", "config.toml", b"value = \"resolved\"\n")
        .expect("resolved file");

    let inner = FakeControlPlaneClient::default();
    let base_ref = inner.create_workspace("ws_code");
    let control_plane = CasFailsOnceControlPlane::new(inner);
    let bundle = create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path("app/config.toml"),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"local\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("bundle");
    control_plane
        .publish_conflict_metadata(bowline_control_plane::ConflictMetadataPublish {
            workspace_id: "ws_code".to_string(),
            conflict_id: bundle.record.id.clone(),
            conflict_kind: "text".to_string(),
            paths: bundle.record.paths.clone(),
            contains_secrets: false,
            base_snapshot_id: "empty".to_string(),
            remote_snapshot_id: base_ref.snapshot_id.clone(),
            detected_by_device_id: "device-a".to_string(),
            bundle_object: None,
        })
        .expect("publish conflict metadata");
    mark_only_conflict_bundle_state(state.root(), "accepted");

    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 31).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [31_u8; 32],
            storage_key: StorageKey::deterministic(31),
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:30Z".to_string(),
            sync_operation_id: None,
        },
    );

    let error = runner
        .tick()
        .expect_err("upload fails before resolution can be durable");
    assert!(error.to_string().contains("injected CAS failure"));
    assert_eq!(
        control_plane
            .list_workspace_conflicts("ws_code", "device-a")
            .expect("conflict metadata remains unresolved")
            .len(),
        1
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.root.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert!(
        manifest.get("remoteResolutionSyncedAt").is_none(),
        "failed durable sync must not locally acknowledge remote resolution"
    );
}

#[test]
fn resolved_conflict_without_prior_publish_publishes_then_clears_after_durable_upload() {
    let workspace = TempWorkspace::new("phase7-resolved-publish-before-clear").expect("workspace");
    let state = TempWorkspace::new("phase7-resolved-publish-before-clear-state").expect("state");
    workspace
        .write_project_file("app", "config.toml", b"value = \"resolved\"\n")
        .expect("resolved file");

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let mut record = ConflictRecord::same_path("app/config.toml");
    record.base_snapshot_id = Some("empty".to_string());
    record.remote_snapshot_id = Some(base_ref.snapshot_id.clone());
    let bundle = create_conflict_bundle(
        state.root(),
        record,
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"local\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("bundle");
    mark_only_conflict_bundle_state(state.root(), "accepted");

    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 33).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [33_u8; 32],
            storage_key: StorageKey::deterministic(33),
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:45Z".to_string(),
            sync_operation_id: None,
        },
    );

    assert!(matches!(
        runner
            .tick()
            .expect("resolution upload publishes then clears metadata"),
        SyncTickOutcome::Uploaded(_)
    ));
    assert_eq!(
        control_plane
            .list_workspace_conflicts("ws_code", "device-a")
            .expect("conflict metadata cleared")
            .len(),
        0
    );
    let events = control_plane.list_events("ws_code").expect("events");
    assert!(
        events
            .iter()
            .any(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictDetected)
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictResolved)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.root.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert!(manifest.get("remoteConflictPublishedAt").is_some());
    assert!(manifest.get("remoteResolutionSyncedAt").is_some());
}

#[test]
fn runner_noops_when_workspace_content_already_matches_head() {
    let workspace = TempWorkspace::new("phase7-runner-noop-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-runner-noop-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 14).expect("byte store");
    let storage_key = StorageKey::deterministic(14);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [14_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:07:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    let first = runner.tick().expect("first tick");
    assert!(
        matches!(first, SyncTickOutcome::Uploaded(_)),
        "first dirty workspace tick should upload, got {first:?}"
    );
    let first_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");

    let second = runner.tick().expect("second tick");
    assert_eq!(second, SyncTickOutcome::NoChanges);
    let second_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");
    assert_eq!(second_ref.version, first_ref.version);
    assert_eq!(second_ref.snapshot_id, first_ref.snapshot_id);
}

#[test]
fn runner_records_upload_side_effect_checkpoints_for_claimed_operation() {
    let workspace = TempWorkspace::new("phase7-runner-checkpoint-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-runner-checkpoint-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");

    let workspace_id = WorkspaceId::new("ws_code");
    let operation_id = "op-daemon-checkpoint".to_string();
    let metadata = MetadataStore::open(state.root().join("local.sqlite3")).expect("metadata");
    metadata
        .enqueue_sync_operation(&SyncOperationRecord {
            id: operation_id.clone(),
            workspace_id: workspace_id.clone(),
            kind: "daemon-reconcile".to_string(),
            state: "claimed".to_string(),
            idempotency_key: "daemon-reconcile:checkpoint".to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new("device-a")),
            payload_json: "{}".to_string(),
            attempt_count: 1,
            claimed_by: Some("device-a".to_string()),
            heartbeat_at: Some("2026-06-24T12:08:00Z".to_string()),
            next_attempt_at: None,
            last_error: None,
            created_at: "2026-06-24T12:08:00Z".to_string(),
            updated_at: "2026-06-24T12:08:00Z".to_string(),
        })
        .expect("operation");
    drop(metadata);

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 29).expect("byte store");
    let storage_key = StorageKey::deterministic(29);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [29_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:08:00Z".to_string(),
            sync_operation_id: Some(operation_id.clone()),
        },
    );

    assert!(matches!(
        runner.tick().expect("upload tick"),
        SyncTickOutcome::Uploaded(_)
    ));

    let checkpoints = MetadataStore::open(state.root().join("local.sqlite3"))
        .expect("metadata")
        .sync_operation_checkpoints(&operation_id)
        .expect("checkpoints");
    let steps = checkpoints
        .iter()
        .map(|checkpoint| checkpoint.step.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "remote-ref-observed",
        "snapshot-candidate-built",
        "source-packs-written",
        "source-pack-uploaded",
        "snapshot-manifest-uploaded",
        "object-manifest-committed",
        "workspace-ref-advanced",
    ] {
        assert!(
            steps.contains(&expected),
            "missing checkpoint {expected}; got {steps:?}"
        );
    }
    assert!(
        checkpoints
            .iter()
            .all(|checkpoint| checkpoint.workspace_id == workspace_id)
    );
}

#[test]
fn stale_same_line_edit_creates_conflict_instead_of_losing_work() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base_bytes = b"name = \"old\"\n";
    let local_bytes = b"name = \"local\"\n";
    let remote_bytes = b"name = \"remote\"\n";
    let base = snapshot_with_file(&workspace_id, "snap_base", "app/config.toml", base_bytes);
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let mut local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(&workspace_id, "snap_local", "app/config.toml", local_bytes),
    );
    local_candidate.base.version = 1;
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_remote",
        "app/config.toml",
        remote_bytes,
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: "snap_remote".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:03:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Clean(_) => panic!("same-line divergent edits must not auto-merge"),
        MergeOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].paths, vec!["app/config.toml"]);
            assert_eq!(
                conflicts[0].conflict_kind,
                bowline_local::sync::ConflictKind::Text
            );
            assert_eq!(conflicts[0].spans.len(), 1);
            assert_eq!(conflicts[0].spans[0].path, "app/config.toml");
            assert_eq!(conflicts[0].spans[0].local_start_line, 1);
            assert_eq!(conflicts[0].spans[0].remote_start_line, 1);
            assert_eq!(conflicts[0].state, "unresolved");
        }
    }
}

#[test]
fn structured_json_merge_that_fails_validation_conflicts_without_markers() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_json_base",
        "app/config.json",
        b"{\n  \"a\": 1,\n  \"b\": 1\n}\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_json_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_json_local",
            "app/config.json",
            b"{\n  \"a\": 2,\n  \"b\": 1\n}\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_json_remote",
        "app/config.json",
        b"{\n  \"a\": 1,\n  \"b\": }\n}\n",
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: remote.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-26T12:02:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Clean(candidate) => panic!(
            "invalid structured merge must not advance: {:?}",
            candidate.snapshot.file_bytes_for_path("app/config.json")
        ),
        MergeOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].paths, vec!["app/config.json"]);
            assert_eq!(
                conflicts[0].conflict_kind,
                bowline_local::sync::ConflictKind::StructuredText
            );
            assert_eq!(
                conflicts[0].reason,
                "structured text merge did not validate"
            );
        }
    }
}

#[test]
fn structured_config_merge_that_fails_validation_conflicts_without_advancing() {
    for (path, base_bytes, local_bytes, remote_bytes) in [
        (
            "app/config.toml",
            b"a = 1\nb = 1\n".as_slice(),
            b"a = 2\nb = 1\n".as_slice(),
            b"a = 1\nb = \n".as_slice(),
        ),
        (
            "app/pnpm-lock.yaml",
            b"a: 1\nb: 1\n".as_slice(),
            b"a: 2\nb: 1\n".as_slice(),
            b"a: 1\nb: [\n".as_slice(),
        ),
        (
            "app/config.xml",
            b"<root>\n<a>1</a>\n<b>1</b>\n</root>\n".as_slice(),
            b"<root>\n<a>2</a>\n<b>1</b>\n</root>\n".as_slice(),
            b"<root>\n<a>1</a>\n<b>\n</root>\n".as_slice(),
        ),
    ] {
        let workspace_id = WorkspaceId::new(format!(
            "ws_structured_{}",
            path.replace(['/', '.', '-'], "_")
        ));
        let base = snapshot_with_file(&workspace_id, "snap_structured_base", path, base_bytes);
        let local_ref = bowline_control_plane::WorkspaceRef {
            workspace_id: workspace_id.as_str().to_string(),
            version: 1,
            snapshot_id: "snap_structured_base".to_string(),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            updated_by_device_id: Some("device-a".to_string()),
        };
        let local_candidate = coalesced_candidate_from_snapshot(
            &workspace_id,
            &local_ref,
            "device-a",
            snapshot_with_file(&workspace_id, "snap_structured_local", path, local_bytes),
        );
        let remote =
            snapshot_with_file(&workspace_id, "snap_structured_remote", path, remote_bytes);
        let remote_ref = bowline_control_plane::WorkspaceRef {
            workspace_id: workspace_id.as_str().to_string(),
            version: 2,
            snapshot_id: remote.manifest.snapshot_id.as_str().to_string(),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some("device-b".to_string()),
        };

        let outcome = merge_snapshots(
            &base,
            &local_candidate,
            &remote,
            bowline_local::sync::CandidateBase::from_remote(&remote_ref),
            [9_u8; 32],
            "2026-06-26T12:07:00Z",
        )
        .expect("merge");

        match outcome {
            MergeOutcome::Clean(candidate) => panic!(
                "invalid structured merge for {path} must not advance: {:?}",
                candidate.snapshot.file_bytes_for_path(path)
            ),
            MergeOutcome::Conflicted(conflicts) => {
                assert_eq!(conflicts.len(), 1, "{path}");
                assert_eq!(conflicts[0].paths, vec![path], "{path}");
                assert_eq!(
                    conflicts[0].conflict_kind,
                    bowline_local::sync::ConflictKind::StructuredText,
                    "{path}"
                );
                assert_eq!(
                    conflicts[0].reason, "structured text merge did not validate",
                    "{path}"
                );
            }
        }
    }
}

#[test]
fn stale_clean_merge_is_rebased_on_remote_head_for_retry() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_base",
        "app/config.toml",
        b"a = 1\nb = 1\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_local",
            "app/config.toml",
            b"a = 2\nb = 1\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_remote",
        "app/config.toml",
        b"a = 1\nb = 2\n",
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: remote.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:04:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Conflicted(conflicts) => panic!("expected clean merge, got {conflicts:?}"),
        MergeOutcome::Clean(candidate) => {
            assert_eq!(candidate.base.version, 2);
            assert_eq!(candidate.base.snapshot_id.as_str(), "snap_remote");
            assert_eq!(
                candidate
                    .snapshot
                    .file_bytes_for_path("app/config.toml")
                    .expect("merged bytes"),
                b"a = 2\nb = 2\n"
            );
        }
    }
}

#[test]
fn stale_non_overlapping_insert_and_edit_auto_merge() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_base",
        "app/config.toml",
        b"a = 1\nb = 1\nc = 1\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_local",
            "app/config.toml",
            b"a = 1\ninserted = true\nb = 1\nc = 1\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_remote",
        "app/config.toml",
        b"a = 1\nb = 1\nc = 2\n",
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: remote.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:05:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Conflicted(conflicts) => {
            panic!("expected independent insert/edit to merge cleanly, got {conflicts:?}")
        }
        MergeOutcome::Clean(candidate) => {
            assert_eq!(
                candidate
                    .snapshot
                    .file_bytes_for_path("app/config.toml")
                    .expect("merged bytes"),
                b"a = 1\ninserted = true\nb = 1\nc = 2\n"
            );
        }
    }
}

#[test]
fn stale_remote_delete_auto_merges_when_local_only_differs_by_transport_metadata() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_base",
        "app/config.toml",
        b"value = 1\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let mut local_snapshot = snapshot_with_file(
        &workspace_id,
        "snap_local",
        "app/config.toml",
        b"value = 1\n",
    );
    local_snapshot.manifest.entries[0].locator = None;
    local_snapshot.manifest.entries[0].hydration_state = HydrationState::Local;
    let local_candidate =
        coalesced_candidate_from_snapshot(&workspace_id, &local_ref, "device-a", local_snapshot);
    let remote = empty_snapshot(&workspace_id, "snap_remote_deleted");
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: remote.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-24T12:09:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Conflicted(conflicts) => {
            panic!("unchanged local file should accept remote delete, got {conflicts:?}")
        }
        MergeOutcome::Clean(candidate) => {
            assert!(
                candidate.snapshot.manifest.entries.is_empty(),
                "remote delete should win when local is semantically unchanged"
            );
        }
    }
}

#[test]
fn clean_merge_snapshot_id_includes_symlink_target_metadata() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_symlink(&workspace_id, "snap_base", "app/link", "target-a");
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_symlink(&workspace_id, "snap_local", "app/link", "target-a"),
    );

    let first = merge_snapshots(
        &base,
        &local_candidate,
        &snapshot_with_symlink(&workspace_id, "snap_remote_b", "app/link", "target-b"),
        bowline_local::sync::CandidateBase::from_remote(&bowline_control_plane::WorkspaceRef {
            workspace_id: workspace_id.as_str().to_string(),
            version: 2,
            snapshot_id: "snap_remote_b".to_string(),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some("device-b".to_string()),
        }),
        [9_u8; 32],
        "2026-06-24T12:08:00Z",
    )
    .expect("first merge");
    let second = merge_snapshots(
        &base,
        &local_candidate,
        &snapshot_with_symlink(&workspace_id, "snap_remote_c", "app/link", "target-c"),
        bowline_local::sync::CandidateBase::from_remote(&bowline_control_plane::WorkspaceRef {
            workspace_id: workspace_id.as_str().to_string(),
            version: 2,
            snapshot_id: "snap_remote_c".to_string(),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some("device-b".to_string()),
        }),
        [9_u8; 32],
        "2026-06-24T12:08:00Z",
    )
    .expect("second merge");

    let first_id = match first {
        MergeOutcome::Clean(candidate) => candidate.snapshot.manifest.snapshot_id.clone(),
        MergeOutcome::Conflicted(conflicts) => panic!("expected clean merge: {conflicts:?}"),
    };
    let second_id = match second {
        MergeOutcome::Clean(candidate) => candidate.snapshot.manifest.snapshot_id.clone(),
        MergeOutcome::Conflicted(conflicts) => panic!("expected clean merge: {conflicts:?}"),
    };

    assert_ne!(
        first_id, second_id,
        "clean non-file metadata changes must produce distinct merged snapshots"
    );
}

#[test]
fn imported_manifest_rejects_paths_targeting_private_state() {
    let workspace_id = WorkspaceId::new("ws_code");
    for path in [
        ".bowline/private.json",
        ".bowline/conflicts/conflict_1/local/.env",
    ] {
        let mut snapshot = snapshot_with_file(&workspace_id, "snap_bad", "app/main.ts", b"ok");
        snapshot.manifest.entries[0].path = path.to_string();

        let result = bowline_local::sync::download::validate_imported_manifest(
            &workspace_id,
            &bowline_core::ids::SnapshotId::new("snap_bad"),
            &snapshot.manifest,
        );

        assert!(
            matches!(result, Err(DownloadError::UnsafePath(_))),
            "{path} must be rejected as bowline private state"
        );
    }
}

#[test]
fn imported_manifest_rejects_symlink_targets_outside_workspace() {
    let workspace_id = WorkspaceId::new("ws_code");
    for target in [
        "/workspace/user/.ssh/config",
        "../outside",
        "app/../outside",
    ] {
        let snapshot = snapshot_with_symlink(&workspace_id, "snap_bad_link", "app/config", target);

        let result = bowline_local::sync::download::validate_imported_manifest(
            &workspace_id,
            &bowline_core::ids::SnapshotId::new("snap_bad_link"),
            &snapshot.manifest,
        );

        assert!(
            matches!(result, Err(DownloadError::UnsafeManifest(_))),
            "{target} must be rejected as an unsafe symlink target"
        );
    }
}

#[test]
fn imported_manifest_rejects_case_only_path_collisions() {
    let workspace_id = WorkspaceId::new("ws_code");
    let mut snapshot = snapshot_with_file(&workspace_id, "snap_case", "app/Main.ts", b"first");
    let second = snapshot_with_file(&workspace_id, "snap_case", "app/main.ts", b"second")
        .manifest
        .entries
        .into_iter()
        .next()
        .expect("entry");
    snapshot.manifest.entries.push(second);

    let result = bowline_local::sync::download::validate_imported_manifest(
        &workspace_id,
        &bowline_core::ids::SnapshotId::new("snap_case"),
        &snapshot.manifest,
    );

    assert!(matches!(result, Err(DownloadError::UnsafeManifest(_))));

    let mut ancestor_snapshot =
        snapshot_with_file(&workspace_id, "snap_case", "App/a.ts", b"first");
    let second = snapshot_with_file(&workspace_id, "snap_case", "app/b.ts", b"second")
        .manifest
        .entries
        .into_iter()
        .next()
        .expect("entry");
    ancestor_snapshot.manifest.entries.push(second);

    let result = bowline_local::sync::download::validate_imported_manifest(
        &workspace_id,
        &bowline_core::ids::SnapshotId::new("snap_case"),
        &ancestor_snapshot.manifest,
    );

    assert!(
        matches!(result, Err(DownloadError::UnsafeManifest(_))),
        "case-only ancestor directory collisions must be rejected"
    );
}

#[test]
fn imported_manifest_rejects_plaintext_snapshot_id_mismatch() {
    let workspace_id = WorkspaceId::new("ws_code");
    let snapshot = snapshot_with_file(&workspace_id, "snap_plaintext", "app/main.ts", b"ok");

    let result = bowline_local::sync::download::validate_imported_manifest(
        &workspace_id,
        &bowline_core::ids::SnapshotId::new("snap_pointer"),
        &snapshot.manifest,
    );

    assert!(matches!(result, Err(DownloadError::UnsafeManifest(_))));
}

#[cfg(unix)]
#[test]
fn coalescer_syncs_symlink_shape_without_reading_target_bytes() {
    let workspace = TempWorkspace::new("phase7-symlink").expect("workspace");
    let outside = TempWorkspace::new("phase7-symlink-outside").expect("outside");
    workspace
        .write_file("package.json", b"{}")
        .expect("package");
    workspace
        .write_file("src/main.ts", b"export const value = 1;\n")
        .expect("source");
    outside
        .write_file("outside-secret.txt", b"do not upload\n")
        .expect("outside");
    workspace
        .create_symlink(
            "",
            "linked-secret.txt",
            outside.root().join("outside-secret.txt"),
        )
        .expect("symlink");
    workspace
        .create_symlink("", "linked-source.ts", "src/main.ts")
        .expect("relative symlink");
    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");

    let candidate = coalesce_workspace_scan(
        workspace.root(),
        WorkspaceId::new("ws_code"),
        &base_ref,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:05:00Z",
    )
    .expect("candidate");

    assert!(
        candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .all(|entry| entry.path != "linked-secret.txt"),
        "absolute out-of-workspace symlinks must stay local-only so peers never reject the snapshot"
    );
    let link = candidate
        .snapshot
        .manifest
        .entries
        .iter()
        .find(|entry| entry.path == "linked-source.ts")
        .expect("link entry");
    assert_eq!(link.kind, NamespaceEntryKind::Symlink);
    assert_eq!(link.symlink_target.as_deref(), Some("src/main.ts"));
    assert!(
        !candidate
            .snapshot
            .files
            .values()
            .any(|bytes| bytes == b"do not upload\n")
    );

    let first_snapshot_id = candidate.snapshot.manifest.snapshot_id.clone();
    fs::remove_file(workspace.root().join("linked-source.ts")).expect("remove old symlink");
    workspace
        .write_file("src/other.ts", b"export const other = 2;\n")
        .expect("second source");
    workspace
        .create_symlink("", "linked-source.ts", "src/other.ts")
        .expect("second symlink");

    let retargeted = coalesce_workspace_scan(
        workspace.root(),
        WorkspaceId::new("ws_code"),
        &base_ref,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:06:00Z",
    )
    .expect("retargeted candidate");

    assert_ne!(
        retargeted.snapshot.manifest.snapshot_id, first_snapshot_id,
        "changing only a symlink target must publish a distinct snapshot"
    );
}

#[test]
fn coalescer_excludes_bowline_private_state_from_upload() {
    let workspace = TempWorkspace::new("phase7-private-state").expect("workspace");
    workspace
        .write_file("package.json", b"{}")
        .expect("package");
    workspace
        .write_file(".bowline/conflicts/conflict_1/local/app.env", b"SECRET=1\n")
        .expect("private state");
    workspace
        .write_file(".bowline/conflicts/conflict_2/local/app.env", b"SECRET=2\n")
        .expect("private state");
    workspace
        .write_file(".bowline-conflicts/conflict_3/local/app.env", b"SECRET=3\n")
        .expect("ordinary user file");
    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");

    let candidate = coalesce_workspace_scan(
        workspace.root(),
        WorkspaceId::new("ws_code"),
        &base_ref,
        DeviceId::new("device-a"),
        [3_u8; 32],
        "2026-06-24T12:06:00Z",
    )
    .expect("candidate");

    assert!(
        candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .all(|entry| entry.path != ".bowline"
                && !entry.path.starts_with(".bowline/")
                && !entry.path.contains("SECRET"))
    );
    assert!(
        candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == ".bowline-conflicts/conflict_3/local/app.env")
    );
}

#[test]
fn runner_masks_unresolved_conflict_paths_from_next_upload() {
    let workspace = TempWorkspace::new("phase7-conflict-mask-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-conflict-mask-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "config.toml", b"value = \"local unresolved\"\n")
        .expect("local unresolved");
    create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path("app/config.toml"),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"local unresolved\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("conflict bundle");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 12).expect("byte store");
    let storage_key = StorageKey::deterministic(12);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [12_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:07:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    let outcome = runner.tick().expect("tick");
    let snapshot_id = match outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => {
                workspace_ref.snapshot_id
            }
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("first upload should advance")
            }
        },
        other => panic!("expected upload, got {other:?}"),
    };
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import");

    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/package.json")
    );
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .all(|entry| entry.path != "app/config.toml"),
        "unresolved conflict path must not be re-uploaded before accept/reject"
    );
}

#[test]
fn runner_syncs_later_non_overlapping_edits_inside_unresolved_conflict_file() {
    let workspace =
        TempWorkspace::new("phase7-conflict-continuation-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-conflict-continuation-state").expect("state");
    let remote_seed =
        TempWorkspace::new("phase7-conflict-continuation-remote-seed").expect("remote seed");
    let workspace_id = WorkspaceId::new("ws_code");
    let content_key = [21_u8; 32];
    let storage_key = StorageKey::deterministic(21);
    let recorded_local = b"title = \"local\"\nshared = \"same\"\n";
    let recorded_remote = b"title = \"remote\"\nshared = \"same\"\n";
    let live_local = b"title = \"local\"\nshared = \"same\"\nlater = \"kept\"\n";
    let expected_uploaded = b"title = \"remote\"\nshared = \"same\"\nlater = \"kept\"\n";

    remote_seed
        .write_project_file("app", "config.toml", recorded_remote)
        .expect("remote config");
    workspace
        .write_project_file("app", "config.toml", live_local)
        .expect("live config");
    for index in 0..200 {
        let path = format!("src/generated_{index:03}.ts");
        let bytes = format!("export const generated{index} = {index};\n");
        remote_seed
            .write_project_file("app", &path, bytes.as_bytes())
            .expect("remote generated source");
        workspace
            .write_project_file("app", &path, bytes.as_bytes())
            .expect("local generated source");
    }

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 21).expect("byte store");
    let remote_candidate = coalesce_workspace_scan(
        remote_seed.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-b"),
        content_key,
        "2026-06-24T12:08:00Z",
    )
    .expect("remote candidate");
    let remote_ref = match upload_snapshot_candidate(
        &remote_candidate,
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("remote upload")
    {
        bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
        bowline_local::sync::UploadOutcome::Stale { .. } => panic!("seed upload should advance"),
    };

    let store = MetadataStore::open(state.root().join("local.sqlite3")).expect("metadata");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: remote_ref.clone(),
            observed_at: "2026-06-24T12:08:30Z".to_string(),
        })
        .expect("local head");
    drop(store);

    let bundle = create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path_span(
            "app/config.toml",
            ConflictSpan {
                path: "app/config.toml".to_string(),
                base_start_line: 1,
                base_end_line: 1,
                local_start_line: 1,
                local_end_line: 1,
                remote_start_line: 1,
                remote_end_line: 1,
                base_context_hash: None,
                local_context_hash: None,
                remote_context_hash: None,
            },
        ),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"title = \"base\"\nshared = \"same\"\n".to_vec()),
            local: Some(recorded_local.to_vec()),
            remote: Some(recorded_remote.to_vec()),
        }],
    )
    .expect("conflict bundle");

    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: content_key,
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    let outcome = runner.tick().expect("tick");
    let snapshot_id = match outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => {
                workspace_ref.snapshot_id
            }
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("continuation should advance")
            }
        },
        other => panic!("expected upload, got {other:?}"),
    };
    let imported = import_snapshot_by_id(
        &workspace_id,
        &bowline_core::ids::SnapshotId::new(snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import");

    let entry = imported
        .manifest
        .entries
        .iter()
        .find(|entry| entry.path == "app/config.toml")
        .expect("continued config entry");
    let locator = entry.locator.as_ref().expect("continued config locator");
    let pack_id = locator.pack_id.as_ref().expect("continued config pack id");
    let pack_object = imported
        .pack_objects
        .iter()
        .find(|pointer| pointer.content_id == pack_id.as_str())
        .expect("continued config pack object");
    let hydrated = LocalContentCache::open(state.root().join("cache"))
        .expect("content cache")
        .hydrate_record_from_range(
            &byte_store,
            RangeHydrationRequest {
                object_key: &ObjectKey::new(pack_object.object_key.clone()).expect("object key"),
                workspace_id: &workspace_id,
                locator,
                content_key,
                key: storage_key,
                key_epoch: 1,
            },
        )
        .expect("continued config hydration");

    assert_eq!(
        hydrated, expected_uploaded,
        "safe later edit should sync while the unresolved line stays on the remote side"
    );
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/src/generated_199.ts"),
        "many-file trees must keep syncing around an unresolved conflict span"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.root.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    assert_eq!(manifest["state"].as_str(), Some("unresolved"));
}

#[test]
fn runner_does_not_mask_rejected_conflict_paths_after_resolution() {
    let workspace =
        TempWorkspace::new("phase7-rejected-conflict-mask-workspace").expect("workspace");
    let state = TempWorkspace::new("phase7-rejected-conflict-mask-state").expect("state");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    workspace
        .write_project_file("app", "config.toml", b"value = \"rejected local\"\n")
        .expect("local unresolved");
    let bundle = create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path("app/config.toml"),
        &[ConflictFile {
            relative_path: "app/config.toml".to_string(),
            base: Some(b"value = \"base\"\n".to_vec()),
            local: Some(b"value = \"rejected local\"\n".to_vec()),
            remote: Some(b"value = \"remote\"\n".to_vec()),
        }],
    )
    .expect("conflict bundle");
    let manifest_path = bundle.root.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest")).expect("json");
    manifest["state"] = serde_json::Value::String("rejected".to_string());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .expect("write rejected manifest");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 16).expect("byte store");
    let storage_key = StorageKey::deterministic(16);
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [16_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:07:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    let outcome = runner.tick().expect("tick");
    let snapshot_id = match outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => {
                workspace_ref.snapshot_id
            }
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("first upload should advance")
            }
        },
        other => panic!("expected upload, got {other:?}"),
    };
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import");

    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/package.json")
    );
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/config.toml"),
        "rejected conflicts should not silently suppress future uploads after resolve applies a concrete view"
    );
}

#[test]
fn runner_idles_after_recording_unresolved_conflict() {
    let source = TempWorkspace::new("phase7-conflict-stable-source").expect("source workspace");
    let source_state =
        TempWorkspace::new("phase7-conflict-stable-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-conflict-stable-peer").expect("peer workspace");
    let peer_state = TempWorkspace::new("phase7-conflict-stable-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 15)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(15);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [15_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:12:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base tick"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [15_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:13:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer import tick"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    source
        .write_project_file("app", "remote.ts", b"export const remote = true;\n")
        .expect("remote-only file");
    assert!(matches!(
        source_runner.tick().expect("source remote tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");

    let conflict = peer_runner.tick().expect("conflict tick");
    assert!(matches!(conflict, SyncTickOutcome::Conflicted(_)));
    let listed_conflicts = control_plane
        .list_workspace_conflicts("ws_code", "device-b")
        .expect("published conflict metadata");
    assert_eq!(listed_conflicts.len(), 1);
    assert_eq!(listed_conflicts[0].paths, vec!["app/config.toml"]);
    assert_eq!(listed_conflicts[0].state, "unresolved");
    assert_eq!(
        fs::read(peer.root().join("app").join("remote.ts")).expect("remote-only file"),
        b"export const remote = true;\n",
        "non-conflicting remote files must materialize before recording the remote head locally"
    );
    let second = peer_runner.tick().expect("second peer tick");

    assert_eq!(second, SyncTickOutcome::NoChanges);
    let current_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(current_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import current head");
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/remote.ts"),
        "the conflicted peer must not delete non-conflicting remote files on the next tick"
    );
    assert_eq!(
        fs::read(peer.root().join("app").join("config.toml")).expect("local unresolved file"),
        b"value = \"local\"\n",
        "unresolved local view should remain in place until accept/reject"
    );
}

#[test]
fn unresolved_conflict_metadata_publish_retries_existing_bundle_after_failure() {
    let source = TempWorkspace::new("phase7-conflict-publish-retry-source").expect("source");
    let source_state =
        TempWorkspace::new("phase7-conflict-publish-retry-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-conflict-publish-retry-peer").expect("peer");
    let peer_state =
        TempWorkspace::new("phase7-conflict-publish-retry-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let inner = FakeControlPlaneClient::default();
    inner.create_workspace("ws_code");
    let control_plane = CasFailsOnceControlPlane::new_conflict_publish_fails_once(inner);
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 32)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(32);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [32_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:20:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base tick"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [32_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:21:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer import tick"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    assert!(matches!(
        source_runner.tick().expect("source remote tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");

    let first_error = peer_runner
        .tick()
        .expect_err("first conflict metadata publish fails");
    assert!(
        first_error
            .to_string()
            .contains("injected conflict metadata publish failure")
    );
    assert_eq!(
        control_plane
            .list_workspace_conflicts("ws_code", "device-b")
            .expect("no published conflicts yet")
            .len(),
        0
    );

    assert_eq!(
        peer_runner.tick().expect("retry publishes existing bundle"),
        SyncTickOutcome::NoChanges
    );
    assert_eq!(
        control_plane
            .list_workspace_conflicts("ws_code", "device-b")
            .expect("published conflict metadata after retry")
            .len(),
        1
    );
    let entries = fs::read_dir(peer_state.root().join("conflicts"))
        .expect("conflicts dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("conflicts");
    assert_eq!(entries.len(), 1);
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(entries[0].path().join("manifest.json")).expect("manifest"),
    )
    .expect("manifest json");
    assert!(
        manifest.get("remoteConflictPublishedAt").is_some(),
        "successful retry must locally acknowledge remote conflict metadata publication"
    );
}

#[test]
fn accepted_conflict_resolution_advances_ref_and_imports_on_peer() {
    let source = TempWorkspace::new("phase7-accepted-resolution-source").expect("source");
    let source_state =
        TempWorkspace::new("phase7-accepted-resolution-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-accepted-resolution-peer").expect("peer");
    let peer_state =
        TempWorkspace::new("phase7-accepted-resolution-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 17)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(17);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [17_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:14:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base upload"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [17_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:15:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer base import"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    assert!(matches!(
        source_runner.tick().expect("source remote upload"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");
    assert!(matches!(
        peer_runner.tick().expect("peer conflict"),
        SyncTickOutcome::Conflicted(_)
    ));
    assert_eq!(
        control_plane
            .list_workspace_conflicts("ws_code", "device-b")
            .expect("published conflict metadata")
            .len(),
        1
    );

    peer.write_project_file("app", "config.toml", b"value = \"resolved\"\n")
        .expect("accepted resolution bytes");
    mark_only_conflict_bundle_state(peer_state.root(), "accepted");

    assert!(matches!(
        peer_runner.tick().expect("peer accepted resolution upload"),
        SyncTickOutcome::Uploaded(_)
    ));
    assert_eq!(
        control_plane
            .list_workspace_conflicts("ws_code", "device-b")
            .expect("resolved conflict metadata")
            .len(),
        0
    );
    let resolved_event_count = control_plane
        .list_events("ws_code")
        .expect("events")
        .iter()
        .filter(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictResolved)
        .count();
    assert_eq!(
        peer_runner.tick().expect("post-resolution idle"),
        SyncTickOutcome::NoChanges
    );
    assert_eq!(
        control_plane
            .list_events("ws_code")
            .expect("events")
            .iter()
            .filter(|event| event.kind == bowline_control_plane::CompactEventKind::ConflictResolved)
            .count(),
        resolved_event_count,
        "resolved conflicts must not re-publish terminal metadata on idle ticks"
    );
    assert!(matches!(
        source_runner
            .tick()
            .expect("source imports accepted resolution"),
        SyncTickOutcome::Imported(_)
    ));
    assert_eq!(
        fs::read(source.root().join("app").join("config.toml")).expect("source resolved file"),
        b"value = \"resolved\"\n"
    );

    let current_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(current_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import current head");
    assert_eq!(current_ref.version, 3);
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/config.toml"),
        "accepted resolution must remain in the workspace head"
    );
}

#[test]
fn rejected_conflict_resolution_adopts_remote_head_without_new_ref() {
    let source = TempWorkspace::new("phase7-rejected-resolution-source").expect("source");
    let source_state =
        TempWorkspace::new("phase7-rejected-resolution-source-state").expect("source state");
    let peer = TempWorkspace::new("phase7-rejected-resolution-peer").expect("peer");
    let peer_state =
        TempWorkspace::new("phase7-rejected-resolution-peer-state").expect("peer state");
    source
        .write_project_file("app", "config.toml", b"value = \"base\"\n")
        .expect("base");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 18)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(18);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [18_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:16:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source base upload"),
        SyncTickOutcome::Uploaded(_)
    ));

    let peer_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: peer.root().to_path_buf(),
            state_root: peer_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [18_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:17:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        peer_runner.tick().expect("peer base import"),
        SyncTickOutcome::Imported(_)
    ));

    source
        .write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("remote edit");
    assert!(matches!(
        source_runner.tick().expect("source remote upload"),
        SyncTickOutcome::Uploaded(_)
    ));
    peer.write_project_file("app", "config.toml", b"value = \"local\"\n")
        .expect("local edit");
    assert!(matches!(
        peer_runner.tick().expect("peer conflict"),
        SyncTickOutcome::Conflicted(_)
    ));

    peer.write_project_file("app", "config.toml", b"value = \"remote\"\n")
        .expect("rejected resolution adopts remote bytes");
    mark_only_conflict_bundle_state(peer_state.root(), "rejected");

    assert_eq!(
        peer_runner.tick().expect("peer rejected resolution tick"),
        SyncTickOutcome::NoChanges
    );
    let current_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");
    assert_eq!(
        current_ref.version, 2,
        "rejecting current remote side should not create a duplicate workspace ref"
    );
    assert_eq!(
        fs::read(peer.root().join("app").join("config.toml")).expect("peer remote file"),
        b"value = \"remote\"\n"
    );
}

#[test]
fn fresh_empty_runner_imports_remote_head_without_overwriting_it() {
    let source = TempWorkspace::new("phase7-fresh-import-source").expect("source workspace");
    let source_state =
        TempWorkspace::new("phase7-fresh-import-source-state").expect("source state");
    let fresh = TempWorkspace::new("phase7-fresh-import-empty").expect("fresh workspace");
    let fresh_state = TempWorkspace::new("phase7-fresh-import-empty-state").expect("fresh state");
    source
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    source
        .write_project_file("app", ".bowlinesetup", b"echo setup\n")
        .expect("setup recipe");
    source
        .write_project_file("app", "src/main.ts", b"export const value = 1;\n")
        .expect("source");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 13)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(13);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [13_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:08:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let uploaded_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");
    let before_import_metrics = byte_store.metrics();
    {
        let initialized_metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
            .expect("initialized metadata");
        initialized_metadata
            .insert_workspace(&WorkspaceId::new("ws_code"), "Code", "2026-06-24T12:08:30Z")
            .expect("initialized workspace");
        initialized_metadata
            .insert_root(
                "root_code",
                &WorkspaceId::new("ws_code"),
                &fresh.root().display().to_string(),
                "2026-06-24T12:08:30Z",
            )
            .expect("initialized root");
        initialized_metadata
            .enqueue_hydration(&HydrationQueueRecord {
                id: "hydrate-main".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                project_id: None,
                path: "app/src/main.ts".to_string(),
                content_id: None,
                priority: "hot-project-prefetch".to_string(),
                state: "queued".to_string(),
                cause: "hot-project-prefetch".to_string(),
                updated_at: "2026-06-24T12:08:30Z".to_string(),
            })
            .expect("queued hydration");
    }

    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [13_u8; 32],
            storage_key,
            key_epoch: 2,
            generated_at: "2026-06-24T12:09:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    let outcome = fresh_runner.tick().expect("fresh tick");

    assert!(matches!(outcome, SyncTickOutcome::Imported(_)));
    let after_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");
    assert_eq!(
        after_ref.version, uploaded_ref.version,
        "fresh empty device must not advance remote head with an empty snapshot"
    );
    assert_eq!(after_ref.snapshot_id, uploaded_ref.snapshot_id);
    assert!(
        fresh.root().join("app").join("src").is_dir(),
        "fresh import should materialize directories"
    );
    assert_eq!(
        fs::read(fresh.root().join("app").join("package.json")).expect("package manifest"),
        br#"{"name":"app"}"#,
        "package manifests should materialize as normal source bytes"
    );
    assert_eq!(
        fs::read(fresh.root().join("app").join("src").join("main.ts")).expect("source file"),
        b"export const value = 1;\n",
        "fresh import should materialize canonical source bytes into the real root"
    );
    let after_import_metrics = byte_store.metrics();
    assert_eq!(
        after_import_metrics.full_read_count - before_import_metrics.full_read_count,
        2,
        "fresh import should full-read the encrypted manifest and source pack"
    );
    assert_eq!(
        after_import_metrics.range_read_count - before_import_metrics.range_read_count,
        0,
        "fresh import should hydrate from the prefetched source pack instead of per-file range reads"
    );
    let metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
        .expect("fresh import metadata");
    let pack_records = metadata
        .pack_records(&WorkspaceId::new("ws_code"))
        .expect("pack records");
    assert_eq!(pack_records.len(), 1);
    assert!(
        !pack_records[0].object_hash.is_empty(),
        "structure import should persist real pack object metadata"
    );
    assert_eq!(
        pack_records[0].key_epoch, 1,
        "imported pack metadata should preserve the remote object epoch"
    );
    assert_eq!(
        metadata
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/src/main.ts")
            .expect("projected node lookup")
            .expect("projected node")
            .hydration_state,
        HydrationState::Local
    );
    assert_eq!(
        metadata
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/package.json")
            .expect("package projected node lookup")
            .expect("package projected node")
            .hydration_state,
        HydrationState::Local,
        "materialized files are real local files, not cold placeholders"
    );
    assert!(
        metadata
            .hydration_queue(&WorkspaceId::new("ws_code"))
            .expect("hydration queue")
            .iter()
            .any(|record| record.path == "app/src/main.ts" && record.state == "completed"),
        "remote import should settle queued hot-project hydration after writing real bytes"
    );

    source
        .write_project_file("app", "src/remote.ts", b"export const remote = 2;\n")
        .expect("remote update");
    assert!(
        matches!(
            source_runner.tick().expect("source remote update tick"),
            SyncTickOutcome::Uploaded(_)
        ),
        "source remote update should advance"
    );
    assert!(
        matches!(
            fresh_runner.tick().expect("fresh remote update tick"),
            SyncTickOutcome::Imported(_)
        ),
        "remote-only updates should import and materialize the new source bytes"
    );
    assert_eq!(
        fs::read(fresh.root().join("app").join("src").join("remote.ts"))
            .expect("remote source materialized"),
        b"export const remote = 2;\n",
        "remote-only ordinary source updates should appear in the real root"
    );

    fs::remove_file(source.root().join("app").join(".bowlinesetup")).expect("remote setup delete");
    assert!(
        matches!(
            source_runner.tick().expect("source remote delete tick"),
            SyncTickOutcome::Uploaded(_)
        ),
        "source remote delete should advance"
    );
    assert!(
        matches!(
            fresh_runner.tick().expect("fresh remote delete tick"),
            SyncTickOutcome::Imported(_)
        ),
        "remote-only deletes should update the real root"
    );
    assert!(
        !fresh.root().join("app").join(".bowlinesetup").exists(),
        "remote deletion of a bootstrap-materialized file should remove the local file"
    );
    assert!(
        matches!(
            fresh_runner.tick().expect("fresh after remote delete tick"),
            SyncTickOutcome::NoChanges
        ),
        "remote-deleted bootstrap files must not be resurrected as local edits"
    );

    fs::remove_file(fresh.root().join("app").join("package.json")).expect("remove package file");
    let deletion_outcome = fresh_runner.tick().expect("package deletion tick");
    let (deleted_ref, deleted_object_manifest) = match deletion_outcome {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced {
                workspace_ref,
                object_manifest,
            } => (workspace_ref, object_manifest),
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("package deletion should advance")
            }
        },
        other => panic!("expected package deletion upload, got {other:?}"),
    };
    assert_eq!(
        deleted_object_manifest.pack_objects.len(),
        1,
        "uploads that preserve cold locators must retain their source pack pointers"
    );
    let after_package_delete = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(deleted_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import after package delete");
    assert!(
        !after_package_delete
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/package.json"),
        "deleting a bootstrap-materialized file must sync as a deletion"
    );
    assert!(
        metadata
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/package.json")
            .expect("deleted package projected node lookup")
            .is_none(),
        "accepted local deletions should remove projected-node metadata"
    );
    assert!(
        after_package_delete
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/src/main.ts"),
        "cold source files must still be preserved while real local deletions sync"
    );

    fs::create_dir_all(fresh.root().join("app").join("src")).expect("recreate src dir");
    fs::write(
        fresh.root().join("app").join("src").join("main.ts"),
        b"console.log('edited locally');\n",
    )
    .expect("edit cold file");
    let edited_ref = match fresh_runner.tick().expect("edited cold file tick") {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => panic!("edit should advance"),
        },
        other => panic!("expected edit upload, got {other:?}"),
    };
    let edited_snapshot = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(edited_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import after edit");
    assert!(
        edited_snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/src/main.ts"),
        "local edits to formerly cold files should sync as real files"
    );

    fs::remove_file(fresh.root().join("app").join("src").join("main.ts"))
        .expect("delete materialized source file");
    let deleted_cold_ref = match fresh_runner.tick().expect("delete source file tick") {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => panic!("delete should advance"),
        },
        other => panic!("expected delete upload, got {other:?}"),
    };
    let deleted_cold_snapshot = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(deleted_cold_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import after source delete");
    assert!(
        !deleted_cold_snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/src/main.ts"),
        "deleting a materialized source file must sync as a deletion"
    );
    assert!(
        metadata
            .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/src/main.ts")
            .expect("deleted source projected node lookup")
            .is_none(),
        "accepted deletion of a materialized source file should remove projected-node metadata"
    );

    fs::remove_file(fresh.root().join("app").join("src").join("remote.ts"))
        .expect("delete remaining source file before replacing directory");
    fs::remove_dir(fresh.root().join("app").join("src")).expect("remove empty src dir");
    fs::remove_dir(fresh.root().join("app")).expect("remove empty app dir");
    fs::write(
        fresh.root().join("app"),
        b"local file replacing directory\n",
    )
    .expect("replace directory with file");
    let replaced_dir_ref = match fresh_runner.tick().expect("directory replacement tick") {
        SyncTickOutcome::Uploaded(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => {
                panic!("directory replacement should advance")
            }
        },
        other => panic!("expected directory replacement upload, got {other:?}"),
    };
    let replaced_dir_snapshot = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(replaced_dir_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import after directory replacement");
    assert!(
        replaced_dir_snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app" && entry.kind == NamespaceEntryKind::File),
        "local file replacement should be represented as the path itself"
    );
    assert!(
        !replaced_dir_snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path.starts_with("app/")),
        "preserved cold descendants must not survive under a local file replacement"
    );
}

#[test]
fn fresh_import_keeps_large_lazy_files_cold_without_pack_prefetch() {
    let source = TempWorkspace::new("phase7-lazy-source").expect("source workspace");
    let source_state = TempWorkspace::new("phase7-lazy-source-state").expect("source state");
    let fresh = TempWorkspace::new("phase7-lazy-fresh").expect("fresh workspace");
    let fresh_state = TempWorkspace::new("phase7-lazy-fresh-state").expect("fresh state");
    source
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let large_bytes = vec![b'x'; 8 * 1024 * 1024 + 1];
    source
        .write_project_file("app", "assets/video.bin", &large_bytes)
        .expect("large file");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 45)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(45);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [45_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:08:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let before_import_metrics = byte_store.metrics();

    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [45_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:09:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    assert!(matches!(
        fresh_runner.tick().expect("fresh import tick"),
        SyncTickOutcome::Imported(_)
    ));

    assert_eq!(
        fs::read(fresh.root().join("app/package.json")).expect("package materialized"),
        br#"{"name":"app"}"#,
        "ordinary source/config files still materialize into the real directory"
    );
    assert!(
        !fresh.root().join("app/assets/video.bin").exists(),
        "large lazy files should stay locator-backed instead of materializing during import"
    );
    let after_import_metrics = byte_store.metrics();
    assert_eq!(
        after_import_metrics.full_read_count - before_import_metrics.full_read_count,
        1,
        "fresh import should full-read only the encrypted manifest when a pack also contains cold lazy records"
    );
    assert_eq!(
        after_import_metrics.range_read_count - before_import_metrics.range_read_count,
        1,
        "fresh import should range-read only the eager source record from a mixed pack"
    );

    let metadata =
        MetadataStore::open(fresh_state.root().join("local.sqlite3")).expect("fresh metadata");
    let large_node = metadata
        .projected_node_by_path(&WorkspaceId::new("ws_code"), "app/assets/video.bin")
        .expect("large projected node lookup")
        .expect("large projected node");
    assert_eq!(large_node.hydration_state, HydrationState::Cold);
    let locators = metadata
        .content_locators(&WorkspaceId::new("ws_code"))
        .expect("content locators");
    assert!(
        locators
            .iter()
            .any(|locator| large_node.content_id.as_ref() == Some(&locator.content_id)),
        "cold large file should keep a persisted locator for later active-read hydration"
    );
}

#[test]
fn fresh_non_empty_runner_merges_with_remote_head_without_overwriting_it() {
    let source = TempWorkspace::new("phase7-fresh-merge-source").expect("source workspace");
    let source_state = TempWorkspace::new("phase7-fresh-merge-source-state").expect("source state");
    let fresh = TempWorkspace::new("phase7-fresh-merge-local").expect("fresh workspace");
    let fresh_state = TempWorkspace::new("phase7-fresh-merge-local-state").expect("fresh state");
    source
        .write_project_file("app", "remote.ts", b"export const remote = true;\n")
        .expect("remote file");
    fresh
        .write_project_file("app", "local.ts", b"export const local = true;\n")
        .expect("local file");

    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_code");
    let byte_store = LocalByteStore::open_deterministic(source_state.root().join("objects"), 14)
        .expect("byte store");
    let storage_key = StorageKey::deterministic(14);
    let source_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: source.root().to_path_buf(),
            state_root: source_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-a"),
            workspace_content_key: [14_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:10:00Z".to_string(),
            sync_operation_id: None,
        },
    );
    assert!(matches!(
        source_runner.tick().expect("source tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    let uploaded_ref = control_plane
        .get_workspace_ref("ws_code")
        .expect("workspace ref")
        .expect("workspace exists");

    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [14_u8; 32],
            storage_key,
            key_epoch: 1,
            generated_at: "2026-06-24T12:11:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    let outcome = fresh_runner.tick().expect("fresh tick");

    let merged_ref = match outcome {
        SyncTickOutcome::Merged(upload) => match *upload {
            bowline_local::sync::UploadOutcome::Advanced { workspace_ref, .. } => workspace_ref,
            bowline_local::sync::UploadOutcome::Stale { .. } => panic!("merge should advance"),
        },
        other => panic!("expected merge, got {other:?}"),
    };
    assert!(
        merged_ref.version > uploaded_ref.version,
        "fresh local additions should publish a merge, not overwrite the current head directly"
    );
    let imported = import_snapshot_by_id(
        &WorkspaceId::new("ws_code"),
        &bowline_core::ids::SnapshotId::new(merged_ref.snapshot_id),
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import merged");
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/remote.ts")
    );
    assert!(
        imported
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/local.ts")
    );
}

#[test]
fn conflict_bundle_rejects_paths_that_escape_bundle_roots() {
    let state = TempWorkspace::new("phase7-conflict-path").expect("state");
    let result = create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path("../escape.txt"),
        &[ConflictFile {
            relative_path: "../escape.txt".to_string(),
            base: Some(b"base".to_vec()),
            local: Some(b"local".to_vec()),
            remote: Some(b"remote".to_vec()),
        }],
    );

    assert!(matches!(result, Err(ConflictBundleError::UnsafePath(_))));
}

#[test]
fn conflict_bundle_marks_env_paths_as_secret_bearing() {
    let state = TempWorkspace::new("phase7-conflict-secret").expect("state");
    let bundle = create_conflict_bundle(
        state.root(),
        ConflictRecord::same_path("app/.env.local"),
        &[ConflictFile {
            relative_path: "app/.env.local".to_string(),
            base: Some(b"SECRET=base\n".to_vec()),
            local: Some(b"SECRET=local\n".to_vec()),
            remote: Some(b"SECRET=remote\n".to_vec()),
        }],
    )
    .expect("conflict bundle");
    let manifest = fs::read_to_string(bundle.root.join("manifest.json")).expect("manifest");
    let manifest: serde_json::Value = serde_json::from_str(&manifest).expect("json");

    assert_eq!(manifest["containsSecrets"], true);
}

#[test]
fn opaque_git_state_uploads_and_imports_as_encrypted_workspace_bytes() {
    let workspace = TempWorkspace::new("phase7-git-opaque-source").expect("source root");
    workspace
        .write_project_file("app", "package.json", br#"{"name":"app"}"#)
        .expect("package");
    let git = workspace.create_git_repo("app").expect("git repo");
    fs::write(git.join("refs").join("heads").join("main"), b"abc123\n").expect("branch ref");
    fs::write(git.join("packed-refs"), b"dddd refs/tags/v1\n").expect("packed refs");
    fs::write(git.join("refs").join("stash"), b"stash123\n").expect("stash ref");
    fs::create_dir_all(git.join("hooks")).expect("hooks dir");
    fs::write(git.join("hooks").join("pre-commit"), b"#!/bin/sh\n").expect("hook");
    fs::create_dir_all(git.join("modules").join("lib")).expect("submodule git dir");
    fs::write(
        git.join("modules").join("lib").join("HEAD"),
        b"ref: refs/heads/main\n",
    )
    .expect("submodule git head");
    fs::create_dir_all(git.join("lfs").join("objects").join("aa").join("bb")).expect("lfs dir");
    fs::write(
        git.join("lfs")
            .join("objects")
            .join("aa")
            .join("bb")
            .join("oid"),
        b"lfs object bytes",
    )
    .expect("lfs object");
    fs::create_dir_all(git.join("objects").join("ab")).expect("object dir");
    fs::write(git.join("objects").join("ab").join("cdef"), b"loose-object").expect("loose object");
    fs::create_dir_all(git.join("objects").join("pack")).expect("pack dir");
    fs::write(
        git.join("objects").join("pack").join("pack-main-001.pack"),
        b"git pack bytes",
    )
    .expect("pack file");
    fs::write(
        git.join("objects").join("pack").join("tmp_pack_001"),
        b"tmp",
    )
    .expect("transient pack temp");
    let detector = workspace.mutation_detector().expect("mutation detector");

    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    let state = TempWorkspace::new("phase7-git-opaque-state").expect("state root");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 21).expect("byte store");
    let workspace_id = WorkspaceId::new("ws_code");
    let storage_key = StorageKey::deterministic(21);
    let content_key = [21_u8; 32];
    let candidate = coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref,
        DeviceId::new("device-a"),
        content_key,
        "2026-06-26T12:00:00Z",
    )
    .expect("candidate");

    detector
        .assert_unchanged()
        .expect("sync should not mutate git");
    assert!(candidate.snapshot.manifest.entries.iter().any(|entry| {
        entry.path == "app/.git/refs/heads/main" && entry.mode == MaterializationMode::EncryptedSync
    }));
    assert!(candidate.snapshot.manifest.entries.iter().any(|entry| {
        entry.path == "app/.git/objects/ab/cdef" && entry.mode == MaterializationMode::EncryptedSync
    }));
    assert!(candidate.snapshot.manifest.entries.iter().any(|entry| {
        entry.path == "app/.git/objects/pack/pack-main-001.pack"
            && entry.mode == MaterializationMode::EncryptedSync
    }));
    for path in [
        "app/.git/packed-refs",
        "app/.git/refs/stash",
        "app/.git/hooks/pre-commit",
        "app/.git/modules/lib/HEAD",
        "app/.git/lfs/objects/aa/bb/oid",
    ] {
        assert!(
            candidate
                .snapshot
                .manifest
                .entries
                .iter()
                .any(|entry| entry.path == path && entry.mode == MaterializationMode::EncryptedSync),
            "{path} should sync as opaque Git state"
        );
    }
    assert!(
        !candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .any(|entry| entry.path == "app/.git/objects/pack/tmp_pack_001"),
        "bounded Git transients must stay local-only and out of workspace head"
    );

    let outcome =
        upload_snapshot_candidate(&candidate, &control_plane, &byte_store, storage_key, 1)
            .expect("upload");
    let object_manifest = match outcome {
        bowline_local::sync::UploadOutcome::Advanced {
            object_manifest, ..
        } => object_manifest,
        bowline_local::sync::UploadOutcome::Stale { .. } => {
            panic!("first writer should advance")
        }
    };
    assert_eq!(
        object_manifest.pack_objects.len(),
        1,
        ".git entries should pack with ordinary workspace bytes, not one remote object per Git file"
    );
    for pointer in
        std::iter::once(&object_manifest.manifest_object).chain(object_manifest.pack_objects.iter())
    {
        for forbidden in [".git", "refs", "heads", "main", "pack-main"] {
            assert!(
                !pointer.object_key.contains(forbidden),
                "object key leaked Git path detail `{forbidden}`"
            );
        }
    }

    let imported = import_snapshot_by_id(
        &workspace_id,
        &candidate.snapshot.manifest.snapshot_id,
        &control_plane,
        &byte_store,
        storage_key,
        1,
    )
    .expect("import");
    let pack_entry = imported
        .manifest
        .entries
        .iter()
        .find(|entry| entry.path == "app/.git/objects/pack/pack-main-001.pack")
        .expect("imported pack entry");
    let locator = pack_entry.locator.as_ref().expect("packed locator");
    let pack_id = locator.pack_id.as_ref().expect("pack id");
    let pack_object = imported
        .pack_objects
        .iter()
        .find(|pointer| pointer.content_id == pack_id.as_str())
        .expect("pack object pointer");
    let object_key = ObjectKey::new(pack_object.object_key.clone()).expect("object key");
    let cache = LocalContentCache::open(state.root().join("cache")).expect("content cache");
    let hydrated = cache
        .hydrate_record_from_range(
            &byte_store,
            RangeHydrationRequest {
                object_key: &object_key,
                workspace_id: &workspace_id,
                locator,
                content_key,
                key: storage_key,
                key_epoch: 1,
            },
        )
        .expect("range hydrate git pack entry");

    assert_eq!(hydrated, b"git pack bytes");

    let fresh = TempWorkspace::new("phase7-git-opaque-fresh").expect("fresh root");
    let fresh_state = TempWorkspace::new("phase7-git-opaque-fresh-state").expect("fresh state");
    {
        let initialized_metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
            .expect("initialized metadata");
        initialized_metadata
            .insert_workspace(&workspace_id, "Code", "2026-06-26T12:00:30Z")
            .expect("initialized workspace");
        initialized_metadata
            .insert_root(
                "root_code",
                &workspace_id,
                &fresh.root().display().to_string(),
                "2026-06-26T12:00:30Z",
            )
            .expect("initialized root");
    }
    let fresh_runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: fresh.root().to_path_buf(),
            state_root: fresh_state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: content_key,
            storage_key,
            key_epoch: 2,
            generated_at: "2026-06-26T12:01:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    assert!(
        matches!(
            fresh_runner.tick().expect("fresh git import tick"),
            SyncTickOutcome::Imported(_)
        ),
        "fresh devices should import opaque Git continuity into the real directory"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/refs/heads/main")).expect("branch materialized"),
        b"abc123\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/objects/ab/cdef")).expect("object materialized"),
        b"loose-object"
    );
    assert_eq!(
        fs::read(
            fresh
                .root()
                .join("app/.git/objects/pack/pack-main-001.pack")
        )
        .expect("pack materialized"),
        b"git pack bytes"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/packed-refs")).expect("packed refs"),
        b"dddd refs/tags/v1\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/refs/stash")).expect("stash ref"),
        b"stash123\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/hooks/pre-commit")).expect("hook"),
        b"#!/bin/sh\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/modules/lib/HEAD")).expect("submodule git head"),
        b"ref: refs/heads/main\n"
    );
    assert_eq!(
        fs::read(fresh.root().join("app/.git/lfs/objects/aa/bb/oid")).expect("lfs object"),
        b"lfs object bytes"
    );
    assert!(
        !fresh
            .root()
            .join("app/.git/objects/pack/tmp_pack_001")
            .exists(),
        "bounded Git transients must not rematerialize on fresh devices"
    );
    let fresh_metadata = MetadataStore::open(fresh_state.root().join("local.sqlite3"))
        .expect("fresh import metadata");
    assert_eq!(
        fresh_metadata
            .projected_node_by_path(&workspace_id, "app/.git/objects/pack/pack-main-001.pack")
            .expect("projected node lookup")
            .expect("git pack projected node")
            .hydration_state,
        HydrationState::Local
    );
    let events = fresh_metadata.list_events(20).expect("fresh import events");
    assert!(events.iter().any(|event| {
        event.name == EventName::HydrationStarted
            && event
                .summary
                .contains("Remote snapshot materialization started")
            && event.summary.contains("byte(s)")
    }));
    assert!(events.iter().any(|event| {
        event.name == EventName::HydrationCompleted
            && event
                .summary
                .contains("Remote snapshot materialization completed")
            && event.payload.get("cause").and_then(|value| value.as_str()) == Some("remote-import")
    }));
    let status = compose_status(StatusOptions {
        db_path: Some(fresh_state.root().join("local.sqlite3")),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-26T12:01:01Z".to_string(),
    })
    .expect("status composes");
    assert!(
        status.hydration_progress.iter().any(|progress| {
            progress.cause == "remote-import"
                && progress.bytes_done > 0
                && progress.bytes_remaining == 0
        }),
        "hydration progress: {:#?}; events: {events:#?}",
        status.hydration_progress
    );
}

#[test]
fn missing_remote_materialization_records_blocked_hydration_event() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = FakeControlPlaneClient::default();
    let base_ref = control_plane.create_workspace("ws_code");
    control_plane
        .compare_and_swap_workspace_ref(
            workspace_id.as_str(),
            base_ref.version,
            "snap_missing_manifest",
            "device-a",
        )
        .expect("advance fake remote ref");
    let root = TempWorkspace::new("phase7-missing-remote-root").expect("root");
    let state = TempWorkspace::new("phase7-missing-remote-state").expect("state");
    let byte_store =
        LocalByteStore::open_deterministic(state.root().join("objects"), 31).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: root.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-b"),
            workspace_content_key: [31_u8; 32],
            storage_key: StorageKey::deterministic(31),
            key_epoch: 1,
            generated_at: "2026-06-26T12:02:00Z".to_string(),
            sync_operation_id: None,
        },
    );

    let error = runner.tick().expect_err("missing manifest blocks import");
    assert!(error.to_string().contains("snapshot manifest"));

    let metadata = MetadataStore::open(state.root().join("local.sqlite3")).expect("metadata opens");
    let events = metadata.list_events(20).expect("events");
    assert!(
        events.iter().any(|event| {
            event.name == EventName::HydrationBlocked
                && event
                    .summary
                    .contains("Remote snapshot materialization blocked")
                && event
                    .payload
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .is_some_and(|reason| reason.contains("snapshot manifest"))
        }),
        "events: {events:#?}"
    );
}

#[test]
fn divergent_git_files_create_opaque_conflicts_instead_of_text_merges() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = snapshot_with_file(
        &workspace_id,
        "snap_git_base",
        "app/.git/config",
        b"[core]\n\trepositoryformatversion = 0\n",
    );
    let local_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 1,
        snapshot_id: "snap_git_base".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some("device-a".to_string()),
    };
    let local_candidate = coalesced_candidate_from_snapshot(
        &workspace_id,
        &local_ref,
        "device-a",
        snapshot_with_file(
            &workspace_id,
            "snap_git_local",
            "app/.git/config",
            b"[core]\n\trepositoryformatversion = 0\n[branch \"local\"]\n",
        ),
    );
    let remote = snapshot_with_file(
        &workspace_id,
        "snap_git_remote",
        "app/.git/config",
        b"[core]\n\trepositoryformatversion = 0\n[branch \"remote\"]\n",
    );
    let remote_ref = bowline_control_plane::WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 2,
        snapshot_id: remote.manifest.snapshot_id.as_str().to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
        updated_by_device_id: Some("device-b".to_string()),
    };

    let outcome = merge_snapshots(
        &base,
        &local_candidate,
        &remote,
        bowline_local::sync::CandidateBase::from_remote(&remote_ref),
        [9_u8; 32],
        "2026-06-26T12:01:00Z",
    )
    .expect("merge");

    match outcome {
        MergeOutcome::Clean(candidate) => panic!(
            "opaque Git state must not text-merge into {:?}",
            candidate.snapshot.manifest.entries
        ),
        MergeOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].paths, vec!["app/.git/config"]);
            assert_eq!(conflicts[0].reason, "opaque Git state conflict");
        }
    }
}

fn snapshot_with_file(
    workspace_id: &WorkspaceId,
    snapshot_id: &str,
    path: &str,
    bytes: &[u8],
) -> bowline_local::sync::SnapshotContent {
    let content_id = workspace_content_id([9_u8; 32], bytes);
    bowline_local::sync::SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: bowline_core::ids::SnapshotId::new(snapshot_id),
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: vec![NamespaceEntry {
                path: path.to_string(),
                kind: NamespaceEntryKind::File,
                classification: bowline_core::policy::PathClassification::WorkspaceSync,
                mode: bowline_core::policy::MaterializationMode::WorkspaceSync,
                access: Vec::new(),
                content_id: Some(content_id.clone()),
                locator: Some(bowline_core::workspace_graph::ContentLocator {
                    content_id: content_id.clone(),
                    storage: ContentStorage::Packed,
                    raw_size: bytes.len() as u64,
                    pack_id: Some(bowline_core::ids::PackId::new("pk_test")),
                    offset: Some(0),
                    length: Some(1),
                    chunk_ids: Vec::new(),
                }),
                symlink_target: None,
                byte_len: Some(bytes.len() as u64),
                hydration_state: bowline_core::workspace_graph::HydrationState::Cold,
            }],
            refs: Vec::new(),
        },
        [(content_id, bytes.to_vec())].into_iter().collect(),
    )
}

fn snapshot_with_symlink(
    workspace_id: &WorkspaceId,
    snapshot_id: &str,
    path: &str,
    target: &str,
) -> bowline_local::sync::SnapshotContent {
    bowline_local::sync::SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: bowline_core::ids::SnapshotId::new(snapshot_id.to_string()),
            workspace_id: workspace_id.clone(),
            project_id: None::<bowline_core::ids::ProjectId>,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: vec![NamespaceEntry {
                path: path.to_string(),
                kind: NamespaceEntryKind::Symlink,
                classification: PathClassification::WorkspaceSync,
                mode: MaterializationMode::WorkspaceSync,
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                content_id: None,
                locator: None,
                symlink_target: Some(target.to_string()),
                byte_len: None,
                hydration_state: HydrationState::Local,
            }],
            refs: Vec::new(),
        },
        Default::default(),
    )
}

fn empty_snapshot(
    workspace_id: &WorkspaceId,
    snapshot_id: &str,
) -> bowline_local::sync::SnapshotContent {
    bowline_local::sync::SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: bowline_core::ids::SnapshotId::new(snapshot_id),
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: Vec::new(),
            refs: Vec::new(),
        },
        Default::default(),
    )
}

fn coalesced_candidate_from_snapshot(
    workspace_id: &WorkspaceId,
    base_ref: &bowline_control_plane::WorkspaceRef,
    device_id: &str,
    snapshot: bowline_local::sync::SnapshotContent,
) -> bowline_local::sync::SnapshotCandidate {
    bowline_local::sync::SnapshotCandidate {
        base: bowline_local::sync::CandidateBase::from_remote(base_ref),
        device_id: DeviceId::new(device_id),
        manifest_id: bowline_local::sync::manifest_id_for_snapshot(&snapshot.manifest.snapshot_id),
        snapshot,
        scan_report: bowline_local::scanner::ScanReport {
            root: std::path::PathBuf::new(),
            projects: Vec::new(),
            paths: Vec::new(),
            summary: Default::default(),
        },
        causation_ids: vec![format!("test:{}", workspace_id.as_str())],
        created_at: "2026-06-24T12:00:00Z".to_string(),
    }
}

struct CasFailsOnceControlPlane {
    inner: FakeControlPlaneClient,
    should_fail_cas: AtomicBool,
    should_fail_conflict_publish: AtomicBool,
}

impl CasFailsOnceControlPlane {
    fn new(inner: FakeControlPlaneClient) -> Self {
        Self {
            inner,
            should_fail_cas: AtomicBool::new(true),
            should_fail_conflict_publish: AtomicBool::new(false),
        }
    }

    fn new_conflict_publish_fails_once(inner: FakeControlPlaneClient) -> Self {
        Self {
            inner,
            should_fail_cas: AtomicBool::new(false),
            should_fail_conflict_publish: AtomicBool::new(true),
        }
    }
}

impl ControlPlaneClient for CasFailsOnceControlPlane {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef> {
        self.inner.create_workspace_ref(workspace_id)
    }

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.inner.get_workspace_ref(workspace_id)
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        if self.should_fail_cas.swap(false, Ordering::SeqCst) {
            return Err(CompareAndSwapError::Storage(
                "injected CAS failure after manifest commit".to_string(),
            ));
        }
        self.inner.compare_and_swap_workspace_ref(
            workspace_id,
            expected_version,
            new_snapshot_id,
            writer_device_id,
        )
    }

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>> {
        self.inner.list_events(workspace_id)
    }

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        if self
            .should_fail_conflict_publish
            .swap(false, Ordering::SeqCst)
        {
            return Err(bowline_control_plane::ControlPlaneError::Storage(
                "injected conflict metadata publish failure".to_string(),
            ));
        }
        self.inner.publish_conflict_metadata(input)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.inner
            .list_workspace_conflicts(workspace_id, requested_by_device_id)
    }

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.inner.mark_conflict_resolved(input)
    }

    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent> {
        self.inner.create_upload_intent(request)
    }

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        self.inner.create_download_intent(request)
    }

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        self.inner.create_upload_verification_intent(request)
    }

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        self.inner.mark_object_retention_state(update)
    }

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent> {
        self.inner.create_delete_intent(request)
    }

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        self.inner.head_object_metadata(workspace_id, object_key)
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord> {
        self.inner.commit_object_manifest(commit)
    }

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>> {
        self.inner
            .get_snapshot_manifest_pointer(workspace_id, snapshot_id)
    }

    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        self.inner.create_device_request(input)
    }
}

fn mark_only_conflict_bundle_state(state_root: &std::path::Path, state: &str) {
    let conflicts_root = state_root.join("conflicts");
    let mut entries = fs::read_dir(&conflicts_root)
        .expect("conflicts root")
        .collect::<Result<Vec<_>, _>>()
        .expect("conflict entries");
    assert_eq!(entries.len(), 1, "test expects one conflict bundle");
    let manifest_path = entries.remove(0).path().join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest")).expect("json");
    manifest["state"] = serde_json::Value::String(state.to_string());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .expect("write manifest");
}
