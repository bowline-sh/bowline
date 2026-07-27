//! The workspace-root sentinel: proof that the directory about to be scanned is
//! still the workspace whose ancestor this device committed.
//!
//! Without it the engine had a catastrophic failure mode. `walk_dir` treated a
//! missing root as an empty directory, so an unmounted external volume, a renamed
//! `~/Code`, or an agent host whose home was not mounted yet produced a walk that
//! saw nothing, marked every ancestor row deleted, published an EMPTY manifest,
//! and every other trusted device pulled it and deleted the entire workspace.
//!
//! The marker is Syncthing's `.stfolder` idea: a small file the engine writes
//! when it first claims a root, and requires on every cycle thereafter. It is
//! written INSIDE the root on purpose — it must disappear exactly when the root
//! does.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::push::EngineContext;

/// Marker location, relative to the workspace root. Inside the private engine
/// subtree, so the stat walk never treats it as syncable content.
const MARKER_RELATIVE_PATH: &str = ".bowline/workspace.marker";

/// Marker framing version. A marker this engine cannot parse is treated as a
/// mismatch, never silently adopted.
const MARKER_FORMAT_VERSION: u32 = 1;

/// Why the workspace root cannot be trusted this cycle. Every variant means the
/// same thing operationally — publish nothing — but they carry different user
/// text, and telling "your drive is not mounted" from "this is a different
/// workspace" is the whole point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RootFault {
    Missing,
    NotADirectory,
    Unreadable,
    MarkerMissing,
    MarkerMismatch,
    MarkerUnreadable,
}

impl RootFault {
    /// The user-facing reason a status surface prints. Never mentions the marker
    /// mechanism to a user whose real problem is an unmounted disk.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => {
                "The workspace folder is not there. If it lives on an external or \
                 network drive, reconnect it; if it was renamed or moved, move it back."
            }
            Self::NotADirectory => {
                "Something that is not a folder is sitting at the workspace path. \
                 Move it aside so the workspace folder can be restored."
            }
            Self::Unreadable => {
                "The workspace folder cannot be read. Check its permissions and that \
                 its disk is healthy."
            }
            Self::MarkerMissing => {
                "The folder at the workspace path is not the synced workspace — its \
                 Bowline marker is gone. Reconnect the original drive or folder \
                 rather than letting Bowline treat this one as an empty workspace."
            }
            Self::MarkerMismatch => {
                "The folder at the workspace path belongs to a different Bowline \
                 workspace. Point Bowline at the right folder."
            }
            Self::MarkerUnreadable => {
                "Bowline cannot read its workspace marker. Check the permissions on \
                 the workspace folder."
            }
        }
    }

    /// A short tag for logs and status facts.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Missing => "root-missing",
            Self::NotADirectory => "root-not-a-directory",
            Self::Unreadable => "root-unreadable",
            Self::MarkerMissing => "marker-missing",
            Self::MarkerMismatch => "marker-mismatch",
            Self::MarkerUnreadable => "marker-unreadable",
        }
    }
}

/// Classify a failure to reach the workspace root itself.
///
/// One mapping, shared by every caller that touches the root: the sentinel's
/// `metadata` probe and the conflict scan's `read_dir`. A second copy would let
/// "your drive is not mounted" and "check the permissions" drift apart between
/// two surfaces reporting the same disk.
pub fn root_fault_from_io(error: &io::Error) -> RootFault {
    match error.kind() {
        io::ErrorKind::NotFound => RootFault::Missing,
        io::ErrorKind::NotADirectory => RootFault::NotADirectory,
        _ => RootFault::Unreadable,
    }
}

/// Whether the workspace root is a directory this process can reach, before any
/// marker or content check. `None` means the root itself is fine.
///
/// Callers that walk the root without the marker contract (the conflict scan)
/// need exactly this much of the sentinel: a missing, replaced, or unreachable
/// root must never read as an empty workspace.
pub fn classify_root_directory(root: &Path) -> Option<RootFault> {
    // `metadata` (not `symlink_metadata`): the root itself is the given trust
    // anchor and a symlinked `~/Code` is a normal setup. The no-follow rule
    // governs traversal INSIDE the root, which fs_guard owns.
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => None,
        Ok(_) => Some(RootFault::NotADirectory),
        Err(error) => Some(root_fault_from_io(&error)),
    }
}

