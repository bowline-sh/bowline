use std::path::Path;

use bowline_core::workspace_graph::NamespaceEntryKind;
use bowline_storage::LocalContentCache;

use super::{apply_file_permissions, materialize_verified_content};
use crate::work_views::safe_materialization::SafeMaterializationRoot;
use crate::work_views::{
    WorkViewError,
    exposure::{SnapshotExposurePlan, WorkViewExposurePlan},
    paths::is_owner_only_work_view_policy,
};

pub(crate) fn materialize_live_exposure_plan(
    plan: &WorkViewExposurePlan,
    workspace_content_key: [u8; 32],
    visible_path: &Path,
) -> Result<(), WorkViewError> {
    let staging = SafeMaterializationRoot::new(visible_path)?;
    for planned in &plan.entries {
        let relative_path = Path::new(&planned.relative_path);
        match planned.entry.kind {
            NamespaceEntryKind::Directory => staging.create_dir(relative_path)?,
            NamespaceEntryKind::File => {
                let content_id = planned
                    .entry
                    .content_id
                    .as_ref()
                    .ok_or_else(|| missing_content_id(&planned.entry.path))?;
                let source_identity = planned.source_identity.ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "live exposed file has no stable source identity",
                    )
                })?;
                let destination = staging.prepare_file(relative_path)?;
                crate::work_views::content_identity::materialize_verified_live_content(
                    workspace_content_key,
                    content_id,
                    &planned.source_path,
                    source_identity,
                    &destination,
                    planned.owner_only,
                )?;
                apply_file_permissions(
                    &staging,
                    relative_path,
                    planned.entry.executability,
                    planned.owner_only,
                )?;
            }
            NamespaceEntryKind::Symlink
            | NamespaceEntryKind::Placeholder
            | NamespaceEntryKind::Tombstone => {}
        }
    }
    Ok(())
}

pub(crate) fn materialize_snapshot_exposure_plan(
    plan: &SnapshotExposurePlan,
    project_path: &str,
    cache_root: &Path,
    visible_path: &Path,
) -> Result<(), WorkViewError> {
    let project_path = bowline_core::workspace_graph::normalize_workspace_path(project_path);
    let entries = plan
        .entries
        .iter()
        .map(|entry| {
            let relative = if project_path.is_empty() {
                Some(entry.path.as_str())
            } else {
                entry
                    .path
                    .strip_prefix(&project_path)
                    .and_then(|path| path.strip_prefix('/'))
            }
            .ok_or_else(|| WorkViewError::SnapshotMaterialization {
                snapshot_id: "exposed-base".to_string(),
                reason: format!(
                    "exposed entry `{}` is outside project `{project_path}`",
                    entry.path
                ),
            })?;
            Ok((
                entry,
                relative.to_string(),
                is_owner_only_work_view_policy(entry.classification, entry.mode),
            ))
        })
        .collect::<Result<Vec<_>, WorkViewError>>()?;
    materialize_entries(entries, cache_root, visible_path)
}

fn materialize_entries<'a>(
    entries: impl IntoIterator<
        Item = (
            &'a bowline_core::workspace_graph::NamespaceEntry,
            String,
            bool,
        ),
    >,
    cache_root: &Path,
    visible_path: &Path,
) -> Result<(), WorkViewError> {
    let cache = LocalContentCache::open(cache_root)?;
    let staging = SafeMaterializationRoot::new(visible_path)?;
    for (entry, relative_path, owner_only) in entries {
        let relative_path = Path::new(&relative_path);
        match entry.kind {
            NamespaceEntryKind::Directory => staging.create_dir(relative_path)?,
            NamespaceEntryKind::File => {
                let content_id = entry
                    .content_id
                    .as_ref()
                    .ok_or_else(|| missing_content_id(&entry.path))?;
                let destination = staging.prepare_file(relative_path)?;
                materialize_verified_content(&cache, content_id, &destination, owner_only)?;
                apply_file_permissions(&staging, relative_path, entry.executability, owner_only)?;
            }
            NamespaceEntryKind::Symlink
            | NamespaceEntryKind::Placeholder
            | NamespaceEntryKind::Tombstone => {}
        }
    }
    Ok(())
}

fn missing_content_id(path: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("content-addressed exposed file `{path}` has no content id"),
    )
}
