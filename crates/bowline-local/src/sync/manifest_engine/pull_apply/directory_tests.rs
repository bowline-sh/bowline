//! How a pull treats a path's KIND: directories as containers, and the
//! file↔directory↔symlink replacements that really are conflicts.
//!
//! Split from the merge-matrix suite in the sibling `tests` module at the seam
//! between *which side wins* and *what shape the winner is*. One claim binds the
//! file together: a directory is a container, not content. Its identity is that
//! it exists, its children are reconciled one row at a time, and two directories
//! at one path therefore never disagree — not in the merge matrix, and not at the
//! mutation boundary where the preimage the plan snapshotted no longer holds.
//! Only a genuine kind clash is a conflict, and every one of those still asides.

use super::apply::Applied;
use super::intents::PreimagePayload;
use super::materialize::{DeleteOutcome, checked_delete};
use super::tests::{apply_install_expecting_absent, apply_single_op, aside_names, chmod, wp};
use super::{FsOp, FsOpKind, PullDeps, PullScope};
use crate::sync::manifest_engine::engine_test_support::TestEngine;
use crate::sync::manifest_engine::manifest::{EntryKind, FileMode, ManifestEntry};

// ---- a delete must never destroy local-only content ------------------------

/// A directory kind manifest entry (push records dirs, but a remote peer can also
/// publish one directly).
fn dir_entry() -> ManifestEntry {
    ManifestEntry::Directory {
        mode: FileMode::new(0o755),
    }
}

fn is_dir(engine: &TestEngine, rel: &str) -> bool {
    std::fs::symlink_metadata(engine.root().join(rel))
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

#[test]
fn directory_delete_preserves_untracked_local_child() {
    // Remote deletes a tracked directory tree; the user has an untracked file
    // inside it. A recursive `remove_dir_all` would destroy that racing local work
    // (branch invariant: never silently destroy it). The tracked child goes away;
    // the directory and the untracked child survive and re-push.
    let mut engine = TestEngine::new("dir-delete-untracked-child");
    engine.write("dir/tracked.txt", b"tracked");
    // Present at push time so the directory's recorded fingerprint includes it and
    // the directory still classifies Unchanged (a Delete op is emitted for it).
    engine.write("dir/untracked.txt", b"local only work");
    engine.push(&["dir", "dir/tracked.txt"]);
    assert!(engine.files().contains_key(&wp("dir")));

    engine.publish(&[]); // remote head removes the whole tracked tree
    let outcome = engine.pull();

    assert!(
        engine.exists("dir/untracked.txt"),
        "untracked child survives"
    );
    assert_eq!(engine.read("dir/untracked.txt"), b"local only work");
    assert!(
        is_dir(&engine, "dir"),
        "the directory survives (kept local)"
    );
    assert!(
        !engine.exists("dir/tracked.txt"),
        "the tracked child is deleted"
    );
    assert!(outcome.deleted.contains(&wp("dir/tracked.txt")));
    assert!(
        outcome.push_again.contains(&wp("dir")),
        "the kept-local directory re-pushes"
    );
}

#[test]
fn directory_delete_of_fully_tracked_tree_removes_everything() {
    // The plain case still works: with no local-only content, the whole tree is
    // deleted bottom-up (child unlinked, then the now-empty directory).
    let mut engine = TestEngine::new("dir-delete-fully-tracked");
    engine.write("dir/child.txt", b"tracked");
    engine.push(&["dir", "dir/child.txt"]);
    assert!(engine.exists("dir/child.txt"));

    engine.publish(&[]);
    let outcome = engine.pull();

    assert!(!engine.exists("dir/child.txt"));
    assert!(!engine.exists("dir"));
    assert!(outcome.deleted.contains(&wp("dir")));
    assert!(outcome.deleted.contains(&wp("dir/child.txt")));
}

#[test]
fn checked_delete_keeps_a_nonempty_directory_local() {
    // The delete executor (shared by apply and crash-recovery replay) removes a
    // directory only when empty; local-only content keeps it local rather than
    // destroying it.
    let engine = TestEngine::new("checked-delete-nonempty");
    engine.write("dir/keep.txt", b"must survive");
    let outcome = checked_delete(&engine.ctx, &wp("dir")).expect("checked delete");
    assert!(matches!(outcome, DeleteOutcome::KeptLocal));
    assert!(engine.exists("dir"));
    assert_eq!(engine.read("dir/keep.txt"), b"must survive");

    // An empty directory deletes cleanly.
    std::fs::remove_file(engine.root().join("dir/keep.txt")).expect("rm child");
    let outcome = checked_delete(&engine.ctx, &wp("dir")).expect("checked delete empty");
    assert!(matches!(outcome, DeleteOutcome::Deleted));
    assert!(!engine.exists("dir"));
}

#[test]
fn directory_chmod_between_plan_and_apply_deflects_and_local_mode_survives() {
    // Same guard for a directory: a raced chmod diverges the preimage, so a remote
    // delete keeps local rather than discarding the permission change.
    let mut engine = TestEngine::new("mode-race-dir");
    std::fs::create_dir(engine.root().join("d")).expect("mkdir");
    chmod(&engine, "d", 0o755);
    let observed = engine.observe("d").expect("present");
    let expected = PreimagePayload::from_observed(&observed, None);

    chmod(&engine, "d", 0o700);

    let op = FsOp {
        path: wp("d"),
        kind: FsOpKind::Delete,
        expected,
    };
    let applied = apply_single_op(&mut engine, op, "m_mode_dir");

    assert!(
        matches!(applied, Applied::KeptLocal(_)),
        "the raced directory chmod deflects the remote delete to keep-local"
    );
    assert!(is_dir(&engine, "d"), "the directory survives");
    assert_eq!(
        engine.mode_bits("d"),
        0o700,
        "local directory mode survives"
    );
}

// ---- entry-kind replacement (file↔directory↔symlink) -----------------------

#[test]
fn kind_change_classifies_as_install_not_aside() {
    // The premise: a remote entry of a different kind over an unchanged local entry
    // is a plain Install (not conflict-aside), so the executor must materialize it.
    let mut engine = TestEngine::new("kind-change-classify");
    engine.write("f", b"original");
    engine.push(&["f"]);
    engine.publish(&[("f", dir_entry())]);

    let head = engine.remote.current_ref().expect("head");
    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,

        scope: PullScope::WholeAncestor,
    };
    let plan = super::decide_head(&mut engine.store, &deps, &head).expect("decide head");
    let op = plan
        .fs_ops
        .iter()
        .find(|op| op.path.as_str() == "f")
        .expect("an fs op for f");
    assert!(
        matches!(op.kind, FsOpKind::Install(_)),
        "a kind change classifies as install"
    );
}