/// The sentinel verdict for one cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    Ready,
    Faulted(RootFault),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceMarkerFile {
    format_version: u32,
    workspace_id_hash: String,
}

pub fn marker_path(root: &Path) -> PathBuf {
    root.join(MARKER_RELATIVE_PATH)
}

/// Verify the root before any walk, pull, or push.
///
/// A work view is rooted at a project checkout, where `.bowline` is ordinary
/// project content, so it gets the directory check only — never a marker written
/// into the user's repository.
pub fn verify_root(ctx: &EngineContext) -> RootState {
    if let Some(fault) = classify_root_directory(&ctx.workspace_root) {
        return RootState::Faulted(fault);
    }
    if ctx.project_view {
        return RootState::Ready;
    }
    let raw = match fs::read(marker_path(&ctx.workspace_root)) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return RootState::Faulted(RootFault::MarkerMissing);
        }
        Err(_) => return RootState::Faulted(RootFault::MarkerUnreadable),
    };
    match serde_json::from_slice::<WorkspaceMarkerFile>(&raw) {
        Ok(marker)
            if marker.format_version == MARKER_FORMAT_VERSION
                && marker.workspace_id_hash == ctx.crypto.workspace_id_hash() =>
        {
            RootState::Ready
        }
        _ => RootState::Faulted(RootFault::MarkerMismatch),
    }
}

/// Claim this root for this workspace.
///
/// The caller must have proven the root is safe to claim — in practice, that this
/// device has no committed ancestor rows, so an empty root cannot destroy
/// anything. Never creates the root directory itself: a missing root must stay a
/// fault, or the guard would recreate `~/Code` on an unmounted volume's
/// mountpoint and then sync into it.
pub fn adopt_root(ctx: &EngineContext) -> io::Result<()> {
    let marker = marker_path(&ctx.workspace_root);
    let parent = marker
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "marker path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file = WorkspaceMarkerFile {
        format_version: MARKER_FORMAT_VERSION,
        workspace_id_hash: ctx.crypto.workspace_id_hash().to_string(),
    };
    let bytes = serde_json::to_vec(&file)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    super::fs_guard::write_private_file(&marker, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::manifest_engine::engine_test_support::test_context;
    use crate::workspace::TempWorkspace;

    #[test]
    fn a_fresh_root_is_faulted_until_it_is_adopted() {
        let workspace = TempWorkspace::new("root-adopt").expect("temp workspace");
        let ctx = test_context(workspace.root().to_path_buf(), "device-a");
        assert_eq!(
            verify_root(&ctx),
            RootState::Faulted(RootFault::MarkerMissing)
        );
        adopt_root(&ctx).expect("adopt");
        assert_eq!(verify_root(&ctx), RootState::Ready);
    }

    #[test]
    fn a_missing_root_is_a_fault_and_is_never_recreated() {
        let workspace = TempWorkspace::new("root-missing").expect("temp workspace");
        let root = workspace.root().join("not-mounted");
        let ctx = test_context(root.clone(), "device-a");
        assert_eq!(verify_root(&ctx), RootState::Faulted(RootFault::Missing));
        assert!(!root.exists(), "verification never creates the root");
    }

    #[test]
    fn a_marker_from_another_workspace_does_not_satisfy_the_sentinel() {
        let workspace = TempWorkspace::new("root-mismatch").expect("temp workspace");
        let ctx = test_context(workspace.root().to_path_buf(), "device-a");
        adopt_root(&ctx).expect("adopt");
        let foreign = WorkspaceMarkerFile {
            format_version: MARKER_FORMAT_VERSION,
            workspace_id_hash: "someone-elses-workspace".to_string(),
        };
        fs::write(
            marker_path(&ctx.workspace_root),
            serde_json::to_vec(&foreign).expect("encode"),
        )
        .expect("write marker");
        assert_eq!(
            verify_root(&ctx),
            RootState::Faulted(RootFault::MarkerMismatch)
        );
    }
}
