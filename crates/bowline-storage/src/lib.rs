#![deny(unsafe_code)]

mod cache;
mod envelope;
mod gc;
mod manifest;
mod packfile;

mod store;

pub use cache::{CacheError, LocalContentCache, RangeHydrationRequest};
pub use envelope::{EnvelopeError, SealedEnvelope, StorageKey, seal};
pub use gc::{
    StorageGcDeleteFailure, StorageGcExecutionReport, StorageGcPlan, StorageObjectRef,
    execute_gc_plan, plan_gc,
};
pub use manifest::{
    IndexPackPointer, LocatorIndexBinding, LocatorIndexPointer, ManifestError, ManifestPointer,
    ManifestPointerKind, SealedIndexPack, SealedLocatorIndex, SealedSnapshotManifest,
    open_snapshot_manifest, remap_locator, seal_snapshot_manifest,
};
pub use packfile::{
    PackIndex, PackRecordInput, PackWriteOutput, PackfileError, open_index_pack,
    open_locator_index, parse_index, seal_index_pack, seal_locator_index, write_source_packs,
    write_source_packs_with,
};
pub use store::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, LocalByteStore, ObjectKey, ObjectKind,
    ObjectMetadata, RetentionState,
};
