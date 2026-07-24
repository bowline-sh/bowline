//! Cost-invariant tests (Plan 109 Step 8): idle costs nothing (C1), restart is
//! one stat walk (C3), and the stat walk is stat-only at 10k files (C5).
//!
//! These assert the observable consequences of the invariants through counting
//! test doubles ([`FakeRemote`] event/upload counters) and the walker's own
//! `hashes` counter, which is structurally zero — the walker has no content-open
//! path.

use super::engine_test_support::DriverHarness;
use super::scale_fixture::{STAT_WALK_FILES, measure_stat_walk};
use crate::workspace::TempWorkspace;

#[test]
fn idle_loop_performs_no_writes() {
    let mut harness = DriverHarness::new("inv-idle", "device-a");
    harness.start();
    harness.write("a.txt", b"alpha");
    harness.write("b.txt", b"beta");
    harness.edit(&["a.txt", "b.txt"]);

    // Baseline after the workspace is synced and the engine is idle.
    let events = harness.remote.events().len();
    let uploads = harness.remote.blob_put_count();
    let revision = harness.engine.snapshot().revision;

    // Three idle ticks over the unchanged fixture: no scheduled deadline is due,
    // so each is a no-op — zero network calls, zero uploads, and no revision bump.
    for _ in 0..3 {
        harness.clock.advance(60_000);
        harness.run_due();
    }

    assert_eq!(
        harness.remote.events().len(),
        events,
        "idle performs no transport work (no gets, puts, or CAS)"
    );
    assert_eq!(
        harness.remote.blob_put_count(),
        uploads,
        "idle uploads nothing"
    );
    assert_eq!(
        harness.engine.snapshot().revision,
        revision,
        "an idle poll advances no revision"
    );
    assert!(harness.engine.dirty_paths().is_empty());
}

#[test]
fn restart_cost_is_one_statwalk() {
    let mut harness = DriverHarness::new("inv-restart", "device-a");
    harness.start();
    harness.write("x.txt", b"one");
    harness.write("y.txt", b"two");
    harness.edit(&["x.txt", "y.txt"]);

    let head = harness.remote.current_ref();
    let uploads = harness.remote.blob_put_count();

    // Restart: recover (no intents) → one stat walk over the unchanged fixture →
    // read + verify the ref → already current. Nothing is re-hashed or re-uploaded.
    harness.restart();

    assert_eq!(
        harness.remote.blob_put_count(),
        uploads,
        "restart re-uploads nothing (no full re-hash)"
    );
    assert_eq!(
        harness.remote.current_ref(),
        head,
        "restart advances no ref"
    );
    assert!(
        harness.engine.dirty_paths().is_empty(),
        "the restart stat walk finds nothing changed"
    );
}

#[test]
fn statwalk_10k_under_100ms_zero_hashes() {
    let workspace = TempWorkspace::new("inv-statwalk-10k").expect("temp workspace");
    // Reuse the shared fixture primitive so the C5 invariant and the release
    // fixture JSON measure the same seed-and-walk (no second copy).
    let walk = measure_stat_walk(workspace.root(), STAT_WALK_FILES);

    // Record the measured number in the test output (Plan 109 Step 8).
    println!(
        "stat_walk {STAT_WALK_FILES} files: {} ms, scanned={}, hashes={}",
        walk.millis, walk.scanned, walk.hashes,
    );

    // The zero-hashes property is asserted strictly regardless of the debug-build
    // timing; the 100 ms budget is a release-build target recorded, not enforced,
    // here (Plan 109 Step 8 instruction).
    assert_eq!(
        walk.hashes, 0,
        "the stat walk hashes nothing (invariant C5)"
    );
    assert_eq!(
        walk.dirty, 0,
        "an unchanged fixture is entirely clean under stat comparison"
    );
    assert_eq!(
        walk.scanned, STAT_WALK_FILES as u64,
        "every fixture file is statted once"
    );
}
