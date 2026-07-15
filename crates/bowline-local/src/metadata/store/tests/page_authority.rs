use bowline_core::{
    ids::{ManifestDigest, NamespacePageId},
    workspace_graph::SnapshotKind,
};

use super::*;
use crate::metadata::{
    ConflictSnapshotRetention, LocalMetadataRetentionPolicy, MetadataCacheRecord,
    MetadataCacheState, MetadataGcPhase, MetadataLogicalId, MetadataObjectBindingRecord,
    MetadataObjectKey, MetadataRecordKind, MetadataRecordRef, MetadataVerificationState,
    SnapshotPinId, SnapshotPinOwner, SnapshotPinOwnerKind, SnapshotPinReason, SnapshotPinRecord,
    SnapshotRecord, WorkspaceSyncHeadRecord,
};

const OLD: &str = "2026-07-01T00:00:00Z";
const NOW: &str = "2026-07-14T00:00:00Z";

#[test]
fn metadata_identity_key_round_trips_and_is_immutable_per_workspace() {
    let (mut store, _temp, workspace_id) = authority_store("identity-key");
    assert_eq!(
        store
            .metadata_identity_key(&workspace_id)
            .expect("missing key lookup"),
        None
    );
    store
        .register_metadata_identity_key(&workspace_id, [41; 32], OLD)
        .expect("register identity key");
    store
        .register_metadata_identity_key(&workspace_id, [41; 32], NOW)
        .expect("idempotent identity key registration");
    assert_eq!(
        store
            .metadata_identity_key(&workspace_id)
            .expect("identity key lookup"),
        Some([41; 32])
    );
    assert!(matches!(
        store
            .register_metadata_identity_key(&workspace_id, [42; 32], NOW)
            .expect_err("identity key replacement refused"),
        MetadataError::ImmutableBindingConflict {
            field: "metadata_identity_key",
            ..
        }
    ));
}

#[test]
fn immutable_binding_reuses_exact_pointer_and_rejects_conflicts() {
    let (store, _temp, workspace_id) = authority_store("binding");
    let binding = binding(&workspace_id, "page_root", "metadata_root", OLD);
    assert_eq!(
        store
            .insert_metadata_object_binding(&binding)
            .expect("first binding"),
        binding
    );
    assert_eq!(
        store
            .insert_metadata_object_binding(&binding)
            .expect("idempotent binding"),
        binding
    );
    let mut conflict = binding.clone();
    conflict.object_key = MetadataObjectKey::new("metadata_other");
    assert!(matches!(
        store
            .insert_metadata_object_binding(&conflict)
            .expect_err("pointer replacement refused"),
        MetadataError::ImmutableBindingConflict {
            field: "object_key",
            ..
        }
    ));
}

#[test]
fn snapshot_root_requires_verified_bindings_and_cache_metadata_round_trips() {
    let (mut store, _temp, workspace_id) = authority_store("root");
    let snapshot_id = SnapshotId::new("snap_root");
    let root_id = NamespacePageId::new("page_root");
    let mut root_binding = binding(&workspace_id, root_id.as_str(), "metadata_root", OLD);
    root_binding.verification_state = MetadataVerificationState::Unverified;
    root_binding.verified_at = None;
    store
        .insert_metadata_object_binding(&root_binding)
        .expect("unverified binding");
    let snapshot = snapshot(&workspace_id, &snapshot_id, &root_id);
    assert!(matches!(
        store
            .commit_snapshot_root(&snapshot, &[], NOW)
            .expect_err("unverified root refused"),
        MetadataError::IncompleteSnapshotRoot { .. }
    ));
    let root_ref = record(MetadataRecordKind::NamespacePage, root_id.as_str());
    store
        .set_metadata_binding_verification(
            &workspace_id,
            &root_ref,
            MetadataVerificationState::Verified,
            Some(NOW),
        )
        .expect("verify root");
    store
        .commit_snapshot_root(&snapshot, &[], NOW)
        .expect("commit root");
    assert!(
        store
            .snapshot_root_completeness(&workspace_id, &snapshot_id)
            .expect("completeness")
            .complete
    );
    let cache = MetadataCacheRecord {
        workspace_id: workspace_id.clone(),
        logical_id: MetadataLogicalId::new(root_id.as_str()),
        kind: MetadataRecordKind::NamespacePage,
        cache_path: Some("cache/pages/root".to_string()),
        encoded_bytes: 512,
        state: MetadataCacheState::Present,
        last_accessed_at: NOW.to_string(),
    };
    store
        .put_metadata_cache_record(&cache)
        .expect("cache metadata");
    assert_eq!(
        store
            .metadata_cache_record(&workspace_id, &root_ref)
            .expect("cache read"),
        Some(cache)
    );
}

