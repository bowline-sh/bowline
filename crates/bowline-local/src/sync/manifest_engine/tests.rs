//! Driver tests + the two-engine simulation (Plan 109 Step 7).
//!
//! These drive the real [`ManifestEngine`] state machine over a [`TestClock`] the
//! test advances by hand, so debounce, backoff, and the overflow/full-scan paths
//! are exercised deterministically with no sleeping. The two-engine simulation
//! runs two engines over one shared fake remote to prove peer propagation and a
//! single deterministic conflict-aside.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use bowline_core::ids::ContentId;

use super::engine_test_support::{
    DriverHarness, FakeRemote, TestClock, engine_io, open_engine_store, test_context, test_crypto,
};
use super::{
    Degradation, EngineEvent, EnginePhase, FullScanReason, ManifestEngine, ManifestEntry,
    ManifestKey, RefObservation, SyncBarrierId, WorkspaceCrypto, WorkspacePath,
};
use crate::workspace::TempWorkspace;

/// The content id the head manifest records for `rel`, for asserting that a
/// mode-only change did not corrupt a file's content identity.
fn head_content_id(remote: &FakeRemote, crypto: &WorkspaceCrypto, rel: &str) -> ContentId {
    let manifest = remote.decoded_manifest(crypto).expect("head manifest");
    match manifest
        .entries
        .get(&WorkspacePath::new(rel))
        .unwrap_or_else(|| panic!("{rel} missing from head manifest"))
    {
        ManifestEntry::File { content_id, .. } => content_id.clone(),
        other => panic!("expected {rel} to be a file, got {other:?}"),
    }
}

fn path_set(paths: &[&str]) -> BTreeSet<WorkspacePath> {
    paths.iter().map(|path| WorkspacePath::new(*path)).collect()
}

fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&path, bytes).expect("write");
}

// ---- debounce, idle, overflow ----------------------------------------------

#[test]
fn counters_meter_an_edit_and_stay_flat_when_idle() {
    let mut harness = DriverHarness::new("driver-counters", "device-a");
    harness.start();
    // Startup seeds the dirty set with exactly one stat walk that opens nothing.
    let after_start = harness.counters();
    assert!(after_start.stat_walks >= 1, "startup stat-walk counted");
    assert_eq!(
        after_start.content_hashes, 0,
        "the stat walk hashes nothing (invariant C5)"
    );
    assert_eq!(
        after_start.content_opens, 0,
        "no content opens on a stat walk"
    );

    // One edit costs the edit (invariant C2): one content open+hash, one blob and
    // one manifest upload, one CAS attempt, and store write transactions.
    harness.write("src/a.txt", b"hello counters\n");
    harness.edit(&["src/a.txt"]);
    let after_edit = harness.counters();
    assert_eq!(after_edit.content_opens, 1, "opened the one changed file");
    assert_eq!(after_edit.blob_uploads, 1, "uploaded one blob");
    assert_eq!(after_edit.manifest_uploads, 1, "uploaded one manifest");
    assert_eq!(after_edit.cas_attempts, 1, "one CAS attempt");
    assert_eq!(after_edit.cas_losses, 0, "the CAS won");
    assert!(after_edit.sqlite_mutations >= 1, "committed the push");
    assert!(after_edit.hashed_bytes >= b"hello counters\n".len() as u64);

    // Idle costs nothing (invariant C1): no further uploads or store writes.
    harness.run_due();
    harness.run_due();
    let after_idle = harness.counters();
    assert_eq!(
        after_idle.blob_uploads, after_edit.blob_uploads,
        "idle uploads no blobs"
    );
    assert_eq!(
        after_idle.manifest_uploads, after_edit.manifest_uploads,
        "idle uploads no manifests"
    );
    assert_eq!(
        after_idle.sqlite_mutations, after_edit.sqlite_mutations,
        "idle mutates no SQLite"
    );
}

#[test]
fn idle_engine_blocks_with_no_deadline() {
    let mut harness = DriverHarness::new("driver-idle", "device-a");
    harness.start();
    // Nothing pending: the engine has no scheduled wakeup, so the run loop blocks
    // on the next event and performs no work (invariant C1).
    assert_eq!(harness.engine.next_timeout(0), None);
    let revision = harness.engine.snapshot().revision;
    harness.run_due();
    harness.run_due();
    assert_eq!(
        harness.engine.snapshot().revision,
        revision,
        "idle bumps nothing"
    );
}

