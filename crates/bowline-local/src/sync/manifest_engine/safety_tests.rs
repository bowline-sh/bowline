//! The engine's destructive-failure guards.
//!
//! Every test here corresponds to a way the engine could previously destroy or
//! silently drop user data: an unmounted root read as an empty workspace, a
//! removal batch published without question, one unreadable file killing the
//! whole engine thread, a name that could not be represented being skipped in
//! silence, and a watcher that dropped an event with nothing to catch it.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;

use super::engine_test_support::{DriverHarness, FakeRemote, TestEngine};
use super::store::ManifestStore;
use super::unsyncable::{UnsyncableReason, UnsyncableRecord};
use super::workspace_root::{RootFault, marker_path};
use super::{
    AUDIT_INTERVAL_MAX_MS, CasOutcome, Degradation, DeletionConfirmation, EngineEvent, EngineIo,
    EnginePhase, FullScanReason, Manifest, RefObservation, RemoteRef, WorkspacePath,
    mass_deletion_threshold,
};
use crate::workspace::TempWorkspace;

fn head_paths(harness: &DriverHarness) -> BTreeSet<WorkspacePath> {
    let crypto = super::engine_test_support::test_crypto();
    harness
        .remote
        .decoded_manifest(&crypto)
        .map(|manifest| manifest.entries.keys().cloned().collect())
        .unwrap_or_default()
}

// ---- the workspace-root sentinel --------------------------------------------

#[test]
fn a_vanished_root_publishes_nothing_and_names_the_cause() {
    let mut harness = DriverHarness::new("safety-root-gone", "device-a");
    harness.start();
    harness.write("a.txt", b"alpha");
    harness.write("b.txt", b"beta");
    harness.edit(&["a.txt", "b.txt"]);
    let published = harness.remote.current_ref().expect("the fixture published");
    assert_eq!(head_paths(&harness).len(), 2);

    // The volume goes away (unmount, rename, container bind mount not ready).
    let moved = harness.root.with_extension("unmounted");
    fs::rename(&harness.root, &moved).expect("move the root aside");

    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();

    assert_eq!(
        harness.remote.current_ref(),
        Some(published.clone()),
        "a missing root publishes NOTHING — the empty manifest it would otherwise \
         produce deletes the workspace on every other device"
    );
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::RootUnavailable(RootFault::Missing),
    );
    assert!(
        !RootFault::Missing.reason().is_empty(),
        "the degradation carries a user-facing reason"
    );

    // The volume comes back: the engine re-observes rather than replaying.
    fs::rename(&moved, &harness.root).expect("restore the root");
    harness.clock.advance(60_000);
    harness.run_due();
    assert_eq!(
        head_paths(&harness).len(),
        2,
        "the restored workspace still holds both files"
    );
    assert!(!matches!(
        harness.engine.snapshot().degradation,
        Degradation::RootUnavailable(_)
    ));
}

#[test]
fn a_root_that_lost_its_marker_is_never_re_adopted_under_committed_state() {
    let mut harness = DriverHarness::new("safety-root-marker", "device-a");
    harness.start();
    harness.write("a.txt", b"alpha");
    harness.edit(&["a.txt"]);
    let published = harness.remote.current_ref().expect("the fixture published");

    // A different folder is now mounted at the workspace path: same path, no
    // marker, no content. Without the sentinel this walks empty and publishes a
    // manifest that deletes `a.txt` everywhere.
    fs::remove_file(marker_path(&harness.root)).expect("remove the marker");
    fs::remove_file(harness.root.join("a.txt")).expect("remove the content");

    harness.event(EngineEvent::FullScanRequired(FullScanReason::RootReplaced));
    harness.run_due();

    assert_eq!(
        harness.remote.current_ref(),
        Some(published),
        "an unmarked root under committed state publishes nothing"
    );
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::RootUnavailable(RootFault::MarkerMissing),
    );
}

