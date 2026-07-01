use std::{
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

use bowline_control_plane::{
    CompactEvent, CompareAndSwapError, ConflictMetadataPublish, ConflictMetadataRecord,
    ConflictResolutionMark, ControlPlaneClient, ControlPlaneResult, DeleteIntent,
    DeleteIntentRequest, DeviceRequest, DeviceRequestInput, DownloadIntent, DownloadIntentRequest,
    FakeControlPlaneClient, ObjectManifestCommit, ObjectManifestRecord, ObjectRetentionStateUpdate,
    UploadIntent, UploadIntentRequest, UploadVerificationIntentRequest, WorkspaceRef,
};
use bowline_core::{
    events::EventName,
    ids::{DeviceId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentStorage, HydrationState, NamespaceEntry, NamespaceEntryKind, SnapshotManifest,
        workspace_content_id,
    },
};
use bowline_local::metadata::{HydrationQueueRecord, SyncOperationRecord, WorkspaceSyncHeadRecord};
use bowline_local::{
    metadata::MetadataStore,
    status::{StatusOptions, compose_status},
    sync::{
        ConflictBundleError, ConflictFile, ConflictRecord, ConflictSpan, DownloadError,
        MergeOutcome, SyncRunner, SyncRunnerOptions, SyncTickOutcome, coalesce_workspace_scan,
        create_conflict_bundle, import_snapshot_by_id, merge_snapshots, upload_snapshot_candidate,
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
    snapshot_id: &str,
    path: &str,
    bytes: &[u8],
) -> bowline_local::sync::SnapshotContent {
    let content_id = workspace_content_id([9_u8; 32], bytes);
    bowline_local::sync::SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: bowline_core::ids::SnapshotId::new(snapshot_id),
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: vec![NamespaceEntry {
                path: path.to_string(),
                kind: NamespaceEntryKind::File,
                classification: bowline_core::policy::PathClassification::WorkspaceSync,
                mode: bowline_core::policy::MaterializationMode::WorkspaceSync,
                access: Vec::new(),
                content_id: Some(content_id.clone()),
                locator: Some(bowline_core::workspace_graph::ContentLocator {
                    content_id: content_id.clone(),
                    storage: ContentStorage::Packed,
                    raw_size: bytes.len() as u64,
                    pack_id: Some(bowline_core::ids::PackId::new("pk_test")),
                    offset: Some(0),
                    length: Some(1),
                    chunk_ids: Vec::new(),
                }),
                symlink_target: None,
                byte_len: Some(bytes.len() as u64),
                hydration_state: bowline_core::workspace_graph::HydrationState::Cold,
            }],
            refs: Vec::new(),
        },
        [(content_id, bytes.to_vec())].into_iter().collect(),
    )
}

fn snapshot_with_symlink(
    workspace_id: &WorkspaceId,
    snapshot_id: &str,
    path: &str,
    target: &str,
) -> bowline_local::sync::SnapshotContent {
    bowline_local::sync::SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: bowline_core::ids::SnapshotId::new(snapshot_id.to_string()),
            workspace_id: workspace_id.clone(),
            project_id: None::<bowline_core::ids::ProjectId>,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: vec![NamespaceEntry {
                path: path.to_string(),
                kind: NamespaceEntryKind::Symlink,
                classification: PathClassification::WorkspaceSync,
                mode: MaterializationMode::WorkspaceSync,
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                content_id: None,
                locator: None,
                symlink_target: Some(target.to_string()),
                byte_len: None,
                hydration_state: HydrationState::Local,
            }],
            refs: Vec::new(),
        },
        Default::default(),
    )
}

fn empty_snapshot(
    workspace_id: &WorkspaceId,
    snapshot_id: &str,
) -> bowline_local::sync::SnapshotContent {
    bowline_local::sync::SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: bowline_core::ids::SnapshotId::new(snapshot_id),
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: Vec::new(),
            refs: Vec::new(),
        },
        Default::default(),
    )
}

fn coalesced_candidate_from_snapshot(
    workspace_id: &WorkspaceId,
    base_ref: &bowline_control_plane::WorkspaceRef,
    device_id: &str,
    snapshot: bowline_local::sync::SnapshotContent,
) -> bowline_local::sync::SnapshotCandidate {
    bowline_local::sync::SnapshotCandidate {
        base: bowline_local::sync::CandidateBase::from_remote(base_ref),
        device_id: DeviceId::new(device_id),
        manifest_id: bowline_local::sync::manifest_id_for_snapshot(&snapshot.manifest.snapshot_id),
        snapshot,
        scan_report: bowline_local::scanner::ScanReport {
            root: std::path::PathBuf::new(),
            projects: Vec::new(),
            paths: Vec::new(),
            summary: Default::default(),
        },
        causation_ids: vec![format!("test:{}", workspace_id.as_str())],
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

impl ControlPlaneClient for CasFailsOnceControlPlane {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef> {
        self.inner.create_workspace_ref(workspace_id)
    }

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.inner.get_workspace_ref(workspace_id)
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        if self.should_fail_cas.swap(false, Ordering::SeqCst) {
            return Err(CompareAndSwapError::Storage(
                "injected CAS failure after manifest commit".to_string(),
            ));
        }
        self.inner.compare_and_swap_workspace_ref(
            workspace_id,
            expected_version,
            new_snapshot_id,
            writer_device_id,
        )
    }

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>> {
        self.inner.list_events(workspace_id)
    }

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        if self
            .should_fail_conflict_publish
            .swap(false, Ordering::SeqCst)
        {
            return Err(bowline_control_plane::ControlPlaneError::Storage(
                "injected conflict metadata publish failure".to_string(),
            ));
        }
        self.inner.publish_conflict_metadata(input)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.inner
            .list_workspace_conflicts(workspace_id, requested_by_device_id)
    }

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.inner.mark_conflict_resolved(input)
    }

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

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent> {
        self.inner.create_delete_intent(request)
    }

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        self.inner.head_object_metadata(workspace_id, object_key)
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord> {
        self.inner.commit_object_manifest(commit)
    }

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>> {
        self.inner
            .get_snapshot_manifest_pointer(workspace_id, snapshot_id)
    }

    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        self.inner.create_device_request(input)
    }
}

fn mark_only_conflict_bundle_state(state_root: &std::path::Path, state: &str) {
    let conflicts_root = state_root.join("conflicts");
    let mut entries = fs::read_dir(&conflicts_root)
        .expect("conflicts root")
        .collect::<Result<Vec<_>, _>>()
        .expect("conflict entries");
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
