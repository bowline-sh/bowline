use std::{
    error::Error,
    fmt,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bowline_core::{
    fs_atomic::{AtomicWriteOptions, write_atomic},
    ids::{ContentId, PackId, WorkspaceId},
    workspace_graph::{ContentLocator, workspace_content_id},
};

use crate::{
    ByteRange, ByteStore, ByteStoreError, ObjectKey,
    envelope::{StorageKey, workspace_id_hash},
    packfile::{PackfileError, read_record_range},
};

#[cfg(test)]
mod content_tests;
#[cfg(test)]
mod reader_tests;

#[derive(Debug)]
pub struct LocalContentCache {
    root: PathBuf,
    io_observer: Arc<dyn CachedPackIoObserver>,
}

pub trait CachedPackIoObserver: fmt::Debug + Send + Sync {
    fn opened(&self, _key: &ObjectKey) {}
    fn read(&self, _key: &ObjectKey, _byte_len: u64) {}
    fn closed(&self, _key: &ObjectKey) {}
    fn release_state(&self, _key: &ObjectKey) -> Option<Arc<CachedPackReleaseState>> {
        None
    }
}

#[derive(Debug, Default)]
pub struct CachedPackReleaseState {
    released: AtomicBool,
}

impl CachedPackReleaseState {
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }

    fn mark_released(&self) {
        self.released.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
struct NoopCachedPackIoObserver;

impl CachedPackIoObserver for NoopCachedPackIoObserver {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachedPackReadMetrics {
    pub open_count: u64,
    pub range_read_count: u64,
    pub bytes_read: u64,
}

#[derive(Debug)]
pub struct CachedPackReader {
    key: ObjectKey,
    file: Option<InstrumentedCachedPackFile>,
    byte_len: u64,
    metrics: CachedPackReadMetrics,
    io_observer: Arc<dyn CachedPackIoObserver>,
}

/// A file-backed reader whose complete contents were authenticated against the
/// cache verification marker before the handle was returned.
///
/// Cache writers publish by atomic rename, so this open file continues to name
/// the verified inode even if the cache path is concurrently replaced.
#[derive(Debug)]
pub struct VerifiedContentReader {
    file: File,
    byte_len: u64,
}

impl VerifiedContentReader {
    pub fn byte_len(&self) -> u64 {
        self.byte_len
    }
}

impl Read for VerifiedContentReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

#[derive(Debug)]
struct InstrumentedCachedPackFile {
    file: Option<File>,
    release_state: Option<Arc<CachedPackReleaseState>>,
}

impl InstrumentedCachedPackFile {
    fn new(file: File, release_state: Option<Arc<CachedPackReleaseState>>) -> Self {
        Self {
            file: Some(file),
            release_state,
        }
    }

    fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("instrumented cached pack file owns its handle until drop")
    }
}

impl Drop for InstrumentedCachedPackFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(release_state) = &self.release_state {
            release_state.mark_released();
        }
    }
}

impl Drop for CachedPackReader {
    fn drop(&mut self) {
        drop(self.file.take());
        self.io_observer.closed(&self.key);
    }
}

impl CachedPackReader {
    pub fn metrics(&self) -> CachedPackReadMetrics {
        self.metrics
    }

