pub(super) use std::{fs, path::Path};

pub(super) use bowline_core::{
    ids::{ContentId, DeviceId, EnvRecordId, PackId, ProjectId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{ContentLocator, ContentStorage, HydrationState, NamespaceEntryKind},
};
pub(super) use rusqlite::Connection;

pub(super) use crate::{metadata::schema::CURRENT_SCHEMA_VERSION, workspace::TempWorkspace};

pub(super) use super::{
    CommandIdempotencyRecord, DatabaseState, EnvRecord, HydrationQueueRecord, IndexDocumentRecord,
    IndexPackRecord, IndexWorkRecord, LocalWriteLogRecord, MetadataError, MetadataStore,
    ProjectedNodeRecord, SetupReceiptRecord,
};

mod schema;
mod storage;
mod workspace;

fn is_below(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
