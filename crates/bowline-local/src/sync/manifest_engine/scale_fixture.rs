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

use super::fs_guard::{Observed, observe};
use super::manifest::{
    BlobKey, FileMode, KeyEpoch, Manifest, ManifestEntry, WorkspaceCrypto, WorkspacePath,
    seal_manifest,
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
        let observed = observe(root, &path).expect("observe").expect("present");
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

/// The manifest-shaped restart costs: canonical + sealed size, the CPU to seal
/// one 100k manifest, a local transfer proxy (writing the sealed bytes), and the
/// resident footprint at peak manifest residency.
#[derive(Debug, Clone, Copy)]
pub(super) struct RestartManifestMeasurement {
    pub manifest_bytes: usize,
    pub seal_cpu_ms: u128,
    pub transfer_ms: u128,
    pub peak_memory_bytes: u64,
}

/// Build an in-memory `files`-entry manifest and measure the reseal-and-ship
/// costs. `transfer_ms` is a local proxy — the wall time to write the sealed
/// bytes to disk — because a true network transfer is not available in a unit
/// fixture; it bounds the local half of the restart cost honestly.
pub(super) fn measure_restart_manifest(files: usize) -> RestartManifestMeasurement {
    let mut entries: BTreeMap<WorkspacePath, ManifestEntry> = BTreeMap::new();
    for index in 0..files {
        entries.insert(
            WorkspacePath::new(format!("f{index:06}.dat")),
            ManifestEntry::File {
                size: 7,
                mode: FileMode::new(0o644),
                content_id: ContentId::new(format!("cid_{index:058x}")),
                blob_key: BlobKey::new(format!("b_{index:062x}")),
                key_epoch: KeyEpoch::new(1),
            },
        );
    }
    let manifest = Manifest::new(KeyEpoch::new(1), entries);
    let canonical = manifest.to_canonical_bytes().expect("canonical bytes");

    let crypto = WorkspaceCrypto::new("scale-fixture-workspace", [7u8; 32], KeyEpoch::new(1));
    let seal_started = Instant::now();
    let sealed = seal_manifest(&crypto, &canonical).expect("seal manifest");
    let seal_cpu_ms = seal_started.elapsed().as_millis();

    // Sample residency at peak — after the 100k map plus the canonical and sealed
    // buffers are all live — so the number reflects the working set the restart
    // actually holds.
    let peak_memory_bytes = resident_bytes();

    let workspace = TempWorkspace::new("scale-fixture-transfer").expect("temp workspace");
    let transfer_target = workspace.root().join("sealed.manifest");
    let transfer_started = Instant::now();
    std::fs::write(&transfer_target, sealed.as_bytes()).expect("write sealed manifest");
    let transfer_ms = transfer_started.elapsed().as_millis();

    RestartManifestMeasurement {
        manifest_bytes: canonical.len(),
        seal_cpu_ms,
        transfer_ms,
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
    let restart_manifest = measure_restart_manifest(RESTART_FILES);

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
        restart_manifest.manifest_bytes > 0,
        "sealed a nonempty manifest"
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
            "manifestBytes": restart_manifest.manifest_bytes,
            "sealCpuMs": restart_manifest.seal_cpu_ms,
            "transferMs": restart_manifest.transfer_ms,
            "peakMemoryBytes": restart_manifest.peak_memory_bytes,
        },
    });
    let serialized = serde_json::to_vec(&json).expect("serialize fixture json");
    std::fs::write(Path::new(&out_path), serialized).expect("write fixture json");

    println!(
        "scale fixture: statWalk {}f {}ms; restart {}f {}ms manifest={}B seal={}ms transfer={}ms peakRss={}B",
        stat_walk.files,
        stat_walk.millis,
        restart_walk.files,
        restart_walk.millis,
        restart_manifest.manifest_bytes,
        restart_manifest.seal_cpu_ms,
        restart_manifest.transfer_ms,
        restart_manifest.peak_memory_bytes,
    );
}
