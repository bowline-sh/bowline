use bowline_core::ids::{ConflictId, WorkspaceId};

use super::MetadataError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SyncOperationKind {
    #[serde(rename = "daemon-reconcile")]
    Reconcile,
    #[serde(rename = "conflict-occurrence-reconcile")]
    ConflictOccurrenceReconcile,
    #[serde(rename = "work-view-overlay-sync")]
    WorkViewOverlaySync,
}

impl SyncOperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "daemon-reconcile",
            Self::ConflictOccurrenceReconcile => "conflict-occurrence-reconcile",
            Self::WorkViewOverlaySync => "work-view-overlay-sync",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncResourceKey {
    WorkspaceSync(WorkspaceId),
    ConflictFollowup {
        workspace_id: WorkspaceId,
        conflict_id: ConflictId,
    },
    PostCommit(WorkspaceId),
}

impl SyncResourceKey {
    pub fn workspace_sync(workspace_id: WorkspaceId) -> Self {
        Self::WorkspaceSync(workspace_id)
    }

    pub fn conflict_followup(workspace_id: WorkspaceId, conflict_id: ConflictId) -> Self {
        Self::ConflictFollowup {
            workspace_id,
            conflict_id,
        }
    }

    pub fn post_commit(workspace_id: WorkspaceId) -> Self {
        Self::PostCommit(workspace_id)
    }

    pub fn as_string(&self) -> String {
        match self {
            Self::WorkspaceSync(workspace_id) => {
                format!("workspace_sync:{}", workspace_id.as_str())
            }
            Self::ConflictFollowup {
                workspace_id,
                conflict_id,
            } => format!(
                "conflict_followup:{}:{}",
                workspace_id.as_str(),
                conflict_id.as_str()
            ),
            Self::PostCommit(workspace_id) => {
                format!("post_commit:{}", workspace_id.as_str())
            }
        }
    }

    pub(super) fn from_stored(
        kind: SyncOperationKind,
        workspace_id: &WorkspaceId,
        value: String,
    ) -> Result<Self, MetadataError> {
        match kind {
            SyncOperationKind::Reconcile => {
                let expected = Self::workspace_sync(workspace_id.clone());
                if value == expected.as_string() {
                    Ok(expected)
                } else {
                    Err(MetadataError::InvalidStorageMetadata(format!(
                        "invalid workspace sync resource key `{value}`"
                    )))
                }
            }
            SyncOperationKind::ConflictOccurrenceReconcile => {
                let prefix = format!("conflict_followup:{}:", workspace_id.as_str());
                let conflict_id = value
                    .strip_prefix(&prefix)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        MetadataError::InvalidStorageMetadata(format!(
                            "invalid conflict followup resource key `{value}`"
                        ))
                    })?;
                Ok(Self::conflict_followup(
                    workspace_id.clone(),
                    ConflictId::new(conflict_id.to_string()),
                ))
            }
            SyncOperationKind::WorkViewOverlaySync => {
                let expected = Self::post_commit(workspace_id.clone());
                if value == expected.as_string() {
                    Ok(expected)
                } else {
                    Err(MetadataError::InvalidStorageMetadata(format!(
                        "invalid post-commit resource key `{value}`"
                    )))
                }
            }
        }
    }
}