#[test]
fn remote_wake_is_non_ready_before_and_during_its_pull_cycle() {
    let mut harness = DriverHarness::new("driver-pull-boundary", "device-a");
    harness.start();
    let idle_revision = harness.engine.snapshot().revision;

    harness.event(EngineEvent::RefChanged);
    let scheduled = harness.engine.snapshot();
    assert!(scheduled.unattributed_pull_pending);
    assert!(!scheduled.cycle_active);
    assert!(scheduled.revision > idle_revision);

    assert!(harness.engine.announce_due_work(&harness.clock));
    let active = harness.engine.snapshot();
    assert!(!active.unattributed_pull_pending);
    assert!(active.cycle_active);
    assert!(active.revision > scheduled.revision);

    harness.run_due();
    let settled = harness.engine.snapshot();
    assert!(!settled.unattributed_pull_pending);
    assert!(!settled.cycle_active);
    assert_eq!(settled.phase, EnginePhase::Idle);
}

#[test]
fn verified_ref_observation_skips_the_redundant_ref_read() {
    let mut harness = DriverHarness::new("driver-ref-observation-fast-path", "device-a");
    harness.start();
    harness.write("fast.txt", b"reactive");
    harness.edit(&["fast.txt"]);
    let observed = harness.remote.current_ref().expect("published head");
    let reads_before = harness.remote.read_ref_count();

    harness.event(EngineEvent::RefObserved(observed));
    harness.run_due();

    assert_eq!(
        harness.remote.read_ref_count(),
        reads_before,
        "a verified reactive observation is consumed without a duplicate query"
    );
}

#[test]
fn lagging_subscription_echo_older_than_the_known_head_is_ignored() {
    let mut harness = DriverHarness::new("driver-ref-observation-stale-echo", "device-a");
    harness.start();
    harness.write("fast.txt", b"reactive");
    harness.edit(&["fast.txt"]);
    let current = harness.remote.current_ref().expect("published head");
    let reads_before = harness.remote.read_ref_count();

    harness.event(EngineEvent::RefObserved(RefObservation {
        version: current.version.saturating_sub(1),
        manifest_key: ManifestKey::new("m_stale_subscription_echo"),
    }));
    harness.run_due();

    assert_eq!(harness.remote.read_ref_count(), reads_before);
    assert!(!harness.engine.pull_needed);
    assert_eq!(harness.engine.pending_ref_hint, None);
    assert_eq!(harness.engine.snapshot().phase, EnginePhase::Idle);
}

#[test]
fn payload_free_ref_wake_still_reestablishes_synchronous_authority() {
    let mut harness = DriverHarness::new("driver-ref-observation-fallback", "device-a");
    harness.start();
    let reads_before = harness.remote.read_ref_count();

    harness.event(EngineEvent::RefChanged);
    harness.run_due();

    assert_eq!(
        harness.remote.read_ref_count(),
        reads_before + 1,
        "ambiguous wakes retain the synchronous authority fallback"
    );
}

#[test]
fn ambiguous_ref_wake_cannot_be_overwritten_by_a_queued_hint() {
    let mut harness = DriverHarness::new("driver-ref-observation-sticky-fallback", "device-a");
    harness.start();
    harness.write("fast.txt", b"reactive");
    harness.edit(&["fast.txt"]);
    let observed = harness.remote.current_ref().expect("published head");
    let reads_before = harness.remote.read_ref_count();

    harness.event(EngineEvent::RefChanged);
    harness.event(EngineEvent::RefObserved(observed));
    harness.run_due();

    assert_eq!(
        harness.remote.read_ref_count(),
        reads_before + 1,
        "a reconnect/startup fallback remains authoritative until consumed"
    );
}

