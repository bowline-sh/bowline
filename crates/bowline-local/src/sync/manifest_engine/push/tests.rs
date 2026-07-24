//! Push contract tests (Plan 109 Step 4). The ancestor is sacred: it changes
//! only on a proven CAS advance; a lost or ambiguous CAS never corrupts it.

use std::os::unix::fs::symlink;
use std::path::PathBuf;

use super::{EntryKind, PushDeps, PushError, PushOutcome, push_verifying_dirty_files};
use crate::sync::manifest_engine::engine_test_support::{CasMode, Event, KEY_BYTES, TestEngine};
use crate::sync::manifest_engine::fs_guard::{FileRead, read_file_bounded};
use crate::sync::manifest_engine::manifest::{KeyEpoch, WorkspaceCrypto, WorkspacePath};
use crate::sync::manifest_engine::push::EngineConfig;

/// A secret file OUTSIDE the workspace root that must never be read/sealed into
/// synced state through a symlink. Returned so tests can point links at it.
fn external_secret(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("bowline-external-secret-{name}"));
    std::fs::write(&path, b"TOP SECRET CREDENTIALS OUTSIDE THE WORKSPACE").expect("seed secret");
    path
}

#[test]
fn unchanged_files_are_never_opened() {
    let mut engine = TestEngine::new("push-unchanged");
    engine.write("a.txt", b"alpha");
    engine.write("b.txt", b"beta");
    assert!(matches!(
        engine.push(&["a.txt", "b.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let baseline = engine.remote.blob_put_count();

    // Only b changes. a is fingerprint-clean, so it is never read or uploaded.
    engine.write("b.txt", b"beta-two");
    assert!(matches!(
        engine.push(&["a.txt", "b.txt"]),
        PushOutcome::Advanced { .. }
    ));
    assert_eq!(
        engine.remote.blob_put_count() - baseline,
        1,
        "only the changed file is opened and uploaded"
    );

    // A re-push with nothing changed does no work at all (invariant C1/C2).
    assert!(matches!(
        engine.push(&["a.txt", "b.txt"]),
        PushOutcome::NoChange { skipped } if skipped.is_empty()
    ));
}

#[test]
fn explicit_content_verification_detects_a_same_size_rewrite() {
    let mut engine = TestEngine::new("push-verify-same-size");
    engine.write("same.txt", b"before");
    assert!(matches!(
        engine.push(&["same.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let before = engine
        .files()
        .get(&WorkspacePath::new("same.txt"))
        .expect("file is tracked")
        .content_id
        .clone();

    engine.write("same.txt", b"after!");
    let dirty = engine.dirty(&["same.txt"]);
    let deps = PushDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    assert!(matches!(
        push_verifying_dirty_files(&mut engine.store, &deps, &dirty)
            .expect("verified push succeeds"),
        PushOutcome::Advanced { .. }
    ));
    assert_ne!(
        engine
            .files()
            .get(&WorkspacePath::new("same.txt"))
            .expect("file remains tracked")
            .content_id
            .as_ref(),
        before.as_ref(),
    );
}

#[test]
fn content_equivalent_rewrite_refreshes_the_local_fingerprint_once() {
    let mut engine = TestEngine::new("push-refresh-equivalent");
    engine.write("same.txt", b"unchanged");
    assert!(matches!(
        engine.push(&["same.txt"]),
        PushOutcome::Advanced { .. }
    ));

    engine.write("same.txt", b"unchanged");
    let before_refresh = engine.counters().content_hashes;
    let dirty = engine.dirty(&["same.txt"]);
    let deps = PushDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    assert!(matches!(
        push_verifying_dirty_files(&mut engine.store, &deps, &dirty)
            .expect("verified refresh succeeds"),
        PushOutcome::NoChange { skipped } if skipped.is_empty()
    ));
    let after_refresh = engine.counters().content_hashes;
    assert_eq!(after_refresh, before_refresh + 1);

    assert!(matches!(
        engine.push(&["same.txt"]),
        PushOutcome::NoChange { skipped } if skipped.is_empty()
    ));
    assert_eq!(
        engine.counters().content_hashes,
        after_refresh,
        "the refreshed fingerprint makes the next scan stat-clean",
    );
}

#[test]
fn verified_unchanged_bytes_are_resealed_for_a_new_key_epoch() {
    let mut engine = TestEngine::new("push-verify-new-epoch");
    engine.write("same.txt", b"unchanged");
    assert!(matches!(
        engine.push(&["same.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let before = engine
        .files()
        .get(&WorkspacePath::new("same.txt"))
        .expect("file is tracked")
        .clone();

    engine.ctx.crypto = WorkspaceCrypto::new("ws_code", KEY_BYTES, KeyEpoch::new(2));
    let dirty = engine.dirty(&["same.txt"]);
    let deps = PushDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    assert!(matches!(
        push_verifying_dirty_files(&mut engine.store, &deps, &dirty)
            .expect("epoch migration push succeeds"),
        PushOutcome::Advanced { .. }
    ));

    let after = engine
        .files()
        .get(&WorkspacePath::new("same.txt"))
        .expect("file remains tracked")
        .clone();
    assert_eq!(after.key_epoch, Some(KeyEpoch::new(2)));
    assert_ne!(after.blob_key, before.blob_key);
}

#[test]
fn upload_orders_blob_before_manifest_without_redundant_readback() {
    let mut engine = TestEngine::new("push-order");
    engine.write("x.txt", b"payload");
    assert!(matches!(
        engine.push(&["x.txt"]),
        PushOutcome::Advanced { .. }
    ));

    let events = engine.remote.events();
    let blob_put = events
        .iter()
        .position(|event| matches!(event, Event::PutBlob(_)))
        .expect("blob uploaded");
    let manifest_put = events
        .iter()
        .position(|event| matches!(event, Event::PutManifest(_)))
        .expect("manifest uploaded");
    assert!(
        blob_put < manifest_put,
        "blob is committed before the manifest references it"
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Event::GetBlob(_))),
        "the checksum-backed commit receipt makes post-upload GET redundant"
    );
}

#[test]
fn cas_loss_preserves_ancestor_and_local_edit() {
    let mut engine = TestEngine::new("push-cas-loss");
    engine.write("x.txt", b"one");
    engine.push(&["x.txt"]);
    let baseline = engine.files();

    // A peer advances the ref, so our next CAS is stale.
    let peer = engine.remote_file(b"peer bytes");
    engine.publish(&[("x.txt", peer)]);

    engine.write("x.txt", b"one-edited-locally");
    let outcome = engine.push(&["x.txt"]);
    assert!(matches!(outcome, PushOutcome::RefLost { .. }));

    // The ancestor and the user's local edit are both untouched.
    assert_eq!(engine.files(), baseline, "ancestor unchanged on CAS loss");
    assert_eq!(engine.read("x.txt"), b"one-edited-locally");
}

#[test]
fn crash_after_manifest_upload_before_cas_keeps_path_dirty() {
    let mut engine = TestEngine::new("push-crash");
    engine.remote.set_cas_mode(CasMode::FailBeforeSwap);
    engine.write("x.txt", b"one");

    let result = engine.try_push(&["x.txt"]);
    assert!(
        result.is_err(),
        "CAS transport failure surfaces as an error"
    );

    // Nothing committed: no ancestor row, no advanced ref. The path stays dirty
    // for the driver to re-push against the unchanged base.
    assert!(engine.files().is_empty());
    assert!(engine.remote.current_ref().is_none());
    // The manifest WAS uploaded before the CAS attempt.
    assert!(
        engine
            .remote
            .events()
            .iter()
            .any(|event| matches!(event, Event::PutManifest(_)))
    );
}

#[test]
fn cas_succeeded_ack_lost_adopts_current_head() {
    let mut engine = TestEngine::new("push-ambiguous");
    engine.remote.set_cas_mode(CasMode::AmbiguousAfterSwap);
    engine.write("x.txt", b"one");

    // The swap committed but the ack was dropped; push reads the ref, sees its
    // own candidate is the head, and adopts it.
    let outcome = engine.push(&["x.txt"]);
    assert!(matches!(outcome, PushOutcome::Advanced { .. }));
    assert!(engine.files().contains_key(&WorkspacePath::new("x.txt")));
    assert!(engine.remote.current_ref().is_some());
}

#[test]
fn large_file_memory_stays_bounded() {
    // Threshold below file size routes the sealed blob through a 0600 spool and
    // a streamed upload, so no second in-memory copy is buffered for the send.
    let config = EngineConfig {
        large_file_threshold: 4,
        max_seal_bytes: 4096,
    };
    let mut engine = TestEngine::with_config("push-large", config);
    engine.write("big.bin", &vec![7u8; 512]);
    assert!(matches!(
        engine.push(&["big.bin"]),
        PushOutcome::Advanced { .. }
    ));
    assert_eq!(
        engine.remote.reader_put_count(),
        1,
        "large file is streamed from the spool, not buffered"
    );

    // Above the seal ceiling the envelope cannot stream-seal: STOP, never buffer.
    let ceiling = EngineConfig {
        large_file_threshold: 4,
        max_seal_bytes: 64,
    };
    let mut engine = TestEngine::with_config("push-ceiling", ceiling);
    engine.write("huge.bin", &vec![1u8; 256]);
    let error = engine
        .try_push(&["huge.bin"])
        .expect_err("ceiling stops push");
    assert!(matches!(error, PushError::StreamSealUnsupported { .. }));
}

// ---- echo suppression for directory/symlink watcher events ------------------

#[test]
fn watcher_event_on_unchanged_directory_seals_no_manifest() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let mut engine = TestEngine::new("push-dir-echo");
    std::fs::create_dir(engine.root().join("d")).expect("mkdir");
    std::fs::set_permissions(engine.root().join("d"), Permissions::from_mode(0o755))
        .expect("chmod dir");
    assert!(matches!(engine.push(&["d"]), PushOutcome::Advanced { .. }));

    let ref_before = engine.remote.current_ref();
    let manifests_before = engine.counters().manifest_uploads;
    let cas_before = engine.counters().cas_attempts;

    // A watcher routinely re-reports a parent dir while a child is edited. The echo
    // must publish no manifest and advance no ref (invariant C1/C2).
    match engine.push(&["d"]) {
        PushOutcome::NoChange { skipped } => assert!(skipped.is_empty()),
        other => panic!("expected NoChange for an unchanged directory, got {other:?}"),
    }
    assert_eq!(
        engine.remote.current_ref(),
        ref_before,
        "ref did not advance"
    );
    assert_eq!(
        engine.counters().manifest_uploads,
        manifests_before,
        "no manifest sealed for an unchanged directory"
    );
    assert_eq!(
        engine.counters().cas_attempts,
        cas_before,
        "no CAS attempted for an unchanged directory"
    );
}

#[test]
fn watcher_event_on_unchanged_symlink_seals_no_manifest() {
    let mut engine = TestEngine::new("push-symlink-echo");
    let secret = external_secret("symlink-echo");
    symlink(&secret, engine.root().join("link")).expect("create symlink");
    assert!(matches!(
        engine.push(&["link"]),
        PushOutcome::Advanced { .. }
    ));

    let ref_before = engine.remote.current_ref();
    let manifests_before = engine.counters().manifest_uploads;

    // Applying a remote symlink generates a local event; the echo must not re-seal.
    match engine.push(&["link"]) {
        PushOutcome::NoChange { skipped } => assert!(skipped.is_empty()),
        other => panic!("expected NoChange for an unchanged symlink, got {other:?}"),
    }
    assert_eq!(
        engine.remote.current_ref(),
        ref_before,
        "ref did not advance"
    );
    assert_eq!(
        engine.counters().manifest_uploads,
        manifests_before,
        "no manifest sealed for an unchanged symlink"
    );
}

#[test]
fn chmod_directory_and_retargeted_symlink_still_push() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let mut engine = TestEngine::new("push-dir-symlink-change");
    std::fs::create_dir(engine.root().join("d")).expect("mkdir");
    std::fs::set_permissions(engine.root().join("d"), Permissions::from_mode(0o755))
        .expect("chmod dir");
    let secret = external_secret("retarget-a");
    symlink(&secret, engine.root().join("link")).expect("create symlink");
    assert!(matches!(
        engine.push(&["d", "link"]),
        PushOutcome::Advanced { .. }
    ));

    // A genuine chmod on the directory is a real change and must push.
    std::fs::set_permissions(engine.root().join("d"), Permissions::from_mode(0o700))
        .expect("re-chmod dir");
    assert!(
        matches!(engine.push(&["d"]), PushOutcome::Advanced { .. }),
        "a chmod'ed directory still pushes"
    );

    // Retargeting the symlink is a real change and must push.
    let other = external_secret("retarget-b");
    engine.remove("link");
    symlink(&other, engine.root().join("link")).expect("retarget symlink");
    assert!(
        matches!(engine.push(&["link"]), PushOutcome::Advanced { .. }),
        "a retargeted symlink still pushes"
    );
    assert_eq!(
        engine.files()[&WorkspacePath::new("link")]
            .symlink_target
            .as_deref(),
        other.to_str(),
        "the new symlink target is recorded"
    );
}

// ---- no-follow content-read hardening (review P1) ---------------------------

#[test]
fn symlink_leaf_is_recorded_not_content_read() {
    // (a) A leaf that IS a symlink is sealed AS a symlink entry; its target's
    // bytes never enter workspace state.
    let mut engine = TestEngine::new("push-symlink-leaf");
    let secret = external_secret("symlink-leaf");
    symlink(&secret, engine.root().join("link")).expect("create symlink");

    let baseline = engine.remote.blob_put_count();
    assert!(matches!(
        engine.push(&["link"]),
        PushOutcome::Advanced { .. }
    ));

    let record = engine
        .files()
        .get(&WorkspacePath::new("link"))
        .expect("link recorded")
        .clone();
    assert_eq!(record.kind, EntryKind::Symlink, "recorded as a symlink");
    assert_eq!(
        record.symlink_target.as_deref(),
        secret.to_str(),
        "the link target is recorded verbatim, not dereferenced"
    );
    assert!(
        record.content_id.is_none(),
        "a symlink has no sealed content"
    );
    assert_eq!(
        engine.remote.blob_put_count(),
        baseline,
        "a symlink leaf uploads no blob — the secret bytes are never sealed"
    );
}

#[test]
fn file_swapped_to_external_symlink_is_not_sealed() {
    // (b) An ancestor file replaced by a symlink to an external secret: the swap
    // is recorded as a symlink change and the external bytes are never uploaded.
    let mut engine = TestEngine::new("push-swap-symlink");
    engine.write("doc.txt", b"real workspace bytes");
    assert!(matches!(
        engine.push(&["doc.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let baseline = engine.remote.blob_put_count();

    let secret = external_secret("swap-symlink");
    engine.remove("doc.txt");
    symlink(&secret, engine.root().join("doc.txt")).expect("swap to symlink");

    assert!(matches!(
        engine.push(&["doc.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let record = engine
        .files()
        .get(&WorkspacePath::new("doc.txt"))
        .expect("path still tracked")
        .clone();
    assert_eq!(
        record.kind,
        EntryKind::Symlink,
        "the swap is recorded as a symlink change"
    );
    assert_eq!(record.symlink_target.as_deref(), secret.to_str());
    assert_eq!(
        engine.remote.blob_put_count(),
        baseline,
        "no blob is uploaded — the external secret is never sealed"
    );
}

#[test]
fn intermediate_dir_swapped_to_symlink_is_not_read_through() {
    // (c) A file observed under a real directory whose parent is then swapped for
    // a symlink to an external directory: push must not read the external file
    // beneath the symlinked parent.
    let mut engine = TestEngine::new("push-swap-parent");
    engine.write("dir/file", b"real nested bytes");
    assert!(matches!(
        engine.push(&["dir/file"]),
        PushOutcome::Advanced { .. }
    ));
    let baseline = engine.remote.blob_put_count();
    let real_record = engine
        .files()
        .get(&WorkspacePath::new("dir/file"))
        .expect("nested file tracked")
        .clone();

    // Build an external directory holding a same-named secret file.
    let external_dir = std::env::temp_dir().join("bowline-external-dir-swap-parent");
    std::fs::create_dir_all(&external_dir).expect("external dir");
    std::fs::write(external_dir.join("file"), b"EXTERNAL SECRET UNDER SYMLINK")
        .expect("external secret file");

    // Replace the real `dir` with a symlink to the external directory.
    std::fs::remove_dir_all(engine.root().join("dir")).expect("remove real dir");
    symlink(&external_dir, engine.root().join("dir")).expect("swap dir to symlink");

    // The push must not seal the external file's bytes.
    let outcome = engine.push(&["dir/file"]);
    assert!(
        matches!(outcome, PushOutcome::NoChange { .. }),
        "reading through a symlinked parent is refused, so nothing changes"
    );
    assert_eq!(
        engine.remote.blob_put_count(),
        baseline,
        "no blob uploaded — external bytes under the symlinked parent are never sealed"
    );
    assert_eq!(
        engine.files().get(&WorkspacePath::new("dir/file")),
        Some(&real_record),
        "the tracked record is unchanged; the external file was never adopted"
    );
}

#[test]
fn read_file_bounded_diverges_on_symlink_and_fingerprint_swap() {
    // The mechanism directly: read_file_bounded returns Diverged (never external
    // bytes) when the leaf became a symlink, and when the observed fingerprint no
    // longer matches the on-disk inode.
    let engine = TestEngine::new("read-bounded-diverge");
    let root = engine.root();
    let max = EngineConfig::default().max_seal_bytes;

    // Happy path: an unchanged regular file reads its real bytes.
    engine.write("a.txt", b"real bytes");
    let expected = engine
        .observe("a.txt")
        .expect("observe file")
        .expected_file();
    let read =
        read_file_bounded(&root, &WorkspacePath::new("a.txt"), max, &expected).expect("read");
    assert!(matches!(read, FileRead::Bytes(bytes) if bytes == b"real bytes"));

    // Leaf swapped to a symlink to an external secret AFTER observation: O_NOFOLLOW
    // refuses the open, so the target's bytes are never returned.
    let secret = external_secret("read-bounded");
    engine.remove("a.txt");
    symlink(&secret, root.join("a.txt")).expect("swap to symlink");
    let read =
        read_file_bounded(&root, &WorkspacePath::new("a.txt"), max, &expected).expect("read");
    assert!(
        matches!(read, FileRead::Diverged),
        "a symlinked leaf diverges, never yielding the target bytes"
    );

    // Regular file whose inode/fingerprint no longer matches the observation.
    engine.write("b.txt", b"first");
    let stale = engine.observe("b.txt").expect("observe").expected_file();
    engine.remove("b.txt");
    engine.write("b.txt", b"a different, longer body");
    let read = read_file_bounded(&root, &WorkspacePath::new("b.txt"), max, &stale).expect("read");
    assert!(
        matches!(read, FileRead::Diverged),
        "a replaced inode diverges rather than sealing torn/foreign bytes"
    );
}

#[test]
fn twice_diverged_path_is_reported_as_skipped() {
    // A file reachable ONLY through a symlinked parent observes as a regular file
    // (observe follows the intermediate symlink) but every no-follow content read
    // diverges (the parent walk refuses to descend through the symlink). Two
    // divergences in one scan is the "actively being written" signal: push must
    // hand the path back as `skipped`, never silently drop the pending change.
    let mut engine = TestEngine::new("push-skip-report");
    let external = std::env::temp_dir().join("bowline-push-skip-report-dir");
    std::fs::create_dir_all(&external).expect("external dir");
    std::fs::write(external.join("file"), b"EXTERNAL SECRET").expect("external file");
    symlink(&external, engine.root().join("dir")).expect("symlink dir");

    // Only-skipped batch: no delta to publish, but the churning path is returned.
    match engine.push(&["dir/file"]) {
        PushOutcome::NoChange { skipped } => {
            assert_eq!(
                skipped,
                std::iter::once(WorkspacePath::new("dir/file")).collect(),
                "the twice-diverged path is reported as skipped"
            );
        }
        other => panic!("expected NoChange carrying the skipped path, got {other:?}"),
    }
    assert_eq!(engine.remote.blob_put_count(), 0, "no blob uploaded");

    // Mixed batch: a clean file advances the head AND the churning path is still
    // reported as skipped (the advance never drops it).
    engine.write("clean.txt", b"real workspace bytes");
    match engine.push(&["clean.txt", "dir/file"]) {
        PushOutcome::Advanced { skipped, .. } => {
            assert_eq!(
                skipped,
                std::iter::once(WorkspacePath::new("dir/file")).collect(),
                "an advancing push still reports the churning path"
            );
        }
        other => panic!("expected Advanced carrying the skipped path, got {other:?}"),
    }
    // The advanced head carries the clean file; the churning path was NOT sealed.
    assert!(
        engine
            .files()
            .contains_key(&WorkspacePath::new("clean.txt"))
    );
    assert!(!engine.files().contains_key(&WorkspacePath::new("dir/file")));
}

#[test]
fn read_file_bounded_diverges_through_symlinked_parent() {
    // A file observed under a real directory whose parent is swapped for a symlink
    // to an external directory: the no-follow parent walk refuses to read through.
    let engine = TestEngine::new("read-bounded-parent");
    let root = engine.root();
    let max = EngineConfig::default().max_seal_bytes;

    engine.write("dir/file", b"real nested");
    let expected = engine
        .observe("dir/file")
        .expect("observe nested")
        .expected_file();

    let external_dir = std::env::temp_dir().join("bowline-external-dir-read-parent");
    std::fs::create_dir_all(&external_dir).expect("external dir");
    std::fs::write(external_dir.join("file"), b"EXTERNAL SECRET").expect("external file");
    std::fs::remove_dir_all(root.join("dir")).expect("remove real dir");
    symlink(&external_dir, root.join("dir")).expect("swap dir to symlink");

    let read =
        read_file_bounded(&root, &WorkspacePath::new("dir/file"), max, &expected).expect("read");
    assert!(
        matches!(read, FileRead::Diverged),
        "a symlinked intermediate component diverges, never reading the external file"
    );
}