#[test]
fn kind_change_file_to_directory_converges() {
    let mut engine = TestEngine::new("kind-file-to-dir");
    engine.write("f", b"original file");
    engine.push(&["f"]);

    let child = engine.remote_file(b"child bytes");
    engine.publish(&[("f", dir_entry()), ("f/c.txt", child)]);
    let outcome = engine.pull();

    assert!(is_dir(&engine, "f"), "f became a directory");
    assert_eq!(engine.read("f/c.txt"), b"child bytes");
    assert!(outcome.conflict_asides.is_empty());
}

#[test]
fn kind_change_directory_to_file_converges() {
    let mut engine = TestEngine::new("kind-dir-to-file");
    engine.write("d/c.txt", b"child");
    engine.push(&["d", "d/c.txt"]);

    let file_entry = engine.remote_file(b"now a file");
    engine.publish(&[("d", file_entry)]); // remote drops d/c.txt, makes d a file
    let outcome = engine.pull();

    assert!(!is_dir(&engine, "d"), "d became a file");
    assert_eq!(engine.read("d"), b"now a file");
    assert!(!engine.exists("d/c.txt"));
    assert!(outcome.conflict_asides.is_empty());
}

#[test]
fn kind_change_symlink_to_directory_converges() {
    use std::os::unix::fs::symlink;
    let mut engine = TestEngine::new("kind-symlink-to-dir");
    symlink("elsewhere", engine.root().join("l")).expect("symlink");
    engine.push(&["l"]);

    let child = engine.remote_file(b"deep bytes");
    engine.publish(&[("l", dir_entry()), ("l/c.txt", child)]);
    let outcome = engine.pull();

    assert!(is_dir(&engine, "l"), "l became a directory");
    assert_eq!(engine.read("l/c.txt"), b"deep bytes");
    assert!(outcome.conflict_asides.is_empty());
}

