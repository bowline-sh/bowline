use std::{
    cell::RefCell,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

use bowline_control_plane::{
    ControlPlaneError, FakeControlPlaneClient, WorkspaceControlPlaneClient,
};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::{
    metadata::{
        DEFAULT_DATABASE_FILE, MetadataError, MetadataStore, SyncOperationKind,
        SyncOperationRecord, SyncOperationState,
    },
    sync::{
        DownloadError, ScanStats, SyncRunner, SyncRunnerError, SyncRunnerFailureSource,
        SyncRunnerOptions, SyncTickOutcome, UploadError, UploadFailureSource,
    },
    workspace::TempWorkspace,
};
use bowline_storage::{
    ByteStore as _, ByteStoreError, IntentFailureKind, LocalByteStore, StorageKey,
    TransferOperation,
};

use crate::{CostReport, InvariantError, assert_local_head_supported, assert_object_before_ref};

pub struct SyncScenario {
    workspace: TempWorkspace,
    state: TempWorkspace,
    _owned_object_root: Option<TempWorkspace>,
    object_root: PathBuf,
    control_plane: FakeControlPlaneClient,
    byte_store: LocalByteStore,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    workspace_content_key: [u8; 32],
    storage_key: StorageKey,
    key_epoch: u32,
    last_scan_stats: RefCell<ScanStats>,
}

impl SyncScenario {
    pub fn new(label: &str) -> Result<Self, ScenarioError> {
        let control_plane = FakeControlPlaneClient::default();
        let object_root = TempWorkspace::new(&format!("{label}-objects"))?;
        let mut scenario = Self::with_shared_control_plane(
            label,
            "device_local",
            control_plane,
            object_root.root(),
            1,
        )?;
        scenario._owned_object_root = Some(object_root);
        Ok(scenario)
    }

    fn with_shared_control_plane(
        label: &str,
        device_id: &str,
        control_plane: FakeControlPlaneClient,
        object_root: &Path,
        clock_seed: u64,
    ) -> Result<Self, ScenarioError> {
        let workspace = TempWorkspace::new(&format!("{label}-{device_id}-workspace"))?;
        let state = TempWorkspace::new(&format!("{label}-{device_id}-state"))?;
        let object_root = object_root.to_path_buf();
        let byte_store = LocalByteStore::open_deterministic(&object_root, clock_seed)?;
        let workspace_id = WorkspaceId::new(workspace_id_for_label(label));
        if control_plane.get_workspace_ref(&workspace_id)?.is_none() {
            control_plane.create_workspace_ref(&workspace_id)?;
        }
        Ok(Self {
            workspace,
            state,
            _owned_object_root: None,
            object_root,
            control_plane,
            byte_store,
            workspace_id,
            device_id: DeviceId::new(device_id.to_string()),
            workspace_content_key: [7_u8; 32],
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            last_scan_stats: RefCell::new(ScanStats::default()),
        })
    }

    pub fn workspace(&self) -> &TempWorkspace {
        &self.workspace
    }

    pub fn state_root(&self) -> &Path {
        self.state.root()
    }

    pub fn control_plane(&self) -> &FakeControlPlaneClient {
        &self.control_plane
    }

    pub fn byte_store(&self) -> &LocalByteStore {
        &self.byte_store
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn object_root(&self) -> &Path {
        &self.object_root
    }

    pub fn tick(&self) -> Result<SyncTickOutcome, ScenarioError> {
        let runner = SyncRunner::new(&self.control_plane, &self.byte_store, self.options());
        let outcome = runner.tick();
        *self.last_scan_stats.borrow_mut() = runner.last_scan_stats();
        Ok(outcome?)
    }

    pub fn tick_with_reconcile_queue(&self) -> Result<SyncTickOutcome, ScenarioError> {
        let store = self.metadata_store()?;
        let now = "2026-07-05T12:00:00Z";
        let operation_id = self.reconcile_operation_id();
        let operation_kind = SyncOperationKind::Reconcile;
        if store
            .active_sync_operation_for_device(&self.workspace_id, operation_kind, &self.device_id)?
            .is_none()
        {
            store.enqueue_sync_operation(&SyncOperationRecord {
                id: operation_id.clone(),
                workspace_id: self.workspace_id.clone(),
                kind: operation_kind,
                resource_key: bowline_local::metadata::SyncResourceKey::workspace_sync(
                    self.workspace_id.clone(),
                ),
                state: SyncOperationState::Queued,
                idempotency_key: self.reconcile_idempotency_key(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(self.device_id.clone()),
                payload_json: "{}".to_string(),
                attempt_count: 0,
                claimed_by: None,
                claim_generation: 0,
                heartbeat_at: None,
                lease_expires_at: None,
                cancellation_requested_at: None,
                next_attempt_at: None,
                result_json: None,
                last_error_code: None,
                last_error: None,
                created_at: now.to_string(),
                updated_at: now.to_string(),
            })?;
        }
        let claimed = store
            .claim_next_sync_operation(
                &self.workspace_id,
                self.device_id.as_str(),
                now,
                "2999-01-01T00:00:00Z",
            )?
            .ok_or_else(|| {
                MetadataError::InvalidStorageMetadata(format!(
                    "reconcile operation {operation_id} was not claimable"
                ))
            })?;
        let runner = SyncRunner::new(
            &self.control_plane,
            &self.byte_store,
            self.options_with_operation(Some(claimed.claim.clone())),
        );
        let outcome = runner.tick();
        *self.last_scan_stats.borrow_mut() = runner.last_scan_stats();
        match outcome {
            Ok(outcome) => {
                store.complete_claimed_sync_operation(
                    &claimed.claim,
                    r#"{"outcome":"ok"}"#,
                    now,
                )?;
                store.set_component_state("sync", "ready", now)?;
                store.set_component_state("network", "online", now)?;
                Ok(outcome)
            }
            Err(error) => {
                if sync_runner_error_is_offline(&error) {
                    store.block_claimed_sync_operation_offline(
                        &claimed.claim,
                        "scenario-offline",
                        &error.to_string(),
                        now,
                        now,
                    )?;
                    store.set_component_state("sync", "degraded", now)?;
                    store.set_component_state("network", "offline", now)?;
                } else {
                    store.fail_claimed_sync_operation_for_retry(
                        &claimed.claim,
                        "scenario-retry",
                        &error.to_string(),
                        now,
                        now,
                    )?;
                    store.set_component_state("sync", "degraded", now)?;
                    store.set_component_state("network", "degraded", now)?;
                }
                Err(error.into())
            }
        }
    }

    pub fn assert_invariants(&self) -> Result<(), ScenarioError> {
        assert_object_before_ref(&self.control_plane, &self.byte_store, &self.workspace_id)?;
        assert_local_head_supported(self.state.root(), &self.workspace_id)?;
        Ok(())
    }

    pub fn cost_report(&self) -> CostReport {
        CostReport {
            byte_store: self.byte_store.metrics(),
            scan: self.last_scan_stats.borrow().clone(),
            control_plane_upload_intents: self.control_plane.upload_intent_request_count() as u64,
            peak_memory_bytes: None,
        }
    }

    fn options(&self) -> SyncRunnerOptions {
        self.options_with_operation(None)
    }

    fn options_with_operation(
        &self,
        sync_claim: Option<bowline_local::metadata::SyncClaimHandle>,
    ) -> SyncRunnerOptions {
        SyncRunnerOptions {
            root: self.workspace.root().to_path_buf(),
            state_root: self.state.root().to_path_buf(),
            workspace_id: self.workspace_id.clone(),
            device_id: self.device_id.clone(),
            workspace_content_key: self.workspace_content_key,
            storage_key: self.storage_key,
            key_epoch: self.key_epoch,
            generated_at: "2026-07-05T12:00:00Z".to_string(),
            sync_claim,
            scan_scope: Default::default(),
        }
    }

    fn metadata_store(&self) -> Result<MetadataStore, MetadataError> {
        MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE))
    }

    fn reconcile_operation_id(&self) -> String {
        format!("testkit-daemon-reconcile-{}", self.device_id.as_str())
    }

    fn reconcile_idempotency_key(&self) -> String {
        format!(
            "testkit-daemon-reconcile:{}:{}",
            self.workspace_id.as_str(),
            self.device_id.as_str()
        )
    }
}

fn sync_runner_error_is_offline(error: &SyncRunnerError) -> bool {
    match error.failure_source() {
        SyncRunnerFailureSource::Upload(error) => upload_error_is_offline(error),
        SyncRunnerFailureSource::Download(error) => download_error_is_offline(error),
        SyncRunnerFailureSource::ControlPlane(error) => control_plane_error_is_offline(error),
        SyncRunnerFailureSource::Retry
        | SyncRunnerFailureSource::InvalidImportedSnapshot
        | SyncRunnerFailureSource::Cache(_)
        | SyncRunnerFailureSource::WorkViewOverlay(_) => false,
    }
}

fn upload_error_is_offline(error: &UploadError) -> bool {
    match error.failure_source() {
        UploadFailureSource::ControlPlane(error) => control_plane_error_is_offline(error),
        UploadFailureSource::ByteStore(error) => byte_store_error_is_offline(error),
        UploadFailureSource::Download(error) => download_error_is_offline(error),
        UploadFailureSource::Retry => false,
    }
}

fn download_error_is_offline(error: &DownloadError) -> bool {
    match error {
        DownloadError::ControlPlane(error) => control_plane_error_is_offline(error),
        DownloadError::ByteStore(error) => byte_store_error_is_offline(error),
        DownloadError::SnapshotManifestMissing(_) => true,
        DownloadError::Manifest(_)
        | DownloadError::MetadataPage(_)
        | DownloadError::Namespace(_)
        | DownloadError::MissingBinding(_)
        | DownloadError::CancellationRequested
        | DownloadError::UnsafePath(_)
        | DownloadError::UnsafeManifest(_) => false,
    }
}

fn control_plane_error_is_offline(error: &ControlPlaneError) -> bool {
    matches!(
        error,
        ControlPlaneError::Timeout { .. } | ControlPlaneError::Transport { .. }
    )
}

fn byte_store_error_is_offline(error: &ByteStoreError) -> bool {
    matches!(
        error,
        ByteStoreError::MissingObject { .. } | ByteStoreError::Network { .. }
    ) || matches!(
        error,
        ByteStoreError::HttpStatus {
            operation: TransferOperation::Download,
            status: 404,
            ..
        } | ByteStoreError::IntentFailed {
            kind: IntentFailureKind::Timeout | IntentFailureKind::Transport,
            ..
        }
    )
}

pub struct TwoDeviceSyncScenario {
    _object_root: TempWorkspace,
    first: SyncScenario,
    second: SyncScenario,
}

impl TwoDeviceSyncScenario {
    pub fn new(label: &str) -> Result<Self, ScenarioError> {
        let control_plane = FakeControlPlaneClient::default();
        let object_root = TempWorkspace::new(&format!("{label}-shared-objects"))?;
        let first = SyncScenario::with_shared_control_plane(
            label,
            "device_first",
            control_plane.clone(),
            object_root.root(),
            11,
        )?;
        let second = SyncScenario::with_shared_control_plane(
            label,
            "device_second",
            control_plane,
            object_root.root(),
            12,
        )?;
        Ok(Self {
            _object_root: object_root,
            first,
            second,
        })
    }

    pub fn first(&self) -> &SyncScenario {
        &self.first
    }

    pub fn second(&self) -> &SyncScenario {
        &self.second
    }
}

fn workspace_id_for_label(label: &str) -> String {
    let slug = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("ws_{slug}")
}

#[derive(Debug)]
pub enum ScenarioError {
    Io(io::Error),
    Storage(ByteStoreError),
    ControlPlane(ControlPlaneError),
    Metadata(MetadataError),
    Sync(SyncRunnerError),
    Invariant(InvariantError),
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Storage(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::Sync(error) => error.fmt(formatter),
            Self::Invariant(error) => error.fmt(formatter),
        }
    }
}