#[test]
fn a_fresh_workspace_claims_its_root_without_ceremony() {
    let mut harness = DriverHarness::new("safety-root-adopt", "device-a");
    assert!(!marker_path(&harness.root).exists());
    harness.start();
    assert!(
        marker_path(&harness.root).exists(),
        "a first start claims the root: it just works, no setup step"
    );
    assert_eq!(harness.engine.snapshot().degradation, Degradation::Nominal);
}

// ---- hosted-ref integrity recovery ------------------------------------------

fn engine_stalled_on_hosted_rollback(name: &str) -> (DriverHarness, RefObservation) {
    let mut harness = DriverHarness::new(name, "device-a");
    harness.start();
    harness.write("file.txt", b"v1");
    harness.edit(&["file.txt"]);
    let v1 = harness.remote.current_ref().expect("v1").clone();
    harness.write("file.txt", b"v2");
    harness.edit(&["file.txt"]);
    let v2 = harness.remote.current_ref().expect("v2").clone();

    harness.remote.force_ref(v1.version, v1.manifest_key);
    harness.event(EngineEvent::RefChanged);
    harness.run_due();
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::IntegrityStalled
    );
    assert_eq!(harness.engine.snapshot().phase, EnginePhase::Stalled);
    (harness, v2)
}

#[test]
fn integrity_stall_clears_after_a_verified_head_advances_past_the_ratchet() {
    let (mut harness, v2) = engine_stalled_on_hosted_rollback("integrity-recovers");

    harness.remote.force_ref(v2.version + 1, v2.manifest_key);
    harness.event(EngineEvent::RefChanged);
    harness.run_due();

    assert_eq!(harness.engine.snapshot().degradation, Degradation::Nominal);
    assert_eq!(harness.engine.snapshot().phase, EnginePhase::Idle);
}

#[test]
fn restart_rederives_an_integrity_stall_from_the_durable_ratchet() {
    let (mut harness, _) = engine_stalled_on_hosted_rollback("integrity-restart");

    harness.restart();

    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::IntegrityStalled,
        "startup must reject the still-regressed hosted ref against the persisted ratchet"
    );
    assert_eq!(harness.engine.snapshot().phase, EnginePhase::Stalled);
}

// ---- the mass-deletion circuit breaker ---------------------------------------

/// Enough entries that the 25% rule, not the 64-entry floor, is the binding one.
const BULK_FILES: usize = 400;

fn seed_bulk(harness: &mut DriverHarness) {
    let names: Vec<String> = (0..BULK_FILES)
        .map(|index| format!("f{index:04}.txt"))
        .collect();
    for name in &names {
        harness.write(name, b"body");
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    harness.edit(&refs);
}

#[test]
fn a_removal_batch_over_the_threshold_is_refused_and_counted() {
    let mut harness = DriverHarness::new("safety-mass-delete", "device-a");
    harness.start();
    seed_bulk(&mut harness);
    let published = harness.remote.current_ref().expect("the fixture published");
    assert_eq!(head_paths(&harness).len(), BULK_FILES);

    let doomed = mass_deletion_threshold(BULK_FILES) + 10;
    for index in 0..doomed {
        fs::remove_file(harness.root.join(format!("f{index:04}.txt"))).expect("remove");
    }
    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();

    assert_eq!(
        harness.remote.current_ref(),
        Some(published),
        "the refused push published nothing"
    );
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::MassDeletionBlocked {
            removals: doomed,
            entries: BULK_FILES,
        },
        "the block names the counts so status can print them"
    );
    let refused = harness.engine.snapshot().refused_removals;
    assert_eq!(
        refused.len(),
        doomed,
        "the block names every refused path so an operator can read what a confirmation publishes"
    );
    assert!(
        refused.contains(&WorkspacePath::new("f0000.txt")),
        "the refused set is the paths themselves, not a count"
    );

    // Explicit confirmation is the only way through, and it is one-shot.
    assert_eq!(
        harness.engine.confirm_mass_deletion(&harness.clock),
        DeletionConfirmation::Authorized {
            removals: doomed,
            entries: BULK_FILES,
        }
    );
    harness.run_due();
    assert_eq!(
        head_paths(&harness).len(),
        BULK_FILES - doomed,
        "the confirmed deletion publishes"
    );
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::Nominal,
        "a published batch clears the block"
    );
    assert!(
        harness.engine.snapshot().refused_removals.is_empty(),
        "nothing is refused any more, so nothing is offered for confirmation"
    );
}

