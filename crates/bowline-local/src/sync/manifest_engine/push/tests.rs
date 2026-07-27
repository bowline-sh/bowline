//! Push contract tests (Plan 109 Step 4). The ancestor is sacred: it changes
//! only on a proven CAS advance; a lost or ambiguous CAS never corrupts it.

use std::os::unix::fs::symlink;
use std::path::PathBuf;

use super::{DeletionPolicy, EntryKind, PushDeps, PushOutcome, WatcherEvidence, push_dirty_paths};
use crate::sync::manifest_engine::endpoint::TimestampGranularity;
use crate::sync::manifest_engine::engine_test_support::{CasMode, Event, KEY_BYTES, TestEngine};
use crate::sync::manifest_engine::fs_guard::{FileRead, Observed, read_file_bounded};
use crate::sync::manifest_engine::manifest::{KeyEpoch, WorkspaceCrypto, WorkspacePath};
use crate::sync::manifest_engine::push::EngineConfig;
use crate::sync::manifest_engine::store::StatFingerprint;
use crate::sync::manifest_engine::unsyncable::UnsyncableReason;

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
        push_dirty_paths(
            &mut engine.store,
            &deps,
            &dirty,
            DeletionPolicy::Enforce,
            WatcherEvidence::Gapped,
        )
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
    engine.settle_endpoint_clock("same.txt");
    let before_refresh = engine.counters().content_hashes;
    let dirty = engine.dirty(&["same.txt"]);
    let deps = PushDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    assert!(matches!(
        push_dirty_paths(
            &mut engine.store,
            &deps,
            &dirty,
            DeletionPolicy::Enforce,
            WatcherEvidence::Gapped,
        )
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
        push_dirty_paths(
            &mut engine.store,
            &deps,
            &dirty,
            DeletionPolicy::Enforce,
            WatcherEvidence::Gapped,
        )
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
    // The file becomes a reported unsyncable path — NOT an error that kills the
    // engine. One oversize dataset dropped into ~/Code must not stop the
    // workspace from syncing everything else.
    let ceiling = EngineConfig {
        large_file_threshold: 4,
        max_seal_bytes: 64,
    };
    let mut engine = TestEngine::with_config("push-ceiling", ceiling);
    engine.write("huge.bin", &vec![1u8; 256]);
    engine.write("small.txt", b"fine");
    let outcome = engine
        .try_push(&["huge.bin", "small.txt"])
        .expect("an oversize file is a path-scoped divergence, never a push failure");
    assert!(matches!(outcome, PushOutcome::Advanced { .. }));
    let manifest = engine
        .remote
        .decoded_manifest(&engine.ctx.crypto)
        .expect("head manifest");
    assert!(
        manifest
            .entries
            .contains_key(&WorkspacePath::new("small.txt"))
    );
    assert!(
        !manifest
            .entries
            .contains_key(&WorkspacePath::new("huge.bin"))
    );
    assert_eq!(
        engine
            .store
            .unsyncable()
            .expect("unsyncable")
            .get(&WorkspacePath::new("huge.bin"))
            .map(|record| record.reason),
        Some(UnsyncableReason::AboveSealCeiling),
        "the oversize file is reported with a remedy, not silently dropped"
    );
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
    engine.write("target.txt", b"inside the workspace");
    symlink("target.txt", engine.root().join("link")).expect("create symlink");
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
    engine.write("first.txt", b"first");
    engine.write("second.txt", b"second");
    symlink("first.txt", engine.root().join("link")).expect("create symlink");
    assert!(matches!(
        engine.push(&["d", "link", "first.txt", "second.txt"]),
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
    engine.remove("link");
    symlink("second.txt", engine.root().join("link")).expect("retarget symlink");
    assert!(
        matches!(engine.push(&["link"]), PushOutcome::Advanced { .. }),
        "a retargeted symlink still pushes"
    );
    assert_eq!(
        engine.files()[&WorkspacePath::new("link")]
            .symlink_target
            .as_deref(),
        Some("second.txt"),
        "the new symlink target is recorded"
    );
}

// ---- no-follow content-read hardening (review P1) ---------------------------

#[test]
fn symlink_leaf_is_recorded_not_content_read() {
    // (a) A leaf that IS a symlink is sealed AS a symlink entry; its target's
    // bytes never enter workspace state.
    let mut engine = TestEngine::new("push-symlink-leaf");
    engine.write("target.txt", b"inside the workspace");
    symlink("target.txt", engine.root().join("link")).expect("create symlink");

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
        Some("target.txt"),
        "the link target is recorded verbatim, not dereferenced"
    );
    assert!(
        record.content_id.is_none(),
        "a symlink has no sealed content"
    );
    assert_eq!(
        engine.remote.blob_put_count(),
        baseline,
        "a symlink leaf uploads no blob \u{2014} the target bytes are never sealed"
    );
}

#[test]
fn symlink_leaf_escaping_the_workspace_is_never_published() {
    // A link out of the workspace is refused rather than published: every peer
    // that applied it would get a link their own tools follow to that path on
    // THEIR machine. It is recorded with a remedy, and the push still advances
    // for everything else.
    let mut engine = TestEngine::new("push-symlink-escape");
    let secret = external_secret("symlink-escape");
    symlink(&secret, engine.root().join("link")).expect("create symlink");
    engine.write("fine.txt", b"ordinary");

    let baseline = engine.remote.blob_put_count();
    let outcome = engine
        .try_push(&["link", "fine.txt"])
        .expect("an escaping symlink is a path-scoped divergence, never a push failure");
    assert!(matches!(outcome, PushOutcome::Advanced { .. }));

    let manifest = engine
        .remote
        .decoded_manifest(&engine.ctx.crypto)
        .expect("head manifest");
    assert!(
        !manifest.entries.contains_key(&WorkspacePath::new("link")),
        "the escaping link never reaches a peer's manifest"
    );
    assert!(
        manifest
            .entries
            .contains_key(&WorkspacePath::new("fine.txt")),
        "one refused path does not stop the rest of the workspace syncing"
    );
    assert_eq!(
        engine
            .store
            .unsyncable()
            .expect("unsyncable")
            .get(&WorkspacePath::new("link"))
            .map(|record| record.reason),
        Some(UnsyncableReason::EscapingSymlinkTarget),
        "the refusal is reported with a remedy, not silently dropped"
    );
    assert_eq!(
        engine.remote.blob_put_count(),
        baseline + 1,
        "only the ordinary file uploaded; the secret bytes are never sealed"
    );
}

#[test]
fn absolute_and_climbing_symlink_targets_are_both_refused() {
    let mut engine = TestEngine::new("push-symlink-escape-shapes");
    std::fs::create_dir(engine.root().join("d")).expect("mkdir");
    // Absolute, single climb from the root, and a climb from mid-path that lands
    // outside even though it descends again afterwards.
    symlink("/etc/passwd", engine.root().join("absolute")).expect("absolute link");
    symlink("../outside", engine.root().join("climb")).expect("climbing link");
    symlink("../../outside/x", engine.root().join("d/deep")).expect("deep climbing link");
    // A link that climbs and comes back inside is legitimate and must still sync.
    symlink("../inside.txt", engine.root().join("d/ok")).expect("contained link");
    engine.write("inside.txt", b"inside");

    let outcome = engine
        .try_push(&["absolute", "climb", "d", "d/deep", "d/ok", "inside.txt"])
        .expect("escaping links never fail the push");
    assert!(matches!(outcome, PushOutcome::Advanced { .. }));

    let refused = engine.store.unsyncable().expect("unsyncable");
    for path in ["absolute", "climb", "d/deep"] {
        assert_eq!(
            refused.get(&WorkspacePath::new(path)).map(|r| r.reason),
            Some(UnsyncableReason::EscapingSymlinkTarget),
            "{path} escapes the workspace and must be refused"
        );
    }
    assert!(
        !refused.contains_key(&WorkspacePath::new("d/ok")),
        "a link that climbs but stays inside the workspace is not an escape"
    );
    assert_eq!(
        engine.files()[&WorkspacePath::new("d/ok")]
            .symlink_target
            .as_deref(),
        Some("../inside.txt"),
        "the contained link syncs with its target intact"
    );
}

#[test]
fn file_swapped_to_external_symlink_is_not_sealed() {
    // (b) An ancestor file replaced by a symlink to an external secret: the swap
    // is refused, the external bytes are never uploaded, and — critically — the
    // ancestor row is left alone. Publishing a removal here would delete the
    // user's file on every other device because of a local symlink swap.
    let mut engine = TestEngine::new("push-swap-symlink");
    engine.write("doc.txt", b"real workspace bytes");
    assert!(matches!(
        engine.push(&["doc.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let baseline = engine.remote.blob_put_count();
    let before = engine.files()[&WorkspacePath::new("doc.txt")].clone();

    let secret = external_secret("swap-symlink");
    engine.remove("doc.txt");
    symlink(&secret, engine.root().join("doc.txt")).expect("swap to symlink");

    let outcome = engine
        .try_push(&["doc.txt"])
        .expect("an escaping swap is a path-scoped divergence, never a push failure");
    assert!(
        matches!(outcome, PushOutcome::NoChange { .. }),
        "nothing publishable changed, so no manifest is sealed"
    );
    assert_eq!(
        engine.files().get(&WorkspacePath::new("doc.txt")),
        Some(&before),
        "the ancestor row survives untouched — never republished as a deletion"
    );
    assert_eq!(
        engine
            .store
            .unsyncable()
            .expect("unsyncable")
            .get(&WorkspacePath::new("doc.txt"))
            .map(|record| record.reason),
        Some(UnsyncableReason::EscapingSymlinkTarget),
        "the swap is reported so the user can see why the path stopped syncing"
    );
    assert_eq!(
        engine.remote.blob_put_count(),
        baseline,
        "no blob is uploaded — the external secret is never sealed"
    );
}

#[test]
fn file_swapped_to_internal_symlink_is_recorded_as_a_symlink_change() {
    // The same swap, staying inside the workspace, is an ordinary kind change.
    let mut engine = TestEngine::new("push-swap-symlink-internal");
    engine.write("doc.txt", b"real workspace bytes");
    engine.write("other.txt", b"other");
    assert!(matches!(
        engine.push(&["doc.txt", "other.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let baseline = engine.remote.blob_put_count();

    engine.remove("doc.txt");
    symlink("other.txt", engine.root().join("doc.txt")).expect("swap to symlink");

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
    assert_eq!(record.symlink_target.as_deref(), Some("other.txt"));
    assert_eq!(
        engine.remote.blob_put_count(),
        baseline,
        "a symlink carries no content, so no blob is uploaded"
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

// ---- the blob ledger (dedup) -----------------------------------------------

#[test]
fn a_rename_reuses_the_sealed_object_instead_of_re_uploading_it() {
    let mut engine = TestEngine::new("push-rename-dedup");
    engine.write("first.txt", b"the same bytes under two names");
    assert!(matches!(
        engine.push(&["first.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let uploads = engine.remote.blob_put_count();

    std::fs::rename(
        engine.root().join("first.txt"),
        engine.root().join("second.txt"),
    )
    .expect("rename");
    assert!(matches!(
        engine.push(&["first.txt", "second.txt"]),
        PushOutcome::Advanced { .. }
    ));

    assert_eq!(
        engine.remote.blob_put_count(),
        uploads,
        "a rename moves a manifest entry; the sealed object is already stored"
    );
    let files = engine.files();
    assert_eq!(
        files
            .get(&WorkspacePath::new("second.txt"))
            .expect("renamed path is tracked")
            .blob_key,
        engine
            .store
            .sealed_blob(
                &engine.content_id(b"the same bytes under two names"),
                KeyEpoch::new(1),
            )
            .expect("ledger read")
            .map(|blob| blob.blob_key),
    );
}

#[test]
fn an_a_b_a_edit_cycle_uploads_two_objects_not_three() {
    // Equal-length versions, which is the case a coarse-granularity volume used
    // to lose: same size, same mtime bucket, same ctime bucket, so the third
    // push found nothing to publish. The engine no longer settles a stat it
    // cannot prove, so the dedup this test measures is measured over the
    // content shape that exposed the bug.
    let mut engine = TestEngine::new("push-aba-dedup");
    engine.write("f.txt", b"version A");
    assert!(matches!(
        engine.push(&["f.txt"]),
        PushOutcome::Advanced { .. }
    ));
    engine.write("f.txt", b"version B");
    assert!(matches!(
        engine.push(&["f.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let after_b = engine.remote.blob_put_count();
    assert_eq!(after_b, 2, "A and B are genuinely different content");

    engine.write("f.txt", b"version A");
    assert!(matches!(
        engine.push(&["f.txt"]),
        PushOutcome::Advanced { .. }
    ));

    assert_eq!(
        engine.remote.blob_put_count(),
        after_b,
        "returning to content this device already sealed costs no seal and no PUT"
    );
    assert_eq!(engine.read("f.txt"), b"version A");
}

// ---- the racily-clean window ------------------------------------------------

#[test]
fn a_settled_pull_echo_costs_no_content_reads_on_the_next_push() {
    let mut engine = TestEngine::new("push-racy-echo");
    engine.write("echoed.txt", b"installed by a peer");
    // The peer's apply is what makes this row settleable, and a real one takes
    // far longer than the volume's tick. Wait for the same condition rather than
    // asserting an optimization the endpoint has not offered yet.
    engine.settle_endpoint_clock("echoed.txt");
    assert!(matches!(
        engine.push(&["echoed.txt"]),
        PushOutcome::Advanced { .. }
    ));

    // The watcher re-reports the path (every apply generates local events). Under
    // blanket verification this re-read and re-hashed the file to conclude
    // "unchanged"; the racily-clean window settles it from the stat alone,
    // because the row carries an endpoint-clock reading the file's ctime is
    // strictly older than.
    let before = engine.counters().content_opens;
    let dirty = engine.dirty(&["echoed.txt"]);
    let deps = PushDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    assert!(matches!(
        push_dirty_paths(
            &mut engine.store,
            &deps,
            &dirty,
            DeletionPolicy::Enforce,
            WatcherEvidence::Continuous,
        )
        .expect("echo push"),
        PushOutcome::NoChange { .. }
    ));
    assert_eq!(
        engine.counters().content_opens,
        before,
        "a settled echo is decided by stat alone"
    );
}

/// Every granularity a probe can answer, so each racy-window test states its
/// claim for the whole range rather than for whichever volume CI provides.
const EVERY_GRANULARITY: [TimestampGranularity; 3] = [
    TimestampGranularity::NANOSECOND,
    TimestampGranularity::SECOND,
    TimestampGranularity::TWO_SECONDS,
];

/// The granularities at which the injected model GUARANTEES the endpoint cannot
/// date two rapid writes apart, so a test may assert that it did not.
///
/// `NANOSECOND` is deliberately absent. Whether a nanosecond-granularity engine
/// sees two rapid writes as one is a property of the host volume — ext4 stamps
/// from a per-tick clock and collides, APFS separates them — so asserting the
/// same-tick path there would assert something about the machine the suite
/// happens to run on. That is the exact dependence this fix removes; the
/// behaviour claim below still covers every granularity, only the claim about
/// which internal path ran is narrowed to where it is guaranteed.
const COARSE_GRANULARITIES: [TimestampGranularity; 2] = [
    TimestampGranularity::SECOND,
    TimestampGranularity::TWO_SECONDS,
];

/// Assert this endpoint could not have told the ancestor row and the fresh
/// observation apart. That is the state a coarse volume produces natively, the
/// one the replaced rule mis-settled, and the precondition each test below
/// needs in order to be testing what its name says.
fn assert_stat_collides(
    granularity: TimestampGranularity,
    recorded: &StatFingerprint,
    observed: &Observed,
) {
    assert!(
        granularity.indistinguishable(recorded.mtime_ns, observed.fingerprint.mtime_ns)
            && granularity.indistinguishable(recorded.ctime_ns, observed.fingerprint.ctime_ns),
        "at {granularity:?} the scenario must land inside one bucket, so that the engine faces \
         the stat it cannot date: recorded {recorded:?}, observed {:?}",
        observed.fingerprint
    );
}

/// Set a file's mtime, the way `tar -x`, `rsync -t` and `cp -p` do after writing
/// new bytes. ctime is deliberately NOT settable — that asymmetry is the whole
/// reason the racy-window proof runs on ctime.
fn restore_mtime(path: &std::path::Path, nanos: i64) {
    let stamps = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: nanos / 1_000_000_000,
            tv_nsec: (nanos % 1_000_000_000) as _,
        },
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &stamps,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .expect("restore mtime");
}

/// An mtime far enough back that every window is comfortably clear of it.
const RESTORED_MTIME_NS: i64 = 1_600_000_000_000_000_000;

/// The regression this whole mechanism exists for: a same-size rewrite that
/// leaves the stat looking exactly as the ancestor recorded it must still be
/// published, on every granularity a probe can report.
///
/// The mtime is restored after each write, which is what an archive extractor
/// does, so mtime and size are both unchanged across a real content change. On
/// a coarse-granularity volume the ctime collides too — that is the CI shape,
/// reproduced here on any host by telling the engine the volume is coarse.
///
/// Against the rule this replaced (`mtime + granularity < verified_at`, with
/// `verified_at` read off the process wall clock) every iteration returns
/// `NoChange` and the user's second version is never uploaded: the restored
/// mtime sits an hour below a window anchored to a clock that had, of course,
/// reached the present.
#[test]
fn a_same_size_rewrite_under_an_unchanged_mtime_is_published_on_every_granularity() {
    for granularity in EVERY_GRANULARITY {
        let mut engine = TestEngine::new(&format!("push-restored-mtime-{}", granularity.nanos()));
        engine.ctx.timestamps = granularity;
        let path = engine.root().join("doc.txt");
        engine.align_within_one_bucket();

        engine.write("doc.txt", b"v1");
        restore_mtime(&path, RESTORED_MTIME_NS);
        assert!(matches!(
            engine.push(&["doc.txt"]),
            PushOutcome::Advanced { .. }
        ));
        let after_first = engine.remote.blob_put_count();
        let recorded = engine.files()[&WorkspacePath::new("doc.txt")].fingerprint;

        engine.write("doc.txt", b"v2");
        restore_mtime(&path, RESTORED_MTIME_NS);
        if COARSE_GRANULARITIES.contains(&granularity) {
            // The restored mtime hides the write from mtime on every volume;
            // at these granularities the endpoint cannot see it in the ctime
            // either, which is the whole state under test.
            assert_stat_collides(
                granularity,
                &recorded,
                &engine.observe("doc.txt").expect("observe doc.txt"),
            );
        }
        assert!(
            matches!(engine.push(&["doc.txt"]), PushOutcome::Advanced { .. }),
            "the second version must reach the manifest at {granularity:?}"
        );
        assert_eq!(
            engine.remote.blob_put_count(),
            after_first + 1,
            "the second version's bytes must reach the object store at {granularity:?}"
        );
        let record = engine.files()[&WorkspacePath::new("doc.txt")].clone();
        assert_eq!(record.size, 2);
        assert_eq!(
            record.content_id.as_ref(),
            Some(&engine.ctx.crypto.content_id(b"v2")),
            "the ancestor must name the bytes on disk at {granularity:?}"
        );
    }
}

/// The same-tick case: two writes the endpoint records as one timestamp still
/// publish the second one. The scenario is placed inside a single bucket and the
/// collision is asserted before the push, so this is testing the stat the engine
/// cannot date rather than hoping the host produced one. The decision then rests
/// entirely on the racy-window proof — which the cycle could not take, because it
/// sampled the volume's clock inside the very tick the write landed in.
#[test]
fn two_writes_inside_one_endpoint_tick_still_publish_the_second() {
    for granularity in COARSE_GRANULARITIES {
        let mut engine = TestEngine::new(&format!("push-one-tick-{}", granularity.nanos()));
        engine.ctx.timestamps = granularity;
        engine.align_within_one_bucket();

        engine.write("doc.txt", b"v1");
        assert!(matches!(
            engine.push(&["doc.txt"]),
            PushOutcome::Advanced { .. }
        ));
        let recorded = engine.files()[&WorkspacePath::new("doc.txt")].fingerprint;

        engine.write("doc.txt", b"v2");
        assert_stat_collides(
            granularity,
            &recorded,
            &engine.observe("doc.txt").expect("observe doc.txt"),
        );
        assert!(
            matches!(engine.push(&["doc.txt"]), PushOutcome::Advanced { .. }),
            "a rewrite the endpoint cannot date must still be published at {granularity:?}"
        );
        assert_eq!(
            engine.files()[&WorkspacePath::new("doc.txt")]
                .content_id
                .as_ref(),
            Some(&engine.ctx.crypto.content_id(b"v2"))
        );
    }
}

/// A row may only carry a proof the cycle actually took, and the next push must
/// act on exactly that. Both halves establish their endpoint condition instead
/// of inheriting it: the coarse one places the write and the verifying reading
/// inside one bucket, the fine one waits for the volume's clock to pass the
/// write — the condition a real apply or push clears on its own, in the time it
/// spends installing thousands of files or sealing one.
#[test]
fn a_row_carries_an_observation_instant_only_where_one_was_provable() {
    let mut coarse = TestEngine::new("push-proof-coarse");
    coarse.ctx.timestamps = TimestampGranularity::SECOND;
    coarse.align_within_one_bucket();
    coarse.write("doc.txt", b"written and verified inside one bucket");
    assert!(matches!(
        coarse.push(&["doc.txt"]),
        PushOutcome::Advanced { .. }
    ));
    assert_eq!(
        coarse.files()[&WorkspacePath::new("doc.txt")].verified_at,
        None,
        "a volume that dates the write and the verifying reading alike proves nothing"
    );
    let before = coarse.counters().content_opens;
    assert!(matches!(
        coarse.push(&["doc.txt"]),
        PushOutcome::NoChange { .. }
    ));
    assert_eq!(
        coarse.counters().content_opens,
        before + 1,
        "an unproved row is read, never settled from its stat"
    );

    let mut fine = TestEngine::new("push-proof-fine");
    fine.ctx.timestamps = TimestampGranularity::NANOSECOND;
    fine.write("doc.txt", b"written, then dated by a clock that moved on");
    fine.settle_endpoint_clock("doc.txt");
    assert!(matches!(
        fine.push(&["doc.txt"]),
        PushOutcome::Advanced { .. }
    ));
    let record = fine.files()[&WorkspacePath::new("doc.txt")].clone();
    let verified_at = record
        .verified_at
        .expect("a clock reading past the write proves the row");
    assert!(
        record.fingerprint.ctime_ns < verified_at.nanos(),
        "the instant a row claims must be one the endpoint clock had passed"
    );
    let before = fine.counters().content_opens;
    assert!(matches!(
        fine.push(&["doc.txt"]),
        PushOutcome::NoChange { .. }
    ));
    assert_eq!(
        fine.counters().content_opens,
        before,
        "a proved row settles from its stat"
    );
}

#[test]
fn an_unbounded_unobserved_gap_still_reads_the_bytes() {
    let mut engine = TestEngine::new("push-racy-gapped");
    engine.write("gapped.txt", b"before");
    assert!(matches!(
        engine.push(&["gapped.txt"]),
        PushOutcome::Advanced { .. }
    ));

    let before = engine.counters().content_opens;
    let dirty = engine.dirty(&["gapped.txt"]);
    let deps = PushDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    assert!(matches!(
        push_dirty_paths(
            &mut engine.store,
            &deps,
            &dirty,
            DeletionPolicy::Enforce,
            WatcherEvidence::Gapped,
        )
        .expect("gapped push"),
        PushOutcome::NoChange { .. }
    ));
    assert_eq!(
        engine.counters().content_opens,
        before + 1,
        "a stat-walk batch after a daemon-down window is never settled by stat"
    );
}
