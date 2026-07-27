//! The one side-effect taxonomy. It types `CommandSpec::side_effect_level`, the
//! machine-contract descriptor, and the `--dry-run` risk, which used to be
//! overlapping stringly-typed vocabularies in separate files.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideEffectLevel {
    /// Pure discovery: no local, daemon, or hosted state is read or written.
    None,
    /// Reads state, changes nothing.
    Read,
    /// Takes over the terminal; changes nothing on its own.
    Interactive,
    /// Writes local workspace or account state that is not covered by a more
    /// specific level below.
    Mutation,
    /// Mutates only when an explicit apply flag is passed; previews otherwise.
    ConditionalMutation,
    /// Grants or removes device trust.
    TrustChange,
    /// Creates, rotates, or consumes recovery secret material.
    SecretMaterial,
    /// Writes work-view or namespace metadata.
    WorkspaceMetadata,
    /// Writes project files in the workspace.
    FilesystemWrite,
    /// Deletes materialized bytes on this device.
    LocalFilesystemDelete,
    /// Mutates a remote host.
    RemoteMutation,
    /// Marks remote objects collectible after a grace window.
    RemoteDestructionScheduled,
    /// Starts or stops the local daemon process.
    DaemonMutation,
    /// Installs, restarts, or removes the OS service.
    ServiceMutation,
}

impl SideEffectLevel {
    pub fn token(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Read => "read",
            Self::Interactive => "interactive",
            Self::Mutation => "mutation",
            Self::ConditionalMutation => "conditional-mutation",
            Self::TrustChange => "trust-change",
            Self::SecretMaterial => "secret-material",
            Self::WorkspaceMetadata => "workspace-metadata",
            Self::FilesystemWrite => "filesystem-write",
            Self::LocalFilesystemDelete => "local-filesystem-delete",
            Self::RemoteMutation => "remote-mutation",
            Self::RemoteDestructionScheduled => "remote-destruction-scheduled",
            Self::DaemonMutation => "daemon-mutation",
            Self::ServiceMutation => "service-mutation",
        }
    }
}

impl std::fmt::Display for SideEffectLevel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.token())
    }
}
