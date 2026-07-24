//! Local workspace content-key access for engine-facing surfaces (manifest
//! driver build, work-view RPC transports).

use std::fmt;

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct LocalWorkspaceKey {
    pub(in crate::daemon) bytes: [u8; 32],
    pub(in crate::daemon) key_epoch: u32,
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
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let key_store = key_store()?;
    let workspace_key = key_store
        .load_workspace_key(&workspace_id)?
        .ok_or(LocalWorkspaceKeyError::KeyMissing)?;
    Ok(LocalWorkspaceKey {
        bytes: workspace_key_bytes(&workspace_key.key_bytes)
            .map_err(|_| LocalWorkspaceKeyError::KeyInvalid)?,
        key_epoch: workspace_key.key_epoch,
    })
}
