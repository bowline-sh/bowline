use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    error::Error,
    fmt,
    path::Path,
};

use bowline_control_plane::{
    ControlPlaneError, FakeControlPlaneClient, ObjectControlPlaneClient,
    WorkspaceControlPlaneClient,
};
use bowline_core::{hosted::EMPTY_SNAPSHOT_ID, ids::WorkspaceId};
use bowline_local::metadata::{DEFAULT_DATABASE_FILE, MetadataError, MetadataStore};
use bowline_storage::{ByteStore, ByteStoreError, ObjectKey, RetentionState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedEvidence {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedStatus {
    pub text: String,
}

pub fn assert_object_before_ref(
    control_plane: &FakeControlPlaneClient,
    byte_store: &dyn ByteStore,
    workspace_id: &WorkspaceId,
) -> Result<(), InvariantError> {
    let Some(workspace_ref) = control_plane.get_workspace_ref(workspace_id)? else {
        return Ok(());
    };
    if workspace_ref.snapshot_id == EMPTY_SNAPSHOT_ID {
        return Ok(());
    }
    let Some(root) = control_plane.get_snapshot_root(workspace_id, &workspace_ref.snapshot_id)?
    else {
        return Err(InvariantError::MissingObjectManifest {
            workspace_ref_version: workspace_ref.version,
            snapshot_id: workspace_ref.snapshot_id.to_string(),
        });
    };
    assert_pointer_available(
        control_plane,
        byte_store,
        workspace_id,
        &root.manifest_object.object_key,
    )?;
    if !root.complete {
        return Err(InvariantError::IncompleteSnapshotGraph {
            snapshot_id: workspace_ref.snapshot_id.to_string(),
        });
    }
    let mut queue = VecDeque::from([root.namespace_root_id]);
    let mut seen = BTreeSet::new();
    while !queue.is_empty() {
        let ids = (0..16)
            .filter_map(|_| queue.pop_front())
            .collect::<Vec<_>>();
        let bindings = control_plane.resolve_metadata_bindings(workspace_id, &ids)?;
        let by_id = bindings
            .bindings
            .into_iter()
            .map(|binding| (binding.logical_id.clone(), binding))
            .collect::<BTreeMap<_, _>>();
        for id in ids {
            let Some(binding) = by_id.get(&id) else {
                return Err(InvariantError::IncompleteSnapshotGraph {
                    snapshot_id: workspace_ref.snapshot_id.to_string(),
                });
            };
            if !seen.insert(id) {
                continue;
            }
            assert_pointer_available(
                control_plane,
                byte_store,
                workspace_id,
                &binding.object.object_key,
            )?;
            for object_key in &binding.sidecar.direct_object_keys {
                assert_pointer_available(control_plane, byte_store, workspace_id, object_key)?;
            }
            queue.extend(binding.sidecar.child_logical_ids.iter().cloned());
        }
    }
    Ok(())
}

pub fn assert_local_head_supported(
    state_root: &Path,
    workspace_id: &WorkspaceId,
) -> Result<(), InvariantError> {
    let database_path = state_root.join(DEFAULT_DATABASE_FILE);
    if !database_path.exists() {
        return Ok(());
    }
    let store = MetadataStore::open(database_path)?;
    let Some(local_head) = store.workspace_sync_head(workspace_id)? else {
        return Ok(());
    };
    if local_head.workspace_ref.snapshot_id == EMPTY_SNAPSHOT_ID {
        return Ok(());
    }
    let snapshot_id =
        bowline_core::ids::SnapshotId::new(local_head.workspace_ref.snapshot_id.clone());
    if store.snapshot(workspace_id, &snapshot_id)?.is_none() {
        return Err(InvariantError::MissingLocalHeadSnapshot {
            snapshot_id: local_head.workspace_ref.snapshot_id.to_string(),
        });
    }
    Ok(())
}

pub fn assert_status_not_hiding_degraded(
    evidence: &[DegradedEvidence],
    rendered: &RenderedStatus,
) -> Result<(), InvariantError> {
    for item in evidence {
        if !rendered.text.contains(&item.code) && !rendered.text.contains(&item.message) {
            return Err(InvariantError::HiddenDegradedEvidence {
                code: item.code.clone(),
            });
        }
    }
    Ok(())
}

fn assert_pointer_available(
    control_plane: &FakeControlPlaneClient,
    byte_store: &dyn ByteStore,
    workspace_id: &WorkspaceId,
    object_key: &str,
) -> Result<(), InvariantError> {
    let key = ObjectKey::new(object_key.to_string()).map_err(|_| {
        InvariantError::InvalidCommittedObjectKey {
            object_key: object_key.to_string(),
        }
    })?;
    let metadata = control_plane.head_object_metadata(workspace_id, object_key)?;
    if metadata.retention_state != RetentionState::Current {
        return Err(InvariantError::UnavailableCommittedObject {
            object_key: object_key.to_string(),
            retention_state: metadata.retention_state,
        });
    }
    byte_store.head_object(&key)?;
    Ok(())
}

#[derive(Debug)]
pub enum InvariantError {
    ControlPlane(ControlPlaneError),
    ByteStore(ByteStoreError),
    Metadata(MetadataError),
    MissingObjectManifest {
        workspace_ref_version: u64,
        snapshot_id: String,
    },
    MissingLocalHeadSnapshot {
        snapshot_id: String,
    },
    IncompleteSnapshotGraph {
        snapshot_id: String,
    },
    HiddenDegradedEvidence {
        code: String,
    },
    UnavailableCommittedObject {
        object_key: String,
        retention_state: RetentionState,
    },
    InvalidCommittedObjectKey {
        object_key: String,
    },
}

impl fmt::Display for InvariantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::MissingObjectManifest {
                workspace_ref_version,
                snapshot_id,
            } => write!(
                formatter,
                "workspace ref {workspace_ref_version} points at snapshot {snapshot_id} without a committed object manifest"
            ),
            Self::MissingLocalHeadSnapshot { snapshot_id } => {
                write!(
                    formatter,
                    "local head snapshot {snapshot_id} is not present in metadata"
                )
            }
            Self::IncompleteSnapshotGraph { snapshot_id } => {
                write!(
                    formatter,
                    "snapshot {snapshot_id} has an incomplete metadata graph"
                )
            }
            Self::HiddenDegradedEvidence { code } => {
                write!(formatter, "rendered status hides degraded evidence {code}")
            }
            Self::UnavailableCommittedObject {
                object_key,
                retention_state,
            } => write!(
                formatter,
                "committed object {object_key} is not current in control-plane metadata: {retention_state:?}"
            ),
            Self::InvalidCommittedObjectKey { object_key } => {
                write!(formatter, "committed object key {object_key} is invalid")
            }
        }
    }
}

impl Error for InvariantError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::MissingObjectManifest { .. }
            | Self::MissingLocalHeadSnapshot { .. }
            | Self::IncompleteSnapshotGraph { .. }
            | Self::HiddenDegradedEvidence { .. }
            | Self::UnavailableCommittedObject { .. }
            | Self::InvalidCommittedObjectKey { .. } => None,
        }
    }
}

impl From<ControlPlaneError> for InvariantError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<ByteStoreError> for InvariantError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<MetadataError> for InvariantError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}
