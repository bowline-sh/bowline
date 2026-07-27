//! CI-runnable scale fixtures (Plan 111 Step 5): the numbers the release
//! `prove-candidate` gate reads as `fixtureBudget`. Two measurements share one
//! stat-walk primitive so the 10k C5 invariant test and the 100k restart fixture
//! never grow a second copy of the seed-and-walk logic.
//!
//! The 100k measurement is expensive (110k on-disk files), so it runs only when
//! `BOWLINE_ENGINE_FIXTURE_OUT` names an output path — the release fixture stage
//! opts in; a plain `./scripts/verify --profile rust` run skips the heavy build.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use super::engine_test_support::{
    FakeRemote, observe_present, open_store, publish_test_tree_ledgered,
};
use super::fs_guard::Observed;
use super::manifest::{
    BlobKey, FileMode, KeyEpoch, Manifest, ManifestEntry, WorkspaceCrypto, WorkspacePath,
};
use super::stat_walk::stat_walk;
use super::store::FileRecord;
use crate::policy::UserPolicy;
use crate::workspace::TempWorkspace;
use bowline_core::ids::ContentId;

/// The 10k stat-walk file count (the steady-state C5 audit subject).
pub(super) const STAT_WALK_FILES: usize = 10_000;
/// The 100k restart file count (the Plan 108 scale-claim / chunking-trigger
/// subject).
pub(super) const RESTART_FILES: usize = 100_000;

/// One stat-walk measurement: the walk finds an unchanged fixture entirely clean
/// and hashes nothing. Returned as data so both the invariant assertion and the
/// fixture JSON read the same numbers.
#[derive(Debug, Clone, Copy)]
pub(super) struct StatWalkMeasurement {
    pub files: usize,
    pub millis: u128,
    pub scanned: u64,
    pub hashes: u64,
    pub dirty: usize,
}

/// Write `files` payload files, seed the ancestor by statting each (no hashing),
/// then time a single stat walk over the unchanged tree — the steady-state audit
/// cost and, at 100k, the restart cost (restart = one stat walk).
pub(super) fn measure_stat_walk(root: &Path, files: usize) -> StatWalkMeasurement {
    for index in 0..files {
        std::fs::write(root.join(format!("f{index:06}.dat")), b"payload").expect("write fixture");
    }

    let policy = UserPolicy::load(root).expect("load policy");
    let mut ancestor: BTreeMap<WorkspacePath, FileRecord> = BTreeMap::new();
    for index in 0..files {
        let path = WorkspacePath::new(format!("f{index:06}.dat"));
        let observed = observe_present(root, &path).expect("present");
        ancestor.insert(path, record_from_observed(&observed));
    }

    let started = Instant::now();
    let walk = stat_walk(root, &policy, &ancestor).expect("stat walk");
    let elapsed = started.elapsed();

    StatWalkMeasurement {
        files,
        millis: elapsed.as_millis(),
        scanned: walk.scanned,
        hashes: walk.hashes,
        dirty: walk.dirty.len(),
    }
}

/// The manifest-shaped publish costs at scale, measured on the Merkle form.
///
/// `full_publish_bytes` is the whole tree — what a first push or a fresh device
/// pays once. `edit_publish_bytes` is what a ONE-FILE edit costs afterwards, and
/// it is the number that decides whether the engine's change-proportional claim
/// is true: under the flat manifest it was identical to `full_publish_bytes`.
#[derive(Debug, Clone, Copy)]
pub(super) struct PublishCostMeasurement {
    pub full_publish_bytes: u64,
    pub full_publish_nodes: u64,
    pub full_publish_ms: u128,
    pub edit_publish_bytes: u64,
    pub edit_publish_nodes: u64,
    pub edit_publish_ms: u128,
    pub peak_memory_bytes: u64,
}

/// The fixture's directory shape. A real `~/Code` is nested; a single flat
/// directory of 100k files is the tree's worst case and would measure the wrong
/// thing, so the fixture uses a three-level fan-out closer to a source tree.
const FIXTURE_FANOUT: usize = 64;

fn scale_manifest(files: usize, edited: Option<usize>) -> Manifest {
    let mut entries: BTreeMap<WorkspacePath, ManifestEntry> = BTreeMap::new();
    for index in 0..files {
        let outer = index % FIXTURE_FANOUT;
        let inner = (index / FIXTURE_FANOUT) % FIXTURE_FANOUT;
        let salt = if edited == Some(index) { 1 } else { 0 };
        entries.insert(
            WorkspacePath::new(format!("p{outer:03}/m{inner:03}/f{index:06}.dat")),
            ManifestEntry::File {
                size: 7,
                mode: FileMode::new(0o644),
                content_id: ContentId::new(format!("cid_{:058x}", index + salt * files)),
                blob_key: BlobKey::new(format!("b_{:062x}", index + salt * files)),
                key_epoch: KeyEpoch::new(1),
            },
        );
    }
    Manifest::new(KeyEpoch::new(1), entries)
}

