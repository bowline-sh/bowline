//! The Git contract for pull/apply: when a repository's own paths may be
//! written, and in what order.
//!
//! Split from `apply.rs` at the domain seam its module doc already named — the
//! apply *transaction* versus the rules that make writing INTO a Git repository
//! safe. Nothing here reads the intent journal, the merge plan, or the ancestor;
//! it answers two questions about a path: is Git holding a lock on the repo that
//! owns it, and where does it sort so no ref ever points at an object that has
//! not landed yet.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use bowline_core::git_paths::classify_git_path;

use crate::sync::manifest_engine::manifest::WorkspacePath;

/// How many ops one cached `.git/refs` walk covers before it is walked again. A
/// refs lock taken partway through a long plan must still stop the ops after it;
/// walking the tree on every op is what made the check quadratic in the first
/// place. Nothing else in the lock probe is cached — see [`GitLockCache`].
const GIT_LOCK_REPROBE_OPS: u32 = 256;

/// Per-repo git-lock state for the length of one apply plan.
///
/// Only the RECURSIVE `.git/refs` walk is cached, because only it is expensive: a
/// first pull materializes tens of thousands of `.git/objects/**` entries and
/// used to walk the refs tree once per op. The three single-file locks
/// (`index.lock`, `HEAD.lock`, `packed-refs.lock`) are three stats, so they are
/// re-probed on EVERY op and no "unlocked" answer for them is ever cached.
///
/// That asymmetry is the whole design: `index.lock` is the lock `git add`,
/// `git commit`, `git checkout` and `git rebase` take, so a verdict cached across
/// the following 255 ops meant Bowline kept writing remote opaque Git state into
/// a repo with a live Git transaction — the corruption this guard exists to
/// prevent. Three stats per op close that window while keeping the amortization.
#[derive(Default)]
pub(crate) struct GitLockCache {
    refs_locked: BTreeMap<String, bool>,
    ops_since_probe: u32,
    /// Recursive `.git/refs` walks actually performed. The whole point of this
    /// cache is that this stays bounded by `ops / GIT_LOCK_REPROBE_OPS` rather
    /// than tracking the op count, so it is a value a test can assert. It has no
    /// production consumer, so it exists only in test builds.
    #[cfg(test)]
    probes: u32,
}

impl GitLockCache {
    pub(crate) fn is_active(&mut self, root: &Path, path: &WorkspacePath) -> bool {
        let Some(git_dir) = git_dir_for(path.as_str()) else {
            return false; // ordinary source path: no repo, no probe
        };
        self.ops_since_probe = self.ops_since_probe.saturating_add(1);
        if self.ops_since_probe >= GIT_LOCK_REPROBE_OPS {
            self.ops_since_probe = 0;
            self.refs_locked.clear();
        }
        let git_path = root.join(&git_dir);
        // Uncached and first: a lock Git took since the last op stops the very next
        // one, not the one 255 ops from now.
        if single_file_lock_present(&git_path) {
            return true;
        }
        if let Some(locked) = self.refs_locked.get(&git_dir) {
            return *locked;
        }
        let locked = refs_lock_present(&git_path);
        self.refs_locked.insert(git_dir, locked);
        #[cfg(test)]
        {
            self.probes = self.probes.saturating_add(1);
        }
        locked
    }

    #[cfg(test)]
    pub(crate) fn probes(&self) -> u32 {
        self.probes
    }
}

/// Whether a Git lock is active for the repo containing `path`. While active,
/// that repo's paths defer (auto-rescan after the lock clears).
pub fn git_lock_active(root: &Path, path: &WorkspacePath) -> bool {
    match git_dir_for(path.as_str()) {
        Some(git_dir) => git_lock_active_in(&root.join(git_dir)),
        None => false,
    }
}

/// The full lock probe for one resolved git directory: the cheap single-file
/// locks plus the recursive refs walk. [`GitLockCache`] composes the same two
/// halves, but on different schedules.
fn git_lock_active_in(git_dir: &Path) -> bool {
    single_file_lock_present(git_dir) || refs_lock_present(git_dir)
}

/// The locks Git takes at a fixed, known path — three stats, no traversal. Cheap
/// enough that [`GitLockCache`] re-probes them on every op.
fn single_file_lock_present(git_dir: &Path) -> bool {
    ["index.lock", "HEAD.lock", "packed-refs.lock"]
        .iter()
        .any(|lock| git_dir.join(lock).exists())
}

pub(crate) fn refs_lock_present(git_dir: &Path) -> bool {
    fn any_lock(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if any_lock(&path) {
                    return true;
                }
            } else if path.extension().is_some_and(|ext| ext == "lock") {
                return true;
            }
        }
        false
    }
    any_lock(&git_dir.join("refs"))
}

pub(crate) fn git_dir_for(path: &str) -> Option<String> {
    let marker = "/.git/";
    if let Some(index) = path.find(marker) {
        return Some(format!("{}/.git", &path[..index]));
    }
    if path == ".git" || path.starts_with(".git/") {
        return Some(".git".to_string());
    }
    path.strip_suffix("/.git")
        .map(|prefix| format!("{prefix}/.git"))
}

pub(crate) fn is_git_lock_path(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|leaf| leaf.ends_with(".lock"))
        && path.contains(".git/")
}

/// Apply-order rank: within a Git repo, `objects/**` must land before
/// `refs`/`packed-refs`/`HEAD`/`index` so no ref points at a missing object.
pub fn git_apply_rank(path: &str) -> u8 {
    classify_git_path(path).map_or(1, |class| class.apply_rank())
}
