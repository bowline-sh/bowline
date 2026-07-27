//! Deterministic naming for conflict-asides, staging temps, and
//! quarantine entries: content-derived, wall-clock-free, collision-safe.

use bowline_core::git_paths::classify_git_path;

use crate::sync::manifest_engine::manifest::{
    BlobKey, MAX_WORKSPACE_PATH_LEN, ManifestEntry, WorkspacePath, publishable_workspace_path,
};
use crate::sync::manifest_engine::push::EngineContext;

/// The marker segment that makes a path a conflict-aside.
///
/// Load-bearing shape, not decoration:
/// - it is appended AFTER the original name, so the original extension no longer
///   terminates the path and source globs (`*.ts`, `**/*.rs`), tsconfig
///   `include`, and language servers stop picking asides up as source;
/// - it contains no space, quote, or parenthesis, so the path survives a shell
///   word, a `git add`, and an agent prompt without quoting rituals;
/// - it is one literal string, so `git status`, `rg`, and a `.gitignore` line can
///   all name every aside at once.
pub const CONFLICT_ASIDE_MARKER: &str = ".bowline-conflict.";

/// The lowest collision suffix the engine ever appends. The base name is
/// already the first alternative, so `.1` is never written — and therefore is
/// not part of the grammar a name must match to be recognized as an aside.
const FIRST_COLLISION_SUFFIX: u32 = 2;

/// The disambiguating segment of an aside name: exactly
/// [`AsidePrefix::LEN`] lowercase hex characters.
///
/// A newtype with one constructor because this segment is the whole grammar. A
/// name is treated as engine-authored — listed by `bowline conflicts`, deleted
/// by `--keep-local`, renamed over its neighbour by `--take-remote` — only when
/// it parses back, so a caller free to supply any string could mint asides that
/// no longer parse, or shapes a user's own file could wear by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsidePrefix(String);

impl AsidePrefix {
    /// Characters in the prefix. Long enough that two different remote versions
    /// of one path get different names in practice, short enough to keep the
    /// resulting path inside the workspace path budget.
    const LEN: usize = 8;

