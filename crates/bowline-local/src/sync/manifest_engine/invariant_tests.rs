//! Cost-invariant tests (Plan 109 Step 8): idle costs nothing (C1), restart is
//! one stat walk (C3), the stat walk is stat-only at 10k files (C5), and a pull
//! costs the change rather than the workspace.
//!
//! These assert the observable consequences of the invariants through counting
//! test doubles ([`FakeRemote`] event/upload counters), the engine's own cost
//! meters, and the walker's `hashes` counter, which is structurally zero — the
//! walker has no content-open path.

use std::collections::BTreeSet;

use super::engine_test_support::{DriverHarness, test_crypto};
use super::manifest::WorkspacePath;
use super::scale_fixture::{STAT_WALK_FILES, measure_stat_walk};
use super::{EngineEvent, FullScanReason};
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

/// The pull cost model (R3): one remote change costs one local observation, not
/// one per workspace entry.
///
/// The remote delta is derivable from the ancestor rows and the decoded manifest
/// with zero filesystem access, so every path the remote did not move provably
/// lands on the `(Unchanged, Unchanged)` no-op row. Before the narrowing, this
/// pull performed `PULL_COST_FILES` `symlink_metadata` calls to reach that same
/// conclusion — the assertion below fails under that cost model by three orders
/// of magnitude.
const PULL_COST_FILES: usize = 1_000;

#[test]
fn one_remote_change_costs_one_local_observation() {
    let mut harness = DriverHarness::new("inv-pull-cost", "device-a");
    harness.start();

    let paths: Vec<String> = (0..PULL_COST_FILES)
        .map(|index| format!("f{index:05}.dat"))
        .collect();
    for path in &paths {
        harness.write(path, b"payload");
    }
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    harness.edit(&refs);
    assert!(
        harness.engine.dirty_paths().is_empty(),
        "the fixture must be fully published before the pull is measured"
    );

    // A peer changes exactly one file and advances the ref.
    let crypto = test_crypto();
    let mut manifest = harness
        .remote
        .decoded_manifest(&crypto)
        .expect("a published head");
    assert_eq!(manifest.entries.len(), PULL_COST_FILES);
    let changed = harness.remote.publish_blob(&crypto, b"peer bytes");
    manifest
        .entries
        .insert(WorkspacePath::new(paths[0].clone()), changed);
    harness.remote.publish_manifest(&crypto, &manifest);

    let before = harness.counters().merge_observations;
    harness.event(EngineEvent::RefChanged);
    harness.clock.advance(1_001);
    harness.run_due();
    let observations = harness.counters().merge_observations - before;

    assert_eq!(
        harness.read(&paths[0]),
        b"peer bytes",
        "the pull applied the one remote change"
    );
    assert_eq!(
        observations, 1,
        "a pull observes only the paths the remote moved (saw {observations} for one change \
         across {PULL_COST_FILES} entries)"
    );
}

/// The publish cost model (R5): a one-file edit publishes the nodes from that
/// file to the root, not the whole manifest.
///
/// The fixture is two directory levels of ten, so the tree has 111 nodes over
/// `PUBLISH_COST_FILES` entries. Editing one file rewrites exactly three of them
/// — its directory, that directory's parent, and the root — because every other
/// subtree keeps its content hash and is therefore already in the object store
/// under the very key the new tree names.
///
/// Under the flat manifest these assertions were unreachable: a publish
/// serialized, sealed, and PUT every entry, so the bytes an edit cost were the
/// bytes the workspace cost, and the ratio below was exactly 1.
const PUBLISH_COST_FILES: usize = 1_000;
const PUBLISH_COST_FANOUT: usize = 10;

fn nested_paths(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| {
            let outer = index % PUBLISH_COST_FANOUT;
            let inner = (index / PUBLISH_COST_FANOUT) % PUBLISH_COST_FANOUT;
            format!("p{outer}/m{inner}/f{index:05}.dat")
        })
        .collect()
}

/// Seed a harness with a nested fixture and publish all of it.
fn published_fixture(name: &str) -> (DriverHarness, Vec<String>) {
    let mut harness = DriverHarness::new(name, "device-a");
    harness.start();
    let paths = nested_paths(PUBLISH_COST_FILES);
    for path in &paths {
        harness.write(path, b"payload");
    }
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    harness.edit(&refs);
    assert!(
        harness.engine.dirty_paths().is_empty(),
        "the fixture must be fully published before cost is measured"
    );
    (harness, paths)
}

