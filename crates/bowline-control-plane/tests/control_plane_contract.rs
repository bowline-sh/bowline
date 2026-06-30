use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_control_plane::{
    ByteRange, CompactEventKind, CompareAndSwapError, ConflictMetadataPublish,
    ConflictResolutionMark, ConflictResolutionState, ControlPlaneClient, ControlPlaneError,
    ControlPlaneTimestamp, DeleteIntentRequest, DeviceApprovalInput, DeviceRequestInput,
    DeviceRevocationInput, DownloadIntentRequest, FakeControlPlaneClient,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, LeaseCreate, LeaseExecutionState,
    LeaseOutputState, LeaseUpdate, LeaseWriteTargetMode, ObjectKind, ObjectManifestCommit,
    ObjectPointer, ObjectRetentionStateUpdate, RecoveryDeviceAuthorizationInput,
    RecoveryEnvelopeInput, RecoveryEnvelopeState, UploadIntentRequest, WorkViewCreate,
    WorkViewLifecycleState, WorkViewLifecycleUpdate, WorkViewOverlayCommit, WorkViewUpdateError,
    is_opaque_object_key,
};
use bowline_storage::RetentionState;
use p256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::Signer};
use sha2::{Digest, Sha256};

#[test]
fn fake_client_creates_workspace_ref_and_returns_it() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");

    assert_eq!(
        control_plane
            .get_workspace_ref("workspace-1")
            .expect("fake control plane reads refs"),
        Some(initial_ref)
    );
}

#[test]
fn fake_cas_advances_ref_and_appends_one_compact_event() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");
    let before_events = control_plane
        .list_events("workspace-1")
        .expect("events are readable");

    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            "workspace-1",
            initial_ref.version,
            "snapshot-1",
            "device-1",
        )
        .expect("matching CAS advances");

    let after_events = control_plane
        .list_events("workspace-1")
        .expect("events are readable");
    assert_eq!(advanced_ref.version, 1);
    assert_eq!(advanced_ref.snapshot_id, "snapshot-1");
    assert_eq!(after_events.len(), before_events.len() + 1);
    assert_eq!(
        after_events.last().expect("CAS event exists").kind,
        CompactEventKind::WorkspaceRefAdvanced
    );
}

#[test]
fn fake_cas_with_stale_base_returns_current_ref() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");

    control_plane
        .compare_and_swap_workspace_ref(
            "workspace-1",
            initial_ref.version,
            "snapshot-a",
            "device-a",
        )
        .expect("first writer wins");

    let stale = control_plane
        .compare_and_swap_workspace_ref(
            "workspace-1",
            initial_ref.version,
            "snapshot-b",
            "device-b",
        )
        .expect_err("second writer sees a typed stale-ref error");

    assert!(matches!(
        stale,
        CompareAndSwapError::StaleRef(stale)
            if stale.expected_version == initial_ref.version
                && stale.current.snapshot_id == "snapshot-a"
                && stale.current.updated_by_device_id.as_deref() == Some("device-a")
    ));
}

#[test]
fn conflict_metadata_publish_list_and_resolve_are_device_scoped() {
    let control_plane = FakeControlPlaneClient::default().with_local_device_id("device-1");
    control_plane.create_workspace("workspace-1");

    let record = control_plane
        .publish_conflict_metadata(ConflictMetadataPublish {
            workspace_id: "workspace-1".to_string(),
            conflict_id: "conflict_abc123".to_string(),
            conflict_kind: "env-key".to_string(),
            paths: vec!["apps/web/.env.local".to_string()],
            contains_secrets: true,
            base_snapshot_id: "snap_base".to_string(),
            remote_snapshot_id: "snap_remote".to_string(),
            detected_by_device_id: "device-1".to_string(),
            bundle_object: None,
        })
        .expect("trusted local device publishes conflict metadata");

    assert_eq!(record.state, "unresolved");
    assert!(record.contains_secrets);
    assert_eq!(record.paths, vec!["apps/web/.env.local".to_string()]);
    assert_eq!(
        control_plane
            .list_events("workspace-1")
            .expect("events")
            .last()
            .expect("conflict event")
            .kind,
        CompactEventKind::ConflictDetected
    );

    let listed = control_plane
        .list_workspace_conflicts("workspace-1", "device-1")
        .expect("trusted local device lists unresolved conflicts");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].conflict_id, "conflict_abc123");

    let wrong_device = control_plane
        .list_workspace_conflicts("workspace-1", "device-2")
        .expect_err("fake device scope is enforced");
    assert!(matches!(wrong_device, ControlPlaneError::Limited { .. }));

    let resolved = control_plane
        .mark_conflict_resolved(ConflictResolutionMark {
            workspace_id: "workspace-1".to_string(),
            conflict_id: "conflict_abc123".to_string(),
            resolved_by_device_id: "device-1".to_string(),
            resolution: ConflictResolutionState::Accepted,
        })
        .expect("trusted local device resolves conflict metadata");
    assert_eq!(resolved.state, "accepted");
    assert_eq!(resolved.resolved_by_device_id.as_deref(), Some("device-1"));
    let event_count_after_first_resolve = control_plane
        .list_events("workspace-1")
        .expect("events")
        .len();
    let retried = control_plane
        .mark_conflict_resolved(ConflictResolutionMark {
            workspace_id: "workspace-1".to_string(),
            conflict_id: "conflict_abc123".to_string(),
            resolved_by_device_id: "device-1".to_string(),
            resolution: ConflictResolutionState::Accepted,
        })
        .expect("terminal conflict mark is idempotent");
    assert_eq!(retried, resolved);
    assert_eq!(
        control_plane
            .list_events("workspace-1")
            .expect("events")
            .len(),
        event_count_after_first_resolve,
        "idempotent conflict resolution marks must not append duplicate events"
    );
    let conflicting_terminal_mark = control_plane
        .mark_conflict_resolved(ConflictResolutionMark {
            workspace_id: "workspace-1".to_string(),
            conflict_id: "conflict_abc123".to_string(),
            resolved_by_device_id: "device-1".to_string(),
            resolution: ConflictResolutionState::Rejected,
        })
        .expect_err("terminal conflict resolution state is immutable");
    assert!(matches!(
        conflicting_terminal_mark,
        ControlPlaneError::Conflict { .. }
    ));
    assert_eq!(
        control_plane
            .list_events("workspace-1")
            .expect("events")
            .len(),
        event_count_after_first_resolve,
        "rejected stale terminal mark must not append events"
    );
    assert_eq!(
        control_plane
            .list_workspace_conflicts("workspace-1", "device-1")
            .expect("unresolved conflicts after resolution"),
        Vec::new()
    );
    let events_after_resolve = control_plane
        .list_events("workspace-1")
        .expect("events")
        .len();
    let republished = control_plane
        .publish_conflict_metadata(ConflictMetadataPublish {
            workspace_id: "workspace-1".to_string(),
            conflict_id: "conflict_abc123".to_string(),
            conflict_kind: "env-key".to_string(),
            paths: vec!["apps/web/.env.local".to_string()],
            contains_secrets: true,
            base_snapshot_id: "snap_base".to_string(),
            remote_snapshot_id: "snap_remote".to_string(),
            detected_by_device_id: "device-1".to_string(),
            bundle_object: None,
        })
        .expect("publish retry after terminal state is idempotent");
    assert_eq!(republished.state, "accepted");
    assert_eq!(
        control_plane
            .list_events("workspace-1")
            .expect("events")
            .len(),
        events_after_resolve,
        "publish retry must not reopen terminal conflict metadata"
    );
    let recurring = control_plane
        .publish_conflict_metadata(ConflictMetadataPublish {
            workspace_id: "workspace-1".to_string(),
            conflict_id: "conflict_abc123".to_string(),
            conflict_kind: "env-key".to_string(),
            paths: vec!["apps/web/.env.local".to_string()],
            contains_secrets: true,
            base_snapshot_id: "snap_base_2".to_string(),
            remote_snapshot_id: "snap_remote_2".to_string(),
            detected_by_device_id: "device-1".to_string(),
            bundle_object: None,
        })
        .expect("new snapshot pair reopens recurring conflict metadata");
    assert_eq!(recurring.state, "unresolved");
    assert_eq!(recurring.base_snapshot_id, "snap_base_2");
    assert_eq!(recurring.remote_snapshot_id, "snap_remote_2");
    assert_eq!(
        control_plane
            .list_workspace_conflicts("workspace-1", "device-1")
            .expect("recurring unresolved conflict is visible")
            .len(),
        1
    );
    assert_eq!(
        control_plane
            .list_events("workspace-1")
            .expect("events")
            .last()
            .expect("recurring detected event")
            .kind,
        CompactEventKind::ConflictDetected
    );
}

