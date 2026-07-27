//! The engine's own thin, stat-only workspace walker (Plan 109 Step 7).
//!
//! This is deliberately NOT the project scanner (`crate::scanner`), which does
//! project/Git-health work. The engine's periodic safety net (invariant C5) and
//! its restart seed (invariant C3) need one cheap pass that answers a single
//! question: *which synced paths differ from the committed ancestor?* It stats
//! every candidate (`symlink_metadata` via [`observe`]) and compares the stat
//! fingerprint — it never opens a file or hashes a byte. The change predicate is
//! the exact inverse of push's "fingerprint-clean, never opened" skip, so a walk
//! and a subsequent push agree on what is dirty.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;

use bowline_core::git_paths::is_git_directory_path;

use super::fs_guard::{ObserveOutcome, Observed, observe_classified};
use super::manifest::{
    EntryKind, MAX_WORKSPACE_PATH_LEN, WorkspacePath, publishable_workspace_path,
};
use super::push::now_unix_ns;
use super::store::FileRecord;
use super::unsyncable::{UnsyncableReason, UnsyncableRecord, path_scoped_reason};
use crate::policy::is_private_workspace_state_path;

/// The result of one stat walk. `scanned` counts the paths stat-ed; `hashes` is
/// structurally always zero — the walker has no content-open or hashing code
/// path — and is surfaced so the C5 "zero content opens" invariant is a value a
/// test can assert, not just a comment. `unsyncable` carries the paths the walk
/// reached but cannot represent or read; they are reported, never silently
/// dropped and never fatal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatWalk {
    pub dirty: BTreeSet<WorkspacePath>,
    pub unsyncable: BTreeMap<WorkspacePath, UnsyncableRecord>,
    pub scanned: u64,
    pub hashes: u64,
}

impl StatWalk {
    fn reject(&mut self, path: WorkspacePath, reason: UnsyncableReason, errno: Option<i32>) {
        self.unsyncable
            .insert(path, UnsyncableRecord::new(reason, errno, now_unix_ns()));
    }
}

/// Walk the workspace once, statting each policy-synced path, and return the set
/// that differs from `ancestor` (new, changed, or deleted). Directories are
/// recursed per policy but only reported dirty on a create or mode change (never
/// on mtime churn from a child write, which would manufacture idle work).
pub fn stat_walk(
    root: &Path,
    policy: &crate::policy::UserPolicy,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
) -> io::Result<StatWalk> {
    stat_walk_with_scope(root, policy, ancestor, false)
}

pub fn stat_walk_project_view(
    root: &Path,
    policy: &crate::policy::UserPolicy,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
) -> io::Result<StatWalk> {
    stat_walk_with_scope(root, policy, ancestor, true)
}

/// Existing project-view paths whose bytes may be verified during an explicit
/// capture. Paths excluded by the current policy stay in the ancestor untouched
/// even when the policy changed after materialization.
pub fn project_view_verification_paths(
    policy: &crate::policy::UserPolicy,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
) -> BTreeSet<WorkspacePath> {
    ancestor
        .iter()
        .filter(|(path, record)| policy_syncs(policy, path.as_str(), Some(record), true))
        .map(|(path, _)| path.clone())
        .collect()
}

/// Refuse to walk a root that is not there.
///
/// A missing root must never read as "an empty workspace": `seen` would come
/// back empty, EVERY ancestor row would be reported deleted, and push would
/// publish an empty manifest that wipes the workspace on every other device. The
/// engine's sentinel catches this before a cycle starts; this is the second lock
/// on the same door, for the callers that reach the walker directly (work views,
/// fixtures).
fn require_root_directory(root: &Path) -> io::Result<()> {
    if fs::metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "workspace root is not an existing directory",
    ))
}

fn stat_walk_with_scope(
    root: &Path,
    policy: &crate::policy::UserPolicy,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    project_view: bool,
) -> io::Result<StatWalk> {
    require_root_directory(root)?;
    let mut walk = StatWalk::default();
    let mut seen: BTreeSet<WorkspacePath> = BTreeSet::new();
    walk_dir(
        root,
        "",
        policy,
        ancestor,
        &mut walk,
        &mut seen,
        project_view,
    );

    // Any ancestor path a synced walk did not observe is a deletion. A path the
    // policy no longer syncs is left to the ancestor untouched (it was captured
    // under a prior policy; only the user removing it makes it dirty here).
    for path in ancestor.keys() {
        if seen.contains(path) || (!project_view && is_private_workspace_state_path(path.as_str()))
        {
            continue;
        }
        if policy_syncs(policy, path.as_str(), ancestor.get(path), project_view) {
            walk.dirty.insert(path.clone());
        }
    }
    Ok(walk)
}

