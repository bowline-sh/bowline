use std::collections::BTreeSet;

use bowline_core::{
    ids::{ContentId, ContentLayoutId, PackId, SegmentPageId},
    namespace_snapshot::{NamespaceOperationContext, NamespaceReadError},
    workspace_graph::{ContentLayout, SegmentId, SegmentLocator},
};

use super::{
    codec::{Decoder, Encoder, logical_id},
    layout_validation::{
        layout_root_summary, segment_summary, validate_content_layout_record,
        validate_expected_summary, validate_locator_storage_ranges, validate_segment_page_shape,
        validate_segment_values, validate_segments, validate_supported_segment_count_u64,
    },
    types::{
        CONTENT_LAYOUT_FORMAT_VERSION, INLINE_SEGMENT_MAX_BYTES, MetadataRecordKind, PageStore,
        PagedRecordSource, SEGMENT_PAGE_FORMAT_VERSION, SEGMENT_PAGE_MAX_BYTES,
        SEGMENT_PAGE_TARGET_BYTES,
    },
};

#[cfg(test)]
pub(crate) use super::layout_validation::validate_supported_segment_count;

const CONTENT_LAYOUT_MAGIC: &[u8; 4] = b"BWCL";
const SEGMENT_PAGE_MAGIC: &[u8; 4] = b"BWSP";
const SEGMENT_INDEX_FANOUT: usize = 128;

#[derive(Clone, Copy)]
struct SegmentSource<'a> {
    workspace_id: &'a str,
    store: &'a dyn PagedRecordSource,
}

#[derive(Clone, Copy)]
struct LogicalRange {
    start: u64,
    end: u64,
}

#[derive(Clone, Copy)]
struct SegmentRangeRead<'a> {
    source: SegmentSource<'a>,
    range: LogicalRange,
    total_segments: u64,
    segment_size: u64,
}

