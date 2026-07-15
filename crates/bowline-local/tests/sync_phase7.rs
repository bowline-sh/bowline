// Integration-test crate: long end-to-end scenario tests are expected here.
#![allow(clippy::too_many_lines)]

use std::{
    collections::BTreeSet,
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

use bowline_control_plane::{
    Capability, CapabilityReporting, CompactEvent, CompareAndSwapError, ConflictMetadataRecord,
    ConflictOccurrenceReconcile, ConflictOccurrenceState, ConflictReconcileOutcome,
    ConflictReconcileResult, ControlPlaneClient, ControlPlaneResult, DeviceControlPlaneClient,
    DeviceRequest, DeviceRequestInput, DownloadIntent, DownloadIntentRequest,
    FakeControlPlaneClient, LeaseControlPlaneClient, MetadataBindingBatch, MetadataBindingCommit,
    ObjectControlPlaneClient, ObjectMetadataCommit, ObjectRetentionStateUpdate,
    RecoveryControlPlaneClient, SnapshotRootCommit, SnapshotRootRecord, UploadIntent,
    UploadIntentRequest, UploadVerificationIntentRequest, WorkViewControlPlaneClient,
    WorkspaceControlPlaneClient, WorkspaceRef,
};
use bowline_core::{
    events::EventName,
    ids::{DeviceId, ProjectId, SnapshotId, WorkspaceId},
    namespace_snapshot::{
        EntryVisitor, NamespaceOperationBudget, NamespaceOperationContext, NamespaceReadError,
        NamespaceSnapshotReader, NamespaceVisitControl,
    },
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, HydrationState, NamespaceEntry,
        NamespaceEntryKind, SegmentLocator, SnapshotDraft, WorkspaceRelativePath,
        workspace_content_id,
    },
};
use bowline_local::metadata::{
    DEFAULT_DATABASE_FILE, SyncClaimCheck, SyncCommittedCancelledLateResult, SyncOperationKind,
    SyncOperationRecord, SyncOperationState, SyncResourceKey, WorkspaceSyncHeadRecord,
};
use bowline_local::{
    metadata::MetadataStore,
    sync::{
        ConflictBundleError, ConflictFile, ConflictRecord, ConflictSpan, DownloadError,
        MergeOutcome, PreparedContent, SnapshotContent, SyncRunner, SyncRunnerError,
        SyncRunnerOptions, SyncTickOutcome, WorkViewOverlaySyncResult, coalesce_workspace_scan,
        conflict_occurrence_is_current, conflict_occurrence_queue_result, create_conflict_bundle,
        decode_conflict_occurrence_operation, decode_work_view_overlay_sync_operation,
        import_snapshot_by_id, mark_conflict_occurrence_reconciled, merge_snapshots,
        namespace::MetadataIdentityKey, open_remote_snapshot_by_id, upload_snapshot_candidate,
        work_view_overlay_sync_result,
    },
    workspace::TempWorkspace,
};
use bowline_storage::{ByteStore, LocalByteStore, StorageKey};
use bowline_storage::{LocalContentCache, ObjectKey, RangeHydrationRequest};

#[path = "sync_phase7/conflicts.rs"]
mod conflicts;
#[path = "sync_phase7/core.rs"]
mod core;
#[path = "sync_phase7/imports.rs"]
mod imports;

fn snapshot_with_file(
    workspace_id: &WorkspaceId,
    _snapshot_id: &str,
    path: &str,
    bytes: &[u8],
) -> bowline_local::sync::SnapshotContent {
    let content_id = workspace_content_id([9_u8; 32], bytes);
    let entries = vec![NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: bowline_core::policy::PathClassification::WorkspaceSync,
        mode: bowline_core::policy::MaterializationMode::WorkspaceSync,
        access: Vec::new(),
        content_id: Some(content_id.clone()),
        content_layout: Some(
            ContentLayout::single_segment(ContentLocator {
                content_id: content_id.clone(),
                storage: ContentStorage::Packed,
                raw_size: bytes.len() as u64,
                pack_id: Some(bowline_core::ids::PackId::new("pk_test")),
                offset: Some(0),
                length: Some(1),
            })
            .expect("test layout"),
        ),
        symlink_target: None,
        byte_len: Some(bytes.len() as u64),
        executability: bowline_core::workspace_graph::FileExecutability::Regular,
        hydration_state: bowline_core::workspace_graph::HydrationState::Cold,
    }];
    let canonical_snapshot_id =
        bowline_local::sync::rebuild_manifest_identity(workspace_id, &entries, "test")
            .snapshot_id()
            .clone();
    bowline_local::sync::SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: canonical_snapshot_id,
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: Vec::new(),
        },
        [(content_id, bytes.to_vec())].into_iter().collect(),
        [9; 32],
    )
    .expect("page-backed file snapshot")
}

