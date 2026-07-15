//! Characterization oracle for `SyncRunner::tick` (Plan 06 U1, KTD-5).
//!
//! These tests lock the *observable* behavior of the current five-branch
//! `tick` cascade — the returned `SyncTickOutcome` variant AND the recorded
//! operation checkpoints — before the planner/executor refactor (U2–U4). They
//! must be green against the unmodified `tick` and stay green afterwards. No
//! branch is mocked: each `SyncAction` is reached through real control-plane +
//! byte-store state, matching the B1..B5 mapping in KTD-2
//! (B1→`NoChanges`, B2→`Import`, B3→`Materialize`, B4→`StaleMerge`,
//! B5→`Upload`).
//!
//! The tests also assert the product invariants that stand above the current
//! branch shape (KTD-5): Import/Materialize never overwrite non-empty local
//! work, Upload/StaleMerge never discard local edits, a remote advance never
//! mutates the canonical live folder except through import/materialization, and
//! an `Upload`→`Stale` race resolves through merge/conflict rather than a silent
//! retry or drop.

use super::*;
use crate::metadata::MaterializationTaskState;
use crate::sync::stat_cache::{
    VERIFY_SHARD_COUNT, verify_shard_for_path, verify_shard_for_timestamp,
};
use bowline_control_plane::FakeControlPlaneClient;
use bowline_core::retry::OFFLINE_SYNC_RETRY_POLICY;
use bowline_storage::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, LocalByteStore, ObjectKey, ObjectKind,
    ObjectMetadata, TransferOperation, open,
};

// A fixed clock stamp is fine across a device's ticks: snapshot IDs are
// content-derived, so an unchanged workspace re-scans to the same snapshot ID
// (the B1 `NoChanges` invariant) regardless of this value.
const GENERATED_AT: &str = "2026-07-06T12:00:00Z";

// Paths whose substrings must never appear in any external checkpoint payload
// (Security/privacy contract redaction canary).
const SECRET_ENV: &str = ".env";
const SECRET_KEY: &str = "secrets/prod.key";
const SECRET_CLIENT: &str = "client/acme-payroll/keys.json";

struct FailOncePackDownload<'a> {
    inner: &'a LocalByteStore,
    fail_next_pack: Cell<bool>,
}

impl<'a> FailOncePackDownload<'a> {
    fn new(inner: &'a LocalByteStore) -> Self {
        Self {
            inner,
            fail_next_pack: Cell::new(true),
        }
    }

    fn inject_failure(&self, key: &ObjectKey) -> Result<(), ByteStoreError> {
        if key.as_str().starts_with("packs_") && self.fail_next_pack.replace(false) {
            return Err(ByteStoreError::Network {
                operation: TransferOperation::Download,
                detail: "injected transient download failure".to_string(),
            });
        }
        Ok(())
    }
}

impl ByteStore for FailOncePackDownload<'_> {
    fn put_object(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.inner
            .put_object(key, kind, bytes, created_by_device_id)
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        self.inject_failure(key)?;
        self.inner.get_object(key)
    }

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        self.inject_failure(key)?;
        self.inner.get_range(key, range)
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        self.inner.head_object(key)
    }

    fn metrics(&self) -> ByteStoreMetrics {
        self.inner.metrics()
    }
}

struct Device {
    workspace: TempWorkspace,
    state: TempWorkspace,
    objects_root: PathBuf,
    byte_store: bowline_storage::LocalByteStore,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
}

impl Device {
    fn new(
        label: &str,
        device_id: &str,
        objects_dir: &Path,
        clock_seed: u64,
        workspace_id: WorkspaceId,
    ) -> Self {
        let workspace = TempWorkspace::new(&format!("{label}-{device_id}-workspace")).expect("ws");
        let state = TempWorkspace::new(&format!("{label}-{device_id}-state")).expect("state");
        let byte_store =
            bowline_storage::LocalByteStore::open_deterministic(objects_dir, clock_seed)
                .expect("byte store");
        Self {
            workspace,
            state,
            objects_root: objects_dir.to_path_buf(),
            byte_store,
            workspace_id,
            device_id: DeviceId::new(device_id),
        }
    }

    fn options(&self, sync_claim: Option<SyncClaimHandle>) -> SyncRunnerOptions {
        self.options_at(GENERATED_AT, sync_claim)
    }

    fn options_at(
        &self,
        generated_at: &str,
        sync_claim: Option<SyncClaimHandle>,
    ) -> SyncRunnerOptions {
        SyncRunnerOptions {
            root: self.workspace.root().to_path_buf(),
            state_root: self.state.root().to_path_buf(),
            workspace_id: self.workspace_id.clone(),
            device_id: self.device_id.clone(),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: generated_at.to_string(),
            sync_claim,
            scan_scope: Default::default(),
        }
    }

    fn tick(&self, control_plane: &FakeControlPlaneClient) -> SyncTickOutcome {
        SyncRunner::new(control_plane, &self.byte_store, self.options(None))
            .tick()
            .expect("tick")
    }

