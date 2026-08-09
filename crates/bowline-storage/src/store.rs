use std::{
    cell::RefCell,
    error::Error,
    fmt, fs, io,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use bowline_core::{
    fs_atomic::{AtomicWriteOptions, write_atomic, write_atomic_with},
    ids::DeviceId,
};
use serde::{Deserialize, Serialize};

mod clock;
mod object_key;
mod range;
mod recovery;
mod request;

use clock::StoreClock;
pub use object_key::ObjectKey;
#[cfg(test)]
pub(super) use object_key::assert_object_key_does_not_leak_path;
pub use range::ByteRange;
pub use request::{
    ObjectContentId, ObjectHash, PutObjectRequest, PutObjectSource, ReopenableObjectSource,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectKind {
    // Manifest-sync engine (Plan 108/110). These are the sealed-envelope purposes:
    // a file blob (`b_<sealed hash>`) and a workspace manifest (`m_<sealed hash>`).
    WorkspaceFileV1,
    WorkspaceManifestV1,
}

impl ObjectKind {
    /// Token this kind contributes to AEAD associated data.
    ///
    /// Written out rather than derived from the serde rename so that changing
    /// the JSON vocabulary cannot silently change the associated data and brick
    /// every object already sealed under the old bytes. Changing a token here
    /// is exactly that break, and the pinned-byte tests in `envelope` say so.
    pub(crate) const fn associated_data_token(self) -> &'static str {
        match self {
            Self::WorkspaceFileV1 => "workspace-file-v1",
            Self::WorkspaceManifestV1 => "workspace-manifest-v1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetentionState {
    Pending,
    Current,
    OrphanCandidate,
    Retained,
    DeleteEligible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMetadata {
    pub key: ObjectKey,
    pub kind: ObjectKind,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
    pub created_by_device_id: Option<DeviceId>,
    pub created_at_unix_ms: u64,
    pub retention_state: RetentionState,
    #[serde(default)]
    pub retain_until_unix_ms: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ByteStoreMetrics {
    pub put_count: u64,
    pub full_read_count: u64,
    pub range_read_count: u64,
    pub head_count: u64,
    pub delete_count: u64,
    pub conditional_write_conflict_count: u64,
    pub verification_failure_count: u64,
    pub retryable_failure_count: u64,
    pub convex_action_count: u64,
    pub convex_mutation_count: u64,
    pub convex_query_count: u64,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub peak_object_bytes_in_flight: u64,
}

/// A content-addressed store of immutable, already-sealed object bytes.
///
/// Every method is required. There are deliberately no defaulted convenience
/// puts: each one used to discard a guarantee the caller had supplied — the
/// content id that binds an upload intent, the key epoch, or the streaming
/// property of a reader source — and a backend that forgot an override lost
/// that guarantee silently.
pub trait ByteStore {
    fn put(&self, request: PutObjectRequest<'_>) -> Result<ObjectMetadata, ByteStoreError>;

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError>;

    /// Streams a verified object into `writer` without buffering it in memory.
    fn get_object_to_writer(
        &self,
        key: &ObjectKey,
        writer: &mut dyn Write,
    ) -> Result<u64, ByteStoreError>;

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError>;

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError>;

    /// Removes an object. Idempotent: a key whose bytes are already gone is
    /// `Ok(())`, so a sweep that crashed part way through converges on re-run.
    fn delete_object(&self, key: &ObjectKey) -> Result<(), ByteStoreError>;

    fn metrics(&self) -> ByteStoreMetrics;
}

#[derive(Debug)]
pub struct LocalByteStore {
    root: PathBuf,
    clock: StoreClock,
    metrics: RefCell<ByteStoreMetrics>,
}

impl LocalByteStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ByteStoreError> {
        let root = root.into();
        fs::create_dir_all(objects_dir(&root))?;
        Self::reclaim_stale_write_temps(&root)?;
        Ok(Self {
            root,
            clock: StoreClock::system(),
            metrics: RefCell::default(),
        })
    }

    pub fn open_deterministic(
        root: impl Into<PathBuf>,
        start_unix_ms: u64,
    ) -> Result<Self, ByteStoreError> {
        let root = root.into();
        fs::create_dir_all(objects_dir(&root))?;
        Self::reclaim_stale_write_temps(&root)?;
        Ok(Self {
            root,
            clock: StoreClock::deterministic(start_unix_ms),
            metrics: RefCell::default(),
        })
    }

    fn stored_path(&self, key: &ObjectKey) -> PathBuf {
        objects_dir(&self.root).join(key.as_str())
    }

    fn metadata_path(&self, key: &ObjectKey) -> PathBuf {
        objects_dir(&self.root).join(format!("{}.meta.json", key.as_str()))
    }

    pub fn list_object_keys(&self) -> Result<Vec<ObjectKey>, ByteStoreError> {
        let mut keys = Vec::new();
        for entry in fs::read_dir(objects_dir(&self.root))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".meta.json") || is_atomic_temp_sibling(&name) {
                continue;
            }
            keys.push(ObjectKey::new(name)?);
        }
        keys.sort();
        Ok(keys)
    }

    fn metadata_for(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        byte_len: u64,
        hash: ObjectHash,
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> ObjectMetadata {
        ObjectMetadata {
            key,
            kind,
            byte_len,
            hash: hash.into_string(),
            key_epoch,
            created_by_device_id: created_by_device_id.cloned(),
            created_at_unix_ms: self.clock.now_unix_ms(),
            retention_state: RetentionState::Pending,
            retain_until_unix_ms: None,
        }
    }

    fn read_metadata(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        let bytes = fs::read(self.metadata_path(key))
            .map_err(|error| map_missing(error, key, "metadata"))?;
        serde_json::from_slice(&bytes).map_err(|_| ByteStoreError::CorruptObject {
            key: key.clone(),
            reason: "metadata JSON did not parse",
        })
    }

    fn metadata_for_key(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        let metadata = self.read_metadata(key)?;
        if metadata.key != *key {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "metadata key did not match object key",
            });
        }
        Ok(metadata)
    }

    fn write_metadata(&self, metadata: &ObjectMetadata) -> Result<(), ByteStoreError> {
        let bytes = serde_json::to_vec(metadata).expect("object metadata serializes");
        write_atomic(
            &self.metadata_path(&metadata.key),
            &bytes,
            create_new_options(),
        )
        .map_err(|error| map_create_error(error, &metadata.key))
    }

    fn verify_metadata(&self, metadata: &ObjectMetadata) -> Result<(), ByteStoreError> {
        let bytes = fs::read(self.stored_path(&metadata.key))
            .map_err(|error| map_missing(error, &metadata.key, "object"))?;
        verify_object_bytes(metadata, &bytes)
    }

    fn matching_committed_metadata(
        &self,
        expected: &ObjectMetadata,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let metadata = self.metadata_for_key(&expected.key)?;
        if metadata.kind != expected.kind
            || metadata.byte_len != expected.byte_len
            || metadata.hash != expected.hash
            || metadata.key_epoch != expected.key_epoch
        {
            return Err(ByteStoreError::ObjectAlreadyExists(expected.key.clone()));
        }
        self.verify_metadata(&metadata)?;
        Ok(metadata)
    }

    fn commit_metadata_after_object_write(
        &self,
        metadata: &ObjectMetadata,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        match self.write_metadata(metadata) {
            Ok(()) => Ok(metadata.clone()),
            Err(ByteStoreError::ObjectAlreadyExists(_)) => {
                self.matching_committed_metadata(metadata)
            }
            Err(error) => {
                if !self.metadata_path(&metadata.key).exists() {
                    let _ = fs::remove_file(self.stored_path(&metadata.key));
                }
                Err(error)
            }
        }
    }

    fn read_verified_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        let metadata = self.metadata_for_key(key)?;
        let bytes =
            fs::read(self.stored_path(key)).map_err(|error| map_missing(error, key, "object"))?;
        verify_object_bytes(&metadata, &bytes)?;
        Ok(bytes)
    }

    fn verify_range_object_len(
        &self,
        key: &ObjectKey,
        byte_len: u64,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let metadata = self.metadata_for_key(key)?;
        if byte_len != metadata.byte_len {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "object length did not match metadata",
            });
        }
        Ok(metadata)
    }

    /// Streams a source into the object file, hashing as it goes.
    ///
    /// The identity check happens inside the atomic write so a source that
    /// changed underneath us never reaches the committed object path.
    fn write_streamed_object(
        &self,
        request: &PutObjectRequest<'_>,
        reader: &mut dyn Read,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let key = &request.key;
        let path = self.stored_path(key);
        if self.metadata_path(key).exists() {
            return Err(ByteStoreError::ObjectAlreadyExists(key.clone()));
        }

        let mut hasher = blake3::Hasher::new();
        let mut byte_len = 0_u64;
        let mut source_identity_mismatch = false;
        let write_result = write_atomic_with(&path, create_new_options(), |file| {
            let mut buffer = [0_u8; OBJECT_STREAM_BUFFER_BYTES];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                let next_byte_len = byte_len.checked_add(read as u64).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "object length overflow")
                })?;
                if next_byte_len > request.byte_len {
                    source_identity_mismatch = true;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "streamed object source exceeded requested length",
                    ));
                }
                hasher.update(&buffer[..read]);
                byte_len = next_byte_len;
                file.write_all(&buffer[..read])?;
            }
            if byte_len != request.byte_len
                || ObjectHash::from_hasher(&hasher) != request.expected_hash
            {
                source_identity_mismatch = true;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "streamed object source changed identity",
                ));
            }
            Ok(())
        });
        if let Err(error) = write_result {
            if source_identity_mismatch {
                return Err(ByteStoreError::CorruptObject {
                    key: key.clone(),
                    reason: "streamed object source did not match requested identity",
                });
            }
            let error = map_create_error(error, key);
            if matches!(error, ByteStoreError::ObjectAlreadyExists(_)) {
                let observed_hash = ObjectHash::from_hasher(&hasher);
                let metadata = self.metadata_for(
                    key.clone(),
                    request.kind,
                    byte_len,
                    observed_hash.clone(),
                    request.key_epoch,
                    request.created_by_device_id,
                );
                if let Some(metadata) =
                    self.adopt_matching_uncommitted_object(&metadata, byte_len, &observed_hash)?
                {
                    self.record_put_metrics(byte_len);
                    return Ok(metadata);
                }
            }
            return Err(error);
        }

        let metadata = self.metadata_for(
            key.clone(),
            request.kind,
            byte_len,
            ObjectHash::from_hasher(&hasher),
            request.key_epoch,
            request.created_by_device_id,
        );
        let metadata = self.commit_metadata_after_object_write(&metadata)?;

        self.record_put_metrics(byte_len);

        Ok(metadata)
    }

    fn write_buffered_object(
        &self,
        request: &PutObjectRequest<'_>,
        bytes: &[u8],
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let key = &request.key;
        let observed_hash = ObjectHash::of_bytes(bytes);
        if bytes.len() as u64 != request.byte_len || observed_hash != request.expected_hash {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "object source did not match requested identity",
            });
        }
        let path = self.stored_path(key);
        if self.metadata_path(key).exists() {
            return Err(ByteStoreError::ObjectAlreadyExists(key.clone()));
        }

        let metadata = self.metadata_for(
            key.clone(),
            request.kind,
            request.byte_len,
            observed_hash.clone(),
            request.key_epoch,
            request.created_by_device_id,
        );
        if let Err(error) = write_atomic(&path, bytes, create_new_options()) {
            let error = map_create_error(error, key);
            if matches!(error, ByteStoreError::ObjectAlreadyExists(_)) {
                if let Some(metadata) = self.adopt_matching_uncommitted_object(
                    &metadata,
                    request.byte_len,
                    &observed_hash,
                )? {
                    self.record_put_metrics(request.byte_len);
                    return Ok(metadata);
                }
                return Err(ByteStoreError::ObjectAlreadyExists(key.clone()));
            }
            return Err(error);
        }
        let metadata = self.commit_metadata_after_object_write(&metadata)?;

        self.record_put_metrics(request.byte_len);

        Ok(metadata)
    }

    /// Removes write temps left behind by a crash.
    ///
    /// Nothing else reclaims them: `list_object_keys` deliberately skips them,
    /// so without this sweep an interrupted sync silently costs the user disk
    /// in files they will never find.
    fn reclaim_stale_write_temps(root: &Path) -> Result<(), ByteStoreError> {
        let now = SystemTime::now();
        for entry in fs::read_dir(objects_dir(root))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_atomic_temp_sibling(&name) {
                continue;
            }
            // A temp older than this cannot belong to an in-flight write: every
            // atomic write renames or removes its temp within one operation.
            // Age rather than owning-pid keeps a concurrent process's in-flight
            // write safe.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= STALE_WRITE_TEMP_AGE);
            if !stale {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(ByteStoreError::Io(error)),
            }
        }
        Ok(())
    }
}

