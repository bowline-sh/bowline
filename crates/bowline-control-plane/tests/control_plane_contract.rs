use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_control_plane::{
    ByteRange, Capability, CompactEventKind, CompareAndSwapError, ConflictOccurrenceReconcile,
    ConflictOccurrenceState, ConflictReconcileOutcome, ControlPlaneClient, ControlPlaneError,
    ControlPlaneTimestamp, DeviceApprovalInput, DeviceControlPlaneClient, DeviceRequestInput,
    DeviceRequestInputDraft, DeviceRevocationInput, DownloadIntentRequest, FakeControlPlaneClient,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, LeaseControlPlaneClient, LeaseCreate,
    LeaseSessionState, LeaseUpdate, LeaseWriteTargetMode, ObjectControlPlaneClient, ObjectKind,
    ObjectPointer, ObjectRetentionStateUpdate, RecoveryControlPlaneClient,
    RecoveryDeviceAuthorizationInput, RecoveryEnvelopeInput, RecoveryEnvelopeState, RejectionCode,
    SnapshotRootCommit, UploadIntentRequest, WorkViewControlPlaneClient, WorkViewCreate,
    WorkViewLifecycleState, WorkViewLifecycleUpdate, WorkViewOverlayCommit, WorkViewUpdateError,
    WorkspaceControlPlaneClient, device_request_proof_subject, device_revocation_proof_subject,
    is_opaque_object_key, recovery_envelope_payload_proof_subject, recovery_envelope_proof_subject,
};
use bowline_core::ids::*;
use bowline_storage::RetentionState;
use p256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::Signer};
use sha2::{Digest, Sha256};

#[path = "support/snapshot_graph.rs"]
mod snapshot_graph_support;
use snapshot_graph_support::{SnapshotGraphCommit, SnapshotGraphRecord, SnapshotGraphTestApi};

fn assert_device_not_trusted(error: ControlPlaneError) {
    assert!(matches!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::DeviceNotTrusted,
            ..
        }
    ));
}

fn assert_domain_contract<T: ControlPlaneClient>() {}

#[test]
fn fake_client_implements_the_canonical_domain_contract() {
    assert_domain_contract::<FakeControlPlaneClient>();
}

#[cfg(feature = "hosted-convex")]
#[test]
fn hosted_client_implements_the_same_canonical_domain_contract() {
    assert_domain_contract::<bowline_control_plane::HostedControlPlaneClient>();
}

#[test]
fn fake_client_creates_workspace_ref_and_returns_it() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");

    assert_eq!(
        control_plane
            .get_workspace_ref(&WorkspaceId::new("workspace-1"))
            .expect("fake control plane reads refs"),
        Some(initial_ref)
    );
}

#[test]
fn fake_cas_advances_ref_and_appends_one_compact_event() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");
    let before_events = control_plane
        .list_events(&WorkspaceId::new("workspace-1"))
        .expect("events are readable");

    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-1"),
            &DeviceId::new("device-1"),
        )
        .expect("matching CAS advances");

    let after_events = control_plane
        .list_events(&WorkspaceId::new("workspace-1"))
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
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-a"),
            &DeviceId::new("device-a"),
        )
        .expect("first writer wins");

    let stale = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-b"),
            &DeviceId::new("device-b"),
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
fn fake_project_scoped_cas_records_project_id_in_ref_history() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");

    control_plane
        .compare_and_swap_workspace_ref_for_project(
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-project-a"),
            &DeviceId::new("device-a"),
            Some(&ProjectId::new("project-a")),
        )
        .expect("scoped CAS advances");

    let history = control_plane
        .list_workspace_ref_history(&WorkspaceId::new("workspace-1"), 10)
        .expect("history is readable");

    assert_eq!(history.len(), 1);
    assert_eq!(history[0].project_id.as_deref(), Some("project-a"));
}

#[test]
fn fake_client_reports_only_typed_supported_capabilities() {
    let control_plane = FakeControlPlaneClient::default();
    assert_capability_matrix(
        &control_plane,
        &[
            (Capability::WorkspaceRefHistory, true),
            (Capability::StorageGc, true),
            (Capability::ObjectMetadata, true),
            (Capability::WorkViews, true),
            (Capability::AgentLeases, true),
            (Capability::DeviceBootstrap, true),
            (Capability::DeviceTrust, true),
            (Capability::RecoveryKey, true),
        ],
    );
}

#[cfg(feature = "hosted-convex")]
#[test]
fn hosted_client_reports_only_typed_supported_capabilities() {
    let control_plane = bowline_control_plane::HostedControlPlaneClient::try_new_with_token(
        "http://127.0.0.1:3210",
        "token",
    )
    .expect("hosted client can be constructed for capability reporting");
    assert_capability_matrix(
        &control_plane,
        &[
            (Capability::WorkspaceRefHistory, true),
            (Capability::StorageGc, true),
            (Capability::ObjectMetadata, true),
            (Capability::WorkViews, true),
            (Capability::AgentLeases, true),
            (Capability::DeviceBootstrap, true),
            (Capability::DeviceTrust, true),
            (Capability::RecoveryKey, true),
        ],
    );
}

fn assert_capability_matrix(
    control_plane: &dyn ControlPlaneClient,
    expected: &[(Capability, bool)],
) {
    let capabilities = control_plane.capabilities();

    assert_eq!(
        expected.len(),
        Capability::ALL.len(),
        "capability matrix must make one support decision per known capability"
    );

    for capability in Capability::ALL {
        let matching_rows = expected
            .iter()
            .filter(|(expected_capability, _)| expected_capability == capability)
            .count();
        assert_eq!(
            matching_rows, 1,
            "capability matrix must contain exactly one row for {capability}"
        );
    }

    for (capability, supported) in expected {
        assert!(
            capabilities.contains(capability) == *supported,
            "reported capability support for {capability} did not match expected {supported}"
        );
        assert_eq!(control_plane.supports_capability(*capability), *supported);
    }
}

