use super::*;
use crate::*;
use crate::{
    ControlPlaneClient, DeviceControlPlaneClient, LeaseControlPlaneClient,
    ObjectControlPlaneClient, RecoveryControlPlaneClient, WorkViewControlPlaneClient,
    WorkspaceControlPlaneClient,
};

impl ControlPlaneClient for HostedControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef> {
        WorkspaceControlPlaneClient::create_workspace_ref(self, workspace_id)
    }

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>> {
        WorkspaceControlPlaneClient::get_workspace_ref(self, workspace_id)
    }

    fn observe_workspace_ref(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        WorkspaceControlPlaneClient::observe_workspace_ref(self, workspace_id)
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        WorkspaceControlPlaneClient::compare_and_swap_workspace_ref(
            self,
            workspace_id,
            expected_version,
            new_snapshot_id,
            writer_device_id,
        )
    }

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>> {
        WorkspaceControlPlaneClient::list_events(self, workspace_id)
    }

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        WorkspaceControlPlaneClient::publish_conflict_metadata(self, input)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        WorkspaceControlPlaneClient::list_workspace_conflicts(
            self,
            workspace_id,
            requested_by_device_id,
        )
    }

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        WorkspaceControlPlaneClient::mark_conflict_resolved(self, input)
    }

    fn publish_workspace_status(
        &self,
        _snapshot: &WorkspaceStatusSnapshot,
    ) -> ControlPlaneResult<()> {
        WorkspaceControlPlaneClient::publish_workspace_status(self, _snapshot)
    }

    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent> {
        ObjectControlPlaneClient::create_upload_intent(self, request)
    }

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        ObjectControlPlaneClient::create_download_intent(self, request)
    }

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        ObjectControlPlaneClient::create_upload_verification_intent(self, request)
    }

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        ObjectControlPlaneClient::mark_object_retention_state(self, update)
    }

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent> {
        ObjectControlPlaneClient::create_delete_intent(self, request)
    }

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        ObjectControlPlaneClient::head_object_metadata(self, workspace_id, object_key)
    }

    fn commit_uploaded_object_metadata(
        &self,
        _commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        ObjectControlPlaneClient::commit_uploaded_object_metadata(self, _commit)
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord> {
        ObjectControlPlaneClient::commit_object_manifest(self, commit)
    }

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>> {
        ObjectControlPlaneClient::get_snapshot_manifest_pointer(self, workspace_id, snapshot_id)
    }

    fn create_work_view(&self, _input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        WorkViewControlPlaneClient::create_work_view(self, _input)
    }

    fn list_work_views(
        &self,
        _workspace_id: &str,
        _include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        WorkViewControlPlaneClient::list_work_views(self, _workspace_id, _include_all)
    }

    fn update_work_view_lifecycle(
        &self,
        _input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        WorkViewControlPlaneClient::update_work_view_lifecycle(self, _input)
    }

    fn restore_work_view(
        &self,
        _workspace_id: &str,
        _work_view_id: &str,
        _restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        WorkViewControlPlaneClient::restore_work_view(
            self,
            _workspace_id,
            _work_view_id,
            _restored_by_device_id,
        )
    }

    fn commit_work_view_overlay(
        &self,
        _input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        WorkViewControlPlaneClient::commit_work_view_overlay(self, _input)
    }

    fn create_lease(&self, _input: LeaseCreate) -> ControlPlaneResult<Lease> {
        LeaseControlPlaneClient::create_lease(self, _input)
    }

    fn update_lease(&self, _input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        LeaseControlPlaneClient::update_lease(self, _input)
    }

    fn list_leases(&self, _workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        LeaseControlPlaneClient::list_leases(self, _workspace_id)
    }

    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        DeviceControlPlaneClient::create_device_request(self, input)
    }

    fn create_bootstrap_session(
        &self,
        _input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        DeviceControlPlaneClient::create_bootstrap_session(self, _input)
    }

    fn create_first_authorized_device(
        &self,
        _input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        DeviceControlPlaneClient::create_first_authorized_device(self, _input)
    }

    fn list_device_trust(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        DeviceControlPlaneClient::list_device_trust(self, _workspace_id)
    }

    fn approve_device_request(
        &self,
        _input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        DeviceControlPlaneClient::approve_device_request(self, _input)
    }

    fn deny_device_request(&self, _input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        DeviceControlPlaneClient::deny_device_request(self, _input)
    }

    fn revoke_device(
        &self,
        _input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        DeviceControlPlaneClient::revoke_device(self, _input)
    }

    fn get_encrypted_device_grant(
        &self,
        _request_id: &str,
        _device_id: &str,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        DeviceControlPlaneClient::get_encrypted_device_grant(self, _request_id, _device_id)
    }

    fn confirm_device_grant_accepted(
        &self,
        _input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        DeviceControlPlaneClient::confirm_device_grant_accepted(self, _input)
    }

    fn create_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        RecoveryControlPlaneClient::create_recovery_envelope(self, _input)
    }

    fn verify_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _verified_by_device_id: &str,
        _verified_by_device_proof: &str,
        _recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        RecoveryControlPlaneClient::verify_recovery_envelope(
            self,
            _workspace_id,
            _envelope_id,
            _verified_by_device_id,
            _verified_by_device_proof,
            _recovery_proof,
        )
    }

    fn rotate_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        RecoveryControlPlaneClient::rotate_recovery_envelope(self, _input)
    }

    fn revoke_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _revoked_by_device_id: &str,
        _revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        RecoveryControlPlaneClient::revoke_recovery_envelope(
            self,
            _workspace_id,
            _envelope_id,
            _revoked_by_device_id,
            _revoked_by_device_proof,
        )
    }

    fn list_recovery_envelopes(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        RecoveryControlPlaneClient::list_recovery_envelopes(self, _workspace_id)
    }

    fn authorize_device_with_recovery(
        &self,
        _input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        RecoveryControlPlaneClient::authorize_device_with_recovery(self, _input)
    }
}