#[test]
fn upload_and_download_intents_use_opaque_object_keys() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let pack_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SourcePack, 128)
                .with_content_id("content-1"),
        )
        .expect("upload intent");
    let upload_retry = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SourcePack, 128)
                .with_content_id("content-1"),
        )
        .expect("idempotent upload retry");

    assert_eq!(upload_retry, pack_upload);
    assert!(is_opaque_object_key(&pack_upload.object_key));
    assert!(!pack_upload.object_key.contains("Users"));
    assert!(!pack_upload.object_key.contains("src"));
    assert_eq!(pack_upload.object_kind, ObjectKind::SourcePack);

    let missing = control_plane
        .create_download_intent(DownloadIntentRequest::full(
            "workspace-1",
            pack_upload.object_key.clone(),
        ))
        .expect_err("reserved upload key is not downloadable until commit");
    assert!(matches!(missing, ControlPlaneError::ObjectMissing { .. }));

    let manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SnapshotManifest, 64)
                .with_content_id("manifest-content"),
        )
        .expect("manifest upload intent");
    let manifest_object = ObjectPointer {
        object_key: manifest_upload.object_key.clone(),
        content_id: "manifest-content".to_string(),
        byte_len: 64,
        hash: "b3_manifest".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp { tick: 10 },
    };
    let pack_object = ObjectPointer {
        object_key: pack_upload.object_key.clone(),
        content_id: "content-1".to_string(),
        byte_len: pack_upload.byte_len,
        hash: "b3_pack".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };

    control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-1".to_string(),
            snapshot_id: "snapshot-1".to_string(),
            manifest_id: "manifest-1".to_string(),
            manifest_object,
            pack_objects: vec![pack_object],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect("manifest commit publishes object pointers");

    let download = control_plane
        .create_download_intent(DownloadIntentRequest {
            workspace_id: "workspace-1".to_string(),
            object_key: pack_upload.object_key.clone(),
            range: Some(ByteRange::new(4, 16)),
        })
        .expect("download intent");

    assert_eq!(download.object_key, pack_upload.object_key);
    assert_eq!(download.range, Some(ByteRange::new(4, 16)));
    assert!(download.signed_url.url.contains("action=download"));
    assert!(!format!("{:?}", download.signed_url).contains("action=download"));
    assert!(format!("{:?}", download.signed_url).contains("<redacted>"));
}

#[test]
fn explicit_object_keys_are_scoped_by_workspace() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-a");
    control_plane.create_workspace("workspace-b");

    let shared_pack_key = "packs_pk_aaaaaaaaaaaaaaaa";
    let shared_manifest_key = "manifests_mf_bbbbbbbbbbbbbbbb";

    for workspace_id in ["workspace-a", "workspace-b"] {
        let pack_upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new(workspace_id, ObjectKind::SourcePack, 128)
                    .with_content_id(format!("content-{workspace_id}"))
                    .with_object_key(shared_pack_key),
            )
            .expect("workspace-scoped pack upload intent");
        let manifest_upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new(workspace_id, ObjectKind::SnapshotManifest, 64)
                    .with_content_id(format!("manifest-{workspace_id}"))
                    .with_object_key(shared_manifest_key),
            )
            .expect("workspace-scoped manifest upload intent");

        assert_eq!(pack_upload.object_key, shared_pack_key);
        assert_eq!(manifest_upload.object_key, shared_manifest_key);

        let pack_object = ObjectPointer {
            object_key: shared_pack_key.to_string(),
            content_id: format!("content-{workspace_id}"),
            byte_len: 128,
            hash: format!("b3_pack_{workspace_id}"),
            key_epoch: 1,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp { tick: 11 },
        };
        let manifest_object = ObjectPointer {
            object_key: shared_manifest_key.to_string(),
            content_id: format!("manifest-{workspace_id}"),
            byte_len: 64,
            hash: format!("b3_manifest_{workspace_id}"),
            key_epoch: 1,
            kind: ObjectKind::SnapshotManifest,
            created_at: ControlPlaneTimestamp { tick: 10 },
        };

        control_plane
            .commit_object_manifest(ObjectManifestCommit {
                workspace_id: workspace_id.to_string(),
                snapshot_id: format!("snapshot-{workspace_id}"),
                manifest_id: format!("manifest-{workspace_id}"),
                manifest_object,
                pack_objects: vec![pack_object],
                committed_by_device_id: "device-1".to_string(),
            })
            .expect("same logical object key commits independently per workspace");
    }

    let metadata_a = control_plane
        .head_object_metadata("workspace-a", shared_pack_key)
        .expect("workspace a metadata");
    let metadata_b = control_plane
        .head_object_metadata("workspace-b", shared_pack_key)
        .expect("workspace b metadata");

    assert_eq!(metadata_a.key.as_str(), shared_pack_key);
    assert_eq!(metadata_a.hash, "b3_pack_workspace-a");
    assert_eq!(metadata_b.key.as_str(), shared_pack_key);
    assert_eq!(metadata_b.hash, "b3_pack_workspace-b");

    assert!(matches!(
        control_plane.head_object_metadata("workspace-missing", shared_pack_key),
        Err(ControlPlaneError::WorkspaceMissing { .. })
    ));
}

#[test]
fn delete_intents_require_delete_eligible_metadata() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-delete");
    let pack_object = commit_one_pack_object(
        &control_plane,
        "workspace-delete",
        "snapshot-delete",
        "manifest-delete",
        "content-delete",
    );

    let not_eligible = control_plane
        .create_delete_intent(
            DeleteIntentRequest::new("workspace-delete", &pack_object.object_key)
                .with_object_kind(ObjectKind::SourcePack)
                .with_key_epoch(pack_object.key_epoch),
        )
        .expect_err("current objects are not delete-eligible");
    assert!(matches!(
        not_eligible,
        ControlPlaneError::Conflict {
            resource: "delete intent",
            ..
        }
    ));

    let metadata = control_plane
        .mark_object_retention_state(ObjectRetentionStateUpdate::new(
            "workspace-delete",
            &pack_object.object_key,
            RetentionState::DeleteEligible,
        ))
        .expect("mark delete eligible");
    assert_eq!(metadata.retention_state, RetentionState::DeleteEligible);

    let intent = control_plane
        .create_delete_intent(
            DeleteIntentRequest::new("workspace-delete", &pack_object.object_key)
                .with_object_kind(ObjectKind::SourcePack)
                .with_key_epoch(pack_object.key_epoch),
        )
        .expect("delete intent");
    assert_eq!(intent.object_key, pack_object.object_key);
    assert_eq!(intent.object_kind, ObjectKind::SourcePack);
    assert!(intent.signed_url.url.contains("action=delete"));
    assert!(!format!("{:?}", intent.signed_url).contains("action=delete"));
    assert!(format!("{:?}", intent.signed_url).contains("<redacted>"));
}

#[test]
fn index_and_locator_upload_intents_use_index_object_keys() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    for (kind, content_id) in [
        (ObjectKind::IndexPack, "index-content"),
        (ObjectKind::LocatorIndex, "locator-content"),
    ] {
        let upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new("workspace-1", kind, 128).with_content_id(content_id),
            )
            .expect("upload intent");

        assert_eq!(upload.object_kind, kind);
        assert!(upload.object_key.starts_with("indexes_ix_"));
        assert!(is_opaque_object_key(&upload.object_key));
    }
}

#[test]
fn object_manifest_commit_records_only_object_pointers_and_event() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SnapshotManifest, 64)
                .with_content_id("manifest-content"),
        )
        .expect("manifest upload intent");
    let pack_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SourcePack, 256)
                .with_content_id("pack-content"),
        )
        .expect("pack upload intent");

    let manifest_object = ObjectPointer {
        object_key: manifest_upload.object_key,
        content_id: "manifest-content".to_string(),
        byte_len: 64,
        hash: "b3_manifest".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp { tick: 10 },
    };
    let pack_object = ObjectPointer {
        object_key: pack_upload.object_key,
        content_id: "pack-content".to_string(),
        byte_len: 256,
        hash: "b3_pack".to_string(),
        key_epoch: 7,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };

    let record = control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-1".to_string(),
            snapshot_id: "snapshot-1".to_string(),
            manifest_id: "manifest-1".to_string(),
            manifest_object: manifest_object.clone(),
            pack_objects: vec![pack_object.clone()],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect("manifest commit");

    assert_eq!(record.manifest_object, manifest_object);
    assert_eq!(record.pack_objects, vec![pack_object.clone()]);
    assert_eq!(record.pack_objects[0].key_epoch, 7);
    let pack_metadata = control_plane
        .head_object_metadata("workspace-1", &pack_object.object_key)
        .expect("pack metadata");
    assert_eq!(pack_metadata.key_epoch, 7);
    assert!(
        control_plane
            .list_events("workspace-1")
            .expect("events")
            .iter()
            .any(
                |event| event.kind == CompactEventKind::ObjectManifestCommitted
                    && event.subject == "manifest-1"
            )
    );

    let repeated = control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-1".to_string(),
            snapshot_id: "snapshot-1".to_string(),
            manifest_id: "manifest-1".to_string(),
            manifest_object: manifest_object.clone(),
            pack_objects: vec![pack_object.clone()],
            committed_by_device_id: "device-2".to_string(),
        })
        .expect("idempotent manifest commit from another device");
    assert_eq!(repeated, record);
    assert_eq!(repeated.committed_by_device_id, "device-1");

    let remote_metadata = control_plane
        .head_object_metadata("workspace-1", &pack_object.object_key)
        .expect("committed object metadata is readable through the control plane");
    assert_eq!(remote_metadata.hash, "b3_pack");
    assert_eq!(remote_metadata.byte_len, 256);

    let mismatched_existing_object = control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-1".to_string(),
            snapshot_id: "snapshot-2".to_string(),
            manifest_id: "manifest-2".to_string(),
            manifest_object,
            pack_objects: vec![ObjectPointer {
                hash: "b3_different_pack".to_string(),
                ..pack_object
            }],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect_err("existing object metadata must match exactly");
    assert!(matches!(
        mismatched_existing_object,
        ControlPlaneError::Conflict { .. }
    ));
}

#[test]
fn snapshot_manifest_pointer_is_lookupable_by_snapshot_id() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let record = commit_snapshot_manifest(
        &control_plane,
        "workspace-1",
        "snapshot-1",
        "device-1",
        "manifest-content-1",
        "pack-content-1",
    )
    .expect("manifest commit");

    let fetched = control_plane
        .get_snapshot_manifest_pointer("workspace-1", "snapshot-1")
        .expect("snapshot lookup")
        .expect("snapshot manifest pointer exists");

    assert_eq!(fetched, record);
    assert_eq!(fetched.manifest_object.kind, ObjectKind::SnapshotManifest);
    assert_eq!(fetched.pack_objects[0].kind, ObjectKind::SourcePack);
}

