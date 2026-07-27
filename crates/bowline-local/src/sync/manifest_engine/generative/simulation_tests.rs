//! The Trinity analogue: a seeded storm of transport faults and crashes, then
//! quiescence.
//!
//! Every object and ref call is rolled against a seeded fault schedule, the
//! workspace is snapshotted and rolled back to simulate power loss mid-cycle,
//! and the engine is restarted at arbitrary points. Nothing is asserted *during*
//! the storm — partial states are legal there. The contract is what happens
//! after: once the faults stop, both devices must reach a state where each
//! workspace matches the other and matches the published head.
//!
//! Failures print the seed and the exact fault schedule that produced them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::super::engine_test_support::test_crypto;
use super::super::manifest::{ManifestEntry, WorkspaceCrypto, WorkspacePath};
use super::super::{EngineEvent, FullScanReason};
use super::chaos_transport::{ChaosRemote, FaultRate};
use super::fleet::{Device, FleetError};
use super::rng::{Rng, Seed};
use super::tree::{FileBody, TreeSpec, generate_mutations};
use super::{base_seed, case_count, replay_hint};

/// Cases per storm in CI. Each runs a full two-device fleet through a fault
/// storm plus a quiescing phase.
const CI_CASES: u32 = 6;
/// Cycles of storm before the faults are switched off.
const STORM_STEPS: u32 = 24;
/// One transport call in five fails during the storm — high enough that a
/// multi-round-trip push is very unlikely to complete intact, low enough that
/// progress still happens.
const STORM_RATE: FaultRate = FaultRate::new(1, 5);

#[test]
fn a_fault_storm_converges_once_it_stops() {
    let base = base_seed();
    for index in 0..case_count(CI_CASES) {
        let seed = base.case(index);
        if let Err(failure) = run_storm(seed) {
            panic!(
                "generative simulation failure\nreplay with: {}\n{failure}",
                replay_hint(base, index)
            );
        }
    }
}

/// A crash exactly at the moment a cycle is about to publish is the case the
/// intent journal exists for; assert it explicitly rather than leaving it to
/// the storm's dice.
#[test]
fn a_rollback_mid_cycle_still_converges() {
    let seed = base_seed().case(u32::MAX);
    let remote = ChaosRemote::new(seed, FaultRate::CALM);
    let mut device = match Device::new("sim-rollback", "device-a") {
        Ok(device) => device,
        Err(error) => panic!("device: {error}"),
    };
    let mut rng = Rng::from_seed(seed);
    let tree = TreeSpec::generate(&mut rng, 5);
    if let Err(error) = tree.materialize(device.root()) {
        panic!("materialize: {error}");
    }
    expect(device.start(&remote));
    expect(device.settle(&remote).map(|_| ()));

    let snapshot = snapshot_dir("sim-rollback");
    expect(device.snapshot_to(&snapshot));

    // Edit, push, then roll the whole device (workspace and engine state) back
    // to the pre-edit snapshot while the remote keeps the published head.
    let mutations = generate_mutations(&mut rng, &tree, 3);
    for mutation in &mutations {
        if let Err(error) = mutation.apply_to_disk(device.root()) {
            panic!("mutate: {error}");
        }
    }
    device.wake(EngineEvent::FullScanRequired(
        FullScanReason::WatcherOverflow,
    ));
    expect(device.settle(&remote).map(|_| ()));
    expect(device.crash_to_snapshot(&snapshot, &remote));
    expect(device.settle(&remote).map(|_| ()));

    let crypto = test_crypto();
    let files = match device.files() {
        Ok(files) => files,
        Err(error) => panic!("read files: {error}"),
    };
    if let Some(mismatch) = head_mismatch(&remote, &crypto, &files) {
        panic!("rollback did not reconverge with the head:\n{mismatch}");
    }
    let _ = std::fs::remove_dir_all(&snapshot);
}

fn run_storm(seed: Seed) -> Result<(), String> {
    let remote = ChaosRemote::new(seed.case(1), STORM_RATE);
    let mut rng = Rng::from_seed(seed);
    let mut first = Device::new(&format!("sim-storm-a-{}", seed.get()), "device-a")
        .map_err(|error| error.to_string())?;
    let mut second = Device::new(&format!("sim-storm-b-{}", seed.get()), "device-b")
        .map_err(|error| error.to_string())?;

    let tree = TreeSpec::generate(&mut rng, 6);
    tree.materialize(first.root())
        .map_err(|error| error.to_string())?;
    // Startup itself runs under the storm: a failed start is a legal outcome the
    // driver retries, so it is tolerated exactly like a failed cycle.
    let _ = first.start(&remote);
    let _ = second.start(&remote);

    let snapshot = snapshot_dir(&format!("sim-storm-{}", seed.get()));
    for _ in 0..STORM_STEPS {
        storm_step(&mut rng, &mut first, &mut second, &remote, &snapshot)?;
    }

    // Quiesce: faults off, both devices restarted, then settled to a fixpoint.
    remote.set_rate(FaultRate::CALM);
    first.restart(&remote).map_err(|error| error.to_string())?;
    second.restart(&remote).map_err(|error| error.to_string())?;
    let outcome = settle_pair(&mut first, &mut second, &remote);
    let _ = std::fs::remove_dir_all(&snapshot);
    outcome.map_err(|failure| format!("{failure}\nfaults injected:\n{}", render(&remote)))
}

