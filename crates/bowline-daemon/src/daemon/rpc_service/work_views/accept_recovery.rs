use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use bowline_core::fs_atomic::{AtomicWriteOptions, write_atomic};
use bowline_local::sync::manifest_engine::work_view::diff_manifests;
use bowline_local::sync::manifest_engine::{
    Manifest, ManifestKey, RemoteObjects, RemoteRef, WorkspacePath,
};
use serde::{Deserialize, Serialize};

use super::{
    AcceptOutcome, WorkUnresolvedPath, WorkViewEngineEnv, WorkViewRpcError,
    materialize_existing_view, view_engine_dir,
};

const ACCEPT_INTENT_FILE: &str = "accept-intent.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AcceptIntent {
    pub(super) base_manifest_key: String,
    pub(super) requested_overlay_manifest_key: String,
    pub(super) captured_overlay_manifest_key: String,
    pub(super) project_path: String,
    pub(super) paths: Vec<String>,
    pub(super) next_overlay_manifest_key: String,
    pub(super) next_base_manifest_key: String,
    pub(super) published_manifest_key: String,
    #[serde(default)]
    pub(super) published_ref_version: Option<u64>,
    pub(super) conflict_asides: Vec<String>,
    pub(super) discarded_deletions: Vec<String>,
    pub(super) aside_refused_paths: Vec<String>,
    pub(super) accepted_paths: Vec<String>,
    #[serde(default)]
    pub(super) unresolved_paths: Vec<WorkUnresolvedPath>,
}

impl AcceptIntent {
    pub(super) fn matches(
        &self,
        base: &ManifestKey,
        overlay: &ManifestKey,
        project_path: &str,
        paths: &[String],
    ) -> bool {
        let original_request = self.base_manifest_key == base.as_str()
            && self.requested_overlay_manifest_key == overlay.as_str();
        let transitioned_request = self.next_base_manifest_key == base.as_str()
            && self.next_overlay_manifest_key == overlay.as_str();
        (original_request || transitioned_request)
            && self.project_path == project_path
            && self.paths == paths
    }

    pub(super) fn outcome(
        &self,
        local_rebase_pending: bool,
        local_rebase_error: Option<String>,
    ) -> AcceptOutcome {
        AcceptOutcome {
            overlay: ManifestKey::new(self.next_overlay_manifest_key.clone()),
            base: ManifestKey::new(self.next_base_manifest_key.clone()),
            published: ManifestKey::new(self.published_manifest_key.clone()),
            conflict_asides: self.conflict_asides.clone(),
            discarded_deletions: self.discarded_deletions.clone(),
            aside_refused_paths: self.aside_refused_paths.clone(),
            accepted_paths: self.accepted_paths.clone(),
            unresolved_paths: self.unresolved_paths.clone(),
            local_rebase_pending,
            local_rebase_error,
        }
    }
}

pub(crate) fn accept_intent_path<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
) -> PathBuf {
    view_engine_dir(&env.state_root, &env.workspace_root, view_dir).join(ACCEPT_INTENT_FILE)
}

pub(super) fn read_accept_intent<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
) -> Result<Option<AcceptIntent>, WorkViewRpcError> {
    match fs::read(accept_intent_path(env, view_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(WorkViewRpcError::engine),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WorkViewRpcError::engine(error)),
    }
}

pub(super) fn write_accept_intent<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    intent: &AcceptIntent,
) -> Result<(), WorkViewRpcError> {
    let path = accept_intent_path(env, view_dir);
    let bytes = serde_json::to_vec(intent).map_err(WorkViewRpcError::engine)?;
    write_atomic(
        &path,
        &bytes,
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: true,
        },
    )
    .map_err(WorkViewRpcError::engine)
}

pub(super) fn clear_accept_intent<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
) -> Result<(), WorkViewRpcError> {
    match fs::remove_file(accept_intent_path(env, view_dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WorkViewRpcError::engine(error)),
    }
}

pub(super) fn rebase_partial_overlay(
    next_base: &Manifest,
    previous_base: &Manifest,
    captured: &Manifest,
    accepted_paths: &[String],
) -> Manifest {
    let accepted = accepted_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut rebased = next_base.clone();
    for change in diff_manifests(previous_base, captured) {
        if accepted.contains(change.path.as_str()) {
            continue;
        }
        match captured.entries.get(&change.path) {
            Some(entry) => {
                rebased.entries.insert(change.path, entry.clone());
            }
            None => {
                rebased.entries.remove(&change.path);
            }
        }
    }
    rebased
}

pub(super) fn finish_accepted_intent<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    intent: &AcceptIntent,
) -> Result<AcceptOutcome, WorkViewRpcError> {
    if !intent.paths.is_empty() {
        let overlay = ManifestKey::new(intent.next_overlay_manifest_key.clone());
        if let Err(error) = materialize_existing_view(env, view_dir, &overlay, false) {
            return Ok(intent.outcome(true, Some(format!("{error:?}"))));
        }
    }
    match clear_accept_intent(env, view_dir) {
        Ok(()) => Ok(intent.outcome(false, None)),
        Err(error) => Ok(intent.outcome(true, Some(format!("{error:?}")))),
    }
}

pub(super) fn project_relative_path(path: &WorkspacePath, project_path: &str) -> String {
    let prefix = project_path.trim_matches('/');
    if prefix.is_empty() {
        return path.as_str().to_string();
    }
    path.as_str()
        .strip_prefix(&format!("{prefix}/"))
        .unwrap_or(path.as_str())
        .to_string()
}