    /// Tick with a live sync claim so checkpoints are persisted, then
    /// return both the tick result and the recorded checkpoints. Mirrors the
    /// `SyncRunnerOptions.sync_claim` gating (`record_sync_checkpoint` no-ops
    /// when the claim is `None`).
    fn tick_with_sync_operation(
        &self,
        control_plane: &FakeControlPlaneClient,
        operation_id: &str,
    ) -> (
        Result<SyncTickOutcome, SyncRunnerError>,
        Vec<SyncOperationCheckpointRecord>,
    ) {
        let sync_claim = self.enqueue_operation(operation_id);
        let result = SyncRunner::new(
            control_plane,
            &self.byte_store,
            self.options(Some(sync_claim.clone())),
        )
        .tick();
        self.complete_test_claim(&sync_claim);
        (result, self.checkpoints(operation_id))
    }

    fn tick_with_sync_operation_at(
        &self,
        control_plane: &FakeControlPlaneClient,
        operation_id: &str,
        generated_at: &str,
    ) -> (
        Result<SyncTickOutcome, SyncRunnerError>,
        Vec<SyncOperationCheckpointRecord>,
    ) {
        let sync_claim = self.enqueue_operation(operation_id);
        let result = SyncRunner::new(
            control_plane,
            &self.byte_store,
            self.options_at(generated_at, Some(sync_claim.clone())),
        )
        .tick();
        self.complete_test_claim(&sync_claim);
        (result, self.checkpoints(operation_id))
    }

    fn corrupt_first_pack_object(&self) {
        let pack_key = self
            .byte_store
            .list_object_keys()
            .expect("object keys")
            .into_iter()
            .find(|key| key.as_str().starts_with("packs_"))
            .expect("source pack object");
        let object_path = self.objects_root.join("objects").join(pack_key.as_str());
        let byte_len = fs::metadata(&object_path).expect("pack metadata").len();
        let corrupt_len = usize::try_from(byte_len).expect("pack length fits usize");
        fs::write(object_path, vec![b'x'; corrupt_len]).expect("corrupt pack object");
    }

    fn enqueue_operation(&self, operation_id: &str) -> SyncClaimHandle {
        let store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: operation_id.to_string(),
                workspace_id: self.workspace_id.clone(),
                kind: SyncOperationKind::Reconcile,
                resource_key: crate::metadata::SyncResourceKey::workspace_sync(
                    self.workspace_id.clone(),
                ),
                state: SyncOperationState::Queued,
                idempotency_key: operation_id.to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(self.device_id.clone()),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                claim_generation: 0,
                heartbeat_at: None,
                lease_expires_at: None,
                cancellation_requested_at: None,
                next_attempt_at: None,
                result_json: None,
                last_error_code: None,
                last_error: None,
                created_at: GENERATED_AT.to_string(),
                updated_at: GENERATED_AT.to_string(),
            })
            .expect("operation");
        for _ in 0..16 {
            let claim = store
                .claim_next_sync_operation(
                    &self.workspace_id,
                    "plan-characterization",
                    GENERATED_AT,
                    "2999-01-01T00:00:00Z",
                )
                .expect("claim operation")
                .expect("queued operation")
                .claim;
            if claim.operation_id() == operation_id {
                return claim;
            }
            // Earlier ticks can leave durable post-commit work ahead of the
            // characterization operation. Settle it so this helper returns the
            // exact claim whose checkpoints the test reads.
            store
                .complete_claimed_sync_operation(&claim, "{}", GENERATED_AT)
                .expect("complete preceding durable operation");
        }
        panic!("characterization operation was not claimable after draining preceding work")
    }

    fn complete_test_claim(&self, claim: &SyncClaimHandle) {
        let store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .complete_claimed_sync_operation(claim, "{}", "2026-07-05T12:02:00Z")
            .expect("complete characterization claim");
    }

    fn checkpoints(&self, operation_id: &str) -> Vec<SyncOperationCheckpointRecord> {
        let store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .sync_operation_checkpoints(operation_id)
            .expect("checkpoints")
    }

    fn file(&self, relative_path: &str) -> Option<Vec<u8>> {
        fs::read(self.workspace.root().join(relative_path)).ok()
    }
}

fn new_control_plane(workspace_id: &WorkspaceId) -> FakeControlPlaneClient {
    let control_plane = FakeControlPlaneClient::default();
    control_plane
        .create_workspace_ref(workspace_id)
        .expect("workspace ref");
    control_plane
}

fn checkpoint_steps(checkpoints: &[SyncOperationCheckpointRecord]) -> Vec<&str> {
    checkpoints
        .iter()
        .map(|checkpoint| checkpoint.step.as_str())
        .collect()
}

fn has_step(checkpoints: &[SyncOperationCheckpointRecord], step: &str) -> bool {
    checkpoints.iter().any(|checkpoint| checkpoint.step == step)
}

fn verify_timestamp_for_shard(shard: u64) -> String {
    let seconds = i64::try_from(shard).expect("verify shard fits i64") * 600;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .expect("valid timestamp")
        .format(&time::format_description::well_known::Rfc3339)
        .expect("timestamp formats")
}

