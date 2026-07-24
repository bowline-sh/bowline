//! Pull / merge-matrix / apply-transaction / recovery tests (Plan 109 Step 5).
//!
//! Every one of the eleven merge-matrix rows is a named test, plus the apply
//! guards (preimage race, no-resurrect, mode-only, symlink), the Git contract,
//! and the six recovery boundaries as pure-function checks.

use super::apply::{Applied, apply_op, git_apply_rank, recovery_action, recovery_boundary};
use super::intents::{PreimagePayload, target_payload};
use super::materialize::{DeleteOutcome, checked_delete};
use super::{
    FsOp, FsOpKind, LocalDelta, PullDeps, PullError, RecoveryAction, RecoveryBoundary,
    RecoveryObservation, local_vs_record,
};
use crate::sync::manifest_engine::engine_test_support::{Event, TestEngine};
use crate::sync::manifest_engine::manifest::{FileMode, ManifestEntry, ManifestKey, WorkspacePath};
use crate::sync::manifest_engine::store::{Intent, IntentOperationKind};

fn wp(path: &str) -> WorkspacePath {
    WorkspacePath::new(path)
}

// ---- the eleven merge-matrix rows ------------------------------------------

#[test]
fn row_ancestor_absent_local_absent_remote_present_creates_remote() {
    let mut engine = TestEngine::new("row-create");
    let entry = engine.remote_file(b"remote bytes");
    engine.publish(&[("new.txt", entry)]);

    let outcome = engine.pull();
    assert_eq!(engine.read("new.txt"), b"remote bytes");
    assert!(engine.files().contains_key(&wp("new.txt")));
    assert!(outcome.installed.contains(&wp("new.txt")));
}

#[test]
fn row_ancestor_absent_local_present_remote_absent_keeps_local() {
    let mut engine = TestEngine::new("row-keep-untracked");
    engine.write("local.txt", b"local only");
    engine.publish(&[]); // remote head lists nothing

    engine.pull();
    // Remote absence has no deletion authority over an untracked local file.
    assert_eq!(engine.read("local.txt"), b"local only");
    assert!(!engine.files().contains_key(&wp("local.txt")));
}

#[test]
fn row_ancestor_absent_local_present_remote_identical_adopts_without_rewrite() {
    let mut engine = TestEngine::new("row-adopt");
    engine.write("same.txt", b"shared bytes");
    let before = engine.observe("same.txt").expect("present");
    let entry = engine.remote_file(b"shared bytes");
    engine.publish(&[("same.txt", entry)]);

    let outcome = engine.pull();
    assert!(outcome.conflict_asides.is_empty());
    assert!(engine.files().contains_key(&wp("same.txt")));
    // Adopted, not rewritten: the file's fingerprint is unchanged.
    let after = engine.observe("same.txt").expect("present");
    assert_eq!(before.fingerprint, after.fingerprint);
}

#[test]
fn row_ancestor_absent_local_present_remote_different_asides_remote() {
    let mut engine = TestEngine::new("row-untracked-conflict");
    engine.write("conf.txt", b"local version");
    let entry = engine.remote_file(b"remote version");
    engine.publish(&[("conf.txt", entry)]);

    let outcome = engine.pull();
    assert_eq!(engine.read("conf.txt"), b"local version");
    let aside = outcome.conflict_asides.iter().next().expect("one aside");
    assert_eq!(engine.read(aside.as_str()), b"remote version");
}

#[test]
fn row_ancestor_absent_local_present_remote_different_repushes_kept_local() {
    use crate::sync::manifest_engine::push::PushOutcome;

    let mut engine = TestEngine::new("row-untracked-conflict-repush");
    engine.write("conf.txt", b"local version");
    let entry = engine.remote_file(b"remote version");
    engine.publish(&[("conf.txt", entry)]);

    let outcome = engine.pull();
    // The remote diverges: it asides, but the kept-local original MUST also be
    // re-queued for push. Without the re-queue a coalesced/lost watcher event on
    // the local file would keep its bytes out of the manifest until an unrelated
    // full scan (mirrors the changed-vs-changed conflict row).
    assert_eq!(engine.read("conf.txt"), b"local version");
    let aside = outcome.conflict_asides.iter().next().expect("one aside");
    assert_eq!(engine.read(aside.as_str()), b"remote version");
    assert!(
        outcome.push_again.contains(&wp("conf.txt")),
        "the kept-local original is re-queued for push"
    );

    // The follow-on push publishes the local original's content.
    assert!(matches!(
        engine.push(&["conf.txt"]),
        PushOutcome::Advanced { .. }
    ));
    assert_eq!(
        engine.files()[&wp("conf.txt")].content_id,
        Some(engine.content_id(b"local version")),
        "the local original entered the manifest"
    );
}

#[test]
fn row_ancestor_present_local_unchanged_remote_deleted_deletes() {
    let mut engine = TestEngine::new("row-delete");
    engine.write("del.txt", b"content");
    engine.push(&["del.txt"]);
    engine.publish(&[]); // remote removed it

    let outcome = engine.pull();
    assert!(!engine.exists("del.txt"));
    assert!(!engine.files().contains_key(&wp("del.txt")));
    assert!(outcome.deleted.contains(&wp("del.txt")));
}

#[test]
fn echo_pull_of_own_push_performs_no_sqlite_writes() {
    let mut engine = TestEngine::new("echo-pull-no-writes");
    engine.write("f.txt", b"pushed bytes");
    engine.push(&["f.txt"]);
    // The first pull verifies the freshness ratchet against our own pushed head
    // and records it once.
    engine.pull();
    // A second pull observes the identical head: the ratchet already records it
    // and the head is already applied, so it must write nothing (invariant C1 —
    // idle costs nothing). Before the fix the ratchet was rewritten every pull.
    let before = engine.counters().sqlite_mutations;
    let outcome = engine.pull();
    assert!(outcome.already_current, "echo pull is already current");
    assert_eq!(
        engine.counters().sqlite_mutations,
        before,
        "an echo pull of our own head performs zero SQLite writes"
    );
}

#[test]
fn conflict_aside_recovery_is_idempotent_across_double_crash() {
    use super::apply::{build_intent, recover_one};
    use crate::sync::manifest_engine::store::AncestorCommit;
    use std::collections::BTreeSet;

    let mut engine = TestEngine::new("aside-recovery-idempotent");
    // A pushed ancestor so a divergence is a genuine three-way conflict.
    engine.write("c.txt", b"local original");
    engine.push(&["c.txt"]);
    // Local edit diverges; the remote publishes different bytes: conflict aside.
    engine.write("c.txt", b"local edit");
    let entry = engine.remote_file(b"remote bytes");
    engine.publish(&[("c.txt", entry)]);

    let head = engine.remote.current_ref().expect("head");
    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    let plan = super::decide_head(&mut engine.store, &deps, &head).expect("decide head");
    let op = plan
        .fs_ops
        .into_iter()
        .find(|op| op.path.as_str() == "c.txt")
        .expect("an fs op for c.txt");
    assert!(
        matches!(op.kind, FsOpKind::ConflictAside(_)),
        "the divergence is a conflict aside"
    );

    let intent = build_intent(&op, None, &head.manifest_key);
    engine.store.open_intent(&intent).expect("open intent");

    // Drive recovery twice WITHOUT ever committing the outcome — a crash after
    // materialize, before the outcome commit, that re-enters recovery each time.
    let mut temps = BTreeSet::new();
    let mut commit = AncestorCommit::default();
    recover_one(&mut engine.store, &deps, &intent, &mut commit, &mut temps).expect("recover 1");
    recover_one(&mut engine.store, &deps, &intent, &mut commit, &mut temps).expect("recover 2");

    let asides = aside_names(&engine.root());
    assert_eq!(
        asides.len(),
        1,
        "exactly one aside survives a double-crash recovery: {asides:?}"
    );
    assert_eq!(
        engine.read(&asides[0]),
        b"remote bytes",
        "aside carries remote bytes"
    );
    assert_eq!(
        engine.read("c.txt"),
        b"local edit",
        "the local original is untouched"
    );
}