#[test]
fn reactive_ref_observations_coalesce_to_the_newest_version() {
    let mut harness = DriverHarness::new("driver-ref-observation-coalesce", "device-a");
    harness.start();
    let older = RefObservation {
        version: 4,
        manifest_key: ManifestKey::new("m_older"),
    };
    let newer = RefObservation {
        version: 6,
        manifest_key: ManifestKey::new("m_newer"),
    };

    harness.event(EngineEvent::RefObserved(older));
    harness.event(EngineEvent::RefObserved(newer.clone()));
    harness.event(EngineEvent::RefObserved(RefObservation {
        version: 5,
        manifest_key: ManifestKey::new("m_middle"),
    }));

    assert_eq!(harness.engine.pending_ref_hint, Some(newer));
}

#[test]
fn connectivity_restoration_requires_a_fresh_remote_pull() {
    let mut harness = DriverHarness::new("driver-connectivity-pull", "device-a");
    harness.start();
    assert!(!harness.engine.pull_needed);

    harness.event(EngineEvent::ConnectivityRestored);

    assert!(harness.engine.pull_needed);
    assert!(harness.engine.snapshot().unattributed_pull_pending);
}

#[test]
fn debounce_holds_publication_until_the_window_elapses() {
    let mut harness = DriverHarness::new("driver-debounce", "device-a");
    harness.start();
    harness.write("a.txt", b"one");
    harness.event(EngineEvent::Paths(path_set(&["a.txt"])));

    // Before the window, nothing is published.
    harness.clock.advance(200);
    harness.run_due();
    assert!(
        harness.remote.current_ref().is_none(),
        "held during debounce"
    );

    // After the window, the batch publishes as one push.
    harness.clock.advance(100);
    harness.run_due();
    assert!(
        harness.remote.current_ref().is_some(),
        "published after debounce"
    );
}

#[test]
fn overflow_triggers_immediate_full_scan() {
    let mut producer = DriverHarness::new("driver-overflow", "device-a");
    producer.start();
    producer.write("tracked.txt", b"tracked");
    producer.edit(&["tracked.txt"]);

    // A file appears with NO watcher Paths event — the lost-event case. An
    // overflow signal must fall back to a full stat walk immediately.
    producer.write("missed.txt", b"missed by the watcher");
    producer.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherOverflow,
    ));
    // No clock advance: overflow recovery is immediate, not debounced.
    producer.run_due();

    // The missed file propagates to a fresh peer, proving the scan found + pushed
    // it without any Paths event naming it.
    let mut peer = DriverHarness::new("driver-overflow-peer", "device-b");
    swap_remote(&mut peer, &producer);
    peer.start();
    assert_eq!(peer_read(&peer, "missed.txt"), b"missed by the watcher");
    assert_eq!(peer_read(&peer, "tracked.txt"), b"tracked");
}

#[test]
fn sync_barrier_finds_a_change_before_its_watcher_event_and_acknowledges_exactly() {
    let mut producer = DriverHarness::new("driver-sync-barrier", "device-a");
    producer.start();
    producer.write("missed.txt", b"written before watcher delivery");

    let barrier = SyncBarrierId(41);
    producer.event(EngineEvent::SyncBarrier(barrier));
    producer.run_due();

    assert_eq!(
        producer.engine.take_completed_barriers(),
        BTreeSet::from([barrier]),
        "the exact barrier completes after its forced scan and ref read"
    );
    let mut peer = DriverHarness::new("driver-sync-barrier-peer", "device-b");
    swap_remote(&mut peer, &producer);
    peer.start();
    assert_eq!(
        peer_read(&peer, "missed.txt"),
        b"written before watcher delivery"
    );
}

#[test]
fn startup_full_scan_publishes_preexisting_opaque_git_state() {
    let mut producer = DriverHarness::new("driver-startup-git-state", "device-a");
    producer.write("repo/.git/objects/ab/cdef", b"opaque object");
    producer.write("repo/.git/refs/heads/main", b"abc123\n");
    producer.write("repo/.git/HEAD", b"ref: refs/heads/main\n");

    producer.start();

    let mut peer = DriverHarness::new("driver-startup-git-state-peer", "device-b");
    swap_remote(&mut peer, &producer);
    peer.start();
    assert_eq!(
        peer_read(&peer, "repo/.git/objects/ab/cdef"),
        b"opaque object"
    );
    assert_eq!(peer_read(&peer, "repo/.git/refs/heads/main"), b"abc123\n");
    assert_eq!(
        peer_read(&peer, "repo/.git/HEAD"),
        b"ref: refs/heads/main\n"
    );
}

