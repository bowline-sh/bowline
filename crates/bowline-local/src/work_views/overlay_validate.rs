use std::{collections::BTreeMap, path::Path};

use bowline_core::{
    work_views::WorkView,
    workspace_graph::{NamespaceEntry, NamespaceEntryKind},
};

use crate::{
    policy::{PathFacts, UserPolicy, classify_path},
    sync::paths::validate_case_folded_prefixes,
};

use super::{
    WorkViewError, WorkViewOverlaySyncError,
    overlay_wire::{OverlayManifest, OverlayOperation},
    paths::{
        is_bowline_owned_namespace, is_clean_accept_policy_eligible,
        is_ignored_clean_accept_policy, is_owner_only_work_view_policy,
        is_source_control_metadata_path, workspace_path_for_project_file,
    },
};

pub(super) fn validate_incoming_overlay(
    workspace_root: &Path,
    work_view: &WorkView,
    project_prefix: &str,
    exposed: &[NamespaceEntry],
    manifest: &OverlayManifest,
) -> Result<BTreeMap<String, bool>, WorkViewOverlaySyncError> {
    let mut namespace = BTreeMap::new();
    for exposed_entry in exposed {
        let relative = exposed_entry
            .path
            .strip_prefix(project_prefix)
            .map(|path| path.trim_start_matches('/'))
            .ok_or_else(|| WorkViewError::UnsafeWorkViewPath {
                path: exposed_entry.path.clone(),
                reason: "exposed base entry is outside its project prefix",
            })?;
        if !relative.is_empty() {
            namespace.insert(relative.to_string(), exposed_entry.kind);
        }
    }

    let mut owner_only_by_path = BTreeMap::new();
    for entry in manifest.operations() {
        let removed = match entry.operation {
            OverlayOperation::Delete => Some(entry.path.as_str()),
            OverlayOperation::Rename => entry.from.as_deref(),
            OverlayOperation::Create | OverlayOperation::Modify => None,
        };
        if let Some(source) = removed {
            let prefix = format!("{source}/");
            namespace.retain(|path, _| path != source && !path.starts_with(&prefix));
        }
        if entry.content.is_some() {
            let relative = Path::new(&entry.path);
            if is_bowline_owned_namespace(relative) || is_source_control_metadata_path(relative) {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: entry.path.clone(),
                    reason: "overlay destination is reserved or source-control metadata",
                }
                .into());
            }
            let workspace_path = workspace_path_for_project_file(work_view, relative);
            let policy = classify_path(
                &PathFacts {
                    relative_path: workspace_path.clone(),
                    is_dir: false,
                    byte_len: entry.content.as_ref().map(|content| content.byte_len),
                },
                &UserPolicy::load_for_path(workspace_root, &workspace_path)
                    .map_err(WorkViewError::from)?,
            );
            if is_ignored_clean_accept_policy(policy.classification, policy.mode)
                || !is_clean_accept_policy_eligible(policy.classification, policy.mode)
            {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: entry.path.clone(),
                    reason: "overlay destination is excluded by receiver policy",
                }
                .into());
            }
            owner_only_by_path.insert(
                entry.path.clone(),
                is_owner_only_work_view_policy(policy.classification, policy.mode),
            );
            namespace.insert(entry.path.clone(), NamespaceEntryKind::File);
        }
    }
    validate_overlay_namespace(&namespace)?;
    Ok(owner_only_by_path)
}

pub(super) fn validate_overlay_namespace(
    namespace: &BTreeMap<String, NamespaceEntryKind>,
) -> Result<(), WorkViewOverlaySyncError> {
    let mut folded_paths = BTreeMap::new();
    for (path, kind) in namespace {
        validate_case_folded_prefixes(path, &mut folded_paths).map_err(|collision| {
            WorkViewError::UnsafeWorkViewPath {
                path: collision.incoming,
                reason: "case-folded overlay namespace collision",
            }
        })?;
        let mut prefix = String::new();
        let components = path.split('/').collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if namespace.get(&prefix) == Some(&NamespaceEntryKind::File) {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: path.clone(),
                    reason: "overlay namespace places an entry below a file",
                }
                .into());
            }
        }
        let child_prefix = format!("{path}/");
        if *kind == NamespaceEntryKind::File
            && namespace
                .range(child_prefix.clone()..)
                .next()
                .is_some_and(|(candidate, _)| candidate.starts_with(&child_prefix))
        {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: path.clone(),
                reason: "overlay namespace uses a file as a directory",
            }
            .into());
        }
    }
    Ok(())
}