/// Whether `dir`'s filesystem folds case (macOS/Windows default). Probes with a
/// matched pair of names differing only in case: on a case-insensitive volume the
/// upper-case name resolves to the file created under the lower-case name. The
/// case-fold collision tests use this to run on the matching filesystem and skip
/// cleanly on the other.
fn is_case_insensitive_dir(dir: &std::path::Path) -> bool {
    let lower = dir.join(".bowline-case-probe");
    let upper = dir.join(".bowline-case-PROBE");
    let _ = std::fs::remove_file(&lower);
    let _ = std::fs::remove_file(&upper);
    if std::fs::File::create(&lower).is_err() {
        return false;
    }
    let insensitive = std::fs::symlink_metadata(&upper).is_ok();
    let _ = std::fs::remove_file(&lower);
    let _ = std::fs::remove_file(&upper);
    insensitive
}

/// Workspace-relative names of every conflict aside under `root`.
fn aside_names(root: &std::path::Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return names;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains("conflict from") {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// FINDING A — a kill-9 during the FIRST pull (no prior applied head) must not
/// let recovery fabricate a ref version. If it persists the incoming head at a
/// fabricated version 0, the follow-on pull short-circuits at `already_current`
/// and never corrects it, so every subsequent push CASes against 0, loses, and
/// livelocks. Recovery must NOT advance the head (its documented contract); the
/// follow-on pull re-derives and commits the TRUE head + version.
#[test]
fn first_pull_crash_recovery_lets_a_later_push_cas_against_the_real_version() {
    use crate::sync::manifest_engine::push::PushOutcome;

    let mut engine = TestEngine::new("first-pull-crash");
    let entry = engine.remote_file(b"remote create bytes");
    engine.publish(&[("created.txt", entry)]);
    let head = engine.remote.current_ref().expect("seeded head");

    // Reproduce the crash in-process: journal the intent and install the file
    // (apply_op), then stop before `commit_pull_outcome` advances the head. On a
    // first pull the engine_state has no prior `last_ref_version`.
    {
        let deps = PullDeps {
            ctx: &engine.ctx,
            objects: &engine.remote,
            refs: &engine.remote,
        };
        let plan = super::decide_head(&mut engine.store, &deps, &head).expect("decide head");
        let op = plan
            .fs_ops
            .into_iter()
            .find(|op| op.path.as_str() == "created.txt")
            .expect("install op for created.txt");
        apply_op(&mut engine.store, &deps, &op, &head.manifest_key).expect("apply op");
    }
    assert!(
        !engine.store.pending_intents().expect("intents").is_empty(),
        "the crash left a pending intent"
    );

    // Recovery runs inside pull; convergence must land the real applied version.
    engine.pull();

    engine.write("second.txt", b"local addition");
    let outcome = engine.try_push(&["second.txt"]).expect("push");
    assert!(
        matches!(outcome, PushOutcome::Advanced { .. }),
        "post-recovery push must CAS against the real ref version, got {outcome:?}"
    );
}

/// FINDING B (case-insensitive fs) — REFUTED, locked in as a regression test. A
/// remote manifest listing two paths that fold to one case-insensitive name
/// (`Foo` + `foo`) does NOT silently overwrite. Both are scheduled as installs,
/// but the byte-order-least path (`Foo`) applies first (ascending apply order) and
/// installs; the later `foo` re-observes the winner present, fails its
/// absent-preimage check, and deflects to a conflict-aside. Only the winner
/// reaches the ancestor; the loser's bytes survive as an aside; and once the head
/// advances the next pull is `already_current`, so nothing re-duplicates. The
/// apply-time preimage guard, not any create-branch collision handling, is what
/// makes this safe. Skips cleanly on a case-sensitive volume.
#[test]
fn case_fold_create_collision_installs_winner_and_asides_loser() {
    let mut engine = TestEngine::new("case-fold-create");
    if !is_case_insensitive_dir(&engine.root()) {
        eprintln!("skip: case-sensitive filesystem (collision cannot occur)");
        return;
    }
    let upper = engine.remote_file(b"UPPER Foo bytes");
    let lower = engine.remote_file(b"lower foo bytes");
    engine.publish(&[("Foo", upper), ("foo", lower)]);

    let outcome = engine.pull();

    // One live entry at the shared case-folded path, carrying the byte-order
    // winner "Foo"; the loser "foo" is preserved as a single aside.
    assert_eq!(engine.read("Foo"), b"UPPER Foo bytes");
    assert_eq!(
        outcome.installed,
        [wp("Foo")].into_iter().collect(),
        "only the winner installs at a live path"
    );
    assert_eq!(
        outcome.conflict_asides.len(),
        1,
        "the loser lands as an aside"
    );
    // No silent loss: the loser's bytes survive on disk as one aside.
    let asides = aside_names(&engine.root());
    assert_eq!(asides.len(), 1, "exactly one aside: {asides:?}");
    assert_eq!(engine.read(&asides[0]), b"lower foo bytes");
    // Ancestor consistent with disk: winner has a live-path row, loser has none
    // (its bytes live only at the aside, never at the live path).
    assert!(engine.files().contains_key(&wp("Foo")));
    assert!(!engine.files().contains_key(&wp("foo")));

    // Idempotent: a second pull neither loses data nor spawns a duplicate aside.
    engine.pull();
    let asides = aside_names(&engine.root());
    assert_eq!(asides.len(), 1, "no duplicate aside on re-pull: {asides:?}");
    assert_eq!(engine.read("Foo"), b"UPPER Foo bytes");
}

/// FINDING B (case-sensitive fs) — when the filesystem distinguishes case, `Foo`
/// and `foo` are two distinct real paths; both install normally with no spurious
/// asides. Skips cleanly on a case-insensitive volume.
#[test]
fn case_fold_create_collision_installs_both_on_case_sensitive_fs() {
    let mut engine = TestEngine::new("case-fold-create-sensitive");
    if is_case_insensitive_dir(&engine.root()) {
        eprintln!("skip: case-insensitive filesystem (both cannot coexist)");
        return;
    }
    let upper = engine.remote_file(b"UPPER Foo bytes");
    let lower = engine.remote_file(b"lower foo bytes");
    engine.publish(&[("Foo", upper), ("foo", lower)]);

    let outcome = engine.pull();

    assert_eq!(engine.read("Foo"), b"UPPER Foo bytes");
    assert_eq!(engine.read("foo"), b"lower foo bytes");
    assert!(
        outcome.conflict_asides.is_empty(),
        "no spurious asides on a case-sensitive fs: {:?}",
        outcome.conflict_asides
    );
    assert!(engine.files().contains_key(&wp("Foo")));
    assert!(engine.files().contains_key(&wp("foo")));
    assert!(aside_names(&engine.root()).is_empty());
}

#[test]
fn row_ancestor_present_local_changed_remote_deleted_keeps_local_no_aside() {
    let mut engine = TestEngine::new("row-changed-vs-delete");
    engine.write("keep.txt", b"original");
    engine.push(&["keep.txt"]);
    engine.write("keep.txt", b"changed locally");
    engine.publish(&[]); // remote deleted it

    let outcome = engine.pull();
    assert_eq!(engine.read("keep.txt"), b"changed locally");
    assert!(!engine.files().contains_key(&wp("keep.txt")));
    assert!(outcome.push_again.contains(&wp("keep.txt")));
    assert!(outcome.conflict_asides.is_empty());
}

#[test]
fn remote_delta_verification_detects_bytes_behind_an_equal_fingerprint() {
    let mut engine = TestEngine::new("row-equal-fingerprint");
    engine.write("same.txt", b"original");
    engine.push(&["same.txt"]);
    let mut ancestor = engine.files()[&wp("same.txt")].clone();

    engine.write("same.txt", b"changed!");
    let observed = engine.observe("same.txt").expect("changed file");
    ancestor.fingerprint = observed.fingerprint;
    ancestor.size = observed.size;

    let delta = local_vs_record(&engine.ctx, &wp("same.txt"), observed, &ancestor, true)
        .expect("local verification succeeds");
    assert!(matches!(
        delta,
        LocalDelta::Changed {
            content_id: Some(content_id),
            ..
        } if content_id == engine.content_id(b"changed!")
    ));
}

#[test]
fn row_ancestor_present_local_deleted_remote_changed_keeps_deletion_asides_remote() {
    let mut engine = TestEngine::new("row-delete-vs-change");
    engine.write("gone.txt", b"original");
    engine.push(&["gone.txt"]);
    engine.remove("gone.txt");
    let entry = engine.remote_file(b"remote changed");
    engine.publish(&[("gone.txt", entry)]);

    let outcome = engine.pull();
    assert!(!engine.exists("gone.txt")); // deletion kept
    let aside = outcome.conflict_asides.iter().next().expect("one aside");
    assert_eq!(engine.read(aside.as_str()), b"remote changed");
}

#[test]
fn row_ancestor_present_local_changed_remote_same_bytes_adopts_silently() {
    let mut engine = TestEngine::new("row-converged");
    engine.write("conv.txt", b"original");
    engine.push(&["conv.txt"]);
    engine.write("conv.txt", b"converged");
    // Same bytes AND the same full st_mode the local write produced (0o100644): the
    // fixture default 0o644 omits the type bits and would look mode-changed, routing
    // a genuinely converged file through the mode-divergence path instead of adopt.
    let entry = full_mode_file(&engine, b"converged");
    engine.publish(&[("conv.txt", entry)]);

    let outcome = engine.pull();
    assert!(outcome.conflict_asides.is_empty());
    assert!(
        outcome.push_again.is_empty(),
        "a converged file needs no re-push"
    );
    assert_eq!(engine.read("conv.txt"), b"converged");
    assert_eq!(
        engine.files()[&wp("conv.txt")].content_id,
        Some(engine.content_id(b"converged"))
    );
    // The adopt seals ancestor == disk: the stored mode is the real on-disk mode.
    assert_eq!(
        engine.files()[&wp("conv.txt")].mode,
        FileMode::new(0o100644)
    );
    assert_eq!(engine.mode_bits("conv.txt"), 0o644);
}

#[test]
fn row_ancestor_present_local_changed_remote_different_asides_remote() {
    let mut engine = TestEngine::new("row-both-changed");
    engine.write("div.txt", b"original");
    engine.push(&["div.txt"]);
    engine.write("div.txt", b"local change");
    let entry = engine.remote_file(b"remote change");
    engine.publish(&[("div.txt", entry)]);

    let outcome = engine.pull();
    assert_eq!(engine.read("div.txt"), b"local change");
    let aside = outcome.conflict_asides.iter().next().expect("one aside");
    assert_eq!(engine.read(aside.as_str()), b"remote change");
    assert!(outcome.push_again.contains(&wp("div.txt")));
}

#[test]
fn row_ancestor_present_local_deleted_remote_deleted_adopts_deletion() {
    let mut engine = TestEngine::new("row-both-deleted");
    engine.write("both.txt", b"content");
    engine.push(&["both.txt"]);
    engine.remove("both.txt");
    engine.publish(&[]); // remote also removed it

    engine.pull();
    assert!(!engine.exists("both.txt"));
    assert!(!engine.files().contains_key(&wp("both.txt")));
}

#[test]
fn row_ancestor_present_mode_only_change_applies_mode() {
    let mut engine = TestEngine::new("row-mode");
    engine.write("mode.txt", b"content");
    engine.push(&["mode.txt"]);
    let entry = mode_variant(&engine, "mode.txt", 0o755);
    engine.publish(&[("mode.txt", entry)]);

    engine.pull();
    assert_eq!(engine.mode_bits("mode.txt"), 0o755);
    assert_eq!(engine.read("mode.txt"), b"content");
}

// ---- same-content mode-divergence rows (merge-matrix mode resolution) -------

#[test]
fn same_content_remote_mode_change_lands_on_disk_and_ancestor() {
    // Local and remote converged on the SAME bytes, but the remote entry carries a
    // different mode. Once content agrees the remote mode is authoritative (as in
    // the (Unchanged, ModeChanged) row): it must land on disk AND in the ancestor
    // in one apply pass. The old adopt branch sealed the remote mode into the
    // ancestor while leaving disk at the local mode — a half-adoption the next scan
    // misreads as a spurious mode change.
    let mut engine = TestEngine::new("mode-converge-remote-wins");
    engine.write("f", b"base");
    engine.push(&["f"]); // ancestor {base, 0o100644}

    // Local edits the content (mode stays 0o100644): a local content change.
    engine.write("f", b"shared");
    // The remote publishes the same bytes at a DIFFERENT full st_mode.
    let entry = full_mode_file_at(&engine, b"shared", 0o100600);
    engine.publish(&[("f", entry)]);

    let outcome = engine.pull();
    assert!(outcome.conflict_asides.is_empty());
    assert_eq!(engine.read("f"), b"shared", "content untouched");
    assert_eq!(
        engine.mode_bits("f"),
        0o600,
        "the remote mode landed on disk, not left at the local mode"
    );
    assert_eq!(
        engine.files()[&wp("f")].mode,
        FileMode::new(0o100600),
        "the ancestor records the applied mode: ancestor == disk"
    );
}

#[test]
fn both_chmod_identical_content_local_mode_wins_and_peer_converges() {
    use crate::sync::manifest_engine::push::PushOutcome;
    // Two devices chmod identical content to DIFFERENT modes. Local's deliberate
    // chmod is the documented winner: it survives the pull, is re-queued, and the
    // follow-on push republishes exactly it. A peer that made no competing mode
    // change converges to the winner on its next pull.
    let mut winner = TestEngine::new("mode-conflict-winner");
    winner.write("f", b"content");
    winner.push(&["f"]); // ancestor {content, 0o100644}
    chmod(&winner, "f", 0o600); // winner's deliberate chmod -> disk 0o100600

    // The remote already carries a peer's DIFFERENT chmod of the same bytes.
    let peer_chmod = mode_variant(&winner, "f", 0o100700);
    winner.publish(&[("f", peer_chmod)]);

    let outcome = winner.pull();
    assert!(outcome.conflict_asides.is_empty());
    assert_eq!(
        winner.mode_bits("f"),
        0o600,
        "local's deliberate mode survives"
    );
    assert!(
        outcome.push_again.contains(&wp("f")),
        "the winning local mode is re-queued for push"
    );

    // The follow-on push publishes exactly the winner's mode; ancestor == disk.
    assert!(matches!(winner.push(&["f"]), PushOutcome::Advanced { .. }));
    assert_eq!(
        winner.files()[&wp("f")].mode,
        FileMode::new(0o100600),
        "ancestor == disk == winner mode after the republish"
    );

    // Convergence from the peer's side: a device holding the same content it already
    // pushed at the losing mode adopts the winner's republished mode.
    let mut peer = TestEngine::new("mode-conflict-peer");
    peer.write("f", b"content");
    peer.push(&["f"]); // peer ancestor {content, 0o100644}, no chmod
    let winner_mode = mode_variant(&peer, "f", 0o100600);
    peer.publish(&[("f", winner_mode)]);
    peer.pull();
    assert_eq!(
        peer.mode_bits("f"),
        0o600,
        "the peer converges to the winner mode"
    );
    assert_eq!(
        peer.files()[&wp("f")].mode,
        FileMode::new(0o100600),
        "peer ancestor == disk == winner mode"
    );
}

#[test]
fn both_chmod_to_same_mode_adopts_without_push() {
    use crate::sync::manifest_engine::push::PushOutcome;
    // Both devices chmod identical content to the SAME mode: fully converged. The
    // pull adopts the agreed mode into the ancestor (disk already holds it) with no
    // fs op and no re-push, and a follow-on push seals nothing. The old code lumped
    // this into local-ahead and echo-pushed a manifest for a no-op.
    let mut engine = TestEngine::new("mode-converge-same");
    engine.write("f", b"content");
    engine.push(&["f"]); // ancestor {content, 0o100644}
    chmod(&engine, "f", 0o600); // disk 0o100600

    // The remote carries the same bytes at the same mode the local settled on.
    let entry = mode_variant(&engine, "f", 0o100600);
    engine.publish(&[("f", entry)]);

    let before = engine.remote.events().len();
    let outcome = engine.pull();
    assert!(outcome.conflict_asides.is_empty());
    assert!(
        outcome.push_again.is_empty(),
        "a converged mode needs no re-push"
    );
    assert_eq!(
        engine.files()[&wp("f")].mode,
        FileMode::new(0o100600),
        "ancestor == disk == the agreed mode"
    );
    assert_eq!(engine.mode_bits("f"), 0o600);

    // Nothing was sealed by the pull, and a follow-on push publishes nothing.
    let during = &engine.remote.events()[before..];
    assert!(
        !during
            .iter()
            .any(|event| matches!(event, Event::PutManifest(_))),
        "an adopt seals no manifest"
    );
    assert!(matches!(engine.push(&["f"]), PushOutcome::NoChange { .. }));
}

// ---- essential common path + apply guards ----------------------------------

#[test]
fn install_applies_remote_update_when_local_unchanged() {
    let mut engine = TestEngine::new("peer-update");
    engine.write("f.txt", b"v1");
    engine.push(&["f.txt"]);
    let entry = engine.remote_file(b"v2 from peer");
    engine.publish(&[("f.txt", entry)]);

    let outcome = engine.pull();
    assert_eq!(engine.read("f.txt"), b"v2 from peer");
    assert!(outcome.installed.contains(&wp("f.txt")));
}

#[test]
fn mode_only_change_moves_no_content() {
    let mut engine = TestEngine::new("mode-no-content");
    engine.write("m.txt", b"body");
    engine.push(&["m.txt"]);
    let entry = mode_variant(&engine, "m.txt", 0o600);
    engine.publish(&[("m.txt", entry)]);

    let before = engine.remote.events().len();
    engine.pull();
    let during = &engine.remote.events()[before..];
    assert!(
        !during
            .iter()
            .any(|event| matches!(event, Event::GetBlob(_))),
        "a mode-only change downloads no blob"
    );
    assert_eq!(engine.mode_bits("m.txt"), 0o600);
}

#[test]
fn symlink_recreated_never_followed() {
    let mut engine = TestEngine::new("symlink");
    let entry = ManifestEntry::Symlink {
        mode: FileMode::new(0o777),
        target: "does/not/exist.txt".to_string(),
    };
    engine.publish(&[("link", entry)]);

    engine.pull();
    let meta = std::fs::symlink_metadata(engine.root().join("link")).expect("symlink");
    assert!(meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(engine.root().join("link")).expect("readlink"),
        std::path::Path::new("does/not/exist.txt")
    );
}

#[test]
fn deletion_does_not_resurrect() {
    let mut engine = TestEngine::new("no-resurrect");
    engine.write("d.txt", b"content");
    engine.push(&["d.txt"]);
    engine.publish(&[]); // remote deletes it
    engine.pull();
    assert!(!engine.exists("d.txt"));

    // A second pull of the same (deletion) head must not recreate the file.
    engine.pull();
    assert!(!engine.exists("d.txt"));
    assert!(!engine.files().contains_key(&wp("d.txt")));
}

#[test]
fn same_path_write_between_intent_and_install_is_preserved() {
    // Drives the apply-transaction guard directly: the engine decided to install
    // (expecting an absent target), but a user write landed first. The mutation
    // must keep the user's bytes and materialize the remote as an aside.
    let mut engine = TestEngine::new("apply-race");
    engine.write("race.txt", b"user wrote this after we planned an install");
    let entry = engine.remote_file(b"remote install bytes");
    let applied = apply_install_expecting_absent(&mut engine, "race.txt", entry, "m_race");

    assert!(matches!(applied, Applied::Aside(_)));
    assert_eq!(
        engine.read("race.txt"),
        b"user wrote this after we planned an install"
    );
}

#[test]
fn install_applies_manifest_mode_not_temp_mode() {
    // Regression (rehost map, "Engine finding surfaced by Step 4"): the staging
    // temp is written 0600, but the installed file must carry the manifest
    // entry's mode. Otherwise the ancestor row (which records the entry mode)
    // and the on-disk mode diverge, so the next pull reads the file back as a
    // local mode change and conflict-asides a peer edit instead of installing it.
    // Production entry modes are the full st_mode from `metadata.permissions()`
    // (push.rs), so the test uses `0o100644` to match what `observe` reads back.
    let mut engine = TestEngine::new("install-mode");
    let entry = full_mode_file(&engine, b"remote bytes");
    engine.publish(&[("f.txt", entry)]);
    engine.pull();

    // Installed with the manifest mode, not the 0600 temp mode.
    assert_eq!(engine.mode_bits("f.txt"), 0o644);

    // A peer edits the SAME file; the local mode still matches the ancestor, so
    // this is a clean install (local Unchanged), never a mode-driven aside.
    let updated = full_mode_file(&engine, b"peer edit");
    engine.publish(&[("f.txt", updated)]);
    let outcome = engine.pull();
    assert!(
        outcome.conflict_asides.is_empty(),
        "a materialized-then-peer-edited file installs cleanly, no spurious mode aside"
    );
    assert_eq!(engine.read("f.txt"), b"peer edit");
    assert!(outcome.installed.contains(&wp("f.txt")));
}

// ---- Git contract ----------------------------------------------------------

#[test]
fn object_before_ref_apply() {
    // Ranking: objects rank below refs/HEAD/index within a repo, so apply_plan
    // materializes objects first.
    assert!(git_apply_rank(".git/objects/ab/cdef") < git_apply_rank(".git/refs/heads/main"));
    assert!(git_apply_rank(".git/objects/ab/cdef") < git_apply_rank(".git/HEAD"));
    assert!(git_apply_rank(".git/objects/ab/cdef") < git_apply_rank(".git/index"));

    let mut engine = TestEngine::new("git-order");
    let object = engine.remote_file(b"loose object bytes");
    let reference = engine.remote_file(b"ref pointer bytes");
    engine.publish(&[
        (".git/refs/heads/main", reference),
        (".git/objects/ab/cdef", object),
    ]);

    let before = engine.remote.events().len();
    engine.pull();
    let events = engine.remote.events();
    let first_get = events[before..]
        .iter()
        .position(|event| matches!(event, Event::GetBlob(_)))
        .map(|index| index + before)
        .expect("object downloaded first");
    let last_get = events[before..]
        .iter()
        .rposition(|event| matches!(event, Event::GetBlob(_)))
        .map(|index| index + before)
        .expect("ref downloaded second");
    assert!(first_get < last_get, "the object is applied before the ref");
}

#[test]
fn git_add_races_remote_index_apply() {
    // A `git add` rewrites `.git/index` while we apply a remote index change. The
    // preimage re-observation must keep the local index and aside the remote.
    let mut engine = TestEngine::new("git-index-race");
    engine.write(".git/index", b"index written by a racing git add");
    let entry = engine.remote_file(b"remote index bytes");
    let applied = apply_install_expecting_absent(&mut engine, ".git/index", entry, "m_idx");
    assert!(matches!(applied, Applied::Aside(_)));
    assert_eq!(
        engine.read(".git/index"),
        b"index written by a racing git add"
    );
}

#[test]
fn kill9_with_index_lock_present() {
    // An active `.git/index.lock` means git is mid-operation: defer that repo's
    // paths (auto-rescan after the lock clears), never apply into a live repo.
    let mut engine = TestEngine::new("git-lock");
    engine.write(".git/index.lock", b"");
    let entry = engine.remote_file(b"new ref bytes");
    engine.publish(&[(".git/refs/heads/main", entry)]);

    let outcome = engine.pull();
    assert!(outcome.deferred.contains(&wp(".git/refs/heads/main")));
    assert!(!engine.exists(".git/refs/heads/main"));
}

// ---- recovery boundaries (pure classification) -----------------------------

/// Named recovery facts for the boundary tests. A struct with `Default` keeps
/// each case readable (only the true facts are named) and avoids a five-bool
/// positional helper (`clippy::fn-params-excessive-bools`).
#[derive(Default)]
struct RecoveryFacts {
    target_present: bool,
    target_matches_target_record: bool,
    target_matches_preimage: bool,
    temp_exists: bool,
    quarantine_exists: bool,
}

fn observation(facts: RecoveryFacts) -> RecoveryObservation {
    RecoveryObservation {
        target_present: facts.target_present,
        target_matches_target_record: facts.target_matches_target_record,
        target_matches_preimage: facts.target_matches_preimage,
        temp_exists: facts.temp_exists,
        quarantine_exists: facts.quarantine_exists,
    }
}

#[test]
fn recovery_boundary_temp_only_discards() {
    let observed = observation(RecoveryFacts {
        temp_exists: true,
        ..Default::default()
    });
    let boundary = recovery_boundary(IntentOperationKind::Install, &observed);
    assert_eq!(boundary, RecoveryBoundary::TempOnly);
    assert_eq!(recovery_action(boundary), RecoveryAction::DiscardTemp);
}

#[test]
fn recovery_boundary_intent_old_target_reapplies() {
    let observed = observation(RecoveryFacts {
        target_present: true,
        target_matches_preimage: true,
        temp_exists: true,
        ..Default::default()
    });
    let boundary = recovery_boundary(IntentOperationKind::Install, &observed);
    assert_eq!(boundary, RecoveryBoundary::IntentOldTarget);
    assert_eq!(recovery_action(boundary), RecoveryAction::Reapply);
}

#[test]
fn recovery_boundary_installed_intent_finalizes() {
    let observed = observation(RecoveryFacts {
        target_present: true,
        target_matches_target_record: true,
        ..Default::default()
    });
    let boundary = recovery_boundary(IntentOperationKind::Install, &observed);
    assert_eq!(boundary, RecoveryBoundary::InstalledIntent);
    assert_eq!(recovery_action(boundary), RecoveryAction::FinalizeInstalled);
}

#[test]
fn recovery_boundary_preserved_no_target_restores() {
    let observed = observation(RecoveryFacts {
        quarantine_exists: true,
        ..Default::default()
    });
    let boundary = recovery_boundary(IntentOperationKind::Install, &observed);
    assert_eq!(boundary, RecoveryBoundary::PreservedNoTarget);
    assert_eq!(recovery_action(boundary), RecoveryAction::RestoreOrComplete);
}

#[test]
fn recovery_boundary_delete_done_intent_finalizes() {
    let observed = observation(RecoveryFacts::default());
    let boundary = recovery_boundary(IntentOperationKind::Delete, &observed);
    assert_eq!(boundary, RecoveryBoundary::DeleteDoneIntent);
    assert_eq!(recovery_action(boundary), RecoveryAction::FinalizeDeleted);
}

#[test]
fn recovery_boundary_target_modified_while_down_keeps_local() {
    let observed = observation(RecoveryFacts {
        target_present: true,
        ..Default::default()
    });
    let boundary = recovery_boundary(IntentOperationKind::Install, &observed);
    assert_eq!(boundary, RecoveryBoundary::TargetModifiedWhileDown);
    assert_eq!(recovery_action(boundary), RecoveryAction::KeepLocalAside);
}

// ---- recovery integration --------------------------------------------------

#[test]
fn recover_intents_finalizes_an_installed_target_and_clears_the_journal() {
    let mut engine = TestEngine::new("recover-installed");
    // Establish an applied head so recovery has a manifest key to commit under.
    engine.write("seed.txt", b"seed");
    engine.push(&["seed.txt"]);

    // Simulate an interrupted install: the file is on disk (installed) but its
    // intent was never cleared because the outcome transaction was lost.
    let bytes = b"already installed bytes";
    engine.write("recovered.txt", bytes);
    let entry = engine.remote_file(bytes);
    let target_record = install_target_record(&entry);
    let intent = Intent {
        path: wp("recovered.txt"),
        operation_kind: IntentOperationKind::Install,
        temp_name: None,
        expected_preimage: Some(serde_json::to_string(&PreimagePayload::absent()).expect("encode")),
        target_record: Some(target_record),
        preserved_preimage: None,
        target_manifest_key: engine.remote.current_ref().map(|head| head.manifest_key),
        created_at: 1,
    };
    engine.store.open_intent(&intent).expect("open intent");

    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    super::recover_intents(&mut engine.store, &deps).expect("recover");

    // Journal cleared; the file survived; ancestor now records it.
    assert!(engine.store.pending_intents().expect("intents").is_empty());
    assert_eq!(engine.read("recovered.txt"), bytes);
    assert!(engine.files().contains_key(&wp("recovered.txt")));
}

// ---- symlinked-parent workspace escape (P1 security guard) -----------------

// A sealed manifest from an authorized peer can name `dir/file` while local
// `dir` is a symlink pointing OUTSIDE the workspace. Applying that entry must
// never write, delete, or chmod through the symlink; every case must keep the
// external target untouched and land in the engine's normal keep-local
// divergence (Applied::KeptLocal), never a fatal.

/// A directory fully OUTSIDE the engine's workspace root (its own temp dir), the
/// symlink target an escape would land in. Held by the caller so Drop cleans it.
fn external_dir(name: &str) -> crate::workspace::TempWorkspace {
    crate::workspace::TempWorkspace::new(name).expect("external temp dir")
}

fn read_dir_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| entries.count())
        .unwrap_or(0)
}

