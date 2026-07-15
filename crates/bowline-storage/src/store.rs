use std::{
    cell::RefCell,
    error::Error,
    fmt, fs, io,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use bowline_core::{
    fs_atomic::{AtomicWriteOptions, write_atomic, write_atomic_with},
    ids::DeviceId,
};
use serde::{Deserialize, Serialize};

mod clock;
mod journal;
mod object_key;
mod range;
mod recovery;
mod request;
mod streaming;

use clock::StoreClock;
use journal::upload_journal_dir;
pub use journal::{
    SourcePackUploadJournalDigest, SourcePackUploadJournalEntry, SourcePackUploadJournalKey,
    SourcePackUploadJournalObjectHash, SourcePackUploadJournalPointer,
};
pub use object_key::ObjectKey;
#[cfg(test)]
pub(super) use object_key::assert_object_key_does_not_leak_path;
pub use range::ByteRange;
use request::read_verified_source;
pub use request::{ObjectContentId, ObjectHash, PutObjectReaderRequest, ReopenableObjectSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectKind {
    SourcePack,
    SnapshotManifest,
    SnapshotMetadataPage,
    LocatorIndex,
    AgentOverlay,
    ConflictBundle,
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

pub trait ByteStore {
    fn put_object(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError>;

    fn put_object_with_content_id(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        _content_id: &str,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.put_object(key, kind, bytes, created_by_device_id)
    }

    fn put_object_with_content_id_at_epoch(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        content_id: &str,
        bytes: &[u8],
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let metadata = self.put_object_with_content_id(
            key.clone(),
            kind,
            content_id,
            bytes,
            created_by_device_id,
        )?;
        if metadata.key_epoch == key_epoch {
            Ok(metadata)
        } else {
            Err(ByteStoreError::CorruptObject {
                key,
                reason: "object metadata key epoch did not match requested epoch",
            })
        }
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError>;

    fn put_object_reader(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        reader: &mut dyn Read,
        _byte_len_hint: Option<u64>,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        self.put_object(key, kind, &bytes, created_by_device_id)
    }

    fn put_object_reader_with_content_id_at_epoch(
        &self,
        request: PutObjectReaderRequest<'_>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let bytes = read_verified_source(
            &request.key,
            request.source.open()?.as_mut(),
            request.byte_len,
            request.expected_hash.as_str(),
        )?;
        self.put_object_with_content_id_at_epoch(
            request.key,
            request.kind,
            request.content_id.as_str(),
            &bytes,
            request.key_epoch,
            request.created_by_device_id,
        )
    }

    fn supports_streaming_puts(&self) -> bool {
        false
    }

    fn get_object_to_writer(
        &self,
        key: &ObjectKey,
        writer: &mut dyn Write,
    ) -> Result<u64, ByteStoreError> {
        let bytes = self.get_object(key)?;
        writer.write_all(&bytes)?;
        Ok(bytes.len() as u64)
    }

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError>;

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError>;

    fn delete_object(&self, _key: &ObjectKey) -> Result<(), ByteStoreError> {
        Err(ByteStoreError::UnsupportedOperation("delete_object"))
    }

    fn creates_upload_intents(&self) -> bool {
        false
    }

    fn source_pack_upload_journal(
        &self,
        _key: &SourcePackUploadJournalKey,
    ) -> Result<Vec<SourcePackUploadJournalEntry>, ByteStoreError> {
        Ok(Vec::new())
    }

    fn record_source_pack_upload_journal(
        &self,
        _key: &SourcePackUploadJournalKey,
        _entry: &SourcePackUploadJournalEntry,
    ) -> Result<(), ByteStoreError> {
        Ok(())
    }

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
        fs::create_dir_all(upload_journal_dir(&root))?;
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
        fs::create_dir_all(upload_journal_dir(&root))?;
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
        hash: String,
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> ObjectMetadata {
        ObjectMetadata {
            key,
            kind,
            byte_len,
            hash,
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

    fn put_object_reader_at_epoch(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        reader: &mut dyn Read,
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
        expected_identity: Option<(u64, &str)>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let path = self.stored_path(&key);
        if self.metadata_path(&key).exists() {
            return Err(ByteStoreError::ObjectAlreadyExists(key));
        }

        let mut hasher = blake3::Hasher::new();
        let mut byte_len = 0_u64;
        let mut source_identity_mismatch = false;
        let write_result = write_atomic_with(&path, create_new_options(), |file| {
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                let next_byte_len = byte_len.checked_add(read as u64).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "object length overflow")
                })?;
                if expected_identity.is_some_and(|(expected_len, _)| next_byte_len > expected_len) {
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
            let actual_hash = format!("b3_{}", hasher.clone().finalize().to_hex());
            if expected_identity.is_some_and(|(expected_len, expected_hash)| {
                byte_len != expected_len || actual_hash != expected_hash
            }) {
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
                    key,
                    reason: "streamed object source did not match requested identity",
                });
            }
            let error = map_create_error(error, &key);
            if matches!(error, ByteStoreError::ObjectAlreadyExists(_)) {
                let expected_hash = format!("b3_{}", hasher.finalize().to_hex());
                let metadata = self.metadata_for(
                    key.clone(),
                    kind,
                    byte_len,
                    expected_hash.clone(),
                    key_epoch,
                    created_by_device_id,
                );
                if let Some(metadata) =
                    self.adopt_matching_uncommitted_object(&metadata, byte_len, &expected_hash)?
                {
                    self.record_put_metrics(byte_len);
                    return Ok(metadata);
                }
            }
            return Err(error);
        }

        let metadata = self.metadata_for(
            key.clone(),
            kind,
            byte_len,
            format!("b3_{}", hasher.finalize().to_hex()),
            key_epoch,
            created_by_device_id,
        );
        let metadata = self.commit_metadata_after_object_write(&metadata)?;

        self.record_put_metrics(byte_len);

        Ok(metadata)
    }
}

impl ByteStore for LocalByteStore {
    fn put_object(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.put_object_with_content_id_at_epoch(
            key,
            kind,
            &stable_object_hash(bytes),
            bytes,
            CURRENT_WRITE_KEY_EPOCH,
            created_by_device_id,
        )
    }

    fn put_object_with_content_id_at_epoch(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        _content_id: &str,
        bytes: &[u8],
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let path = self.stored_path(&key);
        if self.metadata_path(&key).exists() {
            return Err(ByteStoreError::ObjectAlreadyExists(key));
        }

        let metadata = self.metadata_for(
            key.clone(),
            kind,
            bytes.len() as u64,
            stable_object_hash(bytes),
            key_epoch,
            created_by_device_id,
        );
        if let Err(error) = write_atomic(&path, bytes, create_new_options()) {
            let error = map_create_error(error, &key);
            if matches!(error, ByteStoreError::ObjectAlreadyExists(_)) {
                if let Some(metadata) = self.adopt_matching_uncommitted_object(
                    &metadata,
                    bytes.len() as u64,
                    &stable_object_hash(bytes),
                )? {
                    self.record_put_metrics(bytes.len() as u64);
                    return Ok(metadata);
                }
                return Err(ByteStoreError::ObjectAlreadyExists(key));
            } else {
                return Err(error);
            }
        }
        let metadata = self.commit_metadata_after_object_write(&metadata)?;

        self.record_put_metrics(bytes.len() as u64);

        Ok(metadata)
    }

    fn put_object_reader(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        reader: &mut dyn Read,
        _byte_len_hint: Option<u64>,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.put_object_reader_at_epoch(
            key,
            kind,
            reader,
            CURRENT_WRITE_KEY_EPOCH,
            created_by_device_id,
            None,
        )
    }

    fn put_object_reader_with_content_id_at_epoch(
        &self,
        request: PutObjectReaderRequest<'_>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let mut source = request.source.open()?;
        self.put_object_reader_at_epoch(
            request.key,
            request.kind,
            source.as_mut(),
            request.key_epoch,
            request.created_by_device_id,
            Some((request.byte_len, request.expected_hash.as_str())),
        )
    }

    fn supports_streaming_puts(&self) -> bool {
        true
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        let bytes = self.read_verified_object(key)?;
        let mut metrics = self.metrics.borrow_mut();
        metrics.full_read_count += 1;
        metrics.bytes_downloaded += bytes.len() as u64;
        Ok(bytes)
    }

    fn get_object_to_writer(
        &self,
        key: &ObjectKey,
        writer: &mut dyn Write,
    ) -> Result<u64, ByteStoreError> {
        let (temp_path, byte_len) = streaming::write_verified_object_to_temp(
            &self.root,
            key,
            self.metadata_for_key(key)?,
            self.stored_path(key),
        )?;
        let copy_result = (|| {
            let mut temp = fs::File::open(&temp_path)?;
            let copied = io::copy(&mut temp, writer)?;
            if copied != byte_len {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "verified object copy wrote an unexpected byte count",
                ));
            }
            Ok(())
        })();
        let cleanup_result = fs::remove_file(&temp_path);
        if let Err(error) = copy_result {
            return Err(ByteStoreError::Io(error));
        }
        cleanup_result.map_err(ByteStoreError::Io)?;
        let mut metrics = self.metrics.borrow_mut();
        metrics.full_read_count += 1;
        metrics.bytes_downloaded += byte_len;
        Ok(byte_len)
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
        let metadata = self.metadata_for_key(key)?;
        self.verify_metadata(&metadata)?;
        fs::remove_file(self.metadata_path(key))
            .map_err(|error| map_missing(error, key, "metadata"))?;
        fs::remove_file(self.stored_path(key))
            .map_err(|error| map_missing(error, key, "object"))?;
        self.metrics.borrow_mut().delete_count += 1;
        Ok(())
    }

    fn source_pack_upload_journal(
        &self,
        key: &SourcePackUploadJournalKey,
    ) -> Result<Vec<SourcePackUploadJournalEntry>, ByteStoreError> {
        self.source_pack_upload_journal_entries(key)
    }

    fn record_source_pack_upload_journal(
        &self,
        key: &SourcePackUploadJournalKey,
        entry: &SourcePackUploadJournalEntry,
    ) -> Result<(), ByteStoreError> {
        self.record_source_pack_upload_journal_entry(key, entry)
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
    stable_object_hash_reader(&mut Cursor::new(bytes)).expect("slice hashing does not fail")
}

pub fn stable_object_hash_reader(reader: &mut dyn Read) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("b3_{}", hasher.finalize().to_hex()))
}

fn verify_object_bytes(metadata: &ObjectMetadata, bytes: &[u8]) -> Result<(), ByteStoreError> {
    if bytes.len() as u64 != metadata.byte_len || stable_object_hash(bytes) != metadata.hash {
        return Err(ByteStoreError::CorruptObject {
            key: metadata.key.clone(),
            reason: "object bytes did not match metadata",
        });
    }
    Ok(())
}

const CURRENT_WRITE_KEY_EPOCH: u32 = 1;

fn create_new_options() -> AtomicWriteOptions {
    AtomicWriteOptions {
        replace_existing: false,
        ..AtomicWriteOptions::default()
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
