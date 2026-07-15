use std::collections::BTreeMap;

use super::{MergeBudget, MergePhase};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LineChange<'a> {
    pub(super) base_start: usize,
    pub(super) base_end: usize,
    modified_start: usize,
    modified_end: usize,
    pub(super) replacement: Vec<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffFailure {
    ResourceLimit {
        phase: MergePhase,
        budget: MergeBudget,
    },
    InvalidEditScript,
}

pub(super) struct WorkBudget {
    steps: usize,
    max_steps: usize,
    trace_cells: usize,
    max_trace_cells: usize,
    max_anchor_depth: usize,
    anchor_count: usize,
    myers_regions: usize,
}

impl WorkBudget {
    pub(super) fn new(max_steps: usize, max_trace_cells: usize, max_anchor_depth: usize) -> Self {
        Self {
            steps: 0,
            max_steps,
            trace_cells: 0,
            max_trace_cells,
            max_anchor_depth,
            anchor_count: 0,
            myers_regions: 0,
        }
    }

    pub(super) fn anchor_count(&self) -> usize {
        self.anchor_count
    }

    pub(super) fn myers_regions(&self) -> usize {
        self.myers_regions
    }

    fn consume_steps(&mut self, count: usize, phase: MergePhase) -> Result<(), DiffFailure> {
        self.steps = self
            .steps
            .checked_add(count)
            .filter(|steps| *steps <= self.max_steps)
            .ok_or(DiffFailure::ResourceLimit {
                phase,
                budget: MergeBudget::WorkSteps,
            })?;
        Ok(())
    }

    fn reserve_trace(&mut self, cells: usize) -> Result<(), DiffFailure> {
        self.trace_cells = self
            .trace_cells
            .checked_add(cells)
            .filter(|used| *used <= self.max_trace_cells)
            .ok_or(DiffFailure::ResourceLimit {
                phase: MergePhase::Myers,
                budget: MergeBudget::TraceCells,
            })?;
        Ok(())
    }

    fn release_trace(&mut self, cells: usize) {
        self.trace_cells = self.trace_cells.saturating_sub(cells);
    }
}

#[derive(Debug, Clone, Copy)]
struct Region {
    base_start: usize,
    base_end: usize,
    modified_start: usize,
    modified_end: usize,
    depth: usize,
}

pub(super) fn anchored_diff_changes<'a>(
    base: &[&'a str],
    modified: &[&'a str],
    budget: &mut WorkBudget,
) -> Result<Vec<LineChange<'a>>, DiffFailure> {
    let mut matches = Vec::new();
    collect_matches(
        base,
        modified,
        Region {
            base_start: 0,
            base_end: base.len(),
            modified_start: 0,
            modified_end: modified.len(),
            depth: 0,
        },
        budget,
        &mut matches,
    )?;
    matches.sort_unstable();
    validate_matches(base, modified, &matches)?;
    Ok(changes_from_matches(base, modified, &matches))
}