fn is_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// Apply exactly one fs op through the production apply transaction.
fn apply_single_op(engine: &mut TestEngine, op: FsOp, manifest_key: &str) -> Applied {
    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    apply_op(
        &mut engine.store,
        &deps,
        &op,
        &ManifestKey::new(manifest_key),
    )
    .expect("apply op")
}

#[test]
fn install_through_symlinked_parent_does_not_escape_workspace() {
    use std::os::unix::fs::symlink;
    let mut engine = TestEngine::new("symlink-parent-install");
    let external = external_dir("symlink-parent-install-ext");
    symlink(external.root(), engine.root().join("dir")).expect("symlink dir");

    // Remote adds `dir/file`; local `dir` is a symlink escaping the root.
    let entry = engine.remote_file(b"remote payload that must not escape");
    let op = FsOp {
        path: wp("dir/file"),
        kind: FsOpKind::Install(entry),
        expected: PreimagePayload::absent(),
    };
    let applied = apply_single_op(&mut engine, op, "m_sym_install");

    assert!(
        matches!(applied, Applied::KeptLocal(_)),
        "a symlinked parent blocks the install and keeps local"
    );
    assert_eq!(
        read_dir_count(external.root()),
        0,
        "nothing was written into the external directory through the symlink"
    );
    assert!(
        is_symlink(&engine.root().join("dir")),
        "the local symlink is intact, never replaced by a real directory"
    );
}