impl ByteStore for LocalByteStore {
    fn put(&self, request: PutObjectRequest<'_>) -> Result<ObjectMetadata, ByteStoreError> {
        // `content_id` names an upload intent, which only a remote backend
        // creates; a local put has nothing to bind it to. Every other field is
        // enforced: byte_len and expected_hash are checked against the source
        // before anything is committed.
        match request.source {
            PutObjectSource::Bytes(bytes) => self.write_buffered_object(&request, bytes),
            PutObjectSource::Reader(source) => {
                let mut reader = source.open()?;
                self.write_streamed_object(&request, reader.as_mut())
            }
        }
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        let bytes = self.read_verified_object(key)?;
        let mut metrics = self.metrics.borrow_mut();
        metrics.full_read_count += 1;
        metrics.bytes_downloaded += bytes.len() as u64;
        Ok(bytes)
    }

    /// Verifies the stored object, then streams it straight into `writer`.
    ///
    /// Two passes over the immutable object file rather than one pass into a
    /// verified temp copy: the temp cost a full extra write plus an fsync plus
    /// 2x free space per hydration, and survived at full size on a crash.
    fn get_object_to_writer(
        &self,
        key: &ObjectKey,
        writer: &mut dyn Write,
    ) -> Result<u64, ByteStoreError> {
        let metadata = self.metadata_for_key(key)?;
        let path = self.stored_path(key);
        let mut source =
            fs::File::open(&path).map_err(|error| map_missing(error, key, "object"))?;
        let observed_hash = ObjectHash::of_reader(&mut source).map_err(ByteStoreError::Io)?;
        if observed_hash.as_str() != metadata.hash {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "object bytes did not match metadata",
            });
        }

        let mut source =
            fs::File::open(&path).map_err(|error| map_missing(error, key, "object"))?;
        let copied = io::copy(&mut source, writer).map_err(ByteStoreError::Io)?;
        if copied != metadata.byte_len {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "object bytes did not match metadata",
            });
        }

        let mut metrics = self.metrics.borrow_mut();
        metrics.full_read_count += 1;
        metrics.bytes_downloaded += copied;
        Ok(copied)
    }

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        let path = self.stored_path(key);
        let mut file = fs::File::open(&path).map_err(|error| map_missing(error, key, "object"))?;
        let byte_len = file.metadata().map_err(ByteStoreError::Io)?.len();
        let metadata = self.verify_range_object_len(key, byte_len)?;
        range.checked_end(metadata.byte_len)?;
        file.seek(SeekFrom::Start(range.offset))
            .map_err(ByteStoreError::Io)?;
        let range_len =
            usize::try_from(range.length).map_err(|_| ByteStoreError::RangeOutOfBounds {
                offset: range.offset,
                length: range.length,
                byte_len,
            })?;
        let mut selected = vec![0_u8; range_len];
        file.read_exact(&mut selected).map_err(ByteStoreError::Io)?;
        let mut metrics = self.metrics.borrow_mut();
        metrics.range_read_count += 1;
        metrics.bytes_downloaded += selected.len() as u64;
        Ok(selected)
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        let metadata = self.metadata_for_key(key)?;
        self.verify_metadata(&metadata)?;
        self.metrics.borrow_mut().head_count += 1;
        Ok(metadata)
    }

    fn delete_object(&self, key: &ObjectKey) -> Result<(), ByteStoreError> {
        let metadata_path = self.metadata_path(key);
        let object_path = self.stored_path(key);
        // Verify only a complete object. A half-deleted pair in either
        // direction is residue to reclaim, not something to authenticate.
        if metadata_path.exists() && object_path.exists() {
            let metadata = self.metadata_for_key(key)?;
            self.verify_metadata(&metadata)?;
        }
        // Blob first. Metadata is the commit record: an object without it is
        // already invisible to every read path, and `adopt_matching_uncommitted_object`
        // relies on exactly that. Removing metadata first instead strands a blob
        // that no later sweep can name, which is how GC used to wedge.
        let removed_object = remove_file_if_present(&object_path)?;
        let removed_metadata = remove_file_if_present(&metadata_path)?;
        if removed_object || removed_metadata {
            self.metrics.borrow_mut().delete_count += 1;
        }
        Ok(())
    }

    fn metrics(&self) -> ByteStoreMetrics {
        *self.metrics.borrow()
    }
}

