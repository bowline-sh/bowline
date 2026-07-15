use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read as _,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_control_plane::{
    ControlPlaneError, FakeControlPlaneClient, ObjectControlPlaneClient as _, ObjectMetadataCommit,
    ObjectPointer, UploadIntentRequest, WorkspaceControlPlaneClient as _,
};
use bowline_core::{
    ids::{ContentId, DeviceId, PackId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, HydrationState, NamespaceEntry,
        NamespaceEntryKind, RefKind, SnapshotDraft, SnapshotKind, WorkspaceRef,
    },
};
use bowline_storage::{
    ByteStore, LocalByteStore, ObjectKind as StorageObjectKind, SourcePackUploadJournalEntry,
    SourcePackUploadJournalKey, SourcePackUploadJournalObjectHash, SourcePackUploadJournalPointer,
    parse_index,
};

use crate::{
    scanner::ScanReport,
    sync::{
        CandidateBase, FullScanReason, ScanScope, SnapshotContent, manifest_id_for_snapshot,
        rebuild_manifest_identity,
    },
};

use super::*;

#[path = "upload_reuse_tests/fixtures.rs"]
mod fixtures;

use fixtures::*;

#[test]
fn source_packs_written_payload_reports_reused_and_packed_counts() {
    let content_a = ContentId::new("cid_a");
    let content_b = ContentId::new("cid_b");
    let pack_id = PackId::new("pk_0011223344556677");
    let candidate = candidate_with_manifest(
        WorkspaceId::new("ws_upload"),
        manifest_for_entries(vec![
            file_entry("a.txt", content_a.clone()),
            file_entry_with_locator("b.txt", content_b.clone(), locator(content_b, pack_id, 0)),
        ]),
        BTreeMap::new(),
    );
    let reusable = content_layout_map_from_snapshot(&candidate.snapshot).expect("layout map");

    assert_eq!(
        reused_record_count(&candidate.snapshot, &reusable).expect("reused records"),
        1
    );
    assert_eq!(reused_pack_count(&reusable), 1);
}

#[test]
fn reused_locator_entries_skip_packing_and_commit_reused_pack_pointers() {
    let workspace_id = WorkspaceId::new("ws_reuse_mixed");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("mixed-reuse");
    let store = LocalByteStore::open_deterministic(&root, 21).expect("byte store");
    let changed_id = ContentId::new("cid_changed");
    let reused_id = ContentId::new("cid_reused");
    let reused_pack = PackId::new("pk_0011223344556677");
    let reused_key = ObjectKey::from_pack_id(&reused_pack).expect("pack key");
    commit_pointer(
        &control_plane,
        &workspace_id,
        &reused_key,
        reused_pack.as_str(),
        b"reused pack bytes",
        stable_hash(b"reused pack bytes"),
    );
    let candidate = candidate_with_manifest(
        workspace_id.clone(),
        manifest_for_entries(vec![
            file_entry("changed.txt", changed_id.clone()),
            file_entry_with_locator(
                "unchanged.txt",
                reused_id.clone(),
                locator(reused_id, reused_pack.clone(), 0),
            ),
        ]),
        BTreeMap::from([(changed_id, b"changed bytes".to_vec())]),
    );
    let mut checkpoints = Vec::new();

    let outcome = upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
        |step, payload| {
            checkpoints.push((step.to_string(), payload));
            Ok(())
        },
    )
    .expect("upload");

    let bound_snapshot = match outcome {
        UploadOutcome::Advanced {
            snapshot_root,
            bound_snapshot,
            ..
        } => {
            assert!(snapshot_root.complete);
            bound_snapshot.expect("bound snapshot")
        }
        UploadOutcome::Stale { .. } => panic!("workspace should advance"),
    };
    let bound_layouts = content_layout_map_from_snapshot(&bound_snapshot).expect("bound layouts");
    let bound_packs = bound_layouts
        .values()
        .flat_map(ContentLayout::segments)
        .map(|segment| segment.pack_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(bound_packs.len(), 2);
    assert!(bound_packs.contains(&reused_pack));
    let source_payload = checkpoints
        .iter()
        .find(|(step, _)| step == "source-packs-written")
        .map(|(_, payload)| payload)
        .expect("source payload");
    let source_payload =
        serde_json::from_str::<serde_json::Value>(source_payload).expect("source payload json");
    assert_eq!(source_payload["recordCount"], serde_json::json!(1));
    assert_eq!(source_payload["reusedRecordCount"], serde_json::json!(1));
    assert_eq!(
        source_payload["residentContentBytes"],
        serde_json::json!(13)
    );
    assert_eq!(source_payload["largestContentBytes"], serde_json::json!(13));
    assert_eq!(source_payload["packedInputBytes"], serde_json::json!(13));
    for field in [
        "residentContentBytes",
        "largestContentBytes",
        "packedInputBytes",
    ] {
        assert!(
            source_payload[field].is_u64(),
            "streaming telemetry must stay numeric"
        );
        assert!(
            !source_payload[field].to_string().contains('/'),
            "streaming telemetry must not expose paths"
        );
    }

    fs::remove_dir_all(root).expect("remove test root");
    assert_journaled_source_pack_retry_commits_without_reencrypting_pack();
}

