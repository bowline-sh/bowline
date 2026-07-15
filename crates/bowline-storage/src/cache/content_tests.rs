use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use bowline_core::{
    ids::{ContentId, PackId, WorkspaceId},
    workspace_graph::{ContentLocator, ContentStorage, workspace_content_id},
};

use crate::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, LocalByteStore, ObjectKey, ObjectKind,
    ObjectMetadata,
    envelope::StorageKey,
    packfile::{PackRecordInput, PackWriter},
};

use super::{CacheError, ContentVerification, LocalContentCache, RangeHydrationRequest};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

#[test]
fn hydrated_content_reads_from_cache_without_store() {
    let temp = TempDir::new("cache");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let content_key = [9_u8; 32];
    let content_id = workspace_content_id(content_key, b"already hydrated");
    cache
        .put_content(&content_id, b"already hydrated")
        .expect("content cached");
    let locator = ContentLocator {
        content_id: content_id.clone(),
        storage: ContentStorage::Packed,
        raw_size: 16,
        pack_id: Some(PackId::new("pk_0011223344556677")),
        offset: Some(10),
        length: Some(20),
    };
    let object_key =
        ObjectKey::from_pack_id(locator.pack_id.as_ref().expect("locator has pack ID"))
            .expect("pack ID is a valid object key");

    assert_eq!(
        cache
            .hydrate_record_from_range(
                &UnavailableByteStore,
                RangeHydrationRequest {
                    object_key: &object_key,
                    workspace_id: &WorkspaceId::new("ws_cache"),
                    locator: &locator,
                    content_key,
                    content_verification: ContentVerification::WorkspaceKeyed,
                    key: StorageKey::deterministic(9),
                    key_epoch: 1,
                },
            )
            .expect("cached content"),
        b"already hydrated"
    );
}

#[test]
fn hydrate_range_stores_decrypted_content_and_counts_one_range_read() {
    let temp = TempDir::new("cache-range");
    let object_root = temp.path().join("objects");
    let cache_root = temp.path().join("cache");
    let store = LocalByteStore::open_deterministic(object_root, 1).expect("store opens");
    let cache = LocalContentCache::open(cache_root).expect("cache opens");
    let key = StorageKey::deterministic(4);
    let content_id = workspace_content_id([1_u8; 32], b"hello from pack");
    let output = PackWriter::new(
        WorkspaceId::new("ws_cache"),
        PackId::new("pk_0011223344556677"),
        key,
        1,
    )
    .write(&[PackRecordInput {
        content_id: content_id.clone(),
        bytes: b"hello from pack".to_vec(),
    }])
    .expect("pack writes");
    store
        .put_object(
            output.object_key.clone(),
            ObjectKind::SourcePack,
            &output.bytes,
            None,
        )
        .expect("pack stored");

    let locator = &output.locators[0];
    let hydrated = cache
        .hydrate_record_from_range(
            &store,
            RangeHydrationRequest {
                object_key: &output.object_key,
                workspace_id: &WorkspaceId::new("ws_cache"),
                locator,
                content_key: [1_u8; 32],
                content_verification: ContentVerification::WorkspaceKeyed,
                key,
                key_epoch: 1,
            },
        )
        .expect("hydrated");

    assert_eq!(hydrated, b"hello from pack");
    assert_eq!(
        cache
            .get_content(&content_id, [1_u8; 32])
            .expect("cached content reads"),
        b"hello from pack"
    );
    assert_eq!(store.metrics().range_read_count, 1);
    assert_eq!(store.metrics().full_read_count, 0);
}

