use bowline_core::policy::{MaterializationMode, PathClassification};

use super::{PathPolicyDecision, UserPolicy};

pub(crate) fn policy_prunes_subtree(decision: &PathPolicyDecision) -> bool {
    matches!(
        decision.classification,
        PathClassification::Dependency
            | PathClassification::Generated
            | PathClassification::Cache
            | PathClassification::LocalOnly
            | PathClassification::Blocked
    )
}

pub(crate) fn policy_syncs_workspace_state(decision: &PathPolicyDecision) -> bool {
    matches!(
        decision.mode,
        MaterializationMode::WorkspaceSync
            | MaterializationMode::EncryptedSync
            | MaterializationMode::Lazy
            | MaterializationMode::ProjectEnv
    )
}

pub fn policy_should_recurse(
    decision: &PathPolicyDecision,
    user_policy: &UserPolicy,
    path: &str,
) -> bool {
    user_policy.has_include_below(path) || !policy_prunes_subtree(decision)
}