#[test]
fn install_through_nested_symlinked_parent_does_not_escape() {
    use std::os::unix::fs::symlink;
    let mut engine = TestEngine::new("symlink-parent-nested");
    let external = external_dir("symlink-parent-nested-ext");
    // The symlink is TWO levels above the target (`a` -> external, remote a/b/c).
    symlink(external.root(), engine.root().join("a")).expect("symlink a");

    let entry = engine.remote_file(b"deep payload that must not escape");
    let op = FsOp {
        path: wp("a/b/deep.txt"),
        kind: FsOpKind::Install(entry),
        expected: PreimagePayload::absent(),
    };
    let applied = apply_single_op(&mut engine, op, "m_sym_nested");

    assert!(matches!(applied, Applied::KeptLocal(_)));
    assert_eq!(
        read_dir_count(external.root()),
        0,
        "no `b/` subtree was created inside the external directory"
    );
    assert!(is_symlink(&engine.root().join("a")));
}

#[test]
fn conflict_aside_through_symlinked_parent_does_not_escape() {
    use std::os::unix::fs::symlink;
    let mut engine = TestEngine::new("symlink-parent-aside");
    let external = external_dir("symlink-parent-aside-ext");
    symlink(external.root(), engine.root().join("dir")).expect("symlink dir");

    // A conflict-aside op targets `dir/file`; the aside shares the symlinked
    // parent, so there is nowhere safe to place it — keep local, write nothing.
    let entry = engine.remote_file(b"remote conflict bytes that must not escape");
    let op = FsOp {
        path: wp("dir/file"),
        kind: FsOpKind::ConflictAside(entry),
        expected: PreimagePayload::absent(),
    };
    let applied = apply_single_op(&mut engine, op, "m_sym_aside");

    assert!(
        matches!(applied, Applied::KeptLocal(_)),
        "a blocked aside keeps local rather than escaping the root"
    );
    assert_eq!(
        read_dir_count(external.root()),
        0,
        "no aside file was materialized through the symlink"
    );
}