#[test]
fn object_manifest_commit_rejects_different_manifest_for_same_snapshot() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    commit_snapshot_manifest(
        &control_plane,
        "workspace-1",
        "snapshot-1",
        "device-1",
        "manifest-content-1",
        "pack-content-1",
    )
    .expect("first manifest commit");

    let manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SnapshotManifest, 64)
                .with_content_id("manifest-content-2"),
        )
        .expect("manifest upload intent");
    let pack_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SourcePack, 256)
                .with_content_id("pack-content-2"),
        )
        .expect("pack upload intent");

    let error = control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-1".to_string(),
            snapshot_id: "snapshot-1".to_string(),
            manifest_id: "manifest-snapshot-1-different".to_string(),
            manifest_object: ObjectPointer {
                object_key: manifest_upload.object_key,
                content_id: "manifest-content-2".to_string(),
                byte_len: 64,
                hash: "b3_manifest-content-2".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 12 },
            },
            pack_objects: vec![ObjectPointer {
                object_key: pack_upload.object_key,
                content_id: "pack-content-2".to_string(),
                byte_len: 256,
                hash: "b3_pack-content-2".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 13 },
            }],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect_err("same snapshot cannot point at a different manifest");

    assert!(matches!(error, ControlPlaneError::Conflict { .. }));
    let fetched = control_plane
        .get_snapshot_manifest_pointer("workspace-1", "snapshot-1")
        .expect("snapshot lookup")
        .expect("snapshot manifest pointer exists");
    assert_eq!(fetched.manifest_id, "manifest-snapshot-1");
}

#[test]
fn missing_snapshot_manifest_pointer_returns_none() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    assert_eq!(
        control_plane
            .get_snapshot_manifest_pointer("workspace-1", "missing-snapshot")
            .expect("snapshot lookup"),
        None
    );
}

#[test]
fn snapshot_manifest_lookup_is_workspace_scoped() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-a");
    control_plane.create_workspace("workspace-b");

    let workspace_a = commit_snapshot_manifest(
        &control_plane,
        "workspace-a",
        "snapshot-shared",
        "device-a",
        "manifest-content-a",
        "pack-content-a",
    )
    .expect("workspace a manifest commit");
    let workspace_b = commit_snapshot_manifest(
        &control_plane,
        "workspace-b",
        "snapshot-shared",
        "device-b",
        "manifest-content-b",
        "pack-content-b",
    )
    .expect("workspace b manifest commit");

    assert_eq!(
        control_plane
            .get_snapshot_manifest_pointer("workspace-a", "snapshot-shared")
            .expect("workspace a snapshot lookup"),
        Some(workspace_a)
    );
    assert_eq!(
        control_plane
            .get_snapshot_manifest_pointer("workspace-b", "snapshot-shared")
            .expect("workspace b snapshot lookup"),
        Some(workspace_b)
    );
    assert_eq!(
        control_plane
            .get_snapshot_manifest_pointer("workspace-b", "snapshot-a-only")
            .expect("wrong workspace lookup"),
        None
    );
}

#[test]
fn trusted_workspace_rejects_untrusted_manifest_commit_and_lookup() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-trust");
    create_first_device(&control_plane, "workspace-trust", "device-1");

    let commit = commit_snapshot_manifest(
        &control_plane,
        "workspace-trust",
        "snapshot-untrusted",
        "device-2",
        "manifest-content-untrusted",
        "pack-content-untrusted",
    )
    .expect_err("untrusted device cannot commit a manifest");
    assert!(matches!(commit, ControlPlaneError::Limited { .. }));

    let untrusted_reader = control_plane.clone().with_local_device_id("device-2");
    let lookup = untrusted_reader
        .get_snapshot_manifest_pointer("workspace-trust", "snapshot-untrusted")
        .expect_err("untrusted device cannot lookup manifests");
    assert!(matches!(lookup, ControlPlaneError::Limited { .. }));
}

#[test]
fn revoked_device_cannot_commit_or_lookup_snapshot_manifest() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-revoked-object");
    create_first_device(&control_plane, "workspace-revoked-object", "device-1");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-revoked-object",
                "device-2",
                "linux",
                "age1device2",
                "fp_device_2",
                "maple-river-4821",
            )
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    let request_id = request.request_id.clone();
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request_id.clone(),
            approved_by_device_id: "device-1".to_string(),
            approved_by_device_proof: device_proof(
                "workspace-revoked-object",
                "device-1",
                "approve-device-request",
                &request_id,
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-revoked-object",
                &request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect("trusted device approves");
    control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request_id.clone(),
            device_id: "device-2".to_string(),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-revoked-object",
                &request_id,
                "device-2",
            ),
        })
        .expect("requester accepts grant");

    let record = commit_snapshot_manifest(
        &control_plane,
        "workspace-revoked-object",
        "snapshot-before-revoke",
        "device-2",
        "manifest-content-before-revoke",
        "pack-content-before-revoke",
    )
    .expect("trusted device can commit before revocation");

    control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: "workspace-revoked-object".to_string(),
            device_id: "device-2".to_string(),
            revoked_by_device_id: "device-1".to_string(),
            revoked_by_device_proof: device_proof(
                "workspace-revoked-object",
                "device-1",
                "revoke-device",
                "device-2",
            ),
            reason: "lost device".to_string(),
        })
        .expect("trusted device revokes device-2");

    let revoked_commit = commit_snapshot_manifest(
        &control_plane,
        "workspace-revoked-object",
        "snapshot-after-revoke",
        "device-2",
        "manifest-content-after-revoke",
        "pack-content-after-revoke",
    )
    .expect_err("revoked device cannot commit a manifest");
    assert!(matches!(revoked_commit, ControlPlaneError::Limited { .. }));

    let revoked_reader = control_plane.clone().with_local_device_id("device-2");
    let revoked_lookup = revoked_reader
        .get_snapshot_manifest_pointer("workspace-revoked-object", "snapshot-before-revoke")
        .expect_err("revoked device cannot lookup manifests");
    assert!(matches!(revoked_lookup, ControlPlaneError::Limited { .. }));

    let trusted_reader = control_plane.with_local_device_id("device-1");
    assert_eq!(
        trusted_reader
            .get_snapshot_manifest_pointer("workspace-revoked-object", "snapshot-before-revoke")
            .expect("trusted reader lookup"),
        Some(record)
    );
}

#[test]
fn object_manifest_commit_rejects_unreserved_or_mismatched_objects() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");
    let manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SnapshotManifest, 64)
                .with_content_id("manifest-content"),
        )
        .expect("manifest upload intent");

    let unreserved_pack = ObjectPointer {
        object_key: "packs_pk_0011223344556677".to_string(),
        content_id: "pack-content".to_string(),
        byte_len: 256,
        hash: "b3_pack".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };
    let error = control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-1".to_string(),
            snapshot_id: "snapshot-unreserved".to_string(),
            manifest_id: "manifest-unreserved".to_string(),
            manifest_object: ObjectPointer {
                object_key: manifest_upload.object_key.clone(),
                content_id: "manifest-content".to_string(),
                byte_len: 64,
                hash: "b3_manifest".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 10 },
            },
            pack_objects: vec![unreserved_pack],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect_err("unreserved pack cannot become downloadable");
    assert!(matches!(error, ControlPlaneError::ObjectMissing { .. }));

    let pack_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::SourcePack, 256)
                .with_content_id("pack-content"),
        )
        .expect("pack upload intent");
    let mismatch = control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-1".to_string(),
            snapshot_id: "snapshot-mismatch".to_string(),
            manifest_id: "manifest-mismatch".to_string(),
            manifest_object: ObjectPointer {
                object_key: manifest_upload.object_key,
                content_id: "manifest-content".to_string(),
                byte_len: 64,
                hash: "b3_manifest".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 10 },
            },
            pack_objects: vec![ObjectPointer {
                object_key: pack_upload.object_key,
                content_id: "pack-content".to_string(),
                byte_len: 128,
                hash: "b3_pack".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 11 },
            }],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect_err("reserved object metadata must match");
    assert!(matches!(mismatch, ControlPlaneError::Conflict { .. }));
}

#[test]
fn path_shaped_object_keys_are_rejected() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let error = control_plane
        .create_download_intent(DownloadIntentRequest::full(
            "workspace-1",
            "/workspace/Code/acme/src/main.rs",
        ))
        .expect_err("path-derived key is rejected before lookup");

    assert!(matches!(error, ControlPlaneError::InvalidObjectKey { .. }));
    assert!(!error.to_string().contains("src/main.rs"));
    assert!(!error.to_string().contains("/workspace/Code"));
}

