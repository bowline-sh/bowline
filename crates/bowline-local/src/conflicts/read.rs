use std::path::Path;

use crate::sync::manifest_engine::fs_guard::{
    FileRead, ObserveOutcome, observe_classified, read_file_bounded,
};
use crate::sync::manifest_engine::{EntryKind, WorkspacePath};

use super::{ConflictError, workspace_conflict_path};

/// One side of a conflict, as far as it can be shown to a human.
///
/// Every non-text answer is named rather than collapsed into "nothing to show",
/// because the surface that asked (`bowline resolve --diff`) prints these bytes
/// to a terminal: a caller that cannot tell "identical" from "refused to read a
/// symlink" cannot tell whether it is looking at the file it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictSide {
    /// UTF-8 text read from a regular file through the no-follow boundary.
    Text(String),
    /// A symlink. Its target is deliberately NOT read: an aside-shaped symlink
    /// can point at any file the user can read (`~/.ssh/id_rsa`), and following
    /// it would print that file's bytes under the name of a workspace path.
    Symlink,
    /// A directory, which has no line content to compare.
    Directory,
    /// Nothing is at the path.
    Missing,
    /// A regular file larger than the caller's ceiling.
    TooLarge { byte_len: u64 },
    /// A regular file whose bytes are not UTF-8 text.
    Binary,
    /// The path exists but could not be read as the file it was observed to be:
    /// unreadable, an unsupported kind, or changed under the read.
    Unreadable,
}

/// Read one side of a conflict for display, never following a symlink.
///
/// Both the parent chain and the leaf go through the engine's no-follow read
/// boundary ([`read_file_bounded`]), so no component of the path can redirect
/// the read outside the workspace root.
pub fn read_conflict_side(
    root: &Path,
    path: &WorkspacePath,
    max_bytes: u64,
) -> Result<ConflictSide, ConflictError> {
    let path = workspace_conflict_path(path)?;
    let observed = match observe_classified(root, &path) {
        ObserveOutcome::Absent => return Ok(ConflictSide::Missing),
        ObserveOutcome::Unsyncable(_) => return Ok(ConflictSide::Unreadable),
        ObserveOutcome::Present(observed) => observed,
    };
    match observed.kind {
        EntryKind::Symlink => return Ok(ConflictSide::Symlink),
        EntryKind::Directory => return Ok(ConflictSide::Directory),
        EntryKind::File => {}
    }
    if observed.size > max_bytes {
        return Ok(ConflictSide::TooLarge {
            byte_len: observed.size,
        });
    }
    // The size was just checked against the same ceiling the read enforces, so
    // an error here is a filesystem fault on one path, not a bound the caller
    // can act on: it reads as "this side cannot be shown", like a divergence.
    match read_file_bounded(root, &path, max_bytes, &observed.expected_file()) {
        Ok(FileRead::Bytes(bytes)) => {
            Ok(String::from_utf8(bytes).map_or(ConflictSide::Binary, ConflictSide::Text))
        }
        Ok(FileRead::Diverged) | Err(_) => Ok(ConflictSide::Unreadable),
    }
}
