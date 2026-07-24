//! CLI/daemon adapter for work views on the manifest engine (Plan 112 rewire).
//!
//! The durable work-view record is the encrypted auxiliary index riding the
//! reserved manifest entry [`super::aux_index::AUX_INDEX_PATH`]. Because that
//! entry syncs through the ordinary push/pull loop, its *materialized form* is
//! a plain file at `<workspace root>/.bowline-meta/aux-index` holding the
//! canonical plaintext. This module is the single owner of reading and writing
//! that file from the product surfaces (CLI commands and daemon RPC handlers):
//! mutating a work view is editing the file; the engine seals, uploads, and
//! republishes it exactly as it does any other workspace file.
//!
//! It also owns the synthesis of the frozen CLI wire struct
//! ([`bowline_core::work_views::WorkView`]) from an aux record plus the
//! metadata-DB naming registry, so the contract-v8 field names survive the
//! engine cutover unchanged.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bowline_core::ids::{SnapshotId, WorkViewId as WireWorkViewId, WorkspaceId};
use bowline_core::work_views::{
    OVERLAY_HEAD_EMPTY, WorkView, WorkViewLifecycle as WireLifecycle, WorkViewRetention,
    WorkViewRetentionState, WorkViewSyncState, WorkViewVisibility,
};

use super::aux_index::{
    AUX_INDEX_PATH, AuxDecodeLimits, AuxIndex, AuxIndexError, SealedAuxIndex, WorkViewId,
    WorkViewLifecycle, WorkViewRecord, decode_aux_index_plaintext,
};
use super::fs_guard::{AtomicWrite, write_private_file_atomic};
use super::push::PushError;

// ---- aux-index file IO ------------------------------------------------------

/// Read the materialized aux index from the workspace root. A missing file is an
/// empty index (no work views yet), never an error — the file first appears when
/// the first view is registered.
pub fn read_aux_index_file(workspace_root: &Path) -> Result<AuxIndex, WorkViewCliError> {
    let path = aux_index_file_path(workspace_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(AuxIndex::empty()),
        Err(error) => return Err(WorkViewCliError::Io(error)),
    };
    decode_aux_index_plaintext(&bytes, &AuxDecodeLimits::default()).map_err(WorkViewCliError::Aux)
}

/// Atomically write the aux index's canonical plaintext to its reserved path
/// (temp + rename in the same directory). The sync engine observes the write and
/// publishes it as an ordinary file edit.
///
/// The write goes through the engine's no-follow boundary rather than raw
/// directory-create + file-write primitives: the index materializes at
/// `.bowline-meta/aux-index`, and a hostile project could make `.bowline-meta`
/// (or a parent) a symlink pointing outside the workspace. A naive write would
/// follow it and overwrite external files as the Bowline user; the guard refuses
/// to descend through any non-directory and surfaces that as a blocked path.
pub fn write_aux_index_file(workspace_root: &Path, aux: &AuxIndex) -> Result<(), WorkViewCliError> {
    let bytes = aux.to_canonical_bytes().map_err(WorkViewCliError::Aux)?;
    match write_private_file_atomic(workspace_root, &SealedAuxIndex::manifest_path(), &bytes)
        .map_err(WorkViewCliError::Fs)?
    {
        AtomicWrite::Written => Ok(()),
        AtomicWrite::Blocked => Err(WorkViewCliError::WorkspacePathBlocked {
            path: AUX_INDEX_PATH,
        }),
    }
}

/// The materialized location of the aux index inside a workspace root.
pub fn aux_index_file_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(AUX_INDEX_PATH)
}

// ---- identity + lifecycle ---------------------------------------------------

/// Map an aux lifecycle onto the frozen wire enum. The wire enum retains a
/// `ReviewReady` variant for contract stability, but the manifest model has no
/// such state and it is never emitted.
pub fn wire_lifecycle(lifecycle: WorkViewLifecycle) -> WireLifecycle {
    match lifecycle {
        WorkViewLifecycle::Active => WireLifecycle::Active,
        WorkViewLifecycle::Accepted => WireLifecycle::Accepted,
        WorkViewLifecycle::Discarded => WireLifecycle::Discarded,
    }
}

/// Re-activate a discarded record (`work restore`). Distinct from
/// [`super::work_view::set_lifecycle`], which only transitions *out of* Active:
/// restore is the one sanctioned backward transition, and an accepted view is
/// never restorable (its overlay is already merged).
pub fn restore_record(aux: &mut AuxIndex, id: &WorkViewId) -> Result<(), WorkViewCliError> {
    let record = aux
        .work_views
        .get_mut(id)
        .ok_or_else(|| WorkViewCliError::UnknownView {
            id: id.as_str().to_string(),
        })?;
    match record.lifecycle {
        WorkViewLifecycle::Discarded => {
            record.lifecycle = WorkViewLifecycle::Active;
            Ok(())
        }
        WorkViewLifecycle::Active => Ok(()),
        WorkViewLifecycle::Accepted => Err(WorkViewCliError::Unrestorable {
            id: id.as_str().to_string(),
        }),
    }
}

