#![deny(unsafe_code)]

mod cache;
mod envelope;
mod gc;
mod hydration;
mod manifest;
mod metadata_page;
mod packfile;

mod store;

pub use cache::{
    CacheError, CachedPackIoObserver, CachedPackReadMetrics, CachedPackReader,
    CachedPackReleaseState, ContentVerification, LocalContentCache, RangeHydrationRequest,
    RecordRangeProofRequest, VerifiedContentReader, verify_record_range,
};
pub use envelope::{
    EnvelopeContext, EnvelopeError, SealedEnvelope, StorageKey, open, seal, workspace_id_hash,
};
pub use gc::{
    StorageGcDeleteFailure, StorageGcExecutionReport, StorageGcPlan, StorageObjectRef,
    execute_gc_plan, plan_gc,
};
pub use hydration::{
    CoalescedRange, HydrationPlanError, HydrationRecord, PackHydrationPlan, PackHydrationSource,
    PlannedRecord, plan_pack_hydration,
};
pub use manifest::{
    LocatorIndexBinding, LocatorIndexPointer, ManifestError, ManifestPointer, ManifestPointerKind,
    SNAPSHOT_ROOT_FORMAT_VERSION, SNAPSHOT_ROOT_MAX_PLAINTEXT_BYTES,
    SNAPSHOT_ROOT_MAX_SEALED_BYTES, SealedLocatorIndex, SealedSnapshotManifest,
    open_snapshot_manifest, seal_snapshot_manifest,
};
pub use metadata_page::{
    MetadataPageError, SNAPSHOT_METADATA_PAGE_FORMAT_VERSION,
    SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES, SNAPSHOT_METADATA_PAGE_MAX_SEALED_BYTES,
    SealedSnapshotMetadataPage, SnapshotMetadataPagePointer, SnapshotMetadataRecordId,
    SnapshotMetadataRecordKind, open_snapshot_metadata_page, seal_snapshot_metadata_page,
};
pub use packfile::{
    ContentSourceReader, PackIndex, PackRecordInput, PackRecordReader, PackRecordRef,
    PackStreamWriteOutput, PackWriteOutput, PackWriter, PackfileError, open_locator_index,
    parse_index, seal_locator_index, write_source_pack_batches_with,
    write_source_pack_reader_batches_with, write_source_pack_ref_batches_with,
    write_source_pack_refs, write_source_packs, write_source_packs_with,
};
pub use store::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, IntentFailureKind, LocalByteStore,
    ObjectContentId, ObjectHash, ObjectKey, ObjectKind, ObjectMetadata, PutObjectReaderRequest,
    ReopenableObjectSource, RetentionState, SourcePackUploadJournalDigest,
    SourcePackUploadJournalEntry, SourcePackUploadJournalKey, SourcePackUploadJournalObjectHash,
    SourcePackUploadJournalPointer, TransferOperation, stable_object_hash,
};
