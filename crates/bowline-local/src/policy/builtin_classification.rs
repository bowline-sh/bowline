//! Classification from the built-in rules alone, before any `.bowlineignore`
//! policy is consulted: the materialization-temp, work-view, git-state, env,
//! secret, dependency, generated and large-file name lists, and the
//! `PathPolicyDecision` each one produces.

use bowline_core::{
    git_paths::is_git_derivable_volatile_path,
    git_worktree_link::worktree_link_file,
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::NamespaceEntryKind,
};

use super::{PathPolicyDecision, is_work_view_namespace_path};

const LARGE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Whether the dependency/generated *name* lists apply to this classification.
/// They are waived only for a path an explicit `!`-include named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NameHeuristics {
    Applied,
    Waived,
}

pub(super) fn classify_builtin(
    path: &str,
    is_dir: bool,
    byte_len: Option<u64>,
    heuristics: NameHeuristics,
) -> PathPolicyDecision {
    let parts = path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let name = parts.last().copied().unwrap_or("");

    if is_materialization_temp_path(&parts) {
        return decision(
            PathClassification::LocalOnly,
            MaterializationMode::LocalOnly,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        );
    }

    if is_work_view_namespace_path(path) {
        return decision(
            PathClassification::LocalOnly,
            MaterializationMode::LocalOnly,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        );
    }

    if is_portable_git_worktree_link_policy_path(path, is_dir) {
        return git_opaque_state_decision();
    }

    if is_git_transient_path(&parts) {
        return decision(
            PathClassification::LocalOnly,
            MaterializationMode::LocalOnly,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        );
    }

    if parts.contains(&".git") {
        return git_opaque_state_decision();
    }

    if heuristics == NameHeuristics::Applied {
        if is_dependency_path(&parts) {
            return decision(
                PathClassification::Dependency,
                MaterializationMode::LocalRegenerate,
                vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            );
        }

        if is_generated_path(&parts) {
            return decision(
                generated_classification(&parts),
                generated_mode(&parts),
                vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            );
        }
    }

    if is_project_env_name(name) {
        return decision(
            PathClassification::ProjectEnv,
            MaterializationMode::ProjectEnv,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        );
    }

    if is_secret_name(name) {
        return decision(
            PathClassification::SecretLooking,
            MaterializationMode::EncryptedSync,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        );
    }

    if byte_len.is_some_and(|len| len >= LARGE_FILE_BYTES) {
        return decision(
            PathClassification::LargeFile,
            MaterializationMode::Lazy,
            vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        );
    }

    decision(
        PathClassification::WorkspaceSync,
        MaterializationMode::WorkspaceSync,
        vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
    )
}

fn decision(
    classification: PathClassification,
    mode: MaterializationMode,
    access: Vec<AccessFlag>,
) -> PathPolicyDecision {
    PathPolicyDecision {
        classification,
        mode,
        access,
    }
}

pub(super) fn preserves_safety_classification(classification: PathClassification) -> bool {
    matches!(
        classification,
        PathClassification::ProjectEnv
            | PathClassification::SecretLooking
            | PathClassification::Blocked
            | PathClassification::Dependency
            | PathClassification::Generated
            | PathClassification::Cache
    )
}

fn is_git_transient_path(parts: &[&str]) -> bool {
    is_git_derivable_volatile_path(&parts.join("/"))
}

fn is_portable_git_worktree_link_policy_path(path: &str, is_dir: bool) -> bool {
    !is_dir && worktree_link_file(path, NamespaceEntryKind::File).is_some()
}

fn git_opaque_state_decision() -> PathPolicyDecision {
    decision(
        PathClassification::WorkspaceSync,
        MaterializationMode::EncryptedSync,
        vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
    )
}

fn is_materialization_temp_path(parts: &[&str]) -> bool {
    parts
        .iter()
        .any(|part| part.starts_with(".bowline-materialize-") && part.ends_with(".tmp"))
}

pub(crate) fn is_project_env_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env")
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "id_rsa"
        || lower == "id_dsa"
        || lower == "id_ed25519"
        || lower.contains("private_key")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
}

fn is_dependency_path(parts: &[&str]) -> bool {
    parts.iter().any(|part| is_dependency_name(part))
}

fn is_generated_path(parts: &[&str]) -> bool {
    parts.iter().any(|part| is_generated_name(part))
}

pub(super) fn is_dependency_name(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | ".pnpm-store" | ".yarn" | ".venv" | "venv"
    )
}

pub(super) fn is_generated_name(name: &str) -> bool {
    matches!(
        name,
        ".next"
            | ".nuxt"
            | ".svelte-kit"
            | "dist"
            | "build"
            | "target"
            | "__pycache__"
            | ".pytest_cache"
            | ".turbo"
            | ".cache"
            | "coverage"
            | "out"
    )
}

fn generated_classification(parts: &[&str]) -> PathClassification {
    if parts
        .iter()
        .any(|part| matches!(*part, ".cache" | ".pytest_cache"))
    {
        PathClassification::Cache
    } else {
        PathClassification::Generated
    }
}

fn generated_mode(parts: &[&str]) -> MaterializationMode {
    if parts
        .iter()
        .any(|part| matches!(*part, ".cache" | ".pytest_cache"))
    {
        MaterializationMode::LocalCache
    } else {
        MaterializationMode::LocalRegenerate
    }
}
