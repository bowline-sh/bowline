pub(super) use std::{fs, path::Path};

pub(super) use bowline_core::{
    ids::{EnvRecordId, ProjectId, SnapshotId, WorkViewId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    work_views::{
        OVERLAY_HEAD_EMPTY, WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    },
};
pub(super) use rusqlite::Connection;

pub(super) use crate::{metadata::schema::CURRENT_SCHEMA_VERSION, workspace::TempWorkspace};

pub(super) use super::{
    DatabaseState, EnvRecord, MetadataError, MetadataStore, ObservedLocalPath, ProjectUpsert,
    SetupReceiptRecord,
};

fn project_upsert(id: &str, path: &str) -> ProjectUpsert {
    ProjectUpsert {
        id: ProjectId::new(id),
        path: path.to_string(),
        git_observer_state: bowline_core::status::GitObserverState::Ok,
    }
}

mod schema;
pub(crate) mod work_views;
mod workspace;

fn is_below(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
