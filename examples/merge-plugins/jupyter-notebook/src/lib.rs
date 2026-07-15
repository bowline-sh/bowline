#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    id: String,
    start: usize,
    end: usize,
}

fn merge_bytes(base: &[u8], local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
    if local == remote {
        return validate_notebook(local).then(|| local.to_vec());
    }
    if local == base {
        return validate_notebook(remote).then(|| remote.to_vec());
    }
    if remote == base {
        return validate_notebook(local).then(|| local.to_vec());
    }

    let base_text = core::str::from_utf8(base).ok()?;
    let local_text = core::str::from_utf8(local).ok()?;
    let remote_text = core::str::from_utf8(remote).ok()?;
    let base_cells = parse_cells(base_text)?;
    let local_cells = parse_cells(local_text)?;
    let remote_cells = parse_cells(remote_text)?;
    if !same_cell_ids(&base_cells, &local_cells) || !same_cell_ids(&base_cells, &remote_cells) {
        return None;
    }

    let local_changed = changed_cell_ids(base_text, local_text, &base_cells, &local_cells)?;
    let remote_changed = changed_cell_ids(base_text, remote_text, &base_cells, &remote_cells)?;
    if local_changed
        .iter()
        .any(|id| remote_changed.iter().any(|remote_id| remote_id == id))
    {
        return None;
    }

    let mut merged = split_lines(remote_text);
    for id in local_changed {
        let local_cell = local_cells.iter().find(|cell| cell.id == id)?;
        let remote_cell = parse_cells(&join_lines(&merged))?
            .into_iter()
            .find(|cell| cell.id == id)?;
        let replacement = split_lines(local_text)[local_cell.start..=local_cell.end].to_vec();
        merged.splice(remote_cell.start..=remote_cell.end, replacement);
    }

    let output = join_lines(&merged).into_bytes();
    validate_notebook(&output).then_some(output)
}

fn validate_notebook(bytes: &[u8]) -> bool {
    let Ok(text) = core::str::from_utf8(bytes) else {
        return false;
    };
    if !looks_like_json(bytes) || !text.contains("\"cells\"") || !text.contains("\"nbformat\"") {
        return false;
    }
    let Some(cells) = parse_cells(text) else {
        return false;
    };
    !cells.is_empty()
        && cells.iter().enumerate().all(|(index, cell)| {
            cells
                .iter()
                .skip(index + 1)
                .all(|other| other.id != cell.id)
        })
}

fn parse_cells(text: &str) -> Option<Vec<Cell>> {
    let lines = split_lines(text);
    let mut cells = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(id) = extract_json_string_field(line, "id") else {
            continue;
        };
        let start = (0..=index)
            .rev()
            .find(|&candidate| lines[candidate].trim() == "{")?;
        let end = (index..lines.len()).find(|&candidate| {
            let trimmed = lines[candidate].trim();
            trimmed == "}" || trimmed == "},"
        })?;
        cells.push(Cell { id, start, end });
    }
    Some(cells)
}

fn same_cell_ids(left: &[Cell], right: &[Cell]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.id == right.id)
}

fn changed_cell_ids(
    base_text: &str,
    side_text: &str,
    base_cells: &[Cell],
    side_cells: &[Cell],
) -> Option<Vec<String>> {
    let base_lines = split_lines(base_text);
    let side_lines = split_lines(side_text);
    let mut changed = Vec::new();
    for (base, side) in base_cells.iter().zip(side_cells) {
        if base_lines.get(base.start..=base.end)? != side_lines.get(side.start..=side.end)? {
            changed.push(base.id.clone());
        }
    }
    Some(changed)
}

fn extract_json_string_field(line: &str, field: &str) -> Option<String> {
    let marker = format!("\"{field}\"");
    let field_start = line.find(&marker)?;
    let after_field = &line[field_start + marker.len()..];
    let colon = after_field.find(':')?;
    let after_colon = after_field[colon + 1..].trim_start();
    let value = after_colon.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

fn split_lines(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_string).collect()
}

fn join_lines(lines: &[String]) -> String {
    lines.concat()
}

fn looks_like_json(bytes: &[u8]) -> bool {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return false;
    }
    if !matches!(
        (trimmed.first(), trimmed.last()),
        (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']'))
    ) {
        return false;
    }

    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut last_significant = 0;
    for &byte in trimmed {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => stack.push(byte),
            b'}' => {
                if last_significant == b',' || stack.pop() != Some(b'{') {
                    return false;
                }
            }
            b']' => {
                if last_significant == b',' || stack.pop() != Some(b'[') {
                    return false;
                }
            }
            _ => {}
        }
        last_significant = byte;
    }
    !in_string && stack.is_empty()
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

fn read_guest_bytes(ptr: i32, len: i32) -> Option<&'static [u8]> {
    if ptr < 0 || len < 0 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(ptr as usize as *const u8, len as usize) })
}

fn finish_output(mut bytes: Vec<u8>) -> i64 {
    if bytes.len() > u32::MAX as usize {
        return -1;
    }
    let ptr = bytes.as_mut_ptr() as usize as u64;
    let len = bytes.len() as u64;
    core::mem::forget(bytes);
    ((ptr << 32) | len) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn bowline_alloc(len: i32) -> i32 {
    if len < 0 {
        return -1;
    }
    let mut bytes = vec![0_u8; len as usize];
    let ptr = bytes.as_mut_ptr() as usize;
    core::mem::forget(bytes);
    i32::try_from(ptr).unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn bowline_merge(
    base_ptr: i32,
    base_len: i32,
    local_ptr: i32,
    local_len: i32,
    remote_ptr: i32,
    remote_len: i32,
    _path_ptr: i32,
    _path_len: i32,
) -> i64 {
    let Some(base) = read_guest_bytes(base_ptr, base_len) else {
        return -1;
    };
    let Some(local) = read_guest_bytes(local_ptr, local_len) else {
        return -1;
    };
    let Some(remote) = read_guest_bytes(remote_ptr, remote_len) else {
        return -1;
    };
    merge_bytes(base, local, remote).map_or(-1, finish_output)
}

#[unsafe(no_mangle)]
pub extern "C" fn bowline_validate(
    candidate_ptr: i32,
    candidate_len: i32,
    _path_ptr: i32,
    _path_len: i32,
) -> i32 {
    read_guest_bytes(candidate_ptr, candidate_len).is_some_and(validate_notebook) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_disjoint_cell_edits() {
        let base = include_bytes!("../fixtures/base.ipynb");
        let local = include_bytes!("../fixtures/local.ipynb");
        let remote = include_bytes!("../fixtures/remote.ipynb");
        let expected = include_bytes!("../fixtures/expected.ipynb");

        assert_eq!(
            merge_bytes(base, local, remote).as_deref(),
            Some(&expected[..])
        );
    }

    #[test]
    fn refuses_same_cell_edits() {
        let base = include_bytes!("../fixtures/base.ipynb");
        let local = include_bytes!("../fixtures/conflict-local.ipynb");
        let remote = include_bytes!("../fixtures/conflict-remote.ipynb");

        assert!(merge_bytes(base, local, remote).is_none());
    }
}