/// The confirmation authorises ONE push. This is the property the operator
/// surface rests on: a user who agrees to delete a directory has not agreed to
/// whatever the next unobserved gap produces.
#[test]
fn a_confirmation_authorises_exactly_one_push() {
    let mut harness = DriverHarness::new("safety-mass-delete-once", "device-a");
    harness.start();
    seed_bulk(&mut harness);

    let first = mass_deletion_threshold(BULK_FILES) + 10;
    for index in 0..first {
        fs::remove_file(harness.root.join(format!("f{index:04}.txt"))).expect("remove");
    }
    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();
    harness.event(EngineEvent::ConfirmMassDeletion);
    harness.run_due();
    let remaining = BULK_FILES - first;
    assert_eq!(head_paths(&harness).len(), remaining);

    // A second oversized batch is refused again: the earlier confirmation was
    // spent on the batch the user actually looked at.
    for index in first..BULK_FILES {
        fs::remove_file(harness.root.join(format!("f{index:04}.txt"))).expect("remove");
    }
    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();
    assert_eq!(
        head_paths(&harness).len(),
        remaining,
        "the second oversized batch published nothing"
    );
    assert!(
        matches!(
            harness.engine.snapshot().degradation,
            Degradation::MassDeletionBlocked { .. }
        ),
        "the guard re-armed itself rather than staying confirmed"
    );
}

/// The paths a bulk fixture deletes to trip the breaker.
fn doomed_paths(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("f{index:04}.txt")).collect()
}

fn delete_locally(harness: &DriverHarness, paths: &[String]) {
    for name in paths {
        fs::remove_file(harness.root.join(name)).expect("remove");
    }
}

/// The head another trusted device would publish: the current head's entries,
/// minus `dropped`, plus `added`. Advances the ref exactly as a peer's CAS would.
fn peer_head(harness: &DriverHarness, dropped: &[String], added: &[(&str, &[u8])]) -> Manifest {
    let crypto = super::engine_test_support::test_crypto();
    let mut entries = harness
        .remote
        .decoded_manifest(&crypto)
        .expect("a head to build the peer's on")
        .entries;
    for path in dropped {
        entries.remove(&WorkspacePath::new(path));
    }
    for (path, bytes) in added {
        entries.insert(
            WorkspacePath::new(*path),
            harness.remote.publish_blob(&crypto, bytes),
        );
    }
    Manifest::new(crypto.key_epoch(), entries)
}

fn peer_publishes(harness: &DriverHarness, dropped: &[String], added: &[(&str, &[u8])]) {
    let crypto = super::engine_test_support::test_crypto();
    let manifest = peer_head(harness, dropped, added);
    harness.remote.publish_manifest(&crypto, &manifest);
}

/// The regression: a device parked on the deletion breaker went dark. The push
/// error aborted the cycle before the pull, so the one thing that could have
/// cleared the block — learning that the remote had already removed those very
/// paths — was the one thing the block prevented.
#[test]
fn a_blocked_device_still_receives_and_the_block_clears_when_the_remote_agrees() {
    let mut harness = DriverHarness::new("safety-mass-delete-still-pulls", "device-a");
    harness.start();
    seed_bulk(&mut harness);
    let doomed = doomed_paths(mass_deletion_threshold(BULK_FILES) + 10);

    // Both devices lose the same tree: this one from disk (the removal it wants
    // to publish), the peer by publishing the deletion first.
    delete_locally(&harness, &doomed);
    peer_publishes(&harness, &doomed, &[("sync-probe.txt", b"from the peer")]);
    let peer_ref = harness.remote.current_ref().expect("the peer published");

    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();

    assert_eq!(
        harness.read("sync-probe.txt"),
        b"from the peer",
        "a device that may not publish must still receive; without this the block \
         is a trap the device cannot be dug out of from either end"
    );
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::Nominal,
        "the remote had already resolved every refused path, so the batch \
         evaporated and no confirmation was ever needed"
    );
    assert!(harness.engine.snapshot().refused_removals.is_empty());
    assert_eq!(
        harness.remote.current_ref(),
        Some(peer_ref),
        "the block cleared by RECEIVING, not by publishing a batch nobody confirmed"
    );
    assert_eq!(
        head_paths(&harness).len(),
        BULK_FILES - doomed.len() + 1,
        "the surviving files plus the peer's new one"
    );
}

