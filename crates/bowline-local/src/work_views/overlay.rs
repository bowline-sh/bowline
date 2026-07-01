use std::path::{Path, PathBuf};

use bowline_core::{
    work_views::{WorkDiffChangeKind, WorkDiffEntry, WorkView},
    workspace_graph::normalize_workspace_path,
};

use crate::metadata::{LocalWriteLogRecord, MetadataStore};

use super::{
    WorkViewError,
    paths::{
        file_content_hash, files_under, is_secret_bearing_work_path,
        is_source_control_metadata_path, work_view_base_has_path,
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
}

pub fn logged_overlay_deltas(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<OverlayDelta>, WorkViewError> {
    let visible_prefix = normalize_workspace_path(
        &store.workspace_relative_path(&work_view.workspace_id, &work_view.visible_path)?,
    );
    let mut deltas = Vec::new();
    for write in store.local_write_log(&work_view.workspace_id)? {
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
        deltas.push(delta_from_write(write, relative_path));
    }
    deltas.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.write_id.cmp(&right.write_id))
    });
    deltas.dedup_by(|left, right| left.path == right.path && left.kind == right.kind);
    Ok(deltas)
}

pub fn filesystem_overlay_deltas(
    store: &MetadataStore,
    work_view: &WorkView,
    work_root: &Path,
) -> Result<Vec<OverlayDelta>, WorkViewError> {
    let mut changes = Vec::new();
    if !work_root.exists() {
        return Ok(changes);
    }
    for file in files_under(work_root)? {
        let relative = file
            .strip_prefix(work_root)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            .to_path_buf();
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        let relative_path = normalize_workspace_path(&relative.display().to_string());
        let kind = match store.work_view_base_hash(
            &work_view.workspace_id,
            &work_view.id,
            &relative_path,
        )? {
            Some(base_hash) => {
                if file_content_hash(&file)? == base_hash {
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
    for (relative, _hash) in store.work_view_base_files(&work_view.workspace_id, &work_view.id)? {
        let relative = PathBuf::from(relative);
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        let work_path = work_root.join(&relative);
        if work_view_base_has_path(store, work_view, &relative)? && !work_path.is_file() {
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

fn delta_from_write(write: LocalWriteLogRecord, relative_path: PathBuf) -> OverlayDelta {
    let operation = write.operation.as_str();
    let kind = match operation {
        "create" | "created" => OverlayDeltaKind::Create,
        "delete" | "deleted" => OverlayDeltaKind::Delete,
        "rename" | "renamed" => OverlayDeltaKind::Rename {
            from: write.source_path.map(PathBuf::from).unwrap_or_default(),
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