pub trait PackLengthResolver: Send + Sync {
    fn pack_length(&self, pack_id: &PackId) -> Result<Option<u64>, NamespaceReadError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentLayoutRecord {
    pub format_version: u16,
    pub logical_content_id: ContentId,
    pub logical_length: u64,
    pub segment_size: u64,
    pub segments: SegmentSequence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentSequence {
    Inline(Vec<SegmentLocator>),
    Paged { root: SegmentPageId, count: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentChildSummary {
    pub first_ordinal: u32,
    pub segment_count: u32,
    pub logical_start: u64,
    pub logical_length: u64,
    pub page_id: SegmentPageId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SegmentPage {
    Leaf {
        first_ordinal: u32,
        logical_start: u64,
        segments: Vec<SegmentLocator>,
    },
    Index {
        first_ordinal: u32,
        segment_count: u32,
        logical_start: u64,
        logical_length: u64,
        children: Vec<SegmentChildSummary>,
    },
}

pub(crate) fn layout_pack_ids(record: &ContentLayoutRecord) -> Vec<String> {
    match &record.segments {
        SegmentSequence::Inline(segments) => segments
            .iter()
            .map(|segment| segment.pack_id.as_str().to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        SegmentSequence::Paged { .. } => Vec::new(),
    }
}

pub(crate) fn segment_page_pack_ids(page: &SegmentPage) -> Vec<String> {
    match page {
        SegmentPage::Leaf { segments, .. } => segments
            .iter()
            .map(|segment| segment.pack_id.as_str().to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        SegmentPage::Index { .. } => Vec::new(),
    }
}

pub(crate) fn store_content_layout(
    workspace_id: &str,
    layout: &ContentLayout,
    store: &mut PageStore,
    pack_lengths: Option<&dyn PackLengthResolver>,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<ContentLayoutId, NamespaceReadError> {
    let ContentLayout::SegmentedV1 {
        logical_content_id,
        logical_length,
        segment_size,
        segments,
    } = layout;
    validate_segments(
        *logical_length,
        *segment_size,
        segments,
        pack_lengths,
        context,
    )?;
    let inline = ContentLayoutRecord {
        format_version: CONTENT_LAYOUT_FORMAT_VERSION,
        logical_content_id: logical_content_id.clone(),
        logical_length: *logical_length,
        segment_size: *segment_size,
        segments: SegmentSequence::Inline(segments.clone()),
    };
    let record = if encode_content_layout_with_limit(&inline, usize::MAX)?.len()
        <= INLINE_SEGMENT_MAX_BYTES
    {
        inline
    } else {
        let root = store_segment_tree(workspace_id, segments, store, context)?;
        ContentLayoutRecord {
            format_version: CONTENT_LAYOUT_FORMAT_VERSION,
            logical_content_id: logical_content_id.clone(),
            logical_length: *logical_length,
            segment_size: *segment_size,
            segments: SegmentSequence::Paged {
                root,
                count: segments.len() as u64,
            },
        }
    };
    let bytes = encode_content_layout(&record)?;
    let id = ContentLayoutId::new(logical_id("ctl", store.identity_key(), &bytes));
    insert_layout(id.clone(), bytes, store)?;
    Ok(id)
}

pub(crate) fn resolve_content_layout(
    workspace_id: &str,
    id: &ContentLayoutId,
    store: &dyn PagedRecordSource,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<ContentLayout, NamespaceReadError> {
    let record = load_content_layout(workspace_id, id, store, context)?;
    let segments = match &record.segments {
        SegmentSequence::Inline(segments) => segments.clone(),
        SegmentSequence::Paged { root, count } => {
            validate_supported_segment_count_u64(*count)?;
            let capacity =
                usize::try_from(*count).map_err(|_| NamespaceReadError::CorruptGraph {
                    reason: "content layout exceeds the supported segment count",
                })?;
            let mut segments = Vec::with_capacity(capacity);
            let expected = layout_root_summary(root, *count, record.logical_length)?;
            collect_segment_page(
                workspace_id,
                root,
                store,
                context,
                &mut segments,
                0,
                Some(&expected),
            )?;
            if segments.len() as u64 != *count {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "segment tree count does not match content layout",
                });
            }
            segments
        }
    };
    validate_segments(
        record.logical_length,
        record.segment_size,
        &segments,
        None,
        context,
    )?;
    Ok(ContentLayout::SegmentedV1 {
        logical_content_id: record.logical_content_id,
        logical_length: record.logical_length,
        segment_size: record.segment_size,
        segments,
    })
}

pub(crate) fn read_layout_range(
    workspace_id: &str,
    id: &ContentLayoutId,
    store: &dyn PagedRecordSource,
    logical_offset: u64,
    logical_length: u64,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<Vec<SegmentLocator>, NamespaceReadError> {
    let record = load_content_layout(workspace_id, id, store, context)?;
    let range_end =
        logical_offset
            .checked_add(logical_length)
            .ok_or(NamespaceReadError::CorruptGraph {
                reason: "requested logical range overflows",
            })?;
    if range_end > record.logical_length {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "requested logical range exceeds content length",
        });
    }
    if logical_length == 0 {
        return Ok(Vec::new());
    }
    match &record.segments {
        SegmentSequence::Inline(segments) => {
            validate_segment_values(record.logical_length, record.segment_size, segments)?;
            validate_locator_storage_ranges(segments)?;
            Ok(select_range(segments, 0, logical_offset, range_end))
        }
        SegmentSequence::Paged { root, count } => {
            let mut selected = Vec::new();
            let expected = layout_root_summary(root, *count, record.logical_length)?;
            read_segment_range(
                SegmentRangeRead {
                    source: SegmentSource {
                        workspace_id,
                        store,
                    },
                    range: LogicalRange {
                        start: logical_offset,
                        end: range_end,
                    },
                    total_segments: *count,
                    segment_size: record.segment_size,
                },
                root,
                context,
                &mut selected,
                0,
                Some(&expected),
            )?;
            Ok(selected)
        }
    }
}

fn store_segment_tree(
    workspace_id: &str,
    segments: &[SegmentLocator],
    store: &mut PageStore,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<SegmentPageId, NamespaceReadError> {
    let mut leaves = Vec::new();
    let mut start = 0;
    let mut logical_start = 0_u64;
    while start < segments.len() {
        context.ensure_active()?;
        let mut end = start + 1;
        while end <= segments.len() {
            let page = SegmentPage::Leaf {
                first_ordinal: segments[start].ordinal,
                logical_start,
                segments: segments[start..end].to_vec(),
            };
            match encode_segment_page(&page) {
                Ok(bytes) if bytes.len() <= SEGMENT_PAGE_TARGET_BYTES => end += 1,
                Ok(_) | Err(NamespaceReadError::OversizedRecord { .. }) if end > start + 1 => {
                    end -= 1;
                    break;
                }
                Err(error) => return Err(error),
                Ok(_) => break,
            }
        }
        end = end.min(segments.len());
        if end == start {
            end += 1;
        }
        let page = SegmentPage::Leaf {
            first_ordinal: segments[start].ordinal,
            logical_start,
            segments: segments[start..end].to_vec(),
        };
        let summary = store_segment_page(workspace_id, page, store)?;
        logical_start = logical_start.checked_add(summary.logical_length).ok_or(
            NamespaceReadError::CorruptGraph {
                reason: "segment page logical offset overflows",
            },
        )?;
        leaves.push(summary);
        start = end;
    }
    let mut level = leaves;
    while level.len() > 1 {
        let mut parent_level = Vec::new();
        for children in level.chunks(SEGMENT_INDEX_FANOUT) {
            context.ensure_active()?;
            let first = children.first().ok_or(NamespaceReadError::CorruptGraph {
                reason: "empty segment index group",
            })?;
            let segment_count = children.iter().try_fold(0_u32, |count, child| {
                count
                    .checked_add(child.segment_count)
                    .ok_or(NamespaceReadError::CorruptGraph {
                        reason: "segment index count overflows",
                    })
            })?;
            let logical_length = children.iter().try_fold(0_u64, |length, child| {
                length
                    .checked_add(child.logical_length)
                    .ok_or(NamespaceReadError::CorruptGraph {
                        reason: "segment index logical length overflows",
                    })
            })?;
            let page = SegmentPage::Index {
                first_ordinal: first.first_ordinal,
                segment_count,
                logical_start: first.logical_start,
                logical_length,
                children: children.to_vec(),
            };
            parent_level.push(store_segment_page(workspace_id, page, store)?);
        }
        level = parent_level;
    }
    level
        .pop()
        .map(|summary| summary.page_id)
        .ok_or(NamespaceReadError::CorruptGraph {
            reason: "paged segment layout has no root",
        })
}

fn store_segment_page(
    _workspace_id: &str,
    page: SegmentPage,
    store: &mut PageStore,
) -> Result<SegmentChildSummary, NamespaceReadError> {
    let summary = segment_summary(&page)?;
    let bytes = encode_segment_page(&page)?;
    let id = SegmentPageId::new(logical_id("sgp", store.identity_key(), &bytes));
    store.insert_segment_page(id.clone(), bytes)?;
    Ok(SegmentChildSummary {
        page_id: id,
        ..summary
    })
}

fn insert_layout(
    id: ContentLayoutId,
    bytes: Vec<u8>,
    store: &mut PageStore,
) -> Result<(), NamespaceReadError> {
    store.insert_content_layout(id, bytes)
}

fn load_content_layout(
    _workspace_id: &str,
    id: &ContentLayoutId,
    store: &dyn PagedRecordSource,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<ContentLayoutRecord, NamespaceReadError> {
    let bytes = store
        .load_record(MetadataRecordKind::ContentLayout, id.as_str(), context)?
        .ok_or(NamespaceReadError::MissingRecord {
            record: "content layout",
        })?;
    context.charge_layout_record(bytes.len() as u64)?;
    if logical_id("ctl", store.metadata_identity_key(), &bytes) != id.as_str() {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "content layout logical ID mismatch",
        });
    }
    decode_content_layout(&bytes)
}

fn load_segment_page(
    _workspace_id: &str,
    id: &SegmentPageId,
    store: &dyn PagedRecordSource,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<SegmentPage, NamespaceReadError> {
    let bytes = store
        .load_record(MetadataRecordKind::SegmentPage, id.as_str(), context)?
        .ok_or(NamespaceReadError::MissingRecord {
            record: "segment page",
        })?;
    context.charge_segment_page(bytes.len() as u64)?;
    if logical_id("sgp", store.metadata_identity_key(), &bytes) != id.as_str() {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "segment page logical ID mismatch",
        });
    }
    decode_segment_page(&bytes)
}

fn collect_segment_page(
    workspace_id: &str,
    id: &SegmentPageId,
    store: &dyn PagedRecordSource,
    context: &mut NamespaceOperationContext<'_>,
    output: &mut Vec<SegmentLocator>,
    depth: usize,
    expected: Option<&SegmentChildSummary>,
) -> Result<(), NamespaceReadError> {
    if depth > 32 {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "segment page tree exceeds maximum depth",
        });
    }
    let page = load_segment_page(workspace_id, id, store, context)?;
    validate_expected_summary(id, &page, expected)?;
    match page {
        SegmentPage::Leaf { segments, .. } => output.extend(segments),
        SegmentPage::Index { children, .. } => {
            store.prefetch_records(
                MetadataRecordKind::SegmentPage,
                &children
                    .iter()
                    .map(|child| child.page_id.as_str().to_string())
                    .collect::<Vec<_>>(),
                context,
            )?;
            for child in children {
                collect_segment_page(
                    workspace_id,
                    &child.page_id,
                    store,
                    context,
                    output,
                    depth + 1,
                    Some(&child),
                )?;
            }
        }
    }
    Ok(())
}

fn read_segment_range(
    request: SegmentRangeRead<'_>,
    id: &SegmentPageId,
    context: &mut NamespaceOperationContext<'_>,
    output: &mut Vec<SegmentLocator>,
    depth: usize,
    expected: Option<&SegmentChildSummary>,
) -> Result<(), NamespaceReadError> {
    if depth > 32 {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "segment page tree exceeds maximum depth",
        });
    }
    let page = load_segment_page(
        request.source.workspace_id,
        id,
        request.source.store,
        context,
    )?;
    validate_expected_summary(id, &page, expected)?;
    match page {
        SegmentPage::Leaf {
            first_ordinal,
            logical_start,
            segments,
            ..
        } => {
            validate_range_leaf_segments(
                first_ordinal,
                &segments,
                request.total_segments,
                request.segment_size,
            )?;
            output.extend(select_range(
                &segments,
                logical_start,
                request.range.start,
                request.range.end,
            ));
        }
        SegmentPage::Index { children, .. } => {
            let mut relevant_children = Vec::new();
            for child in children {
                let child_end = child
                    .logical_start
                    .checked_add(child.logical_length)
                    .ok_or(NamespaceReadError::CorruptGraph {
                        reason: "segment child logical range overflows",
                    })?;
                if child.logical_start < request.range.end && child_end > request.range.start {
                    relevant_children.push(child);
                }
            }
            request.source.store.prefetch_records(
                MetadataRecordKind::SegmentPage,
                &relevant_children
                    .iter()
                    .map(|child| child.page_id.as_str().to_string())
                    .collect::<Vec<_>>(),
                context,
            )?;
            for child in relevant_children {
                read_segment_range(
                    request,
                    &child.page_id,
                    context,
                    output,
                    depth + 1,
                    Some(&child),
                )?;
            }
        }
    }
    Ok(())
}

fn validate_range_leaf_segments(
    first_ordinal: u32,
    segments: &[SegmentLocator],
    total_segments: u64,
    segment_size: u64,
) -> Result<(), NamespaceReadError> {
    validate_locator_storage_ranges(segments)?;
    for (offset, segment) in segments.iter().enumerate() {
        let ordinal = u64::from(first_ordinal).checked_add(offset as u64).ok_or(
            NamespaceReadError::CorruptGraph {
                reason: "segment leaf ordinal range overflows",
            },
        )?;
        let is_last = ordinal
            .checked_add(1)
            .is_some_and(|next| next == total_segments);
        if ordinal >= total_segments
            || segment.plaintext_length > segment_size
            || (!is_last && segment.plaintext_length != segment_size)
        {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "segment leaf does not match its content layout",
            });
        }
    }
    Ok(())
}