/// Reactively walk only watcher-reported directory subtrees.
///
/// A native recursive watcher may report a newly created directory after its
/// children already exist without emitting one reliable event per child. This
/// scoped walk turns that directory notification into complete manifest input
/// without polling or restatting unrelated workspace trees.
pub fn stat_walk_subtrees(
    root: &Path,
    policy: &crate::policy::UserPolicy,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    roots: &BTreeSet<WorkspacePath>,
) -> io::Result<StatWalk> {
    require_root_directory(root)?;
    let mut walk = StatWalk::default();
    let mut seen: BTreeSet<WorkspacePath> = BTreeSet::new();

    for subtree in outermost_roots(roots) {
        let relative = subtree.as_str();
        if is_private_workspace_state_path(relative) {
            continue;
        }
        match observe_classified(root, subtree) {
            ObserveOutcome::Present(observed) => {
                walk.scanned += 1;
                let decision = classify(policy, relative, &observed, false);
                if decision.syncs {
                    seen.insert(subtree.clone());
                    if is_dirty(&observed, ancestor.get(subtree)) {
                        walk.dirty.insert(subtree.clone());
                    }
                }
                if observed.kind == EntryKind::Directory && decision.recurse {
                    walk_dir(
                        root, relative, policy, ancestor, &mut walk, &mut seen, false,
                    );
                }
            }
            ObserveOutcome::Absent => {}
            ObserveOutcome::Unsyncable(reason) => walk.reject(subtree.clone(), reason, None),
        }

        for path in ancestor.keys() {
            if !path_is_at_or_below(path.as_str(), relative)
                || seen.contains(path)
                || is_private_workspace_state_path(path.as_str())
            {
                continue;
            }
            if policy_syncs(policy, path.as_str(), ancestor.get(path), false) {
                walk.dirty.insert(path.clone());
            }
        }
    }
    Ok(walk)
}

fn outermost_roots(roots: &BTreeSet<WorkspacePath>) -> Vec<&WorkspacePath> {
    let mut outermost = Vec::new();
    for root in roots {
        if outermost
            .iter()
            .any(|parent: &&WorkspacePath| path_is_at_or_below(root.as_str(), parent.as_str()))
        {
            continue;
        }
        outermost.push(root);
    }
    outermost
}

/// Whether `path` is `root` itself or lives under it. One owner: the scoped walk
/// and the driver's scoped attention bookkeeping must agree on "under".
pub(super) fn path_is_at_or_below(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Walk one directory, recording rather than propagating every per-path failure.
///
/// Infallible by construction, like [`observe_classified`]: a directory the
/// engine cannot open or enumerate is ONE unsyncable subtree, never a reason to
/// abandon the workspace. The previous `None => return Err(error)` arms sent an
/// unmodelled errno (ELOOP on a cyclic symlink chain, EIO on a failing disk) up
/// through `full_scan` into `CycleError::Fatal` — and because startup always
/// stat-walks, one such directory failed every start.
fn walk_dir(
    root: &Path,
    relative: &str,
    policy: &crate::policy::UserPolicy,
    ancestor: &BTreeMap<WorkspacePath, FileRecord>,
    walk: &mut StatWalk,
    seen: &mut BTreeSet<WorkspacePath>,
    project_view: bool,
) {
    let absolute = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let entries = match fs::read_dir(&absolute) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            walk.reject(
                WorkspacePath::new(relative.to_string()),
                path_scoped_reason(&error),
                error.raw_os_error(),
            );
            return;
        }
    };

    // Sort children so the walk is deterministic (ordering feeds the dirty set).
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                walk.reject(
                    WorkspacePath::new(relative.to_string()),
                    path_scoped_reason(&error),
                    error.raw_os_error(),
                );
                continue;
            }
        };
        // A name that is not UTF-8 has NO workspace path: statting its lossy
        // U+FFFD form finds nothing, so the old code silently skipped a real
        // file. Report it under its lossy name (the only thing we can print)
        // rather than pretending the workspace is fully synced.
        match child_name(&entry.file_name()) {
            Some(name) => names.push(name),
            None => {
                let lossy = entry.file_name().to_string_lossy().into_owned();
                walk.reject(
                    WorkspacePath::new(join_child(relative, &lossy)),
                    UnsyncableReason::NonUtf8Name,
                    None,
                );
            }
        }
    }
    names.sort();

    for name in names {
        let child = join_child(relative, &name);
        if !project_view && is_private_workspace_state_path(&child) {
            continue;
        }
        let path = WorkspacePath::new(child.clone());
        // The writer and the reader share one path predicate. A name that would
        // be rejected by every peer's manifest decode (an unnormalizable form
        // such as a POSIX filename containing a backslash) must never be
        // published — one such file would otherwise kill the whole fleet.
        if let Err(rejection) =
            publishable_workspace_path(&child, MAX_WORKSPACE_PATH_LEN, project_view)
        {
            walk.reject(path, rejection.into(), None);
            continue;
        }
        let observed = match observe_classified(root, &path) {
            ObserveOutcome::Present(observed) => observed,
            ObserveOutcome::Absent => continue,
            ObserveOutcome::Unsyncable(reason) => {
                walk.reject(path, reason, None);
                continue;
            }
        };
        walk.scanned += 1;
        let decision = classify(policy, &child, &observed, project_view);

        match observed.kind {
            EntryKind::Directory => {
                if decision.syncs {
                    seen.insert(path.clone());
                    if is_dirty(&observed, ancestor.get(&path)) {
                        walk.dirty.insert(path.clone());
                    }
                }
                if decision.recurse {
                    walk_dir(root, &child, policy, ancestor, walk, seen, project_view);
                }
            }
            EntryKind::File | EntryKind::Symlink => {
                if decision.syncs {
                    seen.insert(path.clone());
                    if is_dirty(&observed, ancestor.get(&path)) {
                        walk.dirty.insert(path);
                    }
                }
            }
        }
    }
}