#[test]
fn snapshot_root_retry_keeps_first_commit_timestamp_for_same_immutable_identity() {
    let (mut store, _temp, workspace_id) = authority_store("root-retry");
    let snapshot_id = SnapshotId::new("snap_root_retry");
    let root_id = NamespacePageId::new("page_root_retry");
    store
        .insert_metadata_object_binding(&binding(
            &workspace_id,
            root_id.as_str(),
            "metadata_root_retry",
            OLD,
        ))
        .expect("root binding");
    let first = snapshot(&workspace_id, &snapshot_id, &root_id);
    let committed = store
        .commit_snapshot_root(&first, &[], OLD)
        .expect("first root commit");

    let mut retry = first.clone();
    retry.created_at = NOW.to_string();
    assert_eq!(
        store
            .commit_snapshot_root(&retry, &[], NOW)
            .expect("idempotent retry"),
        committed
    );
    assert_eq!(
        store
            .snapshot(&workspace_id, &snapshot_id)
            .expect("snapshot lookup")
            .expect("snapshot")
            .created_at,
        OLD
    );
}

#[test]
fn metadata_record_children_are_typed_and_deterministically_ordered() {
    let (mut store, _temp, workspace_id) = authority_store("children");
    let parent = record(MetadataRecordKind::NamespacePage, "page_parent");
    let namespace_child = record(MetadataRecordKind::NamespacePage, "page_z");
    let layout_child = record(MetadataRecordKind::ContentLayout, "layout_a");
    for (record, object_key) in [
        (&parent, "metadata_parent"),
        (&namespace_child, "metadata_page_z"),
        (&layout_child, "metadata_layout_a"),
    ] {
        let mut binding = binding(&workspace_id, record.logical_id.as_str(), object_key, OLD);
        binding.kind = record.kind;
        store
            .insert_metadata_object_binding(&binding)
            .expect("binding");
    }
    store
        .replace_metadata_record_edges(
            &workspace_id,
            &parent,
            &[namespace_child.clone(), layout_child.clone()],
        )
        .expect("edges");

    assert_eq!(
        store
            .metadata_record_children(&workspace_id, &parent)
            .expect("typed children"),
        vec![layout_child, namespace_child]
    );
}

#[test]
fn locally_verified_cache_can_precede_an_optional_hosted_binding() {
    let (mut store, _temp, workspace_id) = authority_store("local-cache-root");
    let snapshot_id = SnapshotId::new("snap_local_cache");
    let root_id = NamespacePageId::new("page_local_cache");
    let root = record(MetadataRecordKind::NamespacePage, root_id.as_str());
    store
        .put_metadata_cache_record(&cache(&workspace_id, &root, OLD))
        .expect("verified local record cache");

    store
        .commit_snapshot_root(&snapshot(&workspace_id, &snapshot_id, &root_id), &[], NOW)
        .expect("local canonical root");

    assert!(
        store
            .snapshot_root_completeness(&workspace_id, &snapshot_id)
            .expect("completeness")
            .complete
    );
    assert!(
        store
            .metadata_object_binding(&workspace_id, root.kind, &root.logical_id)
            .expect("optional binding")
            .is_none()
    );
}

#[test]
fn local_gc_removes_unbound_cache_records_without_a_remote_delete_intent() {
    let (mut store, temp, workspace_id) = authority_store("local-cache-gc");
    let orphan = record(MetadataRecordKind::NamespacePage, "page_local_orphan");
    let cache_root = temp.root().join("metadata-pages");
    fs::create_dir_all(&cache_root).expect("cache root");
    let cache_path = cache_root.join("page_local_orphan.page");
    fs::write(&cache_path, vec![0_u8; 512]).expect("cached page");
    store
        .put_metadata_cache_record(&MetadataCacheRecord {
            cache_path: Some(cache_path.display().to_string()),
            ..cache(&workspace_id, &orphan, OLD)
        })
        .expect("local orphan cache");
    store
        .start_metadata_gc(&workspace_id, "local-generation", NOW, NOW)
        .expect("start");

    for _ in 0..4 {
        if store
            .run_metadata_gc_batch(&workspace_id, 1, NOW)
            .expect("GC batch")
            .complete
        {
            break;
        }
    }

    assert!(
        store
            .metadata_cache_record(&workspace_id, &orphan)
            .expect("cache lookup")
            .is_none()
    );
    assert!(!cache_path.exists());
}