fn canonical_conflict_occurrence(
    mut record: ConflictRecord,
    base_snapshot_id: impl Into<String>,
    remote_snapshot_id: impl Into<String>,
) -> ConflictRecord {
    record.base_snapshot_id = Some(base_snapshot_id.into());
    record.remote_snapshot_id = Some(remote_snapshot_id.into());
    record
}

fn snapshot_with_symlink(
    workspace_id: &WorkspaceId,
    _snapshot_id: &str,
    path: &str,
    target: &str,
) -> bowline_local::sync::SnapshotContent {
    let entries = vec![NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::Symlink,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: None,
        content_layout: None,
        symlink_target: Some(target.to_string()),
        byte_len: None,
        executability: bowline_core::workspace_graph::FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }];
    let canonical_snapshot_id =
        bowline_local::sync::rebuild_manifest_identity(workspace_id, &entries, "test")
            .snapshot_id()
            .clone();
    bowline_local::sync::SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: canonical_snapshot_id,
            workspace_id: workspace_id.clone(),
            project_id: None::<bowline_core::ids::ProjectId>,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: Vec::new(),
        },
        Default::default(),
        [9; 32],
    )
    .expect("page-backed symlink snapshot")
}

fn content_locator_for_segment(segment: &SegmentLocator) -> ContentLocator {
    ContentLocator {
        content_id: bowline_core::ids::ContentId::new(segment.segment_id.as_str()),
        storage: ContentStorage::Packed,
        raw_size: segment.plaintext_length,
        pack_id: Some(segment.pack_id.clone()),
        offset: Some(segment.offset),
        length: Some(segment.length),
    }
}

fn empty_snapshot(
    workspace_id: &WorkspaceId,
    _snapshot_id: &str,
) -> bowline_local::sync::SnapshotContent {
    let entries = Vec::new();
    let canonical_snapshot_id =
        bowline_local::sync::rebuild_manifest_identity(workspace_id, &entries, "test")
            .snapshot_id()
            .clone();
    bowline_local::sync::SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: canonical_snapshot_id,
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: Vec::new(),
        },
        Default::default(),
        [9; 32],
    )
    .expect("page-backed empty snapshot")
}

fn snapshot_entries(snapshot: &SnapshotContent) -> Vec<NamespaceEntry> {
    struct Collector(Vec<NamespaceEntry>);

    impl EntryVisitor for Collector {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            self.0.push(entry.clone());
            Ok(NamespaceVisitControl::Continue)
        }
    }

    let mut context = NamespaceOperationContext::uncancelled(NamespaceOperationBudget::new(
        snapshot.manifest().entry_count,
        0,
        0,
    ));
    let mut collector = Collector(Vec::new());
    snapshot
        .visit_entries(&mut context, &mut collector)
        .expect("page-backed integration snapshot");
    collector.0
}