#[test]
fn conflict_occurrences_are_monotonic_exact_and_device_scoped() {
    let control_plane = FakeControlPlaneClient::default().with_local_device_id("device-1");
    control_plane.create_workspace("workspace-1");

    let initial = conflict_occurrence(10, ConflictOccurrenceState::Unresolved);
    let applied = control_plane
        .reconcile_conflict_occurrence(initial.clone())
        .expect("trusted local device reconciles conflict occurrence");
    assert_eq!(applied.outcome, ConflictReconcileOutcome::Applied);
    assert_eq!(applied.conflict.state, ConflictOccurrenceState::Unresolved);
    assert_eq!(applied.conflict.occurrence_version, 10);

    let event_count = control_plane
        .list_events(&WorkspaceId::new("workspace-1"))
        .expect("events")
        .len();
    let retry = control_plane
        .reconcile_conflict_occurrence(initial.clone())
        .expect("exact occurrence retry is idempotent");
    assert_eq!(retry.outcome, ConflictReconcileOutcome::Idempotent);
    assert_eq!(
        control_plane
            .list_events(&WorkspaceId::new("workspace-1"))
            .expect("events")
            .len(),
        event_count
    );

    let mut stale = initial.clone();
    stale.occurrence_version = 9;
    stale.base_snapshot_id = SnapshotId::new("snap_stale");
    let superseded = control_plane
        .reconcile_conflict_occurrence(stale)
        .expect("stale occurrence is fenced");
    assert_eq!(superseded.outcome, ConflictReconcileOutcome::Superseded);
    assert_eq!(superseded.conflict.occurrence_version, 10);

    let mut replacement = initial.clone();
    replacement.occurrence_version = 11;
    replacement.base_snapshot_id = SnapshotId::new("snap_base_2");
    replacement.remote_snapshot_id = SnapshotId::new("snap_remote_2");
    let replaced = control_plane
        .reconcile_conflict_occurrence(replacement.clone())
        .expect("higher occurrence replaces lower atomically");
    assert_eq!(replaced.outcome, ConflictReconcileOutcome::Applied);
    assert_eq!(replaced.conflict.occurrence_version, 11);

    let mut accepted = replacement.clone();
    accepted.desired_state = ConflictOccurrenceState::Accepted;
    let resolved = control_plane
        .reconcile_conflict_occurrence(accepted.clone())
        .expect("exact occurrence resolves");
    assert_eq!(resolved.outcome, ConflictReconcileOutcome::Applied);
    assert_eq!(resolved.conflict.state, ConflictOccurrenceState::Accepted);
    assert_eq!(
        resolved.conflict.resolved_by_device_id.as_deref(),
        Some("device-1")
    );
    assert_eq!(
        control_plane
            .reconcile_conflict_occurrence(accepted)
            .expect("resolution retry is idempotent")
            .outcome,
        ConflictReconcileOutcome::Idempotent
    );

    let mut inexact_resolution = replacement;
    inexact_resolution.occurrence_version = 12;
    inexact_resolution.desired_state = ConflictOccurrenceState::Rejected;
    assert!(matches!(
        control_plane
            .reconcile_conflict_occurrence(inexact_resolution)
            .expect_err("resolution requires an exact occurrence"),
        ControlPlaneError::Conflict { .. }
    ));

    let wrong_device = control_plane
        .list_workspace_conflicts(&WorkspaceId::new("workspace-1"), &DeviceId::new("device-2"))
        .expect_err("fake device scope is enforced");
    assert_device_not_trusted(wrong_device);
}

fn conflict_occurrence(
    occurrence_version: u64,
    desired_state: ConflictOccurrenceState,
) -> ConflictOccurrenceReconcile {
    ConflictOccurrenceReconcile {
        workspace_id: WorkspaceId::new("workspace-1"),
        conflict_id: ConflictId::new("conflict_abc123"),
        conflict_kind: "env-key".to_string(),
        paths: vec!["apps/web/.env.local".to_string()],
        contains_secrets: true,
        base_snapshot_id: SnapshotId::new("snap_base"),
        remote_snapshot_id: SnapshotId::new("snap_remote"),
        occurrence_version,
        desired_state,
        device_id: DeviceId::new("device-1"),
        reason: "remote workspace head diverged".to_string(),
        bundle_object: None,
    }
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
        content_id: ContentId::new("manifest-content"),
        byte_len: 64,
        hash: "b3_manifest".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp { tick: 10 },
    };
    let direct_object = ObjectPointer {
        object_key: pack_upload.object_key.clone(),
        content_id: ContentId::new("content-1"),
        byte_len: pack_upload.byte_len,
        hash: "b3_pack".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };

    control_plane
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            snapshot_id: SnapshotId::new("snapshot-1"),
            manifest_id: ManifestId::new("manifest-1"),
            manifest_object,
            direct_objects: vec![direct_object],
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("manifest commit publishes object pointers");

    let download = control_plane
        .create_download_intent(DownloadIntentRequest {
            workspace_id: WorkspaceId::new("workspace-1"),
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

        let direct_object = ObjectPointer {
            object_key: shared_pack_key.to_string(),
            content_id: ContentId::new(format!("content-{workspace_id}")),
            byte_len: 128,
            hash: format!("b3_pack_{workspace_id}"),
            key_epoch: 1,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp { tick: 11 },
        };
        let manifest_object = ObjectPointer {
            object_key: shared_manifest_key.to_string(),
            content_id: ContentId::new(format!("manifest-{workspace_id}")),
            byte_len: 64,
            hash: format!("b3_manifest_{workspace_id}"),
            key_epoch: 1,
            kind: ObjectKind::SnapshotManifest,
            created_at: ControlPlaneTimestamp { tick: 10 },
        };

        control_plane
            .commit_snapshot_graph(SnapshotGraphCommit {
                workspace_id: WorkspaceId::new(workspace_id),
                snapshot_id: SnapshotId::new(format!("snapshot-{workspace_id}")),
                manifest_id: ManifestId::new(format!("manifest-{workspace_id}")),
                manifest_object,
                direct_objects: vec![direct_object],
                committed_by_device_id: DeviceId::new("device-1"),
            })
            .expect("same logical object key commits independently per workspace");
    }

    let metadata_a = control_plane
        .head_object_metadata(&WorkspaceId::new("workspace-a"), shared_pack_key)
        .expect("workspace a metadata");
    let metadata_b = control_plane
        .head_object_metadata(&WorkspaceId::new("workspace-b"), shared_pack_key)
        .expect("workspace b metadata");

    assert_eq!(metadata_a.key.as_str(), shared_pack_key);
    assert_eq!(metadata_a.hash, "b3_pack_workspace-a");
    assert_eq!(metadata_b.key.as_str(), shared_pack_key);
    assert_eq!(metadata_b.hash, "b3_pack_workspace-b");

    assert!(matches!(
        control_plane.head_object_metadata(&WorkspaceId::new("workspace-missing"), shared_pack_key),
        Err(ControlPlaneError::WorkspaceMissing { .. })
    ));
}

