/// Role of a path inside a `.git` directory, classified by well-known name
/// shape only. Bowline never runs git or parses git file contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitPathClass {
    /// `objects/**` content-addressed state; apply before refs that can name it.
    ImmutableObject,
    /// `HEAD`, `refs/**`, `packed-refs`, and `shallow`; apply after objects.
    PointerState,
    /// Machine-local, derivable, or transient git state.
    DerivableVolatile,
    /// Everything else under `.git`, synced opaquely as today.
    OrdinaryState,
}

impl GitPathClass {
    pub fn apply_rank(self) -> u8 {
        match self {
            Self::ImmutableObject => 0,
            // A ref must never materialize before the objects it can point at.
            Self::OrdinaryState | Self::DerivableVolatile => 1,
            Self::PointerState => 2,
        }
    }

    pub fn is_derivable_volatile(self) -> bool {
        self == Self::DerivableVolatile
    }
}

/// Machine-local git state, and nothing else.
///
/// The split here is deliberate and load-bearing. An interrupted merge, rebase,
/// cherry-pick, revert, or bisect is uncommitted work: the conflicted worktree
/// files sync, and the index that stages them syncs, so the operation head that
/// names them must sync too. A device that received the conflicted tree without
/// `MERGE_HEAD` or `rebase-merge/` cannot run `git rebase --continue` or commit
/// the merge, which is exactly the stranding Bowline exists to remove.
///
/// What stays local is state that is derivable, per-machine, or scratch: reflogs
/// (`logs/`), the last fetch and pre-operation heads, gc bookkeeping, git's own
/// lockfiles, and loose-object/pack temp files.
const GIT_VOLATILE_ROOT_NAMES: &[&str] = &["FETCH_HEAD", "ORIG_HEAD", "gc.log", "gc.pid"];

const GIT_VOLATILE_DIR_PREFIXES: &[&str] = &["logs/"];
const GIT_VOLATILE_DIR_NAMES: &[&str] = &["logs"];
const GIT_WORKTREE_LOCAL_ROOT_NAMES: &[&str] =
    &["gitdir", "commondir", "locked", "config.worktree"];

pub fn classify_git_path(path: &str) -> Option<GitPathClass> {
    if is_git_directory_path(path) {
        return Some(GitPathClass::DerivableVolatile);
    }
    let tail_path = git_tail_path(path)?;
    let components = tail_path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if components.is_empty() {
        return None;
    }

    classify_git_tail(&components)
}

/// Whether `path` names the root directory of a Git repository inside a
/// workspace. The directory entry itself is derivable, but walkers must still
/// descend through it to reach the opaque state that syncs.
pub fn is_git_directory_path(path: &str) -> bool {
    path == ".git" || path.ends_with("/.git")
}

pub fn is_git_derivable_volatile_path(path: &str) -> bool {
    classify_git_path(path).is_some_and(GitPathClass::is_derivable_volatile)
}

fn git_tail_path(path: &str) -> Option<&str> {
    if let Some(tail) = path.strip_prefix(".git/") {
        return Some(tail);
    }
    path.find("/.git/")
        .map(|index| &path[index + "/.git/".len()..])
}

fn classify_git_tail(tail: &[&str]) -> Option<GitPathClass> {
    match tail.first() {
        Some(&"worktrees") if tail.len() >= 3 => Some(classify_worktree_tail(&tail[2..])),
        Some(&"modules") if tail.len() >= 3 => Some(classify_module_tail(&tail[1..])),
        Some(_) => Some(classify_plain_git_tail(tail)),
        None => None,
    }
}

fn classify_worktree_tail(tail: &[&str]) -> GitPathClass {
    if tail
        .first()
        .is_some_and(|root| GIT_WORKTREE_LOCAL_ROOT_NAMES.contains(root))
    {
        return GitPathClass::DerivableVolatile;
    }
    classify_git_tail(tail).unwrap_or(GitPathClass::OrdinaryState)
}

fn classify_module_tail(tail: &[&str]) -> GitPathClass {
    if tail.len() == 2 {
        return classify_git_tail(&tail[1..]).unwrap_or(GitPathClass::OrdinaryState);
    }
    for gitdir_start in 1..tail.len() {
        if classify_nested_module_suffix(&tail[gitdir_start..])
            == Some(GitPathClass::DerivableVolatile)
        {
            return GitPathClass::DerivableVolatile;
        }
    }
    for gitdir_start in (1..tail.len()).rev() {
        let class = classify_nested_module_suffix(&tail[gitdir_start..]);
        if let Some(class) = class {
            return class;
        }
    }
    GitPathClass::OrdinaryState
}

