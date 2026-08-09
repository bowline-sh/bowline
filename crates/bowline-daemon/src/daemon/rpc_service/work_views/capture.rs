use std::collections::BTreeSet;
use std::path::Path;

use bowline_local::sync::manifest_engine::work_view::{
    WorkViewChange, capture_overlay, review_view,
};
use bowline_local::sync::manifest_engine::{
    ManifestKey, RemoteObjects, RemoteRef, project_view_verification_paths, stat_walk_project_view,
};
use serde::{Deserialize, Serialize};

use super::{WorkViewEngineEnv, WorkViewRpcError, view_engine};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::daemon::rpc_service) struct WorkUnresolvedPath {
    pub(in crate::daemon::rpc_service) path: String,
    pub(in crate::daemon::rpc_service) reason: String,
}

pub(super) struct CaptureOutcome {
    pub(super) overlay: ManifestKey,
    pub(super) unresolved_paths: Vec<WorkUnresolvedPath>,
}

pub(super) fn capture_view<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    current_overlay: &ManifestKey,
) -> Result<CaptureOutcome, WorkViewRpcError> {
    let (mut store, ctx) = view_engine(
        &env.workspace_root,
        &env.state_root,
        &env.workspace_id,
        env.crypto,
        &env.device_id,
        view_dir,
    )?;
    let policy =
        bowline_local::policy::UserPolicy::load(view_dir).map_err(WorkViewRpcError::engine)?;
    let ancestor = store.all_files().map_err(WorkViewRpcError::engine)?;
    let walk =
        stat_walk_project_view(view_dir, &policy, &ancestor).map_err(WorkViewRpcError::engine)?;
    let mut dirty: BTreeSet<_> = walk.dirty;
    let mut unresolved_paths = walk
        .unsyncable
        .into_iter()
        .map(|(path, record)| WorkUnresolvedPath {
            path: path.as_str().to_string(),
            reason: record.reason.tag().to_string(),
        })
        .collect::<Vec<_>>();
    dirty.extend(project_view_verification_paths(&policy, &ancestor));
    if dirty.is_empty() {
        return Ok(CaptureOutcome {
            overlay: current_overlay.clone(),
            unresolved_paths,
        });
    }
    let captured = capture_overlay(&mut store, &ctx, env.objects, current_overlay, &dirty)
        .map_err(WorkViewRpcError::engine)?;
    unresolved_paths.extend(captured.skipped.into_iter().map(|path| WorkUnresolvedPath {
        path: path.as_str().to_string(),
        reason: "changed-during-capture".to_string(),
    }));
    unresolved_paths
        .sort_by(|left, right| (&left.path, &left.reason).cmp(&(&right.path, &right.reason)));
    Ok(CaptureOutcome {
        overlay: captured.overlay.unwrap_or_else(|| current_overlay.clone()),
        unresolved_paths,
    })
}

pub(super) struct ReviewOutcome {
    pub(super) overlay: ManifestKey,
    pub(super) changes: Vec<WorkViewChange>,
    pub(super) unresolved_paths: Vec<WorkUnresolvedPath>,
}

pub(super) fn ensure_unresolved_acknowledged(
    unresolved_paths: &[WorkUnresolvedPath],
    acknowledged: &[String],
) -> Result<(), WorkViewRpcError> {
    let required = unresolved_paths
        .iter()
        .map(|issue| format!("{}={}", issue.path, issue.reason))
        .collect::<BTreeSet<_>>();
    if required.is_empty() {
        if acknowledged.is_empty() {
            return Ok(());
        }
    } else if required == acknowledged.iter().cloned().collect() {
        return Ok(());
    }
    Err(WorkViewRpcError::engine(format!(
        "unresolved work-view paths block acceptance; review again and pass each exact \
         --acknowledge-unresolved token: {}",
        required.into_iter().collect::<Vec<_>>().join(", ")
    )))
}

pub(super) fn review_view_dir<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    base: &ManifestKey,
    overlay: &ManifestKey,
) -> Result<ReviewOutcome, WorkViewRpcError> {
    let captured = capture_view(env, view_dir, overlay)?;
    let changes = review_view(env.objects, env.crypto, base, &captured.overlay)
        .map_err(WorkViewRpcError::engine)?;
    Ok(ReviewOutcome {
        overlay: captured.overlay,
        changes,
        unresolved_paths: captured.unresolved_paths,
    })
}
