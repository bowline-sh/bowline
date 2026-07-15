#![cfg(feature = "fault-injection")]

use bowline_core::retry::BOUNDED_SYNC_RETRY_POLICY;
use bowline_local::{
    metadata::{DEFAULT_DATABASE_FILE, EnvRecord, MaterializationTaskState, MetadataStore},
    sync::{
        SyncRunnerError, UploadError,
        fault::{self, FaultPlan, FaultPoint},
        stat_cache::StatCacheRow,
    },
};
use bowline_testkit::{SyncScenario, TwoDeviceSyncScenario};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

#[test]
fn object_upload_fault_recovers_after_reopening_runner_state() {
    let _lock = fault_test_lock();
    let scenario = SyncScenario::new("fault-object-upload").expect("scenario");
    scenario
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    let guard = fault::arm(FaultPlan::new(FaultPoint::AfterObjectUpload, 1));

    let error = scenario.tick().expect_err("fault trips");
    drop(guard);

    assert_upload_fault(error, FaultPoint::AfterObjectUpload);
    let _outcome = scenario.tick().expect("recovery tick");
    scenario.assert_invariants().expect("invariants");
}

#[test]
fn manifest_object_upload_fault_recovers_after_reopening_runner_state() {
    let _lock = fault_test_lock();
    let scenario = SyncScenario::new("fault-manifest-object-upload").expect("scenario");
    scenario
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    let guard = fault::arm(FaultPlan::new(FaultPoint::AfterObjectUpload, 2));

    let error = scenario.tick().expect_err("fault trips");
    drop(guard);

    assert_upload_fault(error, FaultPoint::AfterObjectUpload);
    let _outcome = scenario.tick().expect("recovery tick");
    scenario.assert_invariants().expect("invariants");
}

#[test]
fn manifest_commit_fault_recovers_after_reopening_runner_state() {
    let _lock = fault_test_lock();
    let scenario = SyncScenario::new("fault-manifest-commit").expect("scenario");
    scenario
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    let guard = fault::arm(FaultPlan::new(FaultPoint::AfterManifestCommit, 1));

    let error = scenario.tick().expect_err("fault trips");
    drop(guard);

    assert_upload_fault(error, FaultPoint::AfterManifestCommit);
    let _outcome = scenario.tick().expect("recovery tick");
    scenario.assert_invariants().expect("invariants");
}

#[test]
fn ref_cas_fault_recovers_after_reopening_runner_state() {
    let _lock = fault_test_lock();
    let scenario = SyncScenario::new("fault-ref-cas").expect("scenario");
    scenario
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    let guard = fault::arm(FaultPlan::new(FaultPoint::AfterRefCas, 1));

    let error = scenario.tick().expect_err("fault trips");
    drop(guard);

    assert_upload_fault(error, FaultPoint::AfterRefCas);
    let _outcome = scenario.tick().expect("recovery tick");
    scenario.assert_invariants().expect("invariants");
}

#[test]
fn materialization_rename_fault_recovers_after_reopening_runner_state() {
    let _lock = fault_test_lock();
    let scenario = TwoDeviceSyncScenario::new("fault-materialization-rename").expect("scenario");
    scenario
        .first()
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    let _outcome = scenario.first().tick().expect("first device upload");
    let guard = fault::arm(FaultPlan::new(FaultPoint::AfterMaterializationRename, 1));

    let error = scenario.second().tick().expect_err("fault trips");
    drop(guard);

    assert_runner_fault(error, FaultPoint::AfterMaterializationRename);
    let failed_store =
        MetadataStore::open(scenario.second().state_root().join(DEFAULT_DATABASE_FILE))
            .expect("failed materialization metadata");
    let failed_tasks = failed_store
        .materialization_tasks(scenario.second().workspace_id())
        .expect("failed materialization tasks");
    assert_eq!(failed_tasks.len(), 1);
    assert_eq!(
        failed_tasks[0].state,
        MaterializationTaskState::WaitingRetry
    );
    assert_eq!(failed_tasks[0].attempt_count, 1);
    let retry_delay =
        BOUNDED_SYNC_RETRY_POLICY.delay(failed_tasks[0].id.as_str(), failed_tasks[0].attempt_count);
    thread::sleep(retry_delay + Duration::from_millis(100));
    let _outcome = scenario.second().tick().expect("recovery tick");
    scenario.second().assert_invariants().expect("invariants");
}

