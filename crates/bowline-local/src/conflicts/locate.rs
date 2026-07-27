//! Reaching the conflict a caller-supplied aside path names.
//!
//! Every conflict verb — the read-only preview included — enters here, so all of
//! them accept and refuse exactly the same paths. Locating by scanning instead
//! (`list_conflicts`) would answer a different question: the scan prunes
//! subtrees, stops at a descriptor depth, and gives up at an entry budget, so a
//! preview built on it refuses paths the irreversible verbs act on.

use std::path::Path;

use crate::sync::manifest_engine::fs_guard::{
    AnchoredDirectory, AnchoredLeafKind, AnchoredOpen, LeafName, open_containing_directory,
};
use crate::sync::manifest_engine::{WorkspacePath, conflict_aside_origin};

use super::{ConflictAside, ConflictError, workspace_conflict_path};

/// A located conflict, together with the descriptor its two sides were seen
/// through.
///
/// One descriptor, opened by walking every component from the root no-follow,
/// carries the whole operation. Verifying the chain and THEN renaming or
/// unlinking by path would let the kernel re-resolve those components: these
/// commands run while the daemon materializes into the same tree, so a parent
/// replaced with a symlink in that window would move the rename or the delete
/// outside the workspace. The aside and its origin are siblings — the marker and
/// the prefix contain no `/` — so this one directory covers both.
pub(super) struct AnchoredConflict {
    pub(super) directory: AnchoredDirectory,
    pub(super) aside_leaf: LeafName,
    pub(super) origin_leaf: LeafName,
    /// What the aside is right now, classified through the held descriptor.
    /// `is_dir` would answer about a symlink's TARGET, making an aside symlinked
    /// at an external directory look like a directory tree of this workspace.
    pub(super) aside_kind: AnchoredLeafKind,
    pub(super) conflict: ConflictAside,
}

/// The conflict `aside` names, or the named reason there is none.
pub(super) fn open_conflict(
    root: &Path,
    aside: &WorkspacePath,
) -> Result<AnchoredConflict, ConflictError> {
    // First, and before any filesystem call: an unvalidated path would make
    // every operation below name a file outside the workspace, and one of the
    // things this workspace's verbs do with such a name is delete it.
    let aside = workspace_conflict_path(aside)?;
    let Some(origin) = conflict_aside_origin(aside.as_str()) else {
        return Err(ConflictError::NotAnAside { path: aside });
    };
    let origin = WorkspacePath::new(origin);
    let (Some(aside_leaf), Some(origin_leaf)) = (LeafName::of(&aside), LeafName::of(&origin))
    else {
        return Err(ConflictError::NotAnAside { path: aside });
    };
    let directory = match open_containing_directory(root, &aside) {
        AnchoredOpen::Ready(directory) => directory,
        // No chain, no aside: the same answer a caller gets for a name that was
        // already reconciled.
        AnchoredOpen::Absent => return Err(ConflictError::NoSuchAside { path: aside }),
        AnchoredOpen::Blocked => return Err(ConflictError::ParentNotADirectory { path: aside }),
    };

    let aside_kind = classify(&directory, &aside_leaf, &aside)?;
    if matches!(aside_kind, AnchoredLeafKind::Absent) {
        return Err(ConflictError::NoSuchAside { path: aside });
    }
    let origin_missing = matches!(
        classify(&directory, &origin_leaf, &origin)?,
        AnchoredLeafKind::Absent
    );

    Ok(AnchoredConflict {
        directory,
        aside_leaf,
        origin_leaf,
        aside_kind,
        conflict: ConflictAside {
            origin,
            aside,
            origin_missing,
        },
    })
}

/// The conflict `aside` names, for a caller that only reads.
pub fn conflict_at(root: &Path, aside: &WorkspacePath) -> Result<ConflictAside, ConflictError> {
    open_conflict(root, aside).map(|located| located.conflict)
}

fn classify(
    directory: &AnchoredDirectory,
    leaf: &LeafName,
    path: &WorkspacePath,
) -> Result<AnchoredLeafKind, ConflictError> {
    directory
        .classify(leaf)
        .map_err(|source| ConflictError::Path {
            path: path.clone(),
            operation: "inspect the conflict entry",
            source,
        })
}