    fn read_range_for(
        &mut self,
        key: &ObjectKey,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, CacheError> {
        if self.key != *key {
            return Err(CacheError::MismatchedCachedPackReader {
                expected: self.key.clone(),
                actual: key.clone(),
            });
        }
        self.read_range(offset, length)
    }

    fn read_range(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
        let end = offset
            .checked_add(length)
            .ok_or(CacheError::InvalidCachedPackRange {
                offset,
                length,
                pack_len: self.byte_len,
            })?;
        if end > self.byte_len {
            return Err(CacheError::InvalidCachedPackRange {
                offset,
                length,
                pack_len: self.byte_len,
            });
        }
        let buffer_len =
            usize::try_from(length).map_err(|_| CacheError::InvalidCachedPackRange {
                offset,
                length,
                pack_len: self.byte_len,
            })?;
        let file = self
            .file
            .as_mut()
            .expect("cached pack reader owns its file wrapper until drop")
            .file_mut();
        file.seek(SeekFrom::Start(offset))?;
        self.metrics.range_read_count += 1;
        let mut bytes = vec![0_u8; buffer_len];
        let mut read_len = 0_usize;
        while read_len < buffer_len {
            let chunk_len = file.read(&mut bytes[read_len..])?;
            if chunk_len == 0 {
                return Err(CacheError::ShortCachedPackRead {
                    expected: length,
                    actual: read_len as u64,
                });
            }
            read_len += chunk_len;
            self.metrics.bytes_read += chunk_len as u64;
            self.io_observer.read(&self.key, chunk_len as u64);
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RangeHydrationRequest<'a> {
    pub object_key: &'a ObjectKey,
    pub workspace_id: &'a WorkspaceId,
    pub locator: &'a ContentLocator,
    pub content_key: [u8; 32],
    pub content_verification: ContentVerification,
    pub key: StorageKey,
    pub key_epoch: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentVerification {
    WorkspaceKeyed,
    AuthenticatedSegment,
}

#[derive(Debug, Clone, Copy)]
pub struct RecordRangeProofRequest<'a> {
    pub object_key: &'a ObjectKey,
    pub workspace_id: &'a WorkspaceId,
    pub locator: &'a ContentLocator,
    pub key: StorageKey,
    pub key_epoch: u32,
}

pub fn verify_record_range<S: ByteStore + ?Sized>(
    store: &S,
    request: RecordRangeProofRequest<'_>,
    expected_bytes: &[u8],
) -> Result<(), CacheError> {
    let (pack_id, offset, length) = packed_parts(request.locator)?;
    let encrypted_record = store.get_range(request.object_key, ByteRange::new(offset, length))?;
    let bytes = read_record_range(
        &encrypted_record,
        &workspace_id_hash(request.workspace_id.as_str()),
        pack_id,
        &request.locator.content_id,
        request.key,
        request.key_epoch,
    )?;
    if bytes != expected_bytes {
        return Err(CacheError::ContentIdMismatch {
            expected: request.locator.content_id.clone(),
        });
    }
    Ok(())
}

impl LocalContentCache {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let root = root.into();
        fs::create_dir_all(root.join("content"))?;
        fs::create_dir_all(root.join("packs"))?;
        Ok(Self {
            root,
            io_observer: Arc::new(NoopCachedPackIoObserver),
        })
    }

    pub fn open_with_io_observer(
        root: impl Into<PathBuf>,
        io_observer: Arc<dyn CachedPackIoObserver>,
    ) -> Result<Self, CacheError> {
        let mut cache = Self::open(root)?;
        cache.io_observer = io_observer;
        Ok(cache)
    }

    pub fn put_content(&self, content_id: &ContentId, bytes: &[u8]) -> Result<(), CacheError> {
        write_atomic(
            &self.content_path(content_id)?,
            bytes,
            AtomicWriteOptions::default(),
        )?;
        self.remove_content_verification(content_id)?;
        Ok(())
    }

    pub fn get_content(
        &self,
        content_id: &ContentId,
        content_key: [u8; 32],
    ) -> Result<Vec<u8>, CacheError> {
        let bytes = self.get_content_unchecked(content_id)?;
        if workspace_content_id(content_key, &bytes) == *content_id {
            self.put_content_verification(content_id, &bytes)?;
            Ok(bytes)
        } else {
            Err(CacheError::ContentIdMismatch {
                expected: content_id.clone(),
            })
        }
    }

    pub fn get_previously_verified_content(
        &self,
        content_id: &ContentId,
    ) -> Result<Vec<u8>, CacheError> {
        let mut reader = self.open_previously_verified_content(content_id)?;
        let capacity = usize::try_from(reader.byte_len()).unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn open_previously_verified_content(
        &self,
        content_id: &ContentId,
    ) -> Result<VerifiedContentReader, CacheError> {
        let path = self.content_path(content_id)?;
        let mut file = File::open(path).map_err(|error| map_missing(error, "content"))?;
        let marker = fs::read_to_string(self.content_verification_path(content_id)?)
            .map_err(|error| map_missing(error, "verified content marker"))?;
        let (digest, byte_len) = stream_content_marker(&mut file)?;
        if marker != content_verification_marker_from_parts(&digest, byte_len) {
            return Err(CacheError::ContentIdMismatch {
                expected: content_id.clone(),
            });
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(VerifiedContentReader { file, byte_len })
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
        write_atomic(&self.pack_path(key), bytes, AtomicWriteOptions::default())?;
        Ok(())
    }

    pub fn get_pack(&self, key: &ObjectKey) -> Result<Vec<u8>, CacheError> {
        fs::read(self.pack_path(key)).map_err(|error| map_missing(error, "pack"))
    }

    pub fn open_cached_pack(&self, key: &ObjectKey) -> Result<CachedPackReader, CacheError> {
        let file = File::open(self.pack_path(key)).map_err(|error| map_missing(error, "pack"))?;
        self.io_observer.opened(key);
        let release_state = self.io_observer.release_state(key);
        let mut file = InstrumentedCachedPackFile::new(file, release_state);
        let byte_len = match file.file_mut().metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                drop(file);
                self.io_observer.closed(key);
                return Err(error.into());
            }
        };
        Ok(CachedPackReader {
            key: key.clone(),
            file: Some(file),
            byte_len,
            metrics: CachedPackReadMetrics {
                open_count: 1,
                ..CachedPackReadMetrics::default()
            },
            io_observer: Arc::clone(&self.io_observer),
        })
    }

    pub fn evict_content(&self, content_id: &ContentId) -> Result<bool, CacheError> {
        let path = self.content_path(content_id)?;
        match fs::remove_file(path) {
            Ok(()) => {
                self.remove_content_verification(content_id)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn hydrate_record_from_range<S: ByteStore + ?Sized>(
        &self,
        store: &S,
        request: RangeHydrationRequest<'_>,
    ) -> Result<Vec<u8>, CacheError> {
        if let Some(bytes) = self.cached_content_for(&request)? {
            return Ok(bytes);
        }
        let mut reader = match self.open_cached_pack(request.object_key) {
            Ok(reader) => Some(reader),
            Err(CacheError::MissingCachedBytes(_)) => None,
            Err(error) => return Err(error),
        };
        self.hydrate_uncached_record(store, request, &mut reader)
    }

    pub fn hydrate_record_with_cached_pack<S: ByteStore + ?Sized>(
        &self,
        store: &S,
        request: RangeHydrationRequest<'_>,
        reader: &mut Option<CachedPackReader>,
    ) -> Result<Vec<u8>, CacheError> {
        if let Some(bytes) = self.cached_content_for(&request)? {
            return Ok(bytes);
        }
        self.hydrate_uncached_record(store, request, reader)
    }

    pub fn hydrate_record_from_fetched_range(
        &self,
        request: RangeHydrationRequest<'_>,
        encrypted_record: &[u8],
    ) -> Result<Vec<u8>, CacheError> {
        if let Some(bytes) = self.cached_content_for(&request)? {
            return Ok(bytes);
        }
        let (_, _, expected_length) = packed_parts(request.locator)?;
        if encrypted_record.len() as u64 != expected_length {
            return Err(CacheError::ShortFetchedRange {
                expected: expected_length,
                actual: encrypted_record.len() as u64,
            });
        }
        let bytes = read_record_range(
            encrypted_record,
            &workspace_id_hash(request.workspace_id.as_str()),
            request
                .locator
                .pack_id
                .as_ref()
                .ok_or(CacheError::MissingPackedLocator("pack_id"))?,
            &request.locator.content_id,
            request.key,
            request.key_epoch,
        )?;
        if request.content_verification == ContentVerification::WorkspaceKeyed
            && !content_matches(&request, &bytes)
        {
            return Err(CacheError::ContentIdMismatch {
                expected: request.locator.content_id.clone(),
            });
        }
        self.put_content(&request.locator.content_id, &bytes)?;
        self.put_content_verification(&request.locator.content_id, &bytes)?;
        Ok(bytes)
    }

    fn cached_content_for(
        &self,
        request: &RangeHydrationRequest<'_>,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        let cached = match request.content_verification {
            ContentVerification::WorkspaceKeyed => {
                self.get_content(&request.locator.content_id, request.content_key)
            }
            ContentVerification::AuthenticatedSegment => {
                self.get_previously_verified_content(&request.locator.content_id)
            }
        };
        match cached {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(CacheError::ContentIdMismatch { .. }) => {
                self.evict_content(&request.locator.content_id)?;
            }
            Err(CacheError::MissingCachedBytes(_)) => {}
            Err(error) => return Err(error),
        }
        Ok(None)
    }

    fn hydrate_uncached_record<S: ByteStore + ?Sized>(
        &self,
        store: &S,
        request: RangeHydrationRequest<'_>,
        reader: &mut Option<CachedPackReader>,
    ) -> Result<Vec<u8>, CacheError> {
        let (pack_id, offset, length) = packed_parts(request.locator)?;
        let (encrypted_record, from_pack_cache) = match reader.as_mut() {
            Some(cached_reader) => {
                match cached_reader.read_range_for(request.object_key, offset, length) {
                    Ok(bytes) => (bytes, true),
                    Err(CacheError::InvalidCachedPackRange { .. })
                    | Err(CacheError::ShortCachedPackRead { .. })
                    | Err(CacheError::Pack(_)) => {
                        drop(reader.take());
                        self.evict_pack(request.object_key)?;
                        (
                            store.get_range(request.object_key, ByteRange::new(offset, length))?,
                            false,
                        )
                    }
                    Err(error) => return Err(error),
                }
            }
            None => (
                store.get_range(request.object_key, ByteRange::new(offset, length))?,
                false,
            ),
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
                drop(reader.take());
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
        if request.content_verification == ContentVerification::WorkspaceKeyed
            && !content_matches(&request, &bytes)
        {
            return Err(CacheError::ContentIdMismatch {
                expected: request.locator.content_id.clone(),
            });
        }
        self.put_content(&request.locator.content_id, &bytes)?;
        self.put_content_verification(&request.locator.content_id, &bytes)?;
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

    fn content_verification_path(&self, content_id: &ContentId) -> Result<PathBuf, CacheError> {
        Ok(self
            .root
            .join("content")
            .join(format!("{}.verified", path_component(content_id.as_str())?)))
    }

    fn put_content_verification(
        &self,
        content_id: &ContentId,
        bytes: &[u8],
    ) -> Result<(), CacheError> {
        write_atomic(
            &self.content_verification_path(content_id)?,
            content_verification_marker(bytes).as_bytes(),
            AtomicWriteOptions::default(),
        )?;
        Ok(())
    }

    fn remove_content_verification(&self, content_id: &ContentId) -> Result<(), CacheError> {
        match fs::remove_file(self.content_verification_path(content_id)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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
}

fn packed_parts(locator: &ContentLocator) -> Result<(&PackId, u64, u64), CacheError> {
    let pack_id = locator
        .pack_id
        .as_ref()
        .ok_or(CacheError::MissingPackedLocator("pack_id"))?;
    let offset = locator
        .offset
        .ok_or(CacheError::MissingPackedLocator("offset"))?;
    let length = locator
        .length
        .ok_or(CacheError::MissingPackedLocator("length"))?;
    Ok((pack_id, offset, length))
}

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    MissingCachedBytes(&'static str),
    MissingPackedLocator(&'static str),
    ContentIdMismatch {
        expected: ContentId,
    },
    InvalidCacheKey(String),
    InvalidCachedPackRange {
        offset: u64,
        length: u64,
        pack_len: u64,
    },
    ShortCachedPackRead {
        expected: u64,
        actual: u64,
    },
    ShortFetchedRange {
        expected: u64,
        actual: u64,
    },
    MismatchedCachedPackReader {
        expected: ObjectKey,
        actual: ObjectKey,
    },
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
            Self::InvalidCachedPackRange {
                offset,
                length,
                pack_len,
            } => write!(
                formatter,
                "cached pack range offset {offset} length {length} exceeds pack length {pack_len}"
            ),
            Self::ShortCachedPackRead { expected, actual } => write!(
                formatter,
                "cached pack range read returned {actual} bytes, expected {expected}"
            ),
            Self::ShortFetchedRange { expected, actual } => write!(
                formatter,
                "fetched record range returned {actual} bytes, expected {expected}"
            ),
            Self::MismatchedCachedPackReader { expected, actual } => write!(
                formatter,
                "cached pack reader for {} cannot read {}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::Store(error) => write!(formatter, "byte store read failed: {error}"),
            Self::Pack(error) => write!(formatter, "pack hydration failed: {error}"),
        }
    }
}

fn content_matches(request: &RangeHydrationRequest<'_>, bytes: &[u8]) -> bool {
    workspace_content_id(request.content_key, bytes) == request.locator.content_id
}

fn content_verification_marker(bytes: &[u8]) -> String {
    content_verification_marker_from_parts(blake3::hash(bytes).as_bytes(), bytes.len() as u64)
}

fn stream_content_marker(file: &mut File) -> Result<([u8; 32], u64), CacheError> {
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        byte_len = byte_len.checked_add(read as u64).ok_or_else(|| {
            CacheError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "verified content length overflowed",
            ))
        })?;
    }
    Ok((*hasher.finalize().as_bytes(), byte_len))
}

fn content_verification_marker_from_parts(digest: &[u8; 32], byte_len: u64) -> String {
    format!(
        "blake3={}\nlength={byte_len}\n",
        blake3::Hash::from_bytes(*digest).to_hex()
    )
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
