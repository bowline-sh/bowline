use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use bowline_core::commands::{
    CONTRACT_VERSION, CommandName, DoctorCheckId, DoctorCheckStatus, DoctorCommandOutput,
    DoctorEngine, DoctorReason, DoctorSummary,
};
use bowline_core::hosted::DEFAULT_CONVEX_URL;
use bowline_core::ids::{ContentId, WorkspaceId};
use bowline_local::sync::manifest_engine::manifest::{
    BlobKey, EntryKind, FileMode, KeyEpoch, ManifestKey, WorkspacePath,
};
use bowline_local::sync::manifest_engine::store::{
    AncestorCommit, FileRecord, ManifestStore, StatFingerprint,
};

use super::DoctorContext;
use super::checks::{KeyProbe, RefProbe};

// A distinctive token embedded in seeded workspace paths. The redaction test
// proves it never survives into the doctor output even though checks read it.
const DISTINCTIVE: &str = "DOCTORXSECRETXPATH";

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

fn temp_state_root(tag: &str) -> PathBuf {
    let unique = format!("bowline-doctor-{tag}-{}-{}", std::process::id(), nanos());
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("create temp state root");
    dir
}

fn file_record(seed: u64) -> FileRecord {
    FileRecord {
        kind: EntryKind::File,
        size: seed * 100,
        mode: FileMode::new(0o644),
        symlink_target: None,
        content_id: Some(ContentId::new(format!("cid_{seed}"))),
        blob_key: Some(BlobKey::new(format!("b_{seed}"))),
        key_epoch: Some(KeyEpoch::new(1)),
        fingerprint: StatFingerprint {
            mtime_ns: seed as i64,
            ctime_ns: seed as i64 + 1,
            inode: seed,
            dev: 1,
        },
        hashed_at: Some(1000 + seed as i64),
        verified_at: None,
    }
}

/// Seeds a read-only engine store at a fresh temp state root, its two ancestor
/// rows carrying [`DISTINCTIVE`] path names. Returns the root (caller removes it)
/// and a context wired with offline probes so only local reads run.
fn seeded_context(tag: &str) -> (PathBuf, DoctorContext) {
    let root = temp_state_root(tag);
    let db = root.join(super::ENGINE_DB_FILE);
    {
        let mut store = ManifestStore::open(&db).expect("open store");
        let mut upserts = BTreeMap::new();
        upserts.insert(
            WorkspacePath::new(format!("{DISTINCTIVE}/alpha.txt")),
            file_record(1),
        );
        upserts.insert(
            WorkspacePath::new(format!("nested/{DISTINCTIVE}_beta.bin")),
            file_record(2),
        );
        let commit = AncestorCommit {
            upserts,
            removals: BTreeSet::new(),
        };
        store
            .commit_push_success(&commit, &ManifestKey::new("m_head"), 3)
            .expect("seed push");
    }
    let store = ManifestStore::open_read_only(&db).expect("reopen read-only");
    let ctx = DoctorContext {
        workspace_id: WorkspaceId::new("ws_doctor_test"),
        state_root: Some(root.clone()),
        store: Some(store),
        engine_db_present: true,
        ref_probe: RefProbe::Unreachable,
        daemon: None,
        convex_url: Some(DEFAULT_CONVEX_URL.to_string()),
        key_probe: KeyProbe::Missing,
    };
    (root, ctx)
}

fn output_of(ctx: &DoctorContext) -> DoctorCommandOutput {
    let checks = ctx.run_all_checks();
    DoctorCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Doctor,
        generated_at: "2026-07-21T00:00:00Z".to_string(),
        engine: DoctorEngine::Manifest,
        workspace_id: ctx.workspace_id.clone(),
        summary: DoctorSummary::tally(&checks),
        checks,
    }
}

fn check(output: &DoctorCommandOutput, id: DoctorCheckId) -> &bowline_core::commands::DoctorCheck {
    output
        .checks
        .iter()
        .find(|check| check.id == id)
        .expect("check present")
}

#[test]
fn doctor_output_never_contains_workspace_paths() {
    let (root, ctx) = seeded_context("redaction");
    let output = output_of(&ctx);
    let json = serde_json::to_string_pretty(&output).expect("serialize doctor output");
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        !json.contains(DISTINCTIVE),
        "a seeded workspace path leaked into doctor output",
    );
    assert!(
        !json.contains(&root.display().to_string()),
        "the state-root path leaked into doctor output",
    );
    assert!(
        !json.contains("manifest_engine.sqlite3"),
        "the engine database filename leaked into doctor output",
    );
}

#[test]
fn every_check_runs_in_declared_order() {
    let (root, ctx) = seeded_context("order");
    let output = output_of(&ctx);
    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(output.checks.len(), DoctorCheckId::ALL.len());
    for (produced, expected) in output.checks.iter().zip(DoctorCheckId::ALL) {
        assert_eq!(produced.id, expected);
    }
}

