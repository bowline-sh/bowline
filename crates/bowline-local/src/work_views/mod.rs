use std::{error::Error, fmt, io, path::PathBuf};

use bowline_control_plane::{ControlPlaneError, WorkViewUpdateError};
use bowline_core::{
    ids::{DeviceId, WorkspaceId},
    namespace_snapshot::{NamespaceBuildError, NamespaceReadError},
};
use bowline_storage::{ByteStoreError, CacheError, PackfileError};

use crate::metadata::{MetadataError, MetadataStore};

mod accept_completion;
#[cfg(test)]
mod accept_journal;
mod accept_operation;
mod accept_transaction;
mod candidate;
mod cleanup;
mod content_identity;
mod create_list;
mod create_publish_checkpoint;
mod diff;
mod exposure;
mod lifecycle;
mod materialize;
mod namespace;
mod overlay;
mod overlay_commit;
mod overlay_delta_operation;
mod overlay_objects;
mod overlay_preserve;
mod overlay_publish;
mod overlay_receive;
pub mod overlay_resolution;
mod overlay_retention;
mod overlay_sync;
mod overlay_upload;
mod overlay_validate;
mod overlay_wire;
mod paths;
mod pending_materialization;
mod safe_materialization;
pub(crate) mod snapshot_accept;
mod writer_lock;

#[cfg(test)]
pub(crate) use accept_completion::advance_partial_exposed_base;
#[cfg(test)]
pub(crate) use accept_completion::advance_partial_exposed_base_from_live_tree;
#[cfg(test)]
pub(crate) use accept_completion::finalize_review_ready;
pub(crate) use accept_completion::{
    PartialExposedBaseAdvance, WorkViewAcceptReview, finalize_review_ready_under_claim,
    prepare_partial_exposed_base, publish_partial_exposed_base_under_claim,
};
pub use accept_operation::{
    WorkViewAcceptPhase, WorkViewAcceptProgress, enqueue_work_view_accept,
    work_view_accept_progress,
};
pub(crate) use candidate::{PolicyDriftRecord, WorkCandidateUniverse};
pub use cleanup::cleanup_work_views;
pub(crate) use create_list::create_work_view_with_id_and_key;
pub use create_list::{create_work_view, list_work_views};
pub use diff::{diff_work_view, diff_work_view_with_checkpoint};
pub(crate) use exposure::{plan_live_tree_exposure, plan_snapshot_exposure};
#[cfg(test)]
pub(crate) use lifecycle::accept_work_view;
pub use lifecycle::{discard_work_view, restore_work_view};
pub use overlay_sync::{
    WorkViewOverlaySyncOptions, WorkViewOverlaySyncReport, sync_local_work_view_overlays,
    sync_local_work_view_overlays_with_checkpoint,
};
pub(crate) use paths::expand_display_path;