#[test]
fn fetched_range_requires_exact_authenticated_record_before_caching() {
    let temp = TempDir::new("fetched-range");
    let content_key = [13_u8; 32];
    let workspace_id = WorkspaceId::new("ws_fetched_range");
    let key = StorageKey::deterministic(13);
    let content_id = workspace_content_id(content_key, b"authenticated record");
    let output = PackWriter::new(
        workspace_id.clone(),
        PackId::new("pk_fedcba9876543210"),
        key,
        1,
    )
    .write(&[PackRecordInput {
        content_id: content_id.clone(),
        bytes: b"authenticated record".to_vec(),
    }])
    .expect("pack writes");
    let locator = &output.locators[0];
    let offset = locator.offset.expect("offset") as usize;
    let length = locator.length.expect("length") as usize;
    let encrypted_record = &output.bytes[offset..offset + length];
    let request = RangeHydrationRequest {
        object_key: &output.object_key,
        workspace_id: &workspace_id,
        locator,
        content_key,
        content_verification: ContentVerification::WorkspaceKeyed,
        key,
        key_epoch: 1,
    };

    let short_cache = LocalContentCache::open(temp.path().join("short")).expect("cache");
    assert!(matches!(
        short_cache.hydrate_record_from_fetched_range(request, &encrypted_record[..length - 1]),
        Err(CacheError::ShortFetchedRange { .. })
    ));
    assert!(!short_cache.contains_content(&content_id));

    let corrupt_cache = LocalContentCache::open(temp.path().join("corrupt")).expect("cache");
    let mut corrupt = encrypted_record.to_vec();
    corrupt[0] ^= 0xff;
    assert!(matches!(
        corrupt_cache.hydrate_record_from_fetched_range(request, &corrupt),
        Err(CacheError::Pack(_))
    ));
    assert!(!corrupt_cache.contains_content(&content_id));

    let valid_cache = LocalContentCache::open(temp.path().join("valid")).expect("cache");
    assert_eq!(
        valid_cache
            .hydrate_record_from_fetched_range(request, encrypted_record)
            .expect("authenticated"),
        b"authenticated record"
    );
    assert_eq!(
        valid_cache
            .get_previously_verified_content(&content_id)
            .expect("verified cache"),
        b"authenticated record"
    );
}

#[test]
fn prefetched_pack_hydrates_without_byte_store() {
    let temp = TempDir::new("cache-prefetch");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let key = StorageKey::deterministic(6);
    let content_key = [6_u8; 32];
    let content_id = workspace_content_id(content_key, b"hello from cached pack");
    let workspace_id = WorkspaceId::new("ws_cache");
    let output = PackWriter::new(
        workspace_id.clone(),
        PackId::new("pk_8899aabbccddeeff"),
        key,
        1,
    )
    .write(&[PackRecordInput {
        content_id: content_id.clone(),
        bytes: b"hello from cached pack".to_vec(),
    }])
    .expect("pack writes");
    cache
        .put_pack(&output.object_key, &output.bytes)
        .expect("pack cached");

    let hydrated = cache
        .hydrate_record_from_range(
            &UnavailableByteStore,
            RangeHydrationRequest {
                object_key: &output.object_key,
                workspace_id: &workspace_id,
                locator: &output.locators[0],
                content_key,
                content_verification: ContentVerification::WorkspaceKeyed,
                key,
                key_epoch: 1,
            },
        )
        .expect("hydrated from cached pack");

    assert_eq!(hydrated, b"hello from cached pack");
    assert_eq!(
        cache
            .get_content(&content_id, content_key)
            .expect("cached content reads"),
        b"hello from cached pack"
    );
}

#[test]
fn public_content_reads_reject_corrupt_cache_bytes() {
    let temp = TempDir::new("cache-corrupt");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let content_key = [10_u8; 32];
    let content_id = workspace_content_id(content_key, b"good bytes");
    cache
        .put_content(&content_id, b"wrong bytes")
        .expect("corrupt content stored");

    assert!(matches!(
        cache.get_content(&content_id, content_key),
        Err(CacheError::ContentIdMismatch { expected }) if expected == content_id
    ));
}

