use std::io::{self, Cursor, Read};

use serde::{Deserialize, Serialize};

use crate::ids::ContentId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NamespaceEntryKind {
    Directory,
    File,
    Symlink,
    Placeholder,
    Tombstone,
}

/// Exactly one POSIX mode bit syncs: executable. setuid/setgid/sticky and
/// group/world-write bits are deliberately normalized away because syncing
/// them would replicate privilege-escalation surface across machines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileExecutability {
    #[default]
    Regular,
    Executable,
}

pub fn workspace_content_id(workspace_content_key: [u8; 32], bytes: &[u8]) -> ContentId {
    workspace_content_id_reader(workspace_content_key, &mut Cursor::new(bytes))
        .expect("slice hashing does not fail")
}

pub fn workspace_content_id_reader(
    workspace_content_key: [u8; 32],
    reader: &mut dyn Read,
) -> io::Result<ContentId> {
    let mut hasher = blake3::Hasher::new_keyed(&workspace_content_key);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(ContentId::new(format!(
        "cid_{}",
        hasher.finalize().to_hex()
    )))
}

pub fn normalize_workspace_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    // Stripping leading `./` can expose another `/` and vice versa (`/./x`), so
    // trim to a fixpoint. Callers use `normalize(p) == p` as a canonicality
    // guard; a single pass would let `./x` through that guard.
    loop {
        let trimmed = normalized
            .trim_start_matches("./")
            .trim_start_matches('/')
            .trim_end_matches('/');
        if trimmed.len() == normalized.len() {
            break;
        }
        normalized = trimmed.to_string();
    }
    if normalized == "." {
        String::new()
    } else {
        normalized
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    pub fn new(path: impl AsRef<str>) -> Self {
        Self(normalize_workspace_path(path.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_equal_to_or_below(&self, root: &Self) -> bool {
        root.is_empty() || self == root || self.0.starts_with(&format!("{}/", root.0))
    }
}

impl From<String> for WorkspaceRelativePath {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl From<&str> for WorkspaceRelativePath {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl<'de> serde::Deserialize<'de> for WorkspaceRelativePath {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let path = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::new(path))
    }
}

/// The workspace-relative path a symlink stored at `link_path` with `target`
/// names *lexically*, or `None` when the walk leaves the workspace root.
///
/// The kernel resolves a relative target against the directory holding the link,
/// never against the workspace root, so the walk starts at the link's own parent:
/// `docs/x -> ../README.md` lands on `README.md` and is inside, while
/// `docs/x -> ../../etc/passwd` climbs out. Absolute and empty targets always
/// escape.
///
/// This is the single lexical traversal policy for symlinks — the one owner of
/// "where does this target point", so no caller re-derives it.
pub fn resolve_symlink_target(link_path: &str, target: &str) -> Option<WorkspaceRelativePath> {
    if target.is_empty() || target.starts_with('/') {
        return None;
    }
    let link = normalize_workspace_path(link_path);
    if link.is_empty() {
        return None;
    }
    // Start at the directory holding the link. A root-level link starts empty, so
    // a single `..` already leaves the workspace.
    let mut resolved: Vec<&str> = link.split('/').collect();
    resolved.pop();
    for component in target.split('/') {
        match component {
            // A trailing or doubled separator contributes nothing, and `.` stays put.
            "" | "." => {}
            // Popping past the root escapes.
            ".." => {
                resolved.pop()?;
            }
            // Deliberately not path-normalized: on POSIX a backslash is an ordinary
            // filename character, so `..\..\x` is one child name, not a traversal.
            _ => resolved.push(component),
        }
    }
    Some(WorkspaceRelativePath::new(resolved.join("/")))
}

/// Whether a symlink stored at `link_path` with `target` names a location inside
/// the workspace root, judged lexically (see [`resolve_symlink_target`]).
///
/// This is the cheap FIRST gate, and the only gate push can apply: push decides
/// whether to publish a link whose target need not exist anywhere yet. Apply runs
/// it too, so a target that escapes on its face never reaches `symlink()`.
///
/// Being lexical, it cannot see through a component that is itself a symlink —
/// and a symlink escaping the workspace CAN exist on disk: Bowline refuses to
/// publish the user's own `~/Code/escape -> /etc`, but refusing to sync it does
/// not remove it, so a peer entry `read -> escape/passwd` is lexically contained
/// and physically outside. That is why containment is two gates: the on-disk
/// resolution
/// (`bowline_local::sync::manifest_engine::fs_guard::symlink_target_lands_in_workspace`)
/// is the second, and it is the one that decides materialization.
pub fn symlink_target_stays_in_workspace(link_path: &str, target: &str) -> bool {
    resolve_symlink_target(link_path, target).is_some()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        WorkspaceRelativePath, normalize_workspace_path, resolve_symlink_target,
        symlink_target_stays_in_workspace, workspace_content_id, workspace_content_id_reader,
    };

    #[test]
    fn content_id_is_workspace_scoped_and_path_independent() {
        let key_a = [7_u8; 32];
        let key_b = [9_u8; 32];

        let first = workspace_content_id(key_a, b"same file bytes");
        let same_workspace = workspace_content_id(key_a, b"same file bytes");
        let other_workspace = workspace_content_id(key_b, b"same file bytes");

        assert_eq!(first, same_workspace);
        assert_ne!(first, other_workspace);
        assert!(!first.as_str().contains("src/auth.ts"));
    }

    #[test]
    fn content_id_reader_matches_slice_hasher() {
        let key = [11_u8; 32];
        let bytes = (0..200_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();

        assert_eq!(
            workspace_content_id(key, &bytes),
            workspace_content_id_reader(key, &mut Cursor::new(&bytes)).expect("reader hash")
        );
    }

    #[test]
    fn workspace_paths_are_canonical_relative_paths() {
        assert_eq!(normalize_workspace_path("."), "");
        assert_eq!(normalize_workspace_path("./acme//web/src/"), "acme/web/src");
        assert_eq!(
            normalize_workspace_path("/workspace/Code/acme"),
            "workspace/Code/acme"
        );
    }

    #[test]
    fn path_normalization_reaches_a_fixpoint_in_one_call() {
        // Interleaved `/` and `./` prefixes: a single trim pass leaves a second
        // form behind, and callers guard on `normalize(p) == p`.
        for path in ["/./acme", "././acme", "/././/acme/", "./../acme"] {
            let once = normalize_workspace_path(path);
            assert_eq!(
                normalize_workspace_path(&once),
                once,
                "normalization is not idempotent for {path:?}"
            );
        }
        assert_eq!(normalize_workspace_path("/./acme"), "acme");
    }

    #[test]
    fn workspace_relative_path_deserialization_preserves_canonical_invariants() {
        let path: WorkspaceRelativePath =
            serde_json::from_str(r#""./a//b/""#).expect("path deserializes");
        let canonical = WorkspaceRelativePath::new("a/b");
        let parent = WorkspaceRelativePath::new("a");
        let later = WorkspaceRelativePath::new("a/c");

        assert_eq!(path, canonical);
        assert!(path.is_equal_to_or_below(&parent));
        assert!(path < later);
        assert_eq!(
            serde_json::from_str::<WorkspaceRelativePath>(
                &serde_json::to_string(&path).expect("path serializes")
            )
            .expect("serialized path deserializes"),
            path
        );
        assert_eq!(
            serde_json::to_string(&path).expect("canonical path serializes"),
            r#""a/b""#
        );
    }

    #[test]
    fn symlink_targets_that_climb_out_of_the_workspace_are_refused() {
        // Link path, escaping target.
        for (link, target) in [
            ("link", ""),
            ("link", "/etc/passwd"),
            ("link", ".."),
            ("link", "../outside"),
            ("docs/link", "../../outside"),
            ("docs/link", "../../../.ssh/authorized_keys"),
            // Climbs out mid-path, then comes back down under a different root.
            ("a/b/link", "../../../etc/passwd"),
            ("a/b/link", "sub/../../../../outside"),
        ] {
            assert!(
                !symlink_target_stays_in_workspace(link, target),
                "escaping target accepted: {link:?} -> {target:?}"
            );
        }
    }

    #[test]
    fn symlink_targets_that_resolve_inside_the_workspace_are_kept() {
        for (link, target) in [
            ("link", "inside"),
            ("link", "inside/file"),
            ("docs/link", "../README.md"),
            // The real shape in this repo: `.claude/skills/ads` points two levels
            // up and back down into `.agents`, never leaving the root.
            (".claude/skills/ads", "../../.agents/skills/ads"),
            // Descends and climbs back to exactly the root.
            ("a/b/link", "../../x"),
            ("a/b/link", "sub/../sibling"),
            // A backslash is an ordinary POSIX filename character, not a traversal.
            ("link", "inside\\file"),
            ("link", "inside..name/file"),
        ] {
            assert!(
                symlink_target_stays_in_workspace(link, target),
                "contained target rejected: {link:?} -> {target:?}"
            );
        }
    }

    #[test]
    fn a_contained_target_resolves_to_the_workspace_path_it_names() {
        // The filesystem-aware second gate walks this path component by component,
        // so the lexical resolution must name the location the kernel would reach.
        for (link, target, expected) in [
            ("link", "inside/file", "inside/file"),
            ("docs/link", "../README.md", "README.md"),
            (
                ".claude/skills/ads",
                "../../.agents/skills/ads",
                ".agents/skills/ads",
            ),
            ("a/b/link", "sub/../sibling", "a/b/sibling"),
            // Resolving onto the workspace root itself is contained, not an escape.
            ("docs/link", "..", ""),
        ] {
            assert_eq!(
                resolve_symlink_target(link, target).map(|path| path.as_str().to_string()),
                Some(expected.to_string()),
                "wrong resolution for {link:?} -> {target:?}"
            );
        }
        assert_eq!(resolve_symlink_target("link", "../outside"), None);
    }
}
