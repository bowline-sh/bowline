mod identity;
mod paged;
mod path;

use bowline_core::namespace_snapshot::NamespaceOperationBudget;

const MAX_OPERATION_NAMESPACE_PAGES: u64 = 1_000_000;
const MAX_OPERATION_LAYOUT_RECORDS: u64 = 1_000_000;
const MAX_OPERATION_SEGMENT_PAGES: u64 = 1_000_000;
const MAX_OPERATION_METADATA_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub use identity::{
    SemanticManifestDigest, SemanticManifestIdentity, semantic_manifest_identity_with_context,
};
pub(crate) use identity::{semantic_manifest_identity, semantic_manifest_identity_from_reader};
pub(crate) use paged::PagedRecordSource;
pub use paged::{
    BuiltPagedNamespaceSnapshot, CONTENT_LAYOUT_FORMAT_VERSION, ChangedPageSummary,
    ContentLayoutRecord, INLINE_SEGMENT_MAX_BYTES, MAX_SEGMENTS_PER_LAYOUT, MetadataIdentityKey,
    MetadataPlaintextRecord, MetadataRecordKind, MetadataRecordSummary,
    NAMESPACE_PAGE_FORMAT_VERSION, NAMESPACE_PAGE_MAX_BYTES, NAMESPACE_PAGE_MIN_BYTES,
    NAMESPACE_PAGE_TARGET_BYTES, NamespaceEntryDescriptor, PackLengthResolver,
    PageNamespaceBuilder, PageNamespaceReader, PageStore, SEGMENT_PAGE_FORMAT_VERSION,
    SEGMENT_PAGE_MAX_BYTES, SegmentSequence, validate_content_layout_encoding,
    validate_namespace_page_encoding, validate_segment_page_encoding,
};
pub(crate) use path::validated_path;

pub(crate) const fn operation_budget(
    entries: u64,
    diff_entries: u64,
    mutations: u64,
) -> NamespaceOperationBudget {
    NamespaceOperationBudget::new(entries, diff_entries, mutations).with_metadata_limits(
        MAX_OPERATION_NAMESPACE_PAGES,
        MAX_OPERATION_LAYOUT_RECORDS,
        MAX_OPERATION_SEGMENT_PAGES,
        MAX_OPERATION_METADATA_BYTES,
    )
}

pub(crate) fn lazy_namespace_read_limits(entry_count: u64) -> (u64, u64) {
    // A compressed radix tree has at most one leaf and one branch per entry,
    // plus the canonical empty root. Resident cache counts are not graph bounds.
    let pages = entry_count
        .saturating_mul(2)
        .saturating_add(1)
        .min(MAX_OPERATION_NAMESPACE_PAGES);
    let bytes = pages
        .saturating_mul(NAMESPACE_PAGE_MAX_BYTES as u64)
        .min(MAX_OPERATION_METADATA_BYTES);
    (pages, bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_OPERATION_METADATA_BYTES, MAX_OPERATION_NAMESPACE_PAGES, NAMESPACE_PAGE_MAX_BYTES,
        lazy_namespace_read_limits,
    };

    #[test]
    fn lazy_read_limits_derive_from_graph_cardinality_not_residency() {
        assert_eq!(
            lazy_namespace_read_limits(0),
            (1, NAMESPACE_PAGE_MAX_BYTES as u64)
        );
        assert_eq!(
            lazy_namespace_read_limits(111),
            (223, 223 * NAMESPACE_PAGE_MAX_BYTES as u64)
        );
        assert_eq!(
            lazy_namespace_read_limits(u64::MAX),
            (MAX_OPERATION_NAMESPACE_PAGES, MAX_OPERATION_METADATA_BYTES)
        );
    }
}
