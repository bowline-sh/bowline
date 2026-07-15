use std::{
    cell::RefCell,
    io::Cursor,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use bowline_core::{
    ids::{ContentId, PackId, SnapshotId, WorkspaceId},
    workspace_graph::{ContentLocator, ContentStorage},
};

use super::*;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

struct BytesSource(Vec<u8>);

impl ReopenableObjectSource for BytesSource {
    fn open(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(self.0.clone())))
    }
}

struct ReadErrorSource(Vec<u8>);

impl ReopenableObjectSource for ReadErrorSource {
    fn open(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(ReadErrorReader {
            prefix: self.0.clone(),
            emitted: false,
        }))
    }
}

struct ReadErrorReader {
    prefix: Vec<u8>,
    emitted: bool,
}

impl Read for ReadErrorReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.emitted {
            return Err(io::Error::other("injected source read failure"));
        }
        let read = self.prefix.len().min(buffer.len());
        buffer[..read].copy_from_slice(&self.prefix[..read]);
        self.emitted = true;
        Ok(read)
    }
}

#[derive(Default)]
struct InheritedDefaultStore {
    stored: RefCell<Option<Vec<u8>>>,
}

impl ByteStore for InheritedDefaultStore {
    fn put_object(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        bytes: &[u8],
        _created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.stored.replace(Some(bytes.to_vec()));
        Ok(ObjectMetadata {
            key,
            kind,
            byte_len: bytes.len() as u64,
            hash: stable_object_hash(bytes),
            key_epoch: 1,
            created_by_device_id: None,
            created_at_unix_ms: 0,
            retention_state: RetentionState::Pending,
            retain_until_unix_ms: None,
        })
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        self.stored
            .borrow()
            .clone()
            .ok_or_else(|| ByteStoreError::MissingObject {
                key: key.clone(),
                component: "inherited default test store",
            })
    }

    fn get_range(&self, _key: &ObjectKey, _range: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        Err(ByteStoreError::UnsupportedOperation("test get_range"))
    }

    fn head_object(&self, _key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        Err(ByteStoreError::UnsupportedOperation("test head_object"))
    }

    fn metrics(&self) -> ByteStoreMetrics {
        ByteStoreMetrics::default()
    }
}

fn reader_request<'a>(
    key: ObjectKey,
    source: &'a dyn ReopenableObjectSource,
    expected: &[u8],
) -> PutObjectReaderRequest<'a> {
    PutObjectReaderRequest {
        key,
        kind: ObjectKind::SourcePack,
        content_id: ObjectContentId::from_pack_id(&PackId::new("pk_00112233445566ee")),
        source,
        byte_len: expected.len() as u64,
        expected_hash: ObjectHash::from_stable_hash(stable_object_hash(expected)),
        key_epoch: 1,
        created_by_device_id: None,
    }
}

#[test]
fn inherited_reader_default_rejects_claimed_identity_before_put() {
    let expected = b"expected default bytes";
    let mut mutated_bytes = expected.to_vec();
    mutated_bytes[0] ^= 0xff;
    let mutated = BytesSource(mutated_bytes);
    let store = InheritedDefaultStore::default();
    let key = ObjectKey::new("packs_pk_00112233445566e1").expect("key");

    let error = store
        .put_object_reader_with_content_id_at_epoch(reader_request(key.clone(), &mutated, expected))
        .expect_err("same-length mutation must fail before inherited put");

    assert!(matches!(error, ByteStoreError::CorruptObject { .. }));
    assert!(store.stored.borrow().is_none());
}

#[test]
fn local_reader_identity_failure_is_atomic_and_clean_retry_succeeds() {
    let temp = TempDir::new("reader-identity");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let expected = b"expected local bytes";
    let mut mutated_bytes = expected.to_vec();
    mutated_bytes[0] ^= 0xff;
    let mutated = BytesSource(mutated_bytes);
    let valid = BytesSource(expected.to_vec());
    let key = ObjectKey::new("packs_pk_00112233445566e2").expect("key");

    let error = store
        .put_object_reader_with_content_id_at_epoch(reader_request(key.clone(), &mutated, expected))
        .expect_err("same-length mutation must fail atomically");
    assert!(matches!(error, ByteStoreError::CorruptObject { .. }));
    assert!(matches!(
        store.head_object(&key),
        Err(ByteStoreError::MissingObject { .. })
    ));
    assert!(!store.stored_path(&key).exists());
    assert!(!store.metadata_path(&key).exists());
    assert_eq!(store.metrics().put_count, 0);

    let metadata = store
        .put_object_reader_with_content_id_at_epoch(reader_request(key.clone(), &valid, expected))
        .expect("clean retry succeeds");
    assert_eq!(metadata.hash, stable_object_hash(expected));
    assert_eq!(store.get_object(&key).expect("stored object"), expected);
    assert_eq!(store.metrics().put_count, 1);
}

