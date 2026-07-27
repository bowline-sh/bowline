use std::{fmt, path::Path};

use crate::{canonical_framing::CanonicalFrame, envelope::workspace_id_hash};
use bowline_core::ids::WorkspaceId;

use super::LocalRecoveryPreimageError;

const LOCAL_RECOVERY_PREIMAGE_DOMAIN: &str = "bowline-local-recovery-preimage";
const LOCAL_RECOVERY_PREIMAGE_FORMAT_VERSION: u16 = 1;
const LOCAL_RECOVERY_PREIMAGE_ROOT: &str = "filesystem-epochs/encrypted-quarantine";
const LOCAL_RECOVERY_PLAINTEXT_ROOT: &str = "filesystem-epochs/plaintext-quarantine";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRecoveryPreimageLocator(String);

impl LocalRecoveryPreimageLocator {
    pub fn new(value: impl Into<String>) -> Result<Self, LocalRecoveryPreimageError> {
        let value = value.into();
        validate_sealed_locator(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn for_epoch_path(
        filesystem_epoch_identity: &LocalRecoveryEpochIdentity,
        workspace_path_identity: &LocalRecoveryWorkspacePath,
    ) -> Self {
        let epoch_hash = domain_hash("epoch", filesystem_epoch_identity.as_str());
        let path_hash = domain_hash("path", workspace_path_identity.as_str());
        Self::new(format!(
            "{LOCAL_RECOVERY_PREIMAGE_ROOT}/{epoch_hash}/{path_hash}.bowline-envelope"
        ))
        .expect("generated encrypted recovery locator is canonical")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRecoveryPlaintextLocator(String);

impl LocalRecoveryPlaintextLocator {
    pub fn for_epoch_path(
        filesystem_epoch_identity: &LocalRecoveryEpochIdentity,
        workspace_path_identity: &LocalRecoveryWorkspacePath,
    ) -> Self {
        let epoch_hash = domain_hash("epoch", filesystem_epoch_identity.as_str());
        let path_hash = domain_hash("path", workspace_path_identity.as_str());
        Self(format!(
            "{LOCAL_RECOVERY_PLAINTEXT_ROOT}/{epoch_hash}/{path_hash}.bowline-plaintext"
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

macro_rules! local_recovery_identity {
    ($name:ident, $field:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, LocalRecoveryPreimageError> {
                let value = value.into();
                validate_identity(&value, $field)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

local_recovery_identity!(LocalRecoveryEpochIdentity, "filesystem_epoch_identity");
local_recovery_identity!(
    LocalRecoveryExpectedPreimageIdentity,
    "expected_preimage_identity"
);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalRecoveryWorkspacePath(String);

impl LocalRecoveryWorkspacePath {
    pub fn new(value: impl Into<String>) -> Result<Self, LocalRecoveryPreimageError> {
        let value = value.into();
        validate_state_relative_path(&value, "workspace_path_identity")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalRecoveryKeyEpoch(u32);

impl LocalRecoveryKeyEpoch {
    pub fn new(value: u32) -> Result<Self, LocalRecoveryPreimageError> {
        if value == 0 {
            Err(LocalRecoveryPreimageError::InvalidContext {
                field: "key_epoch",
                reason: "must be non-zero",
            })
        } else {
            Ok(Self(value))
        }
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LocalRecoveryPreimageContext {
    workspace_id_hash: String,
    filesystem_epoch_identity: String,
    workspace_path_identity: String,
    expected_preimage_identity: String,
    key_epoch: LocalRecoveryKeyEpoch,
    plaintext_locator: LocalRecoveryPlaintextLocator,
    sealed_locator: LocalRecoveryPreimageLocator,
}

impl fmt::Debug for LocalRecoveryPreimageContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalRecoveryPreimageContext(<redacted>)")
    }
}

impl LocalRecoveryPreimageContext {
    pub fn new(
        workspace_id: &WorkspaceId,
        filesystem_epoch_identity: &LocalRecoveryEpochIdentity,
        workspace_path_identity: &LocalRecoveryWorkspacePath,
        expected_preimage_identity: &LocalRecoveryExpectedPreimageIdentity,
        key_epoch: LocalRecoveryKeyEpoch,
    ) -> Result<Self, LocalRecoveryPreimageError> {
        if workspace_id.as_str().is_empty() {
            return Err(LocalRecoveryPreimageError::InvalidContext {
                field: "workspace_id",
                reason: "must not be empty",
            });
        }
        Ok(Self {
            workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
            filesystem_epoch_identity: filesystem_epoch_identity.as_str().to_string(),
            workspace_path_identity: workspace_path_identity.as_str().to_string(),
            expected_preimage_identity: expected_preimage_identity.as_str().to_string(),
            key_epoch,
            plaintext_locator: LocalRecoveryPlaintextLocator::for_epoch_path(
                filesystem_epoch_identity,
                workspace_path_identity,
            ),
            sealed_locator: LocalRecoveryPreimageLocator::for_epoch_path(
                filesystem_epoch_identity,
                workspace_path_identity,
            ),
        })
    }

    pub const fn key_epoch(&self) -> LocalRecoveryKeyEpoch {
        self.key_epoch
    }

    pub fn plaintext_locator(&self) -> &LocalRecoveryPlaintextLocator {
        &self.plaintext_locator
    }

    pub fn sealed_locator(&self) -> &LocalRecoveryPreimageLocator {
        &self.sealed_locator
    }

    /// Canonical AEAD associated data guarding one sealed recovery preimage.
    ///
    /// These bytes protect `.env` contents whose plaintext is deliberately
    /// unlinked once the envelope is published, so the field order and labels
    /// below are permanent: change either and the user's quarantined secrets
    /// become unopenable. `recovery_preimage_associated_data_bytes_are_pinned`
    /// fails loudly if this encoding moves.
    pub(super) fn associated_data(
        &self,
        locator: &LocalRecoveryPreimageLocator,
    ) -> Result<Vec<u8>, LocalRecoveryPreimageError> {
        if locator != &self.sealed_locator {
            return Err(LocalRecoveryPreimageError::ContextLocatorMismatch);
        }
        Ok(CanonicalFrame::new(LOCAL_RECOVERY_PREIMAGE_DOMAIN)
            .str_field("workspace-id-hash", &self.workspace_id_hash)
            .str_field("filesystem-epoch-identity", &self.filesystem_epoch_identity)
            .str_field("workspace-path-identity", &self.workspace_path_identity)
            .str_field(
                "expected-preimage-identity",
                &self.expected_preimage_identity,
            )
            .str_field("locator", locator.as_str())
            .u32_field("key-epoch", self.key_epoch.value())
            .u16_field("format-version", LOCAL_RECOVERY_PREIMAGE_FORMAT_VERSION)
            .into_bytes())
    }
}

fn validate_identity(value: &str, field: &'static str) -> Result<(), LocalRecoveryPreimageError> {
    if value.is_empty() {
        return Err(LocalRecoveryPreimageError::InvalidContext {
            field,
            reason: "must not be empty",
        });
    }
    if value.contains('\0') {
        return Err(LocalRecoveryPreimageError::InvalidContext {
            field,
            reason: "must not contain NUL",
        });
    }
    Ok(())
}

fn validate_state_relative_path(
    value: &str,
    field: &'static str,
) -> Result<(), LocalRecoveryPreimageError> {
    if value.is_empty() {
        return Err(LocalRecoveryPreimageError::InvalidLocator {
            field,
            reason: "must not be empty",
        });
    }
    if value.starts_with('/') {
        return Err(LocalRecoveryPreimageError::InvalidLocator {
            field,
            reason: "must be state-relative",
        });
    }
    if value.contains('\\') {
        return Err(LocalRecoveryPreimageError::InvalidLocator {
            field,
            reason: "must use canonical forward slashes",
        });
    }
    if value.split('/').any(|part| part.is_empty()) {
        return Err(LocalRecoveryPreimageError::InvalidLocator {
            field,
            reason: "must not contain empty path components",
        });
    }
    if value.split('/').any(|part| matches!(part, "." | "..")) {
        return Err(LocalRecoveryPreimageError::InvalidLocator {
            field,
            reason: "must not contain traversal components",
        });
    }
    Ok(())
}

fn validate_sealed_locator(value: &str) -> Result<(), LocalRecoveryPreimageError> {
    validate_state_relative_path(value, "locator")?;
    let components = value.split('/').collect::<Vec<_>>();
    let valid_hash = |candidate: &str| {
        candidate.len() == 64
            && candidate
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let is_valid = components.len() == 4
        && components[0] == "filesystem-epochs"
        && components[1] == "encrypted-quarantine"
        && valid_hash(components[2])
        && components[3]
            .strip_suffix(".bowline-envelope")
            .is_some_and(valid_hash);
    if !is_valid {
        return Err(LocalRecoveryPreimageError::InvalidLocator {
            field: "locator",
            reason: "must use the canonical encrypted-quarantine layout",
        });
    }
    Ok(())
}

fn domain_hash(label: &str, value: &str) -> String {
    CanonicalFrame::new(LOCAL_RECOVERY_PREIMAGE_DOMAIN)
        .str_field(label, value)
        .digest_hex()
}
