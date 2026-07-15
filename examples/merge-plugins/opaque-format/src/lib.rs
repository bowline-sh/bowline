const MAGIC: &[u8] = b"OPAQ\n";

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

    validate_opaque(candidate).then(|| candidate.to_vec())
}

fn validate_opaque(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
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
    read_guest_bytes(candidate_ptr, candidate_len).is_some_and(validate_opaque) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exactly_one_changed_side() {
        let base = include_bytes!("../fixtures/base.opaque");
        let local = include_bytes!("../fixtures/local.opaque");
        let remote = include_bytes!("../fixtures/remote.opaque");
        let expected = include_bytes!("../fixtures/expected.opaque");

        assert_eq!(
            merge_bytes(base, local, remote).as_deref(),
            Some(&expected[..])
        );
    }

    #[test]
    fn refuses_when_both_sides_changed() {
        let base = include_bytes!("../fixtures/base.opaque");
        let local = include_bytes!("../fixtures/both-local.opaque");
        let remote = include_bytes!("../fixtures/both-remote.opaque");

        assert!(merge_bytes(base, local, remote).is_none());
    }
}