#[cfg(test)]
use overlay_sync::{overlay_delta_kind_name, overlay_deltas_for_upload};
#[cfg(test)]
use overlay_upload::upload_staged_content;

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
pub struct WorkCreateOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub name: String,
    pub base_snapshot_selector: Option<String>,
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
    ExposedBaseContentUnavailable {
        path: String,
        content_id: bowline_core::ids::ContentId,
        source: CacheError,
    },
    ContentCache(CacheError),
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
    SnapshotMaterialization {
        snapshot_id: String,
        reason: String,
    },
    AcceptOperationMissing {
        operation_id: String,
    },
    AcceptOperationFailed {
        operation_id: String,
        reason: &'static str,
    },
    AcceptOperationCancelled {
        operation_id: String,
    },
    AcceptOperationPending {
        operation_id: String,
        state: crate::metadata::WorkViewAcceptOperationState,
    },
    NamespaceRead(NamespaceReadError),
    NamespaceBuild(NamespaceBuildError),
    CachedSnapshot(crate::sync::CachedSnapshotError),
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
            Self::ExposedBaseContentUnavailable {
                path,
                content_id,
                source,
            } => write!(
                formatter,
                "authoritative exposed content `{content_id}` for `{path}` is unavailable: {source}"
            ),
            Self::ContentCache(source) => {
                write!(formatter, "work-view content cache failed: {source}")
            }
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
            Self::SnapshotMaterialization {
                snapshot_id,
                reason,
            } => write!(
                formatter,
                "snapshot `{snapshot_id}` could not be materialized for work view: {reason}"
            ),
            Self::AcceptOperationMissing { operation_id } => {
                write!(
                    formatter,
                    "work-view accept operation `{operation_id}` was not found"
                )
            }
            Self::AcceptOperationFailed {
                operation_id,
                reason,
            } => write!(
                formatter,
                "work-view accept operation `{operation_id}` failed ({reason})"
            ),
            Self::AcceptOperationCancelled { operation_id } => write!(
                formatter,
                "work-view accept operation `{operation_id}` was cancelled before publish"
            ),
            Self::AcceptOperationPending {
                operation_id,
                state,
            } => write!(
                formatter,
                "work-view accept operation `{operation_id}` is still {state:?}; the daemon will continue it"
            ),
            Self::NamespaceRead(error) => {
                write!(formatter, "work-view namespace read failed: {error}")
            }
            Self::NamespaceBuild(error) => {
                write!(formatter, "work-view namespace build failed: {error}")
            }
            Self::CachedSnapshot(error) => {
                write!(formatter, "work-view cached snapshot load failed: {error}")
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
            Self::ExposedBaseContentUnavailable { source, .. } => Some(source),
            Self::ContentCache(error) => Some(error),
            Self::NamespaceRead(error) => Some(error),
            Self::NamespaceBuild(error) => Some(error),
            Self::CachedSnapshot(error) => Some(error),
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

impl From<CacheError> for WorkViewError {
    fn from(error: CacheError) -> Self {
        Self::ContentCache(error)
    }
}

impl From<NamespaceReadError> for WorkViewError {
    fn from(error: NamespaceReadError) -> Self {
        Self::NamespaceRead(error)
    }
}

impl From<NamespaceBuildError> for WorkViewError {
    fn from(error: NamespaceBuildError) -> Self {
        Self::NamespaceBuild(error)
    }
}

impl From<crate::sync::CachedSnapshotError> for WorkViewError {
    fn from(error: crate::sync::CachedSnapshotError) -> Self {
        Self::CachedSnapshot(error)
    }
}

#[derive(Debug)]
pub enum WorkViewOverlaySyncError {
    WorkView(WorkViewError),
    Metadata(MetadataError),
    ControlPlane(ControlPlaneError),
    WorkViewUpdate(WorkViewUpdateError),
    CommitCleanup {
        commit: WorkViewUpdateError,
        cleanup: Box<WorkViewOverlaySyncError>,
    },
    PublicationCleanup {
        publication: Box<WorkViewOverlaySyncError>,
        cleanup: Box<WorkViewOverlaySyncError>,
    },
    Packfile(PackfileError),
    ByteStore(ByteStoreError),
    Cache(CacheError),
    Json(serde_json::Error),
    Wire(overlay_wire::OverlayWireError),
    MissingOverlayPack,
    MissingStateRoot,
    MissingStagedContent,
    CancellationRequested,
    ClaimOwnershipLost,
}

impl fmt::Display for WorkViewOverlaySyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkView(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::WorkViewUpdate(error) => error.fmt(formatter),
            Self::CommitCleanup { commit, cleanup } => write!(
                formatter,
                "overlay commit failed ({commit}); proven-unreferenced object cleanup also failed ({cleanup})"
            ),
            Self::PublicationCleanup {
                publication,
                cleanup,
            } => write!(
                formatter,
                "overlay publication failed ({publication}); uploaded chunk cleanup also failed ({cleanup})"
            ),
            Self::Packfile(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Cache(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::Wire(error) => error.fmt(formatter),
            Self::MissingOverlayPack => write!(formatter, "overlay pack writer produced no pack"),
            Self::MissingStateRoot => write!(formatter, "metadata database path has no state root"),
            Self::MissingStagedContent => write!(formatter, "staged overlay content is missing"),
            Self::CancellationRequested => write!(formatter, "overlay sync was cancelled"),
            Self::ClaimOwnershipLost => write!(formatter, "overlay sync claim ownership was lost"),
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
            Self::CommitCleanup { commit, .. } => Some(commit),
            Self::PublicationCleanup { publication, .. } => Some(publication),
            Self::Packfile(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Cache(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Wire(error) => Some(error),
            Self::MissingOverlayPack
            | Self::MissingStateRoot
            | Self::MissingStagedContent
            | Self::CancellationRequested
            | Self::ClaimOwnershipLost => None,
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

impl From<CacheError> for WorkViewOverlaySyncError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<serde_json::Error> for WorkViewOverlaySyncError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<overlay_wire::OverlayWireError> for WorkViewOverlaySyncError {
    fn from(error: overlay_wire::OverlayWireError) -> Self {
        Self::Wire(error)
    }
}

#[cfg(test)]
mod tests;
