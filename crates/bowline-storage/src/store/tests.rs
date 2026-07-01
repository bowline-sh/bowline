use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use bowline_core::ids::PackId;

use super::*;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

#[test]
fn local_store_supports_full_range_head_and_metrics() {
    let temp = TempDir::new("byte-store");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let key = ObjectKey::from_pack_id(&PackId::new("pk_0011223344556677")).expect("opaque key");
    let metadata = store
        .put_object(key.clone(), ObjectKind::SourcePack, b"abcdef", None)
        .expect("put object");

    assert_eq!(metadata.byte_len, 6);
    assert_eq!(
        store.head_object(&key).expect("head object").hash,
        stable_object_hash(b"abcdef")
    );
    assert_eq!(store.get_range(&key, ByteRange::new(2, 3)).unwrap(), b"cde");
    assert_eq!(store.get_object(&key).unwrap(), b"abcdef");
    assert_eq!(
        store.metrics(),
        ByteStoreMetrics {
            put_count: 1,
            full_read_count: 1,
            range_read_count: 1,
            head_count: 1,
            delete_count: 0,
            conditional_write_conflict_count: 0,
            verification_failure_count: 0,
            retryable_failure_count: 0,
            convex_action_count: 0,
            convex_mutation_count: 0,
            convex_query_count: 0,
            bytes_uploaded: 6,
            bytes_downloaded: 9,
        }
    );
}

#[test]
fn local_store_deletes_only_verified_known_objects() {
    let temp = TempDir::new("byte-store-delete");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let key = ObjectKey::from_pack_id(&PackId::new("pk_0011223344556677")).expect("opaque key");
    store
        .put_object(key.clone(), ObjectKind::SourcePack, b"abcdef", None)
        .expect("put object");

    store.delete_object(&key).expect("delete object");

    assert!(matches!(
        store.head_object(&key),
        Err(ByteStoreError::MissingObject {
            component: "metadata",
            ..
        })
    ));
    assert_eq!(store.metrics().delete_count, 1);
}

#[test]
fn local_store_rejects_overwrite() {
    let temp = TempDir::new("byte-store");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");

    store
        .put_object(key.clone(), ObjectKind::SourcePack, b"first", None)
        .expect("first put");
    let error = store
        .put_object(key.clone(), ObjectKind::SourcePack, b"second", None)
        .expect_err("overwrite rejected");

    assert!(matches!(error, ByteStoreError::ObjectAlreadyExists(existing) if existing == key));
    assert_eq!(store.get_object(&key).expect("original remains"), b"first");
}

#[test]
fn head_object_rejects_missing_or_corrupt_object_bytes() {
    let temp = TempDir::new("byte-store-head");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let missing_key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    store
        .put_object(
            missing_key.clone(),
            ObjectKind::SourcePack,
            b"present",
            None,
        )
        .expect("put object");
    fs::remove_file(store.stored_path(&missing_key)).expect("remove object bytes");

    assert!(matches!(
        store.head_object(&missing_key),
        Err(ByteStoreError::MissingObject {
            component: "object",
            ..
        })
    ));

    let corrupt_key = ObjectKey::new("packs_pk_8899aabbccddeeff").expect("opaque key");
    store
        .put_object(
            corrupt_key.clone(),
            ObjectKind::SourcePack,
            b"original",
            None,
        )
        .expect("put object");
    fs::write(store.stored_path(&corrupt_key), b"corrupt").expect("corrupt object bytes");

    assert!(matches!(
        store.head_object(&corrupt_key),
        Err(ByteStoreError::CorruptObject { .. })
    ));
}

#[test]
fn head_object_rejects_metadata_for_different_key() {
    let temp = TempDir::new("byte-store-head-key");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let first_key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    let second_key = ObjectKey::new("packs_pk_8899aabbccddeeff").expect("opaque key");
    store
        .put_object(first_key.clone(), ObjectKind::SourcePack, b"first", None)
        .expect("first object");
    store
        .put_object(second_key.clone(), ObjectKind::SourcePack, b"second", None)
        .expect("second object");
    fs::copy(
        store.metadata_path(&second_key),
        store.metadata_path(&first_key),
    )
    .expect("replace sidecar");

    assert!(matches!(
        store.head_object(&first_key),
        Err(ByteStoreError::CorruptObject { .. })
    ));
}