// ---- wire synthesis ---------------------------------------------------------

/// Overlay the aux record's engine truth onto a metadata-registry wire row.
/// The metadata DB remains the naming registry (project, paths, timestamps);
/// the aux index is authoritative for identity, base, overlay, and lifecycle.
///
/// Field mapping (wire names frozen at contract v8):
/// - `baseSnapshotId` <- the base manifest key (`m_...`); the ref contract
///   already equates `SnapshotId` with the manifest object key (Plan 108).
/// - `overlayHead` <- the overlay manifest key, or [`OVERLAY_HEAD_EMPTY`] while
///   the overlay still equals the base (nothing captured yet).
/// - `overlayVersion` <- 0 while the overlay equals the base, else 1. The aux
///   record carries no revision counter; this field is vestigial wire
///   compatibility and dies at the next contract bump.
pub fn overlay_engine_truth(view: &mut WorkView, record: &WorkViewRecord) {
    view.base_snapshot_id = SnapshotId::new(record.base_manifest_key.as_str().to_string());
    if record.overlay_manifest_key == record.base_manifest_key {
        view.overlay_head = OVERLAY_HEAD_EMPTY.to_string();
        view.overlay_version = 0;
    } else {
        view.overlay_head = record.overlay_manifest_key.as_str().to_string();
        view.overlay_version = 1;
    }
    view.lifecycle = wire_lifecycle(record.lifecycle);
}

pub fn wire_view_from_record(
    workspace_id: &WorkspaceId,
    workspace_root: &Path,
    id: &WorkViewId,
    record: &WorkViewRecord,
) -> WorkView {
    let visible = crate::work_views::visible_path(
        &workspace_root.display().to_string(),
        &record.project_path,
        &record.name,
    );
    let retained = record.lifecycle != WorkViewLifecycle::Active;
    let mut view = WorkView {
        id: WireWorkViewId::new(id.as_str()),
        workspace_id: workspace_id.clone(),
        project_id: record.project_id.clone(),
        project_path: record.project_path.clone(),
        name: record.name.clone(),
        visible_path: crate::work_views::display_path(&visible),
        base_snapshot_id: SnapshotId::new(record.base_manifest_key.as_str()),
        overlay_head: OVERLAY_HEAD_EMPTY.to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: wire_lifecycle(record.lifecycle),
        visibility: if retained {
            WorkViewVisibility::Hidden
        } else {
            WorkViewVisibility::DefaultVisible
        },
        sync_state: WorkViewSyncState::Synced,
        retention: WorkViewRetention {
            state: if retained {
                WorkViewRetentionState::Retained
            } else {
                WorkViewRetentionState::Current
            },
            retain_until: None,
            restorable: record.lifecycle == WorkViewLifecycle::Discarded,
        },
        owner_device_id: Some(record.owner_device_id.clone()),
        followed_by: Vec::new(),
        host_materializations: if visible.is_dir() {
            vec![crate::work_views::display_path(&visible)]
        } else {
            Vec::new()
        },
        attention: Vec::new(),
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
    };
    overlay_engine_truth(&mut view, record);
    view
}

// ---- diff presentation + path selection -------------------------------------

/// Validate + normalize `--path` selectors: bounded length, workspace-relative,
/// no parent-directory components. The rules match the old work-view diff so
/// the CLI's error surface is unchanged.
pub fn normalize_path_selectors(patterns: &[String]) -> Result<Vec<String>, WorkViewCliError> {
    use std::path::Component;
    let mut selectors = Vec::new();
    for pattern in patterns {
        if pattern.len() > crate::glob::MAX_GLOB_MATCH_BYTES {
            return Err(WorkViewCliError::InvalidPathSelector {
                selector: pattern.clone(),
                reason: format!(
                    "must be at most {} bytes",
                    crate::glob::MAX_GLOB_MATCH_BYTES
                ),
            });
        }
        let normalized = bowline_core::workspace_graph::normalize_workspace_path(pattern);
        if normalized.is_empty() || Path::new(&normalized).is_absolute() {
            return Err(WorkViewCliError::InvalidPathSelector {
                selector: pattern.clone(),
                reason: "must be a workspace-relative path or glob".to_string(),
            });
        }
        if Path::new(&normalized)
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(WorkViewCliError::InvalidPathSelector {
                selector: pattern.clone(),
                reason: "must not contain parent-directory components".to_string(),
            });
        }
        selectors.push(normalized);
    }
    selectors.sort();
    selectors.dedup();
    Ok(selectors)
}

