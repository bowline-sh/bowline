use std::{collections::BTreeMap, error::Error, fmt, io, path::PathBuf, str::FromStr};

use age::secrecy::ExposeSecret;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bowline_core::{
    devices::{DeviceFingerprint, PublicDeviceKey},
    ids::{AccountId, DeviceId, WorkspaceId},
};
use serde::{Deserialize, Serialize};

const SERVICE: &str = "bowline";
const DEVICE_IDENTITY_SECRET: &str = "device-identity-v1";
const ACCOUNT_TOKENS_SECRET: &str = "account-tokens-v1";
const DEVICE_PROOF_VERIFIERS_SECRET: &str = "device-proof-verifiers-v1";
mod encoding;
mod replacement;
mod server_local_store;
use encoding::{decode_signing_seed, fingerprint_for_public_key};
#[cfg(test)]
pub(crate) use replacement::transaction_entered;
use replacement::with_verifier_transaction;
#[cfg(test)]
use replacement::{set_transaction_hook, verifier_replacement_lock};
pub use server_local_store::ServerLocalSecretStore;

#[cfg(test)]
mod replacement_tests;

pub trait DeviceKeyStore {
    fn load_or_create_device_identity(&self) -> Result<DeviceIdentity, DeviceKeyError>;

    fn store_account_tokens(&self, tokens: AccountTokens) -> Result<(), DeviceKeyError>;

    fn load_account_tokens(&self) -> Result<Option<AccountTokens>, DeviceKeyError>;

    fn clear_account_tokens(&self) -> Result<bool, DeviceKeyError>;

    fn store_workspace_keyring(&self, keyring: WorkspaceKeyring) -> Result<(), DeviceKeyError>;

    fn load_workspace_keyring(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyring>, DeviceKeyError>;

    /// Adds one epoch's material without disturbing the epochs already held.
    /// Prior epochs are custody, not history: objects sealed under them stay in
    /// the manifest long after a rotation, and dropping the key would make
    /// already-synced content unreadable on a device that did nothing wrong.
    fn store_workspace_key(&self, key: WorkspaceKeyMaterial) -> Result<(), DeviceKeyError> {
        let workspace_id = key.workspace_id.clone();
        let mut keyring = self
            .load_workspace_keyring(&workspace_id)?
            .unwrap_or_else(|| WorkspaceKeyring::empty(workspace_id));
        keyring.insert(key);
        self.store_workspace_keyring(keyring)
    }

    /// The material new writes are sealed under: the epoch the control plane
    /// has established, not simply the newest epoch this device holds. A device
    /// that has just seeded a rotation holds material no other device has yet,
    /// and writing under it would make its output unreadable everywhere else.
    fn load_workspace_key(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyMaterial>, DeviceKeyError> {
        Ok(self
            .load_workspace_keyring(workspace_id)?
            .and_then(|keyring| keyring.established_material()))
    }

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
}

/// Which custody backend `default_device_key_store` selected for this process.
///
/// The workspace master key, the device identity and the WorkOS refresh token
/// all live in the selected store, so the choice is user-visible security
/// posture: anything other than `OsKeychain` means those secrets sit in a
/// plaintext file and `bowline status` says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStoreBackend {
    /// macOS Keychain or the Linux Secret Service.
    OsKeychain,
    /// A private file the operator asked for by path or by
    /// `BOWLINE_SECRET_STORE=server-local`.
    RequestedFile,
    /// The default: a private 0600 file in the state root. Named for what it is
    /// rather than for a host type — it is what a laptop gets too, not only a
    /// headless agent host.
    DefaultFile,
}

impl SecretStoreBackend {
    pub fn is_plaintext_file(self) -> bool {
        matches!(self, Self::RequestedFile | Self::DefaultFile)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OsKeychain => "os-keychain",
            Self::RequestedFile => "requested-file",
            Self::DefaultFile => "server-local",
        }
    }
}