fn assert_journaled_source_pack_retry_commits_without_reencrypting_pack() {
    let workspace_id = WorkspaceId::new("ws_journal_retry");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("journal-retry");
    let store = LocalByteStore::open_deterministic(&root, 31).expect("byte store");
    let storage_key = StorageKey::from_bytes([9_u8; 32]);
    let content_id = ContentId::new("cid_journaled");
    let bytes = b"journaled source bytes".to_vec();
    let candidate = candidate_with_manifest(
        workspace_id.clone(),
        manifest_for_entries(vec![file_entry("journaled.txt", content_id.clone())]),
        BTreeMap::from([(content_id.clone(), bytes.clone())]),
    );
    let prepared_content = candidate
        .snapshot
        .prepared_content()
        .get(&content_id)
        .expect("prepared journal content");
    let pack = prepare_segmented_source_packs(
        workspace_id.clone(),
        &[PackRecordReader {
            content_id: &content_id,
            source: prepared_content,
        }],
        &BTreeMap::new(),
        512,
        storage_key,
        1,
    )
    .expect("segmented source pack writes")
    .packs
    .pop()
    .expect("one pack");
    let mut pack_bytes = Vec::new();
    pack.0
        .spool
        .reader()
        .expect("open segmented source pack")
        .read_to_end(&mut pack_bytes)
        .expect("read segmented source pack");
    let metadata = store
        .put_object_with_content_id_at_epoch(
            pack.object_key().clone(),
            StorageObjectKind::SourcePack,
            pack.pack_id().as_str(),
            &pack_bytes,
            1,
            Some(&candidate.device_id),
        )
        .expect("journaled source pack object");
    let journal_key = SourcePackUploadJournalKey::new(
        workspace_id.clone(),
        candidate.snapshot.manifest.snapshot_id.clone(),
        1,
        candidate
            .snapshot
            .prepared_content()
            .iter()
            .map(|(content_id, content)| (content_id.clone(), content.logical_len)),
    );
    store
        .record_source_pack_upload_journal(
            &journal_key,
            &SourcePackUploadJournalEntry {
                pointer: SourcePackUploadJournalPointer {
                    object_key: metadata.key.clone(),
                    pack_id: pack.pack_id().clone(),
                    byte_len: metadata.byte_len,
                    hash: SourcePackUploadJournalObjectHash::from_stable_hash(
                        metadata.hash.clone(),
                    ),
                    key_epoch: metadata.key_epoch,
                    created_at_unix_ms: metadata.created_at_unix_ms,
                },
                locators: pack.locators().to_vec(),
            },
        )
        .expect("journal source pack");
    let mut checkpoints = Vec::new();

    let outcome = upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        storage_key,
        1,
        |step, payload| {
            checkpoints.push((step.to_string(), payload));
            Ok(())
        },
    )
    .expect("retry upload");

    let bound_snapshot = match outcome {
        UploadOutcome::Advanced {
            snapshot_root,
            bound_snapshot,
            ..
        } => {
            assert!(snapshot_root.complete);
            bound_snapshot.expect("bound snapshot")
        }
        UploadOutcome::Stale { .. } => panic!("workspace should advance"),
    };
    let bound_layouts = content_layout_map_from_snapshot(&bound_snapshot).expect("bound layouts");
    assert_eq!(bound_layouts.len(), 1);
    assert_eq!(
        bound_layouts
            .values()
            .flat_map(ContentLayout::segments)
            .next()
            .expect("bound segment")
            .pack_id,
        pack.pack_id().clone()
    );
    assert!(
        checkpoints
            .iter()
            .any(|(step, _)| step == "source-pack-journal-reused")
    );
    assert!(
        checkpoints
            .iter()
            .all(|(step, _)| step != "source-pack-uploaded")
    );
    let source_payload = checkpoints
        .iter()
        .find(|(step, _)| step == "source-packs-written")
        .map(|(_, payload)| payload)
        .expect("source payload");
    let source_payload =
        serde_json::from_str::<serde_json::Value>(source_payload).expect("source payload json");
    assert_eq!(source_payload["recordCount"], serde_json::json!(0));
    assert_eq!(source_payload["packedInputBytes"], serde_json::json!(0));
    let pack_keys = store
        .list_object_keys()
        .expect("object keys")
        .into_iter()
        .filter(|key| key.as_str().starts_with("packs_"))
        .collect::<Vec<_>>();
    assert_eq!(pack_keys, vec![pack.object_key().clone()]);

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn aborted_after_source_pack_upload_retries_from_journal() {
    let workspace_id = WorkspaceId::new("ws_journal_abort");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("journal-abort");
    let store = LocalByteStore::open_deterministic(&root, 35).expect("byte store");
    let storage_key = StorageKey::from_bytes([9_u8; 32]);
    let content_id = ContentId::new("cid_abort_journaled");
    let candidate = candidate_with_manifest(
        workspace_id.clone(),
        manifest_for_entries(vec![file_entry("abort.txt", content_id.clone())]),
        BTreeMap::from([(content_id, b"abort after source pack".to_vec())]),
    );

    let error = upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        storage_key,
        1,
        |step, _| {
            if step == "source-pack-uploaded" {
                return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
                    resource: "test checkpoint",
                    reason: "abort after source pack upload",
                }));
            }
            Ok(())
        },
    )
    .expect_err("test checkpoint aborts before manifest commit");
    assert!(matches!(
        error,
        UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "test checkpoint",
            ..
        })
    ));
    let pack_keys_after_abort = store
        .list_object_keys()
        .expect("object keys")
        .into_iter()
        .filter(|key| key.as_str().starts_with("packs_"))
        .collect::<Vec<_>>();
    assert_eq!(pack_keys_after_abort.len(), 1);

    let mut retry_checkpoints = Vec::new();
    let outcome = upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        storage_key,
        1,
        |step, payload| {
            retry_checkpoints.push((step.to_string(), payload));
            Ok(())
        },
    )
    .expect("retry reuses journaled source pack");
    let snapshot_root = match outcome {
        UploadOutcome::Advanced { snapshot_root, .. } => snapshot_root,
        UploadOutcome::Stale { .. } => panic!("workspace should advance"),
    };
    assert!(snapshot_root.complete);

    assert!(
        retry_checkpoints
            .iter()
            .any(|(step, _)| step == "source-pack-journal-reused")
    );
    assert!(
        retry_checkpoints
            .iter()
            .all(|(step, _)| step != "source-pack-uploaded")
    );
    assert_eq!(
        store
            .list_object_keys()
            .expect("object keys")
            .into_iter()
            .filter(|key| key.as_str().starts_with("packs_"))
            .collect::<Vec<_>>(),
        pack_keys_after_abort
    );
    let pack_metadata = store
        .head_object(&pack_keys_after_abort[0])
        .expect("journaled pack metadata");
    assert_ne!(pack_metadata.created_at_unix_ms, 0);
    let committed_pack = control_plane
        .head_object_metadata(&workspace_id, pack_keys_after_abort[0].as_str())
        .expect("committed journal pack");
    assert_eq!(committed_pack.hash, pack_metadata.hash);

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn lost_claim_at_cas_authorization_prevents_workspace_ref_advance() {
    let workspace_id = WorkspaceId::new("ws_cas_claim_fence");
    let control_plane = FakeControlPlaneClient::default();
    let initial = control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("cas-claim-fence");
    let store = LocalByteStore::open_deterministic(&root, 36).expect("byte store");
    let content_id = ContentId::new("cid_cas_claim_fence");
    let candidate = candidate_with_manifest(
        workspace_id.clone(),
        manifest_for_entries(vec![file_entry("fenced.txt", content_id.clone())]),
        BTreeMap::from([(content_id, b"fenced bytes".to_vec())]),
    );
    let mut reached_cas_boundary = false;

    let error = upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
        |step, _| {
            if step == "workspace-ref-cas-authorized" {
                reached_cas_boundary = true;
                return Err(UploadError::Checkpoint(
                    "sync operation claim ownership was lost".to_string(),
                ));
            }
            Ok(())
        },
    )
    .expect_err("lost ownership aborts before workspace ref CAS");

    assert!(reached_cas_boundary);
    assert!(matches!(error, UploadError::Checkpoint(_)));
    let current = control_plane
        .get_workspace_ref(&workspace_id)
        .expect("workspace ref")
        .expect("workspace exists");
    assert_eq!(current.version, initial.version);
    assert_eq!(current.snapshot_id, initial.snapshot_id);
}

