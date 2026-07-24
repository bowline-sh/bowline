use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::device_keys::{
    AccountTokens, DeviceIdentity, DeviceKeyError, DeviceKeyStore, DeviceProofVerifier,
    SecretUnavailableReason, WorkspaceKeyMaterial,
};
pub use bowline_control_plane::{
    ByteRange, CompactEvent, CompareAndSwapError, ControlPlaneClient, ControlPlaneError,
    ControlPlaneResult, ControlPlaneTimestamp as FakeTimestamp, DeterministicClock,
    DeterministicIdGenerator, DeviceApproval as FakeDeviceGrant, DeviceApprovalInput,
    DeviceRequest as FakeDeviceRequest, DeviceRequestInput, DownloadIntent, DownloadIntentRequest,
    FakeControlPlaneClient, SignedUrlIntent, StaleWorkspaceRef, UploadIntent, UploadIntentRequest,
    WorkspaceControlPlaneClient, WorkspaceRef, is_opaque_object_key,
};
use bowline_core::ids::WorkspaceId;

const BYTE_STORE_MASTER_KEY: &str = "bowline-local/fake-byte-store/master-key";
const DEFAULT_BYTE_STORE_KEY: &[u8] = b"bowline-local deterministic fake byte store key";
const ENCRYPTED_OBJECT_HEADER: &[u8] = b"bowline-fake-encrypted-v1\n";

