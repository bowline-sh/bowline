use bowline_control_plane::{
    FakeControlPlaneClient, MetadataBindingCommit, MetadataBindingInput, MetadataBindingOutcome,
    MetadataRecordKind, MetadataSidecar, ObjectControlPlaneClient, ObjectKind, ObjectPointer,
    SnapshotRootCommit, UploadIntentRequest,
};
use bowline_core::ids::{ContentId, DeviceId, ManifestId, SnapshotId, WorkspaceId};

const WORKSPACE: &str = "workspace_metadata_graph";

#[test]
fn binding_race_returns_immutable_winner_and_rejects_sidecar_conflict() {
    let control = control_plane();
    let logical_id = logical_id('1');
    let first = binding(&control, &logical_id, '1', sidecar('a'));
    let loser = binding(&control, &logical_id, '2', sidecar('a'));
    let first_result = control
        .commit_metadata_bindings(commit(vec![first.clone()]))
        .expect("first binding");
    assert_eq!(
        first_result.bindings[0].outcome,
        Some(MetadataBindingOutcome::BoundNew)
    );
    let loser_result = control
        .commit_metadata_bindings(commit(vec![loser.clone()]))
        .expect("concurrent loser receives winner");
    assert_eq!(
        loser_result.bindings[0].object.object_key,
        first.object.object_key
    );
    assert_eq!(
        loser_result.bindings[0].outcome,
        Some(MetadataBindingOutcome::ExistingWinner)
    );
    assert_eq!(
        control
            .head_object_metadata(&WorkspaceId::new(WORKSPACE), &loser.object.object_key)
            .expect("loser metadata")
            .retention_state,
        bowline_storage::RetentionState::OrphanCandidate
    );

    let mut conflict = loser;
    conflict.sidecar = sidecar('f');
    assert!(
        control
            .commit_metadata_bindings(commit(vec![conflict]))
            .is_err()
    );
}

#[test]
fn failed_binding_batch_does_not_commit_an_earlier_valid_binding() {
    let control = control_plane();
    let first_id = logical_id('6');
    let second_id = logical_id('7');
    let first = binding(&control, &first_id, '6', sidecar('6'));
    let mut invalid = binding(&control, &second_id, '7', sidecar('7'));
    invalid.object = pointer(&control, ObjectKind::SnapshotManifest, '8');

    assert!(
        control
            .commit_metadata_bindings(commit(vec![first, invalid]))
            .is_err()
    );
    assert!(
        control
            .resolve_metadata_bindings(&WorkspaceId::new(WORKSPACE), &[first_id, second_id])
            .expect("binding lookup after rollback")
            .bindings
            .is_empty()
    );
}

#[test]
fn parents_and_snapshot_roots_require_complete_children() {
    let control = control_plane();
    let child_id = logical_id('3');
    let parent_id = logical_id('4');
    let parent = binding_with_children(&control, &parent_id, '4', vec![child_id.clone()]);
    assert!(
        control
            .commit_metadata_bindings(commit(vec![parent.clone()]))
            .is_err()
    );
    let child = binding(&control, &child_id, '3', sidecar('b'));
    control
        .commit_metadata_bindings(commit(vec![child]))
        .expect("child binding");
    control
        .commit_metadata_bindings(commit(vec![parent]))
        .expect("parent binding");

    let manifest = pointer(&control, ObjectKind::SnapshotManifest, '9');
    let root = control
        .commit_snapshot_root(SnapshotRootCommit {
            workspace_id: WorkspaceId::new(WORKSPACE),
            snapshot_id: SnapshotId::new("snapshot_graph"),
            manifest_id: ManifestId::new("manifest_graph"),
            manifest_object: manifest,
            namespace_root_id: parent_id,
            extra_root_logical_ids: Vec::new(),
            committed_by_device_id: DeviceId::new("device_graph"),
        })
        .expect("complete root");
    assert!(root.complete);
    assert_eq!(
        control
            .get_snapshot_root(
                &WorkspaceId::new(WORKSPACE),
                &SnapshotId::new("snapshot_graph")
            )
            .expect("root query")
            .expect("root")
            .namespace_root_id,
        root.namespace_root_id
    );
}

