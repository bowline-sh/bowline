//! Local workspace content-key access for engine-facing surfaces (manifest
//! driver build, work-view RPC transports).

use std::error::Error;

use bowline_core::ids::WorkspaceId;
use bowline_local::device_keys::DeviceKeyError;

use crate::daemon::SyncArgs;
use crate::daemon::key_store;
use crate::daemon::workspace_key_bytes;
use std::fmt;

/// Every epoch this device holds, plus the one it seals at. Prior epochs are
/// carried because objects sealed before a key rotation stay referenced in the
/// manifest; dropping them would make a remaining device's own history
/// unreadable.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::daemon) struct LocalWorkspaceKey {
    pub(in crate::daemon) bytes: [u8; 32],
    pub(in crate::daemon) key_epoch: u32,
    pub(in crate::daemon) prior_epochs: Vec<(u32, [u8; 32])>,
}

impl LocalWorkspaceKey {
    /// Builds engine crypto that seals at the established epoch and can open
    /// every epoch this device still holds.
    pub(in crate::daemon) fn workspace_crypto(
        &self,
        workspace_id: &WorkspaceId,
    ) -> bowline_local::sync::manifest_engine::WorkspaceCrypto {
        let mut crypto = bowline_local::sync::manifest_engine::WorkspaceCrypto::new(
            workspace_id.as_str(),
            self.bytes,
            bowline_local::sync::manifest_engine::KeyEpoch::new(self.key_epoch),
        );
        for (key_epoch, key_bytes) in &self.prior_epochs {
            crypto = crypto.with_key_epoch(
                bowline_local::sync::manifest_engine::KeyEpoch::new(*key_epoch),
                *key_bytes,
            );
        }
        crypto
    }
}

#[derive(Debug)]
pub(in crate::daemon) enum LocalWorkspaceKeyError {
    KeyMissing,
    KeyInvalid,
    DeviceKeys(DeviceKeyError),
}

impl fmt::Display for LocalWorkspaceKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyMissing => write!(formatter, "workspace key is missing"),
            Self::KeyInvalid => write!(formatter, "workspace key is invalid"),
            Self::DeviceKeys(error) => write!(formatter, "device key store failed: {error}"),
        }
    }
}

impl Error for LocalWorkspaceKeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::KeyMissing | Self::KeyInvalid => None,
            Self::DeviceKeys(error) => Some(error),
        }
    }
}

impl From<DeviceKeyError> for LocalWorkspaceKeyError {
    fn from(error: DeviceKeyError) -> Self {
        Self::DeviceKeys(error)
    }
}

pub(in crate::daemon) fn require_local_workspace_key(
    args: &SyncArgs,
) -> Result<LocalWorkspaceKey, LocalWorkspaceKeyError> {
    let workspace_id = args.workspace_id.clone();
    let key_store = key_store()?;
    let keyring = key_store
        .load_workspace_keyring(&workspace_id)?
        .ok_or(LocalWorkspaceKeyError::KeyMissing)?;
    let established = keyring
        .established_material()
        .ok_or(LocalWorkspaceKeyError::KeyMissing)?;
    let mut prior_epochs = Vec::new();
    for material in keyring.materials() {
        if material.key_epoch == established.key_epoch {
            continue;
        }
        prior_epochs.push((
            material.key_epoch,
            workspace_key_bytes(&material.key_bytes)
                .map_err(|_| LocalWorkspaceKeyError::KeyInvalid)?,
        ));
    }
    Ok(LocalWorkspaceKey {
        bytes: workspace_key_bytes(&established.key_bytes)
            .map_err(|_| LocalWorkspaceKeyError::KeyInvalid)?,
        key_epoch: established.key_epoch,
        prior_epochs,
    })
}