/// Whether a normalized selector matches a workspace-relative path (exact or
/// glob, same matcher as workspace policy).
pub fn selector_matches_path(selector: &str, path: &str) -> bool {
    selector == path || crate::glob::glob_matches(selector, path)
}

/// Present engine manifest-diff changes as the frozen CLI diff entries,
/// applying `--path` selectors. Paths are workspace-relative (a view is the
/// whole workspace under the manifest model). Selecting paths that match no
/// change is an error, matching the old CLI surface.
pub fn wire_diff_entries(
    view_name: &str,
    changes: &[(String, bowline_core::work_views::WorkDiffChangeKind)],
    patterns: &[String],
) -> Result<Vec<bowline_core::work_views::WorkDiffEntry>, WorkViewCliError> {
    use bowline_core::work_views::{WorkDiffChangeKind, WorkDiffEntry};
    let selectors = normalize_path_selectors(patterns)?;
    let mut entries = Vec::new();
    for (path, kind) in changes {
        if !selectors.is_empty()
            && !selectors
                .iter()
                .any(|selector| selector_matches_path(selector, path))
        {
            continue;
        }
        let verb = match kind {
            WorkDiffChangeKind::Added => "created",
            WorkDiffChangeKind::Modified => "modified",
            WorkDiffChangeKind::Deleted => "deleted",
            WorkDiffChangeKind::PolicyReview | WorkDiffChangeKind::Conflict => "changed",
        };
        entries.push(WorkDiffEntry {
            path: path.clone(),
            kind: *kind,
            summary: format!("{verb} in work view {view_name}"),
            contains_secrets: crate::policy::is_secret_bearing_path(path),
        });
    }
    if !selectors.is_empty() && entries.is_empty() {
        return Err(WorkViewCliError::EmptyPathSelection {
            patterns: selectors,
        });
    }
    Ok(entries)
}

/// Restrict an overlay to the selector-matched subset of its changes against
/// `base`, for partial accept: unmatched paths revert to the base entry, so the
/// merged head takes only the selected work. Returns the filtered overlay and
/// the matched changed paths (empty when nothing matched).
pub fn partial_overlay(
    base: &super::manifest::Manifest,
    overlay: &super::manifest::Manifest,
    patterns: &[String],
) -> Result<(super::manifest::Manifest, Vec<String>), WorkViewCliError> {
    let selectors = normalize_path_selectors(patterns)?;
    let mut entries = base.entries.clone();
    let mut matched = Vec::new();
    for change in super::work_view::diff_manifests(base, overlay) {
        let path = change.path.as_str();
        if !selectors
            .iter()
            .any(|selector| selector_matches_path(selector, path))
        {
            continue;
        }
        match overlay.entries.get(&change.path) {
            Some(entry) => {
                entries.insert(change.path.clone(), entry.clone());
            }
            None => {
                entries.remove(&change.path);
            }
        }
        matched.push(path.to_string());
    }
    Ok((
        super::manifest::Manifest::new(overlay.key_epoch, entries),
        matched,
    ))
}

// ---- errors -----------------------------------------------------------------

#[derive(Debug)]
pub enum WorkViewCliError {
    /// A plain (read-side) filesystem error reading the materialized index.
    Io(io::Error),
    /// A filesystem error surfaced by the no-follow write boundary (write side).
    Fs(PushError),
    /// The reserved index path could not be written because an intermediate
    /// component (or its temp leaf) is a symlink/file — an unusable workspace
    /// state, never written through. Carries the reserved path for the message.
    WorkspacePathBlocked {
        path: &'static str,
    },
    Aux(AuxIndexError),
    UnknownView {
        id: String,
    },
    Unrestorable {
        id: String,
    },
    InvalidPathSelector {
        selector: String,
        reason: String,
    },
    EmptyPathSelection {
        patterns: Vec<String>,
    },
}

impl fmt::Display for WorkViewCliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "work-view index file operation failed: {error}"),
            Self::Fs(error) => write!(formatter, "work-view index file operation failed: {error}"),
            Self::WorkspacePathBlocked { path } => write!(
                formatter,
                "work-view index path `{path}` is blocked by a symlink or file and cannot be \
                 written without escaping the workspace"
            ),
            Self::Aux(error) => write!(formatter, "work-view index is invalid: {error}"),
            Self::UnknownView { id } => write!(formatter, "unknown work view: {id}"),
            Self::Unrestorable { id } => {
                write!(
                    formatter,
                    "work view `{id}` was accepted and cannot be restored"
                )
            }
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
        }
    }
}

impl Error for WorkViewCliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Fs(error) => Some(error),
            Self::Aux(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "work_view_cli/tests.rs"]
mod tests;