#[test]
fn local_gc_never_deletes_cache_bytes_inside_an_outer_transaction() {
    let (mut store, temp, workspace_id) = authority_store("local-cache-gc-transaction");
    let orphan = record(MetadataRecordKind::NamespacePage, "page_local_transaction");
    let cache_root = temp.root().join("metadata-pages");
    fs::create_dir_all(&cache_root).expect("cache root");
    let cache_path = cache_root.join("page_local_transaction.page");
    fs::write(&cache_path, vec![0_u8; 512]).expect("cached page");
    store
        .put_metadata_cache_record(&MetadataCacheRecord {
            cache_path: Some(cache_path.display().to_string()),
            ..cache(&workspace_id, &orphan, OLD)
        })
        .expect("local orphan cache");
    store
        .start_metadata_gc(&workspace_id, "transaction-generation", NOW, NOW)
        .expect("start");
    store
        .run_metadata_gc_batch(&workspace_id, 1, NOW)
        .expect("advance to sweep");

    let error = store
        .with_committed(|store| store.run_metadata_gc_batch(&workspace_id, 1, NOW))
        .expect_err("nested filesystem finalization rejected");
    assert!(matches!(error, MetadataError::InvalidStorageMetadata(_)));
    assert!(cache_path.exists());
    assert!(
        store
            .metadata_cache_record(&workspace_id, &orphan)
            .expect("cache lookup")
            .is_some()
    );

    store
        .run_metadata_gc_batch(&workspace_id, 1, NOW)
        .expect("top-level sweep");
    assert!(!cache_path.exists());
    assert!(
        store
            .metadata_cache_record(&workspace_id, &orphan)
            .expect("cache lookup")
            .is_none()
    );
}

#[test]
fn local_gc_retries_a_durable_deleting_cache_after_filesystem_failure() {
    let (mut store, temp, workspace_id) = authority_store("local-cache-gc-retry");
    let orphan = record(MetadataRecordKind::NamespacePage, "page_local_retry");
    let cache_root = temp.root().join("metadata-pages");
    fs::create_dir_all(&cache_root).expect("cache root");
    let cache_path = cache_root.join("page_local_retry.page");
    fs::write(&cache_path, vec![0_u8; 512]).expect("cached page");
    store
        .put_metadata_cache_record(&MetadataCacheRecord {
            cache_path: Some(cache_path.display().to_string()),
            ..cache(&workspace_id, &orphan, OLD)
        })
        .expect("local orphan cache");
    store
        .start_metadata_gc(&workspace_id, "retry-generation", NOW, NOW)
        .expect("start");
    store
        .run_metadata_gc_batch(&workspace_id, 1, NOW)
        .expect("advance to sweep");

    super::super::metadata_gc::set_metadata_cache_delete_fault(&cache_path, true);
    assert!(matches!(
        store
            .run_metadata_gc_batch(&workspace_id, 1, NOW)
            .expect_err("injected filesystem failure"),
        MetadataError::Io(_)
    ));
    assert!(cache_path.exists());
    assert_eq!(
        store
            .metadata_cache_record(&workspace_id, &orphan)
            .expect("cache lookup")
            .expect("durable cache row")
            .state,
        MetadataCacheState::Deleting
    );
    assert!(
        store
            .metadata_gc_checkpoint(&workspace_id)
            .expect("checkpoint")
            .expect("checkpoint row")
            .sweep_cursor
            .is_none()
    );

    super::super::metadata_gc::set_metadata_cache_delete_fault(&cache_path, false);
    store
        .run_metadata_gc_batch(&workspace_id, 1, NOW)
        .expect("retry durable deletion");
    assert!(!cache_path.exists());
    assert!(
        store
            .metadata_cache_record(&workspace_id, &orphan)
            .expect("cache lookup")
            .is_none()
    );
}