#[test]
fn checked_delete_through_symlinked_parent_does_not_delete_outside() {
    use std::os::unix::fs::symlink;
    let mut engine = TestEngine::new("symlink-parent-delete");
    let external = external_dir("symlink-parent-delete-ext");
    // An external file a naive delete-through-symlink would unlink.
    let survivor = b"external bytes that must survive";
    std::fs::write(external.root().join("file"), survivor).expect("seed external file");
    symlink(external.root(), engine.root().join("dir")).expect("symlink dir");

    // A Delete op whose expected preimage matches the file as seen through the
    // symlink, so the preimage re-observation passes and the guard is what stops
    // the unlink (not a preimage mismatch).
    let observed = engine.observe("dir/file").expect("observe through symlink");
    let expected = PreimagePayload::from_observed(&observed, Some(engine.content_id(survivor)));
    let op = FsOp {
        path: wp("dir/file"),
        kind: FsOpKind::Delete,
        expected,
    };
    let applied = apply_single_op(&mut engine, op, "m_sym_delete");

    assert!(
        matches!(applied, Applied::KeptLocal(_)),
        "a symlinked parent blocks the delete and keeps local"
    );
    assert!(
        external.root().join("file").exists(),
        "the external file was NOT deleted through the symlink"
    );
    assert_eq!(
        std::fs::read(external.root().join("file")).expect("read survivor"),
        survivor,
        "the external file's bytes are untouched"
    );
}