fn select_range(
    segments: &[SegmentLocator],
    mut logical_start: u64,
    start: u64,
    end: u64,
) -> Vec<SegmentLocator> {
    let mut selected = Vec::new();
    for segment in segments {
        let logical_end = logical_start.saturating_add(segment.plaintext_length);
        if logical_start < end && logical_end > start {
            selected.push(segment.clone());
        }
        logical_start = logical_end;
    }
    selected
}

pub(crate) fn encode_content_layout(
    record: &ContentLayoutRecord,
) -> Result<Vec<u8>, NamespaceReadError> {
    encode_content_layout_with_limit(record, SEGMENT_PAGE_MAX_BYTES)
}

fn encode_content_layout_with_limit(
    record: &ContentLayoutRecord,
    maximum_bytes: usize,
) -> Result<Vec<u8>, NamespaceReadError> {
    let mut encoder = Encoder::new(CONTENT_LAYOUT_MAGIC, record.format_version);
    encoder.string(record.logical_content_id.as_str())?;
    encoder.u64(record.logical_length);
    encoder.u64(record.segment_size);
    match &record.segments {
        SegmentSequence::Inline(segments) => {
            encoder.u8(0);
            encoder.len(segments.len())?;
            for segment in segments {
                encode_locator(&mut encoder, segment)?;
            }
        }
        SegmentSequence::Paged { root, count } => {
            encoder.u8(1);
            encoder.logical_id(root.as_str(), "sgp")?;
            encoder.u64(*count);
        }
    }
    encoder.finish("content layout", maximum_bytes)
}

