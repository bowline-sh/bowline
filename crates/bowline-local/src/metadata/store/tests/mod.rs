pub(super) use std::{fs, path::Path};

pub(super) use bowline_core::{
    ids::{
        ContentId, DeviceId, EnvRecordId, PackId, ProjectId, SnapshotId, WorkViewId, WorkspaceId,
    },
    policy::{AccessFlag, MaterializationMode, PathClassification},
    work_views::{
        OVERLAY_HEAD_EMPTY, WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    },
    workspace_graph::{
        ContentLocator, ContentStorage, FileExecutability, HydrationState, NamespaceEntry,
        NamespaceEntryKind,
    },
};
pub(super) use rusqlite::Connection;

pub(super) use crate::{metadata::schema::CURRENT_SCHEMA_VERSION, workspace::TempWorkspace};

pub(super) use super::{
    DatabaseState, EnvRecord, LocalWriteLogRecord, MetadataError, MetadataStore, ObservedLocalPath,
    ProjectUpsert, SetupReceiptRecord, SyncClaimTransition, SyncOperationCheckpointRecord,
    SyncOperationKind, SyncOperationState, WORK_VIEW_BASE_DESCRIPTOR_VERSION,
    WorkViewAcceptCheckpointRecord, WorkViewAcceptCheckpointStep, WorkViewAcceptClaimCheck,
    WorkViewAcceptClaimTransition, WorkViewAcceptEnqueueOutcome, WorkViewAcceptFailureReason,
    WorkViewAcceptOperationRecord, WorkViewAcceptOperationState, WorkViewAcceptResourceKey,
    WorkViewAcceptReviewReason, WorkViewBaseDescriptor, WorkViewBaseState,
};

fn project_upsert(id: &str, path: &str) -> ProjectUpsert {
    ProjectUpsert {
        id: ProjectId::new(id),
        path: path.to_string(),
        git_observer_state: bowline_core::status::GitObserverState::Ok,
    }
}

mod materialization;
mod page_authority;
mod preparation;
mod projection;
mod schema;
mod storage;
mod work_view_accept_operations;
mod work_views;
mod workspace;

fn is_below(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
