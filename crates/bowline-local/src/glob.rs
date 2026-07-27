// The glob matcher builds an O(pattern bytes * path bytes) DP table. Keep
// project config patterns and merge-dispatch paths bounded to this value.
pub(crate) const MAX_GLOB_MATCH_BYTES: usize = 1024;

pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_matches_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_matches_bytes(pattern: &[u8], text: &[u8]) -> bool {
    // The bound is enforced here rather than at each call site so no caller can
    // hand the matcher an unbounded DP table by writing a long `.bowlineignore`
    // line. An over-long pattern never matches instead of allocating.
    if pattern.len() > MAX_GLOB_MATCH_BYTES || text.len() > MAX_GLOB_MATCH_BYTES {
        return false;
    }
    let stride = text.len() + 1;
    // One flat allocation instead of `pattern.len() + 1` separate row vectors:
    // `classify_path` runs this per candidate per rule per path in the scan hot
    // loop.
    let mut table = vec![false; stride * (pattern.len() + 1)];
    table[pattern.len() * stride + text.len()] = true;
    for pattern_index in (0..pattern.len()).rev() {
        let row = pattern_index * stride;
        let next_row = row + stride;
        if pattern[pattern_index] == b'*'
            && pattern.get(pattern_index + 1) == Some(&b'*')
            && double_star_is_recursive(pattern, pattern_index)
        {
            fill_double_star_row(pattern, text, pattern_index, stride, &mut table);
            continue;
        }
        for text_index in (0..=text.len()).rev() {
            table[row + text_index] = match pattern[pattern_index] {
                b'*' => {
                    table[next_row + text_index]
                        || (text_index < text.len()
                            && text[text_index] != b'/'
                            && table[row + text_index + 1])
                }
                b'?' => {
                    text_index < text.len()
                        && text[text_index] != b'/'
                        && table[next_row + text_index + 1]
                }
                byte => text.get(text_index) == Some(&byte) && table[next_row + text_index + 1],
            };
        }
    }
    table[0]
}

fn double_star_is_recursive(pattern: &[u8], pattern_index: usize) -> bool {
    let starts_segment = pattern_index == 0 || pattern.get(pattern_index - 1) == Some(&b'/');
    let next_index = pattern_index + 2;
    let ends_segment = next_index == pattern.len() || pattern.get(next_index) == Some(&b'/');
    starts_segment && ends_segment
}

fn fill_double_star_row(
    pattern: &[u8],
    text: &[u8],
    pattern_index: usize,
    stride: usize,
    table: &mut [bool],
) {
    let row = pattern_index * stride;
    let next_pattern_index = pattern_index + 2;
    if pattern.get(next_pattern_index) == Some(&b'/') {
        let after_slash_row = (next_pattern_index + 1) * stride;
        let mut later_segment_matches = false;
        for text_index in (0..=text.len()).rev() {
            if text_index < text.len() && text[text_index] == b'/' {
                later_segment_matches |= table[row + text_index + 1];
            }
            table[row + text_index] = table[after_slash_row + text_index] || later_segment_matches;
        }
        return;
    }
    table[row..row + stride].fill(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching_supports_project_paths() {
        assert!(glob_matches("*.ipynb", "analysis.ipynb"));
        assert!(glob_matches("notebooks/*.ipynb", "notebooks/run.ipynb"));
        assert!(!glob_matches("notebooks/*.ipynb", "src/run.ipynb"));
        assert!(!glob_matches("*.ipynb", "vendored/dep/run.ipynb"));
        assert!(glob_matches("**/*.ipynb", "vendored/dep/run.ipynb"));
        assert!(glob_matches("**/*.ipynb", "analysis.ipynb"));
        assert!(!glob_matches("?.ipynb", "a/run.ipynb"));
        assert!(!glob_matches("a?b", "a/b"));
        assert!(!glob_matches("src/?ain.rs", "src/x/ain.rs"));
        assert!(glob_matches("a/**/run.ipynb", "a/run.ipynb"));
        assert!(glob_matches("a/**/run.ipynb", "a/b/c/run.ipynb"));
        assert!(!glob_matches(
            "notebooks**.ipynb",
            "notebooks/deep/run.ipynb"
        ));
        assert!(!glob_matches("data**", "data/deep/blob.bin"));
        assert!(glob_matches("notebooks**.ipynb", "notebooks-v1.ipynb"));
    }
}
