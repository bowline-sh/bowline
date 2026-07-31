//! Crash-recovery tests for the intent journal (Plan 109 Step 5).
//!
//! Split from the merge-matrix suite in the sibling `tests` module at the seam
//! between *deciding* a pull and *repairing* an interrupted one. Two layers: the
//! six boundaries as pure-function checks over [`RecoveryObservation`], and
//! integration cases that journal a real intent, arrange the on-disk state a
//! crash would leave, and drive the production recovery path.
//!
//! The binding rule these integration cases exist to hold: an ordinary racing
//! filesystem state is NEVER a [`PullError::Internal`]. `Internal` classifies as
//! `CycleError::Fatal` and propagates out of `ManifestEngine::start`, and a
//! journalled intent replays on every restart — so one such error is not a
//! failed cycle, it is a device that can never start again.

use super::apply::{recovery_action, recovery_boundary};
use super::intents::{PreimagePayload, target_payload};
use super::{
    FsOp, FsOpKind, MergePlan, PullDeps, PullScope, RecoveryAction, RecoveryBoundary,
    RecoveryObservation,
};
use crate::sync::manifest_engine::engine_test_support::{
    DriverHarness, TestEngine, open_engine_store, plant_fifo,
};
use crate::sync::manifest_engine::manifest::{FileMode, ManifestEntry, WorkspacePath};
use crate::sync::manifest_engine::push::{EngineConfig, file_record_to_entry};
use crate::sync::manifest_engine::store::{
    AncestorCommit, FileRecord, Intent, IntentOperationKind,
};
use crate::sync::manifest_engine::unsyncable::UnsyncableReason;

fn wp(path: &str) -> WorkspacePath {
    WorkspacePath::new(path)
}

fn install_target_record(entry: &ManifestEntry) -> String {
    let op = FsOp {
        path: wp("recovered.txt"),
        kind: FsOpKind::Install(entry.clone()),
        expected: PreimagePayload::absent(),
    };
    let (_kind, payload) = target_payload(&op);
    serde_json::to_string(&payload).expect("encode target record")
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

        scope: PullScope::WholeAncestor,
    };
    super::recover_intents(&mut engine.store, &deps).expect("recover");

    // Journal cleared; the file survived; ancestor now records it.
    assert!(engine.store.pending_intents().expect("intents").is_empty());
    assert_eq!(engine.read("recovered.txt"), bytes);
    assert!(engine.files().contains_key(&wp("recovered.txt")));
}

#[test]
fn recovery_redownloads_a_segmented_file_when_the_staged_temp_was_lost() {
    let mut engine = TestEngine::with_config(
        "recover-segmented-without-temp",
        EngineConfig {
            large_file_threshold: 4,
            max_seal_bytes: 4096,
        },
    );
    let bytes = vec![0x6d; 512];
    engine.write("recovered.bin", &bytes);
    engine.push(&["recovered.bin"]);
    let record = engine
        .files()
        .get(&wp("recovered.bin"))
        .cloned()
        .expect("published record");
    let entry = file_record_to_entry(&record).expect("complete record");
    engine.remove("recovered.bin");

    let op = FsOp {
        path: wp("recovered.bin"),
        kind: FsOpKind::Install(entry),
        expected: PreimagePayload::absent(),
    };
    let (operation_kind, target) = target_payload(&op);
    engine
        .store
        .open_intent(&Intent {
            path: op.path,
            operation_kind,
            temp_name: None,
            expected_preimage: Some(
                serde_json::to_string(&PreimagePayload::absent()).expect("encode preimage"),
            ),
            target_record: Some(serde_json::to_string(&target).expect("encode target")),
            preserved_preimage: None,
            target_manifest_key: engine.remote.current_ref().map(|head| head.manifest_key),
            created_at: 1,
        })
        .expect("open intent");

    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
        scope: PullScope::WholeAncestor,
    };
    super::recover_intents(&mut engine.store, &deps).expect("recover segmented file");

    assert_eq!(engine.read("recovered.bin"), bytes);
    assert!(engine.store.pending_intents().expect("intents").is_empty());
}

// ---- a racing delete must never brick startup ------------------------------

/// The mode the journalled intent would have applied. Different from the mode
/// the file was pushed under, so a recovery that wrongly finalized would be
/// visible in the ancestor row.
const RECOVERY_MODE: u32 = 0o100_600;

const MODE_CHANGE_TARGET: &str = "work/file.dat";

