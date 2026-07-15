use bowline_core::{
    policy::{MaterializationMode, PathClassification},
    workspace_graph::FileExecutability,
};

use crate::sync::paths::is_secret_bearing_path;

pub(super) fn materialized_file_requires_owner_only(
    path: &str,
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(mode, MaterializationMode::ProjectEnv)
        || matches!(
            classification,
            PathClassification::ProjectEnv | PathClassification::SecretLooking
        )
        || is_secret_bearing_path(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MaterializedFilePermissions {
    OwnerOnly,
    Executable,
    Regular,
}

impl MaterializedFilePermissions {
    pub(super) fn for_entry(
        path: &str,
        classification: PathClassification,
        mode: MaterializationMode,
        executability: FileExecutability,
    ) -> Self {
        // Secret-bearing files stay owner-only even when the source was
        // executable; credentials should not become runnable from a mode bit.
        if materialized_file_requires_owner_only(path, classification, mode) {
            return Self::OwnerOnly;
        }
        if matches!(mode, MaterializationMode::EncryptedSync)
            && matches!(executability, FileExecutability::Regular)
        {
            return Self::OwnerOnly;
        }
        // Encrypted regular bytes stay private; encrypted executables still
        // use the product's only runnable materialized mode.
        match executability {
            FileExecutability::Executable => Self::Executable,
            FileExecutability::Regular => Self::Regular,
        }
    }

    #[cfg(unix)]
    pub(super) fn unix_mode(self) -> u32 {
        match self {
            Self::OwnerOnly => 0o600,
            Self::Executable => 0o755,
            Self::Regular => 0o644,
        }
    }
}