#[test]
fn agent_overlay_uploads_use_opaque_keys_and_commit_to_work_view_head() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-work");
    let work_view = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-work".to_string(),
            work_view_id: "work-1".to_string(),
            project_id: "acme-web".to_string(),
            name: "try-cache".to_string(),
            visible_path: ".work/acme-web/try-cache".to_string(),
            base_snapshot_id: "empty".to_string(),
            base_workspace_version: 0,
            created_by_device_id: "device-1".to_string(),
        })
        .expect("work view create");
    assert_eq!(work_view.overlay_version, 0);
    assert_eq!(work_view.lifecycle, WorkViewLifecycleState::Active);

    let overlay_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-work", ObjectKind::AgentOverlay, 512)
                .with_content_id("overlay-content-1"),
        )
        .expect("overlay upload intent");
    assert!(overlay_upload.object_key.starts_with("packs_pk_"));
    assert!(is_opaque_object_key(&overlay_upload.object_key));
    assert!(!overlay_upload.object_key.contains("acme-web"));
    assert!(!overlay_upload.object_key.contains("try-cache"));

    let overlay_object = ObjectPointer {
        object_key: overlay_upload.object_key.clone(),
        content_id: "overlay-content-1".to_string(),
        byte_len: 512,
        hash: "b3_overlay-content-1".to_string(),
        key_epoch: 1,
        kind: ObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp { tick: 20 },
    };
    let updated = control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: "workspace-work".to_string(),
            work_view_id: "work-1".to_string(),
            expected_overlay_version: 0,
            overlay_object: overlay_object.clone(),
            committed_by_device_id: "device-1".to_string(),
        })
        .expect("overlay commit");

    assert_eq!(updated.overlay_version, 1);
    assert_eq!(updated.overlay_head, Some(overlay_object.clone()));
    assert_eq!(
        control_plane
            .head_object_metadata("workspace-work", &overlay_object.object_key)
            .expect("overlay metadata")
            .kind,
        bowline_storage::ObjectKind::AgentOverlay
    );
    assert!(
        control_plane
            .list_events("workspace-work")
            .expect("events")
            .iter()
            .any(|event| event.kind == CompactEventKind::WorkUpdated && event.subject == "work-1")
    );
}

#[test]
fn work_view_create_requires_current_or_committed_base_snapshot() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-work-base");
    let base_ref = control_plane
        .compare_and_swap_workspace_ref("workspace-work-base", 0, "snap_base", "device-1")
        .expect("base ref");
    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            "workspace-work-base",
            base_ref.version,
            "snap_next",
            "device-1",
        )
        .expect("advanced ref");

    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-work-base".to_string(),
            work_view_id: "work-current".to_string(),
            project_id: "acme-web".to_string(),
            name: "current-base".to_string(),
            visible_path: ".work/acme-web/current-base".to_string(),
            base_snapshot_id: advanced_ref.snapshot_id,
            base_workspace_version: advanced_ref.version,
            created_by_device_id: "device-1".to_string(),
        })
        .expect("current workspace ref is a valid base");

    let missing_historical = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-work-base".to_string(),
            work_view_id: "work-missing-base".to_string(),
            project_id: "acme-web".to_string(),
            name: "missing-base".to_string(),
            visible_path: ".work/acme-web/missing-base".to_string(),
            base_snapshot_id: "snap_base".to_string(),
            base_workspace_version: base_ref.version,
            created_by_device_id: "device-1".to_string(),
        })
        .expect_err("historical base needs committed manifest");
    assert!(matches!(
        missing_historical,
        ControlPlaneError::Conflict { .. }
    ));

    let wrong_manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-work-base", ObjectKind::SnapshotManifest, 64)
                .with_content_id("manifest-wrong"),
        )
        .expect("wrong manifest upload intent");
    let wrong_pack_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-work-base", ObjectKind::SourcePack, 256)
                .with_content_id("pack-wrong"),
        )
        .expect("wrong pack upload intent");
    control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: "workspace-work-base".to_string(),
            snapshot_id: "snap_real".to_string(),
            manifest_id: "snap_missing".to_string(),
            manifest_object: ObjectPointer {
                object_key: wrong_manifest_upload.object_key,
                content_id: "manifest-wrong".to_string(),
                byte_len: 64,
                hash: "b3_manifest-wrong".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 30 },
            },
            pack_objects: vec![ObjectPointer {
                object_key: wrong_pack_upload.object_key,
                content_id: "pack-wrong".to_string(),
                byte_len: 256,
                hash: "b3_pack-wrong".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 31 },
            }],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect("manifest with snapshot-looking id commits");
    let manifest_id_is_not_a_snapshot = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-work-base".to_string(),
            work_view_id: "work-manifest-id".to_string(),
            project_id: "acme-web".to_string(),
            name: "manifest-id-base".to_string(),
            visible_path: ".work/acme-web/manifest-id-base".to_string(),
            base_snapshot_id: "snap_missing".to_string(),
            base_workspace_version: 0,
            created_by_device_id: "device-1".to_string(),
        })
        .expect_err("manifest id alone is not a committed base snapshot");
    assert!(matches!(
        manifest_id_is_not_a_snapshot,
        ControlPlaneError::Conflict { .. }
    ));

    commit_snapshot_manifest(
        &control_plane,
        "workspace-work-base",
        "snap_base",
        "device-1",
        "manifest-base",
        "pack-base",
    )
    .expect("base manifest commit");

    let historical = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-work-base".to_string(),
            work_view_id: "work-historical".to_string(),
            project_id: "acme-web".to_string(),
            name: "historical-base".to_string(),
            visible_path: ".work/acme-web/historical-base".to_string(),
            base_snapshot_id: "snap_base".to_string(),
            base_workspace_version: base_ref.version,
            created_by_device_id: "device-1".to_string(),
        })
        .expect("committed historical base is valid");
    assert_eq!(historical.base_snapshot_id, "snap_base");
}

#[test]
fn stale_work_view_overlay_head_returns_current_record() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-stale-work");
    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-stale-work".to_string(),
            work_view_id: "work-stale".to_string(),
            project_id: "acme-web".to_string(),
            name: "stale-head".to_string(),
            visible_path: ".work/acme-web/stale-head".to_string(),
            base_snapshot_id: "empty".to_string(),
            base_workspace_version: 0,
            created_by_device_id: "device-1".to_string(),
        })
        .expect("work view create");
    let first = reserve_overlay_object(&control_plane, "workspace-stale-work", "overlay-first", 20);
    control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: "workspace-stale-work".to_string(),
            work_view_id: "work-stale".to_string(),
            expected_overlay_version: 0,
            overlay_object: first,
            committed_by_device_id: "device-1".to_string(),
        })
        .expect("first overlay commit");

    let stale = control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: "workspace-stale-work".to_string(),
            work_view_id: "work-stale".to_string(),
            expected_overlay_version: 0,
            overlay_object: reserve_overlay_object(
                &control_plane,
                "workspace-stale-work",
                "overlay-second",
                21,
            ),
            committed_by_device_id: "device-2".to_string(),
        })
        .expect_err("stale overlay writer gets current head");

    assert!(matches!(
        stale,
        WorkViewUpdateError::StaleOverlayHead(stale)
            if stale.expected_overlay_version == 0
                && stale.current.overlay_version == 1
                && stale.current.overlay_head.is_some()
    ));
}

#[test]
fn work_view_lifecycle_updates_emit_phase_9_events_and_filter_lists() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-lifecycle");
    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-lifecycle".to_string(),
            work_view_id: "work-life".to_string(),
            project_id: "acme-web".to_string(),
            name: "review".to_string(),
            visible_path: ".work/acme-web/review".to_string(),
            base_snapshot_id: "empty".to_string(),
            base_workspace_version: 0,
            created_by_device_id: "device-1".to_string(),
        })
        .expect("work view create");

    let review_ready = control_plane
        .update_work_view_lifecycle(WorkViewLifecycleUpdate {
            workspace_id: "workspace-lifecycle".to_string(),
            work_view_id: "work-life".to_string(),
            lifecycle: WorkViewLifecycleState::ReviewReady,
            updated_by_device_id: "device-1".to_string(),
        })
        .expect("review-ready update");
    assert_eq!(review_ready.lifecycle, WorkViewLifecycleState::ReviewReady);
    assert_eq!(
        control_plane
            .list_work_views("workspace-lifecycle", false)
            .expect("default list")
            .len(),
        1
    );

    control_plane
        .update_work_view_lifecycle(WorkViewLifecycleUpdate {
            workspace_id: "workspace-lifecycle".to_string(),
            work_view_id: "work-life".to_string(),
            lifecycle: WorkViewLifecycleState::Discarded,
            updated_by_device_id: "device-1".to_string(),
        })
        .expect("discard update");
    assert!(
        control_plane
            .list_work_views("workspace-lifecycle", false)
            .expect("default list")
            .is_empty()
    );
    assert_eq!(
        control_plane
            .list_work_views("workspace-lifecycle", true)
            .expect("all list")
            .len(),
        1
    );
    control_plane
        .restore_work_view("workspace-lifecycle", "work-life", "device-1")
        .expect("restore work view");

    let event_kinds = control_plane
        .list_events("workspace-lifecycle")
        .expect("events")
        .into_iter()
        .map(|event| event.kind)
        .collect::<Vec<_>>();
    assert!(event_kinds.contains(&CompactEventKind::WorkCreated));
    assert!(event_kinds.contains(&CompactEventKind::WorkReviewReady));
    assert!(event_kinds.contains(&CompactEventKind::WorkDiscarded));
    assert!(event_kinds.contains(&CompactEventKind::WorkRestored));
}