#[test]
fn delete_intents_require_delete_eligible_metadata() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-delete");
    let direct_object = commit_one_direct_object(
        &control_plane,
        "workspace-delete",
        "snapshot-delete",
        "manifest-delete",
        "content-delete",
    );

    let not_eligible = control_plane
        .create_storage_gc_delete_intent(
            &WorkspaceId::new("workspace-delete"),
            &direct_object.object_key,
        )
        .expect_err("current objects are not delete-eligible");
    assert!(matches!(
        not_eligible,
        ControlPlaneError::Conflict {
            resource: "storage GC delete intent",
            ..
        }
    ));

    let forbidden = control_plane
        .mark_object_retention_state(ObjectRetentionStateUpdate::new(
            "workspace-delete",
            &direct_object.object_key,
            RetentionState::DeleteEligible,
        ))
        .expect_err("delete eligibility requires the GC authority");
    assert!(matches!(
        forbidden,
        ControlPlaneError::Conflict {
            resource: "object retention",
            ..
        }
    ));

    let still_live = control_plane
        .create_storage_gc_delete_intent(
            &WorkspaceId::new("workspace-delete"),
            &direct_object.object_key,
        )
        .expect_err("a live object never receives a fake delete intent");
    assert!(matches!(
        still_live,
        ControlPlaneError::Conflict {
            resource: "storage GC delete intent",
            ..
        }
    ));
}

#[test]
fn locator_upload_intents_use_index_object_keys() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new("workspace-1", ObjectKind::LocatorIndex, 128)
                .with_content_id("locator-content"),
        )
        .expect("upload intent");

    assert_eq!(upload.object_kind, ObjectKind::LocatorIndex);
    assert!(upload.object_key.starts_with("indexes_ix_"));
    assert!(is_opaque_object_key(&upload.object_key));
}

#[test]
fn snapshot_graph_commit_records_only_object_pointers_and_event() {
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
        content_id: ContentId::new("manifest-content"),
        byte_len: 64,
        hash: "b3_manifest".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp { tick: 10 },
    };
    let direct_object = ObjectPointer {
        object_key: pack_upload.object_key,
        content_id: ContentId::new("pack-content"),
        byte_len: 256,
        hash: "b3_pack".to_string(),
        key_epoch: 7,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };

    let record = control_plane
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            snapshot_id: SnapshotId::new("snapshot-1"),
            manifest_id: ManifestId::new("manifest-1"),
            manifest_object: manifest_object.clone(),
            direct_objects: vec![direct_object.clone()],
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("manifest commit");

    assert_eq!(record.manifest_object, manifest_object);
    assert_eq!(record.direct_objects, vec![direct_object.clone()]);
    assert_eq!(record.direct_objects[0].key_epoch, 7);
    let pack_metadata = control_plane
        .head_object_metadata(&WorkspaceId::new("workspace-1"), &direct_object.object_key)
        .expect("pack metadata");
    assert_eq!(pack_metadata.key_epoch, 7);
    assert!(
        control_plane
            .list_events(&WorkspaceId::new("workspace-1"))
            .expect("events")
            .iter()
            .any(
                |event| event.kind == CompactEventKind::SnapshotRootCommitted
                    && event.subject == "manifest-1"
            )
    );

    let repeated = control_plane
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            snapshot_id: SnapshotId::new("snapshot-1"),
            manifest_id: ManifestId::new("manifest-1"),
            manifest_object: manifest_object.clone(),
            direct_objects: vec![direct_object.clone()],
            committed_by_device_id: DeviceId::new("device-2"),
        })
        .expect("idempotent manifest commit from another device");
    assert_eq!(repeated, record);
    assert_eq!(repeated.committed_by_device_id, "device-1");

    let remote_metadata = control_plane
        .head_object_metadata(&WorkspaceId::new("workspace-1"), &direct_object.object_key)
        .expect("committed object metadata is readable through the control plane");
    assert_eq!(remote_metadata.hash, "b3_pack");
    assert_eq!(remote_metadata.byte_len, 256);

    let mismatched_existing_object = control_plane
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            snapshot_id: SnapshotId::new("snapshot-2"),
            manifest_id: ManifestId::new("manifest-2"),
            manifest_object,
            direct_objects: vec![ObjectPointer {
                hash: "b3_different_pack".to_string(),
                ..direct_object
            }],
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect_err("existing object metadata must match exactly");
    assert!(matches!(
        mismatched_existing_object,
        ControlPlaneError::Conflict { .. }
    ));
}

