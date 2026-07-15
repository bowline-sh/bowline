use std::{error::Error, fmt};

use bowline_core::{ids::ContentId, workspace_graph::ContentLocator};

use crate::ByteRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationRecord {
    pub locator: ContentLocator,
    pub cached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRecord {
    pub locator: ContentLocator,
    pub offset_within_fetch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedRange {
    pub range: ByteRange,
    pub records: Vec<PlannedRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackHydrationSource {
    CachedPack(Vec<PlannedRecord>),
    RemoteFull(Vec<PlannedRecord>),
    RemoteRanges(Vec<CoalescedRange>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackHydrationPlan {
    pub cached_content_ids: Vec<ContentId>,
    pub source: Option<PackHydrationSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HydrationPlanError {
    MissingLocatorField(&'static str),
    RangeOverflow {
        offset: u64,
        length: u64,
    },
    RangeOutOfBounds {
        offset: u64,
        length: u64,
        pack_len: u64,
    },
    OverlappingRanges {
        previous_end: u64,
        next_offset: u64,
    },
    ConflictingContentLocator {
        content_id: ContentId,
    },
}

impl fmt::Display for HydrationPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingLocatorField(field) => {
                write!(formatter, "packed locator is missing {field}")
            }
            Self::RangeOverflow { offset, length } => {
                write!(formatter, "hydration range {offset}+{length} overflows")
            }
            Self::RangeOutOfBounds {
                offset,
                length,
                pack_len,
            } => write!(
                formatter,
                "hydration range {offset}+{length} exceeds pack length {pack_len}"
            ),
            Self::OverlappingRanges {
                previous_end,
                next_offset,
            } => write!(
                formatter,
                "hydration ranges overlap at {next_offset} before prior end {previous_end}"
            ),
            Self::ConflictingContentLocator { content_id } => write!(
                formatter,
                "content {} has conflicting pack locators",
                content_id.as_str()
            ),
        }
    }
}

impl Error for HydrationPlanError {}

pub fn plan_pack_hydration(
    pack_byte_len: u64,
    full_pack_cached: bool,
    all_records_selected: bool,
    records: Vec<HydrationRecord>,
) -> Result<PackHydrationPlan, HydrationPlanError> {
    let mut record_by_content = std::collections::BTreeMap::<ContentId, HydrationRecord>::new();
    for record in records {
        if let Some(existing) = record_by_content.get_mut(&record.locator.content_id) {
            if existing.locator != record.locator {
                return Err(HydrationPlanError::ConflictingContentLocator {
                    content_id: record.locator.content_id,
                });
            }
            existing.cached |= record.cached;
        } else {
            record_by_content.insert(record.locator.content_id.clone(), record);
        }
    }
    let mut canonical_records = record_by_content.into_values().collect::<Vec<_>>();
    canonical_records.sort_by_key(|record| record.locator.offset);
    let validated_records = validate_records(pack_byte_len, canonical_records)?;

    let mut cached_content_ids = Vec::new();
    let mut remote_records = Vec::new();
    for record in validated_records {
        if record.cached {
            cached_content_ids.push(record.planned.locator.content_id);
        } else {
            remote_records.push(record.planned);
        }
    }
    let source = if remote_records.is_empty() {
        None
    } else if full_pack_cached {
        Some(PackHydrationSource::CachedPack(remote_records))
    } else if all_records_selected {
        Some(PackHydrationSource::RemoteFull(remote_records))
    } else {
        Some(PackHydrationSource::RemoteRanges(coalesce_adjacent(
            remote_records,
        )?))
    };
    Ok(PackHydrationPlan {
        cached_content_ids,
        source,
    })
}

struct ValidatedHydrationRecord {
    planned: PlannedRecord,
    cached: bool,
}

fn validate_records(
    pack_byte_len: u64,
    hydration_records: Vec<HydrationRecord>,
) -> Result<Vec<ValidatedHydrationRecord>, HydrationPlanError> {
    let mut records = Vec::with_capacity(hydration_records.len());
    let mut previous_end = None;
    for hydration_record in hydration_records {
        let locator = hydration_record.locator;
        let offset = locator
            .offset
            .ok_or(HydrationPlanError::MissingLocatorField("offset"))?;
        let length = locator
            .length
            .ok_or(HydrationPlanError::MissingLocatorField("length"))?;
        let end = offset
            .checked_add(length)
            .ok_or(HydrationPlanError::RangeOverflow { offset, length })?;
        if end > pack_byte_len {
            return Err(HydrationPlanError::RangeOutOfBounds {
                offset,
                length,
                pack_len: pack_byte_len,
            });
        }
        if let Some(previous_end) = previous_end
            && offset < previous_end
        {
            return Err(HydrationPlanError::OverlappingRanges {
                previous_end,
                next_offset: offset,
            });
        }
        previous_end = Some(end);
        records.push(ValidatedHydrationRecord {
            planned: PlannedRecord {
                locator,
                offset_within_fetch: offset,
            },
            cached: hydration_record.cached,
        });
    }
    Ok(records)
}

fn coalesce_adjacent(
    records: Vec<PlannedRecord>,
) -> Result<Vec<CoalescedRange>, HydrationPlanError> {
    let mut ranges = Vec::<CoalescedRange>::new();
    for mut record in records {
        let offset = record.offset_within_fetch;
        let length = record
            .locator
            .length
            .ok_or(HydrationPlanError::MissingLocatorField("length"))?;
        if let Some(range) = ranges.last_mut() {
            let range_end = range.range.offset.checked_add(range.range.length).ok_or(
                HydrationPlanError::RangeOverflow {
                    offset: range.range.offset,
                    length: range.range.length,
                },
            )?;
            if range_end == offset {
                record.offset_within_fetch = range.range.length;
                range.range.length = range.range.length.checked_add(length).ok_or(
                    HydrationPlanError::RangeOverflow {
                        offset: range.range.offset,
                        length: range.range.length,
                    },
                )?;
                range.records.push(record);
                continue;
            }
        }
        record.offset_within_fetch = 0;
        ranges.push(CoalescedRange {
            range: ByteRange::new(offset, length),
            records: vec![record],
        });
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        ids::{ContentId, PackId},
        workspace_graph::{ContentLocator, ContentStorage},
    };

    use super::*;

    fn record(name: &str, offset: u64, length: u64) -> HydrationRecord {
        HydrationRecord {
            locator: ContentLocator {
                content_id: ContentId::new(name),
                storage: ContentStorage::Packed,
                raw_size: 1,
                pack_id: Some(PackId::new("pk_1111111111111111")),
                offset: Some(offset),
                length: Some(length),
            },
            cached: false,
        }
    }

    fn cached_record(name: &str, offset: u64, length: u64) -> HydrationRecord {
        HydrationRecord {
            cached: true,
            ..record(name, offset, length)
        }
    }

    #[test]
    fn planner_sorts_and_coalesces_only_exact_adjacency() {
        let plan = plan_pack_hydration(
            100,
            false,
            false,
            vec![record("c", 30, 10), record("a", 0, 10), record("b", 10, 10)],
        )
        .expect("plan");
        let Some(PackHydrationSource::RemoteRanges(ranges)) = plan.source else {
            panic!("expected ranges");
        };
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].range, ByteRange::new(0, 20));
        assert_eq!(ranges[0].records[1].offset_within_fetch, 10);
        assert_eq!(ranges[1].range, ByteRange::new(30, 10));
    }

    #[test]
    fn planner_rejects_overlap_bounds_and_overflow_before_io() {
        assert!(matches!(
            plan_pack_hydration(
                100,
                false,
                false,
                vec![record("a", 0, 20), record("b", 10, 5)]
            ),
            Err(HydrationPlanError::OverlappingRanges { .. })
        ));
        assert!(matches!(
            plan_pack_hydration(10, false, false, vec![record("a", 9, 2)]),
            Err(HydrationPlanError::RangeOutOfBounds { .. })
        ));
        assert!(matches!(
            plan_pack_hydration(u64::MAX, false, false, vec![record("a", u64::MAX, 1)]),
            Err(HydrationPlanError::RangeOverflow { .. })
        ));
    }

    #[test]
    fn planner_omits_cached_records_and_preserves_full_sources() {
        let mut cached = record("a", 0, 10);
        cached.cached = true;
        let plan =
            plan_pack_hydration(20, false, true, vec![cached, record("b", 10, 10)]).expect("plan");
        assert_eq!(plan.cached_content_ids, vec![ContentId::new("a")]);
        assert!(matches!(
            plan.source,
            Some(PackHydrationSource::RemoteFull(_))
        ));

        let plan = plan_pack_hydration(20, true, false, vec![record("b", 10, 10)]).expect("plan");
        assert!(matches!(
            plan.source,
            Some(PackHydrationSource::CachedPack(_))
        ));
    }

    #[test]
    fn planner_rejects_conflicting_cached_and_mixed_locators() {
        for records in [
            vec![cached_record("a", 0, 10), cached_record("a", 10, 10)],
            vec![cached_record("a", 0, 10), record("a", 10, 10)],
            vec![record("a", 0, 10), record("a", 10, 10)],
        ] {
            assert!(matches!(
                plan_pack_hydration(100, false, false, records),
                Err(HydrationPlanError::ConflictingContentLocator { .. })
            ));
        }
    }

    #[test]
    fn cached_records_cannot_bypass_complete_range_validation() {
        assert!(matches!(
            plan_pack_hydration(10, false, false, vec![cached_record("a", 9, 2)]),
            Err(HydrationPlanError::RangeOutOfBounds { .. })
        ));
        assert!(matches!(
            plan_pack_hydration(
                u64::MAX,
                false,
                false,
                vec![cached_record("a", u64::MAX, 1)]
            ),
            Err(HydrationPlanError::RangeOverflow { .. })
        ));
        assert!(matches!(
            plan_pack_hydration(
                100,
                false,
                false,
                vec![cached_record("a", 0, 20), cached_record("b", 10, 5)]
            ),
            Err(HydrationPlanError::OverlappingRanges { .. })
        ));
    }
}
