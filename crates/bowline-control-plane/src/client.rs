use crate::*;

pub type ControlPlaneResult<T> = Result<T, ControlPlaneError>;

pub trait WorkspaceControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef>;

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>>;

    fn observe_workspace_ref(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.get_workspace_ref(workspace_id)
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError>;

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>>;

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord>;

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>>;

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord>;

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
    ) -> ControlPlaneResult<UploadIntent>;

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

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent>;

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn commit_uploaded_object_metadata(
        &self,
        _commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        Err(ControlPlaneError::Limited {
            capability: "object-metadata",
            reason: "committing uploaded object metadata requires a hosted control-plane implementation.",
        })
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord>;

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>>;
}

pub trait WorkViewControlPlaneClient {
    fn create_work_view(&self, _input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work views require the Phase 9 control-plane implementation.",
        })
    }

    fn list_work_views(
        &self,
        _workspace_id: &str,
        _include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view listing requires the Phase 9 control-plane implementation.",
        })
    }

    fn update_work_view_lifecycle(
        &self,
        _input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view lifecycle updates require the Phase 9 control-plane implementation.",
        })
    }

    fn restore_work_view(
        &self,
        _workspace_id: &str,
        _work_view_id: &str,
        _restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view restore requires the Phase 9 control-plane implementation.",
        })
    }

    fn commit_work_view_overlay(
        &self,
        _input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        Err(WorkViewUpdateError::Unsupported {
            capability: "work-views",
            reason: "work view overlay commits require the Phase 9 control-plane implementation.",
        })
    }
}

pub trait LeaseControlPlaneClient {
    fn create_lease(&self, _input: LeaseCreate) -> ControlPlaneResult<Lease> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease metadata requires the Phase 10 control-plane implementation.",
        })
    }

    fn update_lease(&self, _input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease metadata updates require the Phase 10 control-plane implementation.",
        })
    }

    fn list_leases(&self, _workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease listing requires the Phase 10 control-plane implementation.",
        })
    }
}

pub trait DeviceControlPlaneClient {
    fn create_device_request(&self, input: DeviceRequestInput)
    -> ControlPlaneResult<DeviceRequest>;

    fn create_bootstrap_session(
        &self,
        _input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        Err(ControlPlaneError::Limited {
            capability: "device-bootstrap",
            reason: "remote bootstrap sessions require the hosted Phase 5 control plane.",
        })
    }

    fn create_first_authorized_device(
        &self,
        _input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "first-device trust roots require the Phase 5 control-plane implementation.",
        })
    }

    fn list_device_trust(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device trust listing requires the Phase 5 control-plane implementation.",
        })
    }

    fn approve_device_request(
        &self,
        _input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "Phase 4 records pending devices only; real decrypt authority waits for Phase 5.",
        })
    }

    fn deny_device_request(&self, _input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device denial requires the Phase 5 control-plane implementation.",
        })
    }

    fn revoke_device(
        &self,
        _input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device revocation requires the Phase 5 control-plane implementation.",
        })
    }

    fn get_encrypted_device_grant(
        &self,
        _request_id: &str,
        _device_id: &str,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "grant fetching requires the Phase 5 control-plane implementation.",
        })
    }

    fn confirm_device_grant_accepted(
        &self,
        _input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "grant acceptance requires the Phase 5 control-plane implementation.",
        })
    }
}

pub trait RecoveryControlPlaneClient {
    fn create_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery envelopes require the Phase 5 control-plane implementation.",
        })
    }

    fn verify_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _verified_by_device_id: &str,
        _verified_by_device_proof: &str,
        _recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery verification requires the Phase 5 control-plane implementation.",
        })
    }

    fn rotate_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery rotation requires the Phase 5 control-plane implementation.",
        })
    }

    fn revoke_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _revoked_by_device_id: &str,
        _revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery revocation requires the Phase 5 control-plane implementation.",
        })
    }

    fn list_recovery_envelopes(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery listing requires the Phase 5 control-plane implementation.",
        })
    }

    fn authorize_device_with_recovery(
        &self,
        _input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery device authorization requires the Phase 5 control-plane implementation.",
        })
    }
}