#[test]
fn snapshot_graph_root_is_lookupable_by_snapshot_id() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let record = commit_snapshot_graph_fixture(
        &control_plane,
        "workspace-1",
        "snapshot-1",
        "device-1",
        "manifest-content-1",
        "pack-content-1",
    )
    .expect("manifest commit");

    let fetched = control_plane
        .get_snapshot_graph(
            &WorkspaceId::new("workspace-1"),
            &SnapshotId::new("snapshot-1"),
        )
        .expect("snapshot lookup")
        .expect("snapshot graph root exists");

    assert_eq!(fetched, record);
    assert_eq!(fetched.manifest_object.kind, ObjectKind::SnapshotManifest);
    assert_eq!(fetched.direct_objects[0].kind, ObjectKind::SourcePack);
}

#[test]
fn snapshot_graph_commit_rejects_different_manifest_for_same_snapshot() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    commit_snapshot_graph_fixture(
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
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            snapshot_id: SnapshotId::new("snapshot-1"),
            manifest_id: ManifestId::new("manifest-snapshot-1-different"),
            manifest_object: ObjectPointer {
                object_key: manifest_upload.object_key,
                content_id: ContentId::new("manifest-content-2"),
                byte_len: 64,
                hash: "b3_manifest-content-2".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 12 },
            },
            direct_objects: vec![ObjectPointer {
                object_key: pack_upload.object_key,
                content_id: ContentId::new("pack-content-2"),
                byte_len: 256,
                hash: "b3_pack-content-2".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 13 },
            }],
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect_err("same snapshot cannot point at a different manifest");

    assert!(matches!(error, ControlPlaneError::Conflict { .. }));
    let fetched = control_plane
        .get_snapshot_graph(
            &WorkspaceId::new("workspace-1"),
            &SnapshotId::new("snapshot-1"),
        )
        .expect("snapshot lookup")
        .expect("snapshot graph root exists");
    assert_eq!(fetched.manifest_id, "manifest-snapshot-1");
}

#[test]
fn missing_snapshot_graph_root_returns_none() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    assert_eq!(
        control_plane
            .get_snapshot_graph(
                &WorkspaceId::new("workspace-1"),
                &SnapshotId::new("missing-snapshot")
            )
            .expect("snapshot lookup"),
        None
    );
}

#[test]
fn snapshot_graph_lookup_is_workspace_scoped() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-a");
    control_plane.create_workspace("workspace-b");

    let workspace_a = commit_snapshot_graph_fixture(
        &control_plane,
        "workspace-a",
        "snapshot-shared",
        "device-a",
        "manifest-content-a",
        "pack-content-a",
    )
    .expect("workspace a manifest commit");
    let workspace_b = commit_snapshot_graph_fixture(
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
            .get_snapshot_graph(
                &WorkspaceId::new("workspace-a"),
                &SnapshotId::new("snapshot-shared")
            )
            .expect("workspace a snapshot lookup"),
        Some(workspace_a)
    );
    assert_eq!(
        control_plane
            .get_snapshot_graph(
                &WorkspaceId::new("workspace-b"),
                &SnapshotId::new("snapshot-shared")
            )
            .expect("workspace b snapshot lookup"),
        Some(workspace_b)
    );
    assert_eq!(
        control_plane
            .get_snapshot_graph(
                &WorkspaceId::new("workspace-b"),
                &SnapshotId::new("snapshot-a-only")
            )
            .expect("wrong workspace lookup"),
        None
    );
}

#[test]
fn trusted_workspace_rejects_untrusted_manifest_commit_and_lookup() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-trust");
    create_first_device(&control_plane, "workspace-trust", "device-1");

    let commit = commit_snapshot_graph_fixture(
        &control_plane,
        "workspace-trust",
        "snapshot-untrusted",
        "device-2",
        "manifest-content-untrusted",
        "pack-content-untrusted",
    )
    .expect_err("untrusted device cannot commit a manifest");
    assert_device_not_trusted(commit);

    let trusted = commit_snapshot_graph_fixture(
        &control_plane,
        "workspace-trust",
        "snapshot-trusted-root",
        "device-1",
        "manifest-content-trusted-root",
        "pack-content-trusted-root",
    )
    .expect("trusted device commits source graph");
    let root_suffix = Sha256::digest(b"snapshot-trusted-root")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root_commit = control_plane
        .commit_snapshot_root(SnapshotRootCommit {
            workspace_id: WorkspaceId::new("workspace-trust"),
            snapshot_id: SnapshotId::new("snapshot-untrusted-root-only"),
            manifest_id: trusted.manifest_id,
            manifest_object: trusted.manifest_object,
            namespace_root_id: format!("nsp_{root_suffix}"),
            extra_root_logical_ids: Vec::new(),
            committed_by_device_id: DeviceId::new("device-2"),
        })
        .expect_err("untrusted device cannot commit an existing metadata root");
    assert_device_not_trusted(root_commit);

    let untrusted_reader = control_plane.clone().with_local_device_id("device-2");
    let lookup = untrusted_reader
        .get_snapshot_graph(
            &WorkspaceId::new("workspace-trust"),
            &SnapshotId::new("snapshot-untrusted"),
        )
        .expect_err("untrusted device cannot lookup manifests");
    assert_device_not_trusted(lookup);
}

