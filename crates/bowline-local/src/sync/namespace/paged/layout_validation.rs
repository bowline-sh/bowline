use bowline_core::{
    ids::SegmentPageId,
    namespace_snapshot::{NamespaceOperationContext, NamespaceReadError},
    workspace_graph::SegmentLocator,
};

use super::{
    layout::{
        ContentLayoutRecord, PackLengthResolver, SegmentChildSummary, SegmentPage, SegmentSequence,
    },
    types::MAX_SEGMENTS_PER_LAYOUT,
};

pub(super) fn validate_segments(
    logical_length: u64,
    segment_size: u64,
    segments: &[SegmentLocator],
    pack_lengths: Option<&dyn PackLengthResolver>,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<(), NamespaceReadError> {
    validate_segment_values(logical_length, segment_size, segments)?;
    validate_locator_storage_ranges(segments)?;
    for segment in segments {
        context.ensure_active()?;
        let pack_end = segment.offset + segment.length;
        if let Some(resolver) = pack_lengths {
            let pack_length = resolver.pack_length(&segment.pack_id)?.ok_or(
                NamespaceReadError::MissingRecord {
                    record: "source pack length",
                },
            )?;
            if pack_end > pack_length {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "segment range exceeds resolved pack length",
                });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_locator_storage_ranges(
    segments: &[SegmentLocator],
) -> Result<(), NamespaceReadError> {
    for segment in segments {
        segment
            .offset
            .checked_add(segment.length)
            .ok_or(NamespaceReadError::CorruptGraph {
                reason: "segment pack range overflows",
            })?;
    }
    Ok(())
}

pub(super) fn validate_segment_values(
    logical_length: u64,
    segment_size: u64,
    segments: &[SegmentLocator],
) -> Result<(), NamespaceReadError> {
    validate_supported_segment_count(segments.len())?;
    if segment_size == 0 {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "invalid content layout segment count or size",
        });
    }
    if (logical_length == 0) != segments.is_empty() {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "content layout emptiness does not match logical length",
        });
    }
    let mut total = 0_u64;
    for (index, segment) in segments.iter().enumerate() {
        if segment.ordinal as usize != index
            || segment.plaintext_length == 0
            || segment.plaintext_length > segment_size
            || (index + 1 < segments.len() && segment.plaintext_length != segment_size)
            || segment.length == 0
            || segment.format_version == 0
            || segment.segment_id.as_str().is_empty()
            || segment.pack_id.as_str().is_empty()
        {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "invalid segment locator sequence",
            });
        }
        total = total.checked_add(segment.plaintext_length).ok_or(
            NamespaceReadError::CorruptGraph {
                reason: "content layout logical length overflows",
            },
        )?;
    }
    if total != logical_length {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "segment plaintext lengths do not equal logical length",
        });
    }
    Ok(())
}

pub(crate) fn validate_supported_segment_count(count: usize) -> Result<(), NamespaceReadError> {
    if count > MAX_SEGMENTS_PER_LAYOUT {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "content layout exceeds the supported segment count",
        });
    }
    Ok(())
}

pub(super) fn validate_supported_segment_count_u64(count: u64) -> Result<(), NamespaceReadError> {
    if count > MAX_SEGMENTS_PER_LAYOUT as u64 {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "content layout exceeds the supported segment count",
        });
    }
    Ok(())
}

pub(super) fn validate_content_layout_record(
    record: &ContentLayoutRecord,
) -> Result<(), NamespaceReadError> {
    if record.logical_content_id.as_str().is_empty() || record.segment_size == 0 {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "invalid content layout identity or segment size",
        });
    }
    match &record.segments {
        SegmentSequence::Inline(segments) => {
            validate_segment_values(record.logical_length, record.segment_size, segments)
        }
        SegmentSequence::Paged { count, .. } => {
            validate_supported_segment_count_u64(*count)?;
            if *count == 0 || record.logical_length == 0 {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "paged content layout must be non-empty",
                });
            }
            let expected_count = record.logical_length.div_ceil(record.segment_size);
            if *count != expected_count {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "paged content layout count does not match its logical shape",
                });
            }
            Ok(())
        }
    }
}