#[test]
fn recursive_directory_event_publishes_dense_post_start_git_state() {
    let mut producer = DriverHarness::new("driver-reactive-git-state", "device-a");
    producer.start();

    producer.write("repo/src/main.rs", b"fn main() {}\n");
    producer.write("repo/.git/objects/ab/cdef", b"opaque object");
    producer.write("repo/.git/refs/heads/main", b"abc123\n");
    producer.write("repo/.git/HEAD", b"ref: refs/heads/main\n");
    producer.event(EngineEvent::RecursivePaths(path_set(&["repo"])));
    producer.clock.advance(1_001);
    producer.run_due();

    let mut peer = DriverHarness::new("driver-reactive-git-state-peer", "device-b");
    swap_remote(&mut peer, &producer);
    peer.start();
    assert_eq!(peer_read(&peer, "repo/src/main.rs"), b"fn main() {}\n");
    assert_eq!(
        peer_read(&peer, "repo/.git/objects/ab/cdef"),
        b"opaque object"
    );
    assert_eq!(peer_read(&peer, "repo/.git/refs/heads/main"), b"abc123\n");
    assert_eq!(
        peer_read(&peer, "repo/.git/HEAD"),
        b"ref: refs/heads/main\n"
    );
}

#[test]
fn ref_wakeup_does_not_preempt_recursive_directory_debounce() {
    let mut producer = DriverHarness::new("driver-reactive-git-ref-race", "device-a");
    producer.start();
    fs::create_dir_all(producer.root.join("repo/.git")).expect("git dir");
    producer.event(EngineEvent::RecursivePaths(path_set(&["repo"])));
    producer.clock.advance(100);

    // A ref notification can be an echo of our preceding directory publish or
    // a peer update. It must not force a pending subtree scan to run before the
    // native watcher burst settles.
    producer.event(EngineEvent::RefChanged);
    producer.run_due();
    producer.write("repo/.git/objects/ab/cdef", b"opaque object");
    producer.write("repo/.git/refs/heads/main", b"abc123\n");
    producer.write("repo/.git/HEAD", b"ref: refs/heads/main\n");
    producer.clock.advance(200);
    producer.run_due();

    let mut peer = DriverHarness::new("driver-reactive-git-ref-race-peer", "device-b");
    swap_remote(&mut peer, &producer);
    peer.start();
    assert_eq!(
        peer_read(&peer, "repo/.git/objects/ab/cdef"),
        b"opaque object"
    );
    assert_eq!(peer_read(&peer, "repo/.git/refs/heads/main"), b"abc123\n");
    assert_eq!(
        peer_read(&peer, "repo/.git/HEAD"),
        b"ref: refs/heads/main\n"
    );
}

#[test]
fn ref_wakeup_rearms_stranded_recursive_work() {
    let mut producer = DriverHarness::new("driver-reactive-git-rearm", "device-a");
    producer.start();
    producer.event(EngineEvent::RecursivePaths(path_set(&["repo"])));

    // Model a failed cycle after its due deadline was consumed but before the
    // recursive root completed. A ref wakeup must replace the missing schedule
    // when it preempts backoff rather than leaving pending work stranded.
    producer.engine.debounce_deadline = None;
    producer.event(EngineEvent::RefChanged);

    assert_eq!(
        producer.engine.next_timeout(producer.clock.millis()),
        Some(std::time::Duration::ZERO)
    );
}

// ---- backoff ----------------------------------------------------------------

