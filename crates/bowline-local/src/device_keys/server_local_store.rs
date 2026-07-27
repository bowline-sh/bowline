//! The plaintext-file custody backend: the on-disk secret document, its private
//! file/directory permissions, and the `DeviceKeyStore` implementation over it.
//! Selected when the host exposes no platform keychain, or when the operator
//! asked for a file by path (see [`super::SecretStoreBackend`]).

use std::{fmt, fs, io, path::PathBuf};

use bowline_core::{
    fs_atomic::{AtomicWriteOptions, write_atomic},
    ids::WorkspaceId,
};
use serde::{Deserialize, Serialize};

use super::{
    AccountTokens, DeviceIdentity, DeviceKeyError, DeviceKeyStore, DeviceProofVerifier,
    WorkspaceKeyring, upsert_device_proof_verifier, with_verifier_transaction,
};

const SECRET_FILE_NAME: &str = "secrets.v1";

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

    fn store_workspace_keyring(&self, keyring: WorkspaceKeyring) -> Result<(), DeviceKeyError> {
        let mut document = self.read_document()?;
        document
            .workspace_keyrings
            .retain(|existing| existing.workspace_id != keyring.workspace_id);
        document.workspace_keyrings.push(keyring);
        self.write_document(&document)
    }

    fn load_workspace_keyring(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceKeyring>, DeviceKeyError> {
        Ok(self
            .read_document()?
            .workspace_keyrings
            .into_iter()
            .find(|keyring| &keyring.workspace_id == workspace_id))
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
        write_atomic(
            &self.path,
            &bytes,
            AtomicWriteOptions {
                unix_mode: Some(0o600),
                reject_symlink: true,
                replace_existing: true,
            },
        )?;
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
    workspace_keyrings: Vec<WorkspaceKeyring>,
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
            .field("workspace_keyrings", &self.workspace_keyrings)
            .finish()
    }
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

#[cfg(test)]
mod tests {
    use super::ServerLocalSecretStore;
    use crate::device_keys::{
        AccountSessionCredentials, AccountTokens, DeviceKeyStore, DeviceProofVerifier,
        WorkspaceKeyMaterial,
    };
    use bowline_core::ids::{DeviceId, WorkspaceId};

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
                account_session: Some(AccountSessionCredentials {
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
