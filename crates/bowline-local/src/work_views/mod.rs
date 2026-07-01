use std::{error::Error, fmt, io, path::PathBuf};

use bowline_control_plane::{ControlPlaneError, WorkViewUpdateError};
use bowline_core::ids::DeviceId;
use bowline_storage::{ByteStoreError, PackfileError};

use crate::metadata::MetadataError;

mod cleanup;
mod create_list;
mod diff;
mod lifecycle;
mod materialize;
mod overlay;
pub mod overlay_resolution;
mod overlay_sync;
mod paths;

pub use cleanup::cleanup_work_views;
pub use create_list::{create_work_view, list_work_views};
pub use diff::diff_work_view;
pub use lifecycle::{accept_work_view, discard_work_view, restore_work_view};
pub use overlay_sync::{
    WorkViewOverlaySyncOptions, WorkViewOverlaySyncReport, sync_local_work_view_overlays,
};
pub(crate) use paths::expand_display_path;

#[cfg(test)]
use overlay_sync::{overlay_delta_kind_name, overlay_deltas_for_upload};

#[derive(Debug, Clone)]
pub struct WorkonOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub name: String,
    pub owner_device_id: Option<DeviceId>,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct WorkListOptions {
    pub db_path: Option<PathBuf>,
    pub include_hidden: bool,
    pub current_device_id: Option<DeviceId>,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct WorkSelectorOptions {
    pub db_path: Option<PathBuf>,
    pub selector: String,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct WorkCleanupOptions {
    pub db_path: Option<PathBuf>,
    pub apply: bool,
    pub generated_at: String,
}

#[derive(Debug)]
pub enum WorkViewError {
    MissingMetadataDb,
    MissingWorkspace,
    MissingWorkspaceRoot,
    MissingProject {
        path: String,
    },
    MissingBaseSnapshot {
        path: String,
    },
    DirtyProject {
        path: String,
    },
    InvalidName {
        name: String,
        reason: &'static str,
    },
    NameCollision {
        name: String,
        project_path: String,
    },
    AmbiguousSelector {
        selector: String,
        matches: Vec<String>,
    },
    MissingWorkView {
        selector: String,
    },
    InactiveWorkView {
        name: String,
    },
    UnrestorableWorkView {
        name: String,
    },
    UnsafeWorkViewPath {
        path: String,
        reason: &'static str,
    },
    Metadata(MetadataError),
    Io(io::Error),
}

impl fmt::Display for WorkViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMetadataDb => {
                write!(
                    formatter,
                    "metadata database path could not be resolved for work-view commands"
                )
            }
            Self::MissingWorkspace => write!(formatter, "no bowline workspace is initialized"),
            Self::MissingWorkspaceRoot => write!(formatter, "workspace root is missing"),
            Self::MissingProject { path } => {
                write!(formatter, "no tracked project was found for `{path}`")
            }
            Self::MissingBaseSnapshot { path } => write!(
                formatter,
                "work view for `{path}` needs a fresh project snapshot before it can be created"
            ),
            Self::DirtyProject { path } => write!(
                formatter,
                "work view for `{path}` needs the current project changes to sync before it can be created"
            ),
            Self::InvalidName { name, reason } => {
                write!(formatter, "work view name `{name}` is invalid: {reason}")
            }
            Self::NameCollision { name, project_path } => write!(
                formatter,
                "work view `{name}` already exists for project `{project_path}`"
            ),
            Self::AmbiguousSelector { selector, matches } => write!(
                formatter,
                "work view selector `{selector}` is ambiguous: {}",
                matches.join(", ")
            ),
            Self::MissingWorkView { selector } => {
                write!(formatter, "work view `{selector}` was not found")
            }
            Self::InactiveWorkView { name } => {
                write!(
                    formatter,
                    "work view `{name}` must be restored before it can be accepted"
                )
            }
            Self::UnrestorableWorkView { name } => {
                write!(formatter, "work view `{name}` is not restorable")
            }
            Self::UnsafeWorkViewPath { path, reason } => {
                write!(formatter, "unsafe work-view path `{path}`: {reason}")
            }
            Self::Metadata(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "work-view file operation failed: {error}"),
        }
    }
}

impl Error for WorkViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MetadataError> for WorkViewError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<io::Error> for WorkViewError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub enum WorkViewOverlaySyncError {
    WorkView(WorkViewError),
    Metadata(MetadataError),
    ControlPlane(ControlPlaneError),
    WorkViewUpdate(WorkViewUpdateError),
    Packfile(PackfileError),
    ByteStore(ByteStoreError),
    Json(serde_json::Error),
    MissingOverlayPack,
}

impl fmt::Display for WorkViewOverlaySyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkView(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::WorkViewUpdate(error) => error.fmt(formatter),
            Self::Packfile(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::MissingOverlayPack => write!(formatter, "overlay pack writer produced no pack"),
        }
    }
}

impl Error for WorkViewOverlaySyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkView(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::WorkViewUpdate(error) => Some(error),
            Self::Packfile(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::MissingOverlayPack => None,
        }
    }
}

impl From<WorkViewError> for WorkViewOverlaySyncError {
    fn from(error: WorkViewError) -> Self {
        Self::WorkView(error)
    }
}

impl From<MetadataError> for WorkViewOverlaySyncError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<ControlPlaneError> for WorkViewOverlaySyncError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<WorkViewUpdateError> for WorkViewOverlaySyncError {
    fn from(error: WorkViewUpdateError) -> Self {
        Self::WorkViewUpdate(error)
    }
}

impl From<PackfileError> for WorkViewOverlaySyncError {
    fn from(error: PackfileError) -> Self {
        Self::Packfile(error)
    }
}

impl From<ByteStoreError> for WorkViewOverlaySyncError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<serde_json::Error> for WorkViewOverlaySyncError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests;