#[test]
fn backoff_preempted_by_ref_event() {
    let mut harness = DriverHarness::new("driver-backoff", "device-a");
    harness.start();

    // The network is down: the first push fails and the engine backs off.
    harness.remote.set_offline(true);
    harness.write("f.txt", b"pending edit");
    harness.edit(&["f.txt"]);
    let snapshot = harness.engine.snapshot();
    assert!(
        matches!(snapshot.degradation, Degradation::OfflineRetrying { .. }),
        "a transport failure backs off, never an attention state"
    );
    assert_eq!(snapshot.phase, EnginePhase::BackingOff);
    let now = harness.clock.millis();
    assert!(
        harness
            .engine
            .next_timeout(now)
            .is_some_and(|d| d.as_millis() > 0),
        "backoff schedules a future retry"
    );

    // A ref event preempts the backoff: work becomes due immediately (delay 0),
    // not after the backoff interval.
    harness.event(EngineEvent::RefChanged);
    assert_eq!(
        harness.engine.next_timeout(now),
        Some(std::time::Duration::from_millis(0)),
        "an event preempts the pending backoff"
    );

    // With the network restored, the preempted retry succeeds and clears.
    harness.remote.set_offline(false);
    harness.run_due();
    let recovered = harness.engine.snapshot();
    assert_eq!(recovered.degradation, Degradation::Nominal);
    assert!(
        harness.remote.current_ref().is_some(),
        "the edit published on retry"
    );
}

// ---- skipped (twice-diverged) path retention --------------------------------

/// Point `<root>/dir` at an external directory holding `file`, so `dir/file`
/// observes as a regular file but every no-follow content read diverges. This is
/// the deterministic stand-in for a file being actively written under a push
/// (two consecutive scan divergences), without a racing writer thread.
fn symlink_parent_trap(root: &Path, external_name: &str) {
    let external = std::env::temp_dir().join(external_name);
    fs::create_dir_all(&external).expect("external dir");
    fs::write(external.join("file"), b"EXTERNAL SECRET").expect("external file");
    symlink(&external, root.join("dir")).expect("symlink dir");
}

#[test]
fn skipped_path_is_retained_rescheduled_and_converges() {
    let mut harness = DriverHarness::new("driver-skip-retain", "device-a");
    harness.start();

    // A path that diverges on every content read: the churning-writer stand-in.
    symlink_parent_trap(&harness.root, "bowline-driver-skip-retain");
    harness.event(EngineEvent::Paths(path_set(&["dir/file"])));
    harness.clock.advance(1_001);
    harness.run_due();

    // The scan could not settle it: nothing published, but the path is RETAINED
    // and a deadline is armed so a later cycle rescans it with no new event.
    assert!(
        harness.remote.current_ref().is_none(),
        "a twice-diverged path publishes nothing"
    );
    assert_eq!(
        harness.engine.dirty_paths(),
        &path_set(&["dir/file"]),
        "the churning path is retained, not dropped"
    );
    assert!(
        harness
            .engine
            .next_timeout(harness.clock.millis())
            .is_some(),
        "a rescan deadline is armed without any new watcher event"
    );
    assert!(harness.counters().push_skips >= 1, "the skip is observable");

    // The file settles: replace the symlinked parent with a real dir + file.
    fs::remove_file(harness.root.join("dir")).expect("remove symlink");
    harness.write("dir/file", b"settled workspace bytes");

    // The NEXT cycle fires from the armed deadline alone — no Paths event.
    harness.clock.advance(1_001);
    harness.run_due();

    // It seals and publishes, converges, and goes idle (invariant C1).
    assert!(
        harness.remote.current_ref().is_some(),
        "the settled file publishes on the rescheduled cycle"
    );
    assert!(
        harness.engine.dirty_paths().is_empty(),
        "convergence clears the dirty set"
    );
    assert_eq!(
        harness.engine.next_timeout(harness.clock.millis()),
        None,
        "idle after convergence: no deadline armed"
    );
}