pub(super) fn validate_segment_page_shape(page: &SegmentPage) -> Result<(), NamespaceReadError> {
    match page {
        SegmentPage::Leaf {
            first_ordinal,
            segments,
            ..
        } => {
            if segments.is_empty() {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "empty segment leaf page",
                });
            }
            for (offset, segment) in segments.iter().enumerate() {
                let expected = first_ordinal.checked_add(offset as u32).ok_or(
                    NamespaceReadError::CorruptGraph {
                        reason: "segment leaf ordinal range overflows",
                    },
                )?;
                if segment.ordinal != expected
                    || segment.plaintext_length == 0
                    || segment.length == 0
                    || segment.format_version == 0
                    || segment.segment_id.as_str().is_empty()
                    || segment.pack_id.as_str().is_empty()
                {
                    return Err(NamespaceReadError::CorruptGraph {
                        reason: "invalid segment leaf locator sequence",
                    });
                }
            }
            segment_summary(page).map(|_| ())
        }
        SegmentPage::Index {
            first_ordinal,
            segment_count,
            logical_start,
            logical_length,
            children,
        } => {
            let mut next_ordinal = *first_ordinal;
            let mut next_logical_start = *logical_start;
            for child in children {
                if child.segment_count == 0
                    || child.logical_length == 0
                    || child.first_ordinal != next_ordinal
                    || child.logical_start != next_logical_start
                {
                    return Err(NamespaceReadError::CorruptGraph {
                        reason: "segment index summaries are not contiguous",
                    });
                }
                next_ordinal = next_ordinal.checked_add(child.segment_count).ok_or(
                    NamespaceReadError::CorruptGraph {
                        reason: "segment index ordinal range overflows",
                    },
                )?;
                next_logical_start = next_logical_start.checked_add(child.logical_length).ok_or(
                    NamespaceReadError::CorruptGraph {
                        reason: "segment index logical range overflows",
                    },
                )?;
            }
            if next_ordinal != first_ordinal.saturating_add(*segment_count)
                || next_logical_start != logical_start.saturating_add(*logical_length)
            {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "segment index summary totals do not match children",
                });
            }
            Ok(())
        }
    }
}

pub(super) fn layout_root_summary(
    root: &SegmentPageId,
    count: u64,
    logical_length: u64,
) -> Result<SegmentChildSummary, NamespaceReadError> {
    validate_supported_segment_count_u64(count)?;
    Ok(SegmentChildSummary {
        first_ordinal: 0,
        segment_count: u32::try_from(count).map_err(|_| NamespaceReadError::CorruptGraph {
            reason: "content layout exceeds the supported segment count",
        })?,
        logical_start: 0,
        logical_length,
        page_id: root.clone(),
    })
}

pub(super) fn validate_expected_summary(
    id: &SegmentPageId,
    page: &SegmentPage,
    expected: Option<&SegmentChildSummary>,
) -> Result<(), NamespaceReadError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = segment_summary(page)?;
    if id != &expected.page_id
        || actual.first_ordinal != expected.first_ordinal
        || actual.segment_count != expected.segment_count
        || actual.logical_start != expected.logical_start
        || actual.logical_length != expected.logical_length
    {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "segment page does not match its authenticated parent summary",
        });
    }
    Ok(())
}

pub(super) fn segment_summary(
    page: &SegmentPage,
) -> Result<SegmentChildSummary, NamespaceReadError> {
    let (first_ordinal, segment_count, logical_start, logical_length) = match page {
        SegmentPage::Leaf {
            first_ordinal,
            logical_start,
            segments,
        } => (
            *first_ordinal,
            u32::try_from(segments.len()).map_err(|_| NamespaceReadError::CorruptGraph {
                reason: "segment page count overflows",
            })?,
            *logical_start,
            segments.iter().try_fold(0_u64, |length, segment| {
                length.checked_add(segment.plaintext_length).ok_or(
                    NamespaceReadError::CorruptGraph {
                        reason: "segment page logical length overflows",
                    },
                )
            })?,
        ),
        SegmentPage::Index {
            first_ordinal,
            segment_count,
            logical_start,
            logical_length,
            ..
        } => (
            *first_ordinal,
            *segment_count,
            *logical_start,
            *logical_length,
        ),
    };
    Ok(SegmentChildSummary {
        first_ordinal,
        segment_count,
        logical_start,
        logical_length,
        page_id: SegmentPageId::new("unbound"),
    })
}