#[test]
fn recovery_install_through_symlinked_parent_does_not_escape() {
    use super::apply::{build_intent, recover_one};
    use crate::sync::manifest_engine::store::AncestorCommit;
    use std::collections::BTreeSet;
    use std::os::unix::fs::symlink;

    let mut engine = TestEngine::new("symlink-parent-recovery");
    let external = external_dir("symlink-parent-recovery-ext");
    symlink(external.root(), engine.root().join("dir")).expect("symlink dir");

    // An applied head so recovery has a manifest key to commit under.
    engine.write("seed.txt", b"seed");
    engine.push(&["seed.txt"]);

    // A committed-but-unfinished Install intent for a path under the symlink: the
    // crash-recovery replay path (reapply_target) must refuse to write through it.
    let entry = engine.remote_file(b"payload that must not escape via recovery");
    let op = FsOp {
        path: wp("dir/file"),
        kind: FsOpKind::Install(entry),
        expected: PreimagePayload::absent(),
    };
    let head = engine.remote.current_ref().expect("head");
    let intent = build_intent(&op, None, &head.manifest_key);

    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    let mut commit = AncestorCommit::default();
    let mut temps = BTreeSet::new();
    recover_one(&mut engine.store, &deps, &intent, &mut commit, &mut temps).expect("recover");

    assert_eq!(
        read_dir_count(external.root()),
        0,
        "the recovery replay did not write through the symlink"
    );
    assert!(
        !commit.upserts.contains_key(&wp("dir/file")),
        "a blocked replay records no ancestor row"
    );
}

// ---- Finding A: recursive directory delete must not destroy local work ------

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

// ---- Review Finding A: apply-time mode race deflects to keep-local ----------

/// Chmod a workspace-relative path to the exact `mode` bits.
fn chmod(engine: &TestEngine, rel: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        engine.root().join(rel),
        std::fs::Permissions::from_mode(mode),
    )
    .expect("chmod");
}

