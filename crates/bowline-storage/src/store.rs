use std::{
    cell::RefCell,
    error::Error,
    fmt, fs, io,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::ids::{DeviceId, ManifestId, PackId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey(String);

impl ObjectKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ByteStoreError> {
        let value = value.into();
        validate_opaque_object_key(&value)?;
        Ok(Self(value))
    }

    pub fn from_pack_id(pack_id: &PackId) -> Result<Self, ByteStoreError> {
        Self::new(format!("packs_{}", pack_id.as_str()))
    }

    pub fn from_manifest_id(manifest_id: &ManifestId) -> Result<Self, ByteStoreError> {
        Self::new(format!("manifests_{}", manifest_id.as_str()))
    }

    pub fn from_index_pack_id(index_pack_id: &str) -> Result<Self, ByteStoreError> {
        Self::new(format!("indexes_{index_pack_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ObjectKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ObjectKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectKind {
    SourcePack,
    IndexPack,
    LargeChunk,
    SnapshotManifest,
    LocatorIndex,
    AgentOverlay,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

impl ByteRange {
    pub fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    fn checked_end(self, byte_len: u64) -> Result<u64, ByteStoreError> {
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(ByteStoreError::RangeOutOfBounds {
                offset: self.offset,
                length: self.length,
                byte_len,
            })?;

        if end <= byte_len {
            Ok(end)
        } else {
            Err(ByteStoreError::RangeOutOfBounds {
                offset: self.offset,
                length: self.length,
                byte_len,
            })
        }
    }
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

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError>;

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError>;

    fn delete_object(&self, _key: &ObjectKey) -> Result<(), ByteStoreError> {
        Err(ByteStoreError::UnsupportedOperation("delete_object"))
    }

    fn metrics(&self) -> ByteStoreMetrics;
}

#[derive(Debug)]
pub struct LocalByteStore {
    root: PathBuf,
    clock: TestClock,
    metrics: RefCell<ByteStoreMetrics>,
}

impl LocalByteStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ByteStoreError> {
        let root = root.into();
        fs::create_dir_all(objects_dir(&root))?;
        Ok(Self {
            root,
            clock: TestClock::system(),
            metrics: RefCell::default(),
        })
    }

    pub fn open_deterministic(
        root: impl Into<PathBuf>,
        start_unix_ms: u64,
    ) -> Result<Self, ByteStoreError> {
        let root = root.into();
        fs::create_dir_all(objects_dir(&root))?;
        Ok(Self {
            root,
            clock: TestClock::deterministic(start_unix_ms),
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
            if name.ends_with(".meta.json") {
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
        bytes: &[u8],
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> ObjectMetadata {
        ObjectMetadata {
            key,
            kind,
            byte_len: bytes.len() as u64,
            hash: stable_object_hash(bytes),
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
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.metadata_path(&metadata.key))
            .map_err(|error| map_create_error(error, &metadata.key))?;
        file.write_all(&bytes).map_err(ByteStoreError::Io)?;
        Ok(())
    }

    fn verify_metadata(&self, metadata: &ObjectMetadata) -> Result<(), ByteStoreError> {
        let bytes = fs::read(self.stored_path(&metadata.key))
            .map_err(|error| map_missing(error, &metadata.key, "object"))?;
        verify_object_bytes(metadata, &bytes)
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

        let metadata = self.metadata_for(key.clone(), kind, bytes, key_epoch, created_by_device_id);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| map_create_error(error, &key))?;
        file.write_all(bytes).map_err(ByteStoreError::Io)?;
        if let Err(error) = self.write_metadata(&metadata) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }

        let mut metrics = self.metrics.borrow_mut();
        metrics.put_count += 1;
        metrics.bytes_uploaded += bytes.len() as u64;

        Ok(metadata)
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        let bytes = self.read_verified_object(key)?;
        let mut metrics = self.metrics.borrow_mut();
        metrics.full_read_count += 1;
        metrics.bytes_downloaded += bytes.len() as u64;
        Ok(bytes)
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
        fs::remove_file(self.stored_path(key))
            .map_err(|error| map_missing(error, key, "object"))?;
        fs::remove_file(self.metadata_path(key))
            .map_err(|error| map_missing(error, key, "metadata"))?;
        self.metrics.borrow_mut().delete_count += 1;
        Ok(())
    }

    fn metrics(&self) -> ByteStoreMetrics {
        *self.metrics.borrow()
    }
}

#[derive(Debug)]
pub enum ByteStoreError {
    Io(io::Error),
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
    RangeOutOfBounds {
        offset: u64,
        length: u64,
        byte_len: u64,
    },
    UnsupportedOperation(&'static str),
}

impl fmt::Display for ByteStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "byte store I/O failed: {error}"),
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
            _ => None,
        }
    }
}

impl From<io::Error> for ByteStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ObjectKeyLeak {
    pub object_key: String,
    pub leaked_segment: String,
}

#[cfg(test)]
impl fmt::Display for ObjectKeyLeak {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "object key `{}` leaks path segment `{}`",
            self.object_key, self.leaked_segment
        )
    }
}

#[cfg(test)]
impl Error for ObjectKeyLeak {}

fn validate_opaque_object_key(key: &str) -> Result<(), ByteStoreError> {
    if key.is_empty() {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "empty keys are not allowed",
        });
    }
    if key.len() > 180 {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "key is too long for the local storage contract",
        });
    }
    if key.contains('/') || key.contains('\\') || key.contains('.') {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "path separators and dotted names are not allowed",
        });
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "only ASCII alphanumeric, dash, and underscore are allowed",
        });
    }

    if !(matches_opaque_storage_key(key, "packs_pk_", 16)
        || matches_opaque_storage_key(key, "manifests_mf_", 16)
        || matches_opaque_storage_key(key, "indexes_ix_", 16))
    {
        return Err(ByteStoreError::InvalidObjectKey {
            key: key.to_string(),
            reason: "object keys must be generated opaque pack, manifest, index-pack, or overlay keys",
        });
    }

    Ok(())
}