pub trait ControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef>;

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>>;

    fn observe_workspace_ref(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.get_workspace_ref(workspace_id)
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError>;

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>>;

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord>;

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>>;

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord>;

    /// Publish a redacted live status snapshot for the workspace. In-memory and
    /// offline control planes treat this as a no-op; the hosted client forwards
    /// it to the `status:publishWorkspaceStatus` mutation.

    fn publish_workspace_status(
        &self,
        _snapshot: &WorkspaceStatusSnapshot,
    ) -> ControlPlaneResult<()> {
        Ok(())
    }

    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent>;

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

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent>;

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn commit_uploaded_object_metadata(
        &self,
        _commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        Err(ControlPlaneError::Limited {
            capability: "object-metadata",
            reason: "committing uploaded object metadata requires a hosted control-plane implementation.",
        })
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord>;

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>>;

    fn create_work_view(&self, _input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work views require the Phase 9 control-plane implementation.",
        })
    }

    fn list_work_views(
        &self,
        _workspace_id: &str,
        _include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view listing requires the Phase 9 control-plane implementation.",
        })
    }

    fn update_work_view_lifecycle(
        &self,
        _input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view lifecycle updates require the Phase 9 control-plane implementation.",
        })
    }

    fn restore_work_view(
        &self,
        _workspace_id: &str,
        _work_view_id: &str,
        _restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view restore requires the Phase 9 control-plane implementation.",
        })
    }

    fn commit_work_view_overlay(
        &self,
        _input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        Err(WorkViewUpdateError::Unsupported {
            capability: "work-views",
            reason: "work view overlay commits require the Phase 9 control-plane implementation.",
        })
    }

    fn create_lease(&self, _input: LeaseCreate) -> ControlPlaneResult<Lease> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease metadata requires the Phase 10 control-plane implementation.",
        })
    }

    fn update_lease(&self, _input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease metadata updates require the Phase 10 control-plane implementation.",
        })
    }

    fn list_leases(&self, _workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease listing requires the Phase 10 control-plane implementation.",
        })
    }

    fn create_device_request(&self, input: DeviceRequestInput)
    -> ControlPlaneResult<DeviceRequest>;

    fn create_bootstrap_session(
        &self,
        _input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        Err(ControlPlaneError::Limited {
            capability: "device-bootstrap",
            reason: "remote bootstrap sessions require the hosted Phase 5 control plane.",
        })
    }

    fn create_first_authorized_device(
        &self,
        _input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "first-device trust roots require the Phase 5 control-plane implementation.",
        })
    }

    fn list_device_trust(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device trust listing requires the Phase 5 control-plane implementation.",
        })
    }

    fn approve_device_request(
        &self,
        _input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "Phase 4 records pending devices only; real decrypt authority waits for Phase 5.",
        })
    }

    fn deny_device_request(&self, _input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device denial requires the Phase 5 control-plane implementation.",
        })
    }

    fn revoke_device(
        &self,
        _input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device revocation requires the Phase 5 control-plane implementation.",
        })
    }

    fn get_encrypted_device_grant(
        &self,
        _request_id: &str,
        _device_id: &str,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "grant fetching requires the Phase 5 control-plane implementation.",
        })
    }

    fn confirm_device_grant_accepted(
        &self,
        _input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "grant acceptance requires the Phase 5 control-plane implementation.",
        })
    }

    fn create_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery envelopes require the Phase 5 control-plane implementation.",
        })
    }

    fn verify_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _verified_by_device_id: &str,
        _verified_by_device_proof: &str,
        _recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery verification requires the Phase 5 control-plane implementation.",
        })
    }

    fn rotate_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery rotation requires the Phase 5 control-plane implementation.",
        })
    }

    fn revoke_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _revoked_by_device_id: &str,
        _revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery revocation requires the Phase 5 control-plane implementation.",
        })
    }

    fn list_recovery_envelopes(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery listing requires the Phase 5 control-plane implementation.",
        })
    }

    fn authorize_device_with_recovery(
        &self,
        _input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery device authorization requires the Phase 5 control-plane implementation.",
        })
    }
}