pub(crate) fn decode_content_layout(
    bytes: &[u8],
) -> Result<ContentLayoutRecord, NamespaceReadError> {
    if bytes.len() > SEGMENT_PAGE_MAX_BYTES {
        return Err(NamespaceReadError::OversizedRecord {
            record: "content layout",
            encoded_bytes: bytes.len() as u64,
            maximum_bytes: SEGMENT_PAGE_MAX_BYTES as u64,
        });
    }
    let mut decoder = Decoder::new(
        bytes,
        CONTENT_LAYOUT_MAGIC,
        "content layout",
        CONTENT_LAYOUT_FORMAT_VERSION,
    )?;
    let logical_content_id = ContentId::new(decoder.string()?);
    let logical_length = decoder.u64()?;
    let segment_size = decoder.u64()?;
    let segments = match decoder.u8()? {
        0 => {
            let count = decoder.len()?;
            let mut segments = Vec::with_capacity(count);
            for _ in 0..count {
                segments.push(decode_locator(&mut decoder)?);
            }
            SegmentSequence::Inline(segments)
        }
        1 => {
            let root = SegmentPageId::new(decoder.logical_id("sgp")?);
            let count = decoder.u64()?;
            validate_supported_segment_count_u64(count)?;
            SegmentSequence::Paged { root, count }
        }
        _ => {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "unknown content layout sequence discriminant",
            });
        }
    };
    decoder.finish()?;
    let record = ContentLayoutRecord {
        format_version: CONTENT_LAYOUT_FORMAT_VERSION,
        logical_content_id,
        logical_length,
        segment_size,
        segments,
    };
    validate_content_layout_record(&record)?;
    if encode_content_layout(&record)?.as_slice() != bytes {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "non-canonical content layout encoding",
        });
    }
    Ok(record)
}