#[test]
fn kind_change_file_to_symlink_converges() {
    let mut engine = TestEngine::new("kind-file-to-symlink");
    engine.write("s", b"file bytes");
    engine.push(&["s"]);

    let link = ManifestEntry::Symlink {
        mode: FileMode::new(0o777),
        target: "target/path".to_string(),
    };
    engine.publish(&[("s", link)]);
    let outcome = engine.pull();

    let meta = std::fs::symlink_metadata(engine.root().join("s")).expect("metadata");
    assert!(meta.file_type().is_symlink(), "s became a symlink");
    assert_eq!(
        std::fs::read_link(engine.root().join("s")).expect("readlink"),
        std::path::Path::new("target/path")
    );
    assert!(outcome.conflict_asides.is_empty());
}

#[test]
fn kind_change_directory_to_file_preserves_untracked_local_child() {
    // A remote replaces a directory with a file while the directory holds an
    // untracked local file. The replacement must NOT destroy it: keep the
    // directory local and aside the remote file.
    let mut engine = TestEngine::new("kind-dir-to-file-untracked");
    engine.write("d/tracked.txt", b"tracked");
    engine.write("d/keep.txt", b"local work"); // untracked, present at push
    engine.push(&["d", "d/tracked.txt"]);

    let file_entry = engine.remote_file(b"remote file bytes");
    engine.publish(&[("d", file_entry)]); // remote: d is a file, tracked.txt gone
    let outcome = engine.pull();

    assert!(is_dir(&engine, "d"), "d stays a directory (kept local)");
    assert!(engine.exists("d/keep.txt"), "untracked child survives");
    assert_eq!(engine.read("d/keep.txt"), b"local work");
    assert!(!engine.exists("d/tracked.txt"));
    let aside = outcome
        .conflict_asides
        .iter()
        .next()
        .expect("the remote file is asided");
    assert_eq!(engine.read(aside.as_str()), b"remote file bytes");
}

// ---- a directory is a container, not content --------------------------------

#[test]
fn two_devices_creating_the_same_directory_adopt_one_directory() {
    // The merge matrix half of the invariant: a local directory the ancestor has
    // never seen, meeting the same directory in the remote manifest, is adopted
    // without an fs op. Both sides carry no content id, so there is nothing to
    // disagree about.
    let mut engine = TestEngine::new("dir-dir-adopt");
    engine.write("d/local.txt", b"local");
    let remote = engine.remote_file(b"remote");
    engine.publish(&[("d", dir_entry()), ("d/remote.txt", remote)]);

    let outcome = engine.pull();
    assert!(
        outcome.conflict_asides.is_empty(),
        "two devices creating the same directory is a merge, not a conflict"
    );
    assert!(is_dir(&engine, "d"));
    assert_eq!(engine.read("d/local.txt"), b"local", "local child survives");
    assert_eq!(engine.read("d/remote.txt"), b"remote", "remote child lands");
    assert!(
        engine.files().contains_key(&wp("d")),
        "the directory is tracked"
    );
}

#[test]
fn a_directory_that_appears_between_plan_and_apply_is_the_same_directory() {
    // The apply-boundary half, and the one that actually fires in production: the
    // plan saw the path absent, another device's `mkdir` (or this engine's own
    // parent-chain creation) lands first, and the preimage no longer holds. That
    // is not a divergence — an aside here is an empty directory beside the real
    // one, and it never resolves.
    let mut engine = TestEngine::new("dir-race-apply");
    std::fs::create_dir(engine.root().join("d")).expect("mkdir");
    chmod(&engine, "d", 0o700);

    let applied = apply_install_expecting_absent(&mut engine, "d", dir_entry(), "mk");

    match applied {
        Applied::Upsert(path, record) => {
            assert_eq!(path, wp("d"));
            assert_eq!(record.kind, EntryKind::Directory);
        }
        _ => panic!("a directory meeting a directory adopts, it never asides"),
    }
    assert!(is_dir(&engine, "d"), "the one directory survives");
    assert!(
        aside_names(&engine.root()).is_empty(),
        "no empty conflict aside is materialized"
    );
    assert_eq!(
        engine.mode_bits("d"),
        0o755,
        "the published mode is stamped, exactly as an install into an empty slot"
    );
}

