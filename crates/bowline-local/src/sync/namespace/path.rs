use std::path::Path;

use bowline_core::{
    namespace_snapshot::NamespaceReadError,
    workspace_graph::{WorkspaceRelativePath, normalize_workspace_path},
};

const MAX_NORMALIZED_NAMESPACE_PATH_BYTES: usize = 4_096;

pub(crate) fn validated_path(path: &str) -> Result<WorkspaceRelativePath, NamespaceReadError> {
    if path.len() > MAX_NORMALIZED_NAMESPACE_PATH_BYTES {
        return Err(NamespaceReadError::InvalidPath {
            field: "path",
            reason: "normalized path exceeds the byte limit",
        });
    }
    if path != normalize_workspace_path(path) {
        return Err(NamespaceReadError::InvalidPath {
            field: "path",
            reason: "path is not in canonical workspace-relative form",
        });
    }
    if path.is_empty()
        || path
            .split('/')
            .any(|component| matches!(component, "." | ".."))
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(NamespaceReadError::InvalidPath {
            field: "path",
            reason: "path is not a non-empty normalized relative path",
        });
    }
    Ok(WorkspaceRelativePath::new(path))
}