/// The other half of the same split: pulling while a batch is refused must not
/// weaken the guard. When the remote still holds the paths, the local deletion is
/// genuinely this device's own decision and still waits for a human.
#[test]
fn a_batch_the_remote_still_holds_stays_blocked_while_the_device_keeps_receiving() {
    let mut harness = DriverHarness::new("safety-mass-delete-still-blocked", "device-a");
    harness.start();
    seed_bulk(&mut harness);
    let doomed = doomed_paths(mass_deletion_threshold(BULK_FILES) + 10);

    delete_locally(&harness, &doomed);
    // The peer keeps every doomed path and adds one file.
    peer_publishes(&harness, &[], &[("sync-probe.txt", b"from the peer")]);
    let peer_ref = harness.remote.current_ref().expect("the peer published");

    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();

    assert_eq!(
        harness.read("sync-probe.txt"),
        b"from the peer",
        "receiving continues while publishing waits"
    );
    assert_eq!(
        harness.remote.current_ref(),
        Some(peer_ref),
        "an unconfirmed oversized batch publishes NOTHING, pull or no pull"
    );
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::MassDeletionBlocked {
            removals: doomed.len(),
            entries: BULK_FILES + 1,
        },
        "the retry re-derived the batch against the post-pull ancestor, so the \
         counts status shows are the ones a confirmation would publish"
    );
    assert_eq!(
        harness.engine.snapshot().refused_removals.len(),
        doomed.len()
    );
}

/// A ref whose CAS one staged peer publish always beats to the swap: the race
/// that leaves a confirmed push holding a version the hosted ref has moved past.
struct PeerWinsTheCas<'a> {
    remote: &'a FakeRemote,
    staged: RefCell<Option<Manifest>>,
}

impl RemoteRef for PeerWinsTheCas<'_> {
    fn read_ref(&self) -> Result<Option<RefObservation>, super::TransportError> {
        self.remote.read_ref()
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &super::ManifestKey,
    ) -> Result<CasOutcome, super::TransportError> {
        if let Some(manifest) = self.staged.borrow_mut().take() {
            self.remote
                .publish_manifest(&super::engine_test_support::test_crypto(), &manifest);
        }
        self.remote
            .compare_and_swap(expected_version, new_manifest_key)
    }
}

/// A confirmed push from a device the hosted ref has already left behind must
/// converge. Nothing was published, so the cycle pulls the winner against the
/// untouched ancestor and retries — and when the winner already removed those
/// paths, the retry has nothing left to publish at all.
#[test]
fn a_confirmed_push_that_loses_the_cas_converges_instead_of_looping() {
    let mut harness = DriverHarness::new("safety-mass-delete-cas-race", "device-a");
    harness.start();
    seed_bulk(&mut harness);
    let doomed = doomed_paths(mass_deletion_threshold(BULK_FILES) + 10);
    delete_locally(&harness, &doomed);
    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();
    assert!(matches!(
        harness.engine.snapshot().degradation,
        Degradation::MassDeletionBlocked { .. }
    ));

    let staged = peer_head(&harness, &doomed, &[("sync-probe.txt", b"from the peer")]);
    harness.event(EngineEvent::ConfirmMassDeletion);
    let racer = PeerWinsTheCas {
        remote: &harness.remote,
        staged: RefCell::new(Some(staged)),
    };
    let io = EngineIo {
        objects: &harness.remote,
        refs: &racer,
        clock: &harness.clock,
    };
    harness
        .engine
        .run_due_work(&io)
        .expect("a lost CAS is recovered inside the cycle, never fatal");

    assert_eq!(harness.read("sync-probe.txt"), b"from the peer");
    assert_eq!(
        harness.engine.snapshot().degradation,
        Degradation::Nominal,
        "the winner had already removed every refused path"
    );
    assert_eq!(head_paths(&harness).len(), BULK_FILES - doomed.len() + 1);
    assert_eq!(
        harness.engine.snapshot().phase,
        EnginePhase::Idle,
        "converged, not re-armed: a device that keeps retrying a settled batch is \
         the loop this test exists to catch"
    );
}