#[cfg(test)]
fn assert_object_key_does_not_leak_path(
    object_key: &str,
    source_path: impl AsRef<Path>,
) -> Result<(), ObjectKeyLeak> {
    for component in source_path.as_ref().components() {
        let segment = component.as_os_str().to_string_lossy();
        if segment.len() >= 3 && object_key.contains(segment.as_ref()) {
            return Err(ObjectKeyLeak {
                object_key: object_key.to_string(),
                leaked_segment: segment.into_owned(),
            });
        }
    }
    Ok(())
}

pub(crate) fn stable_object_hash(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    format!("b3_{}", hash.to_hex())
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

fn objects_dir(root: &Path) -> PathBuf {
    root.join("objects")
}

fn matches_opaque_storage_key(key: &str, prefix: &str, min_suffix_len: usize) -> bool {
    let Some(suffix) = key.strip_prefix(prefix) else {
        return false;
    };
    suffix.len() >= min_suffix_len
        && suffix.len() <= 80
        && suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
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

#[derive(Debug)]
struct TestClock {
    mode: ClockMode,
}

#[derive(Debug)]
enum ClockMode {
    System,
    Deterministic(RefCell<u64>),
}

impl TestClock {
    fn system() -> Self {
        Self {
            mode: ClockMode::System,
        }
    }

    fn deterministic(start_unix_ms: u64) -> Self {
        Self {
            mode: ClockMode::Deterministic(RefCell::new(start_unix_ms)),
        }
    }

    fn now_unix_ms(&self) -> u64 {
        match &self.mode {
            ClockMode::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after Unix epoch")
                .as_millis() as u64,
            ClockMode::Deterministic(next) => {
                let mut next = next.borrow_mut();
                let current = *next;
                *next += 1;
                current
            }
        }
    }
}

#[cfg(test)]
mod tests;
