use std::{collections::BTreeSet, fs, path::Path};

use bowline_core::{
    work_views::WorkView,
    workspace_graph::{NamespaceEntry, normalize_workspace_path},
};

use crate::metadata::MetadataStore;

use super::{
    WorkViewError, WorkViewOverlaySyncError,
    overlay_receive::checked_overlay_destination,
    overlay_wire::OverlayManifest,
    paths::{
        clean_accept_policy, expand_display_path, is_ignored_clean_accept_policy,
        is_source_control_metadata_path, workspace_path_for_project_file,
    },
};

pub(super) struct PreserveLocalOnlyRequest<'a> {
    pub(super) store: &'a MetadataStore,
    pub(super) work_view: &'a WorkView,
    pub(super) work_root: &'a Path,
    pub(super) staging_root: &'a Path,
    pub(super) project_prefix: &'a str,
    pub(super) exposed: &'a [NamespaceEntry],
    pub(super) prior_manifest: Option<&'a OverlayManifest>,
    pub(super) incoming_manifest: &'a OverlayManifest,
}

pub(super) fn preserve_local_only_files(
    request: PreserveLocalOnlyRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(), WorkViewOverlaySyncError> {
    let PreserveLocalOnlyRequest {
        store,
        work_view,
        work_root,
        staging_root,
        project_prefix,
        exposed,
        prior_manifest,
        incoming_manifest,
    } = request;
    let mut authoritative_paths = exposed
        .iter()
        .filter_map(|entry| {
            let relative = entry
                .path
                .strip_prefix(project_prefix)?
                .trim_start_matches('/');
            (!relative.is_empty()).then(|| relative.to_string())
        })
        .collect::<BTreeSet<_>>();
    for manifest in prior_manifest.into_iter().chain([incoming_manifest]) {
        for entry in manifest.operations() {
            checkpoint()?;
            authoritative_paths.insert(entry.path.clone());
            if let Some(from) = &entry.from {
                authoritative_paths.insert(from.clone());
            }
        }
    }
    preserve_local_tree(
        PreserveLocalTreeRequest {
            store,
            work_view,
            work_root,
            source: work_root,
            staging_root,
            authoritative_paths: &authoritative_paths,
        },
        checkpoint,
    )
}

#[derive(Clone, Copy)]
struct PreserveLocalTreeRequest<'a> {
    store: &'a MetadataStore,
    work_view: &'a WorkView,
    work_root: &'a Path,
    source: &'a Path,
    staging_root: &'a Path,
    authoritative_paths: &'a BTreeSet<String>,
}

fn preserve_local_tree(
    request: PreserveLocalTreeRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(), WorkViewOverlaySyncError> {
    for entry in fs::read_dir(request.source).map_err(WorkViewError::from)? {
        checkpoint()?;
        let entry = entry.map_err(WorkViewError::from)?;
        let source = entry.path();
        let relative_path = source.strip_prefix(request.work_root).map_err(|error| {
            WorkViewError::from(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;
        if is_source_control_metadata_path(relative_path) {
            continue;
        }
        let relative = normalize_workspace_path(&relative_path.display().to_string());
        let metadata = fs::symlink_metadata(&source).map_err(WorkViewError::from)?;
        let policy_local = metadata.is_file()
            && path_is_policy_local(request.store, request.work_view, &source, relative_path)?;
        let preserve = metadata.file_type().is_symlink()
            || !request.authoritative_paths.contains(&relative)
            || policy_local;
        let destination = checked_overlay_destination(request.staging_root, &relative)?;
        if metadata.is_dir() {
            if preserve {
                fs::create_dir_all(&destination).map_err(WorkViewError::from)?;
                fs::set_permissions(&destination, metadata.permissions())
                    .map_err(WorkViewError::from)?;
            }
            preserve_local_tree(
                PreserveLocalTreeRequest {
                    source: &source,
                    ..request
                },
                checkpoint,
            )?;
        } else if metadata.file_type().is_symlink() {
            if preserve {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).map_err(WorkViewError::from)?;
                }
                preserve_symlink(&source, &destination)?;
            }
        } else if metadata.is_file() && preserve {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(WorkViewError::from)?;
            }
            fs::copy(source, destination).map_err(WorkViewError::from)?;
        } else if !metadata.is_file() {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: source.display().to_string(),
                reason: "local-only path has an unsupported filesystem type",
            }
            .into());
        }
    }
    Ok(())
}

fn path_is_policy_local(
    store: &MetadataStore,
    work_view: &WorkView,
    source: &Path,
    relative: &Path,
) -> Result<bool, WorkViewOverlaySyncError> {
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    let workspace_path = workspace_path_for_project_file(work_view, relative);
    let policy = clean_accept_policy(
        store,
        &workspace_root,
        &work_view.workspace_id,
        &workspace_path,
        Some(source),
    )?;
    Ok(is_ignored_clean_accept_policy(
        policy.classification,
        policy.mode,
    ))
}

#[cfg(unix)]
fn preserve_symlink(source: &Path, destination: &Path) -> Result<(), WorkViewOverlaySyncError> {
    std::os::unix::fs::symlink(
        fs::read_link(source).map_err(WorkViewError::from)?,
        destination,
    )
    .map_err(WorkViewError::from)?;
    Ok(())
}

#[cfg(windows)]
fn preserve_symlink(source: &Path, destination: &Path) -> Result<(), WorkViewOverlaySyncError> {
    let target = fs::read_link(source).map_err(WorkViewError::from)?;
    if source.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(target, destination).map_err(WorkViewError::from)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination).map_err(WorkViewError::from)?;
    }
    Ok(())
}