impl fmt::Display for SecretStoreBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Resolves the custody backend without constructing a store, so status and
/// diagnostics report exactly what `default_device_key_store` will build.
/// The private 0600 file is the default on every platform; the OS keychain is
/// opt-in.
///
/// This was briefly inverted, and the inversion was wrong on three counts.
///
/// It did not raise the bar. A keychain item's ACL is bound to the creating
/// binary's designated requirement, so it stops other *applications*, not other
/// code running as this user — and this product's whole job is materializing
/// `.env` files and API keys as ordinary plaintext into `~/Code`, which is not a
/// protected location. Anything that can read `secrets.v1` at 0600 can read the
/// secrets it protects, two directories away. Guarding the key while publishing
/// the plaintext is theatre, not a threat model.
///
/// It cost the user dearly. Our binaries are ad-hoc signed, which yields no
/// stable designated requirement, so every rebuild invalidates every "Always
/// Allow" the user has ever clicked — around fifty prompts in two minutes, with
/// no way to make them stop. A stable grant needs Developer ID signing, which is
/// a prerequisite we do not have yet.
///
/// And it is not what this class of tool does: SSH, AWS, kubectl, npm, rclone,
/// Syncthing and Tailscale all default to a private file. The one common
/// counterexample, `gh`, reaches the keychain through `/usr/bin/security`, whose
/// ACL any process running as the user satisfies — file-grade confidentiality
/// with extra steps.
///
/// What genuinely protects this file is its permissions, which
/// [`ServerLocalSecretStore`] enforces by writing atomically at 0600 and
/// refusing a symlinked path. Beating that means hardware-bound encryption
/// (Secure Enclave / TPM), not an ACL prompt.
pub fn selected_secret_store_backend() -> SecretStoreBackend {
    if configured_secret_store_path().is_some() {
        return SecretStoreBackend::RequestedFile;
    }
    backend_for_env(std::env::var("BOWLINE_SECRET_STORE").ok().as_deref())
}

/// The selection rule, without reading the environment, so it is testable
/// without mutating process state a parallel test also reads.
fn backend_for_env(requested: Option<&str>) -> SecretStoreBackend {
    match requested {
        Some("keychain") => SecretStoreBackend::OsKeychain,
        Some("server-local") => SecretStoreBackend::RequestedFile,
        _ => SecretStoreBackend::DefaultFile,
    }
}

pub fn default_device_key_store() -> Result<Box<dyn DeviceKeyStore>, DeviceKeyError> {
    match selected_secret_store_backend() {
        SecretStoreBackend::OsKeychain => Ok(Box::new(KeyringDeviceKeyStore::new("default"))),
        SecretStoreBackend::RequestedFile | SecretStoreBackend::DefaultFile => {
            let path = match configured_secret_store_path() {
                Some(path) => PathBuf::from(path),
                None => ServerLocalSecretStore::default_path()?,
            };
            Ok(Box::new(ServerLocalSecretStore::new(path)))
        }
    }
}

pub fn clear_account_session_from_daemon_env(state_root: &std::path::Path) -> io::Result<bool> {
    crate::daemon_env::update(state_root, crate::daemon_env::without_account_session)
}

/// Records `session` in the daemon environment under `state_root`, replacing
/// whatever pair was there.
///
/// A bootstrapped agent host has no `AccountTokens` record — only `bowline
/// login` writes one — so the key store cannot be the only home for an account
/// session. `daemon.env` is the copy every host has: it is where a bootstrapped
/// host's session arrives, where the next daemon start reads one, and where
/// `bowline logout` looks for a revocation token.
pub fn store_account_session_in_daemon_env(
    state_root: &std::path::Path,
    session: &AccountSessionCredentials,
) -> io::Result<()> {
    crate::daemon_env::update(state_root, |contents| {
        Some(crate::daemon_env::with_account_session(contents, session))
    })?;
    Ok(())
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

/// Every workspace key epoch this device holds, plus which one the control
/// plane says the workspace is currently writing at.
///
/// Two epochs are deliberately distinguished. `established_key_epoch` is the
/// workspace's answer and governs sealing. The rest of the ring exists only to
/// open what was sealed before, which is what keeps a rotation from making a
/// remaining device's own history unreadable.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceKeyring {
    pub workspace_id: WorkspaceId,
    established_key_epoch: u32,
    keys: BTreeMap<u32, Vec<u8>>,
}

