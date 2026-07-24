use std::collections::BTreeSet;

use bowline_core::ids::{
    DeviceApprovalRequestId, DeviceId, RecoveryEnvelopeId, SnapshotId, WorkspaceId,
};

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput, Capability, CompactEvent,
    CompareAndSwapError, ControlPlaneError, DeleteIntent, DeviceApproval, DeviceApprovalInput,
    DeviceApprovalRequestList, DeviceDenial, DeviceDenialInput, DeviceRequest, DeviceRequestInput,
    DeviceRevocationInput, DownloadIntent, DownloadIntentRequest, FirstAuthorizedDeviceInput,
    GrantAcceptanceInput, ObjectMetadataCommit, ObjectRetentionStateUpdate,
    RecoveryDeviceAuthorizationInput, RecoveryEnvelopeInput, RecoveryEnvelopeRecord,
    RevokedDeviceRecord, UploadIntent, UploadIntentRequest, UploadVerificationIntentRequest,
    WorkspaceRef, WorkspaceRefHistoryRecord, WorkspaceStatusSnapshot,
};

pub type ControlPlaneResult<T> = Result<T, ControlPlaneError>;

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
        _workspace_id: &WorkspaceId,
        _limit: u32,
    ) -> ControlPlaneResult<Vec<WorkspaceRefHistoryRecord>> {
        Err(ControlPlaneError::Limited {
            capability: Capability::WorkspaceRefHistory,
            reason: "workspace ref history requires a hosted control-plane implementation.",
        })
    }

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

    fn create_storage_gc_delete_intent(
        &self,
        _workspace_id: &WorkspaceId,
        _object_key: &str,
    ) -> ControlPlaneResult<DeleteIntent> {
        Err(ControlPlaneError::Limited {
            capability: Capability::StorageGc,
            reason: "storage GC byte deletion requires a hosted control-plane implementation.",
        })
    }

    fn head_object_metadata(
        &self,
        workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn list_storage_gc_objects(
        &self,
        _workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<bowline_storage::StorageObjectRef>> {
        Err(ControlPlaneError::Limited {
            capability: Capability::StorageGc,
            reason: "storage GC requires a hosted control-plane implementation.",
        })
    }

    fn delete_object_metadata_after_gc(
        &self,
        _workspace_id: &WorkspaceId,
        _object_key: &str,
    ) -> ControlPlaneResult<bool> {
        Err(ControlPlaneError::Limited {
            capability: Capability::StorageGc,
            reason: "storage GC metadata finalization requires a hosted control-plane implementation.",
        })
    }

    fn commit_uploaded_object_metadata(
        &self,
        _commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        Err(ControlPlaneError::Limited {
            capability: Capability::ObjectMetadata,
            reason: "committing uploaded object metadata requires a hosted control-plane implementation.",
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
            capability: Capability::DeviceBootstrap,
            reason: "remote bootstrap sessions require a hosted control-plane implementation.",
        })
    }

    fn create_first_authorized_device(
        &self,
        _input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: Capability::DeviceTrust,
            reason: "first-device trust roots require a hosted control-plane implementation.",
        })
    }

    fn list_device_trust(
        &self,
        _workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        Err(ControlPlaneError::Limited {
            capability: Capability::DeviceTrust,
            reason: "device trust listing requires a hosted control-plane implementation.",
        })
    }

    fn approve_device_request(
        &self,
        _input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: Capability::DeviceTrust,
            reason: "device approval requires a hosted control-plane implementation.",
        })
    }

    fn deny_device_request(&self, _input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        Err(ControlPlaneError::Limited {
            capability: Capability::DeviceTrust,
            reason: "device denial requires a hosted control-plane implementation.",
        })
    }

    fn revoke_device(
        &self,
        _input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: Capability::DeviceTrust,
            reason: "device revocation requires a hosted control-plane implementation.",
        })
    }

    fn get_encrypted_device_grant(
        &self,
        _request_id: &DeviceApprovalRequestId,
        _device_id: &DeviceId,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        Err(ControlPlaneError::Limited {
            capability: Capability::DeviceTrust,
            reason: "grant fetching requires a hosted control-plane implementation.",
        })
    }

    fn confirm_device_grant_accepted(
        &self,
        _input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: Capability::DeviceTrust,
            reason: "grant acceptance requires a hosted control-plane implementation.",
        })
    }
}

pub trait RecoveryControlPlaneClient {
    fn create_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: Capability::RecoveryKey,
            reason: "recovery envelopes require a hosted control-plane implementation.",
        })
    }

    fn verify_recovery_envelope(
        &self,
        _workspace_id: &WorkspaceId,
        _envelope_id: &RecoveryEnvelopeId,
        _verified_by_device_id: &DeviceId,
        _verified_by_device_proof: &str,
        _recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: Capability::RecoveryKey,
            reason: "recovery verification requires a hosted control-plane implementation.",
        })
    }

    fn rotate_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: Capability::RecoveryKey,
            reason: "recovery rotation requires a hosted control-plane implementation.",
        })
    }

    fn revoke_recovery_envelope(
        &self,
        _workspace_id: &WorkspaceId,
        _envelope_id: &RecoveryEnvelopeId,
        _revoked_by_device_id: &DeviceId,
        _revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: Capability::RecoveryKey,
            reason: "recovery revocation requires a hosted control-plane implementation.",
        })
    }

    fn list_recovery_envelopes(
        &self,
        _workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        Err(ControlPlaneError::Limited {
            capability: Capability::RecoveryKey,
            reason: "recovery listing requires a hosted control-plane implementation.",
        })
    }

    fn authorize_device_with_recovery(
        &self,
        _input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: Capability::RecoveryKey,
            reason: "recovery device authorization requires a hosted control-plane implementation.",
        })
    }
}

pub trait CapabilityReporting {
    fn capabilities(&self) -> BTreeSet<Capability>;

    fn supports_capability(&self, capability: Capability) -> bool {
        self.capabilities().contains(&capability)
    }
}

pub trait ControlPlaneClient:
    WorkspaceControlPlaneClient
    + ObjectControlPlaneClient
    + DeviceControlPlaneClient
    + RecoveryControlPlaneClient
    + CapabilityReporting
{
}

impl<T> ControlPlaneClient for T where
    T: WorkspaceControlPlaneClient
        + ObjectControlPlaneClient
        + DeviceControlPlaneClient
        + RecoveryControlPlaneClient
        + CapabilityReporting
{
}