#[test]
fn revoked_device_cannot_commit_or_lookup_snapshot_graph() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-revoked-object");
    create_first_device(&control_plane, "workspace-revoked-object", "device-1");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-revoked-object"),
                device_id: DeviceId::new("device-2"),
                device_name: "linux".to_string(),
                device_public_key: "age1device2".to_string(),
                device_fingerprint: "fp_device_2".to_string(),
                device_authorization_proof_verifier: device_verifier("device-2"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    let request_id = request.request_id.clone();
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request_id.clone(),
            approved_by_device_id: DeviceId::new("device-1"),
            approved_by_device_proof: device_proof(
                "workspace-revoked-object",
                "device-1",
                "approve-device-request",
                &device_request_proof_subject(&request_id),
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
            device_id: DeviceId::new("device-2"),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-revoked-object",
                &request_id,
                "device-2",
            ),
        })
        .expect("requester accepts grant");

    let record = commit_snapshot_graph_fixture(
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
            workspace_id: WorkspaceId::new("workspace-revoked-object"),
            device_id: DeviceId::new("device-2"),
            revoked_by_device_id: DeviceId::new("device-1"),
            revoked_by_device_proof: device_proof(
                "workspace-revoked-object",
                "device-1",
                "revoke-device",
                &device_revocation_proof_subject("device-2"),
            ),
            reason: "lost device".to_string(),
        })
        .expect("trusted device revokes device-2");

    let revoked_commit = commit_snapshot_graph_fixture(
        &control_plane,
        "workspace-revoked-object",
        "snapshot-after-revoke",
        "device-2",
        "manifest-content-after-revoke",
        "pack-content-after-revoke",
    )
    .expect_err("revoked device cannot commit a manifest");
    assert_device_not_trusted(revoked_commit);

    let revoked_reader = control_plane.clone().with_local_device_id("device-2");
    let revoked_lookup = revoked_reader
        .get_snapshot_graph(
            &WorkspaceId::new("workspace-revoked-object"),
            &SnapshotId::new("snapshot-before-revoke"),
        )
        .expect_err("revoked device cannot lookup manifests");
    assert_device_not_trusted(revoked_lookup);

    let trusted_reader = control_plane.with_local_device_id("device-1");
    assert_eq!(
        trusted_reader
            .get_snapshot_graph(
                &WorkspaceId::new("workspace-revoked-object"),
                &SnapshotId::new("snapshot-before-revoke")
            )
            .expect("trusted reader lookup"),
        Some(record)
    );
}