fn coalesced_candidate_from_snapshot(
    workspace_id: &WorkspaceId,
    base_ref: &bowline_control_plane::WorkspaceRef,
    device_id: &str,
    snapshot: bowline_local::sync::SnapshotContent,
) -> bowline_local::sync::SnapshotCandidate {
    let manifest_identity = bowline_local::sync::rebuild_manifest_identity(
        &snapshot.manifest().workspace_id,
        &snapshot_entries(&snapshot),
        "2026-06-24T12:00:00Z",
    );
    bowline_local::sync::SnapshotCandidate {
        base: bowline_local::sync::CandidateBase::from_remote(base_ref),
        device_id: DeviceId::new(device_id),
        manifest_id: bowline_local::sync::manifest_id_for_snapshot(
            &snapshot.manifest().snapshot_id,
        ),
        snapshot,
        scan_report: bowline_local::scanner::ScanReport {
            root: std::path::PathBuf::new(),
            projects: Vec::new(),
            paths: Vec::new(),
            summary: Default::default(),
        },
        scan_scope: bowline_local::sync::ScanScope::Full(
            bowline_local::sync::FullScanReason::CliRequested,
        ),
        stat_cache_hit_paths: BTreeSet::new(),
        stat_cache_divergences: Vec::new(),
        scan_stats: Default::default(),
        manifest_identity,
        stat_cache_write_back: None,
        causation_ids: vec![format!("test:{}", workspace_id.as_str())],
        skipped_unsafe_symlinks: BTreeSet::new(),
        created_at: "2026-06-24T12:00:00Z".to_string(),
    }
}

struct CasFailsOnceControlPlane {
    inner: FakeControlPlaneClient,
    should_fail_cas: AtomicBool,
    should_fail_conflict_publish: AtomicBool,
}

impl CasFailsOnceControlPlane {
    fn new(inner: FakeControlPlaneClient) -> Self {
        Self {
            inner,
            should_fail_cas: AtomicBool::new(true),
            should_fail_conflict_publish: AtomicBool::new(false),
        }
    }

    fn new_conflict_publish_fails_once(inner: FakeControlPlaneClient) -> Self {
        Self {
            inner,
            should_fail_cas: AtomicBool::new(false),
            should_fail_conflict_publish: AtomicBool::new(true),
        }
    }
}

impl WorkspaceControlPlaneClient for CasFailsOnceControlPlane {
    fn create_workspace_ref(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<WorkspaceRef> {
        self.inner.create_workspace_ref(workspace_id)
    }

    fn get_workspace_ref(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.inner.get_workspace_ref(workspace_id)
    }

    fn compare_and_swap_workspace_ref_for_project(
        &self,
        workspace_id: &WorkspaceId,
        expected_version: u64,
        new_snapshot_id: &SnapshotId,
        writer_device_id: &DeviceId,
        project_id: Option<&ProjectId>,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        if self.should_fail_cas.swap(false, Ordering::SeqCst) {
            return Err(CompareAndSwapError::Storage(
                "injected CAS failure after manifest commit".to_string(),
            ));
        }
        self.inner.compare_and_swap_workspace_ref_for_project(
            workspace_id,
            expected_version,
            new_snapshot_id,
            writer_device_id,
            project_id,
        )
    }

    fn list_events(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<Vec<CompactEvent>> {
        self.inner.list_events(workspace_id)
    }

    fn reconcile_conflict_occurrence(
        &self,
        input: ConflictOccurrenceReconcile,
    ) -> ControlPlaneResult<ConflictReconcileResult> {
        if self
            .should_fail_conflict_publish
            .swap(false, Ordering::SeqCst)
        {
            return Err(bowline_control_plane::ControlPlaneError::Storage(
                "injected conflict metadata publish failure".to_string(),
            ));
        }
        self.inner.reconcile_conflict_occurrence(input)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &WorkspaceId,
        requested_by_device_id: &DeviceId,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.inner
            .list_workspace_conflicts(workspace_id, requested_by_device_id)
    }
}

impl ObjectControlPlaneClient for CasFailsOnceControlPlane {
    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent> {
        self.inner.create_upload_intent(request)
    }

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        self.inner.create_download_intent(request)
    }

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        self.inner.create_upload_verification_intent(request)
    }

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        self.inner.mark_object_retention_state(update)
    }

    fn head_object_metadata(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        self.inner.head_object_metadata(workspace_id, object_key)
    }

    fn commit_uploaded_object_metadata(
        &self,
        commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        self.inner.commit_uploaded_object_metadata(commit)
    }

    fn commit_metadata_bindings(
        &self,
        commit: MetadataBindingCommit,
    ) -> ControlPlaneResult<MetadataBindingBatch> {
        self.inner.commit_metadata_bindings(commit)
    }

    fn resolve_metadata_bindings(
        &self,
        workspace_id: &WorkspaceId,
        logical_ids: &[String],
    ) -> ControlPlaneResult<MetadataBindingBatch> {
        self.inner
            .resolve_metadata_bindings(workspace_id, logical_ids)
    }

    fn commit_snapshot_root(
        &self,
        commit: SnapshotRootCommit,
    ) -> ControlPlaneResult<SnapshotRootRecord> {
        self.inner.commit_snapshot_root(commit)
    }

    fn get_snapshot_root(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> ControlPlaneResult<Option<SnapshotRootRecord>> {
        self.inner.get_snapshot_root(workspace_id, snapshot_id)
    }
}