impl Error for ScenarioError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::Sync(error) => Some(error),
            Self::Invariant(error) => Some(error),
        }
    }
}

impl From<io::Error> for ScenarioError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ByteStoreError> for ScenarioError {
    fn from(error: ByteStoreError) -> Self {
        Self::Storage(error)
    }
}

impl From<ControlPlaneError> for ScenarioError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<MetadataError> for ScenarioError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<SyncRunnerError> for ScenarioError {
    fn from(error: SyncRunnerError) -> Self {
        Self::Sync(error)
    }
}

impl From<InvariantError> for ScenarioError {
    fn from(error: InvariantError) -> Self {
        Self::Invariant(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_is_opaque_and_stable() {
        assert_eq!(workspace_id_for_label("Plan 16"), "ws_plan_16");
    }

    #[test]
    fn scenario_uses_real_temp_workspace() {
        let scenario = SyncScenario::new("scenario-real-temp").expect("scenario");

        assert!(scenario.workspace().root().exists());
        assert!(scenario.state_root().exists());
    }

    #[test]
    fn two_device_hook_shares_control_plane_and_object_root() {
        let scenario = TwoDeviceSyncScenario::new("two-device-hook").expect("scenario");

        assert_eq!(
            scenario.first().object_root(),
            scenario.second().object_root()
        );
        assert_eq!(
            scenario.first().workspace_id(),
            scenario.second().workspace_id()
        );
    }
}