#[test]
fn snapshot_graph_commit_rejects_unreserved_or_mismatched_objects() {
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
        content_id: ContentId::new("pack-content"),
        byte_len: 256,
        hash: "b3_pack".to_string(),
        key_epoch: 1,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };
    let error = control_plane
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            snapshot_id: SnapshotId::new("snapshot-unreserved"),
            manifest_id: ManifestId::new("manifest-unreserved"),
            manifest_object: ObjectPointer {
                object_key: manifest_upload.object_key.clone(),
                content_id: ContentId::new("manifest-content"),
                byte_len: 64,
                hash: "b3_manifest".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 10 },
            },
            direct_objects: vec![unreserved_pack],
            committed_by_device_id: DeviceId::new("device-1"),
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
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            snapshot_id: SnapshotId::new("snapshot-mismatch"),
            manifest_id: ManifestId::new("manifest-mismatch"),
            manifest_object: ObjectPointer {
                object_key: manifest_upload.object_key,
                content_id: ContentId::new("manifest-content"),
                byte_len: 64,
                hash: "b3_manifest".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 10 },
            },
            direct_objects: vec![ObjectPointer {
                object_key: pack_upload.object_key,
                content_id: ContentId::new("pack-content"),
                byte_len: 128,
                hash: "b3_pack".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 11 },
            }],
            committed_by_device_id: DeviceId::new("device-1"),
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
            workspace_id: WorkspaceId::new("workspace-work"),
            work_view_id: WorkViewId::new("work-1"),
            project_id: ProjectId::new("acme-web"),
            name: "try-cache".to_string(),
            visible_path: ".work/acme-web/try-cache".to_string(),
            base_snapshot_id: SnapshotId::new("empty"),
            base_workspace_version: 0,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
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
        content_id: ContentId::new("overlay-content-1"),
        byte_len: 512,
        hash: "b3_overlay-content-1".to_string(),
        key_epoch: 1,
        kind: ObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp { tick: 20 },
    };
    let updated = control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: WorkspaceId::new("workspace-work"),
            work_view_id: WorkViewId::new("work-1"),
            expected_overlay_version: 0,
            overlay_object: overlay_object.clone(),
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("overlay commit");

    assert_eq!(updated.overlay_version, 1);
    assert_eq!(updated.overlay_head, Some(overlay_object.clone()));
    assert_eq!(
        control_plane
            .head_object_metadata(
                &WorkspaceId::new("workspace-work"),
                &overlay_object.object_key
            )
            .expect("overlay metadata")
            .kind,
        bowline_storage::ObjectKind::AgentOverlay
    );
    assert!(
        control_plane
            .list_events(&WorkspaceId::new("workspace-work"))
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
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("workspace-work-base"),
            0,
            &SnapshotId::new("snap_base"),
            &DeviceId::new("device-1"),
        )
        .expect("base ref");
    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("workspace-work-base"),
            base_ref.version,
            &SnapshotId::new("snap_next"),
            &DeviceId::new("device-1"),
        )
        .expect("advanced ref");

    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: WorkspaceId::new("workspace-work-base"),
            work_view_id: WorkViewId::new("work-current"),
            project_id: ProjectId::new("acme-web"),
            name: "current-base".to_string(),
            visible_path: ".work/acme-web/current-base".to_string(),
            base_snapshot_id: advanced_ref.snapshot_id,
            base_workspace_version: advanced_ref.version,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
        })
        .expect("current workspace ref is a valid base");

    let missing_historical = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: WorkspaceId::new("workspace-work-base"),
            work_view_id: WorkViewId::new("work-missing-base"),
            project_id: ProjectId::new("acme-web"),
            name: "missing-base".to_string(),
            visible_path: ".work/acme-web/missing-base".to_string(),
            base_snapshot_id: SnapshotId::new("snap_base"),
            base_workspace_version: base_ref.version,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
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
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new("workspace-work-base"),
            snapshot_id: SnapshotId::new("snap_real"),
            manifest_id: ManifestId::new("snap_missing"),
            manifest_object: ObjectPointer {
                object_key: wrong_manifest_upload.object_key,
                content_id: ContentId::new("manifest-wrong"),
                byte_len: 64,
                hash: "b3_manifest-wrong".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SnapshotManifest,
                created_at: ControlPlaneTimestamp { tick: 30 },
            },
            direct_objects: vec![ObjectPointer {
                object_key: wrong_pack_upload.object_key,
                content_id: ContentId::new("pack-wrong"),
                byte_len: 256,
                hash: "b3_pack-wrong".to_string(),
                key_epoch: 1,
                kind: ObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 31 },
            }],
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("manifest with snapshot-looking id commits");
    let manifest_id_is_not_a_snapshot = control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: WorkspaceId::new("workspace-work-base"),
            work_view_id: WorkViewId::new("work-manifest-id"),
            project_id: ProjectId::new("acme-web"),
            name: "manifest-id-base".to_string(),
            visible_path: ".work/acme-web/manifest-id-base".to_string(),
            base_snapshot_id: SnapshotId::new("snap_missing"),
            base_workspace_version: 0,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
        })
        .expect_err("manifest id alone is not a committed base snapshot");
    assert!(matches!(
        manifest_id_is_not_a_snapshot,
        ControlPlaneError::Conflict { .. }
    ));

    commit_snapshot_graph_fixture(
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
            workspace_id: WorkspaceId::new("workspace-work-base"),
            work_view_id: WorkViewId::new("work-historical"),
            project_id: ProjectId::new("acme-web"),
            name: "historical-base".to_string(),
            visible_path: ".work/acme-web/historical-base".to_string(),
            base_snapshot_id: SnapshotId::new("snap_base"),
            base_workspace_version: base_ref.version,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
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
            workspace_id: WorkspaceId::new("workspace-stale-work"),
            work_view_id: WorkViewId::new("work-stale"),
            project_id: ProjectId::new("acme-web"),
            name: "stale-head".to_string(),
            visible_path: ".work/acme-web/stale-head".to_string(),
            base_snapshot_id: SnapshotId::new("empty"),
            base_workspace_version: 0,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
        })
        .expect("work view create");
    let first = reserve_overlay_object(&control_plane, "workspace-stale-work", "overlay-first", 20);
    control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: WorkspaceId::new("workspace-stale-work"),
            work_view_id: WorkViewId::new("work-stale"),
            expected_overlay_version: 0,
            overlay_object: first,
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("first overlay commit");

    let stale = control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: WorkspaceId::new("workspace-stale-work"),
            work_view_id: WorkViewId::new("work-stale"),
            expected_overlay_version: 0,
            overlay_object: reserve_overlay_object(
                &control_plane,
                "workspace-stale-work",
                "overlay-second",
                21,
            ),
            committed_by_device_id: DeviceId::new("device-2"),
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
            workspace_id: WorkspaceId::new("workspace-lifecycle"),
            work_view_id: WorkViewId::new("work-life"),
            project_id: ProjectId::new("acme-web"),
            name: "review".to_string(),
            visible_path: ".work/acme-web/review".to_string(),
            base_snapshot_id: SnapshotId::new("empty"),
            base_workspace_version: 0,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
        })
        .expect("work view create");

    let review_ready = control_plane
        .update_work_view_lifecycle(WorkViewLifecycleUpdate {
            workspace_id: WorkspaceId::new("workspace-lifecycle"),
            work_view_id: WorkViewId::new("work-life"),
            lifecycle: WorkViewLifecycleState::ReviewReady,
            updated_by_device_id: DeviceId::new("device-1"),
        })
        .expect("review-ready update");
    assert_eq!(review_ready.lifecycle, WorkViewLifecycleState::ReviewReady);
    assert_eq!(
        control_plane
            .list_work_views(&WorkspaceId::new("workspace-lifecycle"), false)
            .expect("default list")
            .len(),
        1
    );

    control_plane
        .update_work_view_lifecycle(WorkViewLifecycleUpdate {
            workspace_id: WorkspaceId::new("workspace-lifecycle"),
            work_view_id: WorkViewId::new("work-life"),
            lifecycle: WorkViewLifecycleState::Discarded,
            updated_by_device_id: DeviceId::new("device-1"),
        })
        .expect("discard update");
    assert!(
        control_plane
            .list_work_views(&WorkspaceId::new("workspace-lifecycle"), false)
            .expect("default list")
            .is_empty()
    );
    assert_eq!(
        control_plane
            .list_work_views(&WorkspaceId::new("workspace-lifecycle"), true)
            .expect("all list")
            .len(),
        1
    );
    control_plane
        .restore_work_view(
            &WorkspaceId::new("workspace-lifecycle"),
            &WorkViewId::new("work-life"),
            &DeviceId::new("device-1"),
        )
        .expect("restore work view");

    let event_kinds = control_plane
        .list_events(&WorkspaceId::new("workspace-lifecycle"))
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
            workspace_id: WorkspaceId::new("workspace-work-a"),
            work_view_id: WorkViewId::new("work-untrusted"),
            project_id: ProjectId::new("acme-web"),
            name: "untrusted".to_string(),
            visible_path: ".work/acme-web/untrusted".to_string(),
            base_snapshot_id: SnapshotId::new("empty"),
            base_workspace_version: 0,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-2"),
        })
        .expect_err("untrusted device cannot create work");
    assert_device_not_trusted(untrusted_create);

    control_plane
        .create_work_view(WorkViewCreate {
            workspace_id: WorkspaceId::new("workspace-work-a"),
            work_view_id: WorkViewId::new("work-trusted"),
            project_id: ProjectId::new("acme-web"),
            name: "trusted".to_string(),
            visible_path: ".work/acme-web/trusted".to_string(),
            base_snapshot_id: SnapshotId::new("empty"),
            base_workspace_version: 0,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-1"),
        })
        .expect("trusted device creates work");

    let untrusted_reader = control_plane.clone().with_local_device_id("device-2");
    let list_error = untrusted_reader
        .list_work_views(&WorkspaceId::new("workspace-work-a"), true)
        .expect_err("untrusted device cannot list work views");
    assert_device_not_trusted(list_error);

    let workspace_b_overlay =
        reserve_overlay_object(&control_plane, "workspace-work-b", "overlay-b", 22);
    let cross_workspace = control_plane
        .commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: WorkspaceId::new("workspace-work-a"),
            work_view_id: WorkViewId::new("work-trusted"),
            expected_overlay_version: 0,
            overlay_object: workspace_b_overlay,
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect_err("overlay object reservation belongs to another workspace");
    assert!(matches!(cross_workspace, WorkViewUpdateError::Storage(_)));
}