/// Confirming when nothing is refused must not pre-authorise the next refusal:
/// a standing "yes" is the guard's entire blast radius handed away.
#[test]
fn a_confirmation_with_nothing_refused_authorises_nothing() {
    let mut harness = DriverHarness::new("safety-mass-delete-idle", "device-a");
    harness.start();
    seed_bulk(&mut harness);

    let outcome = harness.engine.confirm_mass_deletion(&harness.clock);

    let doomed = mass_deletion_threshold(BULK_FILES) + 10;
    for index in 0..doomed {
        fs::remove_file(harness.root.join(format!("f{index:04}.txt"))).expect("remove");
    }
    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();
    assert_eq!(
        head_paths(&harness).len(),
        BULK_FILES,
        "the earlier confirmation did not wave this batch through"
    );
    assert!(matches!(
        harness.engine.snapshot().degradation,
        Degradation::MassDeletionBlocked { .. }
    ));
    assert_eq!(outcome, DeletionConfirmation::NotBlocked);
}

#[test]
fn an_ordinary_deletion_batch_publishes_without_confirmation() {
    let mut harness = DriverHarness::new("safety-small-delete", "device-a");
    harness.start();
    seed_bulk(&mut harness);

    let removed = mass_deletion_threshold(BULK_FILES) - 1;
    for index in 0..removed {
        fs::remove_file(harness.root.join(format!("f{index:04}.txt"))).expect("remove");
    }
    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();

    assert_eq!(
        head_paths(&harness).len(),
        BULK_FILES - removed,
        "a deletion below the threshold needs no ceremony"
    );
    assert_eq!(harness.engine.snapshot().degradation, Degradation::Nominal);
}

#[test]
fn the_deletion_threshold_never_falls_below_the_floor() {
    assert_eq!(mass_deletion_threshold(0), 64);
    assert_eq!(mass_deletion_threshold(100), 64);
    assert_eq!(mass_deletion_threshold(400), 100);
}

// ---- the periodic audit -------------------------------------------------------

#[test]
fn the_periodic_audit_converges_a_change_no_watcher_ever_reported() {
    let mut harness = DriverHarness::new("safety-audit", "device-a");
    harness.start();
    harness.write("a.txt", b"alpha");
    harness.edit(&["a.txt"]);

    // A change with NO event at all — exactly what a dropped watcher event, a
    // sleeping laptop, or a network mount produces.
    harness.write("silent.txt", b"never announced");
    assert!(
        !head_paths(&harness).contains(&WorkspacePath::new("silent.txt")),
        "nothing has told the engine about it yet"
    );

    harness.clock.advance(AUDIT_INTERVAL_MAX_MS + 1);
    harness.run_due();

    assert!(
        head_paths(&harness).contains(&WorkspacePath::new("silent.txt")),
        "the audit is what makes Everything Syncs true rather than aspirational"
    );
}

// ---- unsyncable paths ---------------------------------------------------------