#[test]
fn local_checks_classify_a_healthy_seeded_workspace() {
    let (root, ctx) = seeded_context("healthy");
    let output = output_of(&ctx);
    let _ = std::fs::remove_dir_all(&root);

    let integrity = check(&output, DoctorCheckId::EngineSqliteIntegrity);
    assert_eq!(integrity.status, DoctorCheckStatus::Ok);
    assert_eq!(integrity.reason, DoctorReason::IntegrityVerified);

    let collisions = check(&output, DoctorCheckId::PortablePathCollisions);
    assert_eq!(collisions.status, DoctorCheckStatus::Ok);
    assert_eq!(collisions.reason, DoctorReason::NoCollisions);

    let intents = check(&output, DoctorCheckId::IntentRecoverability);
    assert_eq!(intents.reason, DoctorReason::IntentsRecoverable);
    assert_eq!(intents.count, Some(0));

    let rename = check(&output, DoctorCheckId::AtomicRenameCapability);
    assert_eq!(rename.status, DoctorCheckStatus::Ok);
    assert_eq!(rename.reason, DoctorReason::RenameSupported);

    // The distinct-cased seed has no fold collisions, and the installed-hash
    // check always surfaces an opaque digest (the test binary), never a path.
    let hash = check(&output, DoctorCheckId::InstalledCandidateHash);
    assert!(hash.opaque.as_ref().is_some_and(|hex| hex.len() == 64));
}

#[test]
fn case_fold_collision_is_detected_and_reported_as_a_count() {
    let root = temp_state_root("collision");
    let db = root.join(super::ENGINE_DB_FILE);
    {
        let mut store = ManifestStore::open(&db).expect("open store");
        let mut upserts = BTreeMap::new();
        upserts.insert(WorkspacePath::new("Case/Report.md"), file_record(1));
        upserts.insert(WorkspacePath::new("case/report.md"), file_record(2));
        let commit = AncestorCommit {
            upserts,
            removals: BTreeSet::new(),
        };
        store
            .commit_push_success(&commit, &ManifestKey::new("m_head"), 1)
            .expect("seed push");
    }
    let store = ManifestStore::open_read_only(&db).expect("reopen read-only");
    let ctx = DoctorContext {
        workspace_id: WorkspaceId::new("ws_doctor_test"),
        state_root: Some(root.clone()),
        store: Some(store),
        engine_db_present: true,
        ref_probe: RefProbe::Unreachable,
        daemon: None,
        convex_url: Some(DEFAULT_CONVEX_URL.to_string()),
        key_probe: KeyProbe::Missing,
    };
    let output = output_of(&ctx);
    let _ = std::fs::remove_dir_all(&root);

    let collisions = check(&output, DoctorCheckId::PortablePathCollisions);
    assert_eq!(collisions.status, DoctorCheckStatus::Failed);
    assert_eq!(collisions.reason, DoctorReason::PortablePathCollision);
    assert_eq!(collisions.count, Some(1));
    assert!(output.summary.attention_required);
}

/// The golden JSON contract for `bowline doctor`. A fully offline context yields
/// a deterministic report (every check `Unavailable` except the installed-hash
/// probe), so the fixture pins the exact wire shape: field names, enum tokens,
/// summary tally, and check order. Only the two inherently volatile values —
/// `generatedAt` and the self-binary hash digest — are normalized before compare.
#[test]
fn doctor_output_matches_golden_contract() {
    let ctx = DoctorContext {
        workspace_id: WorkspaceId::new("ws_doctor_golden"),
        state_root: None,
        store: None,
        engine_db_present: false,
        ref_probe: RefProbe::Unreachable,
        daemon: None,
        convex_url: None,
        key_probe: KeyProbe::Unavailable,
    };
    let output = output_of(&ctx);
    let mut value = serde_json::to_value(&output).expect("serialize doctor output");
    value["generatedAt"] = serde_json::Value::String("1970-01-01T00:00:00Z".to_string());
    for check in value["checks"].as_array_mut().expect("checks array") {
        if check["id"] == serde_json::Value::String("installed-candidate-hash".to_string()) {
            check["opaque"] = serde_json::Value::String("0".repeat(64));
        }
    }

    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/contracts/commands/doctor.json"
    ))
    .expect("golden fixture parses");
    assert_eq!(value, golden);

    // The fixture also round-trips into the typed output, proving it is a valid
    // wire instance and not just shape-compatible JSON.
    let decoded: DoctorCommandOutput =
        serde_json::from_value(golden).expect("golden decodes into DoctorCommandOutput");
    assert_eq!(decoded.checks.len(), DoctorCheckId::ALL.len());
    assert_eq!(decoded.contract_version, CONTRACT_VERSION);
}

#[test]
fn missing_engine_database_is_unavailable_not_failed() {
    let root = temp_state_root("missing-db");
    let ctx = DoctorContext {
        workspace_id: WorkspaceId::new("ws_doctor_test"),
        state_root: Some(root.clone()),
        store: None,
        engine_db_present: false,
        ref_probe: RefProbe::Unreachable,
        daemon: None,
        convex_url: Some(DEFAULT_CONVEX_URL.to_string()),
        key_probe: KeyProbe::Missing,
    };
    let output = output_of(&ctx);
    let _ = std::fs::remove_dir_all(&root);

    let integrity = check(&output, DoctorCheckId::EngineSqliteIntegrity);
    assert_eq!(integrity.status, DoctorCheckStatus::Unavailable);
    assert_eq!(integrity.reason, DoctorReason::EngineDatabaseMissing);
}