#[test]
fn advanced_cycle_retains_skipped_path() {
    let mut harness = DriverHarness::new("driver-skip-mixed", "device-a");
    harness.start();

    // One clean file plus one churning path in the same batch.
    symlink_parent_trap(&harness.root, "bowline-driver-skip-mixed");
    harness.write("clean.txt", b"clean workspace bytes");
    harness.event(EngineEvent::Paths(path_set(&["clean.txt", "dir/file"])));
    harness.clock.advance(1_001);
    harness.run_due();

    // The clean file advanced the head...
    assert!(
        harness.remote.current_ref().is_some(),
        "the clean file published"
    );
    let manifest = harness
        .remote
        .decoded_manifest(&test_crypto())
        .expect("head manifest");
    assert!(
        manifest
            .entries
            .contains_key(&WorkspacePath::new("clean.txt")),
        "the head carries the clean file"
    );
    assert!(
        !manifest
            .entries
            .contains_key(&WorkspacePath::new("dir/file")),
        "the churning path was never sealed into the head"
    );
    // ...and the churning path was retained for a rescheduled rescan.
    assert_eq!(
        harness.engine.dirty_paths(),
        &path_set(&["dir/file"]),
        "the skipped path is retained after an advance"
    );
    assert!(
        harness
            .engine
            .next_timeout(harness.clock.millis())
            .is_some(),
        "a rescan deadline is armed for the retained path"
    );
}

#[test]
fn clean_push_skips_nothing_and_stays_idle() {
    let mut harness = DriverHarness::new("driver-skip-none", "device-a");
    harness.start();
    harness.write("a.txt", b"clean");
    harness.edit(&["a.txt"]);

    // Nothing was skipped: the dirty set is fully cleared and no deadline is
    // armed, so an idle engine performs no further work (invariant C1).
    assert_eq!(harness.counters().push_skips, 0, "nothing was skipped");
    assert!(
        harness.engine.dirty_paths().is_empty(),
        "dirty fully cleared"
    );
    assert_eq!(
        harness.engine.next_timeout(harness.clock.millis()),
        None,
        "no deadline armed when nothing was skipped"
    );
}

// ---- two-engine simulation --------------------------------------------------

/// Two engines over one shared fake remote. Both share the workspace key/epoch so
/// each can open the other's sealed blobs; only the device id differs.
struct TwoEngines {
    _ws_a: TempWorkspace,
    _ws_b: TempWorkspace,
    root_a: PathBuf,
    root_b: PathBuf,
    engine_a: ManifestEngine,
    engine_b: ManifestEngine,
    remote: FakeRemote,
    clock: TestClock,
}

impl TwoEngines {
    fn new() -> Self {
        let ws_a = TempWorkspace::new("sim-a").expect("ws a");
        let ws_b = TempWorkspace::new("sim-b").expect("ws b");
        let root_a = ws_a.root().to_path_buf();
        let root_b = ws_b.root().to_path_buf();
        let engine_a = ManifestEngine::new(
            open_engine_store(&root_a),
            test_context(root_a.clone(), "device-a"),
        );
        let engine_b = ManifestEngine::new(
            open_engine_store(&root_b),
            test_context(root_b.clone(), "device-b"),
        );
        Self {
            _ws_a: ws_a,
            _ws_b: ws_b,
            root_a,
            root_b,
            engine_a,
            engine_b,
            remote: FakeRemote::new(),
            clock: TestClock::new(),
        }
    }

    fn start(&mut self) {
        self.engine_a
            .start(&engine_io(&self.remote, &self.clock))
            .expect("start a");
        self.engine_b
            .start(&engine_io(&self.remote, &self.clock))
            .expect("start b");
    }

    fn edit_a(&mut self, rel: &str, bytes: &[u8]) {
        write_file(&self.root_a, rel, bytes);
        self.engine_a
            .on_event(EngineEvent::Paths(path_set(&[rel])), &self.clock);
        self.clock.advance(1_001);
        self.engine_a
            .run_due_work(&engine_io(&self.remote, &self.clock))
            .expect("push a");
    }

    fn edit_b(&mut self, rel: &str, bytes: &[u8]) {
        write_file(&self.root_b, rel, bytes);
        self.engine_b
            .on_event(EngineEvent::Paths(path_set(&[rel])), &self.clock);
        self.clock.advance(1_001);
        self.engine_b
            .run_due_work(&engine_io(&self.remote, &self.clock))
            .expect("push b");
    }

    /// Chmod a path on A (content unchanged) and push the mode-only change.
    fn chmod_a(&mut self, rel: &str, mode: u32) {
        fs::set_permissions(self.root_a.join(rel), fs::Permissions::from_mode(mode))
            .expect("chmod a");
        self.engine_a
            .on_event(EngineEvent::Paths(path_set(&[rel])), &self.clock);
        self.clock.advance(1_001);
        self.engine_a
            .run_due_work(&engine_io(&self.remote, &self.clock))
            .expect("push a mode change");
    }