#[test]
fn file_chmod_between_plan_and_apply_deflects_and_local_mode_survives() {
    // The plan snapshots the preimage at the pre-chmod mode; a local chmod lands
    // before the mutation boundary. The mode diverged even though the bytes did
    // not, so the remote install must deflect (aside the remote, keep local) and
    // the concurrent permission change must survive — never be silently discarded.
    let mut engine = TestEngine::new("mode-race-file");
    engine.write("f.txt", b"content");
    chmod(&engine, "f.txt", 0o644);
    let observed = engine.observe("f.txt").expect("present");
    let expected = PreimagePayload::from_observed(&observed, Some(engine.content_id(b"content")));

    // The racing local chmod: same bytes, different mode.
    chmod(&engine, "f.txt", 0o600);

    let entry = engine.remote_file(b"remote bytes");
    let op = FsOp {
        path: wp("f.txt"),
        kind: FsOpKind::Install(entry),
        expected,
    };
    let applied = apply_single_op(&mut engine, op, "m_mode_file");

    assert!(
        matches!(applied, Applied::Aside(_)),
        "the raced mode change deflects the remote install to an aside"
    );
    assert_eq!(engine.read("f.txt"), b"content", "local bytes survive");
    assert_eq!(engine.mode_bits("f.txt"), 0o600, "local mode survives");
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

#[test]
fn matching_file_mode_applies_normally() {
    // The happy path is unchanged: when the on-disk mode still matches the plan
    // snapshot, the remote install applies (no spurious mode-driven deflection).
    let mut engine = TestEngine::new("mode-match-file");
    engine.write("f.txt", b"content");
    chmod(&engine, "f.txt", 0o644);
    let observed = engine.observe("f.txt").expect("present");
    let expected = PreimagePayload::from_observed(&observed, Some(engine.content_id(b"content")));

    // No racing chmod: the mode still matches.
    let entry = engine.remote_file(b"remote bytes");
    let op = FsOp {
        path: wp("f.txt"),
        kind: FsOpKind::Install(entry),
        expected,
    };
    let applied = apply_single_op(&mut engine, op, "m_mode_match");

    assert!(
        matches!(applied, Applied::Upsert(_, _)),
        "a matching mode applies the remote install normally"
    );
    assert_eq!(engine.read("f.txt"), b"remote bytes");
}

// ---- Review Finding B: set_mode never chmods through a symlink leaf ----------

fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn set_mode_never_follows_a_symlink_leaf_to_an_external_target() {
    use super::materialize::set_mode;
    use std::os::unix::fs::symlink;

    let engine = TestEngine::new("set-mode-symlink-leaf");
    let external = external_dir("set-mode-symlink-leaf-ext");
    let target = external.root().join("secret");
    std::fs::write(&target, b"external bytes").expect("seed external");
    chmod_path(&target, 0o600);

    // The leaf is a symlink to the external target: a path-resolving chmod would
    // clobber the target's mode. set_mode must refuse to follow it.
    symlink(&target, engine.root().join("link")).expect("symlink leaf");

    set_mode(&engine.root(), &wp("link"), FileMode::new(0o777)).expect("set_mode is not fatal");

    assert_eq!(
        mode_of(&target),
        0o600,
        "the external symlink target's mode is untouched"
    );
    assert!(
        is_symlink(&engine.root().join("link")),
        "the leaf is still the symlink, never chmod'd or replaced"
    );
}

#[test]
fn set_mode_chmods_a_regular_file_and_directory() {
    use super::materialize::set_mode;

    let engine = TestEngine::new("set-mode-normal");
    engine.write("f.txt", b"body");
    set_mode(&engine.root(), &wp("f.txt"), FileMode::new(0o600)).expect("chmod file");
    assert_eq!(engine.mode_bits("f.txt"), 0o600, "regular file chmod works");

    std::fs::create_dir(engine.root().join("d")).expect("mkdir");
    set_mode(&engine.root(), &wp("d"), FileMode::new(0o700)).expect("chmod dir");
    assert_eq!(engine.mode_bits("d"), 0o700, "directory chmod works");
}

#[test]
fn set_mode_never_chmods_external_target_under_a_racing_symlink_swap() {
    // The vulnerability is a TOCTOU: a `symlink_metadata` check followed by a
    // path-based `set_permissions` lets the leaf be swapped for a symlink between
    // the two syscalls, so the chmod follows it onto an EXTERNAL target. Hammer
    // that window: a swapper thread atomically alternates the leaf between a
    // regular file and a symlink to an external target while the main thread
    // chmods it. The fd-based `set_mode` (O_NOFOLLOW open + fchmod) can never
    // re-resolve the path, so the external target's mode must never change.
    use super::materialize::set_mode;
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let engine = TestEngine::new("set-mode-symlink-race");
    let external = external_dir("set-mode-symlink-race-ext");
    let target = external.root().join("secret");
    std::fs::write(&target, b"external bytes").expect("seed external");
    chmod_path(&target, 0o600);

    let root = engine.root();
    let leaf = root.join("leaf");
    let stage_reg = root.join(".leaf-reg");
    let stage_link = root.join(".leaf-link");
    std::fs::write(&leaf, b"x").expect("seed leaf");

    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (stop, leaf, stage_reg, stage_link, target) = (
            Arc::clone(&stop),
            leaf.clone(),
            stage_reg.clone(),
            stage_link.clone(),
            target.clone(),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Atomic rename each staged object onto the leaf: the leaf is always
                // either the regular file or the symlink, never absent.
                let _ = std::fs::write(&stage_reg, b"x");
                let _ = std::fs::rename(&stage_reg, &leaf);
                let _ = std::fs::remove_file(&stage_link);
                if symlink(&target, &stage_link).is_ok() {
                    let _ = std::fs::rename(&stage_link, &leaf);
                }
            }
        })
    };

    for _ in 0..200_000 {
        // Errors (a vanished/raced leaf) are fine; the invariant is only that the
        // EXTERNAL target is never chmod'd through a followed symlink.
        let _ = set_mode(&root, &wp("leaf"), FileMode::new(0o777));
        assert_eq!(
            mode_of(&target),
            0o600,
            "set_mode followed a raced symlink leaf and chmod'd the external target"
        );
    }
    stop.store(true, Ordering::Relaxed);
    swapper.join().expect("swapper thread");
}

/// Chmod an absolute path (an external target outside the workspace).
fn chmod_path(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod path");
}

// ---- Finding C: entry-kind replacement (file↔directory↔symlink) -------------

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

// ---- Finding B: first-pull Git-lock deferral must not commit the head -------

#[test]
fn first_pull_deferred_by_git_lock_materializes_after_lock_clears() {
    // A FIRST pull (no prior applied head) with an active Git lock defers the
    // path. It must NOT record the incoming head as applied — otherwise the next
    // pull short-circuits at `already_current` and the deferred path never lands.
    let mut engine = TestEngine::new("first-pull-git-lock");
    engine.write(".git/index.lock", b""); // git mid-operation
    let entry = engine.remote_file(b"new ref bytes");
    engine.publish(&[(".git/refs/heads/main", entry)]);

    let outcome = engine.pull();
    assert!(outcome.deferred.contains(&wp(".git/refs/heads/main")));
    assert!(!engine.exists(".git/refs/heads/main"));
    assert!(
        engine
            .store
            .engine_state()
            .expect("state")
            .applied_manifest_key
            .is_none(),
        "a first pull that deferred everything records no applied head"
    );

    // The lock clears; the next pull must re-derive and materialize the path.
    engine.remove(".git/index.lock");
    let outcome = engine.pull();
    assert!(
        !outcome.already_current,
        "the deferred head is not already current"
    );
    assert_eq!(engine.read(".git/refs/heads/main"), b"new ref bytes");
    assert!(
        engine
            .store
            .engine_state()
            .expect("state")
            .applied_manifest_key
            .is_some(),
        "once nothing defers, the head is recorded applied"
    );
}

// ---- Ratchet P1 Finding A: push must advance the freshness ratchet ----------

/// A push CAS to version N is a verified observation of the hosted head. If the
/// push does not ratchet it, a device that only ever pushes (never pulled) has an
/// empty ratchet, and a hosted rollback to a LOWER version carrying different bytes
/// passes `enforce_freshness` and reverts the very state this device published.
/// RED before the fix: the rollback applies and `f.txt` is overwritten; GREEN
/// after: the pull refuses on the freshness path and local state is untouched.
#[test]
fn hosted_rollback_below_pushed_version_is_refused() {
    use crate::sync::manifest_engine::push::PushOutcome;

    let mut engine = TestEngine::new("finding-a-rollback");
    engine.write("f.txt", b"first");
    let first = engine.push(&["f.txt"]);
    engine.write("f.txt", b"second");
    let second = engine.push(&["f.txt"]);
    let (
        PushOutcome::Advanced {
            ref_version: v1, ..
        },
        PushOutcome::Advanced {
            ref_version: v2, ..
        },
    ) = (first, second)
    else {
        panic!("both pushes advanced");
    };
    assert!(v1 < v2, "the second push advanced past the first");

    // Forge a hosted rollback: an authentic manifest that would revert f.txt,
    // published then re-pointed to a version BELOW the ratchet the pushes set.
    let evil = engine.remote_file(b"rolled back bytes");
    let evil_key = engine.publish(&[("f.txt", evil)]);
    engine.remote.force_ref(v1, evil_key);

    let err = engine.try_pull().expect_err("rollback must be refused");
    assert!(
        matches!(err, PullError::RefRegressed { observed, highest } if observed == v1 && highest == v2),
        "expected a freshness regression, got {err:?}"
    );
    assert_eq!(
        engine.read("f.txt"),
        b"second",
        "local state this device published is not reverted"
    );
}