impl DeviceControlPlaneClient for CasFailsOnceControlPlane {
    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        self.inner.create_device_request(input)
    }
}

// These tests never exercise the remaining control-plane traits through this
// wrapper, so the trait defaults are enough.
impl WorkViewControlPlaneClient for CasFailsOnceControlPlane {}
impl LeaseControlPlaneClient for CasFailsOnceControlPlane {}
impl RecoveryControlPlaneClient for CasFailsOnceControlPlane {}

impl CapabilityReporting for CasFailsOnceControlPlane {
    fn capabilities(&self) -> BTreeSet<Capability> {
        self.inner.capabilities()
    }
}

fn mark_only_conflict_bundle_state(state_root: &std::path::Path, state: &str) {
    let conflicts_root = state_root.join("conflicts");
    let mut entries = fs::read_dir(&conflicts_root)
        .expect("conflicts root")
        .collect::<Result<Vec<_>, _>>()
        .expect("conflict entries")
        .into_iter()
        .filter(|entry| {
            entry.file_type().expect("conflict entry type").is_dir()
                && entry.path().join("manifest.json").is_file()
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "test expects one conflict bundle");
    let manifest_path = entries.remove(0).path().join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest")).expect("json");
    manifest["state"] = serde_json::Value::String(state.to_string());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .expect("write manifest");
}

fn drive_pending_conflict_occurrences(
    state_root: &std::path::Path,
    workspace_id: &WorkspaceId,
    control_plane: &dyn ControlPlaneClient,
    now: &str,
) -> Result<usize, String> {
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE))
        .map_err(|error| error.to_string())?;
    let mut processed = 0;
    loop {
        let Some(claimed) = store
            .claim_next_sync_operation(
                workspace_id,
                "phase7-conflict-worker",
                now,
                "2999-01-01T00:00:00Z",
            )
            .map_err(|error| error.to_string())?
        else {
            return Ok(processed);
        };
        if claimed.operation.kind == SyncOperationKind::WorkViewOverlaySync {
            complete_empty_work_view_overlay(&store, &claimed, workspace_id, now)?;
            continue;
        }
        if claimed.operation.kind != SyncOperationKind::ConflictOccurrenceReconcile {
            return Err(format!(
                "conflict worker claimed unexpected operation kind {:?}",
                claimed.operation.kind
            ));
        }
        match store
            .authorize_sync_operation_boundary(&claimed.claim)
            .map_err(|error| error.to_string())?
        {
            SyncClaimCheck::Owned => {}
            SyncClaimCheck::CancellationRequested => {
                store
                    .cancel_claimed_sync_operation(
                        &claimed.claim,
                        r#"{"outcome":"cancelled"}"#,
                        now,
                    )
                    .map_err(|error| error.to_string())?;
                return Err("conflict operation was cancelled before hosted reconcile".to_string());
            }
            SyncClaimCheck::OwnershipLost => {
                return Err("conflict operation claim ownership was lost".to_string());
            }
        }

        let input = decode_conflict_occurrence_operation(&claimed.operation)
            .map_err(|error| error.to_string())?;
        let local_state = match input.desired_state {
            ConflictOccurrenceState::Unresolved => bowline_local::sync::ConflictState::Unresolved,
            ConflictOccurrenceState::Accepted => bowline_local::sync::ConflictState::Accepted,
            ConflictOccurrenceState::Rejected => bowline_local::sync::ConflictState::Rejected,
        };
        let current = conflict_occurrence_is_current(
            state_root,
            input.conflict_id.as_str(),
            input.occurrence_version,
            local_state,
        )
        .map_err(|error| error.to_string())?;
        let remote_outcome = if current {
            match control_plane.reconcile_conflict_occurrence(input.clone()) {
                Ok(result) => result.outcome,
                Err(error) => {
                    store
                        .fail_claimed_sync_operation_for_retry(
                            &claimed.claim,
                            "test-conflict-reconcile",
                            &error.to_string(),
                            now,
                            now,
                        )
                        .map_err(|transition_error| transition_error.to_string())?;
                    return Err(error.to_string());
                }
            }
        } else {
            ConflictReconcileOutcome::Superseded
        };
        let terminal_claim_state = store
            .renew_sync_operation_reconciliation_boundary(&claimed.claim)
            .map_err(|error| error.to_string())?;
        if terminal_claim_state == SyncClaimCheck::OwnershipLost {
            return Err(
                "conflict operation claim ownership was lost after hosted reconcile".to_string(),
            );
        }
        let outcome = if matches!(
            remote_outcome,
            ConflictReconcileOutcome::Applied | ConflictReconcileOutcome::Idempotent
        ) {
            if mark_conflict_occurrence_reconciled(
                state_root,
                input.conflict_id.as_str(),
                input.occurrence_version,
                local_state,
                now,
            )
            .map_err(|error| error.to_string())?
            {
                remote_outcome
            } else {
                ConflictReconcileOutcome::Superseded
            }
        } else {
            remote_outcome
        };
        let result_json =
            conflict_occurrence_queue_result(outcome).map_err(|error| error.to_string())?;
        match terminal_claim_state {
            SyncClaimCheck::Owned => {
                store
                    .complete_claimed_sync_operation(&claimed.claim, &result_json, now)
                    .map_err(|error| error.to_string())?;
            }
            SyncClaimCheck::CancellationRequested => {
                let committed_result =
                    serde_json::from_str(&result_json).map_err(|error| error.to_string())?;
                store
                    .complete_committed_cancelled_late_sync_operation(
                        &claimed.claim,
                        &SyncCommittedCancelledLateResult::new(
                            SyncOperationKind::ConflictOccurrenceReconcile,
                            committed_result,
                        ),
                        now,
                    )
                    .map_err(|error| error.to_string())?;
            }
            SyncClaimCheck::OwnershipLost => unreachable!("ownership loss returned above"),
        }
        processed += 1;
    }
}