fn assert_local_reader_failure_is_atomic_then_retry_succeeds(
    name: &str,
    key: ObjectKey,
    source: &dyn ReopenableObjectSource,
    expected: &[u8],
) {
    let temp = TempDir::new(name);
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let valid = BytesSource(expected.to_vec());

    store
        .put_object_reader_with_content_id_at_epoch(reader_request(key.clone(), source, expected))
        .expect_err("faulty source must fail atomically");
    assert!(!store.stored_path(&key).exists());
    assert!(!store.metadata_path(&key).exists());
    assert!(matches!(
        store.head_object(&key),
        Err(ByteStoreError::MissingObject { .. })
    ));
    assert_eq!(store.metrics().put_count, 0);
    assert_eq!(store.metrics().bytes_uploaded, 0);
    assert_eq!(store.metrics().peak_object_bytes_in_flight, 0);

    store
        .put_object_reader_with_content_id_at_epoch(reader_request(key.clone(), &valid, expected))
        .expect("clean retry succeeds");
    assert_eq!(store.get_object(&key).expect("stored object"), expected);
    assert_eq!(store.metrics().put_count, 1);
}

#[test]
fn local_reader_rejects_too_long_source_before_writing_excess() {
    let expected = b"bounded local source";
    let mut too_long = expected.to_vec();
    too_long.extend_from_slice(b"excess bytes must not be written");
    assert_local_reader_failure_is_atomic_then_retry_succeeds(
        "reader-too-long",
        ObjectKey::new("packs_pk_00112233445566e3").expect("key"),
        &BytesSource(too_long),
        expected,
    );
}