/// Publish a `files`-entry manifest as a tree, then publish it again with one
/// entry changed against the SAME object store, and report both costs.
///
/// The second publish reuses every node object the first one produced, so what
/// it uploads is exactly the path from the edited file to the root.
pub(super) fn measure_publish_cost(files: usize) -> PublishCostMeasurement {
    let crypto = WorkspaceCrypto::new("scale-fixture-workspace", [7u8; 32], KeyEpoch::new(1));
    let objects = FakeRemote::new();
    let workspace = TempWorkspace::new("scale-fixture-publish").expect("temp workspace");
    let mut store = open_store(workspace.root());

    let manifest = scale_manifest(files, None);
    let full_started = Instant::now();
    publish_test_tree_ledgered(&objects, &crypto, &manifest, &mut store);
    let full_publish_ms = full_started.elapsed().as_millis();
    let (full_publish_nodes, full_publish_bytes) = objects.manifest_put_totals();

    // Sample residency at peak — after the 100k map and the whole node set are
    // live — so the number reflects the working set a publish actually holds.
    let peak_memory_bytes = resident_bytes();

    let edited = scale_manifest(files, Some(files / 2));
    let edit_started = Instant::now();
    publish_test_tree_ledgered(&objects, &crypto, &edited, &mut store);
    let edit_publish_ms = edit_started.elapsed().as_millis();
    let (nodes_after, bytes_after) = objects.manifest_put_totals();

    PublishCostMeasurement {
        full_publish_bytes,
        full_publish_nodes,
        full_publish_ms,
        edit_publish_bytes: bytes_after - full_publish_bytes,
        edit_publish_nodes: nodes_after - full_publish_nodes,
        edit_publish_ms,
        peak_memory_bytes,
    }
}

/// Resident set size in bytes, sampled through `ps` so the reading is
/// dependency-free and works on both the macOS captain host and the Linux CI
/// host. A zero reading (ps unavailable) is reported honestly rather than faked.
fn resident_bytes() -> u64 {
    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output();
    let Ok(output) = output else {
        return 0;
    };
    let kib = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    kib * 1024
}

pub(super) fn record_from_observed(observed: &Observed) -> FileRecord {
    FileRecord {
        kind: observed.kind,
        size: observed.size,
        mode: observed.mode,
        symlink_target: observed.symlink_target.clone(),
        content_id: None,
        blob_key: None,
        key_epoch: None,
        fingerprint: observed.fingerprint,
        hashed_at: None,
        verified_at: None,
    }
}

// The env var the release fixture stage sets to opt into the expensive run and
// receive the JSON artifact it forwards as `ctx.evidence.fixtureBudget`.
const FIXTURE_OUT_ENV: &str = "BOWLINE_ENGINE_FIXTURE_OUT";

#[test]
fn scale_fixture_budget_emits_statwalk_and_restart_json() {
    let Some(out_path) = std::env::var_os(FIXTURE_OUT_ENV) else {
        // The 100k fixture builds 110k files; only the opt-in release stage pays
        // that. A normal rust-profile verify run has nothing to assert here.
        return;
    };

    let stat_workspace = TempWorkspace::new("scale-fixture-statwalk").expect("temp workspace");
    let stat_walk = measure_stat_walk(stat_workspace.root(), STAT_WALK_FILES);

    let restart_workspace = TempWorkspace::new("scale-fixture-restart").expect("temp workspace");
    let restart_walk = measure_stat_walk(restart_workspace.root(), RESTART_FILES);
    let publish = measure_publish_cost(RESTART_FILES);

    // The absolute invariants are asserted regardless of the debug-build timing:
    // a walk hashes nothing (C5) and an unchanged fixture is entirely clean. The
    // millisecond budgets are release-build targets the JS release gate enforces
    // on the emitted numbers, recorded (not enforced) here.
    assert_eq!(stat_walk.hashes, 0, "10k stat walk hashes nothing (C5)");
    assert_eq!(stat_walk.dirty, 0, "10k unchanged fixture is clean");
    assert_eq!(stat_walk.scanned, STAT_WALK_FILES as u64);
    assert_eq!(
        restart_walk.hashes, 0,
        "100k restart stat walk hashes nothing"
    );
    assert_eq!(restart_walk.dirty, 0, "100k unchanged fixture is clean");
    assert!(
        publish.full_publish_bytes > 0,
        "the first publish sealed a nonempty tree"
    );
    // The claim the Merkle manifest exists to make: an edit costs the edit. The
    // flat form published the whole manifest here, so these were equal.
    assert!(
        publish.edit_publish_bytes * 100 < publish.full_publish_bytes,
        "a one-file edit published {} of {} bytes at {RESTART_FILES} entries",
        publish.edit_publish_bytes,
        publish.full_publish_bytes,
    );

    let json = serde_json::json!({
        "statWalk": {
            "files": stat_walk.files,
            "millis": stat_walk.millis,
            "hashes": stat_walk.hashes,
        },
        "restart": {
            "files": restart_walk.files,
            "millis": restart_walk.millis,
            "fullPublishBytes": publish.full_publish_bytes,
            "fullPublishNodes": publish.full_publish_nodes,
            "fullPublishMs": publish.full_publish_ms,
            "editPublishBytes": publish.edit_publish_bytes,
            "editPublishNodes": publish.edit_publish_nodes,
            "editPublishMs": publish.edit_publish_ms,
            "peakMemoryBytes": publish.peak_memory_bytes,
        },
    });
    let serialized = serde_json::to_vec(&json).expect("serialize fixture json");
    std::fs::write(Path::new(&out_path), serialized).expect("write fixture json");

    println!(
        "scale fixture: statWalk {}f {}ms; restart {}f {}ms; publish full={}B/{}nodes/{}ms \
         edit={}B/{}nodes/{}ms peakRss={}B",
        stat_walk.files,
        stat_walk.millis,
        restart_walk.files,
        restart_walk.millis,
        publish.full_publish_bytes,
        publish.full_publish_nodes,
        publish.full_publish_ms,
        publish.edit_publish_bytes,
        publish.edit_publish_nodes,
        publish.edit_publish_ms,
        publish.peak_memory_bytes,
    );
}