#[test]
fn previously_verified_reads_require_verification_marker() {
    let temp = TempDir::new("cache-verified-marker");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let content_key = [11_u8; 32];
    let content_id = workspace_content_id(content_key, b"verified bytes");
    cache
        .put_content(&content_id, b"verified bytes")
        .expect("content stored");

    assert!(matches!(
        cache.get_previously_verified_content(&content_id),
        Err(CacheError::MissingCachedBytes("verified content marker"))
    ));

    cache
        .get_content(&content_id, content_key)
        .expect("content verified");
    assert_eq!(
        cache
            .get_previously_verified_content(&content_id)
            .expect("previously verified content"),
        b"verified bytes"
    );
}

#[test]
fn previously_verified_reads_reject_bytes_changed_after_verification() {
    let temp = TempDir::new("cache-verified-stale");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let content_key = [12_u8; 32];
    let content_id = workspace_content_id(content_key, b"verified bytes");
    cache
        .put_content(&content_id, b"verified bytes")
        .expect("content stored");
    cache
        .get_content(&content_id, content_key)
        .expect("content verified");
    fs::write(
        cache.content_path(&content_id).expect("content path"),
        b"changed bytes",
    )
    .expect("corrupt cached bytes");

    assert!(matches!(
        cache.get_previously_verified_content(&content_id),
        Err(CacheError::ContentIdMismatch { expected }) if expected == content_id
    ));
}

#[test]
fn verified_content_reader_consumes_large_content_in_bounded_chunks() {
    let temp = TempDir::new("cache-verified-stream");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let content_key = [14_u8; 32];
    let bytes = vec![0x5a; 8 * 1024 * 1024 + 17];
    let content_id = workspace_content_id(content_key, &bytes);
    cache
        .put_content(&content_id, &bytes)
        .expect("content stored");
    cache
        .get_content(&content_id, content_key)
        .expect("content verified");

    let mut reader = cache
        .open_previously_verified_content(&content_id)
        .expect("verified stream");
    assert_eq!(reader.byte_len(), bytes.len() as u64);
    let mut buffer = [0_u8; 32 * 1024];
    let mut consumed = 0_usize;
    let mut largest_read = 0_usize;
    loop {
        let read = reader.read(&mut buffer).expect("stream chunk");
        if read == 0 {
            break;
        }
        largest_read = largest_read.max(read);
        assert!(buffer[..read].iter().all(|byte| *byte == 0x5a));
        consumed += read;
    }
    assert_eq!(consumed, bytes.len());
    assert!(largest_read <= buffer.len());
}

struct UnavailableByteStore;

impl ByteStore for UnavailableByteStore {
    fn put_object(
        &self,
        _key: ObjectKey,
        _kind: ObjectKind,
        _bytes: &[u8],
        _created_by_device_id: Option<&bowline_core::ids::DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        panic!("cache hit should not put objects")
    }

    fn get_object(&self, _key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        panic!("cache hit should not read full objects")
    }

    fn get_range(&self, _key: &ObjectKey, _range: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        panic!("cache hit should not range-read objects")
    }

    fn head_object(&self, _key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        panic!("cache hit should not head objects")
    }

    fn metrics(&self) -> ByteStoreMetrics {
        ByteStoreMetrics::default()
    }
}

#[test]
fn eviction_removes_cache_bytes_only() {
    let temp = TempDir::new("cache-evict");
    let source_path = temp.path().join("workspace").join("src").join("main.rs");
    fs::create_dir_all(source_path.parent().expect("source parent")).expect("parent");
    fs::write(&source_path, b"source").expect("source write");
    let cache = LocalContentCache::open(temp.path().join("cache")).expect("cache opens");
    let content_id = ContentId::new("cid_source");
    cache.put_content(&content_id, b"cached").expect("cached");

    assert!(cache.evict_content(&content_id).expect("evicted"));
    assert_eq!(fs::read(source_path).expect("source remains"), b"source");
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
        fs::create_dir_all(&path).expect("temp dir");
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