#[test]
fn a_git_repo_materializes_without_conflicting_with_its_own_directories() {
    // The production shape, and no race at all: `.git/objects/**` ranks ahead of
    // every other path in apply order (a ref must never point at a missing
    // object), so the parent chain creates the project directory, its `.git`, and
    // `.git/objects` before any of those three entries reaches its own op. Each
    // one then found itself already present, failed its absent-preimage check and
    // deflected — the project directory into an empty conflict aside, the
    // git-internal pair into keep-local with no ancestor row at all. Every fresh
    // materialization of a repo did this, and the workspace could never settle.
    let mut engine = TestEngine::new("git-dir-self-conflict");
    let loose = engine.remote_file(b"loose object bytes");
    let head = engine.remote_file(b"ref: refs/heads/main\n");
    let readme = engine.remote_file(b"# project\n");
    engine.publish(&[
        ("proj", dir_entry()),
        ("proj/README.md", readme),
        ("proj/.git", dir_entry()),
        ("proj/.git/HEAD", head),
        ("proj/.git/objects", dir_entry()),
        ("proj/.git/objects/ab", dir_entry()),
        ("proj/.git/objects/ab/cdef", loose),
    ]);

    let outcome = engine.pull();

    assert!(
        outcome.conflict_asides.is_empty(),
        "a repo must not conflict with the directories its own install created: {:?}",
        outcome.conflict_asides
    );
    assert!(
        aside_names(&engine.root()).is_empty(),
        "no aside directories on disk"
    );
    assert_eq!(
        engine.read("proj/.git/objects/ab/cdef"),
        b"loose object bytes"
    );
    assert_eq!(engine.read("proj/README.md"), b"# project\n");
    for dir in [
        "proj",
        "proj/.git",
        "proj/.git/objects",
        "proj/.git/objects/ab",
    ] {
        assert!(is_dir(&engine, dir), "{dir} is a directory");
        assert!(
            engine.files().contains_key(&wp(dir)),
            "{dir} reaches the ancestor, so the workspace can settle"
        );
        assert_eq!(
            engine.mode_bits(dir),
            0o755,
            "{dir} carries the published mode, not the umask of a parent-chain mkdir"
        );
    }
}

#[test]
fn a_file_where_the_remote_publishes_a_directory_still_conflicts() {
    // The kind clash the narrowing must not swallow: local holds a file, the
    // remote publishes a directory at the same path. Local bytes are kept and the
    // remote is preserved beside them.
    let mut engine = TestEngine::new("dir-vs-file-clash");
    engine.write("d", b"local file bytes");

    let applied = apply_install_expecting_absent(&mut engine, "d", dir_entry(), "mk");

    match applied {
        Applied::Aside(aside) => assert!(is_dir(&engine, aside.as_str())),
        _ => panic!("a directory meeting a file is a real conflict"),
    }
    assert_eq!(engine.read("d"), b"local file bytes", "local bytes survive");
}

#[test]
fn a_directory_where_the_remote_publishes_a_file_still_conflicts() {
    // The mirror clash: the entry is a file, so the directory rule must not apply.
    let mut engine = TestEngine::new("file-vs-dir-clash");
    std::fs::create_dir(engine.root().join("d")).expect("mkdir");
    engine.write("d/local.txt", b"local only work");
    let entry = engine.remote_file(b"remote file bytes");

    let applied = apply_install_expecting_absent(&mut engine, "d", entry, "mk");

    match applied {
        Applied::Aside(aside) => {
            assert_eq!(engine.read(aside.as_str()), b"remote file bytes");
        }
        _ => panic!("a file meeting a directory is a real conflict"),
    }
    assert!(is_dir(&engine, "d"), "the local directory survives");
    assert_eq!(engine.read("d/local.txt"), b"local only work");
}