#[test]
fn object_reads_require_committed_matching_metadata() {
    let temp = TempDir::new("byte-store-read-integrity");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let no_metadata_key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    store
        .put_object(
            no_metadata_key.clone(),
            ObjectKind::SourcePack,
            b"present",
            None,
        )
        .expect("put object");
    fs::remove_file(store.metadata_path(&no_metadata_key)).expect("remove metadata");

    assert!(matches!(
        store.get_object(&no_metadata_key),
        Err(ByteStoreError::MissingObject {
            component: "metadata",
            ..
        })
    ));
    assert!(matches!(
        store.get_range(&no_metadata_key, ByteRange::new(0, 1)),
        Err(ByteStoreError::MissingObject {
            component: "metadata",
            ..
        })
    ));

    let corrupt_hash_key = ObjectKey::new("packs_pk_8899aabbccddeeff").expect("opaque key");
    store
        .put_object(
            corrupt_hash_key.clone(),
            ObjectKind::SourcePack,
            b"original",
            None,
        )
        .expect("put object");
    fs::write(store.stored_path(&corrupt_hash_key), b"corrupt!").expect("corrupt same length");
    assert!(matches!(
        store.get_object(&corrupt_hash_key),
        Err(ByteStoreError::CorruptObject { .. })
    ));

    let corrupt_len_key = ObjectKey::new("packs_pk_0123456789abcdef").expect("opaque key");
    store
        .put_object(
            corrupt_len_key.clone(),
            ObjectKind::SourcePack,
            b"short",
            None,
        )
        .expect("put object");
    fs::write(store.stored_path(&corrupt_len_key), b"longer").expect("corrupt length");
    assert!(matches!(
        store.get_range(&corrupt_len_key, ByteRange::new(0, 1)),
        Err(ByteStoreError::CorruptObject { .. })
    ));

    let missing_epoch_key = ObjectKey::new("packs_pk_fedcba9876543210").expect("opaque key");
    store
        .put_object(
            missing_epoch_key.clone(),
            ObjectKind::SourcePack,
            b"current-format",
            None,
        )
        .expect("put object");
    fs::write(
        store.metadata_path(&missing_epoch_key),
        serde_json::json!({
            "key": missing_epoch_key.as_str(),
            "kind": "source-pack",
            "byteLen": 14,
            "hash": stable_object_hash(b"current-format"),
            "createdByDeviceId": null,
            "createdAtUnixMs": 100,
            "retentionState": "pending"
        })
        .to_string(),
    )
    .expect("write pre-current metadata");
    assert!(matches!(
        store.head_object(&missing_epoch_key),
        Err(ByteStoreError::CorruptObject { .. })
    ));
}

#[test]
fn concurrent_puts_keep_object_immutable() {
    let temp = TempDir::new("byte-store-race");
    let root = temp.path().to_path_buf();
    let key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    let barrier = Arc::new(Barrier::new(2));
    let handles = ["first", "second"]
        .into_iter()
        .map(|value| {
            let root = root.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let store = LocalByteStore::open(root).expect("store opens");
                barrier.wait();
                store
                    .put_object(key, ObjectKind::SourcePack, value.as_bytes(), None)
                    .map(|_| value.as_bytes().to_vec())
            })
        })
        .collect::<Vec<_>>();

    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread joins"))
        .collect::<Vec<_>>();
    let winners = outcomes
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .collect::<Vec<_>>();

    assert_eq!(winners.len(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err(ByteStoreError::ObjectAlreadyExists(_))))
            .count(),
        1
    );
    assert_eq!(
        LocalByteStore::open(root)
            .expect("store reopens")
            .get_object(&key)
            .expect("object bytes"),
        *winners[0]
    );
}

#[test]
fn object_key_policy_rejects_path_and_secret_segments() {
    for key in [
        "packs_env_local",
        "packs_.env",
        "packs/abc",
        "packs_secret_0011223344556677",
        "packs_token_0011223344556677",
        "Users_user_Code_acme",
        "packs_src_auth",
        "packs_main_branch",
        "packs-package-json",
        "packs_pk_main_branch",
        "packs_pk_acme_web",
        "manifests_mf_scan_0011223344556677",
    ] {
        assert!(
            ObjectKey::new(key).is_err(),
            "expected object key {key:?} to be rejected"
        );
    }

    assert!(ObjectKey::new("packs_pk_0011223344556677").is_ok());
    assert!(ObjectKey::new("manifests_mf_0011223344556677").is_ok());
    assert!(ObjectKey::new("indexes_ix_0011223344556677").is_ok());
    assert!(ObjectKey::new("overlays_ov_0011223344556677").is_err());
}

#[test]
fn object_key_deserialization_uses_same_validation_as_constructor() {
    let valid: ObjectKey =
        serde_json::from_str("\"packs_pk_0011223344556677\"").expect("valid key");
    assert_eq!(valid.as_str(), "packs_pk_0011223344556677");

    for key in [
        "\"packs/../secret\"",
        "\"packs_.env\"",
        "\"packs_pk_acme_web\"",
        "\"manifests_mf_scan_0011223344556677\"",
    ] {
        assert!(
            serde_json::from_str::<ObjectKey>(key).is_err(),
            "expected serialized key {key} to be rejected"
        );
    }
}

#[test]
fn object_key_leak_helper_checks_source_path_components() {
    assert_object_key_does_not_leak_path(
        "packs_pk_0011223344556677",
        "/workspace/Code/acme/web/src/main.rs",
    )
    .expect("opaque key is clean");
    let leak = assert_object_key_does_not_leak_path(
        "packs_acme_0011223344556677",
        "/workspace/Code/acme/web/src/main.rs",
    )
    .expect_err("leak detected");
    assert_eq!(leak.leaked_segment, "acme");
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bowline-storage-{}-{}-{}",
            std::process::id(),
            name,
            sequence
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir exists");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
