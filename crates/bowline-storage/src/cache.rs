use std::{error::Error, fmt, fs, io, path::PathBuf};

use bowline_core::{
    ids::{ContentId, WorkspaceId},
    workspace_graph::{ContentLocator, workspace_content_id},
};

use crate::{
    ByteRange, ByteStore, ByteStoreError, ObjectKey,
    envelope::{StorageKey, workspace_id_hash},
    packfile::{PackfileError, read_record_range},
};

#[derive(Debug)]
pub struct LocalContentCache {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct RangeHydrationRequest<'a> {
    pub object_key: &'a ObjectKey,
    pub workspace_id: &'a WorkspaceId,
    pub locator: &'a ContentLocator,
    pub content_key: [u8; 32],
    pub key: StorageKey,
    pub key_epoch: u32,
}

impl LocalContentCache {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let root = root.into();
        fs::create_dir_all(root.join("content"))?;
        fs::create_dir_all(root.join("packs"))?;
        Ok(Self { root })
    }

    pub fn put_content(&self, content_id: &ContentId, bytes: &[u8]) -> Result<(), CacheError> {
        fs::write(self.content_path(content_id)?, bytes)?;
        Ok(())
    }

    pub fn get_content(
        &self,
        content_id: &ContentId,
        content_key: [u8; 32],
    ) -> Result<Vec<u8>, CacheError> {
        let bytes = self.get_content_unchecked(content_id)?;
        if workspace_content_id(content_key, &bytes) == *content_id {
            Ok(bytes)
        } else {
            Err(CacheError::ContentIdMismatch {
                expected: content_id.clone(),
            })
        }
    }

    fn get_content_unchecked(&self, content_id: &ContentId) -> Result<Vec<u8>, CacheError> {
        fs::read(self.content_path(content_id)?).map_err(|error| map_missing(error, "content"))
    }

    pub fn contains_content(&self, content_id: &ContentId) -> bool {
        self.content_path(content_id)
            .map(|path| path.exists())
            .unwrap_or(false)
    }

    pub fn put_pack(&self, key: &ObjectKey, bytes: &[u8]) -> Result<(), CacheError> {
        fs::write(self.pack_path(key), bytes)?;
        Ok(())
    }

    pub fn get_pack(&self, key: &ObjectKey) -> Result<Vec<u8>, CacheError> {
        fs::read(self.pack_path(key)).map_err(|error| map_missing(error, "pack"))
    }

    pub fn evict_content(&self, content_id: &ContentId) -> Result<bool, CacheError> {
        let path = self.content_path(content_id)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn hydrate_record_from_range<S: ByteStore + ?Sized>(
        &self,
        store: &S,
        request: RangeHydrationRequest<'_>,
    ) -> Result<Vec<u8>, CacheError> {
        match self.get_content(&request.locator.content_id, request.content_key) {
            Ok(bytes) => return Ok(bytes),
            Err(CacheError::ContentIdMismatch { .. }) => {
                self.evict_content(&request.locator.content_id)?;
            }
            Err(CacheError::MissingCachedBytes(_)) => {}
            Err(error) => return Err(error),
        }
        let pack_id = request
            .locator
            .pack_id
            .as_ref()
            .ok_or(CacheError::MissingPackedLocator("pack_id"))?;
        let offset = request
            .locator
            .offset
            .ok_or(CacheError::MissingPackedLocator("offset"))?;
        let length = request
            .locator
            .length
            .ok_or(CacheError::MissingPackedLocator("length"))?;
        let (encrypted_record, from_pack_cache) =
            match self.cached_record_range(request.object_key, offset, length) {
                Ok(bytes) => (bytes, true),
                Err(CacheError::MissingCachedBytes(_)) => (
                    store.get_range(request.object_key, ByteRange::new(offset, length))?,
                    false,
                ),
                Err(CacheError::Pack(_)) => {
                    self.evict_pack(request.object_key)?;
                    (
                        store.get_range(request.object_key, ByteRange::new(offset, length))?,
                        false,
                    )
                }
                Err(error) => return Err(error),
            };
        let bytes = match read_record_range(
            &encrypted_record,
            &workspace_id_hash(request.workspace_id.as_str()),
            pack_id,
            &request.locator.content_id,
            request.key,
            request.key_epoch,
        ) {
            Ok(bytes) => bytes,
            Err(_) if from_pack_cache => {
                self.evict_pack(request.object_key)?;
                let encrypted_record =
                    store.get_range(request.object_key, ByteRange::new(offset, length))?;
                read_record_range(
                    &encrypted_record,
                    &workspace_id_hash(request.workspace_id.as_str()),
                    pack_id,
                    &request.locator.content_id,
                    request.key,
                    request.key_epoch,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        if !content_matches(&request, &bytes) {
            return Err(CacheError::ContentIdMismatch {
                expected: request.locator.content_id.clone(),
            });
        }
        self.put_content(&request.locator.content_id, &bytes)?;
        Ok(bytes)
    }

    pub fn prefetch_pack<S: ByteStore + ?Sized>(
        &self,
        store: &S,
        object_key: &ObjectKey,
    ) -> Result<(), CacheError> {
        let bytes = store.get_object(object_key)?;
        self.put_pack(object_key, &bytes)
    }

    fn content_path(&self, content_id: &ContentId) -> Result<PathBuf, CacheError> {
        Ok(self
            .root
            .join("content")
            .join(path_component(content_id.as_str())?))
    }

    fn pack_path(&self, key: &ObjectKey) -> PathBuf {
        self.root.join("packs").join(key.as_str())
    }

    fn evict_pack(&self, key: &ObjectKey) -> Result<bool, CacheError> {
        match fs::remove_file(self.pack_path(key)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn cached_record_range(
        &self,
        key: &ObjectKey,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, CacheError> {
        let pack = self.get_pack(key)?;
        let start = usize::try_from(offset)
            .map_err(|_| CacheError::Pack(PackfileError::InvalidRecordRange))?;
        let length = usize::try_from(length)
            .map_err(|_| CacheError::Pack(PackfileError::InvalidRecordRange))?;
        let end = start
            .checked_add(length)
            .ok_or(CacheError::Pack(PackfileError::InvalidRecordRange))?;
        pack.get(start..end)
            .map(ToOwned::to_owned)
            .ok_or(CacheError::Pack(PackfileError::InvalidRecordRange))
    }
}

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    MissingCachedBytes(&'static str),
    MissingPackedLocator(&'static str),
    ContentIdMismatch { expected: ContentId },
    InvalidCacheKey(String),
    Store(ByteStoreError),
    Pack(PackfileError),
}

impl fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "cache I/O failed: {error}"),
            Self::MissingCachedBytes(kind) => write!(formatter, "missing cached {kind} bytes"),
            Self::MissingPackedLocator(field) => {
                write!(formatter, "packed locator is missing {field}")
            }
            Self::ContentIdMismatch { expected } => {
                write!(
                    formatter,
                    "cached or hydrated bytes did not match content ID {}",
                    expected.as_str()
                )
            }
            Self::InvalidCacheKey(key) => write!(formatter, "invalid cache key `{key}`"),
            Self::Store(error) => write!(formatter, "byte store read failed: {error}"),
            Self::Pack(error) => write!(formatter, "pack hydration failed: {error}"),
        }
    }
}

fn content_matches(request: &RangeHydrationRequest<'_>, bytes: &[u8]) -> bool {
    workspace_content_id(request.content_key, bytes) == request.locator.content_id
}

impl Error for CacheError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Pack(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for CacheError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ByteStoreError> for CacheError {
    fn from(error: ByteStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<PackfileError> for CacheError {
    fn from(error: PackfileError) -> Self {
        Self::Pack(error)
    }
}

fn path_component(value: &str) -> Result<String, CacheError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CacheError::InvalidCacheKey(value.to_string()));
    }
    Ok(value.to_string())
}

fn map_missing(error: io::Error, kind: &'static str) -> CacheError {
    if error.kind() == io::ErrorKind::NotFound {
        CacheError::MissingCachedBytes(kind)
    } else {
        CacheError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

    use bowline_core::{
        ids::{ContentId, PackId, WorkspaceId},
        workspace_graph::{ContentLocator, ContentStorage, workspace_content_id},
    };

    use crate::{
        ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, LocalByteStore, ObjectKey,
        ObjectKind, ObjectMetadata,
        envelope::StorageKey,
        packfile::{PackRecordInput, PackWriter},
    };

    use super::*;

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
            chunk_ids: Vec::new(),
        };
        let object_key = ObjectKey::from_pack_id(locator.pack_id.as_ref().unwrap()).unwrap();

        assert_eq!(
            cache
                .hydrate_record_from_range(
                    &UnavailableByteStore,
                    RangeHydrationRequest {
                        object_key: &object_key,
                        workspace_id: &WorkspaceId::new("ws_cache"),
                        locator: &locator,
                        content_key,
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
                    key,
                    key_epoch: 1,
                },
            )
            .expect("hydrated");

        assert_eq!(hydrated, b"hello from pack");
        assert_eq!(
            cache.get_content(&content_id, [1_u8; 32]).unwrap(),
            b"hello from pack"
        );
        assert_eq!(store.metrics().range_read_count, 1);
        assert_eq!(store.metrics().full_read_count, 0);
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
                    key,
                    key_epoch: 1,
                },
            )
            .expect("hydrated from cached pack");

        assert_eq!(hydrated, b"hello from cached pack");
        assert_eq!(
            cache.get_content(&content_id, content_key).unwrap(),
            b"hello from cached pack"
        );
    }

    #[test]
    fn corrupt_prefetched_pack_falls_back_to_byte_store_range() {
        let temp = TempDir::new("cache-corrupt-pack");
        let store = LocalByteStore::open_deterministic(temp.path().join("objects"), 1)
            .expect("store opens");
        let cache = LocalContentCache::open(temp.path().join("cache")).expect("cache opens");
        let key = StorageKey::deterministic(7);
        let content_key = [7_u8; 32];
        let content_id = workspace_content_id(content_key, b"hello after corrupt cache");
        let workspace_id = WorkspaceId::new("ws_cache");
        let output = PackWriter::new(
            workspace_id.clone(),
            PackId::new("pk_0123456789abcdef"),
            key,
            1,
        )
        .write(&[PackRecordInput {
            content_id: content_id.clone(),
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
            .expect("corrupt pack cached");

        let hydrated = cache
            .hydrate_record_from_range(
                &store,
                RangeHydrationRequest {
                    object_key: &output.object_key,
                    workspace_id: &workspace_id,
                    locator: &output.locators[0],
                    content_key,
                    key,
                    key_epoch: 1,
                },
            )
            .expect("fallback hydrate");

        assert_eq!(hydrated, b"hello after corrupt cache");
        assert_eq!(store.metrics().range_read_count, 1);
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

        fn get_range(
            &self,
            _key: &ObjectKey,
            _range: ByteRange,
        ) -> Result<Vec<u8>, ByteStoreError> {
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
}
