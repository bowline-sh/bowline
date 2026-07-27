//! Command contracts for the conflict-aside lifecycle: list what is
//! unreconciled, and reconcile one.

use serde::{Deserialize, Serialize};

use crate::status::RepairCommand;

use super::CommandName;

/// One unreconciled conflict as the CLI reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictAsideSummary {
    /// Workspace-relative path of the file that kept your version.
    pub origin_path: String,
    /// Workspace-relative path of the aside holding the incoming version.
    pub aside_path: String,
    /// The origin no longer exists locally, so the aside is the only copy left.
    pub origin_missing: bool,
    /// The exact command that reconciles this one conflict, ready to run.
    pub resolve_command: String,
}

/// What `bowline resolve` was asked to do with the two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictAction {
    /// Keep the file as it stands and drop the incoming version.
    KeepLocal,
    /// Replace the file with the incoming version.
    TakeRemote,
    /// Show the difference between the two and change nothing.
    Diff,
}

impl ConflictAction {
    pub fn changes_files(self) -> bool {
        match self {
            Self::KeepLocal | Self::TakeRemote => true,
            Self::Diff => false,
        }
    }
}

/// Why `--diff` compared nothing.
///
/// Named per case rather than collapsed into an absent diff: the two sides are
/// read through the engine's no-follow boundary, so "one side is a symlink" is a
/// refusal to read (following it would print a file from outside the workspace),
/// not an empty comparison. A caller that cannot tell those apart cannot tell
/// whether it saw the file it named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffUnavailable {
    /// One side is a symlink, whose target is never followed or printed.
    Symlink,
    /// One side is a directory, which has no lines to compare.
    Directory,
    /// One side is not on disk.
    Missing,
    /// One side is larger than the diff ceiling.
    TooLarge,
    /// One side is not UTF-8 text.
    Binary,
    /// One side exists but could not be read as the file it appeared to be.
    Unreadable,
}

impl DiffUnavailable {
    /// The sentence a human surface prints in place of a diff. Owned here so
    /// every surface explains the same refusal the same way.
    pub fn message(self) -> &'static str {
        match self {
            Self::Symlink => {
                "one side is a symlink; its target is not read, because it can point outside the workspace"
            }
            Self::Directory => "one side is a directory; reconcile the files inside it",
            Self::Missing => "one side is no longer on disk",
            Self::TooLarge => "one side is too large to diff here; open them side by side",
            Self::Binary => "one side is not text; open them side by side",
            Self::Unreadable => "one side could not be read",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictsCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_root: String,
    pub conflicts: Vec<ConflictAsideSummary>,
    pub next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub workspace_root: String,
    pub conflict: ConflictAsideSummary,
    pub action: ConflictAction,
    /// Whether the workspace changed. False for `--diff`, and the field the
    /// caller checks rather than re-deriving intent from `action`.
    pub changed: bool,
    /// Unified diff between the file and its aside; present only for `--diff`,
    /// and omitted whenever the two sides could not be compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Why `diff` is absent. Set for `--diff` exactly when `diff` is not, so a
    /// caller never has to guess whether an absent diff means "identical".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_unavailable: Option<DiffUnavailable>,
    pub next_actions: Vec<RepairCommand>,
}