fn classify_nested_module_suffix(tail: &[&str]) -> Option<GitPathClass> {
    // Nested submodule paths and gitdir internals share one path namespace under
    // `.git/modules`; avoid directory-prefix drops when the boundary is unclear.
    let root_name = tail[0];
    let final_name = tail[tail.len() - 1];
    let tail_path = tail.join("/");

    if tail.len() == 1
        && (GIT_VOLATILE_ROOT_NAMES.contains(&root_name)
            || GIT_VOLATILE_DIR_NAMES.contains(&root_name))
    {
        return Some(GitPathClass::DerivableVolatile);
    }
    if tail.len() == 1 && root_name.ends_with(".lock") {
        return Some(GitPathClass::DerivableVolatile);
    }
    if is_git_temp_object_path(tail, &tail_path, final_name) {
        return Some(GitPathClass::DerivableVolatile);
    }
    if matches!(tail_path.as_str(), "HEAD" | "packed-refs" | "shallow")
        || tail_path.starts_with("refs/")
    {
        return Some(GitPathClass::PointerState);
    }
    if tail_path.starts_with("objects/") {
        return Some(GitPathClass::ImmutableObject);
    }
    None
}

fn classify_plain_git_tail(tail: &[&str]) -> GitPathClass {
    let tail_path = tail.join("/");
    let root_name = tail[0];
    let final_name = tail[tail.len() - 1];

    if tail.len() == 1 && GIT_VOLATILE_ROOT_NAMES.contains(&root_name) {
        return GitPathClass::DerivableVolatile;
    }
    if tail.len() == 1 && GIT_VOLATILE_DIR_NAMES.contains(&root_name) {
        return GitPathClass::DerivableVolatile;
    }
    if GIT_VOLATILE_DIR_PREFIXES
        .iter()
        .any(|prefix| tail_path.starts_with(prefix))
    {
        return GitPathClass::DerivableVolatile;
    }
    // Inside a git dir, every `*.lock` path is git's own lockfile; repository
    // content lockfiles cannot reach this branch because `.git` already matched.
    if tail.iter().any(|component| component.ends_with(".lock")) {
        return GitPathClass::DerivableVolatile;
    }
    // Git writes loose-object and pack temp files under `objects/**`; they are
    // operation scratch and should not outlive the local git command.
    if is_git_temp_object_path(tail, &tail_path, final_name) {
        return GitPathClass::DerivableVolatile;
    }
    if matches!(tail_path.as_str(), "HEAD" | "packed-refs" | "shallow")
        || tail_path.starts_with("refs/")
    {
        return GitPathClass::PointerState;
    }
    // `objects/info/*` is mutable metadata, but applying it early is safe and
    // keeps object-before-ref ordering owned by one class.
    if tail_path.starts_with("objects/") {
        return GitPathClass::ImmutableObject;
    }
    GitPathClass::OrdinaryState
}

fn is_git_temp_object_path(tail: &[&str], tail_path: &str, final_name: &str) -> bool {
    if !tail_path.starts_with("objects/") {
        return false;
    }
    final_name.starts_with("tmp_")
        || tail
            .iter()
            .skip(1)
            .any(|component| component.starts_with("tmp_"))
}

#[cfg(test)]
mod tests {
    use super::{GitPathClass, classify_git_path, is_git_directory_path};

