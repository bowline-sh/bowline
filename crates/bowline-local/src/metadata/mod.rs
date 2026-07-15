mod paths;
mod schema;
mod sqlite;
mod store;

pub use bowline_core::workspace_graph::WorkspaceRelativePath;
pub use paths::{
    Platform, control_socket_path_for_platform, database_path_for_platform,
    default_control_socket_path, default_database_path, default_state_root,
    state_root_for_platform,
};
pub use store::{
    AgentLeaseRecord, AgentMcpTokenRecord, ClaimedSyncOperation, ClaimedWorkViewAcceptOperation,
    ConflictSnapshotRetention, CurrentNamespaceEntryRecord, CurrentNamespaceReplaceReport,
    DatabaseInspection, DatabaseState, EnvRecord, LocalMetadataPruneReport,
    LocalMetadataRetentionPolicy, LocalPathRecord, LocalSnapshotMaintenanceReport,
    LocalWriteLogRecord, MATERIALIZATION_TASK_HEARTBEAT_SECONDS,
    MATERIALIZATION_TASK_LEASE_SECONDS, MaterializationFailureKind, MaterializationPathState,
    MaterializationPathStateRecord, MaterializationPriorityClass, MaterializationReconcileReport,
    MaterializationTaskFence, MaterializationTaskFinish, MaterializationTaskId,
    MaterializationTaskRecord, MaterializationTaskState, MetadataCacheRecord, MetadataCacheState,
    MetadataError, MetadataGcBatchReport, MetadataGcCandidate, MetadataGcCheckpoint,
    MetadataGcPhase, MetadataLogicalId, MetadataObjectBindingRecord, MetadataObjectKey,
    MetadataRecordKind, MetadataRecordRef, MetadataStore, MetadataVerificationState,
    ObservedLocalPath, OwnedStagedPath, PackRecord, PostCommitSyncComponent, PreparationLeaseId,
    PreparationLeaseRecord, PreparationLeaseState, PreparationOrphanRecord, PreparationOwnerMarker,
    PreparedStagedContentRecord, ProjectLifecycleState, ProjectLocalMaterializationState,
    ProjectRecord, ProjectUpsert, ProjectionRebuildInput, ProjectionSlice, RemoteRefCursorRecord,
    SetupReceiptRecord, SnapshotPinId, SnapshotPinOwner, SnapshotPinOwnerKind, SnapshotPinReason,
    SnapshotPinReconcileReport, SnapshotPinRecord, SnapshotRecord, SnapshotRootCompleteness,
    SnapshotRootKind, SnapshotRootRecord, SnapshotRootReference, SourceFingerprint,
    StoredContentLocator, SyncCancellationOutcome, SyncClaimCheck, SyncClaimHandle, SyncClaimToken,
    SyncClaimTransition, SyncCommittedCancelledLateResult, SyncOperationCheckpointRecord,
    SyncOperationCounts, SyncOperationEnqueueOutcome, SyncOperationKind, SyncOperationRecord,
    SyncOperationState, SyncResourceKey, WORK_VIEW_BASE_DESCRIPTOR_VERSION,
    WorkViewAcceptCancellationOutcome, WorkViewAcceptCandidateObservation,
    WorkViewAcceptCheckpointRecord, WorkViewAcceptCheckpointStep, WorkViewAcceptClaimCheck,
    WorkViewAcceptClaimHandle, WorkViewAcceptClaimTransition, WorkViewAcceptEnqueueOutcome,
    WorkViewAcceptFailureReason, WorkViewAcceptOperationRecord, WorkViewAcceptOperationState,
    WorkViewAcceptResourceKey, WorkViewAcceptReviewReason, WorkViewBaseDescriptor,
    WorkViewBaseState, WorkViewRecord, WorkspaceRecord, WorkspaceSyncHeadRecord,
    all_accepted_roots,
};

pub const DEFAULT_DATABASE_FILE: &str = "local.sqlite3";
pub const DEFAULT_CONTROL_SOCKET_FILE: &str = "bowline-daemon.sock";