/// External checkpoint payloads are aggregate-only: they must never carry
/// workspace paths, secret file names, or raw error text.
fn assert_checkpoints_are_redacted(
    checkpoints: &[SyncOperationCheckpointRecord],
    forbidden: &[&str],
) {
    for checkpoint in checkpoints {
        for needle in forbidden {
            assert!(
                !checkpoint.payload_json.contains(needle),
                "checkpoint `{}` leaked forbidden substring `{needle}`: {}",
                checkpoint.step,
                checkpoint.payload_json
            );
        }
    }
}

// --- Canary: checkpoints only exist when they are actually recorded ---------

#[test]
fn trivial_tick_records_remote_ref_and_snapshot_candidate_checkpoints() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-canary-objects").expect("objects");
    let device = Device::new(
        "plan-char-canary",
        "device_local",
        objects.root(),
        11,
        workspace_id,
    );
    device
        .workspace
        .write_file("README.md", b"hello\n")
        .expect("readme");

    let (outcome, checkpoints) = device.tick_with_sync_operation(&control_plane, "op_canary");

    assert!(matches!(
        outcome.expect("tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    // Missing baseline checkpoints fail the oracle rather than weakening it to
    // outcome-only assertions.
    assert!(
        has_step(&checkpoints, "remote-ref-observed"),
        "steps: {:?}",
        checkpoint_steps(&checkpoints)
    );
    assert!(
        has_step(&checkpoints, "snapshot-candidate-built"),
        "steps: {:?}",
        checkpoint_steps(&checkpoints)
    );
}

#[test]
fn observe_returns_none_when_workspace_ref_absent() {
    // No remote ref exists yet: `get_workspace_ref` yields `None`, so `observe`
    // short-circuits to `None` before any checkpoint and `tick` reports
    // `NoWorkspaceRef`. Guards the U2 extraction's empty-workspace early-out.
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = FakeControlPlaneClient::default();
    let objects = TempWorkspace::new("plan-char-no-ref-objects").expect("objects");
    let device = Device::new(
        "plan-char-no-ref",
        "device_local",
        objects.root(),
        11,
        workspace_id,
    );

    let runner = SyncRunner::new(&control_plane, &device.byte_store, device.options(None));
    assert!(runner.observe().expect("observe").is_none());
    assert_eq!(device.tick(&control_plane), SyncTickOutcome::NoWorkspaceRef);
}

// --- B1: NoChanges ----------------------------------------------------------

#[test]
fn steady_state_retick_returns_no_changes_without_upload() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-nochanges-objects").expect("objects");
    let device = Device::new(
        "plan-char-nochanges",
        "device_local",
        objects.root(),
        11,
        workspace_id,
    );
    device
        .workspace
        .write_file("README.md", b"hello\n")
        .expect("readme");
    assert!(matches!(
        device.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));

    // A steady-state re-tick must not mutate the canonical live folder.
    let detector = device.workspace.mutation_detector().expect("detector");
    let (outcome, checkpoints) = device.tick_with_sync_operation(&control_plane, "op_nochanges");

    assert_eq!(outcome.expect("tick"), SyncTickOutcome::NoChanges);
    detector.assert_unchanged().expect("folder unchanged");
    assert!(has_step(&checkpoints, "remote-ref-observed"));
    assert!(has_step(&checkpoints, "snapshot-candidate-built"));
    assert!(
        !checkpoints
            .iter()
            .any(|checkpoint| checkpoint.step.starts_with("workspace-ref-")),
        "steady-state re-tick must not attempt an upload: {:?}",
        checkpoint_steps(&checkpoints)
    );
}

// --- B5: Upload -------------------------------------------------------------

#[test]
fn reused_pack_verification_samples_upload_hits_by_verify_shard() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-reuse-shard-objects").expect("objects");
    let device = Device::new(
        "plan-char-reuse-shard",
        "device_local",
        objects.root(),
        11,
        workspace_id,
    );
    let reused_path = "src/cache-hit.txt";
    device
        .workspace
        .write_file(reused_path, b"unchanged content\n")
        .expect("seed reused path");
    assert!(matches!(
        device.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));

    device.corrupt_first_pack_object();

    device
        .workspace
        .write_file("src/out-of-shard-trigger.txt", b"force upload one\n")
        .expect("trigger one");
    let path_shard = verify_shard_for_path(reused_path);
    let out_of_shard = (path_shard + 1) % VERIFY_SHARD_COUNT;
    let out_of_shard_timestamp = verify_timestamp_for_shard(out_of_shard);
    assert_ne!(
        verify_shard_for_timestamp(&out_of_shard_timestamp),
        path_shard
    );
    let (outcome, checkpoints) = device.tick_with_sync_operation_at(
        &control_plane,
        "op_reuse_out_of_shard",
        &out_of_shard_timestamp,
    );

    assert!(matches!(
        outcome.expect("out-of-shard tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    assert!(
        has_step(&checkpoints, "source-pack-reused"),
        "out-of-shard skipped bytes should allow reuse: {:?}",
        checkpoint_steps(&checkpoints)
    );
    assert!(
        !has_step(&checkpoints, "source-pack-reuse-repacked"),
        "out-of-shard tick must not verify and repack: {:?}",
        checkpoint_steps(&checkpoints)
    );

    device
        .workspace
        .write_file("src/in-shard-trigger.txt", b"force upload two\n")
        .expect("trigger two");
    let in_shard_timestamp = verify_timestamp_for_shard(path_shard);
    assert_eq!(
        verify_shard_for_timestamp(&in_shard_timestamp),
        verify_shard_for_path(reused_path)
    );
    let (outcome, checkpoints) = device.tick_with_sync_operation_at(
        &control_plane,
        "op_reuse_in_shard",
        &in_shard_timestamp,
    );

    assert!(matches!(
        outcome.expect("in-shard tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    assert!(
        has_step(&checkpoints, "source-pack-reuse-repacked"),
        "in-shard verification should detect corruption and repack: {:?}",
        checkpoint_steps(&checkpoints)
    );
}

#[test]
fn local_edit_on_fresh_workspace_returns_uploaded() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-upload-objects").expect("objects");
    let device = Device::new(
        "plan-char-upload",
        "device_local",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    device
        .workspace
        .write_file("README.md", b"hello\n")
        .expect("readme");

    let (outcome, checkpoints) = device.tick_with_sync_operation(&control_plane, "op_upload");

    assert!(matches!(
        outcome.expect("tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    // The small root and its complete metadata graph committed before ref advance.
    assert!(has_step(&checkpoints, "snapshot-root-committed"));
    assert!(has_step(&checkpoints, "workspace-ref-advanced"));
    // Upload never discards the local edit.
    assert_eq!(
        device.file("README.md").as_deref(),
        Some(b"hello\n".as_ref())
    );
    let advanced = control_plane
        .get_workspace_ref(&workspace_id)
        .expect("ref")
        .expect("ref present");
    assert_eq!(advanced.version, 1);
}

#[cfg(feature = "fault-injection")]
#[test]
fn cancelled_claim_crashing_after_ref_cas_reconciles_local_head_before_terminal_completion() {
    let workspace_id = WorkspaceId::new("ws_crash_after_ref_cas");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-crash-after-cas-objects").expect("objects");
    let device = Device::new(
        "plan-char-crash-after-cas",
        "device_local",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    device
        .workspace
        .write_file("README.md", b"committed before crash\n")
        .expect("readme");
    let original_claim = device.enqueue_operation("op_crash_after_ref_cas");
    let first_runner = SyncRunner::new(
        &control_plane,
        &device.byte_store,
        device.options(Some(original_claim.clone())),
    );
    let _fault = crate::sync::fault::arm(crate::sync::fault::FaultPlan::new(
        crate::sync::fault::FaultPoint::AfterRefCas,
        1,
    ));

    first_runner
        .tick()
        .expect_err("fault stops before local-head persistence");

    let committed_ref = control_plane
        .get_workspace_ref(&workspace_id)
        .expect("remote ref query")
        .expect("remote ref advanced");
    assert_eq!(committed_ref.version, 1);
    let store = MetadataStore::open(device.state.root().join(DEFAULT_DATABASE_FILE))
        .expect("metadata after crash");
    assert!(
        store
            .sync_operation_checkpoints(original_claim.operation_id())
            .expect("durable checkpoints")
            .iter()
            .any(|checkpoint| checkpoint.step == "workspace-ref-advanced"),
        "the externally visible CAS must be durable before the crash seam"
    );
    assert!(
        store
            .workspace_sync_head(&workspace_id)
            .expect("local head query")
            .is_none(),
        "fault must occur before the local head records the committed ref"
    );
    assert_eq!(
        store
            .request_sync_operation_cancellation(
                original_claim.operation_id(),
                "2026-07-06T12:00:01Z",
            )
            .expect("cancellation requested"),
        Some(crate::metadata::SyncCancellationOutcome::Requested)
    );
    store
        .connection()
        .execute(
            "UPDATE sync_operations
             SET lease_expires_at = '2000-01-01T00:00:00Z'
             WHERE id = ?1",
            [original_claim.operation_id()],
        )
        .expect("simulate process death past the lease");
    assert_eq!(
        store
            .requeue_expired_sync_claims(&workspace_id, "2026-07-06T12:00:02Z")
            .expect("expired cancellation swept"),
        1
    );
    let replacement = store
        .claim_next_sync_operation(
            &workspace_id,
            "replacement-daemon",
            "2026-07-06T12:00:03Z",
            "2999-01-01T00:00:00Z",
        )
        .expect("replacement claim query")
        .expect("reconciliation claim");
    assert_eq!(replacement.operation.state, SyncOperationState::Claimed);
    assert_eq!(
        replacement.claim.claimed_from_state(),
        SyncOperationState::ReconciliationRequired
    );

    let recovery_runner = SyncRunner::new(
        &control_plane,
        &device.byte_store,
        device.options(Some(replacement.claim.clone())),
    );
    let recovery_outcome = recovery_runner.tick().expect("remote commit reconciles");
    assert!(
        matches!(
            recovery_outcome,
            SyncTickOutcome::NoChanges | SyncTickOutcome::Imported(_)
        ),
        "recovery must observe/import the existing ref, not execute a new upload: {recovery_outcome:?}"
    );
    assert!(recovery_runner.cancellation_requested_after_commit());
    let local_head = store
        .workspace_sync_head(&workspace_id)
        .expect("reconciled local head query")
        .expect("reconciled local head");
    assert_eq!(local_head.workspace_ref, committed_ref);
    assert_eq!(
        store
            .complete_committed_cancelled_late_sync_operation(
                &replacement.claim,
                &crate::metadata::SyncCommittedCancelledLateResult::new(
                    SyncOperationKind::Reconcile,
                    serde_json::json!({
                        "snapshotId": committed_ref.snapshot_id.as_str(),
                        "version": committed_ref.version,
                    }),
                ),
                "2026-07-06T12:00:04Z",
            )
            .expect("reconciliation completion"),
        SyncClaimTransition::Applied
    );
    let completed = store
        .sync_operation_by_id(replacement.claim.operation_id())
        .expect("completed operation query")
        .expect("completed operation");
    assert_eq!(completed.state, SyncOperationState::Completed);
    let result = completed.result_json.expect("typed completion result");
    assert!(result.contains("committed-cancelled-late"));
    assert!(!result.contains(r#"\"outcome\":\"cancelled\""#));
}

// --- B3: Materialize --------------------------------------------------------

#[test]
fn empty_local_head_materializes_non_empty_remote() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-materialize-objects").expect("objects");
    let author = Device::new(
        "plan-char-materialize",
        "device_author",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    let receiver = Device::new(
        "plan-char-materialize",
        "device_receiver",
        objects.root(),
        12,
        workspace_id,
    );
    author
        .workspace
        .write_file("shared.txt", b"remote body\n")
        .expect("shared");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));

    // Receiver's canonical folder is empty at first sight of a non-empty remote.
    assert!(receiver.file("shared.txt").is_none());
    let (outcome, checkpoints) =
        receiver.tick_with_sync_operation(&control_plane, "op_materialize");

    assert!(matches!(
        outcome.expect("tick"),
        SyncTickOutcome::Imported(_)
    ));
    // The canonical folder is populated from the remote, not overwritten by the
    // empty local candidate.
    assert_eq!(
        receiver.file("shared.txt").as_deref(),
        Some(b"remote body\n".as_ref())
    );
    assert!(has_step(&checkpoints, "remote-materialized"));
    assert!(
        !checkpoints
            .iter()
            .any(|checkpoint| checkpoint.step.starts_with("workspace-ref-")),
        "materialize must not upload: {:?}",
        checkpoint_steps(&checkpoints)
    );
}

// --- B2a: Import ------------------------------------------------------------

#[test]
fn clean_local_with_advanced_remote_imports_structure() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-import-objects").expect("objects");
    let author = Device::new(
        "plan-char-import",
        "device_author",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    let receiver = Device::new(
        "plan-char-import",
        "device_receiver",
        objects.root(),
        12,
        workspace_id.clone(),
    );

    author
        .workspace
        .write_file("shared.txt", b"one\n")
        .expect("shared");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));
    // Receiver syncs to the first remote (materialize), leaving no local edits.
    assert!(matches!(
        receiver.tick(&control_plane),
        SyncTickOutcome::Imported(_)
    ));

    // Remote advances again while the receiver stays clean (candidate == base).
    author
        .workspace
        .write_file("shared2.txt", b"two\n")
        .expect("shared2");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));

    let (outcome, checkpoints) = receiver.tick_with_sync_operation(&control_plane, "op_import");

    let imported = match outcome.expect("tick") {
        SyncTickOutcome::Imported(reference) => reference,
        other => panic!("expected Imported, got {other:?}"),
    };
    assert_eq!(imported.version, 2);
    // Import brings the advanced remote structure into the canonical folder
    // without discarding the already-synced content.
    assert_eq!(
        receiver.file("shared.txt").as_deref(),
        Some(b"one\n".as_ref())
    );
    assert_eq!(
        receiver.file("shared2.txt").as_deref(),
        Some(b"two\n".as_ref())
    );
    assert!(has_step(&checkpoints, "remote-import-started"));
    assert!(has_step(&checkpoints, "remote-import-completed"));
    assert!(
        !checkpoints
            .iter()
            .any(|checkpoint| checkpoint.step.starts_with("workspace-ref-")),
        "import must not upload: {:?}",
        checkpoint_steps(&checkpoints)
    );
}

#[test]
fn transient_materialization_download_is_reclaimed_after_backoff() {
    let workspace_id = WorkspaceId::new("ws_materialization_retry");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-materialization-retry-objects").expect("objects");
    let author = Device::new(
        "plan-char-materialization-retry",
        "device_author",
        objects.root(),
        31,
        workspace_id.clone(),
    );
    let receiver = Device::new(
        "plan-char-materialization-retry",
        "device_receiver",
        objects.root(),
        32,
        workspace_id.clone(),
    );
    author
        .workspace
        .write_file("src/retry.rs", b"pub fn retried() {}\n")
        .expect("source");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));
    let flaky_store = FailOncePackDownload::new(&receiver.byte_store);

    let first_started_at = time::OffsetDateTime::now_utc();
    let first = SyncRunner::new(
        &control_plane,
        &flaky_store,
        receiver.options_at("2026-07-06T12:00:00Z", None),
    )
    .tick();
    let first_finished_at = time::OffsetDateTime::now_utc();
    assert!(matches!(
        first,
        Err(SyncRunnerError::Cache(CacheError::Store(
            ByteStoreError::Network { .. }
        )))
    ));
    assert!(receiver.file("src/retry.rs").is_none());
    let store = MetadataStore::open(receiver.state.root().join(DEFAULT_DATABASE_FILE))
        .expect("receiver metadata");
    let failed = store
        .materialization_tasks(&workspace_id)
        .expect("materialization tasks")
        .into_iter()
        .find(|task| task.path == "src/retry.rs")
        .expect("retry task");
    assert_eq!(failed.state, MaterializationTaskState::BlockedOffline);
    assert_eq!(failed.attempt_count, 1);
    let retry_delay = i64::try_from(
        OFFLINE_SYNC_RETRY_POLICY
            .delay(failed.id.as_str(), failed.attempt_count)
            .as_secs(),
    )
    .expect("retry delay fits i64");
    let expected_not_before = failed.not_before.clone().expect("retry deadline");
    let retry_deadline = time::OffsetDateTime::parse(
        &expected_not_before,
        &time::format_description::well_known::Rfc3339,
    )
    .expect("retry deadline parses");
    let inferred_claimed_at = retry_deadline - time::Duration::seconds(retry_delay);
    assert!(inferred_claimed_at >= first_started_at);
    assert!(inferred_claimed_at <= first_finished_at);
    drop(store);

    let early = SyncRunner::new(
        &control_plane,
        &flaky_store,
        receiver.options_at("2026-07-06T12:00:01Z", None),
    )
    .tick();
    assert!(
        matches!(early, Err(SyncRunnerError::MaterializationRetryPending)),
        "early retry outcome: {early:?}"
    );
    let store = MetadataStore::open(receiver.state.root().join(DEFAULT_DATABASE_FILE))
        .expect("receiver metadata");
    let waiting = store
        .materialization_task(&failed.id)
        .expect("retry task lookup")
        .expect("retry task");
    assert_eq!(waiting.attempt_count, 1);
    drop(store);

    let wait_millis = (retry_deadline - time::OffsetDateTime::now_utc())
        .whole_milliseconds()
        .max(0);
    std::thread::sleep(std::time::Duration::from_millis(
        u64::try_from(wait_millis).expect("nonnegative retry wait") + 100,
    ));

    let recovered = SyncRunner::new(
        &control_plane,
        &flaky_store,
        receiver.options_at(&expected_not_before, None),
    )
    .tick()
    .expect("later tick retries materialization");
    assert!(matches!(recovered, SyncTickOutcome::Imported(_)));
    assert_eq!(
        receiver.file("src/retry.rs").as_deref(),
        Some(b"pub fn retried() {}\n".as_slice())
    );
    let store = MetadataStore::open(receiver.state.root().join(DEFAULT_DATABASE_FILE))
        .expect("receiver metadata");
    let completed = store
        .materialization_task(&failed.id)
        .expect("completed task lookup")
        .expect("completed task");
    assert_eq!(completed.state, MaterializationTaskState::Ready);
    assert_eq!(completed.attempt_count, 2);
    assert!(completed.not_before.is_none());
}

#[test]
fn clean_remote_deletion_runs_as_a_durable_cleanup_task() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-delete-objects").expect("objects");
    let author = Device::new(
        "plan-char-delete",
        "device_author",
        objects.root(),
        21,
        workspace_id.clone(),
    );
    let receiver = Device::new(
        "plan-char-delete",
        "device_receiver",
        objects.root(),
        22,
        workspace_id.clone(),
    );
    author
        .workspace
        .write_file("removed.txt", b"remove me\n")
        .expect("removed file");
    author
        .workspace
        .write_file("retained.txt", b"keep me\n")
        .expect("retained file");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));
    assert!(matches!(
        receiver.tick(&control_plane),
        SyncTickOutcome::Imported(_)
    ));
    fs::remove_file(author.workspace.root().join("removed.txt")).expect("delete author file");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));

    assert!(matches!(
        receiver.tick(&control_plane),
        SyncTickOutcome::Imported(_)
    ));
    assert!(receiver.file("removed.txt").is_none());
    assert_eq!(
        receiver.file("retained.txt").as_deref(),
        Some(b"keep me\n".as_ref())
    );
    let store = MetadataStore::open(receiver.state.root().join(DEFAULT_DATABASE_FILE))
        .expect("receiver metadata");
    let cleanup = store
        .materialization_tasks(&workspace_id)
        .expect("materialization tasks")
        .into_iter()
        .find(|task| task.path == "removed.txt")
        .expect("cleanup task");
    assert_eq!(cleanup.expected_kind, NamespaceEntryKind::Tombstone);
    assert_eq!(cleanup.state, MaterializationTaskState::Ready);
}

