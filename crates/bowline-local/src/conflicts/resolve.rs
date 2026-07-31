use std::path::Path;

use crate::sync::manifest_engine::WorkspacePath;
use crate::sync::manifest_engine::fs_guard::{AnchoredDirectory, AnchoredLeafKind, LeafName};

use super::locate::open_conflict;
use super::{ConflictAside, ConflictError};

/// What to do with the two versions a conflict preserved.
///
/// Both outcomes end with the aside gone, because the aside's absence is what
/// clears the conflict everywhere — status, the other devices, and any future
/// scan. They differ only in which bytes survive at the origin path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Keep the file as it is and drop the incoming version.
    KeepLocal,
    /// Replace the file with the incoming version.
    TakeRemote,
}

impl ConflictResolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeepLocal => "keep-local",
            Self::TakeRemote => "take-remote",
        }
    }
}

/// What a resolution actually did, for the receipt the caller prints and the
/// event it appends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConflict {
    pub conflict: ConflictAside,
    pub resolution: ConflictResolution,
}

/// Reconcile one conflict-aside under `root`.
///
/// `aside` is workspace-relative. The operation is a plain filesystem edit — the
/// same edit a user makes by hand — so it syncs to every device through the
/// ordinary loop with no conflict-specific protocol.
pub fn resolve_conflict(
    root: &Path,
    aside: &WorkspacePath,
    resolution: ConflictResolution,
) -> Result<ResolvedConflict, ConflictError> {
    let located = open_conflict(root, aside)?;
    let aside = located.conflict.aside.clone();

    match resolution {
        ConflictResolution::KeepLocal => {
            discard(
                &located.directory,
                &located.aside_leaf,
                &aside,
                located.aside_kind,
            )?;
        }
        ConflictResolution::TakeRemote => {
            if matches!(located.aside_kind, AnchoredLeafKind::Directory) {
                // A directory aside cannot be renamed over a populated directory;
                // the incoming tree is adopted alongside by removing the aside's
                // own name only once its contents are in place, which the
                // directory case cannot do atomically. Refuse rather than merge
                // two trees silently.
                return Err(ConflictError::DirectoryAside { path: aside });
            }
            if matches!(located.origin_kind, AnchoredLeafKind::Directory) {
                return Err(ConflictError::DirectoryOrigin {
                    path: located.conflict.origin.clone(),
                });
            }
            // Rename over the origin rather than copy-then-delete: the origin is
            // replaced atomically, so no window exists where a build, an editor,
            // or a concurrent sync scan observes the path missing or half-written.
            located
                .directory
                .rename(&located.aside_leaf, &located.origin_leaf)
                .map_err(|source| ConflictError::Path {
                    path: aside.clone(),
                    operation: "replace file with the incoming version",
                    source,
                })?;
        }
    }

    Ok(ResolvedConflict {
        conflict: located.conflict,
        resolution,
    })
}

/// Drop the aside. A symlink loses its own name and its target is left alone; a
/// real directory takes its contents with it, removed through descriptors
/// descended from the one already held.
fn discard(
    directory: &AnchoredDirectory,
    leaf: &LeafName,
    path: &WorkspacePath,
    kind: AnchoredLeafKind,
) -> Result<(), ConflictError> {
    let result = match kind {
        AnchoredLeafKind::Directory => directory.remove_tree(leaf),
        AnchoredLeafKind::NonDirectory => directory.unlink(leaf),
        // The caller refuses an absent aside before reaching here.
        AnchoredLeafKind::Absent => Ok(()),
    };
    result.map_err(|source| ConflictError::Path {
        path: path.clone(),
        operation: "discard the incoming version",
        source,
    })
}
