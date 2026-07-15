fn merge_bytes(base: &[u8], local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
    let candidate = if local == remote {
        local
    } else if local == base {
        remote
    } else if remote == base {
        local
    } else {
        return None;
    };

    looks_like_json(candidate).then(|| candidate.to_vec())
}

fn looks_like_json(bytes: &[u8]) -> bool {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return false;
    }
    let (open, close) = match (trimmed.first(), trimmed.last()) {
        (Some(b'{'), Some(b'}')) => (b'{', b'}'),
        (Some(b'['), Some(b']')) => (b'[', b']'),
        _ => return false,
    };

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

    !in_string && stack.is_empty() && trimmed[0] == open && *trimmed.last().unwrap() == close
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
    read_guest_bytes(candidate_ptr, candidate_len).is_some_and(looks_like_json) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exactly_one_valid_json_side() {
        let base = include_bytes!("../fixtures/base.json");
        let local = include_bytes!("../fixtures/local.json");
        let remote = include_bytes!("../fixtures/remote.json");
        let expected = include_bytes!("../fixtures/expected.json");

        assert_eq!(
            merge_bytes(base, local, remote).as_deref(),
            Some(&expected[..])
        );
    }

    #[test]
    fn refuses_invalid_changed_side() {
        let base = include_bytes!("../fixtures/base.json");
        let local = include_bytes!("../fixtures/invalid-local.json");
        let remote = include_bytes!("../fixtures/remote.json");

        assert!(merge_bytes(base, local, remote).is_none());
    }
}