// --- B4: StaleMerge (clean) -------------------------------------------------

#[test]
fn local_edits_against_stale_base_merge_cleanly() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-merge-objects").expect("objects");
    let author = Device::new(
        "plan-char-merge",
        "device_author",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    let receiver = Device::new(
        "plan-char-merge",
        "device_receiver",
        objects.root(),
        12,
        workspace_id,
    );

    author.workspace.write_file("a.txt", b"base\n").expect("a");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));
    assert!(matches!(
        receiver.tick(&control_plane),
        SyncTickOutcome::Imported(_)
    ));

    // Receiver makes a local edit on a non-conflicting path.
    receiver
        .workspace
        .write_file("b_local.txt", b"mine\n")
        .expect("b_local");
    // Remote advances past the receiver's base with a non-conflicting add.
    author
        .workspace
        .write_file("a2.txt", b"remote add\n")
        .expect("a2");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));

    let (outcome, checkpoints) = receiver.tick_with_sync_operation(&control_plane, "op_merge");

    assert!(matches!(outcome.expect("tick"), SyncTickOutcome::Merged(_)));
    // StaleMerge never discards the local edit and materializes the remote add.
    assert_eq!(
        receiver.file("b_local.txt").as_deref(),
        Some(b"mine\n".as_ref())
    );
    assert_eq!(
        receiver.file("a2.txt").as_deref(),
        Some(b"remote add\n".as_ref())
    );
    assert!(has_step(&checkpoints, "workspace-ref-advanced"));
}