#[derive(Debug)]
pub enum ByteStoreError {
    Io(io::Error),
    Network {
        operation: TransferOperation,
        detail: String,
    },
    HttpStatus {
        key: ObjectKey,
        operation: TransferOperation,
        status: u16,
    },
    IntentFailed {
        operation: TransferOperation,
        kind: IntentFailureKind,
        detail: String,
    },
    InvalidObjectKey {
        key: String,
        reason: &'static str,
    },
    ObjectAlreadyExists(ObjectKey),
    MissingObject {
        key: ObjectKey,
        component: &'static str,
    },
    CorruptObject {
        key: ObjectKey,
        reason: &'static str,
    },
    IntegrityViolation {
        key: ObjectKey,
        reason: &'static str,
    },
    CorruptJournal {
        component: &'static str,
        reason: &'static str,
    },
    RangeOutOfBounds {
        offset: u64,
        length: u64,
        byte_len: u64,
    },
    UnsupportedOperation(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOperation {
    Upload,
    Download,
    Delete,
}

impl fmt::Display for TransferOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upload => formatter.write_str("upload"),
            Self::Download => formatter.write_str("download"),
            Self::Delete => formatter.write_str("delete"),
        }
    }
}

// Control-plane depends on storage, so transfer.rs owns the one-way mapping
// into this storage-local intent failure vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentFailureKind {
    Timeout,
    Transport,
    DeviceNotTrusted,
    Other,
}

