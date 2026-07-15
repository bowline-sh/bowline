use std::{
    fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind,
        normalize_workspace_path,
    },
};

use crate::policy::{PathFacts, PathPolicyDecision, UserPolicy, classify_path};

use super::{
    WorkViewError,
    content_identity::FileIdentity,
    paths::{is_bowline_owned_namespace, is_source_control_metadata_path},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotExposurePlan {
    pub entries: Vec<NamespaceEntry>,
    pub policy_fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureSkipReason {
    AgentHidden,
    Blocked,
    MachineLocal,
    Regenerated,
    BowlineOwned,
    SourceControl,
    Symlink,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedExposureEntry {
    pub relative_path: String,
    pub reason: ExposureSkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedExposureEntry {
    pub entry: NamespaceEntry,
    pub relative_path: String,
    pub source_path: PathBuf,
    pub owner_only: bool,
    pub(super) source_identity: Option<FileIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewExposurePlan {
    pub entries: Vec<PlannedExposureEntry>,
    pub skipped: Vec<SkippedExposureEntry>,
    pub policy_fingerprint: String,
}

pub fn plan_live_tree_exposure(
    workspace_root: &Path,
    project_path: &str,
) -> Result<WorkViewExposurePlan, WorkViewError> {
    let project_path = normalize_workspace_path(project_path);
    let source_root = workspace_root.join(&project_path);
    let mut plan = WorkViewExposurePlan {
        entries: Vec::new(),
        skipped: Vec::new(),
        policy_fingerprint: String::new(),
    };
    walk_exposure(workspace_root, &source_root, &source_root, &mut plan)?;
    plan.entries
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    plan.skipped
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    plan.policy_fingerprint = policy_fingerprint(&plan);
    Ok(plan)
}

pub fn plan_snapshot_exposure(
    workspace_root: &Path,
    project_path: &str,
    entries: Vec<NamespaceEntry>,
) -> Result<SnapshotExposurePlan, WorkViewError> {
    let project_prefix = normalize_workspace_path(project_path);
    let mut exposed = Vec::new();
    let mut structural_candidates = Vec::new();
    for mut candidate in entries {
        let relative = candidate
            .path
            .strip_prefix(&project_prefix)
            .and_then(|path| path.strip_prefix('/'))
            .unwrap_or_default();
        let relative_path = Path::new(relative);
        if relative.is_empty()
            || is_bowline_owned_namespace(relative_path)
            || is_source_control_metadata_path(relative_path)
            || candidate.kind == NamespaceEntryKind::Symlink
        {
            continue;
        }
        let user_policy = UserPolicy::load_for_path(workspace_root, &candidate.path)?;
        let decision = classify_path(
            &PathFacts {
                relative_path: candidate.path.clone(),
                is_dir: candidate.kind == NamespaceEntryKind::Directory,
                byte_len: candidate.byte_len,
            },
            &user_policy,
        );
        if skipped_reason(&decision).is_some() {
            if candidate.kind == NamespaceEntryKind::Directory
                && user_policy.has_include_below(&candidate.path)
            {
                structural_candidates.push((candidate, decision.access));
            }
            continue;
        }
        candidate.classification = decision.classification;
        candidate.mode = decision.mode;
        candidate.access = decision.access;
        exposed.push(candidate);
    }
    for (mut candidate, access) in structural_candidates {
        let descendant_prefix = format!("{}/", candidate.path);
        if exposed
            .iter()
            .any(|entry| entry.path.starts_with(&descendant_prefix))
        {
            candidate.classification = PathClassification::WorkspaceSync;
            candidate.mode = MaterializationMode::StructureOnly;
            candidate.access = access;
            exposed.push(candidate);
        }
    }
    exposed.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(SnapshotExposurePlan {
        policy_fingerprint: entry_policy_fingerprint(exposed.iter()),
        entries: exposed,
    })
}

fn walk_exposure(
    workspace_root: &Path,
    source_root: &Path,
    path: &Path,
    plan: &mut WorkViewExposurePlan,
) -> Result<(), WorkViewError> {
    let mut children = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        plan_path(workspace_root, source_root, &child.path(), plan)?;
    }
    Ok(())
}

fn plan_path(
    workspace_root: &Path,
    source_root: &Path,
    path: &Path,
    plan: &mut WorkViewExposurePlan,
) -> Result<(), WorkViewError> {
    let relative = path
        .strip_prefix(source_root)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let relative_text = normalize_workspace_path(&relative.display().to_string());
    if is_bowline_owned_namespace(relative) {
        push_skip(plan, relative_text, ExposureSkipReason::BowlineOwned);
        return Ok(());
    }
    if is_source_control_metadata_path(relative) {
        push_skip(plan, relative_text, ExposureSkipReason::SourceControl);
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        push_skip(plan, relative_text, ExposureSkipReason::Symlink);
        return Ok(());
    }
    let workspace_path = path
        .strip_prefix(workspace_root)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let workspace_path = normalize_workspace_path(&workspace_path.display().to_string());
    let user_policy = UserPolicy::load_for_path(workspace_root, &workspace_path)?;
    let mut decision = classify_path(
        &PathFacts {
            relative_path: workspace_path.clone(),
            is_dir: metadata.is_dir(),
            byte_len: metadata.is_file().then_some(metadata.len()),
        },
        &user_policy,
    );
    if metadata.is_file()
        && user_policy.has_include_below(&workspace_path)
        && skipped_reason(&decision) == Some(ExposureSkipReason::Regenerated)
    {
        decision.classification = PathClassification::WorkspaceSync;
        decision.mode = MaterializationMode::WorkspaceSync;
    }
    if let Some(reason) = skipped_reason(&decision) {
        push_skip(plan, relative_text, reason);
        if metadata.is_dir() && user_policy.has_include_below(&workspace_path) {
            let entries_before = plan.entries.len();
            walk_exposure(workspace_root, source_root, path, plan)?;
            if plan.entries.len() > entries_before {
                plan.entries.push(structural_ancestor(
                    workspace_path,
                    relative.to_path_buf(),
                    path.to_path_buf(),
                    decision.access,
                ));
            }
        }
        return Ok(());
    }
    let kind = if metadata.is_dir() {
        NamespaceEntryKind::Directory
    } else if metadata.is_file() {
        NamespaceEntryKind::File
    } else {
        push_skip(plan, relative_text, ExposureSkipReason::Unsupported);
        return Ok(());
    };
    let owner_only =
        super::paths::is_owner_only_work_view_policy(decision.classification, decision.mode);
    plan.entries.push(PlannedExposureEntry {
        entry: NamespaceEntry {
            path: workspace_path,
            kind,
            classification: decision.classification,
            mode: decision.mode,
            access: decision.access,
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: metadata.is_file().then_some(metadata.len()),
            executability: file_executability(&metadata),
            hydration_state: HydrationState::Local,
        },
        relative_path: relative_text,
        source_path: path.to_path_buf(),
        owner_only,
        source_identity: metadata
            .is_file()
            .then(|| FileIdentity::from_metadata(&metadata))
            .transpose()?,
    });
    if metadata.is_dir() {
        walk_exposure(workspace_root, source_root, path, plan)?;
    }
    Ok(())
}

fn structural_ancestor(
    workspace_path: String,
    relative_path: PathBuf,
    source_path: PathBuf,
    access: Vec<AccessFlag>,
) -> PlannedExposureEntry {
    PlannedExposureEntry {
        entry: NamespaceEntry {
            path: workspace_path,
            kind: NamespaceEntryKind::Directory,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::StructureOnly,
            access,
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: None,
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        },
        relative_path: normalize_workspace_path(&relative_path.display().to_string()),
        source_path,
        owner_only: false,
        source_identity: None,
    }
}

fn push_skip(plan: &mut WorkViewExposurePlan, relative_path: String, reason: ExposureSkipReason) {
    plan.skipped.push(SkippedExposureEntry {
        relative_path,
        reason,
    });
}

fn skipped_reason(decision: &PathPolicyDecision) -> Option<ExposureSkipReason> {
    if decision.access.contains(&AccessFlag::AgentHidden) {
        return Some(ExposureSkipReason::AgentHidden);
    }
    match (decision.classification, decision.mode) {
        (PathClassification::Blocked, _) | (_, MaterializationMode::Blocked) => {
            Some(ExposureSkipReason::Blocked)
        }
        (PathClassification::LocalOnly, _) | (_, MaterializationMode::LocalOnly) => {
            Some(ExposureSkipReason::MachineLocal)
        }
        (
            PathClassification::Generated
            | PathClassification::Dependency
            | PathClassification::Cache,
            _,
        )
        | (
            _,
            MaterializationMode::LocalRegenerate
            | MaterializationMode::LocalCache
            | MaterializationMode::Ignore,
        ) => Some(ExposureSkipReason::Regenerated),
        (
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync | MaterializationMode::StructureOnly,
        )
        | (PathClassification::LargeFile, MaterializationMode::Lazy)
        | (
            PathClassification::ProjectEnv,
            MaterializationMode::ProjectEnv | MaterializationMode::EncryptedSync,
        ) => None,
        _ => Some(ExposureSkipReason::Unsupported),
    }
}

fn policy_fingerprint(plan: &WorkViewExposurePlan) -> String {
    entry_policy_fingerprint(plan.entries.iter().map(|planned| &planned.entry))
}

pub(crate) fn entry_policy_fingerprint<'a>(
    entries: impl IntoIterator<Item = &'a NamespaceEntry>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        hasher.update(entry.path.as_bytes());
        hasher.update(&[0]);
        let policy = serde_json::to_vec(&(entry.classification, entry.mode, &entry.access))
            .expect("work-view policy enums always serialize");
        hasher.update(&policy);
        hasher.update(&[0xff]);
    }
    format!("b3_{}", hasher.finalize().to_hex())
}

#[cfg(unix)]
fn file_executability(metadata: &fs::Metadata) -> FileExecutability {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o111 != 0 {
        FileExecutability::Executable
    } else {
        FileExecutability::Regular
    }
}

#[cfg(not(unix))]
fn file_executability(_metadata: &fs::Metadata) -> FileExecutability {
    FileExecutability::Regular
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_include_preserves_skipped_directory_as_structural_ancestor() {
        let temp = crate::workspace::TempWorkspace::new("work-view-exposure-include")
            .expect("temp workspace");
        temp.write_file(".bowlineignore", b"!project/node_modules/kept.js\n")
            .expect("policy");
        temp.write_file("project/node_modules/kept.js", b"kept\n")
            .expect("included dependency");
        temp.write_file("project/node_modules/skipped.js", b"skipped\n")
            .expect("skipped dependency");

        let plan = plan_live_tree_exposure(temp.root(), "project").expect("exposure plan");
        assert!(plan.entries.iter().any(|entry| {
            entry.relative_path == "node_modules"
                && entry.entry.classification == PathClassification::WorkspaceSync
                && entry.entry.mode == MaterializationMode::StructureOnly
        }));
        assert!(
            plan.entries
                .iter()
                .any(|entry| entry.relative_path == "node_modules/kept.js")
        );
        assert!(
            plan.entries
                .iter()
                .all(|entry| entry.relative_path != "node_modules/skipped.js")
        );
    }
}