#[test]
fn trusted_workspace_rejects_untrusted_work_view_writes_and_cross_workspace_overlay() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-work-a");
    control_plane.create_workspace("workspace-work-b");
    create_first_device(&control_plane, "workspace-work-a", "device-1");

    let untrusted_create = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-work-a".to_string(),
            work_view_id: "work-untrusted".to_string(),
            project_id: "acme-web".to_string(),
            name: "untrusted".to_string(),
            visible_path: ".work/acme-web/untrusted".to_string(),
            base_snapshot_id: "empty".to_string(),
            base_workspace_version: 0,
            created_by_device_id: "device-2".to_string(),
        })
        .expect_err("untrusted device cannot create work");
    assert!(matches!(
        untrusted_create,
        ControlPlaneError::Limited { .. }
    ));

    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-work-a".to_string(),
            work_view_id: "work-trusted".to_string(),
            project_id: "acme-web".to_string(),
            name: "trusted".to_string(),
            visible_path: ".work/acme-web/trusted".to_string(),
            base_snapshot_id: "empty".to_string(),
            base_workspace_version: 0,
            created_by_device_id: "device-1".to_string(),
        })
        .expect("trusted device creates work");

    let untrusted_reader = control_plane.clone().with_local_device_id("device-2");
    let list_error = untrusted_reader
        .list_work_views("workspace-work-a", true)
        .expect_err("untrusted device cannot list work views");
    assert!(matches!(list_error, ControlPlaneError::Limited { .. }));

    let workspace_b_overlay =
        reserve_overlay_object(&control_plane, "workspace-work-b", "overlay-b", 22);
    let cross_workspace = control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: "workspace-work-a".to_string(),
            work_view_id: "work-trusted".to_string(),
            expected_overlay_version: 0,
            overlay_object: workspace_b_overlay,
            committed_by_device_id: "device-1".to_string(),
        })
        .expect_err("overlay object reservation belongs to another workspace");
    assert!(matches!(cross_workspace, WorkViewUpdateError::Storage(_)));
}

#[test]
fn compact_lease_create_update_and_events_omit_local_only_text() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-lease");
    let output_object =
        reserve_overlay_object(&control_plane, "workspace-lease", "lease-output", 30);

    let lease = control_plane
        .create_lease(LeaseCreate {
            workspace_id: "workspace-lease".to_string(),
            lease_id: "lease-1".to_string(),
            project_id: "project-acme".to_string(),
            device_id: "device-1".to_string(),
            write_target_mode: LeaseWriteTargetMode::WorkView,
            work_view_id: Some("work-lease-1".to_string()),
            base_snapshot_id: "empty".to_string(),
            execution_state: LeaseExecutionState::Active,
            output_state: LeaseOutputState::Empty,
            status_code: "active".to_string(),
            output_object: None,
            audit_object: None,
            expires_at: ControlPlaneTimestamp { tick: 3_600 },
        })
        .expect("compact lease create");

    assert_eq!(lease.project_id, "project-acme");
    assert_eq!(lease.write_target_mode, LeaseWriteTargetMode::WorkView);
    assert_eq!(lease.work_view_id.as_deref(), Some("work-lease-1"));
    assert_eq!(lease.execution_state.as_str(), "active");
    assert_eq!(lease.output_state.as_str(), "empty");
    assert_eq!(lease.status_code, "active");

    let updated = control_plane
        .update_lease(LeaseUpdate {
            workspace_id: "workspace-lease".to_string(),
            lease_id: "lease-1".to_string(),
            expected_version: 0,
            updated_by_device_id: "device-1".to_string(),
            execution_state: None,
            output_state: Some(LeaseOutputState::ReviewReady),
            status_code: Some("review-ready".to_string()),
            output_object: Some(output_object.clone()),
            audit_object: None,
            event_kind: None,
        })
        .expect("compact lease update");

    assert_eq!(updated.version, 1);
    assert_eq!(updated.output_state, LeaseOutputState::ReviewReady);
    assert_eq!(updated.output_object, Some(output_object));
    let events = control_plane
        .list_events("workspace-lease")
        .expect("lease events");
    assert!(events.iter().any(|event| {
        event.kind == CompactEventKind::LeaseCreated && event.subject == "lease-1"
    }));
    assert!(events.iter().any(|event| {
        event.kind == CompactEventKind::LeaseReviewReady && event.subject == "lease-1"
    }));
}

#[test]
fn compact_lease_metadata_rejects_untrusted_devices_and_remains_visible_to_trusted_devices() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-lease-trust");
    create_first_device(&control_plane, "workspace-lease-trust", "device-1");

    let untrusted = control_plane
        .create_lease(lease_create_input(
            "workspace-lease-trust",
            "lease-untrusted",
            "device-2",
        ))
        .expect_err("untrusted device cannot create lease metadata");
    assert!(matches!(untrusted, ControlPlaneError::Limited { .. }));

    let lease = control_plane
        .create_lease(lease_create_input(
            "workspace-lease-trust",
            "lease-trusted",
            "device-1",
        ))
        .expect("trusted device creates compact lease metadata");
    assert_eq!(lease.lease_id, "lease-trusted");

    let untrusted_reader = control_plane.clone().with_local_device_id("device-2");
    let list_error = untrusted_reader
        .list_leases("workspace-lease-trust")
        .expect_err("untrusted device cannot list lease metadata");
    assert!(matches!(list_error, ControlPlaneError::Limited { .. }));

    authorize_device(
        &control_plane,
        "workspace-lease-trust",
        "device-1",
        "device-2",
    );
    let trusted_reader = control_plane.with_local_device_id("device-2");
    assert_eq!(
        trusted_reader
            .list_leases("workspace-lease-trust")
            .expect("trusted second device can see compact lease metadata"),
        vec![lease]
    );
}

#[test]
fn revoked_devices_cannot_create_work_views_or_leases() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-revoked-work");
    create_first_device(&control_plane, "workspace-revoked-work", "device-1");
    authorize_device(
        &control_plane,
        "workspace-revoked-work",
        "device-1",
        "device-2",
    );
    revoke_device(
        &control_plane,
        "workspace-revoked-work",
        "device-1",
        "device-2",
    );

    let work_error = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: "workspace-revoked-work".to_string(),
            work_view_id: "work-revoked".to_string(),
            project_id: "acme-web".to_string(),
            name: "revoked".to_string(),
            visible_path: ".work/acme-web/revoked".to_string(),
            base_snapshot_id: "empty".to_string(),
            base_workspace_version: 0,
            created_by_device_id: "device-2".to_string(),
        })
        .expect_err("revoked device cannot create work views");
    assert!(matches!(work_error, ControlPlaneError::Limited { .. }));

    let lease_error = control_plane
        .create_lease(lease_create_input(
            "workspace-revoked-work",
            "lease-revoked",
            "device-2",
        ))
        .expect_err("revoked device cannot create leases");
    assert!(matches!(lease_error, ControlPlaneError::Limited { .. }));

    let revoked_reader = control_plane.with_local_device_id("device-2");
    let list_error = revoked_reader
        .list_leases("workspace-revoked-work")
        .expect_err("revoked device cannot list leases");
    assert!(matches!(list_error, ControlPlaneError::Limited { .. }));
}

#[test]
fn compact_lease_metadata_rejects_pathlike_or_uncommitted_pointer_fields() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-lease-allowlist");

    let pathlike_project = control_plane
        .create_lease(LeaseCreate {
            project_id: "Users/tristan/Code/acme".to_string(),
            ..lease_create_input("workspace-lease-allowlist", "lease-bad-project", "device-1")
        })
        .expect_err("path-like project labels stay local-only");
    assert!(matches!(
        pathlike_project,
        ControlPlaneError::Conflict { .. }
    ));

    let raw_status = control_plane
        .create_lease(LeaseCreate {
            status_code: "needs review: src/lib.rs".to_string(),
            ..lease_create_input("workspace-lease-allowlist", "lease-bad-status", "device-1")
        })
        .expect_err("raw review notes stay local-only");
    assert!(matches!(raw_status, ControlPlaneError::Conflict { .. }));

    let pathlike_pointer = control_plane
        .update_lease(LeaseUpdate {
            workspace_id: "workspace-lease-allowlist".to_string(),
            lease_id: "lease-missing".to_string(),
            expected_version: 0,
            updated_by_device_id: "device-1".to_string(),
            execution_state: None,
            output_state: Some(LeaseOutputState::Dirty),
            status_code: Some("dirty".to_string()),
            output_object: Some(ObjectPointer {
                object_key: "packs_src_auth".to_string(),
                content_id: "content".to_string(),
                byte_len: 1,
                hash: "b3_bad".to_string(),
                key_epoch: 1,
                kind: ObjectKind::AgentOverlay,
                created_at: ControlPlaneTimestamp { tick: 1 },
            }),
            audit_object: None,
            event_kind: Some(CompactEventKind::LeaseUpdated),
        })
        .expect_err("path-derived object pointers are rejected before lookup");
    assert!(matches!(
        pathlike_pointer,
        ControlPlaneError::InvalidObjectKey { .. }
    ));
}

#[test]
fn fake_device_approval_creates_encrypted_grant_and_authorized_device() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: "workspace-1".to_string(),
            device_id: "device-1".to_string(),
            device_name: "macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_1".to_string(),
            device_authorization_proof_verifier: device_verifier("device-1"),
        })
        .expect("first device trust root");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-1",
                "device-2",
                "laptop",
                "age1device2",
                "fp_device_2",
                "maple-river-4821",
            )
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");

    let approval_input = DeviceApprovalInput {
        request_id: request.request_id.clone(),
        approved_by_device_id: "device-1".to_string(),
        approved_by_device_proof: device_proof(
            "workspace-1",
            "device-1",
            "approve-device-request",
            &request.request_id,
        ),
        encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
        grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
            "workspace-1",
            &request.request_id,
            "device-2",
        ),
        key_epoch: 1,
        expires_in_ticks: 600,
    };
    let approval = control_plane
        .approve_device_request(approval_input)
        .expect("trusted device can approve");

    assert!(!approval.harness_only);
    assert_eq!(approval.device_id, "device-2");
    assert_eq!(
        approval.encrypted_grant_ciphertext,
        "age-encrypted-workspace-key"
    );

    let trust_after_approval = control_plane
        .list_device_trust("workspace-1")
        .expect("trust list");
    assert_eq!(trust_after_approval.authorized_devices.len(), 1);
    assert_eq!(trust_after_approval.pending_requests.len(), 1);
    assert_eq!(
        trust_after_approval.pending_requests[0].state,
        bowline_control_plane::DeviceRequestState::Approved
    );

    let fetched = control_plane
        .get_encrypted_device_grant(&request.request_id, "device-2")
        .expect("grant lookup")
        .expect("approved grant");
    assert_eq!(fetched.grant_id, approval.grant_id);

    let accepted = control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: "device-2".to_string(),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-1",
                &request.request_id,
                "device-2",
            ),
        })
        .expect("requester accepts grant");
    assert!(accepted.accepted_at.is_some());

    let trust_after_acceptance = control_plane
        .list_device_trust("workspace-1")
        .expect("trust list");
    assert_eq!(trust_after_acceptance.authorized_devices.len(), 2);
    assert!(trust_after_acceptance.pending_requests.is_empty());
}