    /// The only constructor: the leading hex of a BLAKE3 digest of `seed`.
    ///
    /// Hashing rather than slicing the seed is what makes the grammar total. A
    /// symlink entry's identity is its target, whose trailing characters can be
    /// `/` or `.`, and a directory's is a three-letter word; neither would
    /// parse back, so asides the engine itself wrote would be invisible to the
    /// scanner and unresolvable by name.
    pub fn derive(seed: &str) -> Self {
        Self(blake3::hash(seed.as_bytes()).to_hex()[..Self::LEN].to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The aside name `path` would carry under `prefix`, or `None` when this path
/// may not carry an aside at all.
///
/// THE gate every aside writer passes through, because both refusals belong to
/// the aside concept and not to one writer. Pull reached them through
/// [`free_aside_path`] while work-view accept composed a name directly, so accept
/// could publish an aside under `.git/**` — which every device then materializes,
/// since `install_entry` never refuses on aside grounds — or a name past the path
/// budget, which fails the whole publish instead of one path.
pub fn permitted_aside_path(path: &WorkspacePath, prefix: &AsidePrefix) -> Option<WorkspacePath> {
    if !aside_is_permitted(path) {
        return None;
    }
    let candidate = aside_path_with_prefix(path, prefix);
    usable_aside_path(&candidate).then_some(candidate)
}

/// The first free aside path for `entry` at `path`, or `None` when this path may
/// not carry an aside at all (see [`permitted_aside_path`]).
pub(crate) fn free_aside_path(
    ctx: &EngineContext,
    path: &WorkspacePath,
    entry: &ManifestEntry,
) -> Option<WorkspacePath> {
    let base = permitted_aside_path(path, &entry_manifest_prefix(entry))?;
    if !ctx.workspace_root.join(base.as_str()).exists() {
        return Some(base);
    }
    // Deterministic collision suffix; no wall-clock.
    for suffix in FIRST_COLLISION_SUFFIX..u32::MAX {
        let candidate = collision_aside_path(&base, suffix);
        if !usable_aside_path(&candidate) {
            return None;
        }
        if !ctx.workspace_root.join(candidate.as_str()).exists() {
            return Some(candidate);
        }
    }
    None
}

/// Whether a generated aside name is one this workspace would itself accept.
///
/// The marker and hash lengthen the origin, so a source path near the ceiling
/// yields an aside past it. Without this check the writer can emit a name the
/// reader refuses: materialization either fails outright, or succeeds and leaves
/// a preserved version that `bowline conflicts` will not list and `bowline
/// resolve` will not accept — a conflict the product cannot see or reconcile.
/// Refusing here keeps the divergence a keep-local instead, which is visible.
fn usable_aside_path(path: &WorkspacePath) -> bool {
    publishable_workspace_path(path.as_str(), MAX_WORKSPACE_PATH_LEN, false).is_ok()
}

/// Whether an aside may be materialized for `path`.
///
/// Git's own state is the one place a second file is destructive rather than
/// helpful: `.git/index`, `.git/HEAD`, and `.git/objects/**` are read by name and
/// by directory listing, so an extra sibling either corrupts the repository or
/// gets swept up by `git gc`. There is no aside that preserves both sides here,
/// so the divergence stays a keep-local: the workspace copy remains canonical and
/// pushes back, exactly as it does when an aside has nowhere safe to land.
pub(crate) fn aside_is_permitted(path: &WorkspacePath) -> bool {
    classify_git_path(path.as_str()).is_none()
}

/// THE conflict-aside naming scheme (single source of truth):
///
///   `<workspace-path>.bowline-conflict.<content-prefix>`
///
/// The name is derived only from the losing (remote) entry: its path and a
/// content-derived prefix (`entry_manifest_prefix`). It carries NO device id and
/// NO wall-clock. This is load-bearing: asides themselves sync, so two devices
/// materializing the SAME remote conflict for the SAME path must produce the
/// SAME name, or sync would treat them as two entries and spawn endless
/// duplicate conflict copies. Different content for the same path still yields a
/// distinct prefix (and `free_aside_path` appends a `.<n>` collision suffix if a
/// name is already taken).
pub(crate) fn materialized_aside_path(
    path: &WorkspacePath,
    entry: &ManifestEntry,
) -> WorkspacePath {
    aside_path_with_prefix(path, &entry_manifest_prefix(entry))
}

/// The same scheme for a caller that already has its own disambiguating prefix
/// (work-view accept keys asides by overlay, not by pulled content). One
/// function so every aside on disk is recognizable by one parser.
pub fn aside_path_with_prefix(path: &WorkspacePath, prefix: &AsidePrefix) -> WorkspacePath {
    WorkspacePath::new(format!(
        "{}{CONFLICT_ASIDE_MARKER}{}",
        path.as_str(),
        prefix.as_str()
    ))
}

/// The `n`th alternative for an aside base name whose slot is already taken.
pub(crate) fn collision_aside_path(base: &WorkspacePath, suffix: u32) -> WorkspacePath {
    WorkspacePath::new(format!("{}.{suffix}", base.as_str()))
}

/// Whether `path` is a conflict-aside this engine wrote.
pub fn is_conflict_aside(path: &str) -> bool {
    conflict_aside_origin(path).is_some()
}

/// The path an aside was written beside, or `None` when `path` is not an aside.
///
/// THE aside parser: the scanner, the resolver, and status all reach this one
/// function, and a name it rejects is an ordinary user file that no command may
/// list or destroy. Matching the marker alone is not enough — `notes.md` is a
/// plausible thing to call `notes.bowline-conflict.template`, and treating it as
/// an aside would offer to delete it or rename it over `notes`.
///
/// Split from the right so an aside of an aside (a conflict on a path that
/// already carried one) reports the aside it displaced, not the original source
/// file several levels up.
pub fn conflict_aside_origin(path: &str) -> Option<&str> {
    let (origin, suffix) = path.rsplit_once(CONFLICT_ASIDE_MARKER)?;
    // An aside is always written BESIDE a named entry, so the origin's final
    // component is a real name. An empty one (`a/` or the whole path) names the
    // containing directory, and `.`/`..` name a directory too: `...bowline-conflict.<hex>`
    // is a name a user's own file may wear and would otherwise parse to origin
    // `..`, which `bowline conflicts` lists as real and `--take-remote` then tries
    // to rename over — a rename POSIX refuses with a raw errno rather than a
    // coherent answer.
    if matches!(
        origin.rsplit('/').next().unwrap_or_default(),
        "" | "." | ".."
    ) {
        return None;
    }
    generated_aside_suffix(suffix).then_some(origin)
}

/// Whether everything after the marker is what the writer above produces: an
/// [`AsidePrefix`], optionally followed by a `.<n>` collision suffix.
///
/// The collision suffix is matched canonically (`.2`, not `.02` or `.+2`) so one
/// spelling of a generated name exists, and `.1` is rejected because the writer
/// starts at [`FIRST_COLLISION_SUFFIX`].
fn generated_aside_suffix(suffix: &str) -> bool {
    let (prefix, collision) = match suffix.split_once('.') {
        Some((prefix, collision)) => (prefix, Some(collision)),
        None => (suffix, None),
    };
    if prefix.len() != AsidePrefix::LEN
        || !prefix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return false;
    }
    match collision {
        None => true,
        Some(collision) => collision
            .parse::<u32>()
            .is_ok_and(|value| value >= FIRST_COLLISION_SUFFIX && collision == value.to_string()),
    }
}

pub(crate) fn entry_manifest_prefix(entry: &ManifestEntry) -> AsidePrefix {
    // Deterministic tag from the remote entry's content identity (no clock).
    let identity = match entry {
        ManifestEntry::File { content_id, .. } => content_id.as_str(),
        ManifestEntry::Directory { .. } => "dir",
        ManifestEntry::Symlink { target, .. } => target.as_str(),
    };
    AsidePrefix::derive(identity)
}

pub(crate) fn temp_name(path: &WorkspacePath, blob_key: &BlobKey) -> String {
    format!(
        "{}-{}",
        sanitize(path.as_str()),
        &blob_key.as_str()[..blob_key.as_str().len().min(16)]
    )
}

pub(crate) fn quarantine_name(path: &WorkspacePath) -> String {
    format!("quarantine/{}", quarantine_leaf(path))
}

pub(crate) fn quarantine_leaf(path: &WorkspacePath) -> String {
    // `sanitize` folds '/' to '_', so `a/b` and `a_b` would share a leaf and one
    // preimage would clobber the other. Append a hash of the ORIGINAL path so the
    // leaf is collision-free while staying deterministic across process restarts
    // (recovery re-derives the same name to find the preserved preimage).
    let digest = blake3::hash(path.as_str().as_bytes()).to_hex();
    format!("{}-{}", sanitize(path.as_str()), &digest[..16])
}

pub(crate) fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '/' { '_' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::manifest_engine::engine_test_support::test_context;
    use crate::sync::manifest_engine::manifest::FileMode;
    use bowline_core::ids::ContentId;
    use std::path::PathBuf;

    fn remote_file(content: &str) -> ManifestEntry {
        ManifestEntry::File {
            size: 1,
            mode: FileMode::new(0o644),
            content_id: ContentId::new(format!("c_{content}")),
            blob_key: BlobKey::new(format!("b_{content}")),
            key_epoch: crate::sync::manifest_engine::manifest::KeyEpoch::new(1),
        }
    }

    #[test]
    fn same_remote_conflict_names_identically_across_devices() {
        // Two devices materialize the SAME remote entry for the SAME path. The
        // aside name must be identical so sync converges them as one entry rather
        // than two duplicate conflict copies (asides sync). A missing (nonexistent)
        // root yields the base name from both contexts.
        let root = PathBuf::from("/bowline-nonexistent-root");
        let path = WorkspacePath::new("src/auth.ts");
        let entry = remote_file("deadbeefcafef00d");
        let mac = test_context(root.clone(), "mac-ts");
        let vivobook = test_context(root, "vivobook-ts");
        assert_eq!(
            free_aside_path(&mac, &path, &entry),
            free_aside_path(&vivobook, &path, &entry),
        );
    }

    /// The writer must never emit a name the reader refuses. The marker and hash
    /// lengthen the origin, so a path near the ceiling would otherwise produce an
    /// aside past it — preserved on disk but invisible to `bowline conflicts` and
    /// rejected by `bowline resolve`.
    #[test]
    fn an_aside_that_would_exceed_the_path_budget_is_refused_rather_than_generated() {
        let root = PathBuf::from("/bowline-nonexistent-root");
        let ctx = test_context(root, "mac-ts");
        let entry = remote_file("deadbeefcafef00d");

        let comfortable = WorkspacePath::new("src/auth.ts");
        let generated = free_aside_path(&ctx, &comfortable, &entry).expect("an ordinary path");
        assert!(
            publishable_workspace_path(generated.as_str(), MAX_WORKSPACE_PATH_LEN, false).is_ok(),
            "every generated aside must satisfy the rule the resolver enforces"
        );

        // Long enough that the origin is acceptable but the aside cannot be.
        let brink = WorkspacePath::new(format!(
            "src/{}.ts",
            "a".repeat(MAX_WORKSPACE_PATH_LEN as usize - 16)
        ));
        assert!(
            publishable_workspace_path(brink.as_str(), MAX_WORKSPACE_PATH_LEN, false).is_ok(),
            "the origin itself must be a legal path, or this proves nothing"
        );
        assert_eq!(free_aside_path(&ctx, &brink, &entry), None);
    }

    #[test]
    fn distinct_content_for_one_path_yields_distinct_names() {
        // Different remote content for the same path must not collide into one
        // aside name; the content-derived prefix keeps them apart.
        let path = WorkspacePath::new("src/auth.ts");
        assert_ne!(
            materialized_aside_path(&path, &remote_file("11111111aaaaaaaa")),
            materialized_aside_path(&path, &remote_file("22222222bbbbbbbb")),
        );
    }

    #[test]
    fn aside_name_is_device_independent_and_clock_free() {
        // The name carries no device id and no timestamp: it is a pure function
        // of the losing path + content.
        let path = WorkspacePath::new("notes.md");
        let name = materialized_aside_path(&path, &remote_file("0123456789abcdef"));
        assert_eq!(name.as_str(), "notes.md.bowline-conflict.16b9cc7c");
    }

    #[test]
    fn every_entry_kind_yields_a_name_that_parses_back() {
        // The prefix is hashed rather than sliced off the entry's identity, so
        // a symlink target ending in `/` or `.` and a directory's three-letter
        // tag both produce a name the scanner and the resolver recognize.
        for entry in [
            remote_file("0123456789abcdef"),
            ManifestEntry::Directory {
                mode: FileMode::new(0o040_755),
            },
            ManifestEntry::Symlink {
                target: "../packages/ui/".to_string(),
                mode: FileMode::new(0o120_777),
            },
        ] {
            let name = materialized_aside_path(&WorkspacePath::new("src/auth.ts"), &entry);
            assert_eq!(
                conflict_aside_origin(name.as_str()),
                Some("src/auth.ts"),
                "`{}` must parse back",
                name.as_str(),
            );
        }
    }

    #[test]
    fn an_aside_is_always_a_sibling_of_the_path_it_sits_beside() {
        // Load-bearing for `bowline resolve`: it opens ONE directory descriptor
        // and performs both the delete and the rename through it. A generated
        // name that landed in another directory would silently move the
        // mutation somewhere the descriptor does not cover.
        let name = materialized_aside_path(
            &WorkspacePath::new("acme/web/src/auth.ts"),
            &remote_file("0123456789abcdef"),
        );
        for candidate in [name.as_str(), collision_aside_path(&name, 2).as_str()] {
            let origin = conflict_aside_origin(candidate).expect("a generated name");
            assert_eq!(
                candidate.rsplit_once('/').map(|(parent, _)| parent),
                origin.rsplit_once('/').map(|(parent, _)| parent),
                "`{candidate}` must sit in the same directory as `{origin}`",
            );
        }
    }

    #[test]
    fn a_user_file_wearing_the_marker_is_not_an_aside() {
        // `notes.bowline-conflict.template` is a plausible thing to call a file.
        // Treating it as engine-authored would offer `--keep-local` (delete it)
        // and `--take-remote` (rename it over `notes`).
        for name in [
            "notes.bowline-conflict.template",
            "notes.bowline-conflict.DEADBEEF", // uppercase is not the alphabet
            "notes.bowline-conflict.deadbee",  // one character short
            "notes.bowline-conflict.deadbeef1", // one character long
            "notes.bowline-conflict.deadbeef.x", // junk where a collision goes
            "notes.bowline-conflict.deadbeef.", // an empty collision suffix
            "notes.bowline-conflict.deadbeef.02", // not the canonical spelling
            "notes.bowline-conflict.deadbeef.1", // below the first suffix written
            "notes.bowline-conflict.deadbeef.2.3", // only one suffix is ever added
            "notes.bowline-conflict.deadg00d", // `g` is not hex
            ".bowline-conflict.deadbeef",      // nothing to sit beside
            "src/.bowline-conflict.deadbeef",  // would name the directory itself
            "...bowline-conflict.deadbeef",    // origin `..` — the parent directory
            "a/...bowline-conflict.deadbeef",  // origin `a/..` — the same, nested
            "..bowline-conflict.deadbeef",     // origin `.` — the directory itself
            "a/..bowline-conflict.deadbeef",   // origin `a/.` — the same, nested
        ] {
            assert_eq!(conflict_aside_origin(name), None, "`{name}` is a user file");
            assert!(!is_conflict_aside(name), "`{name}` is a user file");
        }
    }

    #[test]
    fn a_generated_name_and_its_collision_alternatives_are_asides() {
        assert_eq!(
            conflict_aside_origin("notes.md.bowline-conflict.deadbeef"),
            Some("notes.md"),
        );
        for suffix in [FIRST_COLLISION_SUFFIX, 3, 4_294_967_295] {
            let name = format!("notes.md.bowline-conflict.deadbeef.{suffix}");
            assert_eq!(conflict_aside_origin(&name), Some("notes.md"), "{name}");
        }
    }

    #[test]
    fn aside_name_does_not_end_in_the_source_extension() {
        // The whole point of the suffix shape: `*.ts` no longer matches, so
        // tsconfig includes, linters, and build globs skip the aside.
        let name = materialized_aside_path(
            &WorkspacePath::new("src/auth.ts"),
            &remote_file("0123456789abcdef"),
        );
        assert!(!name.as_str().ends_with(".ts"));
        assert!(!name.as_str().contains(' '));
        assert!(!name.as_str().contains('('));
    }

    #[test]
    fn an_aside_name_parses_back_to_the_path_it_sits_beside() {
        let origin = WorkspacePath::new("src/auth.ts");
        let entry = remote_file("0123456789abcdef");
        let aside = materialized_aside_path(&origin, &entry);
        assert_eq!(conflict_aside_origin(aside.as_str()), Some("src/auth.ts"));
        assert_eq!(
            conflict_aside_origin(collision_aside_path(&aside, 2).as_str()),
            Some("src/auth.ts"),
        );
        assert_eq!(conflict_aside_origin("src/auth.ts"), None);
    }

    #[test]
    fn an_aside_of_an_aside_reports_the_aside_it_displaced() {
        let first = materialized_aside_path(
            &WorkspacePath::new("notes.md"),
            &remote_file("0123456789abcdef"),
        );
        let second = materialized_aside_path(&first, &remote_file("fedcba9876543210"));
        assert_eq!(conflict_aside_origin(second.as_str()), Some(first.as_str()));
    }

    #[test]
    fn git_internal_paths_never_receive_an_aside() {
        // A second file inside `.git/**` corrupts the repository rather than
        // preserving anything; those divergences stay keep-local.
        let ctx = test_context(PathBuf::from("/bowline-nonexistent-root"), "mac-ts");
        let entry = remote_file("0123456789abcdef");
        for path in [
            ".git",
            ".git/index",
            ".git/HEAD",
            ".git/objects/ab/cdef",
            "acme/web/.git/refs/heads/main",
            "acme/web/.git",
        ] {
            let path = WorkspacePath::new(path);
            assert!(
                !aside_is_permitted(&path),
                "`{}` must not carry an aside",
                path.as_str()
            );
            assert_eq!(free_aside_path(&ctx, &path, &entry), None);
        }
        assert!(aside_is_permitted(&WorkspacePath::new(
            "acme/web/.gitignore"
        )));
        assert!(aside_is_permitted(&WorkspacePath::new("acme/web/src/a.ts")));
    }

    /// Both refusals belong to the aside concept, so a writer that never touches
    /// the filesystem (work-view accept composes a merged manifest) enforces them
    /// through the same gate the pull path does.
    #[test]
    fn the_shared_gate_refuses_git_internals_and_names_past_the_path_budget() {
        let prefix = AsidePrefix::derive("overlay-key");
        for path in [".git/index", ".git/refs/heads/main", "acme/web/.git/HEAD"] {
            assert_eq!(
                permitted_aside_path(&WorkspacePath::new(path), &prefix),
                None,
                "`{path}` must not carry an aside",
            );
        }
        let brink = WorkspacePath::new(format!(
            "src/{}.ts",
            "a".repeat(MAX_WORKSPACE_PATH_LEN as usize - 16)
        ));
        assert!(
            publishable_workspace_path(brink.as_str(), MAX_WORKSPACE_PATH_LEN, false).is_ok(),
            "the origin itself must be a legal path, or this proves nothing"
        );
        assert_eq!(permitted_aside_path(&brink, &prefix), None);

        let ordinary = WorkspacePath::new("src/auth.ts");
        let granted = permitted_aside_path(&ordinary, &prefix).expect("an ordinary path");
        assert_eq!(
            conflict_aside_origin(granted.as_str()),
            Some("src/auth.ts"),
            "a granted name is THE aside grammar, so every reader recognizes it",
        );
    }

    #[test]
    fn quarantine_leaf_disambiguates_paths_that_fold_to_one_sanitized_name() {
        // `a/b` and `a_b` both sanitize to `a_b`; the hash suffix keeps their
        // quarantine slots distinct so one preimage cannot clobber the other.
        let slashed = quarantine_leaf(&WorkspacePath::new("a/b"));
        let underscored = quarantine_leaf(&WorkspacePath::new("a_b"));
        assert_ne!(slashed, underscored);
        // Deterministic across calls (recovery re-derives the same name).
        assert_eq!(slashed, quarantine_leaf(&WorkspacePath::new("a/b")));
    }
}
