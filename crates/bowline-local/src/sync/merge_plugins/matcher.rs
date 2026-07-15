pub(super) use crate::glob::{MAX_GLOB_MATCH_BYTES, glob_matches};

pub(super) fn policy_bound_matcher_version(version: &str, patterns: &[String]) -> String {
    let mut patterns = patterns
        .iter()
        .map(|pattern| normalize_workspace_match_path(pattern))
        .collect::<Vec<_>>();
    patterns.sort();
    let mut hasher = blake3::Hasher::new();
    for pattern in patterns {
        hasher.update(&(pattern.len() as u64).to_le_bytes());
        hasher.update(pattern.as_bytes());
    }
    format!("{version}+patterns:{}", hasher.finalize().to_hex())
}

pub(super) fn normalize_workspace_match_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
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
