mod paths;
mod schema;
mod store;

pub use paths::{Platform, database_path_for_platform, default_database_path};
pub use store::{
    AgentLeaseRecord, CommandIdempotencyRecord, DatabaseInspection, DatabaseState, EnvRecord,
    HydrationQueueRecord, IndexDocumentRecord, IndexPackRecord, IndexWorkRecord, LocalPathRecord,
    LocalWriteLogRecord, MetadataError, MetadataStore, ObservedLocalPath, PackRecord,
    ProjectRecord, ProjectedNodeRecord, RemoteRefCursorRecord, SetupReceiptRecord,
    StoredContentLocator, SymbolIndexRecord, SyncOperationCheckpointRecord, SyncOperationCounts,
    SyncOperationRecord, WorkspaceRecord, WorkspaceSyncHeadRecord,
};

pub const DEFAULT_DATABASE_FILE: &str = "local.sqlite3";
