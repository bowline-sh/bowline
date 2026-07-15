mod builder;
mod codec;
mod layout;
mod layout_validation;
mod metadata;
mod reader;
mod snapshot;
mod tree;
mod types;

pub use builder::PageNamespaceBuilder;
pub use layout::{ContentLayoutRecord, PackLengthResolver, SegmentSequence};
pub use reader::{NamespaceEntryDescriptor, PageNamespaceReader};
pub use snapshot::{BuiltPagedNamespaceSnapshot, ChangedPageSummary};
pub(crate) use types::PagedRecordSource;
pub use types::{
    CONTENT_LAYOUT_FORMAT_VERSION, INLINE_SEGMENT_MAX_BYTES, MAX_SEGMENTS_PER_LAYOUT,
    MetadataIdentityKey, MetadataPlaintextRecord, MetadataRecordKind, MetadataRecordSummary,
    NAMESPACE_PAGE_FORMAT_VERSION, NAMESPACE_PAGE_MAX_BYTES, NAMESPACE_PAGE_MIN_BYTES,
    NAMESPACE_PAGE_TARGET_BYTES, PageStore, SEGMENT_PAGE_FORMAT_VERSION, SEGMENT_PAGE_MAX_BYTES,
};

pub fn validate_namespace_page_encoding(
    bytes: &[u8],
) -> Result<(), bowline_core::namespace_snapshot::NamespaceReadError> {
    codec::decode_namespace_page(bytes).map(|_| ())
}

pub fn validate_content_layout_encoding(
    bytes: &[u8],
) -> Result<(), bowline_core::namespace_snapshot::NamespaceReadError> {
    layout::decode_content_layout(bytes).map(|_| ())
}

pub fn validate_segment_page_encoding(
    bytes: &[u8],
) -> Result<(), bowline_core::namespace_snapshot::NamespaceReadError> {
    layout::decode_segment_page(bytes).map(|_| ())
}

#[cfg(test)]
mod tests;