impl fmt::Display for ByteStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "byte store I/O failed: {error}"),
            Self::Network { operation, detail } => {
                write!(formatter, "R2 {operation} transport failed: {detail}")
            }
            Self::HttpStatus {
                key,
                operation,
                status,
            } => write!(
                formatter,
                "R2 {operation} for object `{key}` returned HTTP {status}"
            ),
            Self::IntentFailed {
                operation, detail, ..
            } => write!(formatter, "{operation} intent failed: {detail}"),
            Self::InvalidObjectKey { key, reason } => {
                write!(formatter, "invalid object key `{key}`: {reason}")
            }
            Self::ObjectAlreadyExists(key) => {
                write!(formatter, "immutable object `{key}` already exists")
            }
            Self::MissingObject { key, component } => {
                write!(formatter, "missing {component} for object `{key}`")
            }
            Self::CorruptObject { key, reason } => {
                write!(formatter, "corrupt object `{key}`: {reason}")
            }
            Self::IntegrityViolation { key, reason } => {
                write!(
                    formatter,
                    "immutable object `{key}` violated identity: {reason}"
                )
            }
            Self::CorruptJournal { component, reason } => {
                write!(formatter, "corrupt {component}: {reason}")
            }
            Self::RangeOutOfBounds {
                offset,
                length,
                byte_len,
            } => write!(
                formatter,
                "range {offset}+{length} is outside object length {byte_len}"
            ),
            Self::UnsupportedOperation(operation) => {
                write!(
                    formatter,
                    "byte store operation `{operation}` is unsupported"
                )
            }
        }
    }
}

