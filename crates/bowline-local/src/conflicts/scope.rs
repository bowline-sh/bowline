//! Narrowing a conflict listing to one project.
//!
//! Status and `bowline conflicts` list the same asides from the same scan, so
//! they answer `--project` from this one predicate: two copies of it disagreed
//! about the workspace root, and one of them reported a clean workspace for a
//! root that was full of conflicts.

use crate::sync::manifest_engine::conflict_aside_origin;

use super::ConflictAside;

/// A workspace-relative directory a conflict listing is confined to.
///
/// The workspace root is deliberately not representable. Relative to the root
/// the prefix is the empty string, which no aside path is ever under, so an
/// empty scope filtered every conflict out — the surface whose job is listing
/// conflicts reported none. `None` is the root, and `None` means everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectScope(String);

impl ProjectScope {
    /// The scope a workspace-relative path names, or `None` when it names the
    /// workspace root itself and therefore narrows nothing.
    pub fn new(relative: &str) -> Option<Self> {
        let trimmed = relative.trim_matches('/');
        (!trimmed.is_empty()).then(|| Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether an aside sits inside this scope: under it as a directory, or
    /// beside the very file the scope names.
    ///
    /// `--project` documents a "project or path", so naming the conflicted file
    /// is the obvious way to ask about it — and the directory rule alone excludes
    /// exactly that, because the aside's remainder after `src/auth.ts` starts with
    /// `.bowline-conflict.` rather than `/`. The surface whose job is listing that
    /// file's conflict reported none.
    pub fn contains(&self, conflict: &ConflictAside) -> bool {
        self.holds(conflict.aside.as_str()) || self.names_origin_of(conflict.origin.as_str())
    }

    /// Whether `path` lies under this scope as a directory. The separator is
    /// required so `apps/web` never claims the conflicts of `apps/web-legacy`.
    fn holds(&self, path: &str) -> bool {
        path.strip_prefix(&self.0)
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Whether this scope names the file `origin`'s aside chain ultimately sits
    /// beside. The chain is walked because an aside of an aside reports the aside
    /// it displaced, not the source file — and the source file is what the user
    /// named.
    fn names_origin_of(&self, origin: &str) -> bool {
        let mut origin = origin;
        loop {
            if origin == self.0 {
                return true;
            }
            match conflict_aside_origin(origin) {
                Some(next) => origin = next,
                None => return false,
            }
        }
    }
}

/// Whether a conflict belongs in a listing narrowed to `scope`; `None` is the
/// whole workspace.
pub fn in_project_scope(conflict: &ConflictAside, scope: Option<&ProjectScope>) -> bool {
    scope.is_none_or(|scope| scope.contains(conflict))
}
