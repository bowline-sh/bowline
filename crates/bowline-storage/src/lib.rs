#![deny(unsafe_code)]

mod canonical_framing;
mod envelope;
mod gc;
mod recovery_preimage;

mod store;

pub use envelope::{
    EnvelopeContext, EnvelopeError, SealedEnvelope, SegmentedOpenStats, SegmentedSealStats,
    StorageKey, is_segmented_envelope, open, open_bounded, open_segmented, seal, seal_segmented,
    workspace_id_hash,
};
pub use gc::{
    StorageGcDeleteFailure, StorageGcExecutionReport, StorageGcFailureKind, StorageGcPlan,
    StorageObjectRef, execute_gc_plan, plan_gc,
};
pub use recovery_preimage::{
    LocalRecoveryEpochIdentity, LocalRecoveryExpectedPreimageIdentity, LocalRecoveryKeyEpoch,
    LocalRecoveryPlaintextLocator, LocalRecoveryPreimageContext, LocalRecoveryPreimageError,
    LocalRecoveryPreimageLocator, LocalRecoveryWorkspacePath, OpenLocalRecoveryPreimageRequest,
    SealLocalRecoveryPreimageRequest, SealedLocalRecoveryPreimage, open_local_recovery_preimage,
    prepare_local_recovery_file, seal_local_recovery_preimage,
};
pub use store::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, IntentFailureKind, LocalByteStore,
    ObjectContentId, ObjectHash, ObjectKey, ObjectKind, ObjectMetadata, PutObjectRequest,
    PutObjectSource, ReopenableObjectSource, RetentionState, TransferOperation, stable_object_hash,
    stable_object_hash_reader,
};