fn complete_empty_work_view_overlay(
    store: &MetadataStore,
    claimed: &bowline_local::metadata::ClaimedSyncOperation,
    workspace_id: &WorkspaceId,
    now: &str,
) -> Result<(), String> {
    let input = decode_work_view_overlay_sync_operation(&claimed.operation)
        .map_err(|error| error.to_string())?;
    if input.workspace_id != *workspace_id {
        return Err("work-view overlay operation targeted another workspace".to_string());
    }
    if !store
        .work_views(workspace_id, true, None)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Err("conflict fixture unexpectedly contains work views".to_string());
    }
    if store
        .authorize_sync_operation_boundary(&claimed.claim)
        .map_err(|error| error.to_string())?
        != SyncClaimCheck::Owned
    {
        return Err("work-view overlay predecessor claim was not owned".to_string());
    }
    let result_json = work_view_overlay_sync_result(WorkViewOverlaySyncResult {
        uploaded: 0,
        attention: 0,
        ..WorkViewOverlaySyncResult::default()
    })
    .map_err(|error| error.to_string())?;
    if store
        .authorize_sync_operation_boundary(&claimed.claim)
        .map_err(|error| error.to_string())?
        != SyncClaimCheck::Owned
    {
        return Err("work-view overlay predecessor lost its claim".to_string());
    }
    store
        .complete_claimed_sync_operation(&claimed.claim, &result_json, now)
        .map_err(|error| error.to_string())?;
    Ok(())
}