#[test]
fn compact_lease_create_update_and_events_omit_local_only_text() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-lease");
    let lease = control_plane
        .create_lease(LeaseCreate {
            workspace_id: WorkspaceId::new("workspace-lease"),
            lease_id: LeaseId::new("lease-1"),
            project_id: ProjectId::new("project-acme"),
            device_id: DeviceId::new("device-1"),
            target_device_ref: None,
            origin_device_ref: None,
            write_target_mode: LeaseWriteTargetMode::WorkView,
            work_view_id: Some(WorkViewId::new("work-lease-1")),
            base_snapshot_id: SnapshotId::new("empty"),
            task_label: None,
            session_state: LeaseSessionState::Open,
            status_code: "open".to_string(),
            expires_at: ControlPlaneTimestamp { tick: 3_600 },
        })
        .expect("compact lease create");

    assert_eq!(lease.project_id, "project-acme");
    assert_eq!(lease.write_target_mode, LeaseWriteTargetMode::WorkView);
    assert_eq!(lease.work_view_id.as_deref(), Some("work-lease-1"));
    assert_eq!(lease.session_state.as_str(), "open");
    assert_eq!(lease.status_code, "open");

    let updated = control_plane
        .update_lease(LeaseUpdate {
            workspace_id: WorkspaceId::new("workspace-lease"),
            lease_id: LeaseId::new("lease-1"),
            expected_version: 0,
            updated_by_device_id: DeviceId::new("device-1"),
            session_state: None,
            status_code: Some("review-ready".to_string()),
            event_kind: Some(CompactEventKind::LeaseUpdated),
        })
        .expect("compact lease update");

    assert_eq!(updated.version, 1);
    let events = control_plane
        .list_events(&WorkspaceId::new("workspace-lease"))
        .expect("lease events");
    assert!(events.iter().any(|event| {
        event.kind == CompactEventKind::LeaseCreated && event.subject == "lease-1"
    }));
    assert!(events.iter().any(|event| {
        event.kind == CompactEventKind::LeaseUpdated && event.subject == "lease-1"
    }));
}

#[test]
fn handoff_lease_contract_rejects_invalid_refs_and_forged_events() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-handoff-contract");
    let mut handoff = lease_create_input(
        "workspace-handoff-contract",
        "lease-handoff-contract",
        "device-origin",
    );
    handoff.target_device_ref = Some("device-target".to_string());
    handoff.origin_device_ref = Some("device-origin".to_string());
    handoff.write_target_mode = LeaseWriteTargetMode::WorkView;
    handoff.work_view_id = Some(WorkViewId::new("work-handoff-contract"));
    let missing_task = control_plane
        .create_lease(handoff.clone())
        .expect_err("handoff leases require task label");
    assert!(matches!(
        missing_task,
        ControlPlaneError::Conflict { resource, reason }
            if resource == "agent lease" && reason.contains("task label")
    ));

    let mut missing_origin = handoff.clone();
    missing_origin.lease_id = LeaseId::new("lease-handoff-missing-origin");
    missing_origin.task_label = Some("fix target bug".to_string());
    missing_origin.origin_device_ref = None;
    let partial_refs = control_plane
        .create_lease(missing_origin)
        .expect_err("handoff leases require both refs");
    assert!(matches!(
        partial_refs,
        ControlPlaneError::Conflict { resource, reason }
            if resource == "agent lease" && reason.contains("both target and origin device refs")
    ));

    let mut wrong_origin = handoff.clone();
    wrong_origin.lease_id = LeaseId::new("lease-handoff-wrong-origin");
    wrong_origin.task_label = Some("fix target bug".to_string());
    wrong_origin.origin_device_ref = Some("device-other".to_string());
    let origin_mismatch = control_plane
        .create_lease(wrong_origin)
        .expect_err("origin device ref must match creating device");
    assert!(matches!(
        origin_mismatch,
        ControlPlaneError::Conflict { resource, reason }
            if resource == "agent lease" && reason.contains("origin device ref")
    ));

    handoff.task_label = Some("fix target bug".to_string());
    control_plane
        .create_lease(handoff)
        .expect("handoff lease with task label and matching refs");

    let direct = control_plane
        .create_lease(lease_create_input(
            "workspace-handoff-contract",
            "lease-direct-contract",
            "device-origin",
        ))
        .expect("direct lease");
    let forged_event = control_plane
        .update_lease(LeaseUpdate {
            workspace_id: WorkspaceId::new("workspace-handoff-contract"),
            lease_id: direct.lease_id,
            expected_version: direct.version,
            updated_by_device_id: DeviceId::new("device-origin"),
            session_state: None,
            status_code: None,
            event_kind: Some(CompactEventKind::DeviceApproved),
        })
        .expect_err("generic update cannot forge a non-lease event");
    assert!(matches!(
        forged_event,
        ControlPlaneError::Conflict { resource, reason }
            if resource == "agent lease" && reason.contains("event kind")
    ));
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
    assert_device_not_trusted(untrusted);

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
        .list_leases(&WorkspaceId::new("workspace-lease-trust"))
        .expect_err("untrusted device cannot list lease metadata");
    assert_device_not_trusted(list_error);

    authorize_device(
        &control_plane,
        "workspace-lease-trust",
        "device-1",
        "device-2",
    );
    let trusted_reader = control_plane.with_local_device_id("device-2");
    assert_eq!(
        trusted_reader
            .list_leases(&WorkspaceId::new("workspace-lease-trust"))
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
            workspace_id: WorkspaceId::new("workspace-revoked-work"),
            work_view_id: WorkViewId::new("work-revoked"),
            project_id: ProjectId::new("acme-web"),
            name: "revoked".to_string(),
            visible_path: ".work/acme-web/revoked".to_string(),
            base_snapshot_id: SnapshotId::new("empty"),
            base_workspace_version: 0,
            expires_at: None,
            retain_until: None,
            created_by_device_id: DeviceId::new("device-2"),
        })
        .expect_err("revoked device cannot create work views");
    assert_device_not_trusted(work_error);

    let lease_error = control_plane
        .create_lease(lease_create_input(
            "workspace-revoked-work",
            "lease-revoked",
            "device-2",
        ))
        .expect_err("revoked device cannot create leases");
    assert_device_not_trusted(lease_error);

    let revoked_reader = control_plane.with_local_device_id("device-2");
    let list_error = revoked_reader
        .list_leases(&WorkspaceId::new("workspace-revoked-work"))
        .expect_err("revoked device cannot list leases");
    assert_device_not_trusted(list_error);
}

