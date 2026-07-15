// The glob matcher builds an O(pattern bytes * path bytes) DP table. Keep
// project config patterns and merge-dispatch paths bounded to this value.
pub(crate) const MAX_GLOB_MATCH_BYTES: usize = 1024;

pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_matches_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_matches_bytes(pattern: &[u8], text: &[u8]) -> bool {
    let mut table = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    table[pattern.len()][text.len()] = true;
    for pattern_index in (0..pattern.len()).rev() {
        if pattern[pattern_index] == b'*'
            && pattern.get(pattern_index + 1) == Some(&b'*')
            && double_star_is_recursive(pattern, pattern_index)
        {
            fill_double_star_row(pattern, text, pattern_index, &mut table);
            continue;
        }
        for text_index in (0..=text.len()).rev() {
            table[pattern_index][text_index] = match pattern[pattern_index] {
                b'*' => {
                    table[pattern_index + 1][text_index]
                        || (text_index < text.len()
                            && text[text_index] != b'/'
                            && table[pattern_index][text_index + 1])
                }
                b'?' => {
                    text_index < text.len()
                        && text[text_index] != b'/'
                        && table[pattern_index + 1][text_index + 1]
                }
                byte => {
                    text.get(text_index) == Some(&byte) && table[pattern_index + 1][text_index + 1]
                }
            };
        }
    }
    table[0][0]
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
    table: &mut [Vec<bool>],
) {
    let next_pattern_index = pattern_index + 2;
    if pattern.get(next_pattern_index) == Some(&b'/') {
        let after_slash_index = next_pattern_index + 1;
        let mut later_segment_matches = false;
        for text_index in (0..=text.len()).rev() {
            if text_index < text.len() && text[text_index] == b'/' {
                later_segment_matches |= table[pattern_index][text_index + 1];
            }
            table[pattern_index][text_index] =
                table[after_slash_index][text_index] || later_segment_matches;
        }
        return;
    }
    table[pattern_index].fill(true);
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