#[test]
fn authorized_device_id_cannot_create_pending_request_with_new_key() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-duplicate-device");
    create_first_device(&control_plane, "workspace-duplicate-device", "device-1");

    let error = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-duplicate-device",
                "device-1",
                "attacker",
                "age1attacker",
                "fp_attacker",
                "maple-river-4821",
            )
            .with_device_proof_verifier("attacker"),
        )
        .expect_err("authorized device id cannot request trust again");

    assert!(matches!(error, ControlPlaneError::Conflict { .. }));
}

#[test]
fn spoofed_approver_device_id_cannot_approve_without_device_proof() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-spoofed-approver");
    create_first_device(&control_plane, "workspace-spoofed-approver", "device-1");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-spoofed-approver",
                "device-2",
                "linux",
                "age1device2",
                "fp_device_2",
                "maple-river-4821",
            )
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");

    let error = control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: "device-1".to_string(),
            approved_by_device_proof: device_proof(
                "workspace-spoofed-approver",
                "device-2",
                "approve-device-request",
                &request.request_id,
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-spoofed-approver",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect_err("device id alone is not enough to approve a request");

    assert!(matches!(error, ControlPlaneError::Limited { .. }));

    let public_verifier = control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: "device-1".to_string(),
            approved_by_device_proof: device_verifier("device-1"),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-spoofed-approver",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect_err("stored public verifier is not a bearer proof");
    assert!(matches!(public_verifier, ControlPlaneError::Limited { .. }));
}

#[test]
fn accepted_grant_cannot_reauthorize_a_revoked_device() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-revoked-grant");
    create_first_device(&control_plane, "workspace-revoked-grant", "device-1");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-revoked-grant",
                "device-2",
                "linux",
                "age1device2",
                "fp_device_2",
                "maple-river-4821",
            )
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: "device-1".to_string(),
            approved_by_device_proof: device_proof(
                "workspace-revoked-grant",
                "device-1",
                "approve-device-request",
                &request.request_id,
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-revoked-grant",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect("trusted device approves");
    control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: "device-2".to_string(),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-revoked-grant",
                &request.request_id,
                "device-2",
            ),
        })
        .expect("requester accepts grant");
    control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: "workspace-revoked-grant".to_string(),
            device_id: "device-2".to_string(),
            revoked_by_device_id: "device-1".to_string(),
            revoked_by_device_proof: device_proof(
                "workspace-revoked-grant",
                "device-1",
                "revoke-device",
                "device-2",
            ),
            reason: "lost device".to_string(),
        })
        .expect("trusted device revokes the accepted device");

    let error = control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: "device-2".to_string(),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-revoked-grant",
                &request.request_id,
                "device-2",
            ),
        })
        .expect_err("accepted grant cannot reauthorize a revoked device");
    assert!(matches!(error, ControlPlaneError::Limited { .. }));

    let trust = control_plane
        .list_device_trust("workspace-revoked-grant")
        .expect("trust list");
    assert!(
        !trust
            .authorized_devices
            .iter()
            .any(|device| device.device_id == "device-2")
    );
    assert!(
        trust
            .revoked_devices
            .iter()
            .any(|device| device.device_id == "device-2")
    );
}

#[test]
fn revoking_last_trusted_device_requires_recovery_or_another_device() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-last-device");
    create_first_device(&control_plane, "workspace-last-device", "device-1");

    let blocked = control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: "workspace-last-device".to_string(),
            device_id: "device-1".to_string(),
            revoked_by_device_id: "device-1".to_string(),
            revoked_by_device_proof: device_proof(
                "workspace-last-device",
                "device-1",
                "revoke-device",
                "device-1",
            ),
            reason: "self revoke without recovery".to_string(),
        })
        .expect_err("last trust path cannot be removed");
    assert!(matches!(blocked, ControlPlaneError::Limited { .. }));

    control_plane
        .create_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-last-device".to_string(),
            envelope_id: "rk_last_device".to_string(),
            created_by_device_id: "device-1".to_string(),
            created_by_device_proof: device_proof(
                "workspace-last-device",
                "device-1",
                "create-recovery-envelope",
                "rk_last_device",
            ),
            ciphertext: "encrypted-workspace-key".to_string(),
            fingerprint: "rk_last_device".to_string(),
            recovery_proof_verifier: recovery_proof_verifier(
                "workspace-last-device",
                "rk_last_device",
                "last device recovery words",
            ),
        })
        .expect("trusted device creates recovery envelope");
    control_plane
        .verify_recovery_envelope(
            "workspace-last-device",
            "rk_last_device",
            "device-1",
            &device_proof(
                "workspace-last-device",
                "device-1",
                "verify-recovery-envelope",
                "rk_last_device",
            ),
            &recovery_proof(
                "workspace-last-device",
                "rk_last_device",
                "last device recovery words",
            ),
        )
        .expect("trusted device verifies recovery envelope");

    let revoked = control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: "workspace-last-device".to_string(),
            device_id: "device-1".to_string(),
            revoked_by_device_id: "device-1".to_string(),
            revoked_by_device_proof: device_proof(
                "workspace-last-device",
                "device-1",
                "revoke-device",
                "device-1",
            ),
            reason: "recovery key exists".to_string(),
        })
        .expect("active recovery key preserves a trust path");
    assert_eq!(revoked.device_id, "device-1");

    let recreate = control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: "workspace-last-device".to_string(),
            device_id: "device-2".to_string(),
            device_name: "new macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_2".to_string(),
            device_authorization_proof_verifier: device_verifier("device-2"),
        })
        .expect_err("trust history must use recovery instead of first-device init");
    assert!(matches!(recreate, ControlPlaneError::Conflict { .. }));
}

#[test]
fn expired_device_request_cannot_be_approved() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-expired-request");
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: "workspace-expired-request".to_string(),
            device_id: "device-1".to_string(),
            device_name: "macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_1".to_string(),
            device_authorization_proof_verifier: device_verifier("device-1"),
        })
        .expect("first device trust root");
    let mut request_input = DeviceRequestInput::new(
        "workspace-expired-request",
        "device-2",
        "laptop",
        "age1device2",
        "fp_device_2",
        "maple-river-4821",
    );
    request_input.device_authorization_proof_verifier = device_verifier("device-2");
    request_input.expires_in_ticks = 1;
    let request = control_plane
        .create_device_request(request_input)
        .expect("device request");

    let error = control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: "device-1".to_string(),
            approved_by_device_proof: device_proof(
                "workspace-expired-request",
                "device-1",
                "approve-device-request",
                &request.request_id,
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-expired-request",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect_err("expired request is rejected");

    assert!(matches!(error, ControlPlaneError::Conflict { .. }));
}

#[test]
fn expired_device_grant_cannot_be_accepted() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-expired-grant");
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: "workspace-expired-grant".to_string(),
            device_id: "device-1".to_string(),
            device_name: "macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_1".to_string(),
            device_authorization_proof_verifier: device_verifier("device-1"),
        })
        .expect("first device trust root");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-expired-grant",
                "device-2",
                "laptop",
                "age1device2",
                "fp_device_2",
                "maple-river-4821",
            )
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: "device-1".to_string(),
            approved_by_device_proof: device_proof(
                "workspace-expired-grant",
                "device-1",
                "approve-device-request",
                &request.request_id,
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-expired-grant",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 1,
        })
        .expect("trusted device approves");

    let fetch_error = control_plane
        .get_encrypted_device_grant(&request.request_id, "device-2")
        .expect_err("expired grant ciphertext is not returned");
    assert!(matches!(fetch_error, ControlPlaneError::Limited { .. }));

    let error = control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: "device-2".to_string(),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-expired-grant",
                &request.request_id,
                "device-2",
            ),
        })
        .expect_err("expired grant is rejected");

    assert!(matches!(error, ControlPlaneError::Limited { .. }));
}