    #[test]
    fn classifies_git_path_shapes() {
        let cases = [
            (".git/index", Some(GitPathClass::OrdinaryState)),
            (
                "repo/.git/refs/heads/main",
                Some(GitPathClass::PointerState),
            ),
            (".git/objects/ab/cdef", Some(GitPathClass::ImmutableObject)),
            (
                ".git/objects/pack/pack-x.idx",
                Some(GitPathClass::ImmutableObject),
            ),
            (
                ".git/objects/pack/tmp_pack_1",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/objects/pack/tmp_pack_1/pack-123.pack",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/objects/ab/tmp_obj_1",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/packed-refs.lock",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/refs/heads/main.lock",
                Some(GitPathClass::DerivableVolatile),
            ),
            (".git/config.lock", Some(GitPathClass::DerivableVolatile)),
            (".git/logs", Some(GitPathClass::DerivableVolatile)),
            (".git/logs/HEAD", Some(GitPathClass::DerivableVolatile)),
            (".git/rebase-merge", Some(GitPathClass::OrdinaryState)),
            (".git/rebase-merge/done", Some(GitPathClass::OrdinaryState)),
            (".git/sequencer", Some(GitPathClass::OrdinaryState)),
            (".git/MERGE_HEAD", Some(GitPathClass::OrdinaryState)),
            (".git/MERGE_MSG", Some(GitPathClass::OrdinaryState)),
            (".git/MERGE_MODE", Some(GitPathClass::OrdinaryState)),
            (".git/MERGE_AUTOSTASH", Some(GitPathClass::OrdinaryState)),
            (".git/AUTO_MERGE", Some(GitPathClass::OrdinaryState)),
            (".git/CHERRY_PICK_HEAD", Some(GitPathClass::OrdinaryState)),
            (".git/REVERT_HEAD", Some(GitPathClass::OrdinaryState)),
            (".git/REBASE_HEAD", Some(GitPathClass::OrdinaryState)),
            (".git/COMMIT_EDITMSG", Some(GitPathClass::OrdinaryState)),
            (".git/SQUASH_MSG", Some(GitPathClass::OrdinaryState)),
            (".git/FETCH_HEAD", Some(GitPathClass::DerivableVolatile)),
            (".git/ORIG_HEAD", Some(GitPathClass::DerivableVolatile)),
            (".git/gc.log", Some(GitPathClass::DerivableVolatile)),
            (".git/modules/sub/index", Some(GitPathClass::OrdinaryState)),
            (
                ".git/modules/libs/foo/index",
                Some(GitPathClass::OrdinaryState),
            ),
            (
                ".git/modules/libs/logs/config",
                Some(GitPathClass::OrdinaryState),
            ),
            (
                ".git/modules/foo/logs/HEAD",
                Some(GitPathClass::PointerState),
            ),
            (
                ".git/modules/foo/rebase-merge/head-name",
                Some(GitPathClass::OrdinaryState),
            ),
            (
                ".git/modules/libs/foo/logs",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/modules/libs/foo/rebase-merge",
                Some(GitPathClass::OrdinaryState),
            ),
            (
                ".git/modules/libs/foo/rebase-apply",
                Some(GitPathClass::OrdinaryState),
            ),
            (
                ".git/modules/libs/foo/sequencer",
                Some(GitPathClass::OrdinaryState),
            ),
            (
                ".git/modules/libs/logs/HEAD",
                Some(GitPathClass::PointerState),
            ),
            (
                ".git/modules/libs/foo/refs/heads/main",
                Some(GitPathClass::PointerState),
            ),
            (
                ".git/modules/libs/foo/objects/ab/cdef",
                Some(GitPathClass::ImmutableObject),
            ),
            (
                ".git/modules/vendor/foo.lock/config",
                Some(GitPathClass::OrdinaryState),
            ),
            (
                ".git/modules/vendor/foo.lock/objects/ab/cdef",
                Some(GitPathClass::ImmutableObject),
            ),
            (
                ".git/modules/vendor/foo/refs/heads/main.lock",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/modules/libs/foo/objects/pack/tmp_pack_1/pack-123.pack",
                Some(GitPathClass::DerivableVolatile),
            ),
            (".git/worktrees/wt/index", Some(GitPathClass::OrdinaryState)),
            (
                ".git/worktrees/wt/gitdir",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/worktrees/wt/commondir",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/worktrees/wt/locked",
                Some(GitPathClass::DerivableVolatile),
            ),
            (
                ".git/worktrees/wt/config.worktree",
                Some(GitPathClass::DerivableVolatile),
            ),
            (".git/worktrees/wt/HEAD", Some(GitPathClass::PointerState)),
            (".git/config", Some(GitPathClass::OrdinaryState)),
            (".git/hooks/pre-commit", Some(GitPathClass::OrdinaryState)),
            (".git/info/exclude", Some(GitPathClass::OrdinaryState)),
            ("Cargo.lock", None),
            ("src/foo.lock", None),
            ("foo.git/index", None),
            ("sub/.git", Some(GitPathClass::DerivableVolatile)),
            (".git/BISECT_LOG", Some(GitPathClass::OrdinaryState)),
            (".git/shallow", Some(GitPathClass::PointerState)),
        ];

        for (path, expected) in cases {
            assert_eq!(classify_git_path(path), expected, "{path}");
        }
    }

    /// An interrupted operation must travel as one bundle: the conflicted
    /// worktree, the index that stages it, and the operation head that names it.
    /// Any one of these classified apart from the others strands the user on the
    /// second machine.
    #[test]
    fn in_progress_operation_state_travels_with_the_index_it_belongs_to() {
        for path in [
            ".git/index",
            ".git/MERGE_HEAD",
            ".git/MERGE_MSG",
            ".git/CHERRY_PICK_HEAD",
            ".git/REVERT_HEAD",
            ".git/REBASE_HEAD",
            ".git/rebase-merge/head-name",
            ".git/rebase-apply/next",
            ".git/sequencer/todo",
        ] {
            assert_eq!(
                classify_git_path(path),
                Some(GitPathClass::OrdinaryState),
                "{path}"
            );
        }
    }

    #[test]
    fn apply_rank_puts_objects_before_refs() {
        assert!(
            GitPathClass::ImmutableObject.apply_rank() < GitPathClass::OrdinaryState.apply_rank()
        );
        assert!(GitPathClass::OrdinaryState.apply_rank() < GitPathClass::PointerState.apply_rank());
    }

    #[test]
    fn identifies_only_git_directory_roots() {
        assert!(is_git_directory_path(".git"));
        assert!(is_git_directory_path("repo/.git"));
        assert!(!is_git_directory_path("repo/.git/HEAD"));
        assert!(!is_git_directory_path("repo.git"));
    }
}
