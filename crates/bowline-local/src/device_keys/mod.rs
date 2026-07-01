use std::{error::Error, fmt, fs, io, path::PathBuf, str::FromStr};

use age::secrecy::ExposeSecret;
use bowline_core::{
    devices::{DeviceFingerprint, PublicDeviceKey},
    ids::{AccountId, WorkspaceId},
};
use serde::{Deserialize, Serialize};

const SERVICE: &str = "bowline";
const DEVICE_IDENTITY_SECRET: &str = "device-identity-v1";
const ACCOUNT_TOKENS_SECRET: &str = "account-tokens-v1";
const SECRET_FILE_NAME: &str = "secrets.v1";

pub trait DeviceKeyStore {
    fn load_or_create_device_identity(&self) -> Result<DeviceIdentity, DeviceKeyError>;

    fn store_account_tokens(&self, tokens: AccountTokens) -> Result<(), DeviceKeyError>;

    fn load_account_tokens(&self) -> Result<Option<AccountTokens>, DeviceKeyError>;

    fn clear_account_tokens(&self) -> Result<bool, DeviceKeyError>;

    fn store_workspace_key(&self, key: WorkspaceKeyMaterial) -> Result<(), DeviceKeyError>;

    fn load_workspace_key(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyMaterial>, DeviceKeyError>;

    fn mark_secret_unavailable(
        &self,
        reason: SecretUnavailableReason,
    ) -> Result<(), DeviceKeyError>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    secret: String,
    pub public_key: PublicDeviceKey,
    pub fingerprint: DeviceFingerprint,
}

impl DeviceIdentity {
    pub fn generate() -> Self {
        let identity = age::x25519::Identity::generate();
        Self::from_age_identity(identity)
    }