#[test]
fn large_upload_uses_streamed_pack_path_end_to_end() {
    let workspace_id = WorkspaceId::new("ws_stream_end_to_end");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("stream-end-to-end");
    let store = LocalByteStore::open_deterministic(&root, 41).expect("byte store");
    let storage_key = StorageKey::from_bytes([9_u8; 32]);
    let content_id = ContentId::new("cid_stream_end_to_end");
    let bytes = deterministic_bytes((SOURCE_PACK_TARGET_BYTES as u64 + 1) as usize);
    let expected_segment_count = (bytes.len() as u64).div_ceil(packs::SOURCE_SEGMENT_BYTES);
    let candidate = candidate_with_manifest(
        workspace_id,
        manifest_for_entries(vec![file_entry("large.bin", content_id)]),
        BTreeMap::from([(ContentId::new("cid_stream_end_to_end"), bytes)]),
    );
    let mut checkpoints = Vec::new();

    upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        storage_key,
        1,
        |step, payload| {
            checkpoints.push((step.to_string(), payload));
            Ok(())
        },
    )
    .expect("large upload succeeds");

    let source_payload = checkpoints
        .iter()
        .find(|(step, _)| step == "source-packs-written")
        .map(|(_, payload)| payload)
        .expect("source payload");
    let source_payload =
        serde_json::from_str::<serde_json::Value>(source_payload).expect("source payload json");
    assert_eq!(source_payload["recordCount"], serde_json::json!(1));
    assert!(source_payload["packedInputBytes"].as_u64().unwrap() > SOURCE_PACK_TARGET_BYTES as u64);
    assert!(
        checkpoints
            .iter()
            .any(|(step, _)| step == "source-pack-uploaded")
    );
    let pack_keys = store
        .list_object_keys()
        .expect("object keys")
        .into_iter()
        .filter(|key| key.as_str().starts_with("packs_"))
        .collect::<Vec<_>>();
    let streamed_segment_count = pack_keys
        .iter()
        .map(|pack_key| {
            let pack_bytes = store.get_object(pack_key).expect("source pack bytes");
            parse_index(&pack_bytes)
                .expect("streamed source pack index parses")
                .records
                .len() as u64
        })
        .sum::<u64>();
    assert_eq!(streamed_segment_count, expected_segment_count);

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn snapshot_root_reuse_returns_bound_snapshot_for_persistence() {
    let workspace_id = WorkspaceId::new("ws_reuse_manifest");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("manifest-reuse-bound");
    let store = LocalByteStore::open_deterministic(&root, 25).expect("byte store");
    let changed_id = ContentId::new("cid_changed");
    let reused_id = ContentId::new("cid_reused");
    let reused_pack = PackId::new("pk_9988776655443322");
    let reused_key = ObjectKey::from_pack_id(&reused_pack).expect("pack key");
    commit_pointer(
        &control_plane,
        &workspace_id,
        &reused_key,
        reused_pack.as_str(),
        b"reused pack bytes",
        stable_hash(b"reused pack bytes"),
    );
    let candidate = candidate_with_manifest(
        workspace_id,
        manifest_for_entries(vec![
            file_entry("changed.txt", changed_id.clone()),
            file_entry_with_locator(
                "unchanged.txt",
                reused_id.clone(),
                locator(reused_id, reused_pack, 0),
            ),
        ]),
        BTreeMap::from([(changed_id, b"changed bytes".to_vec())]),
    );

    upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
    )
    .expect("first upload commits bound snapshot root");
    let retry = upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
    )
    .expect("second upload reuses snapshot root");

    let bound_snapshot = retry
        .bound_snapshot()
        .expect("root reuse should import the committed bound snapshot");
    let changed = bound_snapshot
        .entries_for_test()
        .into_iter()
        .find(|entry| entry.path == "changed.txt")
        .expect("changed entry");
    assert!(
        changed.content_layout.is_some(),
        "retry persistence needs the locator assigned during the first upload"
    );

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn unchanged_content_uploads_zero_source_pack_bytes() {
    let workspace_id = WorkspaceId::new("ws_reuse_all");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("all-reuse");
    let store = LocalByteStore::open_deterministic(&root, 22).expect("byte store");
    let content_id = ContentId::new("cid_reused");
    let pack_id = PackId::new("pk_8899aabbccddeeff");
    let reused_key = ObjectKey::from_pack_id(&pack_id).expect("pack key");
    commit_pointer(
        &control_plane,
        &workspace_id,
        &reused_key,
        pack_id.as_str(),
        b"reused pack bytes",
        stable_hash(b"reused pack bytes"),
    );
    let candidate = candidate_with_manifest(
        workspace_id,
        manifest_for_entries(vec![file_entry_with_locator(
            "unchanged.txt",
            content_id.clone(),
            locator(content_id, pack_id, 0),
        )]),
        BTreeMap::new(),
    );

    upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
    )
    .expect("upload");

    let object_keys = store.list_object_keys().expect("object keys");
    assert!(
        object_keys
            .iter()
            .all(|key| !key.as_str().starts_with("packs_")),
        "all-reused upload should not write source-pack bytes"
    );

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn missing_reused_pack_with_bytes_repacks_inline() {
    let workspace_id = WorkspaceId::new("ws_repack_reuse");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("repack-reuse");
    let store = LocalByteStore::open_deterministic(&root, 24).expect("byte store");
    let content_id = ContentId::new("cid_missing_repacked");
    let missing_pack = PackId::new("pk_1020304050607080");
    let candidate = candidate_with_manifest(
        workspace_id,
        manifest_for_entries(vec![file_entry_with_locator(
            "unchanged.txt",
            content_id.clone(),
            locator(content_id.clone(), missing_pack, 0),
        )]),
        BTreeMap::from([(content_id, b"available bytes".to_vec())]),
    );
    let mut checkpoints = Vec::new();

    upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
        |step, payload| {
            checkpoints.push((step.to_string(), payload));
            Ok(())
        },
    )
    .expect("upload repacks missing reused pack when bytes are present");

    assert!(
        checkpoints
            .iter()
            .any(|(step, _)| step == "source-pack-reuse-repacked")
    );
    assert!(
        store
            .list_object_keys()
            .expect("object keys")
            .iter()
            .any(|key| key.as_str().starts_with("packs_")),
        "repacked fallback should write a replacement source pack"
    );

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn metadata_present_unreadable_reused_pack_with_bytes_repacks_inline() {
    let workspace_id = WorkspaceId::new("ws_repack_unreadable");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("unreadable-reuse");
    let store = LocalByteStore::open_deterministic(&root, 27).expect("byte store");
    let content_id = ContentId::new("cid_unreadable_repacked");
    let stale_pack = PackId::new("pk_2030405060708090");
    let stale_key = ObjectKey::from_pack_id(&stale_pack).expect("pack key");
    commit_pointer(
        &control_plane,
        &workspace_id,
        &stale_key,
        stale_pack.as_str(),
        b"stale pack bytes",
        stable_hash(b"stale pack bytes"),
    );
    let candidate = candidate_with_manifest(
        workspace_id,
        manifest_for_entries(vec![file_entry_with_locator(
            "unchanged.txt",
            content_id.clone(),
            locator(content_id.clone(), stale_pack, 0),
        )]),
        BTreeMap::from([(content_id, b"available bytes".to_vec())]),
    );
    let mut checkpoints = Vec::new();

    upload_snapshot_candidate_with_checkpoints(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
        |step, payload| {
            checkpoints.push((step.to_string(), payload));
            Ok(())
        },
    )
    .expect("upload repacks unreadable reused pack when bytes are present");

    assert!(
        checkpoints
            .iter()
            .any(|(step, _)| step == "source-pack-reuse-repacked"),
        "metadata-present but unreadable reused packs must not be published when local bytes can repair them"
    );

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn missing_reused_pack_with_duplicate_content_repacks_unique_records() {
    let workspace_id = WorkspaceId::new("ws_repack_copies");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("repack-copies");
    let store = LocalByteStore::open_deterministic(&root, 26).expect("byte store");
    let content_id = ContentId::new("cid_copied_content");
    let missing_pack = PackId::new("pk_abcdefabcdefabcd");
    let candidate = candidate_with_manifest(
        workspace_id,
        manifest_for_entries(vec![
            file_entry_with_locator(
                "copy-a.txt",
                content_id.clone(),
                locator(content_id.clone(), missing_pack.clone(), 0),
            ),
            file_entry_with_locator(
                "copy-b.txt",
                content_id.clone(),
                locator(content_id.clone(), missing_pack, 0),
            ),
        ]),
        BTreeMap::from([(content_id, b"copied bytes".to_vec())]),
    );

    upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
    )
    .expect("upload repacks one record for copied content");

    let replacement_pack = store
        .list_object_keys()
        .expect("object keys")
        .into_iter()
        .find(|key| key.as_str().starts_with("packs_"))
        .expect("replacement pack");
    let replacement_bytes = store
        .get_object(&replacement_pack)
        .expect("replacement pack bytes");
    parse_index(&replacement_bytes).expect("replacement pack has unique content IDs");

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn missing_reused_pack_without_bytes_returns_typed_error() {
    let workspace_id = WorkspaceId::new("ws_missing_reuse");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("missing-reuse");
    let store = LocalByteStore::open_deterministic(&root, 23).expect("byte store");
    let content_id = ContentId::new("cid_missing_reused");
    let pack_id = PackId::new("pk_0123456789abcdef");
    let candidate = candidate_with_manifest(
        workspace_id,
        manifest_for_entries(vec![file_entry_with_locator(
            "unchanged.txt",
            content_id.clone(),
            locator(content_id, pack_id.clone(), 0),
        )]),
        BTreeMap::new(),
    );

    let error = upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &store,
        StorageKey::from_bytes([9_u8; 32]),
        1,
    )
    .expect_err("missing reused pack without bytes must ask runner to retry");

    assert!(
        matches!(error, UploadError::ReusedPackMissing { pack_id: missing } if missing == pack_id)
    );

    fs::remove_dir_all(root).expect("remove test root");
}