pub(crate) fn encode_segment_page(page: &SegmentPage) -> Result<Vec<u8>, NamespaceReadError> {
    let mut encoder = Encoder::new(SEGMENT_PAGE_MAGIC, SEGMENT_PAGE_FORMAT_VERSION);
    match page {
        SegmentPage::Leaf {
            first_ordinal,
            logical_start,
            segments,
        } => {
            encoder.u8(0);
            encoder.u32(*first_ordinal);
            encoder.u64(*logical_start);
            encoder.len(segments.len())?;
            for segment in segments {
                encode_locator(&mut encoder, segment)?;
            }
        }
        SegmentPage::Index {
            first_ordinal,
            segment_count,
            logical_start,
            logical_length,
            children,
        } => {
            encoder.u8(1);
            encoder.u32(*first_ordinal);
            encoder.u32(*segment_count);
            encoder.u64(*logical_start);
            encoder.u64(*logical_length);
            encoder.len(children.len())?;
            let mut previous = None;
            for child in children {
                if previous.is_some_and(|ordinal| ordinal >= child.first_ordinal) {
                    return Err(NamespaceReadError::NonCanonicalOrder {
                        field: "segment page child",
                    });
                }
                encoder.u32(child.first_ordinal);
                encoder.u32(child.segment_count);
                encoder.u64(child.logical_start);
                encoder.u64(child.logical_length);
                encoder.logical_id(child.page_id.as_str(), "sgp")?;
                previous = Some(child.first_ordinal);
            }
        }
    }
    encoder.finish("segment page", SEGMENT_PAGE_MAX_BYTES)
}

