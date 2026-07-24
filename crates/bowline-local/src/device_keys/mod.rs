use std::{error::Error, fmt, fs, io, path::PathBuf, str::FromStr};

use age::secrecy::ExposeSecret;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bowline_core::{
    devices::{DeviceFingerprint, PublicDeviceKey},
    fs_atomic::{AtomicWriteOptions, write_atomic},
    ids::{AccountId, DeviceId, WorkspaceId},
};
use serde::{Deserialize, Serialize};

const SERVICE: &str = "bowline";
const DEVICE_IDENTITY_SECRET: &str = "device-identity-v1";
const ACCOUNT_TOKENS_SECRET: &str = "account-tokens-v1";
const DEVICE_PROOF_VERIFIERS_SECRET: &str = "device-proof-verifiers-v1";
const SECRET_FILE_NAME: &str = "secrets.v1";
mod daemon_env;
mod encoding;
mod replacement;
use encoding::{decode_signing_seed, fingerprint_for_public_key};
#[cfg(test)]
pub(crate) use replacement::transaction_entered;
use replacement::{secret_temp_path, with_verifier_transaction};
#[cfg(test)]
use replacement::{set_transaction_hook, verifier_replacement_lock};

#[cfg(test)]
mod replacement_tests;

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

    fn store_device_proof_verifier(
        &self,
        verifier: DeviceProofVerifier,
    ) -> Result<(), DeviceKeyError>;

    fn load_device_proof_verifiers(&self) -> Result<Vec<DeviceProofVerifier>, DeviceKeyError>;

    fn replace_device_proof_verifiers_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        verifiers: Vec<DeviceProofVerifier>,
    ) -> Result<(), DeviceKeyError>;

    fn mark_secret_unavailable(
        &self,
        reason: SecretUnavailableReason,
    ) -> Result<(), DeviceKeyError>;
}

pub fn default_device_key_store() -> Result<Box<dyn DeviceKeyStore>, DeviceKeyError> {
    if let Some(path) = configured_secret_store_path() {
        return Ok(Box::new(ServerLocalSecretStore::new(path)));
    }
    if keychain_secret_store_allowed() {
        return Ok(Box::new(KeyringDeviceKeyStore::new("default")));
    }
    Ok(Box::new(ServerLocalSecretStore::new(
        ServerLocalSecretStore::default_path()?,
    )))
}

pub fn clear_account_session_from_daemon_env(state_root: &std::path::Path) -> io::Result<bool> {
    let path = state_root.join("daemon.env");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let Some(updated) = daemon_env::without_account_session(&contents) else {
        return Ok(false);
    };
    write_atomic(
        &path,
        updated.as_bytes(),
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: true,
        },
    )?;
    Ok(true)
}

pub fn workspace_key_bytes(bytes: &[u8]) -> Result<[u8; 32], DeviceKeyError> {
    bytes
        .try_into()
        .map_err(|_| DeviceKeyError::CorruptSecret("workspace key must be 32 bytes".to_string()))
}

fn configured_secret_store_path() -> Option<String> {
    std::env::var("BOWLINE_SECRET_STORE_PATH")
        .ok()
        .filter(|path| !path.is_empty())
}

