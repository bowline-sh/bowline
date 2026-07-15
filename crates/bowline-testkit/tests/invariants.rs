use bowline_control_plane::{
    ControlPlaneTimestamp, FakeControlPlaneClient, MetadataBindingCommit, MetadataBindingInput,
    MetadataRecordKind, MetadataSidecar, ObjectControlPlaneClient, ObjectKind,
    ObjectMetadataCommit, ObjectPointer, ObjectRetentionStateUpdate, SnapshotRootCommit,
    UploadIntentRequest, WorkspaceControlPlaneClient,
};
use bowline_core::ids::{ContentId, DeviceId, ManifestId, SnapshotId, WorkspaceId};
use bowline_local::workspace::TempWorkspace;
use bowline_storage::{
    ByteStore, ByteStoreError, LocalByteStore, ObjectKey, ObjectKind as StorageObjectKind,
    RetentionState, stable_object_hash,
};
use bowline_testkit::{
    DegradedEvidence, InvariantError, RenderedStatus, SyncScenario, assert_object_before_ref,
    assert_status_not_hiding_degraded,
};

#[test]
fn object_and_local_head_invariants_pass_for_normal_sync() {
    let scenario = SyncScenario::new("invariants-pass").expect("scenario");
    scenario
        .workspace()
        .write_file("notes.txt", b"hello\n")
        .expect("fixture file");

    let _outcome = scenario.tick().expect("sync tick");

    scenario.assert_invariants().expect("invariants");
}

#[test]
fn status_invariant_rejects_hidden_degraded_evidence() {
    let evidence = [DegradedEvidence {
        code: "stat-cache-divergence".to_string(),
        message: "stat cache degraded".to_string(),
    }];
    let rendered = RenderedStatus {
        text: "healthy".to_string(),
    };

    let error = assert_status_not_hiding_degraded(&evidence, &rendered)
        .expect_err("hidden degraded evidence fails");

    assert!(matches!(
        error,
        InvariantError::HiddenDegradedEvidence { code } if code == "stat-cache-divergence"
    ));
}

#[test]
fn object_invariant_rejects_ref_without_manifest_pointer() {
    let control_plane = FakeControlPlaneClient::default();
    let workspace_id = WorkspaceId::new("ws_missing_manifest");
    let object_root = TempWorkspace::new("missing-manifest-objects").expect("object root");
    let byte_store = LocalByteStore::open_deterministic(object_root.root(), 1).expect("byte store");

    control_plane
        .create_workspace_ref(&workspace_id)
        .expect("workspace ref");
    control_plane
        .compare_and_swap_workspace_ref(
            &workspace_id,
            0,
            &SnapshotId::new("snap_missing"),
            &DeviceId::new("device_test"),
        )
        .expect("advance ref");

    let error = assert_object_before_ref(&control_plane, &byte_store, &workspace_id)
        .expect_err("missing manifest pointer fails");

    assert!(matches!(
        error,
        InvariantError::MissingObjectManifest {
            workspace_ref_version: 1,
            snapshot_id
        } if snapshot_id == "snap_missing"
    ));
}

#[test]
fn object_invariant_rejects_committed_manifest_absent_from_byte_store() {
    let control_plane = FakeControlPlaneClient::default();
    let workspace_id = WorkspaceId::new("ws_missing_manifest_bytes");
    let object_root = TempWorkspace::new("missing-manifest-bytes-objects").expect("object root");
    let byte_store = LocalByteStore::open_deterministic(object_root.root(), 1).expect("byte store");
    let manifest_pointer = object_pointer(
        ObjectKind::SnapshotManifest,
        "manifests_mf_0000000000000001",
        b"{\"manifest\":\"missing\"}",
    );

    control_plane
        .create_workspace_ref(&workspace_id)
        .expect("workspace ref");
    reserve_and_commit_pointer(
        &control_plane,
        &workspace_id,
        manifest_pointer.clone(),
        "device_test",
    );
    commit_test_snapshot_root(
        &control_plane,
        &workspace_id,
        "snap_missing_bytes",
        "manifest_missing_bytes",
        manifest_pointer,
        "device_test",
    );
    control_plane
        .compare_and_swap_workspace_ref(
            &workspace_id,
            0,
            &SnapshotId::new("snap_missing_bytes"),
            &DeviceId::new("device_test"),
        )
        .expect("advance ref");

    let error = assert_object_before_ref(&control_plane, &byte_store, &workspace_id)
        .expect_err("missing manifest object bytes fail");

    assert!(matches!(
        error,
        InvariantError::ByteStore(ByteStoreError::MissingObject { .. })
    ));
}