// --- B4: StaleMerge (conflicting) -------------------------------------------

#[test]
fn conflicting_local_edit_against_stale_base_conflicts() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-conflict-objects").expect("objects");
    let author = Device::new(
        "plan-char-conflict",
        "device_author",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    let receiver = Device::new(
        "plan-char-conflict",
        "device_receiver",
        objects.root(),
        12,
        workspace_id,
    );

    author
        .workspace
        .write_file("conflict.txt", b"base body\n")
        .expect("conflict");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));
    assert!(matches!(
        receiver.tick(&control_plane),
        SyncTickOutcome::Imported(_)
    ));

    // Both devices edit the same path to different content.
    receiver
        .workspace
        .write_file("conflict.txt", b"local body\n")
        .expect("local edit");
    author
        .workspace
        .write_file("conflict.txt", b"remote body\n")
        .expect("remote edit");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));

    let (outcome, _checkpoints) = receiver.tick_with_sync_operation(&control_plane, "op_conflict");

    let conflicts = match outcome.expect("tick") {
        SyncTickOutcome::Conflicted(records) => records,
        other => panic!("expected Conflicted, got {other:?}"),
    };
    assert!(!conflicts.is_empty());
    assert!(
        conflicts
            .iter()
            .all(|record| record.bundle_object.is_some())
    );
    // The local edit for the conflicted path is preserved on disk (the merge
    // materializes the remote for everything except the conflicted paths).
    assert_eq!(
        receiver.file("conflict.txt").as_deref(),
        Some(b"local body\n".as_ref())
    );
    let published = control_plane
        .list_workspace_conflicts(&receiver.workspace_id, &receiver.device_id)
        .expect("published conflicts");
    assert!(
        published.is_empty(),
        "daemon scheduler owns remote reconcile"
    );
    let store = MetadataStore::open(receiver.state.root().join(DEFAULT_DATABASE_FILE))
        .expect("metadata store");
    assert!(
        store
            .sync_operations(&receiver.workspace_id)
            .expect("sync operations")
            .iter()
            .any(|operation| operation.kind == SyncOperationKind::ConflictOccurrenceReconcile),
        "conflict occurrence must be queued durably"
    );
    let pointer = conflicts
        .iter()
        .find_map(|record| record.bundle_object.as_ref())
        .expect("local conflict carries bundle pointer");
    assert_eq!(
        pointer.kind,
        bowline_control_plane::ObjectKind::ConflictBundle
    );

    let recoverer = Device::new(
        "plan-char-conflict",
        "device_recoverer",
        objects.root(),
        13,
        receiver.workspace_id.clone(),
    );
    let object_key = ObjectKey::new(pointer.object_key.clone()).expect("bundle object key");
    let sealed = recoverer
        .byte_store
        .get_object(&object_key)
        .expect("recovering device fetches bundle object");
    let opened = open(
        &sealed,
        recoverer.options(None).storage_key,
        &crate::sync::upload::conflict_bundle_envelope_context(
            &recoverer.workspace_id,
            &pointer.content_id,
            pointer.key_epoch,
        ),
    )
    .expect("recovering device opens sealed conflict bundle");
    let payload: crate::sync::conflicts::ConflictBundlePayload =
        serde_json::from_slice(&opened).expect("payload decodes");
    assert_eq!(
        crate::sync::conflict_bundle_object_id(&payload.record),
        pointer.content_id
    );
    assert_eq!(payload.files[0].relative_path, "conflict.txt");
    assert_eq!(
        payload.files[0].local.as_deref(),
        Some(b"local body\n".as_ref())
    );
    assert_eq!(
        payload.files[0].remote.as_deref(),
        Some(b"remote body\n".as_ref())
    );
}