fn collect_matches(
    base: &[&str],
    modified: &[&str],
    mut region: Region,
    budget: &mut WorkBudget,
    matches: &mut Vec<(usize, usize)>,
) -> Result<(), DiffFailure> {
    if region.depth > budget.max_anchor_depth {
        return Err(DiffFailure::ResourceLimit {
            phase: MergePhase::Anchors,
            budget: MergeBudget::AnchorDepth,
        });
    }

    while region.base_start < region.base_end
        && region.modified_start < region.modified_end
        && base[region.base_start] == modified[region.modified_start]
    {
        budget.consume_steps(1, MergePhase::Anchors)?;
        matches.push((region.base_start, region.modified_start));
        region.base_start += 1;
        region.modified_start += 1;
    }

    let mut suffix = Vec::new();
    while region.base_start < region.base_end
        && region.modified_start < region.modified_end
        && base[region.base_end - 1] == modified[region.modified_end - 1]
    {
        budget.consume_steps(1, MergePhase::Anchors)?;
        region.base_end -= 1;
        region.modified_end -= 1;
        suffix.push((region.base_end, region.modified_end));
    }

    if region.base_start < region.base_end && region.modified_start < region.modified_end {
        let anchors = unique_anchors(base, modified, region, budget)?;
        if anchors.is_empty() {
            budget.myers_regions += 1;
            matches.extend(myers_matches(base, modified, region, budget)?);
        } else {
            budget.anchor_count = budget.anchor_count.checked_add(anchors.len()).ok_or(
                DiffFailure::ResourceLimit {
                    phase: MergePhase::Anchors,
                    budget: MergeBudget::WorkSteps,
                },
            )?;
            let mut base_cursor = region.base_start;
            let mut modified_cursor = region.modified_start;
            for (base_anchor, modified_anchor) in anchors {
                collect_matches(
                    base,
                    modified,
                    Region {
                        base_start: base_cursor,
                        base_end: base_anchor,
                        modified_start: modified_cursor,
                        modified_end: modified_anchor,
                        depth: region.depth + 1,
                    },
                    budget,
                    matches,
                )?;
                matches.push((base_anchor, modified_anchor));
                base_cursor = base_anchor + 1;
                modified_cursor = modified_anchor + 1;
            }
            collect_matches(
                base,
                modified,
                Region {
                    base_start: base_cursor,
                    base_end: region.base_end,
                    modified_start: modified_cursor,
                    modified_end: region.modified_end,
                    depth: region.depth + 1,
                },
                budget,
                matches,
            )?;
        }
    }

    suffix.reverse();
    matches.extend(suffix);
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct Occurrence {
    count: usize,
    index: usize,
}

fn unique_anchors(
    base: &[&str],
    modified: &[&str],
    region: Region,
    budget: &mut WorkBudget,
) -> Result<Vec<(usize, usize)>, DiffFailure> {
    let mut base_occurrences = BTreeMap::<&str, Occurrence>::new();
    let mut modified_occurrences = BTreeMap::<&str, Occurrence>::new();
    for (index, line) in base
        .iter()
        .enumerate()
        .take(region.base_end)
        .skip(region.base_start)
    {
        budget.consume_steps(1, MergePhase::Anchors)?;
        let occurrence = base_occurrences.entry(line).or_default();
        occurrence.count = occurrence.count.saturating_add(1);
        occurrence.index = index;
    }
    for (index, line) in modified
        .iter()
        .enumerate()
        .take(region.modified_end)
        .skip(region.modified_start)
    {
        budget.consume_steps(1, MergePhase::Anchors)?;
        let occurrence = modified_occurrences.entry(line).or_default();
        occurrence.count = occurrence.count.saturating_add(1);
        occurrence.index = index;
    }

    let candidates = base
        .iter()
        .enumerate()
        .take(region.base_end)
        .skip(region.base_start)
        .filter_map(|(base_index, line)| {
            let base_occurrence = base_occurrences.get(line)?;
            let modified_occurrence = modified_occurrences.get(line)?;
            (base_occurrence.count == 1 && modified_occurrence.count == 1)
                .then_some((base_index, modified_occurrence.index))
        })
        .collect::<Vec<_>>();
    budget.consume_steps(candidates.len(), MergePhase::Anchors)?;
    Ok(longest_increasing_anchors(&candidates))
}

fn longest_increasing_anchors(candidates: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut tail_values = Vec::<usize>::new();
    let mut tail_candidates = Vec::<usize>::new();
    let mut predecessors = vec![None; candidates.len()];

    for (candidate_index, &(_, modified_index)) in candidates.iter().enumerate() {
        let position = tail_values.partition_point(|tail| *tail < modified_index);
        if position > 0 {
            predecessors[candidate_index] = Some(tail_candidates[position - 1]);
        }
        if position == tail_values.len() {
            tail_values.push(modified_index);
            tail_candidates.push(candidate_index);
        } else if modified_index < tail_values[position] {
            tail_values[position] = modified_index;
            tail_candidates[position] = candidate_index;
        }
    }

    let mut selected = Vec::with_capacity(tail_values.len());
    let mut cursor = tail_candidates.last().copied();
    while let Some(candidate_index) = cursor {
        selected.push(candidates[candidate_index]);
        cursor = predecessors[candidate_index];
    }
    selected.reverse();
    selected
}

fn myers_matches(
    base: &[&str],
    modified: &[&str],
    region: Region,
    budget: &mut WorkBudget,
) -> Result<Vec<(usize, usize)>, DiffFailure> {
    let base_region = &base[region.base_start..region.base_end];
    let modified_region = &modified[region.modified_start..region.modified_end];
    let max_distance =
        base_region
            .len()
            .checked_add(modified_region.len())
            .ok_or(DiffFailure::ResourceLimit {
                phase: MergePhase::Myers,
                budget: MergeBudget::TraceCells,
            })?;
    let frontier_len = max_distance
        .checked_mul(2)
        .and_then(|value| value.checked_add(3))
        .ok_or(DiffFailure::ResourceLimit {
            phase: MergePhase::Myers,
            budget: MergeBudget::TraceCells,
        })?;
    let offset = max_distance + 1;
    let mut frontier = vec![-1_isize; frontier_len];
    frontier[offset + 1] = 0;
    let mut trace = Vec::new();

    for distance in 0..=max_distance {
        budget.consume_steps(distance.saturating_add(1), MergePhase::Myers)?;
        for diagonal in (-(distance as isize)..=distance as isize).step_by(2) {
            let index = diagonal_index(offset, diagonal)?;
            let from_down = diagonal == -(distance as isize)
                || (diagonal != distance as isize && frontier[index - 1] < frontier[index + 1]);
            let mut base_index = if from_down {
                frontier[index + 1]
            } else {
                frontier[index - 1] + 1
            };
            let mut modified_index = base_index - diagonal;
            while base_index >= 0
                && modified_index >= 0
                && (base_index as usize) < base_region.len()
                && (modified_index as usize) < modified_region.len()
                && base_region[base_index as usize] == modified_region[modified_index as usize]
            {
                budget.consume_steps(1, MergePhase::Myers)?;
                base_index += 1;
                modified_index += 1;
            }
            frontier[index] = base_index;
            if base_index as usize == base_region.len()
                && modified_index as usize == modified_region.len()
            {
                budget.reserve_trace(frontier_len)?;
                trace.push(frontier.clone());
                let cells = trace.len() * frontier_len;
                let result = backtrack_matches(
                    &trace,
                    distance,
                    offset,
                    region.base_start,
                    region.modified_start,
                    base_region.len(),
                    modified_region.len(),
                );
                budget.release_trace(cells);
                return result;
            }
        }
        budget.reserve_trace(frontier_len)?;
        trace.push(frontier.clone());
    }
    Err(DiffFailure::InvalidEditScript)
}

fn diagonal_index(offset: usize, diagonal: isize) -> Result<usize, DiffFailure> {
    (offset as isize)
        .checked_add(diagonal)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(DiffFailure::InvalidEditScript)
}

fn backtrack_matches(
    trace: &[Vec<isize>],
    distance: usize,
    offset: usize,
    base_offset: usize,
    modified_offset: usize,
    base_len: usize,
    modified_len: usize,
) -> Result<Vec<(usize, usize)>, DiffFailure> {
    let mut base_index = base_len as isize;
    let mut modified_index = modified_len as isize;
    let mut matches = Vec::new();

    for current_distance in (1..=distance).rev() {
        let previous = trace
            .get(current_distance - 1)
            .ok_or(DiffFailure::InvalidEditScript)?;
        let diagonal = base_index - modified_index;
        let index = diagonal_index(offset, diagonal)?;
        let from_down = diagonal == -(current_distance as isize)
            || (diagonal != current_distance as isize && previous[index - 1] < previous[index + 1]);
        let previous_diagonal = if from_down {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let previous_index = diagonal_index(offset, previous_diagonal)?;
        let previous_base = previous[previous_index];
        let previous_modified = previous_base - previous_diagonal;
        while base_index > previous_base && modified_index > previous_modified {
            base_index -= 1;
            modified_index -= 1;
            matches.push((
                base_offset + base_index as usize,
                modified_offset + modified_index as usize,
            ));
        }
        if from_down {
            modified_index -= 1;
        } else {
            base_index -= 1;
        }
    }
    while base_index > 0 && modified_index > 0 {
        base_index -= 1;
        modified_index -= 1;
        matches.push((
            base_offset + base_index as usize,
            modified_offset + modified_index as usize,
        ));
    }
    matches.reverse();
    Ok(matches)
}

fn validate_matches(
    base: &[&str],
    modified: &[&str],
    matches: &[(usize, usize)],
) -> Result<(), DiffFailure> {
    let mut previous = None;
    for &(base_index, modified_index) in matches {
        if base.get(base_index) != modified.get(modified_index) {
            return Err(DiffFailure::InvalidEditScript);
        }
        if previous.is_some_and(|(previous_base, previous_modified)| {
            previous_base >= base_index || previous_modified >= modified_index
        }) {
            return Err(DiffFailure::InvalidEditScript);
        }
        previous = Some((base_index, modified_index));
    }
    Ok(())
}

fn changes_from_matches<'a>(
    base: &[&str],
    modified: &[&'a str],
    matches: &[(usize, usize)],
) -> Vec<LineChange<'a>> {
    let mut changes = Vec::new();
    let mut previous_base = 0;
    let mut previous_modified = 0;
    for &(matched_base, matched_modified) in matches {
        if previous_base != matched_base || previous_modified != matched_modified {
            changes.push(LineChange {
                base_start: previous_base,
                base_end: matched_base,
                modified_start: previous_modified,
                modified_end: matched_modified,
                replacement: modified[previous_modified..matched_modified].to_vec(),
            });
        }
        previous_base = matched_base + 1;
        previous_modified = matched_modified + 1;
    }
    if previous_base != base.len() || previous_modified != modified.len() {
        changes.push(LineChange {
            base_start: previous_base,
            base_end: base.len(),
            modified_start: previous_modified,
            modified_end: modified.len(),
            replacement: modified[previous_modified..].to_vec(),
        });
    }
    expand_ambiguous_alignments(base, modified, changes)
}

fn expand_ambiguous_alignments<'a>(
    base: &[&str],
    modified: &[&'a str],
    changes: Vec<LineChange<'a>>,
) -> Vec<LineChange<'a>> {
    let mut expanded = changes
        .into_iter()
        .map(|mut change| {
            let mut left = (
                change.base_start,
                change.base_end,
                change.modified_start,
                change.modified_end,
            );
            while can_shift_left(base, modified, left) {
                left = (left.0 - 1, left.1 - 1, left.2 - 1, left.3 - 1);
            }

            let mut right = (
                change.base_start,
                change.base_end,
                change.modified_start,
                change.modified_end,
            );
            while can_shift_right(base, modified, right) {
                right = (right.0 + 1, right.1 + 1, right.2 + 1, right.3 + 1);
            }

            change.base_start = left.0;
            change.base_end = right.1;
            change.modified_start = left.2;
            change.modified_end = right.3;
            change.replacement = modified[change.modified_start..change.modified_end].to_vec();
            change
        })
        .collect::<Vec<_>>();
    expanded.sort_by_key(|change| (change.base_start, change.modified_start));

    let mut canonical = Vec::<LineChange<'a>>::new();
    for change in expanded {
        if let Some(previous) = canonical.last_mut()
            && (change.base_start < previous.base_end
                || change.modified_start < previous.modified_end)
        {
            previous.base_end = previous.base_end.max(change.base_end);
            previous.modified_end = previous.modified_end.max(change.modified_end);
            previous.replacement =
                modified[previous.modified_start..previous.modified_end].to_vec();
            continue;
        }
        canonical.push(change);
    }
    canonical
}

fn can_shift_left(
    base: &[&str],
    modified: &[&str],
    (base_start, base_end, modified_start, modified_end): (usize, usize, usize, usize),
) -> bool {
    base_start > 0
        && base_end > 0
        && modified_start > 0
        && modified_end > 0
        && base[base_start - 1] == modified[modified_start - 1]
        && base[base_end - 1] == modified[modified_end - 1]
}

fn can_shift_right(
    base: &[&str],
    modified: &[&str],
    (base_start, base_end, modified_start, modified_end): (usize, usize, usize, usize),
) -> bool {
    base_start < base.len()
        && base_end < base.len()
        && modified_start < modified.len()
        && modified_end < modified.len()
        && base[base_start] == modified[modified_start]
        && base[base_end] == modified[modified_end]
}