#[derive(Debug, Clone, Default)]
pub struct FakeKeychain {
    secrets: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl DeviceKeyStore for FakeKeychain {
    fn load_or_create_device_identity(&self) -> Result<DeviceIdentity, DeviceKeyError> {
        if let Some(secret) = self.get_secret("device-identity-v1") {
            let secret = String::from_utf8(secret)
                .map_err(|error| DeviceKeyError::CorruptSecret(error.to_string()))?;
            return DeviceIdentity::parse(secret);
        }

        let identity = DeviceIdentity::try_generate()?;
        self.put_secret(
            "device-identity-v1",
            identity.persisted_secret()?.as_bytes().to_vec(),
        );
        Ok(identity)
    }

    fn store_account_tokens(&self, tokens: AccountTokens) -> Result<(), DeviceKeyError> {
        self.put_secret("account-tokens-v1", serde_json::to_vec(&tokens)?);
        Ok(())
    }

    fn load_account_tokens(&self) -> Result<Option<AccountTokens>, DeviceKeyError> {
        self.get_secret("account-tokens-v1")
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    fn clear_account_tokens(&self) -> Result<bool, DeviceKeyError> {
        Ok(self.delete_secret("account-tokens-v1").is_some())
    }

    fn store_workspace_key(&self, key: WorkspaceKeyMaterial) -> Result<(), DeviceKeyError> {
        self.put_secret(
            format!("workspace-key-v1:{}", key.workspace_id.as_str()),
            serde_json::to_vec(&key)?,
        );
        Ok(())
    }

    fn load_workspace_key(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyMaterial>, DeviceKeyError> {
        self.get_secret(&format!("workspace-key-v1:{}", workspace_id.as_str()))
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    fn store_device_proof_verifier(
        &self,
        verifier: DeviceProofVerifier,
    ) -> Result<(), DeviceKeyError> {
        let mut secrets = self.secrets.lock().expect("fake keychain poisoned");
        #[cfg(test)]
        crate::device_keys::transaction_entered(&self.verifier_transaction_key());
        let mut verifiers: Vec<DeviceProofVerifier> = secrets
            .get("device-proof-verifiers-v1")
            .map(|bytes| serde_json::from_slice(bytes))
            .transpose()?
            .unwrap_or_default();
        verifiers.retain(|existing| {
            existing.workspace_id != verifier.workspace_id
                || existing.device_id != verifier.device_id
        });
        verifiers.push(verifier);
        secrets.insert(
            "device-proof-verifiers-v1".to_string(),
            serde_json::to_vec(&verifiers)?,
        );
        Ok(())
    }

    fn load_device_proof_verifiers(&self) -> Result<Vec<DeviceProofVerifier>, DeviceKeyError> {
        self.get_secret("device-proof-verifiers-v1")
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn replace_device_proof_verifiers_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        verifiers: Vec<DeviceProofVerifier>,
    ) -> Result<(), DeviceKeyError> {
        let mut secrets = self.secrets.lock().expect("fake keychain poisoned");
        #[cfg(test)]
        crate::device_keys::transaction_entered(&self.verifier_transaction_key());
        let mut persisted: Vec<DeviceProofVerifier> = secrets
            .get("device-proof-verifiers-v1")
            .map(|bytes| serde_json::from_slice(bytes))
            .transpose()?
            .unwrap_or_default();
        persisted.retain(|verifier| &verifier.workspace_id != workspace_id);
        persisted.extend(verifiers);
        persisted.sort_by(|left, right| {
            left.workspace_id
                .cmp(&right.workspace_id)
                .then_with(|| left.device_id.cmp(&right.device_id))
        });
        secrets.insert(
            "device-proof-verifiers-v1".to_string(),
            serde_json::to_vec(&persisted)?,
        );
        Ok(())
    }

    fn mark_secret_unavailable(
        &self,
        reason: SecretUnavailableReason,
    ) -> Result<(), DeviceKeyError> {
        self.put_secret("secret-unavailable-reason", serde_json::to_vec(&reason)?);
        Ok(())
    }
}

#[cfg(test)]
impl FakeKeychain {
    pub(crate) fn verifier_transaction_key(&self) -> String {
        format!("fake:{:p}", Arc::as_ptr(&self.secrets))
    }
}

impl FakeKeychain {
    pub fn put_secret(&self, name: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.secrets
            .lock()
            .expect("fake keychain poisoned")
            .insert(name.into(), value.into());
    }

    pub fn get_secret(&self, name: &str) -> Option<Vec<u8>> {
        self.secrets
            .lock()
            .expect("fake keychain poisoned")
            .get(name)
            .cloned()
    }

    pub fn delete_secret(&self, name: &str) -> Option<Vec<u8>> {
        self.secrets
            .lock()
            .expect("fake keychain poisoned")
            .remove(name)
    }

    pub fn secret_names(&self) -> Vec<String> {
        self.secrets
            .lock()
            .expect("fake keychain poisoned")
            .keys()
            .cloned()
            .collect()
    }

    fn ensure_byte_store_key(&self) -> Vec<u8> {
        if let Some(key) = self.get_secret(BYTE_STORE_MASTER_KEY) {
            return key;
        }

        self.put_secret(BYTE_STORE_MASTER_KEY, DEFAULT_BYTE_STORE_KEY.to_vec());
        DEFAULT_BYTE_STORE_KEY.to_vec()
    }
}

pub trait ByteStore {
    fn put_object(&self, bytes: &[u8]) -> Result<ObjectPointer, ByteStoreError>;

    fn get_object(&self, object_key: &str) -> Result<Vec<u8>, ByteStoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Blob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectPointer {
    pub object_key: String,
    pub content_id: String,
    pub byte_len: u64,
    pub kind: ObjectKind,
    pub created_at: FakeTimestamp,
}

#[derive(Debug, Clone)]
pub struct FakeByteStore {
    root: PathBuf,
    keychain: FakeKeychain,
    clock: DeterministicClock,
    ids: DeterministicIdGenerator,
}

impl FakeByteStore {
    pub fn new(
        root: impl Into<PathBuf>,
        keychain: FakeKeychain,
        clock: DeterministicClock,
        ids: DeterministicIdGenerator,
    ) -> Result<Self, ByteStoreError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        keychain.ensure_byte_store_key();

        Ok(Self {
            root,
            keychain,
            clock,
            ids,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn object_path(&self, object_key: &str) -> Result<PathBuf, ByteStoreError> {
        validate_object_key(object_key)?;
        Ok(self.root.join(object_key))
    }

    pub fn stored_bytes(&self, object_key: &str) -> Result<Vec<u8>, ByteStoreError> {
        Ok(fs::read(self.object_path(object_key)?)?)
    }

    pub fn list_object_keys(&self) -> Result<Vec<String>, ByteStoreError> {
        let mut keys = Vec::new();

        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                keys.push(entry.file_name().to_string_lossy().into_owned());
            }
        }

        keys.sort();
        Ok(keys)
    }

    fn encryption_key(&self) -> Result<Vec<u8>, ByteStoreError> {
        self.keychain
            .get_secret(BYTE_STORE_MASTER_KEY)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| ByteStoreError::MissingKey(BYTE_STORE_MASTER_KEY.to_string()))
    }
}

impl ByteStore for FakeByteStore {
    fn put_object(&self, bytes: &[u8]) -> Result<ObjectPointer, ByteStoreError> {
        let object_key = self.ids.next_id("object");
        let pointer = ObjectPointer {
            object_key,
            content_id: format!("content-{:016x}", stable_hash(bytes)),
            byte_len: bytes.len() as u64,
            kind: ObjectKind::Blob,
            created_at: self.clock.now(),
        };
        let encrypted = encrypt_bytes(bytes, &self.encryption_key()?);

        fs::write(self.object_path(&pointer.object_key)?, encrypted)?;

        Ok(pointer)
    }

    fn get_object(&self, object_key: &str) -> Result<Vec<u8>, ByteStoreError> {
        let encrypted = fs::read(self.object_path(object_key)?)?;
        decrypt_bytes(&encrypted, &self.encryption_key()?)
    }
}

#[derive(Debug)]
pub enum ByteStoreError {
    Io(io::Error),
    InvalidObjectKey(String),
    MissingKey(String),
    MissingObject(String),
    CorruptObject(String),
}

impl fmt::Display for ByteStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "byte store I/O failed: {error}"),
            Self::InvalidObjectKey(object_key) => {
                write!(formatter, "invalid object key `{object_key}`")
            }
            Self::MissingKey(name) => write!(formatter, "missing fake keychain secret `{name}`"),
            Self::MissingObject(object_key) => write!(formatter, "missing object `{object_key}`"),
            Self::CorruptObject(object_key) => write!(formatter, "corrupt object `{object_key}`"),
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
        if error.kind() == io::ErrorKind::NotFound {
            Self::MissingObject(error.to_string())
        } else {
            Self::Io(error)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectKeyLeak {
    pub object_key: String,
    pub leaked_segment: String,
}

impl fmt::Display for ObjectKeyLeak {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "object key `{}` leaks path segment `{}`",
            self.object_key, self.leaked_segment
        )
    }
}

impl Error for ObjectKeyLeak {}

pub fn assert_object_key_does_not_leak_path(
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

fn validate_object_key(object_key: &str) -> Result<(), ByteStoreError> {
    let valid = !object_key.is_empty()
        && object_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));

    if valid {
        Ok(())
    } else {
        Err(ByteStoreError::InvalidObjectKey(object_key.to_string()))
    }
}

fn encrypt_bytes(bytes: &[u8], key: &[u8]) -> Vec<u8> {
    let mut encrypted = Vec::with_capacity(ENCRYPTED_OBJECT_HEADER.len() + bytes.len());
    encrypted.extend_from_slice(ENCRYPTED_OBJECT_HEADER);
    encrypted.extend(
        bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ key_stream_byte(key, index)),
    );
    encrypted
}