#[test]
fn one_local_edit_publishes_the_edit_not_the_workspace() {
    let (mut harness, paths) = published_fixture("inv-push-cost");
    let full = harness.counters();

    harness.write(&paths[0], b"payload edited");
    harness.edit(&[paths[0].as_str()]);
    let after = harness.counters();

    let edit_nodes = after.manifest_uploads - full.manifest_uploads;
    let edit_bytes = after.manifest_bytes_published - full.manifest_bytes_published;
    println!(
        "publish cost at {PUBLISH_COST_FILES} entries: full={} bytes / {} nodes, \
         one-file edit={edit_bytes} bytes / {edit_nodes} nodes",
        full.manifest_bytes_published, full.manifest_uploads,
    );

    assert_eq!(
        edit_nodes, 3,
        "an edit rewrites its directory, that directory's parent, and the root"
    );
    assert!(
        edit_bytes * 20 < full.manifest_bytes_published,
        "a one-file edit published {edit_bytes} bytes against a {} byte workspace",
        full.manifest_bytes_published,
    );
}

#[test]
fn one_remote_change_fetches_the_changed_subtree_not_the_tree() {
    let (mut harness, paths) = published_fixture("inv-pull-bytes");
    let full = harness.counters();

    // A peer changes exactly one file and advances the ref.
    let crypto = test_crypto();
    let mut manifest = harness
        .remote
        .decoded_manifest(&crypto)
        .expect("a published head");
    assert_eq!(manifest.entries.len(), PUBLISH_COST_FILES);
    let changed = harness.remote.publish_blob(&crypto, b"peer bytes");
    manifest
        .entries
        .insert(WorkspacePath::new(paths[0].clone()), changed);
    harness.remote.publish_manifest(&crypto, &manifest);

    harness.event(EngineEvent::RefChanged);
    harness.clock.advance(1_001);
    harness.run_due();
    let after = harness.counters();

    let fetched_nodes = after.manifest_downloads - full.manifest_downloads;
    let fetched_bytes = after.manifest_bytes_fetched - full.manifest_bytes_fetched;
    println!(
        "pull cost at {PUBLISH_COST_FILES} entries: fetched={fetched_bytes} bytes / \
         {fetched_nodes} nodes against a {} byte tree",
        full.manifest_bytes_published,
    );

    assert_eq!(
        harness.read(&paths[0]),
        b"peer bytes",
        "the pull applied the one remote change"
    );
    assert_eq!(
        fetched_nodes, 6,
        "the three changed ancestors are compared old-to-new; unchanged subtrees are not fetched"
    );
    assert!(
        fetched_bytes * 10 < full.manifest_bytes_published,
        "one remote change fetched {fetched_bytes} bytes of a {} byte tree",
        full.manifest_bytes_published,
    );
}

/// The audit cycle re-observes the excluded set once (debug builds) and proves
/// it is inert, so the narrowing cannot silently drop a local divergence.
#[test]
fn an_audit_pull_proves_the_narrowing_lost_nothing() {
    let mut harness = DriverHarness::new("inv-pull-audit", "device-a");
    harness.start();
    harness.write("kept.txt", b"local");
    harness.write("other.txt", b"other");
    harness.edit(&["kept.txt", "other.txt"]);

    // A local edit with no watcher event at all: only the audit's stat walk can
    // find it, and the narrowing proof runs on exactly that cycle.
    harness.write("kept.txt", b"local edited");
    harness.event(EngineEvent::FullScanRequired(FullScanReason::PeriodicAudit));
    harness.clock.advance(1_001);
    harness.run_due();

    let crypto = test_crypto();
    let manifest = harness
        .remote
        .decoded_manifest(&crypto)
        .expect("a published head");
    let entry = manifest
        .entries
        .get(&WorkspacePath::new("kept.txt"))
        .expect("kept.txt is published");
    let super::ManifestEntry::File { content_id, .. } = entry else {
        panic!("kept.txt is a file");
    };
    assert_eq!(
        content_id,
        &crypto.content_id(b"local edited"),
        "the audit republished the un-watched local edit"
    );
    assert_eq!(
        harness.engine.dirty_paths(),
        &BTreeSet::new(),
        "nothing is left pending after the audit cycle"
    );
}
