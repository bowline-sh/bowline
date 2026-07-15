use std::fmt;

use bowline_control_plane::{
    ControlPlaneError, FakeControlPlaneClient, ObjectControlPlaneClient as _,
    ObjectKind as ControlObjectKind, RejectionCode, UploadIntentRequest,
};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_storage::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, IntentFailureKind, LocalByteStore,
    ObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, TransferOperation,
    stable_object_hash,
};

pub struct FakeHostedByteStore<'a> {
    control_plane: &'a FakeControlPlaneClient,
    inner: &'a LocalByteStore,
    workspace_id: &'a WorkspaceId,
}

impl<'a> FakeHostedByteStore<'a> {
    pub fn new(
        control_plane: &'a FakeControlPlaneClient,
        inner: &'a LocalByteStore,
        workspace_id: &'a WorkspaceId,
    ) -> Self {
        Self {
            control_plane,
            inner,
            workspace_id,
        }
    }
}

impl fmt::Debug for FakeHostedByteStore<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FakeHostedByteStore")
            .field("workspace_id", &self.workspace_id)
            .finish_non_exhaustive()
    }
}

impl ByteStore for FakeHostedByteStore<'_> {
    fn put_object(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.put_object_with_content_id(
            key,
            kind,
            &stable_object_hash(bytes),
            bytes,
            created_by_device_id,
        )
    }

    fn put_object_with_content_id(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        content_id: &str,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.put_object_with_content_id_at_epoch(
            key,
            kind,
            content_id,
            bytes,
            1,
            created_by_device_id,
        )
    }

    fn put_object_with_content_id_at_epoch(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        content_id: &str,
        bytes: &[u8],
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.as_str(),
                    ControlObjectKind::try_from(kind)?,
                    bytes.len() as u64,
                )
                .with_content_id(content_id)
                .with_object_key(key.as_str()),
            )
            .map_err(fake_hosted_upload_intent_error)?;
        self.inner.put_object_with_content_id_at_epoch(
            key,
            kind,
            content_id,
            bytes,
            key_epoch,
            created_by_device_id,
        )
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        self.ensure_online(TransferOperation::Download)?;
        self.inner.get_object(key)
    }

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        self.ensure_online(TransferOperation::Download)?;
        self.inner.get_range(key, range)
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        self.ensure_online(TransferOperation::Download)?;
        self.inner.head_object(key)
    }

    fn creates_upload_intents(&self) -> bool {
        true
    }

    fn metrics(&self) -> ByteStoreMetrics {
        self.inner.metrics()
    }
}

impl FakeHostedByteStore<'_> {
    fn ensure_online(&self, operation: TransferOperation) -> Result<(), ByteStoreError> {
        if self.control_plane.is_offline() {
            return Err(ByteStoreError::IntentFailed {
                operation,
                kind: IntentFailureKind::Transport,
                detail: FakeControlPlaneClient::offline_transport_error().to_string(),
            });
        }
        Ok(())
    }
}

fn fake_hosted_upload_intent_error(error: ControlPlaneError) -> ByteStoreError {
    let kind = match &error {
        ControlPlaneError::Timeout { .. } => IntentFailureKind::Timeout,
        ControlPlaneError::Transport { .. } => IntentFailureKind::Transport,
        ControlPlaneError::Rejected {
            code:
                RejectionCode::DeviceNotTrusted
                | RejectionCode::Unauthorized
                | RejectionCode::WorkspaceMembershipRequired
                | RejectionCode::WorkspaceOwnerRequired,
            ..
        } => IntentFailureKind::DeviceNotTrusted,
        ControlPlaneError::Rejected {
            code: RejectionCode::InvalidRequest | RejectionCode::Unknown,
            ..
        }
        | ControlPlaneError::WorkspaceMissing { .. }
        | ControlPlaneError::WorkViewMissing { .. }
        | ControlPlaneError::LeaseMissing { .. }
        | ControlPlaneError::CompareAndSwap(_)
        | ControlPlaneError::InvalidObjectKey { .. }
        | ControlPlaneError::ObjectMissing { .. }
        | ControlPlaneError::DeviceRequestMissing { .. }
        | ControlPlaneError::Limited { .. }
        | ControlPlaneError::Unsupported { .. }
        | ControlPlaneError::Conflict { .. }
        | ControlPlaneError::Storage(_) => IntentFailureKind::Other,
    };
    ByteStoreError::IntentFailed {
        operation: TransferOperation::Upload,
        kind,
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_control_plane::WorkspaceControlPlaneClient;
    use bowline_core::ids::WorkspaceId;
    use bowline_local::workspace::TempWorkspace;

    #[test]
    fn default_put_object_records_stable_content_id_in_upload_intent() {
        let control_plane = FakeControlPlaneClient::default();
        let workspace_id = WorkspaceId::new("ws_fake_hosted_store");
        let object_root = TempWorkspace::new("fake-hosted-store-objects").expect("object root");
        let byte_store =
            LocalByteStore::open_deterministic(object_root.root(), 1).expect("byte store");
        let hosted = FakeHostedByteStore::new(&control_plane, &byte_store, &workspace_id);
        let object_key =
            ObjectKey::new("packs_pk_0000000000000001".to_string()).expect("object key");
        let bytes = b"hosted bytes";

        control_plane
            .create_workspace_ref(&workspace_id)
            .expect("workspace ref");
        hosted
            .put_object(
                object_key,
                StorageObjectKind::SourcePack,
                bytes,
                Some(&DeviceId::new("device_test".to_string())),
            )
            .expect("put object");

        let requests = control_plane.upload_intent_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].content_id.as_deref(),
            Some(stable_object_hash(bytes).as_str())
        );
        assert_eq!(requests[0].workspace_id, workspace_id.as_str());
    }

    #[test]
    fn offline_fake_hosted_store_reports_transport_for_reads() {
        let control_plane = FakeControlPlaneClient::default();
        let workspace_id = WorkspaceId::new("ws_fake_hosted_store_offline");
        let object_root =
            TempWorkspace::new("fake-hosted-store-offline-objects").expect("object root");
        let byte_store =
            LocalByteStore::open_deterministic(object_root.root(), 1).expect("byte store");
        let hosted = FakeHostedByteStore::new(&control_plane, &byte_store, &workspace_id);
        let object_key =
            ObjectKey::new("packs_pk_0000000000000002".to_string()).expect("object key");

        control_plane
            .create_workspace_ref(&workspace_id)
            .expect("workspace ref");
        control_plane.set_offline(true);
        let error = hosted
            .get_object(&object_key)
            .expect_err("offline hosted read fails");

        assert!(matches!(
            error,
            ByteStoreError::IntentFailed {
                operation: TransferOperation::Download,
                kind: IntentFailureKind::Transport,
                ..
            }
        ));
    }
}