fn decrypt_bytes(encrypted: &[u8], key: &[u8]) -> Result<Vec<u8>, ByteStoreError> {
    let Some(ciphertext) = encrypted.strip_prefix(ENCRYPTED_OBJECT_HEADER) else {
        return Err(ByteStoreError::CorruptObject(
            "missing fake encryption header".to_string(),
        ));
    };

    Ok(ciphertext
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ key_stream_byte(key, index))
        .collect())
}

fn key_stream_byte(key: &[u8], index: usize) -> u8 {
    key[index % key.len()] ^ ((index as u8).wrapping_mul(31)) ^ 0xa5
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        ByteStore, DeterministicClock, DeterministicIdGenerator, FakeByteStore, FakeKeychain,
        ObjectKind, assert_object_key_does_not_leak_path,
    };
    use crate::workspace::TempWorkspace;

    #[test]
    fn deterministic_clock_and_ids_are_stable() {
        let clock = DeterministicClock::new(41);
        assert_eq!(clock.now().to_string(), "t000000000041");
        assert_eq!(clock.now().to_string(), "t000000000042");

        let ids = DeterministicIdGenerator::new("Phase 0C");
        assert_eq!(ids.next_id("event"), "phase-0c-event-00000001");
        assert_eq!(
            ids.next_id("object pointer"),
            "phase-0c-object-pointer-00000002"
        );
    }

    #[test]
    fn fake_byte_store_encrypts_test_bytes_and_uses_opaque_keys() {
        let workspace = TempWorkspace::new("byte-store").expect("temp workspace");
        let store = FakeByteStore::new(
            workspace.root().join(".bowline-objects"),
            FakeKeychain::default(),
            DeterministicClock::default(),
            DeterministicIdGenerator::new("bowline"),
        )
        .expect("fake byte store");

        let pointer = store
            .put_object(b"plaintext source bytes")
            .expect("stored object");
        let stored_bytes = store
            .stored_bytes(&pointer.object_key)
            .expect("stored encrypted bytes");

        assert_eq!(pointer.kind, ObjectKind::Blob);
        assert_ne!(stored_bytes, b"plaintext source bytes");
        assert_eq!(
            store
                .get_object(&pointer.object_key)
                .expect("decrypted object"),
            b"plaintext source bytes"
        );
        assert_object_key_does_not_leak_path(
            &pointer.object_key,
            "/workspace/Code/acme/web/src/main.rs",
        )
        .expect("object key is opaque");
    }
}