fn storm_step(
    rng: &mut Rng,
    first: &mut Device,
    second: &mut Device,
    remote: &ChaosRemote,
    snapshot: &Path,
) -> Result<(), String> {
    // A user edit lands on one device or the other.
    let editing = if rng.chance(1, 2) {
        &mut *first
    } else {
        &mut *second
    };
    let current = editing.files().map_err(|error| error.to_string())?;
    let edit_count = rng.in_range(0, 2);
    for mutation in generate_mutations(rng, &current, edit_count) {
        mutation
            .apply_to_disk(editing.root())
            .map_err(|error| error.to_string())?;
    }
    editing.wake(EngineEvent::Paths(current.paths().into_iter().collect()));
    editing.wake(EngineEvent::RefChanged);
    editing.advance(2_000);

    // Cycles are expected to fail under the storm; only a panic or a hang would
    // be a defect, so errors are deliberately swallowed here.
    let _ = first.step(remote);
    let _ = second.step(remote);
    first.advance(rng.in_range(0, 3_000).into());
    second.advance(rng.in_range(0, 3_000).into());

    if rng.chance(1, 6) {
        first
            .snapshot_to(snapshot)
            .map_err(|error| error.to_string())?;
    }
    if rng.chance(1, 8) && snapshot.exists() {
        let _ = first.crash_to_snapshot(snapshot, remote);
    } else if rng.chance(1, 6) {
        let _ = first.restart(remote);
    }
    Ok(())
}

/// Settle both devices alternately until neither workspace changes and both
/// agree, then check the shared state against the published head.
fn settle_pair(
    first: &mut Device,
    second: &mut Device,
    remote: &ChaosRemote,
) -> Result<(), String> {
    const QUIESCE_ROUNDS: u32 = 8;
    let crypto = test_crypto();
    for _ in 0..QUIESCE_ROUNDS {
        let before_first = first.files().map_err(|error| error.to_string())?;
        let before_second = second.files().map_err(|error| error.to_string())?;
        first.settle(remote).map_err(|error| error.to_string())?;
        second.settle(remote).map_err(|error| error.to_string())?;
        let after_first = first.files().map_err(|error| error.to_string())?;
        let after_second = second.files().map_err(|error| error.to_string())?;
        if after_first != after_second
            || after_first != before_first
            || after_second != before_second
        {
            continue;
        }
        return match head_mismatch(remote, &crypto, &after_first) {
            None => Ok(()),
            Some(mismatch) => Err(format!(
                "workspaces agree but the head does not:\n{mismatch}"
            )),
        };
    }
    Err(format!(
        "no fixpoint after {QUIESCE_ROUNDS} calm rounds:\n  a={:?}\n  b={:?}",
        first.files().map(|tree| tree.paths()),
        second.files().map(|tree| tree.paths())
    ))
}

/// Compare the published head's file entries against a workspace, by content
/// identity and permission bits. The manifest carries the full `st_mode`, so
/// both sides are masked to `0o777` before comparison — the file-type bits are
/// not a sync outcome.
fn head_mismatch(
    remote: &ChaosRemote,
    crypto: &WorkspaceCrypto,
    files: &TreeSpec,
) -> Option<String> {
    let manifest = remote.decoded_manifest(crypto)?;
    let published: BTreeMap<&WorkspacePath, (String, u32)> = manifest
        .entries
        .iter()
        .filter_map(|(path, entry)| match entry {
            ManifestEntry::File {
                content_id, mode, ..
            } => Some((path, (content_id.as_str().to_string(), mode.get() & 0o777))),
            _ => None,
        })
        .collect();
    let on_disk: BTreeMap<&WorkspacePath, (String, u32)> = files
        .files()
        .iter()
        .map(|(path, body): (&WorkspacePath, &FileBody)| {
            (
                path,
                (
                    crypto.content_id(&body.bytes).as_str().to_string(),
                    body.mode.get(),
                ),
            )
        })
        .collect();
    if published == on_disk {
        return None;
    }
    let mut lines = Vec::new();
    let mut paths: Vec<&WorkspacePath> = published.keys().copied().collect();
    paths.extend(on_disk.keys().copied());
    paths.sort();
    paths.dedup();
    for path in paths {
        let head = published.get(path);
        let disk = on_disk.get(path);
        if head != disk {
            lines.push(format!("  {}: head={head:?} disk={disk:?}", path.as_str()));
        }
    }
    Some(lines.join("\n"))
}

fn render(remote: &ChaosRemote) -> String {
    remote
        .injected()
        .iter()
        .map(|fault| format!("  {fault}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bowline-sim-snapshot-{}-{name}",
        std::process::id()
    ))
}

fn expect(result: Result<(), FleetError>) {
    if let Err(error) = result {
        panic!("{error}");
    }
}