    pub fn parse(secret: impl Into<String>) -> Result<Self, DeviceKeyError> {
        let secret = secret.into();
        let identity = age::x25519::Identity::from_str(&secret)
            .map_err(|error| DeviceKeyError::CorruptSecret(error.to_string()))?;
        let public_key = identity.to_public().to_string();
        Ok(Self {
            fingerprint: fingerprint_for_public_key(&public_key),
            public_key: PublicDeviceKey::new(public_key),
            secret,
        })
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn age_identity(&self) -> Result<age::x25519::Identity, DeviceKeyError> {
        age::x25519::Identity::from_str(&self.secret)
            .map_err(|error| DeviceKeyError::CorruptSecret(error.to_string()))
    }

    fn from_age_identity(identity: age::x25519::Identity) -> Self {
        let secret = identity.to_string().expose_secret().to_string();
        let public_key = identity.to_public().to_string();
        Self {
            fingerprint: fingerprint_for_public_key(&public_key),
            public_key: PublicDeviceKey::new(public_key),
            secret,
        }
    }
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("public_key", &self.public_key)
            .field("fingerprint", &self.fingerprint)
            .field("secret", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountTokens {
    pub account_id: AccountId,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_session_id: Option<String>,
}

impl fmt::Debug for AccountTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountTokens")
            .field("account_id", &self.account_id)
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field(
                "account_session_id",
                &self.account_session_id.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceKeyMaterial {
    pub workspace_id: WorkspaceId,
    pub key_epoch: u32,
    pub key_bytes: Vec<u8>,
}

impl WorkspaceKeyMaterial {
    pub fn generate(workspace_id: WorkspaceId, key_epoch: u32) -> Result<Self, DeviceKeyError> {
        let mut key_bytes = vec![0_u8; 32];
        getrandom::fill(&mut key_bytes)
            .map_err(|error| DeviceKeyError::Unavailable(error.to_string()))?;
        Ok(Self {
            workspace_id,
            key_epoch,
            key_bytes,
        })
    }
}

impl fmt::Debug for WorkspaceKeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceKeyMaterial")
            .field("workspace_id", &self.workspace_id)
            .field("key_epoch", &self.key_epoch)
            .field("key_bytes", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretUnavailableReason {
    KeychainUnavailable,
    CorruptSecret,
    ServerLocalStoreUnavailable,
}

#[derive(Debug)]
pub enum DeviceKeyError {
    Io(io::Error),
    Json(serde_json::Error),
    Keyring(String),
    MissingSecret(String),
    CorruptSecret(String),
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct KeyringDeviceKeyStore {
    namespace: String,
}

impl KeyringDeviceKeyStore {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
        }
    }

    fn secret_name(&self, name: &str) -> String {
        format!("{}:{name}", self.namespace)
    }

    fn get_bytes(&self, name: &str) -> Result<Option<Vec<u8>>, DeviceKeyError> {
        let entry = keyring::Entry::new(SERVICE, &self.secret_name(name))
            .map_err(|error| DeviceKeyError::Keyring(error.to_string()))?;
        keyring_secret_result_to_bytes(entry.get_secret())
    }

    fn set_bytes(&self, name: &str, bytes: &[u8]) -> Result<(), DeviceKeyError> {
        let entry = keyring::Entry::new(SERVICE, &self.secret_name(name))
            .map_err(|error| DeviceKeyError::Keyring(error.to_string()))?;
        entry
            .set_secret(bytes)
            .map_err(|error| DeviceKeyError::Keyring(error.to_string()))
    }

    fn delete_bytes(&self, name: &str) -> Result<bool, DeviceKeyError> {
        let entry = keyring::Entry::new(SERVICE, &self.secret_name(name))
            .map_err(|error| DeviceKeyError::Keyring(error.to_string()))?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(DeviceKeyError::Keyring(error.to_string())),
        }
    }
}

fn keyring_secret_result_to_bytes(
    result: Result<Vec<u8>, keyring::Error>,
) -> Result<Option<Vec<u8>>, DeviceKeyError> {
    match result {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(DeviceKeyError::Keyring(error.to_string())),
    }
}

impl DeviceKeyStore for KeyringDeviceKeyStore {
    fn load_or_create_device_identity(&self) -> Result<DeviceIdentity, DeviceKeyError> {
        if let Some(bytes) = self.get_bytes(DEVICE_IDENTITY_SECRET)? {
            let secret = String::from_utf8(bytes)
                .map_err(|error| DeviceKeyError::CorruptSecret(error.to_string()))?;
            return DeviceIdentity::parse(secret);
        }

        let identity = DeviceIdentity::generate();
        self.set_bytes(DEVICE_IDENTITY_SECRET, identity.secret().as_bytes())?;
        Ok(identity)
    }

    fn store_account_tokens(&self, tokens: AccountTokens) -> Result<(), DeviceKeyError> {
        self.set_bytes(ACCOUNT_TOKENS_SECRET, &serde_json::to_vec(&tokens)?)
    }

    fn load_account_tokens(&self) -> Result<Option<AccountTokens>, DeviceKeyError> {
        self.get_bytes(ACCOUNT_TOKENS_SECRET)?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    fn clear_account_tokens(&self) -> Result<bool, DeviceKeyError> {
        self.delete_bytes(ACCOUNT_TOKENS_SECRET)
    }

    fn store_workspace_key(&self, key: WorkspaceKeyMaterial) -> Result<(), DeviceKeyError> {
        self.set_bytes(
            &workspace_key_secret_name(&key.workspace_id),
            &serde_json::to_vec(&key)?,
        )
    }

    fn load_workspace_key(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyMaterial>, DeviceKeyError> {
        self.get_bytes(&workspace_key_secret_name(workspace_id))?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    fn mark_secret_unavailable(
        &self,
        _reason: SecretUnavailableReason,
    ) -> Result<(), DeviceKeyError> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ServerLocalSecretStore {
    path: PathBuf,
}

impl ServerLocalSecretStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> Result<PathBuf, DeviceKeyError> {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .ok_or_else(|| DeviceKeyError::Unavailable("HOME is not set".to_string()))?;
        Ok(state_home.join("bowline").join(SECRET_FILE_NAME))
    }
}

impl DeviceKeyStore for ServerLocalSecretStore {
    fn load_or_create_device_identity(&self) -> Result<DeviceIdentity, DeviceKeyError> {
        let mut document = self.read_document()?;
        if let Some(secret) = document.device_identity.as_ref() {
            return DeviceIdentity::parse(secret.clone());
        }
        let identity = DeviceIdentity::generate();
        document.device_identity = Some(identity.secret().to_string());
        self.write_document(&document)?;
        Ok(identity)
    }

    fn store_account_tokens(&self, tokens: AccountTokens) -> Result<(), DeviceKeyError> {
        let mut document = self.read_document()?;
        document.account_tokens = Some(tokens);
        self.write_document(&document)
    }

    fn load_account_tokens(&self) -> Result<Option<AccountTokens>, DeviceKeyError> {
        Ok(self.read_document()?.account_tokens)
    }

    fn clear_account_tokens(&self) -> Result<bool, DeviceKeyError> {
        let mut document = self.read_document()?;
        let had_tokens = document.account_tokens.take().is_some();
        if had_tokens {
            self.write_document(&document)?;
        }
        Ok(had_tokens)
    }

    fn store_workspace_key(&self, key: WorkspaceKeyMaterial) -> Result<(), DeviceKeyError> {
        let mut document = self.read_document()?;
        document
            .workspace_keys
            .retain(|existing| existing.workspace_id != key.workspace_id);
        document.workspace_keys.push(key);
        self.write_document(&document)
    }

    fn load_workspace_key(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyMaterial>, DeviceKeyError> {
        Ok(self
            .read_document()?
            .workspace_keys
            .into_iter()
            .find(|key| &key.workspace_id == workspace_id))
    }

    fn mark_secret_unavailable(
        &self,
        reason: SecretUnavailableReason,
    ) -> Result<(), DeviceKeyError> {
        let mut document = self.read_document()?;
        document.unavailable_reason = Some(reason);
        self.write_document(&document)
    }
}

impl ServerLocalSecretStore {
    fn read_document(&self) -> Result<SecretDocument, DeviceKeyError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(Into::into),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SecretDocument::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn write_document(&self, document: &SecretDocument) -> Result<(), DeviceKeyError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
            set_private_directory_permissions(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(document)?;
        let temp_path = self.path.with_extension("tmp");
        fs::write(&temp_path, bytes)?;
        set_private_file_permissions(&temp_path)?;
        fs::rename(&temp_path, &self.path)?;
        set_private_file_permissions(&self.path)?;
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SecretDocument {
    device_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_tokens: Option<AccountTokens>,
    workspace_keys: Vec<WorkspaceKeyMaterial>,
    unavailable_reason: Option<SecretUnavailableReason>,
}

impl fmt::Debug for SecretDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretDocument")
            .field(
                "device_identity",
                &self.device_identity.as_ref().map(|_| "[redacted]"),
            )
            .field("account_tokens", &self.account_tokens)
            .field("workspace_keys", &self.workspace_keys)
            .field("unavailable_reason", &self.unavailable_reason)
            .finish()
    }
}

impl fmt::Display for DeviceKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "device key store I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "device key store JSON failed: {error}"),
            Self::Keyring(error) => write!(formatter, "OS keychain failed: {error}"),
            Self::MissingSecret(name) => write!(formatter, "secret `{name}` is missing"),
            Self::CorruptSecret(error) => write!(formatter, "secret material is corrupt: {error}"),
            Self::Unavailable(reason) => write!(formatter, "secret store unavailable: {reason}"),
        }
    }
}

impl Error for DeviceKeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for DeviceKeyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for DeviceKeyError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn workspace_key_secret_name(workspace_id: &WorkspaceId) -> String {
    format!("workspace-key-v1:{}", workspace_id.as_str())
}

fn fingerprint_for_public_key(public_key: &str) -> DeviceFingerprint {
    let hash = blake3::hash(public_key.as_bytes());
    DeviceFingerprint::new(format!("fp_{}", &hash.to_hex()[..16]))
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AccountTokens, DeviceIdentity, DeviceKeyStore, ServerLocalSecretStore,
        WorkspaceKeyMaterial, keyring_secret_result_to_bytes,
    };
    use bowline_core::ids::WorkspaceId;

    #[test]
    fn generated_device_identity_has_public_fingerprint() {
        let identity = DeviceIdentity::generate();

        assert!(identity.public_key.as_str().starts_with("age1"));
        assert!(identity.fingerprint.as_str().starts_with("fp_"));
        assert!(!format!("{identity:?}").contains(identity.secret()));
    }

    #[test]
    fn workspace_key_debug_redacts_bytes() {
        let key = WorkspaceKeyMaterial {
            workspace_id: WorkspaceId::new("workspace-1"),
            key_epoch: 1,
            key_bytes: vec![7; 32],
        };

        assert!(!format!("{key:?}").contains('7'));
        assert!(format!("{key:?}").contains("[redacted]"));
    }

    #[test]
    fn account_token_debug_redacts_tokens() {
        let tokens = super::AccountTokens {
            account_id: bowline_core::ids::AccountId::new("acct_123"),
            access_token: "access-secret".to_string(),
            refresh_token: "refresh-secret".to_string(),
            expires_at: "later".to_string(),
            account_session_id: Some("bowline_session_secret".to_string()),
        };

        assert!(!format!("{tokens:?}").contains("access-secret"));
        assert!(!format!("{tokens:?}").contains("refresh-secret"));
        assert!(!format!("{tokens:?}").contains("bowline_session_secret"));
        assert!(format!("{tokens:?}").contains("[redacted]"));
    }

    #[test]
    fn keyring_no_entry_is_a_missing_secret_not_a_fatal_error() {
        assert_eq!(
            keyring_secret_result_to_bytes(Err(keyring::Error::NoEntry))
                .expect("missing keyring entry is readable"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn server_local_store_uses_private_file_permissions() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let root = std::env::temp_dir().join(format!(
            "bowline-server-local-secret-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("state").join("bowline").join("secrets.v1");
        let store = ServerLocalSecretStore::new(&path);

        store
            .store_account_tokens(AccountTokens {
                account_id: bowline_core::ids::AccountId::new("acct_server_local"),
                access_token: "access-secret".to_string(),
                refresh_token: "refresh-secret".to_string(),
                expires_at: "2026-06-24T12:00:00Z".to_string(),
                account_session_id: None,
            })
            .expect("server-local write");

        let parent_mode = fs::metadata(path.parent().expect("secret parent"))
            .expect("secret parent metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(&path)
            .expect("secret file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn server_local_store_clears_only_account_tokens() {
        let root = std::env::temp_dir().join(format!(
            "bowline-server-local-clear-account-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("state").join("bowline").join("secrets.v1");
        let store = ServerLocalSecretStore::new(&path);
        let workspace_id = WorkspaceId::new("workspace-clear-account");

        let identity = store
            .load_or_create_device_identity()
            .expect("device identity");
        store
            .store_account_tokens(AccountTokens {
                account_id: bowline_core::ids::AccountId::new("acct_server_local"),
                access_token: "access-secret".to_string(),
                refresh_token: "refresh-secret".to_string(),
                expires_at: "2026-06-24T12:00:00Z".to_string(),
                account_session_id: Some("session-secret".to_string()),
            })
            .expect("account tokens");
        store
            .store_workspace_key(WorkspaceKeyMaterial {
                workspace_id: workspace_id.clone(),
                key_epoch: 7,
                key_bytes: vec![9; 32],
            })
            .expect("workspace key");

        assert!(store.clear_account_tokens().expect("clear tokens"));
        assert!(!store.clear_account_tokens().expect("idempotent clear"));
        assert!(store.load_account_tokens().expect("load tokens").is_none());
        assert_eq!(
            store
                .load_or_create_device_identity()
                .expect("device identity remains")
                .fingerprint,
            identity.fingerprint
        );
        assert_eq!(
            store
                .load_workspace_key(&workspace_id)
                .expect("workspace key load")
                .expect("workspace key remains")
                .key_epoch,
            7
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