#[test]
fn compact_lease_metadata_rejects_pathlike_or_uncommitted_pointer_fields() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-lease-allowlist");

    let pathlike_project = control_plane
        .create_lease(LeaseCreate {
            project_id: ProjectId::new("Users/user/Code/acme"),
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
}

#[path = "control_plane_contract/devices_recovery.rs"]
mod devices_recovery;

fn commit_snapshot_graph_fixture(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    snapshot_id: &str,
    device_id: &str,
    manifest_content_id: &str,
    pack_content_id: &str,
) -> Result<SnapshotGraphRecord, ControlPlaneError> {
    let manifest_upload = control_plane.create_upload_intent(
        UploadIntentRequest::new(workspace_id, ObjectKind::SnapshotManifest, 64)
            .with_content_id(manifest_content_id),
    )?;
    let pack_upload = control_plane.create_upload_intent(
        UploadIntentRequest::new(workspace_id, ObjectKind::SourcePack, 256)
            .with_content_id(pack_content_id),
    )?;

    control_plane.commit_snapshot_graph(SnapshotGraphCommit {
        workspace_id: WorkspaceId::new(workspace_id),
        snapshot_id: SnapshotId::new(snapshot_id),
        manifest_id: ManifestId::new(format!("manifest-{snapshot_id}")),
        manifest_object: ObjectPointer {
            object_key: manifest_upload.object_key,
            content_id: ContentId::new(manifest_content_id),
            byte_len: 64,
            hash: format!("b3_{manifest_content_id}"),
            key_epoch: 1,
            kind: ObjectKind::SnapshotManifest,
            created_at: ControlPlaneTimestamp { tick: 10 },
        },
        direct_objects: vec![ObjectPointer {
            object_key: pack_upload.object_key,
            content_id: ContentId::new(pack_content_id),
            byte_len: 256,
            hash: format!("b3_{pack_content_id}"),
            key_epoch: 1,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp { tick: 11 },
        }],
        committed_by_device_id: DeviceId::new(device_id),
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
        content_id: ContentId::new(content_id),
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
            workspace_id: WorkspaceId::new(workspace_id),
            device_id: DeviceId::new(device_id),
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
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new(workspace_id),
                device_id: DeviceId::new(device_id),
                device_name: format!("{device_id}-name"),
                device_public_key: format!("age1{device_id}"),
                device_fingerprint: format!("fp_{device_id}"),
                device_authorization_proof_verifier: device_verifier(device_id),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier(device_id),
        )
        .expect("device request");
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: DeviceId::new(approver_device_id),
            approved_by_device_proof: device_proof(
                workspace_id,
                approver_device_id,
                "approve-device-request",
                &device_request_proof_subject(&request.request_id),
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
            device_id: DeviceId::new(device_id),
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
            workspace_id: WorkspaceId::new(workspace_id),
            device_id: DeviceId::new(revoked_device_id),
            revoked_by_device_id: DeviceId::new(revoker_device_id),
            revoked_by_device_proof: device_proof(
                workspace_id,
                revoker_device_id,
                "revoke-device",
                &device_revocation_proof_subject(revoked_device_id),
            ),
            reason: "test revocation".to_string(),
        })
        .expect("trusted device revokes");
}

fn commit_one_direct_object(
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
        content_id: ContentId::new(format!("manifest-{content_id}")),
        byte_len: 64,
        hash: format!("b3_manifest_{content_id}"),
        key_epoch: 1,
        kind: ObjectKind::SnapshotManifest,
        created_at: ControlPlaneTimestamp { tick: 10 },
    };
    let direct_object = ObjectPointer {
        object_key: pack_upload.object_key,
        content_id: ContentId::new(content_id),
        byte_len: 128,
        hash: format!("b3_pack_{content_id}"),
        key_epoch: 1,
        kind: ObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };

    control_plane
        .commit_snapshot_graph(SnapshotGraphCommit {
            workspace_id: WorkspaceId::new(workspace_id),
            snapshot_id: SnapshotId::new(snapshot_id),
            manifest_id: ManifestId::new(manifest_id),
            manifest_object,
            direct_objects: vec![direct_object.clone()],
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("manifest commit");

    direct_object
}

fn lease_create_input(workspace_id: &str, lease_id: &str, device_id: &str) -> LeaseCreate {
    LeaseCreate {
        workspace_id: WorkspaceId::new(workspace_id),
        lease_id: LeaseId::new(lease_id),
        project_id: ProjectId::new("project-acme"),
        device_id: DeviceId::new(device_id),
        target_device_ref: None,
        origin_device_ref: None,
        write_target_mode: LeaseWriteTargetMode::Direct,
        work_view_id: None,
        base_snapshot_id: SnapshotId::new("empty"),
        task_label: None,
        session_state: LeaseSessionState::Open,
        status_code: "open".to_string(),
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
