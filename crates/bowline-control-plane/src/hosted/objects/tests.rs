use super::proof::generated_object_key;
use super::*;

fn gc_dto(key: String) -> HostedStorageGcObjectRef {
    HostedStorageGcObjectRef {
        key,
        retention_state: HostedRetentionState::Retained,
        referenced_by_current_head: false,
        referenced_by_snapshot: Some("snapshot_1".to_string()),
        referenced_by_work_view_base: true,
        referenced_by_active_overlay: false,
        referenced_by_active_lease: true,
        referenced_by_conflict_bundle: true,
        verified: true,
    }
}

fn object_pointer_dto(kind: HostedObjectKind) -> HostedObjectPointer {
    HostedObjectPointer {
        object_key: "packs_pk_boundary".to_string(),
        content_id: "cid_boundary".to_string(),
        byte_length: 128,
        hash: "blake3:boundary".to_string(),
        key_epoch: 7,
        kind,
        created_at: "2026-06-23T12:00:00Z".to_string(),
    }
}

fn metadata_dto() -> HostedObjectMetadata {
    let key = generated_object_key(ObjectKind::SourcePack, "metadata-boundary");
    HostedObjectMetadata {
        byte_length: 256,
        content_id: Some("cid_metadata".to_string()),
        created_at: "2026-06-23T12:00:00Z".to_string(),
        hash: "blake3:metadata".to_string(),
        key_epoch: 9,
        kind: HostedObjectKind::SourcePack,
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
    let key = generated_object_key(ObjectKind::SourcePack, "gc-boundary");
    let reference = storage_gc_object_ref_from_dto(gc_dto(key)).expect("valid gc reference");
    assert_eq!(reference.retention_state, RetentionState::Retained);
    assert!(reference.referenced_by_active_lease);
    assert!(reference.referenced_by_conflict_bundle);
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
fn object_kind_dto_round_trips_and_maps_overlay_alias() {
    for (dto, control, storage) in [
        (
            HostedObjectKind::SourcePack,
            ObjectKind::SourcePack,
            StorageObjectKind::SourcePack,
        ),
        (
            HostedObjectKind::LocatorIndex,
            ObjectKind::LocatorIndex,
            StorageObjectKind::LocatorIndex,
        ),
        (
            HostedObjectKind::SnapshotManifest,
            ObjectKind::SnapshotManifest,
            StorageObjectKind::SnapshotManifest,
        ),
        (
            HostedObjectKind::SnapshotMetadataPage,
            ObjectKind::SnapshotMetadataPage,
            StorageObjectKind::SnapshotMetadataPage,
        ),
        (
            HostedObjectKind::AgentOverlay,
            ObjectKind::AgentOverlay,
            StorageObjectKind::AgentOverlay,
        ),
        (
            HostedObjectKind::ConflictBundle,
            ObjectKind::ConflictBundle,
            StorageObjectKind::ConflictBundle,
        ),
    ] {
        assert_eq!(object_kind_from_dto(dto), control);
        assert_eq!(object_kind_to_dto(control), dto);
        assert_eq!(storage_object_kind_from_dto(dto), storage);
    }
    // AgentOverlay must serialize to the canonical wire value the proof
    // subjects and server both expect.
    assert_eq!(ObjectKind::AgentOverlay.as_str(), "overlay-pack");
}

#[test]
fn object_pointer_dto_maps_identity_and_rejects_malformed_timestamp() {
    let pointer = object_pointer_from_dto(object_pointer_dto(HostedObjectKind::LocatorIndex))
        .expect("pointer");
    assert_eq!(pointer.object_key, "packs_pk_boundary");
    assert_eq!(pointer.content_id.as_str(), "cid_boundary");
    assert_eq!(pointer.byte_len, 128);
    assert_eq!(pointer.key_epoch, 7);
    assert_eq!(pointer.kind, ObjectKind::LocatorIndex);

    let mut malformed = object_pointer_dto(HostedObjectKind::SourcePack);
    malformed.created_at = "not-a-timestamp".to_string();
    assert_parse_error_field(object_pointer_from_dto(malformed), "createdAt");
}

#[test]
fn object_pointer_to_dto_preserves_wire_fields() {
    let domain = ObjectPointer {
        object_key: "packs_pk_encode".to_string(),
        content_id: ContentId::new("cid_encode"),
        byte_len: 42,
        hash: "blake3:encode".to_string(),
        key_epoch: 3,
        kind: ObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp {
            tick: 1_730_000_000_000,
        },
    };
    let dto = object_pointer_to_dto(&domain);
    assert_eq!(dto.object_key, "packs_pk_encode");
    assert_eq!(dto.content_id, "cid_encode");
    assert_eq!(dto.byte_length, 42);
    assert_eq!(dto.key_epoch, 3);
    assert_eq!(dto.kind, HostedObjectKind::AgentOverlay);
}

#[test]
fn object_metadata_dto_maps_values_and_rejects_bad_key_and_timestamp() {
    let metadata = object_metadata_from_dto(metadata_dto()).expect("metadata");
    assert_eq!(metadata.kind, StorageObjectKind::SourcePack);
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
