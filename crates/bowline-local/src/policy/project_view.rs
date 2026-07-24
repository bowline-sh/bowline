use bowline_core::policy::{AccessFlag, MaterializationMode, PathClassification};

use super::{
    PathFacts, PathPolicyDecision, UserPolicy, classify_builtin, classify_path,
    normalize_relative_path, preserves_safety_classification,
};

/// Whether a path lives inside Bowline's local work-view materialization
/// namespace. These files are reconstructed from the durable encrypted
/// aux-index and overlay manifests; syncing the materialized tree itself would
/// duplicate Git/object state and let two devices echo competing view copies.
pub fn is_work_view_namespace_path(path: &str) -> bool {
    normalize_relative_path(path)
        .split('/')
        .any(|part| part == ".work")
}

pub(crate) fn classify_project_view_path(
    facts: &PathFacts,
    policy: &UserPolicy,
) -> PathPolicyDecision {
    let path = normalize_relative_path(&facts.relative_path);
    let mut parts = path.splitn(2, '/');
    let root = parts.next().unwrap_or("");
    if !matches!(root, ".work" | ".bowline" | ".bowline-meta") {
        return classify_path(facts, policy);
    }
    let projected = parts.next().unwrap_or("__project_directory__");
    let base = classify_builtin(projected, facts.is_dir, facts.byte_len);
    let Some(rule) = policy.match_rule(&path) else {
        return base;
    };
    if rule.include || preserves_safety_classification(base.classification) {
        return base;
    }
    PathPolicyDecision {
        classification: PathClassification::LocalOnly,
        mode: MaterializationMode::Ignore,
        access: vec![AccessFlag::HumanReadable],
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PathFacts, UserPolicy, classify_path};

    #[test]
    fn work_view_git_state_remains_local_only() {
        for path in [
            ".work/app/feature/.git",
            ".work/app/feature/.git/HEAD",
            "repo/.work/feature/.git/index",
        ] {
            let decision = classify_path(
                &PathFacts {
                    relative_path: path.to_string(),
                    is_dir: path.ends_with(".git"),
                    byte_len: None,
                },
                &UserPolicy::empty(),
            );
            assert_eq!(
                serde_json::to_value(decision.classification).expect("classification serializes"),
                "local-only",
                "{path}"
            );
            assert_eq!(
                serde_json::to_value(decision.mode).expect("mode serializes"),
                "local-only",
                "{path}"
            );
        }
    }
}