// --- B5 Stale: the Upload -> Stale runtime race -----------------------------

#[test]
fn upload_race_resolves_via_merge_not_silent_retry_or_drop() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-upload-race-objects").expect("objects");
    let device = Device::new(
        "plan-char-upload-race",
        "device_local",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    device
        .workspace
        .write_file("local.txt", b"mine\n")
        .expect("local");

    // At observe time the ref is still `empty` (candidate_base == base), so the
    // planner selects Upload (B5), NOT StaleMerge (B4). The race is injected at
    // CAS time: the next workspace-ref CAS returns a StaleRef carrying a valid,
    // importable current head.
    let injected_current = WorkspaceRef {
        workspace_id: WorkspaceId::new(workspace_id.as_str()),
        version: 1,
        snapshot_id: SnapshotId::new(EMPTY_SNAPSHOT_ID),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
        updated_by_device_id: Some(DeviceId::new("device_remote")),
    };
    control_plane
        .make_next_workspace_ref_cas_stale_for_harness(workspace_id.as_str(), injected_current);

    let (outcome, checkpoints) = device.tick_with_sync_operation(&control_plane, "op_upload_race");

    // The race resolves through merge (or conflict), never a silent drop/retry.
    assert!(matches!(outcome.expect("tick"), SyncTickOutcome::Merged(_)));
    // `workspace-ref-stale` is only recorded by the upload path's CAS: reaching
    // it proves the runtime Upload arm ran and hit the stale race, rather than
    // the planner picking StaleMerge (B4), which never uploads the candidate.
    assert!(
        has_step(&checkpoints, "workspace-ref-stale"),
        "expected the runtime Upload arm to record a stale CAS: {:?}",
        checkpoint_steps(&checkpoints)
    );
    // The re-uploaded merge advanced the ref, and the local edit survived.
    assert!(has_step(&checkpoints, "workspace-ref-advanced"));
    assert_eq!(
        device.file("local.txt").as_deref(),
        Some(b"mine\n".as_ref())
    );
    let advanced = control_plane
        .get_workspace_ref(&workspace_id)
        .expect("ref")
        .expect("ref present");
    assert_eq!(advanced.version, 2);
}