    fn ref_sync_a(&mut self) {
        self.engine_a.on_event(EngineEvent::RefChanged, &self.clock);
        self.engine_a
            .run_due_work(&engine_io(&self.remote, &self.clock))
            .expect("ref sync a");
    }

    fn ref_sync_b(&mut self) {
        self.engine_b.on_event(EngineEvent::RefChanged, &self.clock);
        self.engine_b
            .run_due_work(&engine_io(&self.remote, &self.clock))
            .expect("ref sync b");
    }
}

#[test]
fn edit_on_a_propagates_to_b() {
    let mut sim = TwoEngines::new();
    sim.start();
    sim.edit_a("shared.txt", b"from A");
    sim.ref_sync_b();
    assert_eq!(
        fs::read(sim.root_b.join("shared.txt")).expect("b read"),
        b"from A"
    );
}

#[test]
fn mode_only_change_preserves_content_identity_for_next_push() {
    let crypto = test_crypto();
    let mut sim = TwoEngines::new();
    sim.start();
    // A creates a file; B syncs it so both hold the ancestor row.
    sim.edit_a("script.sh", b"#!/bin/sh\necho hi\n");
    sim.ref_sync_b();
    let original = head_content_id(&sim.remote, &crypto, "script.sh");

    // A chmods the file (bytes unchanged) and publishes the mode-only change.
    sim.chmod_a("script.sh", 0o755);
    // B applies the mode-only change: before the fix this wrote an ancestor row
    // with content_id/blob_key/key_epoch = None.
    sim.ref_sync_b();

    // B now pushes an unrelated file. build_manifest projects every ancestor row
    // through file_record_to_entry; a content-less script.sh row would error
    // (AncestorRowMissing) and kill the engine. The push must succeed.
    sim.edit_b("notes.txt", b"unrelated content\n");

    // And the head still carries script.sh's real content identity, unchanged.
    let after = head_content_id(&sim.remote, &crypto, "script.sh");
    assert_eq!(
        after, original,
        "the mode-only change preserved script.sh's content id"
    );
}

#[test]
fn concurrent_edits_produce_one_deterministic_aside() {
    let mut sim = TwoEngines::new();
    sim.start();
    // Establish a shared ancestor.
    sim.edit_a("f.txt", b"base");
    sim.ref_sync_b();
    assert_eq!(fs::read(sim.root_b.join("f.txt")).expect("b base"), b"base");

    // Both edit the same path from that ancestor; A publishes first.
    write_file(&sim.root_a, "f.txt", b"A wins the race");
    write_file(&sim.root_b, "f.txt", b"B loses the race");
    sim.edit_a("f.txt", b"A wins the race");
    // B's push loses the CAS, pulls A, asides A's bytes, keeps its own, re-pushes.
    sim.edit_b("f.txt", b"B loses the race");
    // A pulls the converged head (B's bytes + the aside).
    sim.ref_sync_a();

    // Both converge on B's local bytes, and both see exactly one identical aside.
    assert_eq!(
        fs::read(sim.root_a.join("f.txt")).expect("a f"),
        b"B loses the race"
    );
    assert_eq!(
        fs::read(sim.root_b.join("f.txt")).expect("b f"),
        b"B loses the race"
    );

    let asides_a = aside_files(&sim.root_a);
    let asides_b = aside_files(&sim.root_b);
    assert_eq!(asides_a.len(), 1, "exactly one aside on A: {asides_a:?}");
    assert_eq!(
        asides_b, asides_a,
        "the aside is deterministic across devices"
    );
    assert_eq!(
        fs::read(sim.root_a.join(&asides_a[0])).expect("aside content"),
        b"A wins the race",
        "the aside carries the losing remote bytes"
    );
}