impl Error for ByteStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Network { .. }
            | Self::HttpStatus { .. }
            | Self::IntentFailed { .. }
            | Self::InvalidObjectKey { .. }
            | Self::ObjectAlreadyExists(_)
            | Self::MissingObject { .. }
            | Self::CorruptObject { .. }
            | Self::IntegrityViolation { .. }
            | Self::CorruptJournal { .. }
            | Self::RangeOutOfBounds { .. }
            | Self::UnsupportedOperation(_) => None,
        }
    }
}

impl From<io::Error> for ByteStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn stable_object_hash(bytes: &[u8]) -> String {
    ObjectHash::of_bytes(bytes).into_string()
}

pub fn stable_object_hash_reader(reader: &mut dyn Read) -> io::Result<String> {
    Ok(ObjectHash::of_reader(reader)?.into_string())
}

fn verify_object_bytes(metadata: &ObjectMetadata, bytes: &[u8]) -> Result<(), ByteStoreError> {
    if bytes.len() as u64 != metadata.byte_len
        || ObjectHash::of_bytes(bytes).as_str() != metadata.hash
    {
        return Err(ByteStoreError::CorruptObject {
            key: metadata.key.clone(),
            reason: "object bytes did not match metadata",
        });
    }
    Ok(())
}

const OBJECT_STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// How long a write temp must sit untouched before it counts as crash residue.
/// Generous on purpose: an atomic write renames or removes its temp within one
/// operation, so nothing legitimate ever survives this long.
const STALE_WRITE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