fn keychain_secret_store_allowed() -> bool {
    std::env::var("BOWLINE_SECRET_STORE").as_deref() == Ok("keychain")
        && matches!(
            std::env::var("BOWLINE_ALLOW_KEYCHAIN_PROBE").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    age_secret: String,
    signing_seed: [u8; 32],
    pub public_key: PublicDeviceKey,
    pub fingerprint: DeviceFingerprint,
}

impl DeviceIdentity {
    pub fn generate() -> Self {
        Self::try_generate().expect("device identity CSPRNG should be available")
    }

    pub fn try_generate() -> Result<Self, DeviceKeyError> {
        let identity = age::x25519::Identity::generate();
        let mut signing_seed = [0_u8; 32];
        getrandom::fill(&mut signing_seed)
            .map_err(|error| DeviceKeyError::Unavailable(error.to_string()))?;
        Ok(Self::from_age_identity(identity, signing_seed))
    }

    pub fn parse(secret: impl Into<String>) -> Result<Self, DeviceKeyError> {
        let secret = secret.into();
        let document = serde_json::from_str::<DeviceIdentitySecretDocument>(&secret)?;
        let signing_seed = decode_signing_seed(&document.signing_seed)?;
        Self::from_age_secret(document.age_secret, signing_seed)
    }

    pub fn persisted_secret(&self) -> Result<String, DeviceKeyError> {
        serde_json::to_string(&DeviceIdentitySecretDocument {
            age_secret: self.age_secret.clone(),
            signing_seed: BASE64.encode(self.signing_seed),
        })
        .map_err(Into::into)
    }

    fn from_age_secret(age_secret: String, signing_seed: [u8; 32]) -> Result<Self, DeviceKeyError> {
        let identity = age::x25519::Identity::from_str(&age_secret)
            .map_err(|error| DeviceKeyError::CorruptSecret(error.to_string()))?;
        let public_key = identity.to_public().to_string();
        Ok(Self {
            fingerprint: fingerprint_for_public_key(&public_key),
            public_key: PublicDeviceKey::new(public_key),
            age_secret,
            signing_seed,
        })
    }

    pub fn secret(&self) -> &str {
        &self.age_secret
    }

    pub(crate) fn signing_seed(&self) -> &[u8; 32] {
        &self.signing_seed
    }

    pub fn age_identity(&self) -> Result<age::x25519::Identity, DeviceKeyError> {
        age::x25519::Identity::from_str(&self.age_secret)
            .map_err(|error| DeviceKeyError::CorruptSecret(error.to_string()))
    }

    fn from_age_identity(identity: age::x25519::Identity, signing_seed: [u8; 32]) -> Self {
        let age_secret = identity.to_string().expose_secret().to_string();
        let public_key = identity.to_public().to_string();
        Self {
            fingerprint: fingerprint_for_public_key(&public_key),
            public_key: PublicDeviceKey::new(public_key),
            age_secret,
            signing_seed,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceIdentitySecretDocument {
    age_secret: String,
    signing_seed: String,
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("public_key", &self.public_key)
            .field("fingerprint", &self.fingerprint)
            .field("age_secret", &"[redacted]")
            .field("signing_seed", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSessionCredentials {
    pub session_id: String,
    pub revocation_token: String,
}

impl fmt::Debug for AccountSessionCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountSessionCredentials")
            .field("session_id", &"[redacted]")
            .field("revocation_token", &"[redacted]")
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_session: Option<AccountSessionCredentials>,
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
                "account_session",
                &self.account_session.as_ref().map(|_| "[redacted]"),
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceProofVerifier {
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub proof_verifier: String,
}

impl fmt::Debug for DeviceProofVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceProofVerifier")
            .field("workspace_id", &self.workspace_id)
            .field("device_id", &self.device_id)
            .field("proof_verifier", &"[redacted]")
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

        let identity = DeviceIdentity::try_generate()?;
        self.set_bytes(
            DEVICE_IDENTITY_SECRET,
            identity.persisted_secret()?.as_bytes(),
        )?;
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

    fn store_device_proof_verifier(
        &self,
        verifier: DeviceProofVerifier,
    ) -> Result<(), DeviceKeyError> {
        with_verifier_transaction(
            format!(
                "keyring:{}",
                self.secret_name(DEVICE_PROOF_VERIFIERS_SECRET)
            ),
            || {
                let mut verifiers = self.load_device_proof_verifiers()?;
                upsert_device_proof_verifier(&mut verifiers, verifier);
                self.set_bytes(
                    DEVICE_PROOF_VERIFIERS_SECRET,
                    &serde_json::to_vec(&verifiers)?,
                )
            },
        )
    }

    fn load_device_proof_verifiers(&self) -> Result<Vec<DeviceProofVerifier>, DeviceKeyError> {
        self.get_bytes(DEVICE_PROOF_VERIFIERS_SECRET)?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn replace_device_proof_verifiers_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        verifiers: Vec<DeviceProofVerifier>,
    ) -> Result<(), DeviceKeyError> {
        with_verifier_transaction(
            format!(
                "keyring:{}",
                self.secret_name(DEVICE_PROOF_VERIFIERS_SECRET)
            ),
            || {
                let mut persisted = self.load_device_proof_verifiers()?;
                persisted.retain(|verifier| &verifier.workspace_id != workspace_id);
                persisted.extend(verifiers);
                persisted.sort_by(|left, right| {
                    left.workspace_id
                        .cmp(&right.workspace_id)
                        .then_with(|| left.device_id.cmp(&right.device_id))
                });
                self.set_bytes(
                    DEVICE_PROOF_VERIFIERS_SECRET,
                    &serde_json::to_vec(&persisted)?,
                )
            },
        )
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
        let identity = DeviceIdentity::try_generate()?;
        document.device_identity = Some(identity.persisted_secret()?);
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

    fn store_device_proof_verifier(
        &self,
        verifier: DeviceProofVerifier,
    ) -> Result<(), DeviceKeyError> {
        with_verifier_transaction(format!("file:{}", self.path.display()), || {
            let mut document = self.read_document()?;
            upsert_device_proof_verifier(&mut document.device_proof_verifiers, verifier);
            self.write_document(&document)
        })
    }

    fn load_device_proof_verifiers(&self) -> Result<Vec<DeviceProofVerifier>, DeviceKeyError> {
        Ok(self.read_document()?.device_proof_verifiers)
    }

    fn replace_device_proof_verifiers_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
        verifiers: Vec<DeviceProofVerifier>,
    ) -> Result<(), DeviceKeyError> {
        with_verifier_transaction(format!("file:{}", self.path.display()), || {
            let mut document = self.read_document()?;
            document
                .device_proof_verifiers
                .retain(|verifier| &verifier.workspace_id != workspace_id);
            document.device_proof_verifiers.extend(verifiers);
            document.device_proof_verifiers.sort_by(|left, right| {
                left.workspace_id
                    .cmp(&right.workspace_id)
                    .then_with(|| left.device_id.cmp(&right.device_id))
            });
            self.write_document(&document)
        })
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
        let temp_path = secret_temp_path(&self.path);
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
    #[serde(default)]
    device_proof_verifiers: Vec<DeviceProofVerifier>,
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
            .field("device_proof_verifiers", &"[redacted]")
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

fn upsert_device_proof_verifier(
    verifiers: &mut Vec<DeviceProofVerifier>,
    verifier: DeviceProofVerifier,
) {
    verifiers.retain(|existing| {
        existing.workspace_id != verifier.workspace_id || existing.device_id != verifier.device_id
    });
    verifiers.push(verifier);
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
        AccountTokens, DeviceIdentity, DeviceKeyStore, DeviceProofVerifier, ServerLocalSecretStore,
        WorkspaceKeyMaterial, keyring_secret_result_to_bytes,
    };
    use bowline_core::ids::{DeviceId, WorkspaceId};

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
            account_session: Some(super::AccountSessionCredentials {
                session_id: "bowline_session_secret".to_string(),
                revocation_token: "bowline_revoke_secret".to_string(),
            }),
        };

        assert!(!format!("{tokens:?}").contains("access-secret"));
        assert!(!format!("{tokens:?}").contains("refresh-secret"));
        assert!(!format!("{tokens:?}").contains("bowline_session_secret"));
        assert!(!format!("{tokens:?}").contains("bowline_revoke_secret"));
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
                account_session: None,
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
                account_session: Some(super::AccountSessionCredentials {
                    session_id: "session-secret".to_string(),
                    revocation_token: "revoke-secret".to_string(),
                }),
            })
            .expect("account tokens");
        store
            .store_workspace_key(WorkspaceKeyMaterial {
                workspace_id: workspace_id.clone(),
                key_epoch: 7,
                key_bytes: vec![9; 32],
            })
            .expect("workspace key");
        store
            .store_device_proof_verifier(DeviceProofVerifier {
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("device-1"),
                proof_verifier: "dapv_device_1".to_string(),
            })
            .expect("device proof verifier");

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
        assert_eq!(
            store
                .load_device_proof_verifiers()
                .expect("device proof verifier load")
                .first()
                .expect("device proof verifier remains")
                .proof_verifier,
            "dapv_device_1"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