pub(crate) fn decode_segment_page(bytes: &[u8]) -> Result<SegmentPage, NamespaceReadError> {
    if bytes.len() > SEGMENT_PAGE_MAX_BYTES {
        return Err(NamespaceReadError::OversizedRecord {
            record: "segment page",
            encoded_bytes: bytes.len() as u64,
            maximum_bytes: SEGMENT_PAGE_MAX_BYTES as u64,
        });
    }
    let mut decoder = Decoder::new(
        bytes,
        SEGMENT_PAGE_MAGIC,
        "segment page",
        SEGMENT_PAGE_FORMAT_VERSION,
    )?;
    let page = match decoder.u8()? {
        0 => {
            let first_ordinal = decoder.u32()?;
            let logical_start = decoder.u64()?;
            let count = decoder.len()?;
            let mut segments = Vec::with_capacity(count);
            for _ in 0..count {
                segments.push(decode_locator(&mut decoder)?);
            }
            SegmentPage::Leaf {
                first_ordinal,
                logical_start,
                segments,
            }
        }
        1 => {
            let first_ordinal = decoder.u32()?;
            let segment_count = decoder.u32()?;
            let logical_start = decoder.u64()?;
            let logical_length = decoder.u64()?;
            let count = decoder.len()?;
            let mut children = Vec::with_capacity(count);
            let mut previous = None;
            for _ in 0..count {
                let child = SegmentChildSummary {
                    first_ordinal: decoder.u32()?,
                    segment_count: decoder.u32()?,
                    logical_start: decoder.u64()?,
                    logical_length: decoder.u64()?,
                    page_id: SegmentPageId::new(decoder.logical_id("sgp")?),
                };
                if previous.is_some_and(|ordinal| ordinal >= child.first_ordinal) {
                    return Err(NamespaceReadError::NonCanonicalOrder {
                        field: "segment page child",
                    });
                }
                previous = Some(child.first_ordinal);
                children.push(child);
            }
            if children.is_empty() {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "empty segment index page",
                });
            }
            SegmentPage::Index {
                first_ordinal,
                segment_count,
                logical_start,
                logical_length,
                children,
            }
        }
        _ => {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "unknown segment page discriminant",
            });
        }
    };
    decoder.finish()?;
    validate_segment_page_shape(&page)?;
    if encode_segment_page(&page)?.as_slice() != bytes {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "non-canonical segment page encoding",
        });
    }
    Ok(page)
}

fn encode_locator(
    encoder: &mut Encoder,
    segment: &SegmentLocator,
) -> Result<(), NamespaceReadError> {
    encoder.u32(segment.ordinal);
    encoder.u64(segment.plaintext_length);
    encoder.string(segment.segment_id.as_str())?;
    encoder.string(segment.pack_id.as_str())?;
    encoder.u64(segment.offset);
    encoder.u64(segment.length);
    encoder.u16(segment.format_version);
    Ok(())
}

fn decode_locator(decoder: &mut Decoder<'_>) -> Result<SegmentLocator, NamespaceReadError> {
    Ok(SegmentLocator {
        ordinal: decoder.u32()?,
        plaintext_length: decoder.u64()?,
        segment_id: SegmentId::new(decoder.string()?),
        pack_id: PackId::new(decoder.string()?),
        offset: decoder.u64()?,
        length: decoder.u64()?,
        format_version: decoder.u16()?,
    })
}
