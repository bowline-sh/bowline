mod diff;

use diff::{DiffFailure, LineChange, WorkBudget, anchored_diff_changes};

const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 500_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_WORK_STEPS: usize = 16_000_000;
const MAX_TRACE_CELLS: usize = 8_000_000;
const MAX_ANCHOR_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextMergeConflictReason {
    IncompatibleOverlap,
}

impl TextMergeConflictReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::IncompatibleOverlap => "incompatible-overlap",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotTextReason {
    InvalidUtf8,
    BinaryControlByte,
}

impl NotTextReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidUtf8 => "invalid-utf8",
            Self::BinaryControlByte => "binary-control-byte",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergePhase {
    Input,
    Anchors,
    Myers,
    Output,
}

impl MergePhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Anchors => "anchors",
            Self::Myers => "myers",
            Self::Output => "output",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeBudget {
    InputBytes,
    LineBytes,
    LineCount,
    WorkSteps,
    TraceCells,
    AnchorDepth,
    OutputBytes,
}

impl MergeBudget {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InputBytes => "input-bytes",
            Self::LineBytes => "line-bytes",
            Self::LineCount => "line-count",
            Self::WorkSteps => "work-steps",
            Self::TraceCells => "trace-cells",
            Self::AnchorDepth => "anchor-depth",
            Self::OutputBytes => "output-bytes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InternalMergeError {
    InvalidEditScript,
}

