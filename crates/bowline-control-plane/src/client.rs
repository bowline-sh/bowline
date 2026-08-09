use bowline_core::ids::{DeviceId, RecoveryEnvelopeId, SnapshotId, WorkspaceId};

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput, CompactEvent,
    CompareAndSwapError, ControlPlaneError, DeleteIntent, DeviceApproval, DeviceApprovalInput,
    DeviceApprovalRequestList, DeviceDenial, DeviceDenialInput, DeviceRequest, DeviceRequestInput,
    DeviceRevocationInput, DownloadIntent, DownloadIntentRequest, EncryptedGrantRequest,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, ObjectMetadataCommit,
    ObjectRetentionStateUpdate, RecoveryDeviceAuthorizationInput, RecoveryEnvelopeInput,
    RecoveryEnvelopeRecord, RevokedDeviceRecord, UploadIntentOutcome, UploadIntentRequest,
    UploadVerificationIntentRequest, WorkspaceKeyAcceptanceInput, WorkspaceKeyPublicationInput,
    WorkspaceKeyRegrantWork, WorkspaceKeyRegrantWorkRequest, WorkspaceRef,
    WorkspaceRefHistoryRecord, WorkspaceStatusSnapshot,
};

pub type ControlPlaneResult<T> = Result<T, ControlPlaneError>;

pub const MAX_OBJECT_TRANSFER_BATCH: usize = 64;

pub trait WorkspaceControlPlaneClient {
    /// Establish the workspace by seeding a version-0 genesis ref with no head.
    /// Pure establishment: no snapshot precondition and no head. The first real
    /// head (version >= 1) is published later by a genesis compare-and-swap.
    fn create_workspace_ref(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<WorkspaceRef>;

    fn get_workspace_ref(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Option<WorkspaceRef>>;

    fn observe_workspace_ref(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.get_workspace_ref(workspace_id)
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &WorkspaceId,
        expected_version: u64,
        new_snapshot_id: &SnapshotId,
        writer_device_id: &DeviceId,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        self.compare_and_swap_workspace_ref_for_project(
            workspace_id,
            expected_version,
            new_snapshot_id,
            writer_device_id,
            None,
        )
    }

    fn compare_and_swap_workspace_ref_for_project(
        &self,
        workspace_id: &WorkspaceId,
        expected_version: u64,
        new_snapshot_id: &SnapshotId,
        writer_device_id: &DeviceId,
        project_id: Option<&bowline_core::ids::ProjectId>,
    ) -> Result<WorkspaceRef, CompareAndSwapError>;

    fn list_events(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<Vec<CompactEvent>>;

    fn list_workspace_ref_history(
        &self,
        workspace_id: &WorkspaceId,
        limit: u32,
    ) -> ControlPlaneResult<Vec<WorkspaceRefHistoryRecord>>;

    /// Publish a redacted live status snapshot for the workspace. In-memory and
    /// offline control planes treat this as a no-op; the hosted client forwards
    /// it to the `status:publishWorkspaceStatus` mutation.
    fn publish_workspace_status(
        &self,
        _snapshot: &WorkspaceStatusSnapshot,
    ) -> ControlPlaneResult<()> {
        Ok(())
    }
}

pub trait ObjectControlPlaneClient {
    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntentOutcome>;

    fn reserve_object_uploads_batch(
        &self,
        requests: Vec<UploadIntentRequest>,
    ) -> ControlPlaneResult<Vec<UploadIntentOutcome>> {
        if requests.len() > MAX_OBJECT_TRANSFER_BATCH {
            return Err(ControlPlaneError::Internal {
                reason: "object upload reservation batch exceeds 64 items",
            });
        }
        requests
            .into_iter()
            .map(|request| self.create_upload_intent(request))
            .collect()
    }

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent>;

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent>;

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn create_storage_gc_delete_intent(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<DeleteIntent>;

    fn head_object_metadata(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn list_storage_gc_objects(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<bowline_storage::StorageObjectRef>>;

    fn delete_object_metadata_after_gc(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<bool>;

    fn commit_uploaded_object_metadata(
        &self,
        commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn commit_uploaded_object_metadata_batch(
        &self,
        commits: Vec<ObjectMetadataCommit>,
    ) -> ControlPlaneResult<Vec<bowline_storage::ObjectMetadata>> {
        if commits.len() > MAX_OBJECT_TRANSFER_BATCH {
            return Err(ControlPlaneError::Internal {
                reason: "object metadata commit batch exceeds 64 items",
            });
        }
        commits
            .into_iter()
            .map(|commit| self.commit_uploaded_object_metadata(commit))
            .collect()
    }
}

pub trait DeviceControlPlaneClient {
    fn create_device_request(&self, input: DeviceRequestInput)
    -> ControlPlaneResult<DeviceRequest>;

    fn create_bootstrap_session(
        &self,
        input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession>;

    fn create_first_authorized_device(
        &self,
        input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord>;

    fn list_device_trust(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<DeviceApprovalRequestList>;

    fn approve_device_request(
        &self,
        input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval>;

    fn deny_device_request(&self, input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial>;

    fn revoke_device(
        &self,
        input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord>;

    fn get_encrypted_device_grant(
        &self,
        request: EncryptedGrantRequest,
    ) -> ControlPlaneResult<Option<DeviceApproval>>;

    fn confirm_device_grant_accepted(
        &self,
        input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval>;
}

/// Distribution of workspace key epochs to devices that are already trusted.
///
/// Separate from `DeviceControlPlaneClient` because the trust question is
/// different: enrollment asks "should this device be admitted", while these
/// calls ask "which epoch does an admitted device hold, and who can seal the
/// next one for it". Every call is authenticated by a device proof, so a
/// revoked device — whose authorization row and proof verifier are gone — is
/// refused by the same check that guards every other device endpoint.
pub trait WorkspaceKeyControlPlaneClient {
    fn get_workspace_key_regrant_work(
        &self,
        request: WorkspaceKeyRegrantWorkRequest,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork>;

    fn seed_workspace_key_epoch(
        &self,
        input: WorkspaceKeyPublicationInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork>;

    fn offer_workspace_key_regrants(
        &self,
        input: WorkspaceKeyPublicationInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork>;

    fn accept_workspace_key_regrant(
        &self,
        input: WorkspaceKeyAcceptanceInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork>;
}

pub trait RecoveryControlPlaneClient {
    fn create_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord>;

    fn verify_recovery_envelope(
        &self,
        workspace_id: &WorkspaceId,
        envelope_id: &RecoveryEnvelopeId,
        verified_by_device_id: &DeviceId,
        verified_by_device_proof: &str,
        recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord>;

    fn rotate_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord>;

    fn revoke_recovery_envelope(
        &self,
        workspace_id: &WorkspaceId,
        envelope_id: &RecoveryEnvelopeId,
        revoked_by_device_id: &DeviceId,
        revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord>;

    fn list_recovery_envelopes(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>>;

    fn authorize_device_with_recovery(
        &self,
        input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval>;
}

pub trait ControlPlaneClient:
    WorkspaceControlPlaneClient
    + ObjectControlPlaneClient
    + DeviceControlPlaneClient
    + WorkspaceKeyControlPlaneClient
    + RecoveryControlPlaneClient
{
}

impl<T> ControlPlaneClient for T where
    T: WorkspaceControlPlaneClient
        + ObjectControlPlaneClient
        + DeviceControlPlaneClient
        + WorkspaceKeyControlPlaneClient
        + RecoveryControlPlaneClient
{
}