#[test]
fn local_head_fault_rolls_back_metadata_commit_and_recovers_after_reopen() {
    let _lock = fault_test_lock();
    let scenario = SyncScenario::new("fault-local-head").expect("scenario");
    scenario
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    let before_fault = metadata_snapshot(&scenario);
    let guard = fault::arm(FaultPlan::new(FaultPoint::AfterLocalHeadWrite, 1));

    let error = scenario.tick().expect_err("fault trips");
    drop(guard);

    assert_runner_fault(error, FaultPoint::AfterLocalHeadWrite);
    assert_eq!(metadata_snapshot(&scenario), before_fault);
    scenario
        .assert_invariants()
        .expect("rolled-back invariants");
    let _outcome = scenario.tick().expect("recovery tick");
    assert_ne!(
        metadata_snapshot(&scenario).local_head_snapshot_id,
        before_fault.local_head_snapshot_id
    );
    scenario.assert_invariants().expect("recovered invariants");
}

#[test]
fn stat_cache_write_back_fault_rolls_back_metadata_commit_and_recovers_after_reopen() {
    let _lock = fault_test_lock();
    let scenario = SyncScenario::new("fault-stat-cache").expect("scenario");
    scenario
        .workspace()
        .write_file("README.md", b"hello\n")
        .expect("fixture file");
    scenario
        .workspace()
        .write_file(".env", b"API_URL=https://old.example\n")
        .expect("fixture env");
    let _outcome = scenario.tick().expect("prime stat cache");
    let before_fault = metadata_snapshot(&scenario);
    assert!(
        !before_fault.env_records.is_empty(),
        "prime tick should import env metadata before the fault case"
    );
    scenario
        .workspace()
        .write_file("README.md", b"hello again\n")
        .expect("fixture update");
    scenario
        .workspace()
        .write_file(".env", b"API_URL=https://new.example\nNEW_SECRET=value\n")
        .expect("fixture env update");
    let guard = fault::arm(FaultPlan::new(FaultPoint::AfterStatCacheWriteBack, 1));

    let error = scenario.tick().expect_err("fault trips");
    drop(guard);

    assert_runner_fault(error, FaultPoint::AfterStatCacheWriteBack);
    assert_eq!(metadata_snapshot(&scenario), before_fault);
    scenario
        .assert_invariants()
        .expect("rolled-back invariants");
    let _outcome = scenario.tick().expect("recovery tick");
    let after_recovery = metadata_snapshot(&scenario);
    assert_ne!(
        after_recovery.local_head_snapshot_id,
        before_fault.local_head_snapshot_id
    );
    assert_ne!(after_recovery.stat_cache_rows, before_fault.stat_cache_rows);
    assert_ne!(after_recovery.env_records, before_fault.env_records);
    scenario.assert_invariants().expect("recovered invariants");
}

#[derive(Debug, PartialEq, Eq)]
struct MetadataSnapshot {
    local_head_snapshot_id: Option<String>,
    stat_cache_rows: BTreeMap<String, StatCacheRow>,
    env_records: Vec<EnvRecord>,
}

fn metadata_snapshot(scenario: &SyncScenario) -> MetadataSnapshot {
    let store = MetadataStore::open(scenario.state_root().join(DEFAULT_DATABASE_FILE))
        .expect("metadata store");
    MetadataSnapshot {
        local_head_snapshot_id: store
            .workspace_sync_head(scenario.workspace_id())
            .expect("local head")
            .map(|record| record.workspace_ref.snapshot_id.to_string()),
        stat_cache_rows: store
            .stat_cache_rows(scenario.workspace_id())
            .expect("stat cache rows"),
        env_records: store
            .env_records(scenario.workspace_id())
            .expect("env records"),
    }
}

fn fault_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn assert_upload_fault(error: bowline_testkit::ScenarioError, point: FaultPoint) {
    let matches_expected = match &error {
        bowline_testkit::ScenarioError::Sync(SyncRunnerError::Upload(UploadError::Fault(
            error,
        ))) => {
            assert_eq!(error.point(), point);
            true
        }
        _ => false,
    };
    assert!(matches_expected, "unexpected error: {error}");
}

fn assert_runner_fault(error: bowline_testkit::ScenarioError, point: FaultPoint) {
    let matches_expected = match &error {
        bowline_testkit::ScenarioError::Sync(SyncRunnerError::Fault(error)) => {
            assert_eq!(error.point(), point);
            true
        }
        _ => false,
    };
    assert!(matches_expected, "unexpected error: {error}");
}