fn control_plane() -> FakeControlPlaneClient {
    let control = FakeControlPlaneClient::default();
    control.create_workspace(WORKSPACE);
    control
}

fn commit(bindings: Vec<MetadataBindingInput>) -> MetadataBindingCommit {
    MetadataBindingCommit {
        workspace_id: WorkspaceId::new(WORKSPACE),
        bindings,
        committed_by_device_id: DeviceId::new("device_graph"),
    }
}

fn binding(
    control: &FakeControlPlaneClient,
    logical_id: &str,
    suffix: char,
    sidecar: MetadataSidecar,
) -> MetadataBindingInput {
    binding_with_sidecar(control, logical_id, suffix, sidecar)
}

fn binding_with_children(
    control: &FakeControlPlaneClient,
    logical_id: &str,
    suffix: char,
    children: Vec<String>,
) -> MetadataBindingInput {
    let mut value = sidecar('c');
    value.child_logical_ids = children;
    binding_with_sidecar(control, logical_id, suffix, value)
}

fn binding_with_sidecar(
    control: &FakeControlPlaneClient,
    logical_id: &str,
    suffix: char,
    sidecar: MetadataSidecar,
) -> MetadataBindingInput {
    MetadataBindingInput {
        logical_id: logical_id.to_string(),
        record_kind: MetadataRecordKind::NamespacePage,
        object: page_pointer(control, logical_id, suffix),
        sidecar,
    }
}

fn page_pointer(control: &FakeControlPlaneClient, logical_id: &str, suffix: char) -> ObjectPointer {
    let object_key = format!("metadata_mp_{}", suffix.to_string().repeat(64));
    let content_id = ContentId::new(logical_id);
    control
        .create_upload_intent(
            UploadIntentRequest::new(WORKSPACE, ObjectKind::SnapshotMetadataPage, 16)
                .with_content_id(content_id.as_str())
                .with_object_key(object_key.clone()),
        )
        .expect("metadata page upload reservation");
    ObjectPointer {
        object_key,
        content_id,
        byte_len: 16,
        hash: format!("b3_{}", suffix.to_string().repeat(64)),
        key_epoch: 1,
        kind: ObjectKind::SnapshotMetadataPage,
        created_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
    }
}

fn pointer(control: &FakeControlPlaneClient, kind: ObjectKind, suffix: char) -> ObjectPointer {
    let object_key = match kind {
        ObjectKind::SnapshotMetadataPage => {
            format!("metadata_mp_{}", suffix.to_string().repeat(64))
        }
        ObjectKind::SnapshotManifest => format!("manifests_mf_{}", suffix.to_string().repeat(16)),
        _ => unreachable!(),
    };
    let content_id = ContentId::new(match kind {
        ObjectKind::SnapshotMetadataPage => logical_id(suffix),
        ObjectKind::SnapshotManifest => format!("content_{suffix}"),
        _ => unreachable!(),
    });
    control
        .create_upload_intent(
            UploadIntentRequest::new(WORKSPACE, kind, 16)
                .with_content_id(content_id.as_str())
                .with_object_key(object_key.clone()),
        )
        .expect("upload reservation");
    ObjectPointer {
        object_key,
        content_id,
        byte_len: 16,
        hash: format!("b3_{}", suffix.to_string().repeat(64)),
        key_epoch: 1,
        kind,
        created_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
    }
}

fn logical_id(suffix: char) -> String {
    format!("nsp_{}", suffix.to_string().repeat(64))
}

fn sidecar(suffix: char) -> MetadataSidecar {
    MetadataSidecar {
        child_logical_ids: Vec::new(),
        direct_object_keys: Vec::new(),
        digest: format!("b3_{}", suffix.to_string().repeat(64)),
    }
}