#[test]
fn object_invariant_rejects_committed_manifest_not_current_in_control_plane() {
    let control_plane = FakeControlPlaneClient::default();
    let workspace_id = WorkspaceId::new("ws_manifest_delete_eligible");
    let object_root = TempWorkspace::new("manifest-delete-eligible-objects").expect("object root");
    let byte_store = LocalByteStore::open_deterministic(object_root.root(), 1).expect("byte store");
    let manifest_bytes = b"{\"manifest\":\"available-but-not-current\"}";
    let manifest_pointer = object_pointer(
        ObjectKind::SnapshotManifest,
        "manifests_mf_0000000000000002",
        manifest_bytes,
    );

    control_plane
        .create_workspace_ref(&workspace_id)
        .expect("workspace ref");
    byte_store
        .put_object(
            ObjectKey::new(manifest_pointer.object_key.clone()).expect("object key"),
            StorageObjectKind::SnapshotManifest,
            manifest_bytes,
            None,
        )
        .expect("manifest bytes");
    reserve_and_commit_pointer(
        &control_plane,
        &workspace_id,
        manifest_pointer.clone(),
        "device_test",
    );
    commit_test_snapshot_root(
        &control_plane,
        &workspace_id,
        "snap_not_current",
        "manifest_not_current",
        manifest_pointer.clone(),
        "device_test",
    );
    control_plane
        .mark_object_retention_state(ObjectRetentionStateUpdate::new(
            workspace_id.as_str(),
            manifest_pointer.object_key.clone(),
            RetentionState::OrphanCandidate,
        ))
        .expect("mark object orphan candidate");
    control_plane
        .compare_and_swap_workspace_ref(
            &workspace_id,
            0,
            &SnapshotId::new("snap_not_current"),
            &DeviceId::new("device_test"),
        )
        .expect("advance ref");

    let error = assert_object_before_ref(&control_plane, &byte_store, &workspace_id)
        .expect_err("non-current control-plane metadata fails");

    assert!(matches!(
        error,
        InvariantError::UnavailableCommittedObject {
            object_key,
            retention_state: RetentionState::OrphanCandidate,
        } if object_key == manifest_pointer.object_key
    ));
}

#[test]
fn status_invariant_accepts_rendered_degraded_evidence() {
    let evidence = [DegradedEvidence {
        code: "stat-cache-divergence".to_string(),
        message: "stat cache degraded".to_string(),
    }];
    let rendered = RenderedStatus {
        text: "limited: stat-cache-divergence".to_string(),
    };

    assert!(assert_status_not_hiding_degraded(&evidence, &rendered).is_ok());
}

fn reserve_and_commit_pointer(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &WorkspaceId,
    pointer: ObjectPointer,
    device_id: &str,
) {
    control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id.as_str(), pointer.kind, pointer.byte_len)
                .with_content_id(pointer.content_id.clone())
                .with_object_key(pointer.object_key.clone()),
        )
        .expect("upload intent");
    control_plane
        .commit_uploaded_object_metadata(ObjectMetadataCommit {
            workspace_id: workspace_id.clone(),
            object: pointer,
            committed_by_device_id: DeviceId::new(device_id),
        })
        .expect("commit object metadata");
}

fn commit_test_snapshot_root(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &WorkspaceId,
    snapshot_id: &str,
    manifest_id: &str,
    manifest_pointer: ObjectPointer,
    device_id: &str,
) {
    let metadata_bytes = b"canonical namespace root";
    let metadata_digest = stable_object_hash(metadata_bytes);
    let digest_hex = metadata_digest
        .strip_prefix("b3_")
        .expect("stable object digests use the b3 prefix");
    let metadata_object_key = format!("metadata_mp_{digest_hex}");
    let namespace_root_id = format!("nsp_{digest_hex}");
    let mut metadata_pointer = object_pointer(
        ObjectKind::SnapshotMetadataPage,
        &metadata_object_key,
        metadata_bytes,
    );
    metadata_pointer.content_id = ContentId::new(namespace_root_id.clone());
    reserve_and_commit_pointer(
        control_plane,
        workspace_id,
        metadata_pointer.clone(),
        device_id,
    );
    control_plane
        .commit_metadata_bindings(MetadataBindingCommit {
            workspace_id: workspace_id.clone(),
            bindings: vec![MetadataBindingInput {
                logical_id: namespace_root_id.clone(),
                record_kind: MetadataRecordKind::NamespacePage,
                object: metadata_pointer,
                sidecar: MetadataSidecar {
                    child_logical_ids: Vec::new(),
                    direct_object_keys: Vec::new(),
                    digest: metadata_digest,
                },
            }],
            committed_by_device_id: DeviceId::new(device_id),
        })
        .expect("commit namespace root binding");
    control_plane
        .commit_snapshot_root(SnapshotRootCommit {
            workspace_id: workspace_id.clone(),
            snapshot_id: SnapshotId::new(snapshot_id),
            manifest_id: ManifestId::new(manifest_id),
            manifest_object: manifest_pointer,
            namespace_root_id,
            extra_root_logical_ids: Vec::new(),
            committed_by_device_id: DeviceId::new(device_id),
        })
        .expect("commit snapshot root");
}

fn object_pointer(kind: ObjectKind, object_key: &str, bytes: &[u8]) -> ObjectPointer {
    ObjectPointer {
        object_key: object_key.to_string(),
        content_id: ContentId::new(stable_object_hash(bytes)),
        byte_len: bytes.len() as u64,
        hash: stable_object_hash(bytes),
        key_epoch: 1,
        kind,
        created_at: ControlPlaneTimestamp { tick: 1 },
    }
}
