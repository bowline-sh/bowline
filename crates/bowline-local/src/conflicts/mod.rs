//! The conflict-aside lifecycle: find every unreconciled aside in a workspace,
//! and reconcile one by keeping the local file or taking the incoming version.
//!
//! An aside is an ordinary synced file (see
//! [`crate::sync::manifest_engine::naming`]), so its presence on disk *is* the
//! unresolved state and its absence *is* the resolution. Nothing else records
//! it: an aside written on one device syncs to the next, where no local pull
//! ever produced an event for it, so the filesystem is the only signal both
//! devices agree on.

use std::{error::Error, fmt, io, path::PathBuf};

use bowline_core::commands::CommandRecoverability;

use crate::sync::manifest_engine::{
    MAX_WORKSPACE_PATH_LEN, PathRejection, RootFault, WorkspacePath, publishable_workspace_path,
};

mod locate;
mod read;
mod resolve;
mod scan;
mod scope;

pub use locate::conflict_at;
pub use read::{ConflictSide, read_conflict_side};
pub use resolve::{ConflictResolution, ResolvedConflict, resolve_conflict};
pub use scan::{MAX_CONFLICT_SCAN_ENTRIES, list_conflicts};
pub use scope::{ProjectScope, in_project_scope};

/// The gate every caller-supplied conflict path passes, BEFORE any filesystem
/// call — including a bare `exists()` probe.
///
/// The predicate is the engine's own publish/accept rule, so the sync writer,
/// the sync reader, and these commands agree on exactly one definition of a
/// workspace path. Without it `root.join(path)` is not confined to the
/// workspace: `../secrets.env.bowline-conflict.x` names a real file outside the
/// root that `--keep-local` would delete and `--take-remote` would rename over.
pub fn workspace_conflict_path(path: &WorkspacePath) -> Result<WorkspacePath, ConflictError> {
    // The caller's spelling is checked verbatim rather than normalized first:
    // normalization strips a leading `/`, which would silently turn an absolute
    // path into a different, existing workspace file instead of refusing it.
    match publishable_workspace_path(path.as_str(), MAX_WORKSPACE_PATH_LEN, false) {
        Ok(()) => Ok(path.clone()),
        Err(rejection) => Err(ConflictError::PathRefused {
            path: path.clone(),
            rejection,
        }),
    }
}

/// One unreconciled conflict: the file that stayed canonical, and the incoming
/// version preserved beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictAside {
    /// The path the aside sits beside — the file your tools still open.
    pub origin: WorkspacePath,
    /// The aside itself, holding the other side's bytes.
    pub aside: WorkspacePath,
    /// True when the origin no longer exists, so the aside is all that is left.
    pub origin_missing: bool,
}

#[derive(Debug)]
pub enum ConflictError {
    /// The workspace root could not be walked at all: it is missing, replaced,
    /// or unreadable. Carries the same fault the sync sentinel reports, so an
    /// unmounted drive reads identically whichever surface noticed it.
    Root { path: PathBuf, fault: RootFault },
    /// A filesystem operation on a conflict path failed.
    Path {
        path: WorkspacePath,
        operation: &'static str,
        source: io::Error,
    },
    /// The named path is not a workspace-relative path the engine will act on.
    PathRefused {
        path: WorkspacePath,
        rejection: PathRejection,
    },
    /// The named path is not a conflict-aside this engine wrote.
    NotAnAside { path: WorkspacePath },
    /// The named aside is not on disk (already reconciled, or mistyped).
    NoSuchAside { path: WorkspacePath },
    /// A component of the path's parent chain is not a real directory, so the
    /// chain cannot be descended without following it out of the workspace.
    ParentNotADirectory { path: WorkspacePath },
    /// The incoming version is a directory tree, which cannot be adopted by
    /// renaming it over the local one.
    DirectoryAside { path: WorkspacePath },
    /// The scan hit its entry budget before covering the workspace.
    ScanTruncated { visited: usize },
}

impl ConflictError {
    /// Stable tag for structured reporting; never a formatted sentence.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Root { fault, .. } => fault.tag(),
            Self::Path { .. } => "conflict_path_failed",
            Self::PathRefused { .. } => "conflict_path_refused",
            Self::NotAnAside { .. } => "not_a_conflict_aside",
            Self::NoSuchAside { .. } => "no_such_conflict_aside",
            Self::ParentNotADirectory { .. } => "conflict_parent_not_a_directory",
            Self::DirectoryAside { .. } => "conflict_aside_is_a_directory",
            Self::ScanTruncated { .. } => "conflict_scan_truncated",
        }
    }

    /// Whether repeating the same command can produce a different answer.
    ///
    /// An agent honours this literally, so anything permanent that claims
    /// `Retry` is an instruction to loop forever. Only a filesystem fault on one
    /// path — a lock, a device that comes back, a transient permission change —
    /// earns it. A scan truncation does not: the entry budget is a constant, so
    /// an identical rescan of an unchanged tree truncates identically.
    pub fn recoverability(&self) -> CommandRecoverability {
        match self {
            Self::Path { .. } => CommandRecoverability::Retry,
            Self::Root { .. }
            | Self::PathRefused { .. }
            | Self::NotAnAside { .. }
            | Self::NoSuchAside { .. }
            | Self::ParentNotADirectory { .. }
            | Self::DirectoryAside { .. }
            | Self::ScanTruncated { .. } => CommandRecoverability::UserAction,
        }
    }
}

impl fmt::Display for ConflictError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root { path, fault } => {
                write!(
                    formatter,
                    "the workspace at {} could not be listed. {}",
                    path.display(),
                    fault.reason()
                )
            }
            Self::Path {
                path,
                operation,
                source,
            } => write!(
                formatter,
                "{operation} failed for {}: {source}",
                path.as_str()
            ),
            Self::PathRefused { path, rejection } => write!(
                formatter,
                "{} is not a path inside this workspace ({}); pass the aside path exactly as `bowline conflicts` prints it",
                path.as_str(),
                rejection.reason(),
            ),
            Self::NotAnAside { path } => write!(
                formatter,
                "{} is not a conflict-aside; pass the aside path, not the file it sits beside",
                path.as_str()
            ),
            Self::NoSuchAside { path } => write!(
                formatter,
                "no conflict-aside at {}; it may already be reconciled",
                path.as_str()
            ),
            Self::ParentNotADirectory { path } => write!(
                formatter,
                "a folder on the way to {} is a symlink or a file, so it was not followed; move the conflict out from under it by hand",
                path.as_str()
            ),
            Self::DirectoryAside { path } => write!(
                formatter,
                "the incoming version at {} is a folder; resolve a folder conflict by reconciling the files inside it",
                path.as_str()
            ),
            Self::ScanTruncated { visited } => write!(
                formatter,
                "stopped after {visited} entries before covering the workspace"
            ),
        }
    }
}

impl Error for ConflictError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Path { source, .. } => Some(source),
            Self::Root { .. }
            | Self::PathRefused { .. }
            | Self::NotAnAside { .. }
            | Self::NoSuchAside { .. }
            | Self::ParentNotADirectory { .. }
            | Self::DirectoryAside { .. }
            | Self::ScanTruncated { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests;
