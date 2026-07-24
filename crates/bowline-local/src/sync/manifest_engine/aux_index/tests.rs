//! Aux-index tests (Plan 112 Step 2): canonical round-trip, seal/open through the
//! object store while riding a manifest entry, substitution resistance, and the
//! bounded-decode hygiene the manifest decoder also enforces.

use std::collections::BTreeMap;

use bowline_core::ids::{ContentId, DeviceId, ProjectId};

use super::*;
use crate::sync::manifest_engine::engine_test_support::{KEY_BYTES, test_crypto};
use crate::sync::manifest_engine::manifest::{
    KeyEpoch, Manifest, ManifestEntry, ManifestKey, WorkspaceCrypto, WorkspacePath,
};
use crate::sync::manifest_engine::push::RemoteObjects;

fn sample_index() -> AuxIndex {
    let mut aux = AuxIndex::empty();
    aux.upsert(
        WorkViewId::new("wv_beta"),
        sample_record("beta", WorkViewLifecycle::Active),
    );
    aux.upsert(
        WorkViewId::new("wv_alpha"),
        sample_record("alpha", WorkViewLifecycle::Accepted),
    );
    aux
}

fn sample_record(name: &str, lifecycle: WorkViewLifecycle) -> WorkViewRecord {
    WorkViewRecord {
        project_id: ProjectId::new(format!("proj_{name}")),
        project_path: format!("apps/{name}"),
        name: name.to_string(),
        owner_device_id: DeviceId::new("dev_test"),
        created_at: "2026-07-23T00:00:00Z".to_string(),
        updated_at: "2026-07-23T00:00:00Z".to_string(),
        base_manifest_key: ManifestKey::new(format!("m_base_{name}")),
        overlay_manifest_key: ManifestKey::new(format!("m_overlay_{name}")),
        lifecycle,
    }
}

#[test]
fn canonical_round_trip_preserves_records() {
    let aux = sample_index();
    let bytes = aux.to_canonical_bytes().expect("encode");
    let decoded = decode_aux_index_plaintext(&bytes, &AuxDecodeLimits::default()).expect("decode");
    assert_eq!(decoded, aux);
}

#[test]
fn canonical_round_trip_allows_a_workspace_root_project() {
    let mut aux = AuxIndex::empty();
    let mut record = sample_record("root", WorkViewLifecycle::Active);
    record.project_path = String::new();
    aux.upsert(WorkViewId::new("wv_root"), record);

    let bytes = aux.to_canonical_bytes().expect("encode");
    let decoded = decode_aux_index_plaintext(&bytes, &AuxDecodeLimits::default()).expect("decode");

    assert_eq!(decoded, aux);
}

#[test]
fn canonical_bytes_are_insertion_order_independent() {
    // Insert the same records in the opposite order; the BTreeMap must yield the
    // identical canonical plaintext (the determinism property the seal relies on).
    let forward = sample_index();
    let mut reverse = AuxIndex::empty();
    for (id, record) in sample_index().work_views.into_iter().rev() {
        reverse.upsert(id, record);
    }
    assert_eq!(
        forward.to_canonical_bytes().expect("forward"),
        reverse.to_canonical_bytes().expect("reverse"),
    );
}

#[test]
fn seal_open_round_trip_through_object_store_and_manifest_entry() {
    let crypto = test_crypto();
    let objects = crate::sync::manifest_engine::engine_test_support::FakeRemote::new();
    let aux = sample_index();

    // Upload -> the index rides a manifest entry at the reserved path.
    let (path, entry) = upload_aux_index(&objects, &crypto, &aux).expect("upload");
    assert_eq!(path, WorkspacePath::new(AUX_INDEX_PATH));
    assert!(matches!(entry, ManifestEntry::File { .. }));

    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new("src/main.rs"),
        file_entry(&crypto, b"code"),
    );
    entries.insert(path, entry);
    let manifest = Manifest::new(crypto.key_epoch(), entries);

    let loaded = load_aux_index(&objects, &crypto, &manifest, &AuxDecodeLimits::default())
        .expect("load")
        .expect("present");
    assert_eq!(loaded, aux);
}

#[test]
fn load_returns_none_when_no_reserved_entry() {
    let crypto = test_crypto();
    let objects = crate::sync::manifest_engine::engine_test_support::FakeRemote::new();
    let manifest = Manifest::new(crypto.key_epoch(), BTreeMap::new());
    let loaded =
        load_aux_index(&objects, &crypto, &manifest, &AuxDecodeLimits::default()).expect("load");
    assert!(loaded.is_none());
}

#[test]
fn open_under_a_foreign_key_is_rejected() {
    let crypto = test_crypto();
    let sealed = seal_aux_index(&crypto, &sample_index()).expect("seal");
    // A device that does not hold the workspace key cannot open the index.
    let foreign = WorkspaceCrypto::new("ws_code", [7; 32], KeyEpoch::new(1));
    let error = open_aux_index(
        &foreign,
        &sealed.content_id,
        &sealed.sealed,
        &AuxDecodeLimits::default(),
    )
    .expect_err("foreign key must fail");
    assert!(matches!(error, AuxIndexError::Seal(_)));
}

#[test]
fn open_under_a_foreign_epoch_is_rejected() {
    let crypto = test_crypto();
    let sealed = seal_aux_index(&crypto, &sample_index()).expect("seal");
    // Same key bytes, different epoch: the AEAD context binds the epoch, so open
    // fails (the key-epoch substitution guard).
    let other_epoch = WorkspaceCrypto::new("ws_code", KEY_BYTES, KeyEpoch::new(2));
    let error = open_aux_index(
        &other_epoch,
        &sealed.content_id,
        &sealed.sealed,
        &AuxDecodeLimits::default(),
    )
    .expect_err("foreign epoch must fail");
    assert!(matches!(error, AuxIndexError::Seal(_)));
}