// --- Checkpoint privacy canary ---------------------------------------------

#[test]
fn success_tick_checkpoints_redact_secret_paths() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-privacy-objects").expect("objects");
    let device = Device::new(
        "plan-char-privacy",
        "device_local",
        objects.root(),
        11,
        workspace_id,
    );
    device
        .workspace
        .write_file(SECRET_ENV, b"SECRET=value\n")
        .expect("env");
    device
        .workspace
        .write_file(SECRET_KEY, b"PRIVATE KEY\n")
        .expect("key");
    device
        .workspace
        .write_file(SECRET_CLIENT, b"{\"token\":\"acme\"}\n")
        .expect("client");
    device
        .workspace
        .write_file("README.md", b"hello\n")
        .expect("readme");

    let (outcome, checkpoints) = device.tick_with_sync_operation(&control_plane, "op_privacy_ok");

    assert!(matches!(
        outcome.expect("tick"),
        SyncTickOutcome::Uploaded(_)
    ));
    assert!(has_step(&checkpoints, "snapshot-candidate-built"));
    let root = device.workspace.root().display().to_string();
    assert_checkpoints_are_redacted(
        &checkpoints,
        &[
            SECRET_ENV,
            SECRET_KEY,
            SECRET_CLIENT,
            "acme-payroll",
            "SECRET=value",
            root.as_str(),
        ],
    );
}