#[test]
fn local_reader_cleans_up_after_source_read_error() {
    let expected = b"erroring local source";
    assert_local_reader_failure_is_atomic_then_retry_succeeds(
        "reader-error",
        ObjectKey::new("packs_pk_00112233445566e4").expect("key"),
        &ReadErrorSource(expected[..5].to_vec()),
        expected,
    );
}

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
            peak_object_bytes_in_flight: 6,
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
fn local_store_adopts_matching_bytes_when_metadata_was_lost() {
    let temp = TempDir::new("byte-store-orphan");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    fs::write(store.stored_path(&key), b"orphaned object bytes").expect("seed orphan bytes");

    let metadata = store
        .put_object(
            key.clone(),
            ObjectKind::SourcePack,
            b"orphaned object bytes",
            None,
        )
        .expect("matching orphan bytes are adopted");

    assert_eq!(metadata.key, key);
    assert_eq!(metadata.hash, stable_object_hash(b"orphaned object bytes"));
    assert_eq!(
        store.get_object(&metadata.key).expect("object reads"),
        b"orphaned object bytes"
    );
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
fn streaming_hash_matches_slice_hash() {
    let bytes = (0..200_000)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();

    assert_eq!(
        stable_object_hash(&bytes),
        stable_object_hash_reader(&mut std::io::Cursor::new(&bytes)).expect("reader hash")
    );
}

#[test]
fn local_store_streams_puts_and_gets_with_matching_metadata() {
    let temp = TempDir::new("byte-store-stream");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    let bytes = (0..150_000)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();

    let metadata = store
        .put_object_reader(
            key.clone(),
            ObjectKind::SourcePack,
            &mut std::io::Cursor::new(&bytes),
            Some(bytes.len() as u64),
            None,
        )
        .expect("streaming put");
    let reader_store =
        LocalByteStore::open_deterministic(temp.path(), 200).expect("reader store opens");
    let mut streamed = Vec::new();
    let copied = reader_store
        .get_object_to_writer(&key, &mut streamed)
        .expect("streaming get");

    assert_eq!(metadata.byte_len, bytes.len() as u64);
    assert_eq!(metadata.hash, stable_object_hash(&bytes));
    assert_eq!(copied, bytes.len() as u64);
    assert_eq!(streamed, bytes);
    assert_eq!(reader_store.metrics().peak_object_bytes_in_flight, 0);
}

#[test]
fn streaming_get_rejects_corrupt_object_bytes() {
    let temp = TempDir::new("byte-store-stream-corrupt");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    store
        .put_object_reader(
            key.clone(),
            ObjectKind::SourcePack,
            &mut std::io::Cursor::new(b"original bytes"),
            None,
            None,
        )
        .expect("streaming put");
    fs::write(store.stored_path(&key), b"corrupt bytes!").expect("corrupt bytes");

    let mut output = Vec::new();
    assert!(matches!(
        store.get_object_to_writer(&key, &mut output),
        Err(ByteStoreError::CorruptObject { .. })
    ));
    assert!(output.is_empty());
}

#[test]
fn metadata_race_reuses_matching_commit_without_deleting_bytes() {
    let temp = TempDir::new("byte-store-metadata-race");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let key = ObjectKey::new("packs_pk_0011223344556677").expect("opaque key");
    let bytes = b"already committed bytes";
    let metadata = store.metadata_for(
        key.clone(),
        ObjectKind::SourcePack,
        bytes.len() as u64,
        stable_object_hash(bytes),
        CURRENT_WRITE_KEY_EPOCH,
        None,
    );

    fs::write(store.stored_path(&key), bytes).expect("object bytes written");
    store.write_metadata(&metadata).expect("metadata committed");

    let committed = store
        .commit_metadata_after_object_write(&metadata)
        .expect("matching committed metadata is reused");

    assert_eq!(committed.hash, metadata.hash);
    assert_eq!(fs::read(store.stored_path(&key)).unwrap(), bytes);
}

#[test]
fn upload_journal_appends_and_reads_latest_entry_per_object() {
    let temp = TempDir::new("byte-store-journal");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let content_id = ContentId::new("cid_journal");
    let key = SourcePackUploadJournalKey::new(
        WorkspaceId::new("ws_journal"),
        SnapshotId::new("snap_journal"),
        1,
        [(content_id.clone(), 12)],
    );
    let object_key = ObjectKey::new("packs_pk_0011223344556677").expect("object key");
    let first = SourcePackUploadJournalEntry {
        pointer: SourcePackUploadJournalPointer {
            object_key: object_key.clone(),
            pack_id: PackId::new("pk_0011223344556677"),
            byte_len: 10,
            hash: SourcePackUploadJournalObjectHash::from_stable_hash("b3_first".to_string()),
            key_epoch: 1,
            created_at_unix_ms: 1,
        },
        locators: vec![journal_locator(
            content_id.clone(),
            PackId::new("pk_0011223344556677"),
            10,
        )],
    };
    let latest = SourcePackUploadJournalEntry {
        pointer: SourcePackUploadJournalPointer {
            byte_len: 12,
            hash: SourcePackUploadJournalObjectHash::from_stable_hash("b3_latest".to_string()),
            created_at_unix_ms: 2,
            ..first.pointer.clone()
        },
        locators: vec![journal_locator(
            content_id,
            PackId::new("pk_0011223344556677"),
            12,
        )],
    };

    store
        .record_source_pack_upload_journal(&key, &first)
        .expect("record first");
    store
        .record_source_pack_upload_journal(&key, &latest)
        .expect("record latest");

    let entries = store
        .source_pack_upload_journal(&key)
        .expect("read journal");
    assert_eq!(entries, vec![latest]);
}

#[test]
fn upload_journal_ignores_torn_trailing_line() {
    let temp = TempDir::new("byte-store-journal-torn");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    let content_id = ContentId::new("cid_journal");
    let pack_id = PackId::new("pk_0011223344556677");
    let object_key = ObjectKey::new("packs_pk_0011223344556677").expect("object key");
    let key = SourcePackUploadJournalKey::new(
        WorkspaceId::new("ws_journal"),
        SnapshotId::new("snap_journal"),
        1,
        [(content_id.clone(), 12)],
    );
    let entry = SourcePackUploadJournalEntry {
        pointer: SourcePackUploadJournalPointer {
            object_key: object_key.clone(),
            pack_id: pack_id.clone(),
            byte_len: 12,
            hash: SourcePackUploadJournalObjectHash::from_stable_hash("b3_complete".to_string()),
            key_epoch: 1,
            created_at_unix_ms: 1,
        },
        locators: vec![journal_locator(content_id.clone(), pack_id.clone(), 12)],
    };
    let latest = SourcePackUploadJournalEntry {
        pointer: SourcePackUploadJournalPointer {
            object_key,
            pack_id: pack_id.clone(),
            byte_len: 13,
            hash: SourcePackUploadJournalObjectHash::from_stable_hash("b3_latest".to_string()),
            key_epoch: 1,
            created_at_unix_ms: 2,
        },
        locators: vec![journal_locator(content_id, pack_id, 13)],
    };
    store
        .record_source_pack_upload_journal(&key, &entry)
        .expect("record complete");
    let path = temp
        .path()
        .join("upload-journal")
        .join(format!("{}.json", key.content_set_digest()));
    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open journal")
        .write_all(b"{\"pointer\"")
        .expect("append torn line");

    let entries = store
        .source_pack_upload_journal(&key)
        .expect("read journal");
    assert_eq!(entries, vec![entry]);

    store
        .record_source_pack_upload_journal(&key, &latest)
        .expect("record after torn line");
    let entries = store
        .source_pack_upload_journal(&key)
        .expect("read repaired journal");
    assert_eq!(entries, vec![latest]);
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
fn list_object_keys_ignores_crash_left_atomic_temp_siblings() {
    let temp = TempDir::new("byte-store-list-temp");
    let store = LocalByteStore::open_deterministic(temp.path(), 100).expect("store opens");
    fs::write(
        temp.path()
            .join("objects")
            .join(".packs_pk_0011223344556677.123.1.bowline-tmp"),
        b"partial",
    )
    .expect("seed temp sibling");

    assert_eq!(store.list_object_keys().expect("list keys"), Vec::new());
}

fn journal_locator(content_id: ContentId, pack_id: PackId, raw_size: u64) -> ContentLocator {
    ContentLocator {
        content_id,
        storage: ContentStorage::Packed,
        raw_size,
        pack_id: Some(pack_id),
        offset: Some(0),
        length: Some(raw_size),
    }
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