impl InternalMergeError {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidEditScript => "invalid-edit-script",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TextMergeStats {
    pub(crate) base_lines: usize,
    pub(crate) local_lines: usize,
    pub(crate) remote_lines: usize,
    pub(crate) anchor_count: usize,
    pub(crate) myers_regions: usize,
    pub(crate) local_hunks: usize,
    pub(crate) remote_hunks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextOverlapSummary {
    pub(crate) base_start: usize,
    pub(crate) base_end: usize,
    pub(crate) local_replacement_lines: usize,
    pub(crate) remote_replacement_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextMergeOutcome {
    Clean {
        bytes: Vec<u8>,
        stats: TextMergeStats,
    },
    Conflict {
        reason: TextMergeConflictReason,
        overlaps: Vec<TextOverlapSummary>,
    },
    NotText {
        reason: NotTextReason,
    },
    ResourceLimit {
        phase: MergePhase,
        budget: MergeBudget,
    },
    InternalError {
        reason: InternalMergeError,
    },
}

pub(crate) fn merge_text_lines(base: &[u8], local: &[u8], remote: &[u8]) -> TextMergeOutcome {
    let base = match classify_text(base) {
        Ok(value) => value,
        Err(outcome) => return outcome,
    };
    let local = match classify_text(local) {
        Ok(value) => value,
        Err(outcome) => return outcome,
    };
    let remote = match classify_text(remote) {
        Ok(value) => value,
        Err(outcome) => return outcome,
    };
    if local == remote {
        return clean_fast_path(local.as_bytes());
    }
    if base == local {
        return clean_fast_path(remote.as_bytes());
    }
    if base == remote {
        return clean_fast_path(local.as_bytes());
    }
    let base_lines = split_keep_terminator(base);
    let local_lines = split_keep_terminator(local);
    let remote_lines = split_keep_terminator(remote);

    let mut budget = WorkBudget::new(MAX_WORK_STEPS, MAX_TRACE_CELLS, MAX_ANCHOR_DEPTH);
    let local_changes = match anchored_diff_changes(&base_lines, &local_lines, &mut budget) {
        Ok(changes) => changes,
        Err(failure) => return failure.into_outcome(),
    };
    let remote_changes = match anchored_diff_changes(&base_lines, &remote_lines, &mut budget) {
        Ok(changes) => changes,
        Err(failure) => return failure.into_outcome(),
    };
    let stats = TextMergeStats {
        base_lines: base_lines.len(),
        local_lines: local_lines.len(),
        remote_lines: remote_lines.len(),
        anchor_count: budget.anchor_count(),
        myers_regions: budget.myers_regions(),
        local_hunks: local_changes.len(),
        remote_hunks: remote_changes.len(),
    };
    match merge_line_changes(&base_lines, &local_changes, &remote_changes) {
        Ok(LineMergeResult::Clean(merged_lines)) => match render_lines(&merged_lines) {
            Ok(bytes) => TextMergeOutcome::Clean { bytes, stats },
            Err(outcome) => outcome,
        },
        Ok(LineMergeResult::Conflict(overlap)) => TextMergeOutcome::Conflict {
            reason: TextMergeConflictReason::IncompatibleOverlap,
            overlaps: vec![overlap],
        },
        Err(reason) => TextMergeOutcome::InternalError { reason },
    }
}

fn clean_fast_path(bytes: &[u8]) -> TextMergeOutcome {
    if bytes.len() > MAX_OUTPUT_BYTES {
        return TextMergeOutcome::ResourceLimit {
            phase: MergePhase::Output,
            budget: MergeBudget::OutputBytes,
        };
    }
    TextMergeOutcome::Clean {
        bytes: bytes.to_vec(),
        stats: TextMergeStats::default(),
    }
}

fn classify_text(bytes: &[u8]) -> Result<&str, TextMergeOutcome> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(TextMergeOutcome::ResourceLimit {
            phase: MergePhase::Input,
            budget: MergeBudget::InputBytes,
        });
    }
    if bytes.iter().copied().any(is_binary_control) {
        return Err(TextMergeOutcome::NotText {
            reason: NotTextReason::BinaryControlByte,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| TextMergeOutcome::NotText {
        reason: NotTextReason::InvalidUtf8,
    })?;
    let mut line_count = 0_usize;
    for line in text.split_inclusive('\n') {
        line_count = line_count
            .checked_add(1)
            .filter(|count| *count <= MAX_LINES)
            .ok_or(TextMergeOutcome::ResourceLimit {
                phase: MergePhase::Input,
                budget: MergeBudget::LineCount,
            })?;
        if line.len() > MAX_LINE_BYTES {
            return Err(TextMergeOutcome::ResourceLimit {
                phase: MergePhase::Input,
                budget: MergeBudget::LineBytes,
            });
        }
    }
    Ok(text)
}

fn is_binary_control(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f)
}

pub(crate) fn split_keep_terminator(value: &str) -> Vec<&str> {
    value.split_inclusive('\n').collect()
}

fn render_lines(lines: &[&str]) -> Result<Vec<u8>, TextMergeOutcome> {
    let output_len = lines.iter().try_fold(0_usize, |total, line| {
        total
            .checked_add(line.len())
            .filter(|sum| *sum <= MAX_OUTPUT_BYTES)
    });
    let Some(output_len) = output_len else {
        return Err(TextMergeOutcome::ResourceLimit {
            phase: MergePhase::Output,
            budget: MergeBudget::OutputBytes,
        });
    };
    let mut output = Vec::with_capacity(output_len);
    for line in lines {
        output.extend_from_slice(line.as_bytes());
    }
    Ok(output)
}

fn merge_line_changes<'a>(
    base: &[&'a str],
    local_changes: &[LineChange<'a>],
    remote_changes: &[LineChange<'a>],
) -> Result<LineMergeResult<'a>, InternalMergeError> {
    let mut merged = Vec::new();
    let mut cursor = 0;
    let mut local_index = 0;
    let mut remote_index = 0;

    while local_index < local_changes.len() || remote_index < remote_changes.len() {
        match (
            local_changes.get(local_index),
            remote_changes.get(remote_index),
        ) {
            (Some(local), Some(remote)) if same_insertion_point(local, remote) => {
                if local.replacement != remote.replacement {
                    return Ok(LineMergeResult::Conflict(overlap_summary(local, remote)));
                }
                append_single_change(base, &mut merged, &mut cursor, local)?;
                local_index += 1;
                remote_index += 1;
            }
            (Some(local), Some(remote)) if local.base_end <= remote.base_start => {
                append_single_change(base, &mut merged, &mut cursor, local)?;
                local_index += 1;
            }
            (Some(local), Some(remote)) if remote.base_end <= local.base_start => {
                append_single_change(base, &mut merged, &mut cursor, remote)?;
                remote_index += 1;
            }
            (Some(local), Some(remote)) => {
                let start = local.base_start.min(remote.base_start);
                let mut end = local.base_end.max(remote.base_end);
                let local_start = local_index;
                let remote_start = remote_index;

                local_index += 1;
                remote_index += 1;
                loop {
                    let mut extended = false;
                    while let Some(next) = local_changes.get(local_index) {
                        if !change_overlaps_span(next, start, end) {
                            break;
                        }
                        end = end.max(next.base_end);
                        local_index += 1;
                        extended = true;
                    }
                    while let Some(next) = remote_changes.get(remote_index) {
                        if !change_overlaps_span(next, start, end) {
                            break;
                        }
                        end = end.max(next.base_end);
                        remote_index += 1;
                        extended = true;
                    }
                    if !extended {
                        break;
                    }
                }

                if cursor > start {
                    return Err(InternalMergeError::InvalidEditScript);
                }
                merged.extend_from_slice(&base[cursor..start]);
                let local_chunk =
                    apply_change_group(base, start, end, &local_changes[local_start..local_index])?;
                let remote_chunk = apply_change_group(
                    base,
                    start,
                    end,
                    &remote_changes[remote_start..remote_index],
                )?;
                if local_chunk != remote_chunk {
                    return Ok(LineMergeResult::Conflict(TextOverlapSummary {
                        base_start: start,
                        base_end: end,
                        local_replacement_lines: local_chunk.len(),
                        remote_replacement_lines: remote_chunk.len(),
                    }));
                }
                merged.extend(local_chunk);
                cursor = end;
            }
            (Some(local), None) => {
                append_single_change(base, &mut merged, &mut cursor, local)?;
                local_index += 1;
            }
            (None, Some(remote)) => {
                append_single_change(base, &mut merged, &mut cursor, remote)?;
                remote_index += 1;
            }
            (None, None) => break,
        }
    }

    merged.extend_from_slice(&base[cursor..]);
    Ok(LineMergeResult::Clean(merged))
}

enum LineMergeResult<'a> {
    Clean(Vec<&'a str>),
    Conflict(TextOverlapSummary),
}

fn overlap_summary(local: &LineChange<'_>, remote: &LineChange<'_>) -> TextOverlapSummary {
    TextOverlapSummary {
        base_start: local.base_start.min(remote.base_start),
        base_end: local.base_end.max(remote.base_end),
        local_replacement_lines: local.replacement.len(),
        remote_replacement_lines: remote.replacement.len(),
    }
}

fn change_overlaps_span(change: &LineChange<'_>, start: usize, end: usize) -> bool {
    if change.base_start == change.base_end {
        change.base_start > start && change.base_start < end
    } else {
        change.base_start < end && change.base_end > start
    }
}

fn same_insertion_point(local: &LineChange<'_>, remote: &LineChange<'_>) -> bool {
    local.base_start == local.base_end
        && remote.base_start == remote.base_end
        && local.base_start == remote.base_start
}

fn append_single_change<'a>(
    base: &[&'a str],
    merged: &mut Vec<&'a str>,
    cursor: &mut usize,
    change: &LineChange<'a>,
) -> Result<(), InternalMergeError> {
    if *cursor > change.base_start || change.base_end > base.len() {
        return Err(InternalMergeError::InvalidEditScript);
    }
    merged.extend_from_slice(&base[*cursor..change.base_start]);
    merged.extend(change.replacement.iter().copied());
    *cursor = change.base_end;
    Ok(())
}

fn apply_change_group<'a>(
    base: &[&'a str],
    start: usize,
    end: usize,
    changes: &[LineChange<'a>],
) -> Result<Vec<&'a str>, InternalMergeError> {
    let mut lines = Vec::new();
    let mut cursor = start;
    for change in changes {
        if change.base_start < cursor || change.base_end > end {
            return Err(InternalMergeError::InvalidEditScript);
        }
        lines.extend_from_slice(&base[cursor..change.base_start]);
        lines.extend(change.replacement.iter().copied());
        cursor = change.base_end;
    }
    lines.extend_from_slice(&base[cursor..end]);
    Ok(lines)
}

impl DiffFailure {
    fn into_outcome(self) -> TextMergeOutcome {
        match self {
            Self::ResourceLimit { phase, budget } => {
                TextMergeOutcome::ResourceLimit { phase, budget }
            }
            Self::InvalidEditScript => TextMergeOutcome::InternalError {
                reason: InternalMergeError::InvalidEditScript,
            },
        }
    }
}

#[cfg(test)]
mod tests;