/// A directory entry name as a workspace path component, or `None` when the name
/// is not valid UTF-8 (legal on Linux; common in archives and tarballs).
fn child_name(name: &OsStr) -> Option<String> {
    name.to_str().map(str::to_string)
}

fn join_child(relative: &str, name: &str) -> String {
    if relative.is_empty() {
        name.to_string()
    } else {
        format!("{relative}/{name}")
    }
}

struct Decision {
    syncs: bool,
    recurse: bool,
}

fn classify(
    policy: &crate::policy::UserPolicy,
    path: &str,
    observed: &Observed,
    project_view: bool,
) -> Decision {
    use crate::policy::{PathFacts, classify_path, classify_project_view_path};
    let facts = PathFacts {
        relative_path: path.to_string(),
        is_dir: observed.kind == EntryKind::Directory,
        byte_len: Some(observed.size),
    };
    let decision = if project_view {
        classify_project_view_path(&facts, policy)
    } else {
        classify_path(&facts, policy)
    };
    Decision {
        syncs: crate::policy::policy_syncs_workspace_state(&decision),
        // The `.git` directory entry is derivable and therefore not itself
        // synced, but its children contain opaque workspace state. Treat it as
        // a traversal boundary rather than pruning the entire repository state.
        recurse: (observed.kind == EntryKind::Directory && is_git_directory_path(path))
            || crate::policy::policy_should_recurse(&decision, policy, path),
    }
}

fn policy_syncs(
    policy: &crate::policy::UserPolicy,
    path: &str,
    ancestor: Option<&FileRecord>,
    project_view: bool,
) -> bool {
    use crate::policy::{PathFacts, classify_path, classify_project_view_path};
    let facts = PathFacts {
        relative_path: path.to_string(),
        is_dir: ancestor
            .map(|row| row.kind == EntryKind::Directory)
            .unwrap_or(false),
        byte_len: ancestor.map(|row| row.size),
    };
    let decision = if project_view {
        classify_project_view_path(&facts, policy)
    } else {
        classify_path(&facts, policy)
    };
    crate::policy::policy_syncs_workspace_state(&decision)
}

/// The change predicate. For files and symlinks it is the inverse of push's
/// "fingerprint-clean, same mode" skip; for directories it ignores mtime (which
/// churns on any child write) and reports only creates and mode changes.
///
/// Push is strictly stricter than this: it additionally refuses to settle a
/// matching fingerprint inside the racily-clean window (see
/// [`super::endpoint`]). The asymmetry is the safe direction — a path this
/// predicate calls clean is never published without push agreeing, while a path
/// push would re-read is one this walk has already handed it.
fn is_dirty(observed: &Observed, ancestor: Option<&FileRecord>) -> bool {
    let Some(row) = ancestor else {
        return true;
    };
    if row.kind != observed.kind {
        return true;
    }
    match observed.kind {
        EntryKind::Directory => row.mode != observed.mode,
        EntryKind::File | EntryKind::Symlink => {
            row.fingerprint != observed.fingerprint
                || row.size != observed.size
                || row.mode != observed.mode
        }
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::ids::ContentId;

    use super::*;
    use crate::sync::manifest_engine::{BlobKey, FileMode, KeyEpoch, store::StatFingerprint};

    #[test]
    fn a_size_change_is_dirty_even_when_the_stat_fingerprint_matches() {
        let fingerprint = StatFingerprint {
            mtime_ns: 1,
            ctime_ns: 2,
            inode: 3,
            dev: 4,
        };
        let ancestor = FileRecord {
            kind: EntryKind::File,
            size: 6,
            mode: FileMode::new(0o100_644),
            symlink_target: None,
            content_id: Some(ContentId::new("content_before")),
            blob_key: Some(BlobKey::new("blob_before")),
            key_epoch: Some(KeyEpoch::new(1)),
            fingerprint,
            hashed_at: Some(1),
            verified_at: None,
        };
        let observed = Observed {
            kind: EntryKind::File,
            size: 7,
            mode: ancestor.mode,
            symlink_target: None,
            fingerprint,
        };

        assert!(is_dirty(&observed, Some(&ancestor)));
    }
}
