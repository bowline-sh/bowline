//! Deterministic naming for conflict-asides, staging temps, and
//! quarantine entries: content-derived, wall-clock-free, collision-safe.

use crate::sync::manifest_engine::manifest::{BlobKey, ManifestEntry, WorkspacePath};
use crate::sync::manifest_engine::push::EngineContext;

pub(crate) fn free_aside_path(
    ctx: &EngineContext,
    path: &WorkspacePath,
    entry: &ManifestEntry,
) -> WorkspacePath {
    let base = materialized_aside_path(path, entry);
    if !ctx.workspace_root.join(base.as_str()).exists() {
        return base;
    }
    // Deterministic collision suffix; no wall-clock.
    for suffix in 1..u32::MAX {
        let candidate = WorkspacePath::new(format!("{} ({suffix})", base.as_str()));
        if !ctx.workspace_root.join(candidate.as_str()).exists() {
            return candidate;
        }
    }
    base
}

/// THE conflict-aside naming scheme (single source of truth):
///
///   `<workspace-path> (conflict from <content-prefix>)`
///
/// The name is derived only from the losing (remote) entry: its path and a
/// content-derived prefix (`entry_manifest_prefix`). It carries NO device id and
/// NO wall-clock. This is load-bearing: asides themselves sync, so two devices
/// materializing the SAME remote conflict for the SAME path must produce the
/// SAME name, or sync would treat them as two entries and spawn endless
/// duplicate conflict copies. Different content for the same path still yields a
/// distinct prefix (and `free_aside_path` appends a ` (N)` collision suffix if a
/// name is already taken).
///
/// The cutover verifier parses this scheme back to the original path
/// (`scripts/cutover/verify-inventory.mjs`, `parseAsideOriginalPath`); keep the
/// two in lockstep if the format ever changes.
pub(crate) fn materialized_aside_path(
    path: &WorkspacePath,
    entry: &ManifestEntry,
) -> WorkspacePath {
    let prefix = entry_manifest_prefix(entry);
    WorkspacePath::new(format!("{} (conflict from {})", path.as_str(), prefix))
}

pub(crate) fn entry_manifest_prefix(entry: &ManifestEntry) -> String {
    // Deterministic tag from the remote entry's content identity (no clock).
    let identity = match entry {
        ManifestEntry::File { content_id, .. } => content_id.as_str(),
        ManifestEntry::Directory { .. } => "dir",
        ManifestEntry::Symlink { target, .. } => target.as_str(),
    };
    identity
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
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
        assert!(name.as_str().starts_with("notes.md (conflict from "));
        assert!(name.as_str().ends_with(')'));
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
