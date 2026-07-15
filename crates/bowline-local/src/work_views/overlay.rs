use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use bowline_core::{
    work_views::{WorkDiffChangeKind, WorkDiffEntry, WorkView},
    workspace_graph::{NamespaceEntryKind, normalize_workspace_path},
};
use bowline_storage::LocalContentCache;

use crate::metadata::{LocalWriteLogRecord, MetadataStore};

use super::{
    WorkViewError,
    content_identity::verified_content_matches_path_with_checkpoint,
    paths::{
        cancellation_checkpoint, files_under_with_checkpoint, is_secret_bearing_work_path,
        is_source_control_metadata_path,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayDelta {
    pub path: PathBuf,
    pub kind: OverlayDeltaKind,
    pub contains_secrets: bool,
    pub write_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayDeltaKind {
    Create,
    Modify,
    Delete,
    Rename { from: PathBuf },
    Symlink,
    Chmod,
    Unsupported { operation: String },
}

impl OverlayDeltaKind {
    pub fn diff_kind(&self) -> WorkDiffChangeKind {
        match self {
            Self::Create | Self::Rename { .. } | Self::Symlink => WorkDiffChangeKind::Added,
            Self::Modify | Self::Chmod => WorkDiffChangeKind::Modified,
            Self::Delete => WorkDiffChangeKind::Deleted,
            Self::Unsupported { .. } => WorkDiffChangeKind::PolicyReview,
        }
    }

    pub fn summary(&self, work_view_name: &str) -> String {
        match self {
            Self::Create => format!("created in work view {work_view_name}"),
            Self::Modify => format!("modified in work view {work_view_name}"),
            Self::Delete => format!("deleted in work view {work_view_name}"),
            Self::Rename { from } => format!(
                "renamed from {} in work view {work_view_name}",
                normalize_workspace_path(&from.display().to_string())
            ),
            Self::Symlink => format!("symlink changed in work view {work_view_name}"),
            Self::Chmod => format!("mode changed in work view {work_view_name}"),
            Self::Unsupported { operation } => {
                format!("{operation} needs review in work view {work_view_name}")
            }
        }
    }

    pub fn requires_review(&self) -> bool {
        matches!(self, Self::Unsupported { .. } | Self::Symlink | Self::Chmod)
    }

    fn dedupe_key(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Modify => "modify",
            Self::Delete => "delete",
            Self::Rename { .. } => "rename",
            Self::Symlink => "symlink",
            Self::Chmod => "chmod",
            Self::Unsupported { .. } => "unsupported",
        }
    }
}

pub fn logged_overlay_deltas(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<OverlayDelta>, WorkViewError> {
    let visible_prefix = normalize_workspace_path(
        &store.workspace_relative_path(&work_view.workspace_id, &work_view.visible_path)?,
    );
    let mut deltas = BTreeMap::new();
    for write in store.local_writes_for_path_prefix(&work_view.workspace_id, &visible_prefix)? {
        let path = normalize_workspace_path(
            &store.workspace_relative_path(&work_view.workspace_id, &write.path)?,
        );
        let Some(relative) = relative_to_work_view(&path, &visible_prefix) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        let relative_path = PathBuf::from(relative);
        if is_source_control_metadata_path(&relative_path) {
            continue;
        }
        let rename_from = write.source_path.as_deref().and_then(|source_path| {
            let source_path = normalize_workspace_path(
                &store
                    .workspace_relative_path(&work_view.workspace_id, source_path)
                    .ok()?,
            );
            relative_to_work_view(&source_path, &visible_prefix)
                .filter(|relative| !relative.is_empty())
                .map(PathBuf::from)
        });
        let delta = delta_from_write(write, relative_path, rename_from);
        deltas.insert((delta.path.clone(), delta.kind.dedupe_key()), delta);
    }
    Ok(deltas.into_values().collect())
}

pub fn filesystem_overlay_deltas_with_checkpoint(
    store: &MetadataStore,
    work_view: &WorkView,
    work_root: &Path,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<Vec<OverlayDelta>, WorkViewError> {
    let mut changes = Vec::new();
    if !work_root.exists() {
        return Ok(changes);
    }
    let descriptor = store
        .work_view_exposed_base(&work_view.workspace_id, &work_view.id)?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "authoritative exposed base is missing".to_string(),
        })?;
    let project_prefix = descriptor.project_prefix.trim_end_matches('/');
    let exposed = super::namespace::load_exposed_snapshot(store, &descriptor)?;
    let base_files = super::namespace::collect_prefix(
        &exposed,
        &bowline_core::workspace_graph::WorkspaceRelativePath::new(project_prefix),
    )?
    .into_iter()
    .filter(|entry| entry.kind == NamespaceEntryKind::File)
    .filter_map(|entry| {
        let relative = entry
            .path
            .strip_prefix(project_prefix)?
            .trim_start_matches('/')
            .to_string();
        (!relative.is_empty()).then_some((relative, entry))
    })
    .collect::<BTreeMap<_, _>>();
    let cache = LocalContentCache::open(store.content_cache_root()?)?;
    for file in files_under_with_checkpoint(work_root, checkpoint)? {
        cancellation_checkpoint(checkpoint)?;
        let relative = file
            .strip_prefix(work_root)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            .to_path_buf();
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        let relative_path = normalize_workspace_path(&relative.display().to_string());
        let kind = match base_files.get(&relative_path) {
            Some(base) => {
                let content_id = base.content_id.as_ref().ok_or_else(|| {
                    WorkViewError::SnapshotMaterialization {
                        snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                        reason: format!(
                            "content-addressed exposed file `{relative_path}` has no content id"
                        ),
                    }
                })?;
                let unchanged = verified_content_matches_path_with_checkpoint(
                    &cache, content_id, &file, checkpoint,
                )?;
                if unchanged {
                    continue;
                }
                OverlayDeltaKind::Modify
            }
            None => OverlayDeltaKind::Create,
        };
        changes.push(OverlayDelta {
            contains_secrets: is_secret_bearing_work_path(&relative),
            path: relative,
            kind,
            write_id: None,
        });
    }
    for relative in base_files.keys() {
        cancellation_checkpoint(checkpoint)?;
        let relative = PathBuf::from(relative);
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        let work_path = work_root.join(&relative);
        if !work_path.is_file() {
            changes.push(OverlayDelta {
                contains_secrets: is_secret_bearing_work_path(&relative),
                path: relative,
                kind: OverlayDeltaKind::Delete,
                write_id: None,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes.dedup_by(|left, right| left.path == right.path && left.kind == right.kind);
    Ok(changes)
}

pub fn diff_entries_from_deltas(
    work_view: &WorkView,
    deltas: &[OverlayDelta],
) -> Vec<WorkDiffEntry> {
    let mut entries = deltas
        .iter()
        .map(|delta| WorkDiffEntry {
            path: normalize_workspace_path(&delta.path.display().to_string()),
            kind: delta.kind.diff_kind(),
            summary: delta.kind.summary(&work_view.name),
            contains_secrets: delta.contains_secrets,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.summary.cmp(&right.summary))
    });
    entries.dedup_by(|left, right| left.path == right.path && left.kind == right.kind);
    entries
}

fn delta_from_write(
    write: LocalWriteLogRecord,
    relative_path: PathBuf,
    rename_from: Option<PathBuf>,
) -> OverlayDelta {
    let operation = write.operation.as_str();
    let kind = match operation {
        "create" | "created" => OverlayDeltaKind::Create,
        "delete" | "deleted" => OverlayDeltaKind::Delete,
        "rename" | "renamed" => OverlayDeltaKind::Rename {
            from: rename_from.unwrap_or_default(),
        },
        "symlink" | "readlink" => OverlayDeltaKind::Symlink,
        "chmod" | "mode" => OverlayDeltaKind::Chmod,
        "modify" | "modified" | "update" | "updated" | "safe-save" | "replace" | "write" => {
            OverlayDeltaKind::Modify
        }
        other => OverlayDeltaKind::Unsupported {
            operation: other.to_string(),
        },
    };
    OverlayDelta {
        contains_secrets: is_secret_bearing_work_path(&relative_path),
        path: relative_path,
        kind,
        write_id: Some(write.id),
    }
}

fn relative_to_work_view<'a>(path: &'a str, visible_prefix: &str) -> Option<&'a str> {
    if path == visible_prefix {
        return Some("");
    }
    path.strip_prefix(visible_prefix)
        .and_then(|relative| relative.strip_prefix('/'))
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        ids::{DeviceId, ProjectId, WorkspaceId},
        policy::PathClassification,
    };

    use super::*;

    #[test]
    fn write_operations_become_typed_overlay_deltas() {
        let workspace_id = WorkspaceId::new("ws");
        let writes = [
            ("safe-save", "src/app.ts", None),
            ("update", "src/update.ts", None),
            ("rename", "src/new.ts", Some("src/old.ts")),
            ("symlink", "linked", None),
            ("chmod", "script.sh", None),
            ("delete", ".env.local", None),
            ("mmap", "binary.dat", None),
        ];

        let kinds = writes
            .into_iter()
            .map(|(operation, path, source_path)| {
                delta_from_write(
                    LocalWriteLogRecord {
                        id: format!("write-{operation}-{path}"),
                        workspace_id: workspace_id.clone(),
                        device_id: DeviceId::new("dev"),
                        project_id: Some(ProjectId::new("proj")),
                        path: path.to_string(),
                        source_path: source_path.map(str::to_string),
                        operation: operation.to_string(),
                        staged_content_id: None,
                        policy_classification: PathClassification::WorkspaceSync,
                        causation_id: "test".to_string(),
                        settled_at: "2026-06-27T00:00:00Z".to_string(),
                        created_at: "2026-06-27T00:00:00Z".to_string(),
                    },
                    PathBuf::from(path),
                    source_path.map(PathBuf::from),
                )
                .kind
            })
            .collect::<Vec<_>>();

        assert_eq!(
            kinds,
            vec![
                OverlayDeltaKind::Modify,
                OverlayDeltaKind::Modify,
                OverlayDeltaKind::Rename {
                    from: PathBuf::from("src/old.ts")
                },
                OverlayDeltaKind::Symlink,
                OverlayDeltaKind::Chmod,
                OverlayDeltaKind::Delete,
                OverlayDeltaKind::Unsupported {
                    operation: "mmap".to_string()
                },
            ]
        );
    }
}
