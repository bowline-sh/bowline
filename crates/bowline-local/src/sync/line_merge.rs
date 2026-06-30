pub(crate) fn merge_utf8_lines(base: &[u8], local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
    let base = std::str::from_utf8(base).ok()?;
    let local = std::str::from_utf8(local).ok()?;
    let remote = std::str::from_utf8(remote).ok()?;
    let base_lines = split_keep_terminator(base);
    let local_lines = split_keep_terminator(local);
    let remote_lines = split_keep_terminator(remote);

    if local_lines == remote_lines {
        return Some(local.as_bytes().to_vec());
    }
    if base_lines == local_lines {
        return Some(remote.as_bytes().to_vec());
    }
    if base_lines == remote_lines {
        return Some(local.as_bytes().to_vec());
    }

    let local_changes = diff_changes(&base_lines, &local_lines)?;
    let remote_changes = diff_changes(&base_lines, &remote_lines)?;
    let merged_lines = merge_line_changes(&base_lines, &local_changes, &remote_changes)?;
    let mut merged = String::new();
    for line in merged_lines {
        merged.push_str(line);
    }
    Some(merged.into_bytes())
}

pub(crate) fn split_keep_terminator(value: &str) -> Vec<&str> {
    value.split_inclusive('\n').collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineChange<'a> {
    base_start: usize,
    base_end: usize,
    replacement: Vec<&'a str>,
}

fn diff_changes<'a>(base: &[&'a str], modified: &[&'a str]) -> Option<Vec<LineChange<'a>>> {
    const MAX_LCS_CELLS: usize = 4_000_000;
    let rows = base.len().checked_add(1)?;
    let cols = modified.len().checked_add(1)?;
    if rows.checked_mul(cols)? > MAX_LCS_CELLS {
        return None;
    }

    let mut lcs = vec![0_u32; rows * cols];
    for base_index in (0..base.len()).rev() {
        for modified_index in (0..modified.len()).rev() {
            let current = base_index * cols + modified_index;
            lcs[current] = if base[base_index] == modified[modified_index] {
                1 + lcs[(base_index + 1) * cols + modified_index + 1]
            } else {
                lcs[(base_index + 1) * cols + modified_index]
                    .max(lcs[base_index * cols + modified_index + 1])
            };
        }
    }

    let mut matches = Vec::new();
    let mut base_index = 0;
    let mut modified_index = 0;
    while base_index < base.len() && modified_index < modified.len() {
        if base[base_index] == modified[modified_index] {
            matches.push((base_index, modified_index));
            base_index += 1;
            modified_index += 1;
        } else if lcs[(base_index + 1) * cols + modified_index]
            >= lcs[base_index * cols + modified_index + 1]
        {
            base_index += 1;
        } else {
            modified_index += 1;
        }
    }

    let mut changes = Vec::new();
    let mut previous_base = 0;
    let mut previous_modified = 0;
    for (matched_base, matched_modified) in matches {
        if previous_base != matched_base || previous_modified != matched_modified {
            changes.push(LineChange {
                base_start: previous_base,
                base_end: matched_base,
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
            replacement: modified[previous_modified..].to_vec(),
        });
    }

    Some(changes)
}

fn merge_line_changes<'a>(
    base: &[&'a str],
    local_changes: &[LineChange<'a>],
    remote_changes: &[LineChange<'a>],
) -> Option<Vec<&'a str>> {
    let mut merged = Vec::new();
    let mut cursor = 0;
    let mut local_index = 0;
    let mut remote_index = 0;

    while local_index < local_changes.len() || remote_index < remote_changes.len() {
        match (
            local_changes.get(local_index),
            remote_changes.get(remote_index),
        ) {
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
                        if next.base_start > end {
                            break;
                        }
                        end = end.max(next.base_end);
                        local_index += 1;
                        extended = true;
                    }
                    while let Some(next) = remote_changes.get(remote_index) {
                        if next.base_start > end {
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
                    return None;
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
                    return None;
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
    Some(merged)
}

fn append_single_change<'a>(
    base: &[&'a str],
    merged: &mut Vec<&'a str>,
    cursor: &mut usize,
    change: &LineChange<'a>,
) -> Option<()> {
    if *cursor > change.base_start {
        return None;
    }
    merged.extend_from_slice(&base[*cursor..change.base_start]);
    merged.extend(change.replacement.iter().copied());
    *cursor = change.base_end;
    Some(())
}

fn apply_change_group<'a>(
    base: &[&'a str],
    start: usize,
    end: usize,
    changes: &[LineChange<'a>],
) -> Option<Vec<&'a str>> {
    let mut lines = Vec::new();
    let mut cursor = start;
    for change in changes {
        if change.base_start < cursor || change.base_end > end {
            return None;
        }
        lines.extend_from_slice(&base[cursor..change.base_start]);
        lines.extend(change.replacement.iter().copied());
        cursor = change.base_end;
    }
    lines.extend_from_slice(&base[cursor..end]);
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::merge_utf8_lines;

    #[test]
    fn merges_adjacent_line_edits_without_duplicating_keys() {
        assert_eq!(
            merge_utf8_lines(b"a = 1\nb = 1\n", b"a = 2\nb = 1\n", b"a = 1\nb = 2\n",)
                .expect("merge"),
            b"a = 2\nb = 2\n",
        );
    }

    #[test]
    fn merges_insert_before_later_edit() {
        assert_eq!(
            merge_utf8_lines(
                b"a = 1\nb = 1\nc = 1\n",
                b"a = 1\ninserted = true\nb = 1\nc = 1\n",
                b"a = 1\nb = 1\nc = 2\n",
            )
            .expect("merge"),
            b"a = 1\ninserted = true\nb = 1\nc = 2\n",
        );
    }
}
