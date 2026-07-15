use bowline_core::workspace_graph::normalize_workspace_path;

use super::{WorkViewError, WorkViewOverlaySyncError, overlay, overlay_wire::OverlayOperation};

pub(super) fn overlay_operation(
    kind: &overlay::OverlayDeltaKind,
) -> Result<OverlayOperation, WorkViewOverlaySyncError> {
    match kind {
        overlay::OverlayDeltaKind::Create => Ok(OverlayOperation::Create),
        overlay::OverlayDeltaKind::Modify => Ok(OverlayOperation::Modify),
        overlay::OverlayDeltaKind::Delete => Ok(OverlayOperation::Delete),
        overlay::OverlayDeltaKind::Rename { .. } => Ok(OverlayOperation::Rename),
        overlay::OverlayDeltaKind::Symlink
        | overlay::OverlayDeltaKind::Chmod
        | overlay::OverlayDeltaKind::Unsupported { .. } => Err(WorkViewError::UnsafeWorkViewPath {
            path: "overlay".to_string(),
            reason: "unsupported overlay operation reached wire encoding",
        }
        .into()),
    }
}

#[cfg(test)]
pub(super) fn overlay_delta_kind_name(kind: &overlay::OverlayDeltaKind) -> &'static str {
    match kind {
        overlay::OverlayDeltaKind::Create => "create",
        overlay::OverlayDeltaKind::Modify => "modify",
        overlay::OverlayDeltaKind::Delete => "delete",
        overlay::OverlayDeltaKind::Rename { .. } => "rename",
        overlay::OverlayDeltaKind::Symlink => "symlink",
        overlay::OverlayDeltaKind::Chmod => "chmod",
        overlay::OverlayDeltaKind::Unsupported { .. } => "unsupported",
    }
}

pub(super) fn overlay_delta_rename_from(kind: &overlay::OverlayDeltaKind) -> Option<String> {
    match kind {
        overlay::OverlayDeltaKind::Rename { from } => {
            Some(normalize_workspace_path(&from.display().to_string()))
        }
        _ => None,
    }
}