/// Build the intent an apply writes just before it chmods `path`, retargeted to
/// `RECOVERY_MODE`. This is exactly what is on disk when a crash (or a power
/// loss) lands between the intent write and the chmod reaching the volume.
fn mode_change_intent(path: &str, record: &FileRecord) -> Intent {
    let mut entry = file_record_to_entry(record).expect("ancestor row is complete");
    if let ManifestEntry::File { mode, .. } = &mut entry {
        *mode = FileMode::new(RECOVERY_MODE);
    }
    let op = FsOp {
        path: wp(path),
        kind: FsOpKind::ModeChange(entry),
        expected: PreimagePayload::from_record(record),
    };
    let (operation_kind, target) = target_payload(&op);
    Intent {
        path: wp(path),
        operation_kind,
        temp_name: None,
        expected_preimage: Some(serde_json::to_string(&op.expected).expect("encode preimage")),
        target_record: Some(serde_json::to_string(&target).expect("encode target")),
        preserved_preimage: None,
        target_manifest_key: None,
        created_at: 1,
    }
}

/// A delete that races a journalled mode change is an ordinary filesystem state,
/// not a broken invariant. Recovery must retire the intent with the ancestor
/// untouched and let the next scan classify the deletion through the merge
/// matrix — the alternative (`PullError::Internal`) is unrecoverable, because
/// the intent is durable and replays identically on every restart.
#[test]
fn recover_intents_retires_a_mode_change_whose_target_vanished() {
    let mut engine = TestEngine::new("recover-mode-change-vanished");
    engine.write(MODE_CHANGE_TARGET, b"bytes");
    engine.push(&[MODE_CHANGE_TARGET]);
    let before = engine
        .files()
        .get(&wp(MODE_CHANGE_TARGET))
        .cloned()
        .expect("ancestor row after push");

    engine
        .store
        .open_intent(&mode_change_intent(MODE_CHANGE_TARGET, &before))
        .expect("open intent");
    engine.remove(MODE_CHANGE_TARGET);

    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
        scope: PullScope::WholeAncestor,
    };
    super::recover_intents(&mut engine.store, &deps).expect("recovery survives a vanished target");

    assert!(
        engine.store.pending_intents().expect("intents").is_empty(),
        "the intent is retired, so a restart does not replay it"
    );
    assert_eq!(
        engine.files().get(&wp(MODE_CHANGE_TARGET)),
        Some(&before),
        "the ancestor row is untouched: recovery invented no mode it never applied"
    );

    // The follow-on cycle converges: the deletion is now an ordinary
    // (Unchanged-ancestor, locally Deleted) row and publishes normally.
    engine.push(&[MODE_CHANGE_TARGET]);
    assert!(!engine.files().contains_key(&wp(MODE_CHANGE_TARGET)));
    assert!(engine.pull().already_current);
}

/// The same state, driven through the real startup path. `start` runs recovery
/// first, so a fatal there is a device that never comes up — and comes up no
/// better on the next restart.
#[test]
fn a_mode_change_intent_whose_target_vanished_does_not_brick_startup() {
    let mut harness = DriverHarness::new("startup-mode-change-vanished", "device-a");
    harness.start();
    harness.write(MODE_CHANGE_TARGET, b"bytes");
    harness.edit(&[MODE_CHANGE_TARGET]);

    {
        let mut store = open_engine_store(&harness.root);
        let record = store
            .all_files()
            .expect("ancestor")
            .get(&wp(MODE_CHANGE_TARGET))
            .cloned()
            .expect("ancestor row after the first cycle");
        store
            .open_intent(&mode_change_intent(MODE_CHANGE_TARGET, &record))
            .expect("open intent");
    }
    std::fs::remove_file(harness.root.join(MODE_CHANGE_TARGET)).expect("racing delete");

    harness
        .try_restart()
        .expect("startup survives a vanished mode-change target");
    // Idempotent: a second restart replays nothing and still starts.
    harness.try_restart().expect("startup stays clean");

    let store = open_engine_store(&harness.root);
    assert!(store.pending_intents().expect("intents").is_empty());
    assert!(
        !store
            .all_files()
            .expect("ancestor")
            .contains_key(&wp(MODE_CHANGE_TARGET)),
        "the startup cycle published the deletion through the ordinary merge matrix"
    );
}

// ---- a target that raced into an unsyncable object ------------------------

const UNSYNCABLE_TARGET: &str = "work/raced.dat";