fn create_new_options() -> AtomicWriteOptions {
    AtomicWriteOptions {
        replace_existing: false,
        ..AtomicWriteOptions::default()
    }
}

/// Removes `path` if it exists. Returns whether anything was removed, so
/// callers can tell an idempotent no-op from real work.
fn remove_file_if_present(path: &Path) -> Result<bool, ByteStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ByteStoreError::Io(error)),
    }
}

fn is_atomic_temp_sibling(file_name: &str) -> bool {
    file_name.starts_with('.') && file_name.ends_with(".bowline-tmp")
}

fn objects_dir(root: &Path) -> PathBuf {
    root.join("objects")
}

fn map_missing(error: io::Error, key: &ObjectKey, component: &'static str) -> ByteStoreError {
    if error.kind() == io::ErrorKind::NotFound {
        ByteStoreError::MissingObject {
            key: key.clone(),
            component,
        }
    } else {
        ByteStoreError::Io(error)
    }
}

fn map_create_error(error: io::Error, key: &ObjectKey) -> ByteStoreError {
    if error.kind() == io::ErrorKind::AlreadyExists {
        ByteStoreError::ObjectAlreadyExists(key.clone())
    } else {
        ByteStoreError::Io(error)
    }
}

#[cfg(test)]
mod tests;