#[test]
fn an_unsupported_object_is_reported_and_the_rest_keeps_syncing() {
    let mut harness = DriverHarness::new("safety-sock", "device-a");
    harness.start();
    harness.write("good.txt", b"ordinary bytes");
    // A unix socket is not a file, directory, or symlink. Before the unsyncable
    // classification this was `InvalidData` -> CycleError::Fatal -> dead engine.
    let socket = harness.root.join("app.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();

    let head = head_paths(&harness);
    assert!(
        head.contains(&WorkspacePath::new("good.txt")),
        "one unsyncable object does not stop the workspace"
    );
    assert!(!head.contains(&WorkspacePath::new("app.sock")));
    let snapshot = harness.engine.snapshot();
    let record = snapshot
        .unsyncable
        .get(&WorkspacePath::new("app.sock"))
        .expect("the socket is reported, not silently dropped");
    assert_eq!(record.reason, UnsyncableReason::UnsupportedKind);
    assert!(!record.reason.remedy().is_empty());

    // Removing it clears the attention item on the next authoritative scan.
    drop(listener);
    fs::remove_file(&socket).expect("remove socket");
    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();
    assert!(
        harness.engine.snapshot().unsyncable.is_empty(),
        "a fixed path stops asking for attention"
    );
}

#[test]
fn a_name_no_peer_could_decode_is_never_published() {
    let mut harness = DriverHarness::new("safety-badname", "device-a");
    harness.start();
    harness.write("ok.txt", b"fine");
    // A POSIX filename containing a backslash. The workspace normalizer rewrites
    // it, so every peer's manifest decode rejects it — one such file used to take
    // down the whole fleet's engines.
    harness.write("notes\\draft.txt", b"from a windows archive");

    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherDisconnected,
    ));
    harness.run_due();

    let head = head_paths(&harness);
    assert!(head.contains(&WorkspacePath::new("ok.txt")));
    assert!(
        !head.contains(&WorkspacePath::new("notes\\draft.txt")),
        "the writer refuses exactly what the reader would refuse"
    );
    assert_eq!(
        harness
            .engine
            .snapshot()
            .unsyncable
            .get(&WorkspacePath::new("notes\\draft.txt"))
            .map(|record| record.reason),
        Some(UnsyncableReason::UnrepresentablePath),
    );
}

// ---- quarantine lifecycle ------------------------------------------------------

#[test]
fn a_committed_pull_leaves_no_quarantined_preimage_behind() {
    let mut engine = TestEngine::new("safety-quarantine");
    engine.write("f.txt", b"local bytes");
    engine.push(&["f.txt"]);

    let remote = engine.remote_file(b"peer bytes");
    engine.publish(&[("f.txt", remote)]);
    engine.pull();

    assert_eq!(engine.read("f.txt"), b"peer bytes");
    let quarantine = engine
        .root()
        .join(super::ENGINE_STATE_DIR)
        .join("quarantine");
    let leftovers = fs::read_dir(&quarantine)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        leftovers, 0,
        "a quarantined preimage is dead the moment its intent commits; leaving it \
         converges on a second full copy of the workspace"
    );
}

// ---- the durable unsyncable set ------------------------------------------------

#[test]
fn the_unsyncable_set_survives_a_restart_and_clears_on_repair() {
    let workspace = TempWorkspace::new("safety-unsyncable-store").expect("temp workspace");
    let path = workspace.root().join("manifest_engine.sqlite3");
    let denied = WorkspacePath::new("locked/file.txt");

    {
        let mut store = ManifestStore::open(&path).expect("open");
        let mut observed = std::collections::BTreeMap::new();
        observed.insert(
            denied.clone(),
            UnsyncableRecord::new(UnsyncableReason::PermissionDenied, Some(13), 42),
        );
        store
            .record_unsyncable(&observed, &BTreeSet::new())
            .expect("record");
    }

    let mut store = ManifestStore::open(&path).expect("reopen");
    let entries = store.unsyncable().expect("read");
    assert_eq!(
        entries.get(&denied).map(|record| record.reason),
        Some(UnsyncableReason::PermissionDenied),
        "status must be able to report this the moment the daemon starts"
    );
    assert_eq!(entries[&denied].errno, Some(13));

    store
        .record_unsyncable(
            &std::collections::BTreeMap::new(),
            &[denied.clone()].into_iter().collect(),
        )
        .expect("clear");
    assert!(store.unsyncable().expect("read").is_empty());
}