#[test]
fn open_with_a_substituted_content_id_is_rejected() {
    let crypto = test_crypto();
    let sealed = seal_aux_index(&crypto, &sample_index()).expect("seal");
    let wrong = ContentId::new("cid_not_the_real_index");
    let error = open_aux_index(&crypto, &wrong, &sealed.sealed, &AuxDecodeLimits::default())
        .expect_err("substituted content id must fail");
    assert!(matches!(error, AuxIndexError::Seal(_)));
}

#[test]
fn load_rejects_a_blob_key_that_does_not_match_the_sealed_bytes() {
    let crypto = test_crypto();
    let objects = crate::sync::manifest_engine::engine_test_support::FakeRemote::new();
    let sealed = seal_aux_index(&crypto, &sample_index()).expect("seal");
    // Reference the correct content id but a wrong physical key: the sealed bytes
    // stored under that key hash to a different key, so load must refuse it.
    objects
        .put_blob(crate::sync::manifest_engine::push::BlobUpload {
            key: &crate::sync::manifest_engine::manifest::BlobKey::new("b_wrong_key"),
            content_id: &sealed.content_id,
            key_epoch: sealed.key_epoch,
            sealed: &sealed.sealed,
        })
        .expect("put");
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new(AUX_INDEX_PATH),
        ManifestEntry::File {
            size: sealed.size,
            mode: crate::sync::manifest_engine::manifest::FileMode::new(0o600),
            content_id: sealed.content_id.clone(),
            blob_key: crate::sync::manifest_engine::manifest::BlobKey::new("b_wrong_key"),
            key_epoch: sealed.key_epoch,
        },
    );
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let error = load_aux_index(&objects, &crypto, &manifest, &AuxDecodeLimits::default())
        .expect_err("blob key mismatch");
    assert!(matches!(error, AuxIndexError::BlobKeyMismatch));
}

#[test]
fn aux_index_pointer_rejects_a_non_file_entry() {
    let crypto = test_crypto();
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new(AUX_INDEX_PATH),
        ManifestEntry::Directory {
            mode: crate::sync::manifest_engine::manifest::FileMode::new(0o755),
        },
    );
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let error = aux_index_pointer(&manifest).expect_err("directory at reserved path");
    assert!(matches!(error, AuxIndexError::WrongEntryKind { .. }));
}

#[test]
fn decode_rejects_unsorted_records() {
    let plaintext = br#"{"formatVersion":2,"workViews":[
        {"id":"wv_z","projectId":"proj_z","projectPath":"z","name":"z","ownerDeviceId":"dev_test","createdAt":"now","updatedAt":"now","baseManifestKey":"m_a","overlayManifestKey":"m_b","lifecycle":"active"},
        {"id":"wv_a","projectId":"proj_a","projectPath":"a","name":"a","ownerDeviceId":"dev_test","createdAt":"now","updatedAt":"now","baseManifestKey":"m_a","overlayManifestKey":"m_b","lifecycle":"active"}]}"#;
    let error =
        decode_aux_index_plaintext(plaintext, &AuxDecodeLimits::default()).expect_err("unsorted");
    assert!(matches!(error, AuxIndexError::NotSorted));
}

#[test]
fn decode_rejects_duplicate_ids() {
    let plaintext = br#"{"formatVersion":2,"workViews":[
        {"id":"wv_a","projectId":"proj_a","projectPath":"a","name":"a","ownerDeviceId":"dev_test","createdAt":"now","updatedAt":"now","baseManifestKey":"m_a","overlayManifestKey":"m_b","lifecycle":"active"},
        {"id":"wv_a","projectId":"proj_a","projectPath":"a","name":"a","ownerDeviceId":"dev_test","createdAt":"now","updatedAt":"now","baseManifestKey":"m_a","overlayManifestKey":"m_b","lifecycle":"active"}]}"#;
    let error =
        decode_aux_index_plaintext(plaintext, &AuxDecodeLimits::default()).expect_err("duplicate");
    assert!(matches!(error, AuxIndexError::DuplicateId));
}

#[test]
fn decode_rejects_too_many_records() {
    let limits = AuxDecodeLimits {
        max_records: 1,
        ..AuxDecodeLimits::default()
    };
    let bytes = sample_index().to_canonical_bytes().expect("encode");
    let error = decode_aux_index_plaintext(&bytes, &limits).expect_err("over record cap");
    assert!(matches!(error, AuxIndexError::BoundExceeded { .. }));
}

#[test]
fn live_manifest_keys_excludes_discarded_views() {
    let mut aux = sample_index();
    aux.upsert(
        WorkViewId::new("wv_gone"),
        sample_record("gone", WorkViewLifecycle::Discarded),
    );
    let keys = live_manifest_keys(&aux);
    assert!(!keys.contains(&ManifestKey::new("m_gone_base")));
    assert!(keys.contains(&ManifestKey::new("m_base_alpha")));
}

fn file_entry(crypto: &WorkspaceCrypto, plaintext: &[u8]) -> ManifestEntry {
    let content_id = crypto.content_id(plaintext);
    ManifestEntry::File {
        size: plaintext.len() as u64,
        mode: crate::sync::manifest_engine::manifest::FileMode::new(0o644),
        content_id,
        blob_key: crate::sync::manifest_engine::manifest::BlobKey::new("b_placeholder"),
        key_epoch: crypto.key_epoch(),
    }
}
