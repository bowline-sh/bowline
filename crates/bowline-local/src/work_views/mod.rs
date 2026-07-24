use std::{error::Error, fmt, io, path::PathBuf};

use bowline_core::ids::{DeviceId, WorkspaceId};

use crate::metadata::{MetadataError, MetadataStore};

mod cleanup;
mod create_list;
mod lifecycle;
mod paths;

pub use cleanup::cleanup_work_views;
pub use create_list::{list_work_views, overlay_aux_engine_truth};
pub use lifecycle::{
    WorkAcceptTransition, apply_accept_success, discard_work_view, restore_work_view,
};
pub use paths::{
    append_work_event, display_path, expand_display_path, open_store, reconcile_aux_work_views,
    resolve_work_view, validate_work_view_name, visible_path, work_view_id,
};

pub(super) fn status_all_command(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
) -> Result<String, WorkViewError> {
    let root = store
        .workspace_root(workspace_id)?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    Ok(format!("bowline status --root {} --all", shell_word(&root)))
}

pub(super) fn shell_word(value: &str) -> String {
    if value == "~" {
        return "~".to_string();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", bowline_core::shell::quote_word(rest));
    }
    bowline_core::shell::quote_word(value)
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
    pub paths: Vec<String>,
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
    UnknownBaseSnapshot {
        selector: String,
    },
    DirtyProject {
        path: String,
    },
    FreshCanonicalSnapshotRequired {
        path: String,
    },
    ContentChangedDuringCapture {
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
    ProjectWriterBusy {
        project_path: String,
        reason: String,
    },
    InvalidPathSelector {
        selector: String,
        reason: String,
    },
    EmptyPathSelection {
        patterns: Vec<String>,
    },
    AcceptRollbackFailed {
        path: String,
        reason: String,
    },
    InvalidAcceptRequest {
        field: &'static str,
    },
    InvalidAcceptTimestamp {
        value: String,
    },
    Metadata(MetadataError),
    Io(io::Error),
    Index(crate::sync::manifest_engine::work_view_cli::WorkViewCliError),
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
            Self::UnknownBaseSnapshot { selector } => {
                write!(
                    formatter,
                    "work view base snapshot `{selector}` was not found"
                )
            }
            Self::DirtyProject { path } => write!(
                formatter,
                "work view for `{path}` needs the current project changes to sync before it can be created"
            ),
            Self::FreshCanonicalSnapshotRequired { path } => write!(
                formatter,
                "work-view exposure for `{path}` needs a fresh canonical snapshot before large content can be captured"
            ),
            Self::ContentChangedDuringCapture { path } => write!(
                formatter,
                "work-view content changed while `{path}` was being captured"
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
            Self::ProjectWriterBusy {
                project_path,
                reason,
            } => write!(
                formatter,
                "work-view accept for `{project_path}` is waiting on another writer: {reason}"
            ),
            Self::InvalidPathSelector { selector, reason } => {
                write!(
                    formatter,
                    "work-view path selector `{selector}` is invalid: {reason}"
                )
            }
            Self::EmptyPathSelection { patterns } => write!(
                formatter,
                "no work-view changes matched --path {}",
                patterns.join(", ")
            ),
            Self::AcceptRollbackFailed { path, reason } => write!(
                formatter,
                "work-view accept rollback failed at `{path}`: {reason}"
            ),
            Self::InvalidAcceptRequest { field } => {
                write!(formatter, "work-view accept request has invalid {field}")
            }
            Self::InvalidAcceptTimestamp { value } => {
                write!(formatter, "work-view accept timestamp `{value}` is invalid")
            }
            Self::Metadata(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "work-view file operation failed: {error}"),
            Self::Index(error) => error.fmt(formatter),
        }
    }
}

impl Error for WorkViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Index(error) => Some(error),
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

impl From<crate::sync::manifest_engine::work_view_cli::WorkViewCliError> for WorkViewError {
    fn from(error: crate::sync::manifest_engine::work_view_cli::WorkViewCliError) -> Self {
        Self::Index(error)
    }
}