#[test]
fn local_gc_refuses_cache_paths_outside_the_metadata_cache_root() {
    let (mut store, temp, workspace_id) = authority_store("local-cache-escape");
    let orphan = record(MetadataRecordKind::NamespacePage, "page_escape");
    fs::create_dir_all(temp.root().join("metadata-pages")).expect("cache root");
    let external = temp.root().join("external.page");
    fs::write(&external, b"must survive").expect("external file");
    store
        .put_metadata_cache_record(&MetadataCacheRecord {
            workspace_id: workspace_id.clone(),
            logical_id: orphan.logical_id.clone(),
            kind: orphan.kind,
            cache_path: Some(external.display().to_string()),
            encoded_bytes: 12,
            state: MetadataCacheState::Present,
            last_accessed_at: OLD.to_string(),
        })
        .expect("unsafe cache metadata fixture");
    store
        .start_metadata_gc(&workspace_id, "escape-generation", NOW, NOW)
        .expect("start");
    store
        .run_metadata_gc_batch(&workspace_id, 1, NOW)
        .expect("advance to sweep");
    assert!(matches!(
        store
            .run_metadata_gc_batch(&workspace_id, 1, NOW)
            .expect_err("outside cache path rejected"),
        MetadataError::InvalidStorageMetadata(_)
    ));
    assert_eq!(
        fs::read(&external).expect("external survives"),
        b"must survive"
    );
    assert!(
        store
            .metadata_cache_record(&workspace_id, &orphan)
            .expect("cache lookup")
            .is_some()
    );
}

#[test]
fn pins_drive_bounded_resumable_mark_and_sweep() {
    let (mut store, temp, workspace_id) = authority_store("gc");
    let snapshot_id = SnapshotId::new("snap_gc");
    let root_id = NamespacePageId::new("page_root");
    for binding in [
        binding(&workspace_id, "page_root", "metadata_root", OLD),
        binding(&workspace_id, "page_child", "metadata_child", OLD),
        binding(&workspace_id, "page_orphan", "metadata_orphan", OLD),
    ] {
        store
            .insert_metadata_object_binding(&binding)
            .expect("binding");
    }
    let snapshot = snapshot(&workspace_id, &snapshot_id, &root_id);
    store
        .commit_snapshot_root(&snapshot, &[], NOW)
        .expect("snapshot root");
    store
        .replace_metadata_record_edges(
            &workspace_id,
            &record(MetadataRecordKind::NamespacePage, "page_root"),
            &[record(MetadataRecordKind::NamespacePage, "page_child")],
        )
        .expect("edges");
    let cache_root = temp.root().join("metadata-pages");
    fs::create_dir_all(&cache_root).expect("cache root");
    let orphan_cache_path = cache_root.join("page_orphan.page");
    fs::write(&orphan_cache_path, vec![0_u8; 512]).expect("orphan cache");
    store
        .put_metadata_cache_record(&MetadataCacheRecord {
            workspace_id: workspace_id.clone(),
            logical_id: MetadataLogicalId::new("page_orphan"),
            kind: MetadataRecordKind::NamespacePage,
            cache_path: Some(orphan_cache_path.display().to_string()),
            encoded_bytes: 512,
            state: MetadataCacheState::Present,
            last_accessed_at: OLD.to_string(),
        })
        .expect("orphan cache record");
    store
        .acquire_snapshot_pin(&SnapshotPinRecord {
            id: SnapshotPinId::new("pin_head"),
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            root_id,
            reason: SnapshotPinReason::WorkspaceRef,
            owner: SnapshotPinOwner {
                kind: SnapshotPinOwnerKind::WorkspaceRef,
                id: "main".to_string(),
            },
            expires_at: None,
            created_at: NOW.to_string(),
        })
        .expect("pin");
    store
        .start_metadata_gc(&workspace_id, "generation-1", NOW, NOW)
        .expect("start");

    for _ in 0..10 {
        let report = store
            .run_metadata_gc_batch(&workspace_id, 1, NOW)
            .expect("GC batch");
        if report.complete {
            break;
        }
    }
    assert_eq!(
        store
            .metadata_gc_checkpoint(&workspace_id)
            .expect("checkpoint")
            .expect("checkpoint row")
            .phase,
        MetadataGcPhase::Complete
    );
    drop(store);
    let mut store =
        MetadataStore::open(temp.root().join("metadata.sqlite3")).expect("restart metadata store");
    let candidates = store
        .metadata_gc_delete_candidates(&workspace_id, 10)
        .expect("resume persisted delete candidates");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].record.logical_id.as_str(), "page_orphan");
    assert!(
        store
            .finalize_metadata_gc_candidate(&workspace_id, &candidates[0])
            .expect("finalize")
            .metadata_record_deleted
    );
    assert!(!orphan_cache_path.exists());
    assert!(
        store
            .metadata_object_binding(
                &workspace_id,
                MetadataRecordKind::NamespacePage,
                &MetadataLogicalId::new("page_root"),
            )
            .expect("root binding")
            .is_some()
    );
}