#[test]
fn convergence_after_writes_stop() {
    let mut sim = TwoEngines::new();
    sim.start();
    // A few alternating edits, each propagated.
    sim.edit_a("doc.txt", b"v1");
    sim.ref_sync_b();
    sim.edit_b("doc.txt", b"v2");
    sim.ref_sync_a();
    sim.edit_a("doc.txt", b"v3");
    sim.ref_sync_b();
    assert_eq!(fs::read(sim.root_a.join("doc.txt")).expect("a"), b"v3");
    assert_eq!(fs::read(sim.root_b.join("doc.txt")).expect("b"), b"v3");

    // Writes stop. Idle ticks do no work and do not advance either revision.
    let uploads = sim.remote.blob_put_count();
    let rev_a = sim.engine_a.snapshot().revision;
    let rev_b = sim.engine_b.snapshot().revision;
    for _ in 0..3 {
        sim.clock.advance(10_000);
        sim.engine_a
            .run_due_work(&engine_io(&sim.remote, &sim.clock))
            .expect("idle a");
        sim.engine_b
            .run_due_work(&engine_io(&sim.remote, &sim.clock))
            .expect("idle b");
    }
    assert_eq!(sim.remote.blob_put_count(), uploads, "idle uploads nothing");
    assert_eq!(sim.engine_a.snapshot().revision, rev_a, "A revision stable");
    assert_eq!(sim.engine_b.snapshot().revision, rev_b, "B revision stable");
    assert!(sim.engine_a.dirty_paths().is_empty() && sim.engine_b.dirty_paths().is_empty());
}

#[test]
fn git_lock_deferred_pull_retries_from_its_own_deadline() {
    // A producer publishes a file inside a Git dir; a peer converges cleanly.
    let mut producer = DriverHarness::new("driver-gitlock-producer", "device-a");
    producer.start();
    producer.write("proj/.git/config", b"[core]");
    producer.edit(&["proj/.git/config"]);
    let mut peer = DriverHarness::new("driver-gitlock-peer", "device-b");
    swap_remote(&mut peer, &producer);
    peer.start();
    assert_eq!(peer_read(&peer, "proj/.git/config"), b"[core]");

    // The producer updates the file. The peer learns via RefChanged, but an
    // ACTIVE Git lock is present when its pull runs, so the path defers with
    // NOTHING ELSE dirty — the engine's own re-armed pull is the only retry.
    swap_remote(&mut producer, &peer);
    producer.event(EngineEvent::RefChanged);
    producer.clock.advance(1_001);
    producer.run_due();
    producer.write("proj/.git/config", b"[core]\n[user]");
    producer.edit(&["proj/.git/config"]);
    swap_remote(&mut peer, &producer);
    fs::write(peer.root.join("proj/.git/index.lock"), b"").expect("lock");
    peer.event(EngineEvent::RefChanged);
    peer.clock.advance(1_001);
    peer.run_due();
    assert_eq!(
        peer_read(&peer, "proj/.git/config"),
        b"[core]",
        "the locked path defers"
    );
    assert!(
        peer.engine.next_timeout(peer.clock.millis()).is_some(),
        "a retry deadline is armed for the deferred pull"
    );

    // The lock clears; the ONLY wakeup is the engine's own deadline — no
    // RefChanged, no Paths event. The deferred path must still materialize.
    fs::remove_file(peer.root.join("proj/.git/index.lock")).expect("unlock");
    peer.clock.advance(120_001);
    peer.run_due();
    assert_eq!(
        peer_read(&peer, "proj/.git/config"),
        b"[core]\n[user]",
        "the internally scheduled retry pulls the deferred path"
    );
}

// ---- helpers ----------------------------------------------------------------

/// Move the producer's remote into the peer so the peer syncs against the same
/// head (the two `Harness` engines otherwise hold independent fake remotes).
fn swap_remote(peer: &mut DriverHarness, producer: &DriverHarness) {
    peer.remote = producer.remote.clone_state();
}

fn peer_read(peer: &DriverHarness, rel: &str) -> Vec<u8> {
    fs::read(peer.root.join(rel)).expect("peer read")
}

fn aside_files(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    collect_asides(root, "", &mut names);
    names.sort();
    names
}

fn collect_asides(root: &Path, relative: &str, names: &mut Vec<String>) {
    let dir = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".bowline" {
            continue;
        }
        let child = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            collect_asides(root, &child, names);
        } else if name.contains("conflict from") {
            names.push(child);
        }
    }
}