/// The apply transaction re-observes the preimage AFTER `store.open_intent` has
/// made the operation durable. Wave 1 gave observation a third answer
/// (`ObserveOutcome::Unsyncable`) for exactly this case and converted the
/// scan/push side; the apply side kept the strict adapter, which manufactured an
/// `io::Error` that classified as `CycleError::Fatal`.
///
/// So a target racing into a FIFO, a socket, a device node, or a non-UTF-8
/// symlink between the journal write and the mutation did not fail one path — it
/// failed the cycle, and crash recovery then replayed the identical observation
/// on every restart. The op must settle as a frozen path instead: intent retired,
/// reason recorded, ancestor untouched.
#[test]
fn an_apply_target_that_raced_into_a_fifo_is_frozen_not_fatal() {
    let mut engine = TestEngine::new("apply-target-raced-unsyncable");
    engine.write(UNSYNCABLE_TARGET, b"local bytes");
    engine.push(&[UNSYNCABLE_TARGET]);
    let before = engine
        .files()
        .get(&wp(UNSYNCABLE_TARGET))
        .cloned()
        .expect("ancestor row after push");

    // The remote publishes different content for the same path: the merge matrix
    // row (local Unchanged, remote Changed) that schedules a plain install.
    let entry = engine.remote_file(b"remote bytes");
    let plan = MergePlan {
        fs_ops: vec![FsOp {
            path: wp(UNSYNCABLE_TARGET),
            kind: FsOpKind::Install(entry),
            expected: PreimagePayload::from_record(&before),
        }],
        ..MergePlan::default()
    };
    // The race: the target becomes a FIFO before the op reaches its mutation
    // boundary. `apply_op` journals the intent first, so this is that window.
    engine.remove(UNSYNCABLE_TARGET);
    plant_fifo(&engine.root(), UNSYNCABLE_TARGET).expect("plant fifo");

    let head = engine.remote.current_ref().expect("published head");
    let deps = PullDeps {
        ctx: &engine.ctx,
        objects: &engine.remote,
        refs: &engine.remote,
        scope: PullScope::WholeAncestor,
    };
    let outcome = super::apply::apply_plan(
        &mut engine.store,
        &deps,
        plan,
        &head.manifest_key,
        head.version,
    )
    .expect("an unsyncable object at the target is one path's problem, not the cycle's");

    assert_eq!(
        outcome
            .unsyncable
            .get(&wp(UNSYNCABLE_TARGET))
            .map(|record| record.reason),
        Some(UnsyncableReason::UnsupportedKind),
    );
    assert!(
        engine.store.pending_intents().expect("intents").is_empty(),
        "the intent is retired, so crash recovery does not replay it on every restart"
    );
    assert_eq!(
        engine
            .store
            .unsyncable()
            .expect("unsyncable")
            .get(&wp(UNSYNCABLE_TARGET))
            .map(|record| record.reason),
        Some(UnsyncableReason::UnsupportedKind),
        "the refusal is durable, so status can name the path and its remedy"
    );
    assert_eq!(
        engine.files().get(&wp(UNSYNCABLE_TARGET)),
        Some(&before),
        "the path is frozen: no install, no deletion, no ancestor change"
    );
}

/// The permanent-brick backstop, driven through the real startup path. A
/// journalled intent replays at EVERY start, so an intent whose target can never
/// be observed must be retired rather than retried: otherwise the first restart
/// after the race is also the last one that ever runs.
#[test]
fn startup_converges_when_an_intent_target_is_unsyncable() {
    let mut harness = DriverHarness::new("startup-intent-target-unsyncable", "device-a");
    harness.start();
    harness.write(UNSYNCABLE_TARGET, b"bytes");
    harness.edit(&[UNSYNCABLE_TARGET]);

    {
        let mut store = open_engine_store(&harness.root);
        let record = store
            .all_files()
            .expect("ancestor")
            .get(&wp(UNSYNCABLE_TARGET))
            .cloned()
            .expect("ancestor row after the first cycle");
        store
            .open_intent(&mode_change_intent(UNSYNCABLE_TARGET, &record))
            .expect("open intent");
    }
    std::fs::remove_file(harness.root.join(UNSYNCABLE_TARGET)).expect("racing replace");
    plant_fifo(&harness.root, UNSYNCABLE_TARGET).expect("plant fifo");

    harness
        .try_restart()
        .expect("startup survives an intent whose target is unsyncable");
    // Idempotent: the second restart proves the journal really was cleared, which
    // is the difference between a recovered device and a permanently dead one.
    harness.try_restart().expect("startup stays clean");

    let store = open_engine_store(&harness.root);
    assert!(
        store.pending_intents().expect("intents").is_empty(),
        "the intent is retired, never replayed into a brick"
    );
    assert!(
        store
            .unsyncable()
            .expect("unsyncable")
            .contains_key(&wp(UNSYNCABLE_TARGET)),
        "the abandoned path is recorded, so the user is told rather than left guessing"
    );
}

/// The sibling shape, one call down: `finalize_installed` re-observes the target
/// AFTER recovery classified it, so an ordinary delete landing in that window
/// finds nothing. It must retire the intent with the ancestor untouched for the
/// same reason the mode-change branch must — an `Internal` here replays on every
/// restart just as durably.
#[test]
fn finalizing_an_installed_intent_whose_target_vanished_is_not_fatal() {
    let mut engine = TestEngine::new("recover-finalize-vanished");
    engine.write("seed.txt", b"seed");
    engine.push(&["seed.txt"]);

    let entry = engine.remote_file(b"installed then deleted");
    let target: super::intents::TargetRecordPayload =
        serde_json::from_str(&install_target_record(&entry)).expect("decode target");
    let mut commit = AncestorCommit::default();

    super::apply::finalize_installed(&engine.ctx, &wp("recovered.txt"), &target, &mut commit)
        .expect("a vanished finalize target is a race, not an invariant violation");

    assert!(commit.upserts.is_empty() && commit.removals.is_empty());
}
