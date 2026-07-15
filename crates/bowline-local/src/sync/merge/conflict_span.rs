use super::super::{conflicts::ConflictSpan, line_merge::split_keep_terminator};

pub(super) fn conflict_span(path: &str, base: &[u8], local: &[u8], remote: &[u8]) -> ConflictSpan {
    let base_lines = line_vec(base);
    let local_lines = line_vec(local);
    let remote_lines = line_vec(remote);
    let prefix = common_prefix_len(&base_lines, &local_lines, &remote_lines);
    let suffix = common_suffix_len(&base_lines, &local_lines, &remote_lines, prefix);
    let (base_start_line, base_end_line) = span_line_range(&base_lines, prefix, suffix);
    let (local_start_line, local_end_line) = span_line_range(&local_lines, prefix, suffix);
    let (remote_start_line, remote_end_line) = span_line_range(&remote_lines, prefix, suffix);
    ConflictSpan {
        path: path.to_string(),
        base_start_line,
        base_end_line,
        local_start_line,
        local_end_line,
        remote_start_line,
        remote_end_line,
        base_context_hash: Some(span_context_hash(
            &base_lines,
            prefix,
            base_lines.len() - suffix,
        )),
        local_context_hash: Some(span_context_hash(
            &local_lines,
            prefix,
            local_lines.len() - suffix,
        )),
        remote_context_hash: Some(span_context_hash(
            &remote_lines,
            prefix,
            remote_lines.len() - suffix,
        )),
    }
}

fn line_vec(bytes: &[u8]) -> Vec<&str> {
    std::str::from_utf8(bytes)
        .map(split_keep_terminator)
        .unwrap_or_default()
}

fn common_prefix_len(base: &[&str], local: &[&str], remote: &[&str]) -> usize {
    let min_len = base.len().min(local.len()).min(remote.len());
    (0..min_len)
        .take_while(|index| base[*index] == local[*index] && base[*index] == remote[*index])
        .count()
}

fn common_suffix_len(base: &[&str], local: &[&str], remote: &[&str], prefix: usize) -> usize {
    let max_suffix = base
        .len()
        .saturating_sub(prefix)
        .min(local.len().saturating_sub(prefix))
        .min(remote.len().saturating_sub(prefix));
    (0..max_suffix)
        .take_while(|offset| {
            base[base.len() - 1 - offset] == local[local.len() - 1 - offset]
                && base[base.len() - 1 - offset] == remote[remote.len() - 1 - offset]
        })
        .count()
}

fn span_line_range(lines: &[&str], prefix: usize, suffix: usize) -> (u32, u32) {
    let start = (prefix + 1).min(lines.len().max(1)) as u32;
    let end = lines.len().saturating_sub(suffix).max(start as usize) as u32;
    (start, end)
}

fn span_context_hash(lines: &[&str], start_index: usize, end_exclusive: usize) -> String {
    let start = start_index.saturating_sub(3).min(lines.len());
    let end = (end_exclusive + 3).min(lines.len());
    super::super::short_hash(lines[start..end].iter().map(|line| line.as_bytes()))
}