#[test]
fn retention_pins_workspace_conflict_and_count_or_age_history_deterministically() {
    let (mut store, _temp, workspace_id) = authority_store("retention-policy");
    let old = committed_snapshot(
        &mut store,
        &workspace_id,
        "snap_history_old",
        "page_history_old",
        "2026-07-01T00:00:00Z",
    );
    let counted = committed_snapshot(
        &mut store,
        &workspace_id,
        "snap_history_counted",
        "page_history_counted",
        "2026-07-10T00:00:00Z",
    );
    let recent = committed_snapshot(
        &mut store,
        &workspace_id,
        "snap_history_recent",
        "page_history_recent",
        "2026-07-13T12:00:00Z",
    );
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 7,
                snapshot_id: recent.id.clone(),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 7 },
                updated_by_device_id: None,
            },
            observed_at: NOW.to_string(),
        })
        .expect("workspace head");
    let policy = LocalMetadataRetentionPolicy {
        restore_point_retention_days: 1,
        restore_point_min_keep: 2,
        snapshot_gc_grace_days: 0,
        snapshot_delete_batch: 16,
        metadata_gc_batch: 1,
        metadata_cache_delete_batch: 1,
        ..LocalMetadataRetentionPolicy::default()
    };
    let conflicts = [ConflictSnapshotRetention {
        conflict_id: "conflict_retained".to_string(),
        base_snapshot_id: Some(old.id.clone()),
        remote_snapshot_id: None,
    }];

    let first = store
        .maintain_snapshot_retention(&workspace_id, &conflicts, &policy, NOW)
        .expect("first maintenance");
    let pins = store
        .active_snapshot_pins(&workspace_id, NOW)
        .expect("active pins");
    assert_eq!(first.active_pins, 4);
    assert_eq!(first.pins_acquired, 4);
    assert!(pins.iter().any(|pin| {
        pin.reason == SnapshotPinReason::WorkspaceRef && pin.snapshot_id == recent.id
    }));
    assert!(
        pins.iter()
            .any(|pin| { pin.reason == SnapshotPinReason::Conflict && pin.snapshot_id == old.id })
    );
    assert!(pins.iter().any(|pin| {
        pin.reason == SnapshotPinReason::ExplicitHistory && pin.snapshot_id == counted.id
    }));

    let second = store
        .maintain_snapshot_retention(&workspace_id, &conflicts, &policy, NOW)
        .expect("deterministic maintenance retry");
    assert_eq!(second.pins_acquired, 0);
    assert_eq!(second.pins_updated, 0);
    assert_eq!(second.pins_released, 0);

    let released = store
        .maintain_snapshot_retention(&workspace_id, &[], &policy, NOW)
        .expect("release terminal conflict ownership");
    assert_eq!(released.pins_released, 1);
}

#[test]
fn retention_starts_a_new_gc_generation_when_the_completed_cutoff_advances() {
    let (mut store, _temp, workspace_id) = authority_store("retention-next-generation");
    store
        .start_metadata_gc(
            &workspace_id,
            "completed-generation",
            "2026-07-13T00:00:00Z",
            "2026-07-13T00:00:00Z",
        )
        .expect("start old generation");
    store
        .run_metadata_gc_batch(&workspace_id, 1, "2026-07-13T00:00:00Z")
        .expect("advance old generation to sweep");
    assert!(
        store
            .run_metadata_gc_batch(&workspace_id, 1, "2026-07-13T00:00:00Z")
            .expect("complete old generation")
            .complete
    );

    let policy = LocalMetadataRetentionPolicy {
        snapshot_gc_grace_days: 0,
        metadata_gc_batch: 1,
        ..LocalMetadataRetentionPolicy::default()
    };
    store
        .maintain_snapshot_retention(&workspace_id, &[], &policy, NOW)
        .expect("maintenance with advanced cutoff");
    let checkpoint = store
        .metadata_gc_checkpoint(&workspace_id)
        .expect("checkpoint")
        .expect("new generation");
    assert_ne!(checkpoint.generation, "completed-generation");
    assert_eq!(checkpoint.grace_before, NOW);
    assert_ne!(checkpoint.phase, MetadataGcPhase::Complete);
}