impl WorkspaceKeyring {
    pub fn empty(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            established_key_epoch: 0,
            keys: BTreeMap::new(),
        }
    }

    pub fn from_material(key: WorkspaceKeyMaterial) -> Self {
        let mut keyring = Self::empty(key.workspace_id.clone());
        keyring.insert(key);
        keyring
    }

    pub fn insert(&mut self, key: WorkspaceKeyMaterial) {
        self.keys.insert(key.key_epoch, key.key_bytes);
        // A ring that has never been told an epoch still has to be usable: the
        // first material it receives is the workspace's epoch until the control
        // plane says otherwise.
        self.established_key_epoch = self.established_key_epoch.max(key.key_epoch);
    }

    pub fn remove(&mut self, key_epoch: u32) {
        self.keys.remove(&key_epoch);
        if self.established_key_epoch == key_epoch {
            self.established_key_epoch = self.keys.keys().next_back().copied().unwrap_or(0);
        }
    }

    /// Adopts the workspace's answer. Refuses to move to an epoch this device
    /// does not hold, so a stale or hostile control-plane answer can never point
    /// the write path at material that is not there.
    pub fn set_established_key_epoch(&mut self, key_epoch: u32) -> bool {
        if !self.keys.contains_key(&key_epoch) {
            return false;
        }
        self.established_key_epoch = key_epoch;
        true
    }

    pub fn established_key_epoch(&self) -> u32 {
        self.established_key_epoch
    }

    pub fn highest_key_epoch(&self) -> Option<u32> {
        self.keys.keys().next_back().copied()
    }

    pub fn holds(&self, key_epoch: u32) -> bool {
        self.keys.contains_key(&key_epoch)
    }

    pub fn key_bytes(&self, key_epoch: u32) -> Option<&[u8]> {
        self.keys.get(&key_epoch).map(Vec::as_slice)
    }

    pub fn established_material(&self) -> Option<WorkspaceKeyMaterial> {
        self.material(self.established_key_epoch)
    }

    pub fn material(&self, key_epoch: u32) -> Option<WorkspaceKeyMaterial> {
        self.keys
            .get(&key_epoch)
            .map(|key_bytes| WorkspaceKeyMaterial {
                workspace_id: self.workspace_id.clone(),
                key_epoch,
                key_bytes: key_bytes.clone(),
            })
    }

    /// Ascending by epoch: the order is part of the sealed re-grant payload, so
    /// it is fixed here rather than left to map iteration order elsewhere.
    pub fn materials(&self) -> Vec<WorkspaceKeyMaterial> {
        self.keys
            .iter()
            .map(|(key_epoch, key_bytes)| WorkspaceKeyMaterial {
                workspace_id: self.workspace_id.clone(),
                key_epoch: *key_epoch,
                key_bytes: key_bytes.clone(),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl fmt::Debug for WorkspaceKeyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceKeyring")
            .field("workspace_id", &self.workspace_id)
            .field("established_key_epoch", &self.established_key_epoch)
            .field("epochs", &self.keys.keys().collect::<Vec<_>>())
            .field("keys", &"[redacted]")
            .finish()
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

/// Device proof verifiers indexed for signature-verification lookup. A `None`
/// workspace slot holds this device's own locally generated verifier, which
/// must answer before any workspace-scoped verifier has been published; using
/// an `Option` rather than an empty-string key keeps that case out of the
/// workspace namespace entirely.
pub type DeviceProofVerifierCache = BTreeMap<(Option<WorkspaceId>, DeviceId), String>;

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

    fn store_workspace_keyring(&self, keyring: WorkspaceKeyring) -> Result<(), DeviceKeyError> {
        self.set_bytes(
            &workspace_keyring_secret_name(&keyring.workspace_id),
            &serde_json::to_vec(&keyring)?,
        )
    }

    fn load_workspace_keyring(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyring>, DeviceKeyError> {
        self.get_bytes(&workspace_keyring_secret_name(workspace_id))?
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

pub fn workspace_keyring_secret_name(workspace_id: &WorkspaceId) -> String {
    format!("workspace-keyring-v1:{}", workspace_id.as_str())
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

#[cfg(test)]
mod tests {
    use super::{
        DeviceIdentity, SecretStoreBackend, WorkspaceKeyMaterial, backend_for_env,
        keyring_secret_result_to_bytes,
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

    /// The default must stay the private file. Inverting it to the OS keychain
    /// produced ~50 unstoppable password prompts in two minutes on an ad-hoc
    /// signed build, and bought no confidentiality this product does not already
    /// concede by writing `.env` files into `~/Code` in plaintext.
    #[test]
    fn the_default_backend_is_the_private_file_not_the_keychain() {
        // Asserted through the pure mapping rather than the process environment,
        // which a parallel test could be mutating.
        assert_eq!(backend_for_env(None), SecretStoreBackend::DefaultFile);
        assert_eq!(
            backend_for_env(Some("keychain")),
            SecretStoreBackend::OsKeychain,
            "the keychain must stay reachable, just opt-in"
        );
        assert_eq!(
            backend_for_env(Some("server-local")),
            SecretStoreBackend::RequestedFile
        );
        assert!(backend_for_env(None).is_plaintext_file());
    }
}