#[test]
fn error_path_checkpoints_redact_secret_paths_and_raw_errors() {
    let workspace_id = WorkspaceId::new("ws_code");
    let control_plane = new_control_plane(&workspace_id);
    let objects = TempWorkspace::new("plan-char-privacy-error-objects").expect("objects");
    let author = Device::new(
        "plan-char-privacy-error",
        "device_author",
        objects.root(),
        11,
        workspace_id.clone(),
    );
    let receiver = Device::new(
        "plan-char-privacy-error",
        "device_receiver",
        objects.root(),
        12,
        workspace_id.clone(),
    );

    author
        .workspace
        .write_file(SECRET_ENV, b"SECRET=value\n")
        .expect("env");
    author
        .workspace
        .write_file(SECRET_KEY, b"PRIVATE KEY\n")
        .expect("key");
    author
        .workspace
        .write_file(SECRET_CLIENT, b"{\"token\":\"acme\"}\n")
        .expect("client");
    assert!(matches!(
        author.tick(&control_plane),
        SyncTickOutcome::Uploaded(_)
    ));
    assert!(matches!(
        receiver.tick(&control_plane),
        SyncTickOutcome::Imported(_)
    ));

    // Advance the ref to a snapshot whose objects were never uploaded, so the
    // receiver's next tick imports a structure it cannot hydrate and records a
    // redacted `remote-import-blocked` checkpoint on the error path.
    control_plane
        .compare_and_swap_workspace_ref(
            &workspace_id,
            1,
            &SnapshotId::new("snap_missing_remote"),
            &DeviceId::new("device_remote"),
        )
        .expect("advance to missing snapshot");

    let (outcome, checkpoints) =
        receiver.tick_with_sync_operation(&control_plane, "op_privacy_error");

    assert!(outcome.is_err(), "import of a missing snapshot must fail");
    assert!(
        has_step(&checkpoints, "remote-import-blocked"),
        "steps: {:?}",
        checkpoint_steps(&checkpoints)
    );
    let root = receiver.workspace.root().display().to_string();
    assert_checkpoints_are_redacted(
        &checkpoints,
        &[
            SECRET_ENV,
            SECRET_KEY,
            SECRET_CLIENT,
            "acme-payroll",
            "SECRET=value",
            root.as_str(),
        ],
    );
}