#[test]
fn snapshot_delete_revalidates_live_head_even_before_pin_reconciliation() {
    let (mut store, _temp, workspace_id) = authority_store("retention-live-head");
    let live = committed_snapshot(
        &mut store,
        &workspace_id,
        "snap_live_old",
        "page_live_old",
        OLD,
    );
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: bowline_control_plane::WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 8,
                snapshot_id: live.id.clone(),
                updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 8 },
                updated_by_device_id: None,
            },
            observed_at: NOW.to_string(),
        })
        .expect("live workspace head");

    assert!(
        store
            .delete_unpinned_snapshots_batch(&workspace_id, NOW, 16, NOW)
            .expect("safe delete selection")
            .is_empty()
    );
    assert!(
        store
            .snapshot(&workspace_id, &live.id)
            .expect("live snapshot query")
            .is_some()
    );
}

fn authority_store(name: &str) -> (MetadataStore, TempWorkspace, WorkspaceId) {
    let temp = TempWorkspace::new(name).expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("metadata.sqlite3")).expect("metadata");
    let workspace_id = WorkspaceId::new(format!("ws_{name}"));
    store
        .insert_workspace(&workspace_id, "Workspace", OLD)
        .expect("workspace");
    (store, temp, workspace_id)
}

fn binding(
    workspace_id: &WorkspaceId,
    logical_id: &str,
    object_key: &str,
    created_at: &str,
) -> MetadataObjectBindingRecord {
    MetadataObjectBindingRecord {
        workspace_id: workspace_id.clone(),
        logical_id: MetadataLogicalId::new(logical_id),
        kind: MetadataRecordKind::NamespacePage,
        object_key: MetadataObjectKey::new(object_key),
        byte_len: 512,
        object_hash: format!("hash_{object_key}"),
        key_epoch: 1,
        verification_state: MetadataVerificationState::Verified,
        created_at: created_at.to_string(),
        verified_at: Some(created_at.to_string()),
    }
}

fn snapshot(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    root_id: &NamespacePageId,
) -> SnapshotRecord {
    SnapshotRecord {
        id: snapshot_id.clone(),
        workspace_id: workspace_id.clone(),
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        root_id: root_id.clone(),
        semantic_manifest_digest: ManifestDigest::new("digest_root"),
        entry_count: 2,
        refs: Vec::new(),
        created_at: OLD.to_string(),
    }
}

fn record(kind: MetadataRecordKind, logical_id: &str) -> MetadataRecordRef {
    MetadataRecordRef {
        kind,
        logical_id: MetadataLogicalId::new(logical_id),
    }
}

fn cache(
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
    last_accessed_at: &str,
) -> MetadataCacheRecord {
    MetadataCacheRecord {
        workspace_id: workspace_id.clone(),
        logical_id: record.logical_id.clone(),
        kind: record.kind,
        cache_path: Some(format!("cache/{}", record.logical_id.as_str())),
        encoded_bytes: 512,
        state: MetadataCacheState::Present,
        last_accessed_at: last_accessed_at.to_string(),
    }
}

fn committed_snapshot(
    store: &mut MetadataStore,
    workspace_id: &WorkspaceId,
    snapshot_id: &str,
    root_id: &str,
    created_at: &str,
) -> SnapshotRecord {
    let root_id = NamespacePageId::new(root_id);
    store
        .insert_metadata_object_binding(&binding(
            workspace_id,
            root_id.as_str(),
            &format!("metadata_{}", root_id.as_str()),
            created_at,
        ))
        .expect("snapshot root binding");
    let mut snapshot = snapshot(workspace_id, &SnapshotId::new(snapshot_id), &root_id);
    snapshot.semantic_manifest_digest = ManifestDigest::new(format!("digest_{snapshot_id}"));
    snapshot.created_at = created_at.to_string();
    store
        .commit_snapshot_root(&snapshot, &[], created_at)
        .expect("snapshot root");
    snapshot
}
