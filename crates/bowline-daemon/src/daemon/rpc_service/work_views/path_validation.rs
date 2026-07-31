use std::path::{Component, Path, PathBuf};

use bowline_core::wire::generated::DaemonRpcErrorCode;

use crate::daemon::rpc_service::{RpcResult, rpc_error};

/// A view directory must live under the workspace's `.work/` tree — the daemon
/// never materializes to an arbitrary caller-supplied path.
pub(super) fn checked_view_dir(root: &Path, view_dir: &str) -> RpcResult<PathBuf> {
    let path = PathBuf::from(view_dir);
    let has_traversal = path
        .components()
        .any(|component| matches!(component, Component::ParentDir));
    let inside_work_tree = path.strip_prefix(root).is_ok_and(|relative| {
        let mut parts = relative.components();
        parts
            .next()
            .is_some_and(|first| first.as_os_str() == ".work")
            && parts.next().is_some()
    });
    if !path.is_absolute() || has_traversal || !inside_work_tree {
        return Err(rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "work-view directory must be inside the workspace .work tree",
            false,
        ));
    }
    Ok(path)
}

pub(super) fn checked_project_path(
    root: &Path,
    view_dir: &Path,
    project_path: &str,
) -> RpcResult<String> {
    let project = Path::new(project_path);
    let canonical: PathBuf = project.components().collect();
    let normalized = project_path.is_empty()
        || (!project.is_absolute()
            && !project_path.starts_with('/')
            && project
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && canonical.to_str() == Some(project_path));
    let expected = view_dir
        .strip_prefix(root.join(".work"))
        .ok()
        .and_then(Path::parent);
    if !normalized || expected != Some(project) {
        return Err(rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "project path must be normalized and match the work-view directory scope",
            false,
        ));
    }
    Ok(project_path.to_string())
}