#[test]
fn recovery_authorization_requires_private_proof_not_public_fingerprint() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-recovery-proof");
    create_first_device(&control_plane, "workspace-recovery-proof", "device-1");
    let recovery_proof = recovery_proof(
        "workspace-recovery-proof",
        "rk_public",
        "private recovery words",
    );
    let recovery_proof_verifier = recovery_proof_verifier(
        "workspace-recovery-proof",
        "rk_public",
        "private recovery words",
    );
    control_plane
        .create_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-recovery-proof".to_string(),
            envelope_id: "rk_public".to_string(),
            created_by_device_id: "device-1".to_string(),
            created_by_device_proof: device_proof(
                "workspace-recovery-proof",
                "device-1",
                "create-recovery-envelope",
                "rk_public",
            ),
            ciphertext: "encrypted-workspace-key".to_string(),
            fingerprint: "rk_public".to_string(),
            recovery_proof_verifier: recovery_proof_verifier.clone(),
        })
        .expect("trusted device creates recovery envelope");
    let invalid_verify = control_plane
        .verify_recovery_envelope(
            "workspace-recovery-proof",
            "rk_public",
            "device-1",
            &device_proof(
                "workspace-recovery-proof",
                "device-1",
                "verify-recovery-envelope",
                "rk_public",
            ),
            "rkp_wrong",
        )
        .expect_err("trusted device proof alone cannot verify recovery");
    assert!(matches!(invalid_verify, ControlPlaneError::Conflict { .. }));
    let envelope = control_plane
        .list_recovery_envelopes("workspace-recovery-proof")
        .expect("recovery envelopes")
        .into_iter()
        .find(|envelope| envelope.envelope_id == "rk_public")
        .expect("created envelope");
    assert_eq!(envelope.state, RecoveryEnvelopeState::GeneratedUnverified);

    control_plane
        .verify_recovery_envelope(
            "workspace-recovery-proof",
            "rk_public",
            "device-1",
            &device_proof(
                "workspace-recovery-proof",
                "device-1",
                "verify-recovery-envelope",
                "rk_public",
            ),
            &recovery_proof,
        )
        .expect("trusted device verifies envelope");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-recovery-proof",
                "device-2",
                "linux",
                "age1device2",
                "fp_device_2",
                "maple-river-4821",
            )
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");

    let public_fingerprint = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: "workspace-recovery-proof".to_string(),
            envelope_id: "rk_public".to_string(),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-proof",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof: "rk_public".to_string(),
            expires_in_ticks: 600,
        })
        .expect_err("public fingerprint is not a recovery proof");
    assert!(matches!(
        public_fingerprint,
        ControlPlaneError::Conflict { .. }
    ));

    let stored_verifier = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: "workspace-recovery-proof".to_string(),
            envelope_id: "rk_public".to_string(),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-proof",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof: recovery_proof_verifier,
            expires_in_ticks: 600,
        })
        .expect_err("stored recovery verifier is not a bearer proof");
    assert!(matches!(
        stored_verifier,
        ControlPlaneError::Conflict { .. }
    ));

    let recovered = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: "workspace-recovery-proof".to_string(),
            envelope_id: "rk_public".to_string(),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-proof",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof,
            expires_in_ticks: 600,
        })
        .expect("private recovery proof authorizes the pending device");
    assert_eq!(recovered.approved_by_device_id, "recovery:rk_public");
}

#[test]
fn revoked_recovery_envelope_cannot_be_reactivated_or_used() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-recovery-revoked");
    create_first_device(&control_plane, "workspace-recovery-revoked", "device-1");
    let recovery_proof = recovery_proof(
        "workspace-recovery-revoked",
        "rk_revoked",
        "revoked recovery words",
    );
    control_plane
        .create_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-recovery-revoked".to_string(),
            envelope_id: "rk_revoked".to_string(),
            created_by_device_id: "device-1".to_string(),
            created_by_device_proof: device_proof(
                "workspace-recovery-revoked",
                "device-1",
                "create-recovery-envelope",
                "rk_revoked",
            ),
            ciphertext: "encrypted-workspace-key".to_string(),
            fingerprint: "rk_revoked".to_string(),
            recovery_proof_verifier: recovery_proof_verifier(
                "workspace-recovery-revoked",
                "rk_revoked",
                "revoked recovery words",
            ),
        })
        .expect("trusted device creates recovery envelope");
    control_plane
        .revoke_recovery_envelope(
            "workspace-recovery-revoked",
            "rk_revoked",
            "device-1",
            &device_proof(
                "workspace-recovery-revoked",
                "device-1",
                "revoke-recovery-envelope",
                "rk_revoked",
            ),
        )
        .expect("trusted device revokes recovery envelope");

    let verify = control_plane
        .verify_recovery_envelope(
            "workspace-recovery-revoked",
            "rk_revoked",
            "device-1",
            &device_proof(
                "workspace-recovery-revoked",
                "device-1",
                "verify-recovery-envelope",
                "rk_revoked",
            ),
            &recovery_proof,
        )
        .expect_err("revoked envelopes cannot be reactivated");
    assert!(matches!(verify, ControlPlaneError::Limited { .. }));
    let envelope = control_plane
        .list_recovery_envelopes("workspace-recovery-revoked")
        .expect("envelopes")
        .into_iter()
        .find(|envelope| envelope.envelope_id == "rk_revoked")
        .expect("revoked envelope remains listed");
    assert_eq!(envelope.state, RecoveryEnvelopeState::Revoked);

    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                "workspace-recovery-revoked",
                "device-2",
                "linux",
                "age1device2",
                "fp_device_2",
                "maple-river-4821",
            )
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    let authorize = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: "workspace-recovery-revoked".to_string(),
            envelope_id: "rk_revoked".to_string(),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-revoked",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof,
            expires_in_ticks: 600,
        })
        .expect_err("revoked envelopes cannot authorize devices");
    assert!(matches!(authorize, ControlPlaneError::Limited { .. }));
}

#[test]
fn recovery_rotation_uses_rotate_proof_and_does_not_corrupt_on_conflict() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-recovery-rotate");
    create_first_device(&control_plane, "workspace-recovery-rotate", "device-1");
    control_plane
        .create_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-recovery-rotate".to_string(),
            envelope_id: "rk_current".to_string(),
            created_by_device_id: "device-1".to_string(),
            created_by_device_proof: device_proof(
                "workspace-recovery-rotate",
                "device-1",
                "create-recovery-envelope",
                "rk_current",
            ),
            ciphertext: "current-ciphertext".to_string(),
            fingerprint: "rk_current".to_string(),
            recovery_proof_verifier: recovery_proof_verifier(
                "workspace-recovery-rotate",
                "rk_current",
                "current recovery words",
            ),
        })
        .expect("trusted device creates current recovery envelope");
    control_plane
        .verify_recovery_envelope(
            "workspace-recovery-rotate",
            "rk_current",
            "device-1",
            &device_proof(
                "workspace-recovery-rotate",
                "device-1",
                "verify-recovery-envelope",
                "rk_current",
            ),
            &recovery_proof(
                "workspace-recovery-rotate",
                "rk_current",
                "current recovery words",
            ),
        )
        .expect("current recovery envelope is active");

    let conflict = control_plane
        .rotate_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-recovery-rotate".to_string(),
            envelope_id: "rk_current".to_string(),
            created_by_device_id: "device-1".to_string(),
            created_by_device_proof: device_proof(
                "workspace-recovery-rotate",
                "device-1",
                "rotate-recovery-envelope",
                "rk_current",
            ),
            ciphertext: "different-ciphertext".to_string(),
            fingerprint: "different-fingerprint".to_string(),
            recovery_proof_verifier: recovery_proof_verifier(
                "workspace-recovery-rotate",
                "rk_current",
                "different recovery words",
            ),
        })
        .expect_err("conflicting rotation does not mutate existing envelopes first");
    assert!(matches!(conflict, ControlPlaneError::Conflict { .. }));
    let current_after_conflict = control_plane
        .list_recovery_envelopes("workspace-recovery-rotate")
        .expect("envelopes")
        .into_iter()
        .find(|envelope| envelope.envelope_id == "rk_current")
        .expect("current envelope remains");
    assert_eq!(current_after_conflict.state, RecoveryEnvelopeState::Active);

    let rotated = control_plane
        .rotate_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-recovery-rotate".to_string(),
            envelope_id: "rk_next".to_string(),
            created_by_device_id: "device-1".to_string(),
            created_by_device_proof: device_proof(
                "workspace-recovery-rotate",
                "device-1",
                "rotate-recovery-envelope",
                "rk_next",
            ),
            ciphertext: "next-ciphertext".to_string(),
            fingerprint: "rk_next".to_string(),
            recovery_proof_verifier: recovery_proof_verifier(
                "workspace-recovery-rotate",
                "rk_next",
                "next recovery words",
            ),
        })
        .expect("rotate proof creates next recovery envelope");
    assert_eq!(rotated.state, RecoveryEnvelopeState::GeneratedUnverified);

    let envelopes = control_plane
        .list_recovery_envelopes("workspace-recovery-rotate")
        .expect("envelopes after rotation");
    assert_eq!(
        envelopes
            .iter()
            .find(|envelope| envelope.envelope_id == "rk_current")
            .expect("current envelope")
            .state,
        RecoveryEnvelopeState::Rotated
    );
    assert!(
        envelopes
            .iter()
            .any(|envelope| envelope.envelope_id == "rk_next")
    );
}

#[test]
fn recovery_envelope_idempotency_is_scoped_to_workspace() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-a");
    control_plane.create_workspace("workspace-b");
    create_first_device(&control_plane, "workspace-a", "device-a");
    create_first_device(&control_plane, "workspace-b", "device-b");

    control_plane
        .create_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-a".to_string(),
            envelope_id: "rk_same".to_string(),
            created_by_device_id: "device-a".to_string(),
            created_by_device_proof: device_proof(
                "workspace-a",
                "device-a",
                "create-recovery-envelope",
                "rk_same",
            ),
            ciphertext: "ciphertext-a".to_string(),
            fingerprint: "fingerprint-a".to_string(),
            recovery_proof_verifier: "proof-a".to_string(),
        })
        .expect("workspace a creates envelope");

    let workspace_b = control_plane
        .create_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-b".to_string(),
            envelope_id: "rk_same".to_string(),
            created_by_device_id: "device-b".to_string(),
            created_by_device_proof: device_proof(
                "workspace-b",
                "device-b",
                "create-recovery-envelope",
                "rk_same",
            ),
            ciphertext: "ciphertext-b".to_string(),
            fingerprint: "fingerprint-b".to_string(),
            recovery_proof_verifier: "proof-b".to_string(),
        })
        .expect("same envelope id in another workspace does not return workspace a metadata");
    assert_eq!(workspace_b.workspace_id, "workspace-b");
    assert_eq!(workspace_b.ciphertext, "ciphertext-b");

    let conflicting_retry = control_plane
        .create_recovery_envelope(RecoveryEnvelopeInput {
            workspace_id: "workspace-a".to_string(),
            envelope_id: "rk_same".to_string(),
            created_by_device_id: "device-a".to_string(),
            created_by_device_proof: device_proof(
                "workspace-a",
                "device-a",
                "create-recovery-envelope",
                "rk_same",
            ),
            ciphertext: "different-ciphertext".to_string(),
            fingerprint: "fingerprint-a".to_string(),
            recovery_proof_verifier: "proof-a".to_string(),
        })
        .expect_err("same workspace idempotency still rejects different metadata");
    assert!(matches!(
        conflicting_retry,
        ControlPlaneError::Conflict { .. }
    ));
}

