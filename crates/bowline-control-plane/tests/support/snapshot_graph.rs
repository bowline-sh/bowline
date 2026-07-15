use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

use bowline_control_plane::{
    ControlPlaneError, ControlPlaneTimestamp, FakeControlPlaneClient, MetadataBindingCommit,
    MetadataBindingInput, MetadataRecordKind, MetadataSidecar, ObjectControlPlaneClient,
    ObjectKind, ObjectMetadataCommit, ObjectPointer, SnapshotRootCommit, UploadIntentRequest,
};
use bowline_core::ids::{ContentId, DeviceId, ManifestId, SnapshotId, WorkspaceId};
use sha2::{Digest, Sha256};

type SnapshotDirectObjects = BTreeMap<(WorkspaceId, SnapshotId), Vec<ObjectPointer>>;

static SNAPSHOT_DIRECT_OBJECTS: OnceLock<Mutex<SnapshotDirectObjects>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SnapshotGraphCommit {
    pub(super) workspace_id: WorkspaceId,
    pub(super) snapshot_id: SnapshotId,
    pub(super) manifest_id: ManifestId,
    pub(super) manifest_object: ObjectPointer,
    pub(super) direct_objects: Vec<ObjectPointer>,
    pub(super) committed_by_device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SnapshotGraphRecord {
    pub(super) workspace_id: WorkspaceId,
    pub(super) snapshot_id: SnapshotId,
    pub(super) manifest_id: ManifestId,
    pub(super) manifest_object: ObjectPointer,
    pub(super) direct_objects: Vec<ObjectPointer>,
    pub(super) committed_by_device_id: DeviceId,
    pub(super) committed_at: ControlPlaneTimestamp,
}

pub(super) trait SnapshotGraphTestApi {
    fn commit_snapshot_graph(
        &self,
        commit: SnapshotGraphCommit,
    ) -> Result<SnapshotGraphRecord, ControlPlaneError>;

    fn get_snapshot_graph(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<Option<SnapshotGraphRecord>, ControlPlaneError>;
}

impl SnapshotGraphTestApi for FakeControlPlaneClient {
    fn commit_snapshot_graph(
        &self,
        commit: SnapshotGraphCommit,
    ) -> Result<SnapshotGraphRecord, ControlPlaneError> {
        for pointer in &commit.direct_objects {
            if let Ok(metadata) =
                self.head_object_metadata(&commit.workspace_id, &pointer.object_key)
            {
                if ObjectKind::try_from(metadata.kind).ok() != Some(pointer.kind)
                    || metadata.byte_len != pointer.byte_len
                    || metadata.hash != pointer.hash
                    || metadata.key_epoch != pointer.key_epoch
                {
                    return Err(ControlPlaneError::Conflict {
                        resource: "snapshot metadata graph",
                        reason: "existing object metadata must match exactly",
                    });
                }
            } else {
                self.commit_uploaded_object_metadata(ObjectMetadataCommit {
                    workspace_id: commit.workspace_id.clone(),
                    object: pointer.clone(),
                    committed_by_device_id: commit.committed_by_device_id.clone(),
                })?;
            }
        }
        let seed = Sha256::digest(commit.snapshot_id.as_str().as_bytes());
        let suffix = seed
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let logical_id = format!("nsp_{suffix}");
        let object_key = format!("metadata_mp_{suffix}");
        let metadata_pointer = ObjectPointer {
            object_key: object_key.clone(),
            content_id: ContentId::new(logical_id.clone()),
            byte_len: 1,
            hash: format!("b3_{suffix}"),
            key_epoch: commit.manifest_object.key_epoch,
            kind: ObjectKind::SnapshotMetadataPage,
            created_at: commit.manifest_object.created_at,
        };
        if self
            .head_object_metadata(&commit.workspace_id, &object_key)
            .is_err()
        {
            self.create_upload_intent(
                UploadIntentRequest::new(
                    commit.workspace_id.as_str(),
                    ObjectKind::SnapshotMetadataPage,
                    1,
                )
                .with_content_id(logical_id.clone())
                .with_object_key(object_key),
            )?;
        }
        self.commit_metadata_bindings(MetadataBindingCommit {
            workspace_id: commit.workspace_id.clone(),
            bindings: vec![MetadataBindingInput {
                logical_id: logical_id.clone(),
                record_kind: MetadataRecordKind::NamespacePage,
                object: metadata_pointer,
                sidecar: MetadataSidecar {
                    child_logical_ids: Vec::new(),
                    direct_object_keys: {
                        let mut keys = commit
                            .direct_objects
                            .iter()
                            .map(|pointer| pointer.object_key.clone())
                            .collect::<Vec<_>>();
                        keys.sort();
                        keys.dedup();
                        keys
                    },
                    digest: format!("b3_{suffix}"),
                },
            }],
            committed_by_device_id: commit.committed_by_device_id.clone(),
        })?;
        let root = self.commit_snapshot_root(SnapshotRootCommit {
            workspace_id: commit.workspace_id.clone(),
            snapshot_id: commit.snapshot_id.clone(),
            manifest_id: commit.manifest_id.clone(),
            manifest_object: commit.manifest_object.clone(),
            namespace_root_id: logical_id,
            extra_root_logical_ids: Vec::new(),
            committed_by_device_id: commit.committed_by_device_id.clone(),
        })?;
        SNAPSHOT_DIRECT_OBJECTS
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .expect("snapshot direct-object map poisoned")
            .insert(
                (commit.workspace_id.clone(), commit.snapshot_id.clone()),
                commit.direct_objects.clone(),
            );
        Ok(SnapshotGraphRecord {
            workspace_id: root.workspace_id,
            snapshot_id: root.snapshot_id,
            manifest_id: root.manifest_id,
            manifest_object: root.manifest_object,
            direct_objects: commit.direct_objects,
            committed_by_device_id: root.committed_by_device_id,
            committed_at: root.committed_at,
        })
    }

    fn get_snapshot_graph(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<Option<SnapshotGraphRecord>, ControlPlaneError> {
        let direct_objects = SNAPSHOT_DIRECT_OBJECTS
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .expect("snapshot direct-object map poisoned")
            .get(&(workspace_id.clone(), snapshot_id.clone()))
            .cloned()
            .unwrap_or_default();
        Ok(self
            .get_snapshot_root(workspace_id, snapshot_id)?
            .map(|root| SnapshotGraphRecord {
                workspace_id: root.workspace_id,
                snapshot_id: root.snapshot_id,
                manifest_id: root.manifest_id,
                manifest_object: root.manifest_object,
                direct_objects,
                committed_by_device_id: root.committed_by_device_id,
                committed_at: root.committed_at,
            }))
    }
}
