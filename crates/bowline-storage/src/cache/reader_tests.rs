use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use bowline_core::{
    ids::{PackId, WorkspaceId},
    workspace_graph::workspace_content_id,
};

use crate::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, LocalByteStore, ObjectKey, ObjectKind,
    ObjectMetadata, RangeHydrationRequest,
    envelope::StorageKey,
    packfile::{PackRecordInput, PackWriter},
};

use super::{CacheError, CachedPackReadMetrics, ContentVerification, LocalContentCache};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

#[test]
fn reads_exact_ranges_and_counts_io() {
    let temp = TempDir::new("ranges");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let key = ObjectKey::from_pack_id(&PackId::new("pk_aabbccddeeff0011")).expect("key");
    cache
        .put_pack(&key, b"first--middle--last")
        .expect("cached");
    let mut reader = cache.open_cached_pack(&key).expect("reader opens");
    assert_eq!(reader.read_range(0, 5).expect("first"), b"first");
    assert_eq!(reader.read_range(7, 6).expect("middle"), b"middle");
    assert_eq!(reader.read_range(15, 4).expect("last"), b"last");
    assert_eq!(
        reader.metrics(),
        CachedPackReadMetrics {
            open_count: 1,
            range_read_count: 3,
            bytes_read: 15,
        }
    );
}

#[test]
fn rejects_invalid_overflowed_and_short_ranges() {
    let temp = TempDir::new("invalid");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let key = ObjectKey::from_pack_id(&PackId::new("pk_aabbccddeeff0022")).expect("key");
    cache.put_pack(&key, b"0123456789").expect("cached");
    let mut reader = cache.open_cached_pack(&key).expect("reader opens");
    assert!(matches!(
        reader.read_range(9, 2),
        Err(CacheError::InvalidCachedPackRange { .. })
    ));
    assert!(matches!(
        reader.read_range(u64::MAX, 2),
        Err(CacheError::InvalidCachedPackRange { .. })
    ));
    fs::write(cache.pack_path(&key), b"short").expect("truncate");
    assert!(matches!(
        reader.read_range(5, 5),
        Err(CacheError::ShortCachedPackRead {
            expected: 5,
            actual: 0
        })
    ));
}

#[test]
fn one_reader_hydrates_one_hundred_records_by_selected_bytes() {
    let temp = TempDir::new("many");
    let cache = LocalContentCache::open(temp.path()).expect("cache opens");
    let key = StorageKey::deterministic(13);
    let content_key = [13_u8; 32];
    let workspace_id = WorkspaceId::new("ws_cache_many");
    let records = (0_u16..100)
        .map(|index| {
            let bytes = format!("record-{index:03}").into_bytes();
            PackRecordInput {
                content_id: workspace_content_id(content_key, &bytes),
                bytes,
            }
        })
        .collect::<Vec<_>>();
    let output = PackWriter::new(
        workspace_id.clone(),
        PackId::new("pk_1000000000000000"),
        key,
        1,
    )
    .write(&records)
    .expect("pack writes");
    cache
        .put_pack(&output.object_key, &output.bytes)
        .expect("cached");
    let mut reader = Some(cache.open_cached_pack(&output.object_key).expect("reader"));
    for (record, locator) in records.iter().zip(&output.locators) {
        let bytes = cache
            .hydrate_record_with_cached_pack(
                &UnavailableByteStore,
                RangeHydrationRequest {
                    object_key: &output.object_key,
                    workspace_id: &workspace_id,
                    locator,
                    content_key,
                    content_verification: ContentVerification::WorkspaceKeyed,
                    key,
                    key_epoch: 1,
                },
                &mut reader,
            )
            .expect("hydrates");
        assert_eq!(bytes, record.bytes);
    }
    let selected_bytes = output
        .locators
        .iter()
        .map(|locator| locator.length.expect("length"))
        .sum::<u64>();
    let metrics = reader.as_ref().expect("valid reader").metrics();
    assert_eq!(metrics.open_count, 1);
    assert_eq!(metrics.range_read_count, 100);
    assert_eq!(metrics.bytes_read, selected_bytes);
    assert!(metrics.bytes_read < output.bytes.len() as u64 * 100);
}

#[test]
fn corrupt_pack_closes_evicts_and_falls_back_to_remote_range() {
    let temp = TempDir::new("corrupt");
    let store =
        LocalByteStore::open_deterministic(temp.path().join("objects"), 1).expect("store opens");
    let cache = LocalContentCache::open(temp.path().join("cache")).expect("cache opens");
    let key = StorageKey::deterministic(7);
    let content_key = [7_u8; 32];
    let workspace_id = WorkspaceId::new("ws_cache");
    let content_id = workspace_content_id(content_key, b"hello after corrupt cache");
    let output = PackWriter::new(
        workspace_id.clone(),
        PackId::new("pk_0123456789abcdef"),
        key,
        1,
    )
    .write(&[PackRecordInput {
        content_id,
        bytes: b"hello after corrupt cache".to_vec(),
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
    cache
        .put_pack(&output.object_key, b"truncated")
        .expect("corrupt cached");
    let mut reader = Some(cache.open_cached_pack(&output.object_key).expect("reader"));
    let hydrated = cache
        .hydrate_record_with_cached_pack(
            &store,
            RangeHydrationRequest {
                object_key: &output.object_key,
                workspace_id: &workspace_id,
                locator: &output.locators[0],
                content_key,
                content_verification: ContentVerification::WorkspaceKeyed,
                key,
                key_epoch: 1,
            },
            &mut reader,
        )
        .expect("fallback hydrate");
    assert_eq!(hydrated, b"hello after corrupt cache");
    assert_eq!(store.metrics().range_read_count, 1);
    assert!(reader.is_none(), "reader closes before eviction");
    assert!(!cache.pack_path(&output.object_key).exists());
}

struct UnavailableByteStore;

impl ByteStore for UnavailableByteStore {
    fn put_object(
        &self,
        _: ObjectKey,
        _: ObjectKind,
        _: &[u8],
        _: Option<&bowline_core::ids::DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        unreachable!()
    }
    fn get_object(&self, _: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        unreachable!()
    }
    fn get_range(&self, _: &ObjectKey, _: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        unreachable!()
    }
    fn head_object(&self, _: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        unreachable!()
    }
    fn metrics(&self) -> ByteStoreMetrics {
        ByteStoreMetrics::default()
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bowline-cache-reader-{}-{name}-{sequence}",
            std::process::id()
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