fn commit_snapshot_manifest(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    snapshot_id: &str,
    device_id: &str,
    manifest_content_id: &str,
    pack_content_id: &str,
) -> Result<bowline_control_plane::ObjectManifestRecord, ControlPlaneError> {
    let manifest_upload = control_plane.create_upload_intent(
        UploadIntentRequest::new(workspace_id, ObjectKind::SnapshotManifest, 64)
            .with_content_id(manifest_content_id),
    )?;
    let pack_upload = control_plane.create_upload_intent(
        UploadIntentRequest::new(workspace_id, ObjectKind::SourcePack, 256)
            .with_content_id(pack_content_id),
    )?;

    control_plane.commit_object_manifest(ObjectManifestCommit {
        workspace_id: workspace_id.to_string(),
        snapshot_id: snapshot_id.to_string(),
        manifest_id: format!("manifest-{snapshot_id}"),
        manifest_object: ObjectPointer {
            object_key: manifest_upload.object_key,
            content_id: manifest_content_id.to_string(),
            byte_len: 64,
            hash: format!("b3_{manifest_content_id}"),
            key_epoch: 1,
            kind: ObjectKind::SnapshotManifest,
            created_at: ControlPlaneTimestamp { tick: 10 },
        },
        pack_objects: vec![ObjectPointer {
            object_key: pack_upload.object_key,
            content_id: pack_content_id.to_string(),
            byte_len: 256,
            hash: format!("b3_{pack_content_id}"),
            key_epoch: 1,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp { tick: 11 },
        }],
        committed_by_device_id: device_id.to_string(),
    })
}

fn reserve_overlay_object(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    content_id: &str,
    created_at_tick: u64,
) -> ObjectPointer {
    let upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ObjectKind::AgentOverlay, 512)
                .with_content_id(content_id),
        )
        .expect("overlay upload intent");
    ObjectPointer {
        object_key: upload.object_key,
        content_id: content_id.to_string(),
        byte_len: 512,
        hash: format!("b3_{content_id}"),
        key_epoch: 1,
        kind: ObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp {
            tick: created_at_tick,
        },
    }
}

trait DeviceRequestInputTestExt {
    fn with_device_proof_verifier(self, device_id: &str) -> Self;
}

impl DeviceRequestInputTestExt for DeviceRequestInput {
    fn with_device_proof_verifier(mut self, device_id: &str) -> Self {
        self.device_authorization_proof_verifier = device_verifier(device_id);
        self
    }
}

fn create_first_device(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    device_id: &str,
) {
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: workspace_id.to_string(),
            device_id: device_id.to_string(),
            device_name: format!("{device_id}-name"),
            platform: "macos".to_string(),
            device_fingerprint: format!("fp_{device_id}"),
            device_authorization_proof_verifier: device_verifier(device_id),
        })
        .expect("first device trust root");
}

fn authorize_device(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    approver_device_id: &str,
    device_id: &str,
) {
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(
                workspace_id,
                device_id,
                format!("{device_id}-name"),
                format!("age1{device_id}"),
                format!("fp_{device_id}"),
                "maple-river-4821",
            )
            .with_device_proof_verifier(device_id),
        )
        .expect("device request");
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: approver_device_id.to_string(),
            approved_by_device_proof: device_proof(
                workspace_id,
                approver_device_id,
                "approve-device-request",
                &request.request_id,
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                workspace_id,
                &request.request_id,
                device_id,
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect("trusted device approves");
    let request_id = request.request_id.clone();
    control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request_id.clone(),
            device_id: device_id.to_string(),
            grant_acceptance_proof: grant_acceptance_proof(workspace_id, &request_id, device_id),
        })
        .expect("requester accepts grant");
}

fn revoke_device(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    revoker_device_id: &str,
    revoked_device_id: &str,
) {
    control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: workspace_id.to_string(),
            device_id: revoked_device_id.to_string(),
            revoked_by_device_id: revoker_device_id.to_string(),
            revoked_by_device_proof: device_proof(
                workspace_id,
                revoker_device_id,
                "revoke-device",
                revoked_device_id,
            ),
            reason: "test revocation".to_string(),
        })
        .expect("trusted device revokes");
}

fn commit_one_pack_object(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    snapshot_id: &str,
    manifest_id: &str,
    content_id: &str,
) -> ObjectPointer {
    let manifest_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ObjectKind::SnapshotManifest, 64)
                .with_content_id(format!("manifest-{content_id}")),
        )
        .expect("manifest upload intent");
    let pack_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(workspace_id, ObjectKind::SourcePack, 128)
                .with_content_id(content_id),
        )
        .expect("pack upload intent");

    let manifest_object = ObjectPointer {
        object_key: manifest_upload.object_key,
        content_id: format!("manifest-{content_id}"),
        byte_len: 64,
        hash: format!("b3_manifest_{content_id}"),
        key_epoch: 1,
        kind: ObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp { tick: 10 },
    };
    let pack_object = ObjectPointer {
        object_key: pack_upload.object_key,
        content_id: content_id.to_string(),
        byte_len: 128,
        hash: format!("b3_pack_{content_id}"),
        key_epoch: 1,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };

    control_plane
        .commit_object_manifest(ObjectManifestCommit {
            workspace_id: workspace_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
            manifest_id: manifest_id.to_string(),
            manifest_object,
            pack_objects: vec![pack_object.clone()],
            committed_by_device_id: "device-1".to_string(),
        })
        .expect("manifest commit");

    pack_object
}

fn lease_create_input(workspace_id: &str, lease_id: &str, device_id: &str) -> LeaseCreate {
    LeaseCreate {
        workspace_id: workspace_id.to_string(),
        lease_id: lease_id.to_string(),
        project_id: "project-acme".to_string(),
        device_id: device_id.to_string(),
        write_target_mode: LeaseWriteTargetMode::Direct,
        work_view_id: None,
        base_snapshot_id: "empty".to_string(),
        execution_state: LeaseExecutionState::Active,
        output_state: LeaseOutputState::Empty,
        status_code: "active".to_string(),
        output_object: None,
        audit_object: None,
        expires_at: ControlPlaneTimestamp { tick: 3_600 },
    }
}

fn device_verifier(device_id: &str) -> String {
    let signing_key = test_device_signing_key(device_id);
    let verifying_key = VerifyingKey::from(&signing_key);
    let public_key = verifying_key.to_encoded_point(false);
    format!("dapv_p256_v1_{}", BASE64_URL.encode(public_key.as_bytes()))
}

fn device_proof(workspace_id: &str, device_id: &str, action: &str, subject: &str) -> String {
    let signing_key = test_device_signing_key(device_id);
    let signature: Signature = signing_key.sign(&device_authorization_message(&[
        "bowline device authorization proof v2",
        workspace_id,
        device_id,
        action,
        subject,
    ]));
    format!("dapp_p256_v1_{}", BASE64_URL.encode(signature.to_bytes()))
}

fn grant_acceptance_proof(workspace_id: &str, request_id: &str, device_id: &str) -> String {
    let hash = sha256_proof_fields(&[
        "bowline test grant acceptance proof v1",
        workspace_id,
        request_id,
        device_id,
    ]);
    format!("gap_{}", &hash[..32])
}

fn grant_acceptance_proof_verifier(
    workspace_id: &str,
    request_id: &str,
    device_id: &str,
) -> String {
    let proof = grant_acceptance_proof(workspace_id, request_id, device_id);
    let hash = sha256_proof_fields(&["bowline grant acceptance proof verifier v1", &proof]);
    format!("gapv_{}", &hash[..32])
}

fn recovery_proof(workspace_id: &str, envelope_id: &str, words: &str) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof v2",
        workspace_id,
        envelope_id,
        words,
    ]);
    format!("rkp_{}", &hash[..32])
}

fn test_device_signing_key(device_id: &str) -> SigningKey {
    for counter in 0_u8..=u8::MAX {
        let digest = Sha256::digest(device_authorization_message(&[
            "bowline test device signing key v1",
            device_id,
            &counter.to_string(),
        ]));
        if let Ok(signing_key) = SigningKey::from_slice(&digest) {
            return signing_key;
        }
    }
    unreachable!("test P-256 signing key derivation failed")
}

fn device_authorization_message(fields: &[&str]) -> Vec<u8> {
    let mut message = Vec::new();
    for field in fields {
        message.extend_from_slice(&(field.len() as u64).to_le_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    message
}

fn recovery_proof_verifier(workspace_id: &str, envelope_id: &str, words: &str) -> String {
    recovery_proof_verifier_from_proof(
        &recovery_proof(workspace_id, envelope_id, words),
        workspace_id,
        envelope_id,
    )
}

fn recovery_proof_verifier_from_proof(
    proof: &str,
    workspace_id: &str,
    envelope_id: &str,
) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof verifier v2",
        workspace_id,
        envelope_id,
        proof,
    ]);
    format!("rkpv_{}", &hash[..32])
}

fn sha256_proof_fields(fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}
