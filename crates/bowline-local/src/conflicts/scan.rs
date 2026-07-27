use std::path::Path;

use bowline_core::{git_paths::is_git_directory_path, workspace_graph::normalize_workspace_path};

use crate::{
    policy::{PathFacts, UserPolicy, classify_path, policy_should_recurse},
    sync::manifest_engine::{
        WorkspacePath, conflict_aside_origin,
        fs_guard::{
            AnchoredDirectory, AnchoredLeafKind, AnchoredOpen, LeafName, MAX_ANCHORED_DEPTH,
            open_workspace_root,
        },
        workspace_root::{classify_root_directory, root_fault_from_io},
    },
};

use super::{ConflictAside, ConflictError, workspace_conflict_path};

/// Entry budget for one conflict scan.
///
/// The walk prunes the same subtrees sync prunes (dependencies, build output,
/// caches, local-only and blocked paths), so a workspace of ordinary source sits
/// far below this. The cap exists so a pathological tree degrades into a named,
/// reportable truncation instead of an unbounded status call.
pub const MAX_CONFLICT_SCAN_ENTRIES: usize = 200_000;

/// Every conflict-aside currently on disk under `root`, sorted by aside path.
///
/// Sorted output is load-bearing: status text, attention counts, and the
/// `bowline conflicts` listing all hash or display this order.
pub fn list_conflicts(root: &Path) -> Result<Vec<ConflictAside>, ConflictError> {
    // The sentinel's own root check, before policy discovery: `UserPolicy::load`
    // walks the tree best-effort and answers `Ok` for a root that is not there,
    // so a missing or replaced root would otherwise reach the walk and report a
    // clean empty workspace.
    if let Some(fault) = classify_root_directory(root) {
        return Err(ConflictError::Root {
            path: root.to_path_buf(),
            fault,
        });
    }
    let policy = UserPolicy::load(root).map_err(|error| ConflictError::Root {
        path: root.to_path_buf(),
        fault: root_fault_from_io(&error),
    })?;
    // The root descriptor anchors the entire walk. Every directory below it is
    // opened from the descriptor above it, never from a rebuilt path, so a
    // directory replaced with a symlink after the scan passed it cannot send the
    // walk outside the workspace — where the aside-shaped names it found would
    // be printed to the user and offered a resolve command.
    let directory = open_root(root)?;
    let mut walk = ConflictWalk {
        root,
        policy,
        visited: 0,
        found: Vec::new(),
    };
    walk.descend(&directory, "", MAX_ANCHORED_DEPTH)?;
    walk.found
        .sort_by(|left, right| left.aside.as_str().cmp(right.aside.as_str()));
    Ok(walk.found)
}

/// An unreachable ROOT is not an empty workspace: there is nothing visible to
/// report on, and answering "no conflicts" hides an unmounted drive, a
/// permission problem, or a mistyped `--root`.
fn open_root(root: &Path) -> Result<AnchoredDirectory, ConflictError> {
    open_workspace_root(root).map_err(|error| ConflictError::Root {
        path: root.to_path_buf(),
        fault: root_fault_from_io(&error),
    })
}

struct ConflictWalk<'a> {
    root: &'a Path,
    policy: UserPolicy,
    visited: usize,
    found: Vec<ConflictAside>,
}

impl ConflictWalk<'_> {
    fn descend(
        &mut self,
        directory: &AnchoredDirectory,
        relative: &str,
        remaining_depth: u32,
    ) -> Result<(), ConflictError> {
        let entries = match directory.entries() {
            Ok(entries) => entries,
            Err(error) if relative.is_empty() => {
                return Err(ConflictError::Root {
                    path: self.root.to_path_buf(),
                    fault: root_fault_from_io(&error),
                });
            }
            // An unreadable subdirectory is a permission fact about one subtree,
            // not a failure of the scan: report the conflicts that are visible
            // rather than none at all.
            Err(_) => return Ok(()),
        };
        for entry in entries {
            if self.visited >= MAX_CONFLICT_SCAN_ENTRIES {
                return Err(ConflictError::ScanTruncated {
                    visited: self.visited,
                });
            }
            self.visited += 1;
            let Some(name) = entry.name.as_str() else {
                continue; // non-UTF-8 names are unsyncable, so never asides
            };
            let child = if relative.is_empty() {
                name.to_string()
            } else {
                format!("{relative}/{name}")
            };
            let child = normalize_workspace_path(&child);
            let is_dir = matches!(entry.kind, AnchoredLeafKind::Directory);
            let decision = classify_path(
                &PathFacts {
                    relative_path: child.clone(),
                    is_dir,
                    byte_len: entry.byte_len,
                },
                &self.policy,
            );
            // The listing and the resolver share one definition of a workspace
            // path, so `bowline conflicts` never prints a resolve command that
            // `bowline resolve` would then refuse.
            if let Some(origin) = conflict_aside_origin(&child)
                && let Ok(aside) = workspace_conflict_path(&WorkspacePath::new(child.clone()))
            {
                let origin = WorkspacePath::new(origin);
                self.found.push(ConflictAside {
                    origin_missing: origin_missing(directory, &origin),
                    origin,
                    aside,
                });
            }
            // `.git/**` never carries an aside (the engine refuses to write one
            // there), and it is the largest subtree in a typical project, so
            // descending it would cost the whole walk and find nothing.
            if is_dir
                && !is_git_directory_path(&child)
                && policy_should_recurse(&decision, &self.policy, &child)
            {
                self.descend_into(directory, &entry.name, &child, remaining_depth)?;
            }
        }
        Ok(())
    }

    /// Descend one level through the parent's descriptor.
    ///
    /// A subtree nested deeper than the descriptor budget is skipped exactly as
    /// an unreadable one is: the walk holds one descriptor per level, and losing
    /// the conflicts under a 64-deep path is a far smaller harm than failing the
    /// whole listing or exhausting the process's descriptors.
    fn descend_into(
        &mut self,
        directory: &AnchoredDirectory,
        leaf: &LeafName,
        child: &str,
        remaining_depth: u32,
    ) -> Result<(), ConflictError> {
        let Some(remaining_depth) = remaining_depth.checked_sub(1) else {
            return Ok(());
        };
        // A child raced into a symlink or a file between the listing and this
        // open is `Blocked`, and skipped for the same reason an unreadable
        // subtree is: there is nothing inside the workspace to report there.
        if let AnchoredOpen::Ready(child_directory) = directory.open_directory(leaf) {
            self.descend(&child_directory, child, remaining_depth)?;
        }
        Ok(())
    }
}

/// Whether the file an aside sits beside is gone.
///
/// Classified through the aside's own directory descriptor rather than by
/// re-resolving `root.join(origin)`: the origin is the aside's sibling — the
/// marker and the prefix contain no `/` — so the descriptor the listing already
/// holds is the one that answers, and no component is resolved twice.
fn origin_missing(directory: &AnchoredDirectory, origin: &WorkspacePath) -> bool {
    let Some(leaf) = LeafName::of(origin) else {
        return true;
    };
    // A stat the filesystem refuses says nothing about absence, so only a
    // definite `Absent` marks the origin gone.
    matches!(directory.classify(&leaf), Ok(AnchoredLeafKind::Absent))
}
