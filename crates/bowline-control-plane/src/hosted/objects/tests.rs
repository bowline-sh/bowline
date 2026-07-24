use super::proof::generated_object_key;
use super::*;
use bowline_core::ids::ContentId;

fn gc_dto(key: String) -> HostedStorageGcObjectRef {
    HostedStorageGcObjectRef {
        key,
        retention_state: HostedRetentionState::Retained,
        referenced_by_current_head: false,
        referenced_by_snapshot: Some("snapshot_1".to_string()),
        referenced_by_work_view_base: true,
        referenced_by_active_overlay: false,
        // Still on the wire (server sends constant false) until the contract
        // regen drops them; the domain conversion discards them.
        referenced_by_active_lease: false,
        referenced_by_conflict_bundle: false,
        verified: true,
    }
}

fn blob_object_key(seed: &str) -> String {
    generated_object_key(ObjectKind::Blob, seed)
}

fn metadata_dto() -> HostedObjectMetadata {
    let key = generated_object_key(ObjectKind::Blob, "metadata-boundary");
    HostedObjectMetadata {
        byte_length: 256,
        content_id: Some("cid_metadata".to_string()),
        created_at: "2026-06-23T12:00:00Z".to_string(),
        hash: "blake3:metadata".to_string(),
        key_epoch: 9,
        kind: HostedObjectKind::Blob,
        object_key: key,
        retention_state: HostedRetentionState::Retained,
        workspace_id: "ws_code".to_string(),
    }
}

fn assert_parse_error_field<T: std::fmt::Debug>(result: ControlPlaneResult<T>, field: &str) {
    let error = result.expect_err("malformed value must reject");
    assert!(
        error.to_string().contains(&format!("`{field}`")),
        "error must identify field `{field}`, got: {error}"
    );
}

#[test]
fn gc_object_ref_dto_maps_flags_state_and_snapshot() {
    let key = generated_object_key(ObjectKind::Blob, "gc-boundary");
    let reference = storage_gc_object_ref_from_dto(gc_dto(key)).expect("valid gc reference");
    assert_eq!(reference.retention_state, RetentionState::Retained);
    assert!(reference.referenced_by_work_view_base);
    assert_eq!(
        reference
            .referenced_by_snapshot
            .as_ref()
            .map(|id| id.as_str()),
        Some("snapshot_1")
    );
    assert!(reference.verified);
}

#[test]
fn gc_object_ref_dto_rejects_invalid_opaque_key() {
    assert!(matches!(
        storage_gc_object_ref_from_dto(gc_dto("../secret".to_string())),
        Err(ControlPlaneError::InvalidObjectKey { .. })
    ));
}

#[test]
fn retention_state_dto_maps_every_variant() {
    for (dto, domain) in [
        (HostedRetentionState::Pending, RetentionState::Pending),
        (HostedRetentionState::Current, RetentionState::Current),
        (
            HostedRetentionState::OrphanCandidate,
            RetentionState::OrphanCandidate,
        ),
        (HostedRetentionState::Retained, RetentionState::Retained),
        (
            HostedRetentionState::DeleteEligible,
            RetentionState::DeleteEligible,
        ),
    ] {
        assert_eq!(retention_state_from_dto(dto), domain);
        assert_eq!(retention_state_to_dto(domain), dto);
    }
}

#[test]
fn object_kind_dto_round_trips_blob_and_manifest() {
    for (dto, control, storage) in [
        (
            HostedObjectKind::Blob,
            ObjectKind::Blob,
            StorageObjectKind::WorkspaceFileV1,
        ),
        (
            HostedObjectKind::Manifest,
            ObjectKind::Manifest,
            StorageObjectKind::WorkspaceManifestV1,
        ),
    ] {
        assert_eq!(object_kind_from_dto(dto), control);
        assert_eq!(object_kind_to_dto(control), dto);
        assert_eq!(storage_object_kind_from_dto(dto), storage);
    }
    // The manifest-sync kinds serialize to their opaque wire values.
    assert_eq!(ObjectKind::Blob.as_str(), "blob");
    assert_eq!(ObjectKind::Manifest.as_str(), "manifest");
}

#[test]
fn object_pointer_to_dto_preserves_wire_fields() {
    let domain = ObjectPointer {
        object_key: blob_object_key("pointer-encode"),
        content_id: ContentId::new("cid_encode"),
        byte_len: 42,
        hash: "blake3:encode".to_string(),
        key_epoch: 3,
        kind: ObjectKind::Blob,
        created_at: ControlPlaneTimestamp {
            tick: 1_730_000_000_000,
        },
    };
    let dto = object_pointer_to_dto(&domain);
    assert_eq!(dto.object_key, blob_object_key("pointer-encode"));
    assert_eq!(dto.content_id, "cid_encode");
    assert_eq!(dto.byte_length, 42);
    assert_eq!(dto.key_epoch, 3);
    assert_eq!(dto.kind, HostedObjectKind::Blob);
}

#[test]
fn object_metadata_dto_maps_values_and_rejects_bad_key_and_timestamp() {
    let metadata = object_metadata_from_dto(metadata_dto()).expect("metadata");
    assert_eq!(metadata.kind, StorageObjectKind::WorkspaceFileV1);
    assert_eq!(metadata.byte_len, 256);
    assert_eq!(metadata.key_epoch, 9);
    assert_eq!(metadata.retention_state, RetentionState::Retained);
    assert_eq!(metadata.created_by_device_id, None);

    let mut bad_key = metadata_dto();
    bad_key.object_key = "../secret".to_string();
    assert!(matches!(
        object_metadata_from_dto(bad_key),
        Err(ControlPlaneError::InvalidObjectKey { .. })
    ));

    let mut bad_time = metadata_dto();
    bad_time.created_at = "nope".to_string();
    assert_parse_error_field(object_metadata_from_dto(bad_time), "createdAt");
}

#[test]
fn signed_url_dto_rejects_malformed_expiry() {
    assert_parse_error_field(
        signed_url_from_dto("https://example".to_string(), "nope"),
        "expiresAt",
    );
}