/// An ABA hosted sequence (this device's manifest key re-pointed at a NEWER
/// version while it was offline) must persist the newer version on the
/// already-current fast path — otherwise every later push CASes against the
/// stale stored version, loses, re-pulls the same key, and livelocks.
#[test]
fn already_current_persists_a_newer_ref_version() {
    use crate::sync::manifest_engine::push::PushOutcome;

    let mut engine = TestEngine::new("aba-already-current");
    engine.write("f.txt", b"mine");
    let PushOutcome::Advanced {
        manifest_key,
        ref_version,
        ..
    } = engine.push(&["f.txt"])
    else {
        panic!("genesis push advanced");
    };

    // Hosted ABA while offline: the same manifest key re-presented at a newer
    // version (another device pushed B then restored A's manifest).
    let newer = ref_version + 2;
    engine.remote.force_ref(newer, manifest_key.clone());
    let outcome = engine.pull();
    assert!(outcome.already_current, "same key is already current");
    assert_eq!(outcome.ref_version, Some(newer));
    let state = engine.store.engine_state().expect("state");
    assert_eq!(
        state.last_ref_version,
        Some(newer),
        "the newer version is persisted, not just returned in memory"
    );

    // The next local edit must publish by CASing against the NEWER version.
    engine.write("f.txt", b"mine again");
    assert!(
        matches!(engine.push(&["f.txt"]), PushOutcome::Advanced { .. }),
        "a later push CASes against the persisted newer version"
    );
}

// ---- Ratchet P1 Finding B: ratchet advances only after the head verifies ----

/// A forged high-version ref whose manifest object is missing must NOT freeze the
/// ratchet. Before the fix the ratchet was persisted BEFORE the fetch, so the
/// missing object left it stuck at 999 and every legitimate lower head afterward
/// read as regressed. After the fix nothing is persisted on a fetch failure and a
/// later legitimate head still applies.
#[test]
fn missing_high_version_manifest_leaves_ratchet_unchanged() {
    let mut engine = TestEngine::new("finding-b-missing-manifest");
    // A ref claiming version 999 whose manifest was never uploaded.
    engine
        .remote
        .force_ref(999, ManifestKey::new("m_never_uploaded"));
    let err = engine.try_pull().expect_err("missing manifest errors");
    assert!(
        matches!(err, PullError::Transport(_)),
        "a missing manifest is a transport failure, got {err:?}"
    );
    assert_eq!(
        engine
            .store
            .engine_state()
            .expect("state")
            .highest_verified_ref_version,
        None,
        "a fetch that verified nothing left the ratchet untouched"
    );

    // A later LEGITIMATE head (lower version than the forged 999) still applies:
    // it would be rejected as regressed if 999 had frozen the ratchet.
    let entry = engine.remote_file(b"legit bytes");
    engine.publish(&[("real.txt", entry)]);
    let outcome = engine.pull();
    assert_eq!(engine.read("real.txt"), b"legit bytes");
    assert!(outcome.installed.contains(&wp("real.txt")));
}

/// After a successful pull the ratchet equals the applied version (the regression
/// lock): the head was fetched, authenticated, and decoded, so its version is now
/// the verified floor.
#[test]
fn successful_pull_advances_ratchet_to_applied_version() {
    let mut engine = TestEngine::new("finding-b-ratchet-locks");
    let entry = engine.remote_file(b"peer bytes");
    let key = engine.publish(&[("p.txt", entry)]);

    let outcome = engine.pull();
    let applied_version = outcome.ref_version.expect("a head was applied");
    let state = engine.store.engine_state().expect("state");
    assert_eq!(state.highest_verified_ref_version, Some(applied_version));
    assert_eq!(state.highest_verified_manifest_key, Some(key));
}

/// The ratchet is durable: it survives a store reopen (restart) and still refuses a
/// genuine regression below the persisted floor.
#[test]
fn verified_ratchet_survives_restart_and_refuses_regression() {
    let mut engine = TestEngine::new("finding-b-restart");
    let e1 = engine.remote_file(b"one");
    engine.publish(&[("a.txt", e1)]);
    engine.pull();
    let e2 = engine.remote_file(b"two");
    engine.publish(&[("a.txt", e2)]);
    let applied = engine.pull().ref_version.expect("second head applied");

    engine.reopen_store();
    assert_eq!(
        engine
            .store
            .engine_state()
            .expect("state")
            .highest_verified_ref_version,
        Some(applied),
        "the ratchet survives a restart"
    );

    // A rollback below the persisted floor is still refused after the restart.
    engine
        .remote
        .force_ref(applied - 1, ManifestKey::new("m_rolled_back"));
    let err = engine
        .try_pull()
        .expect_err("post-restart rollback refused");
    assert!(
        matches!(err, PullError::RefRegressed { observed, highest } if observed == applied - 1 && highest == applied),
        "expected a freshness regression after restart, got {err:?}"
    );
}

// ---- helpers ---------------------------------------------------------------

/// A remote entry with the same content as `rel`'s ancestor row but a different
/// mode (drives the mode-only rows without moving content).
fn mode_variant(engine: &TestEngine, rel: &str, mode: u32) -> ManifestEntry {
    let record = engine.files()[&wp(rel)].clone();
    ManifestEntry::File {
        size: record.size,
        mode: FileMode::new(mode),
        content_id: record.content_id.expect("content id"),
        blob_key: record.blob_key.expect("blob key"),
        key_epoch: record.key_epoch.expect("key epoch"),
    }
}

/// A remote file entry whose mode is the full regular-file st_mode (`0o100644`)
/// that production `push` records via `metadata.permissions().mode()`, so a
/// re-observe after install compares equal (the fixture default `0o644` omits
/// the type bits and would look mode-changed).
fn full_mode_file(engine: &TestEngine, plaintext: &[u8]) -> ManifestEntry {
    full_mode_file_at(engine, plaintext, 0o100644)
}

/// A remote file entry for freshly published `plaintext` at an explicit full
/// st_mode. Production records the type bits (`0o100xxx`), so a test that wants a
/// mode to compare equal against a real on-disk file must carry them too.
fn full_mode_file_at(engine: &TestEngine, plaintext: &[u8], mode: u32) -> ManifestEntry {
    match engine.remote_file(plaintext) {
        ManifestEntry::File {
            size,
            content_id,
            blob_key,
            key_epoch,
            ..
        } => ManifestEntry::File {
            size,
            mode: FileMode::new(mode),
            content_id,
            blob_key,
            key_epoch,
        },
        other => other,
    }
}

fn apply_install_expecting_absent(
    engine: &mut TestEngine,
    path: &str,
    entry: ManifestEntry,
    manifest_key: &str,
) -> Applied {
    let op = FsOp {
        path: wp(path),
        kind: FsOpKind::Install(entry),
        expected: PreimagePayload::absent(),
    };
    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
    };
    apply_op(
        &mut engine.store,
        &deps,
        &op,
        &ManifestKey::new(manifest_key),
    )
    .expect("apply op")
}

/// The `target_record` JSON an install intent would carry, built through the
/// production `target_payload` path (never hand-assembled).
fn install_target_record(entry: &ManifestEntry) -> String {
    let op = FsOp {
        path: wp("recovered.txt"),
        kind: FsOpKind::Install(entry.clone()),
        expected: PreimagePayload::absent(),
    };
    let (_kind, payload) = target_payload(&op);
    serde_json::to_string(&payload).expect("encode target record")
}
