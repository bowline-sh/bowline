//! Work-view RPC handlers (Plan 112 rewire): the daemon is a stateless engine
//! executor for the three work-view operations that need live transport —
//! create (materialize the current head into a view directory), review
//! (capture the view's edits, then manifest-diff base vs overlay), and accept
//! (capture, three-way merge into the current head, publish via the ordinary
//! manifest CAS). All persistent work-view *state* (the aux index riding
//! `.bowline-meta/aux-index` plus the metadata naming registry) is owned by the
//! CLI; the daemon receives manifest keys as parameters and returns manifest
//! keys as results.
//!
//! Every operation reuses the Plan 109 engine: materialize is a `pull` into the
//! view directory, capture is a `push` against a view-local ref, and the accept
//! publish is the same seal → create-only PUT → CAS contract as the core loop.
//! The daemon's own engine driver observes the accepted head through its ref
//! subscription and applies it to the workspace as an ordinary remote change.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_core::wire::generated::DaemonRpcErrorCode;

use crate::daemon::rpc_service::{
    CancellationPoint, RequestContext, RpcResult, checkpoint, internal_serialization_error,
    rpc_error,
};
use crate::daemon::{DaemonServerState, hosted_control_plane, key_store};

use bowline_daemon::manifest_transport::ManifestTransport;
use bowline_local::sync::manifest_engine::work_view::{
    ChangeKind, WorkViewChange, aside_prefix, diff_manifests, fetch_manifest,
    fetch_project_manifest, lift_project_manifest, materialize_view, project_manifest,
    three_way_merge,
};
use bowline_local::sync::manifest_engine::work_view_cli::partial_overlay;
use bowline_local::sync::manifest_engine::{
    CasOutcome, EngineConfig, EngineContext, EngineCounters, Manifest, ManifestKey, ManifestStore,
    ParentChain, ParentChainMode, PublishTreeRequest, RefVersionLookup, RemoteObjects, RemoteRef,
    UnledgeredNodes, WorkspaceCrypto, WorkspacePath, prepare_parent_chain,
    probe_endpoint_capabilities, publish_tree,
};
use serde::{Deserialize, Serialize};

use crate::daemon::sync::require_local_workspace_key;

mod accept_recovery;
mod capture;
mod path_validation;
#[cfg(test)]
pub(super) use accept_recovery::accept_intent_path;
use accept_recovery::{
    AcceptIntent, clear_accept_intent, finish_accepted_intent, project_relative_path,
    read_accept_intent, rebase_partial_overlay, write_accept_intent,
};
use capture::{WorkUnresolvedPath, capture_view, ensure_unresolved_acknowledged, review_view_dir};
use path_validation::{checked_project_path, checked_view_dir};

/// Private per-view engine state lives outside the synced workspace tree.
const VIEW_ENGINE_STATE_DIR: &str = "work-views";
const VIEW_ENGINE_DB_FILE: &str = "manifest_engine.sqlite3";
const WORK_VIEW_ACCEPT_MAX_ATTEMPTS: u8 = 3;

// ---- wire shapes ------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkCreateParams {
    view_dir: String,
    project_path: String,
    /// When present, rematerialize an existing synced view at this overlay
    /// instead of forking a new project view from the workspace head.
    #[serde(default)]
    overlay_manifest_key: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkCreateResult {
    base_manifest_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkViewOpParams {
    view_dir: String,
    project_path: String,
    base_manifest_key: String,
    overlay_manifest_key: String,
    /// Normalized `--path` selectors for a partial accept; empty = whole view.
    #[serde(default)]
    paths: Vec<String>,
    /// Exact `path=reason` tokens returned by review for unresolved paths.
    #[serde(default)]
    acknowledged_unresolved: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkReviewResult {
    overlay_manifest_key: String,
    changes: Vec<WorkChangeWire>,
    unresolved_paths: Vec<WorkUnresolvedPath>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkChangeWire {
    path: String,
    kind: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkAcceptResult {
    overlay_manifest_key: String,
    base_manifest_key: String,
    published_manifest_key: String,
    conflict_asides: Vec<String>,
    discarded_deletions: Vec<String>,
    aside_refused_paths: Vec<String>,
    accepted_paths: Vec<String>,
    unresolved_paths: Vec<WorkUnresolvedPath>,
    local_rebase_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_rebase_error: Option<String>,
}

fn change_kind_wire(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
}

fn changes_wire(changes: Vec<WorkViewChange>) -> Vec<WorkChangeWire> {
    changes
        .into_iter()
        .map(|change| WorkChangeWire {
            path: change.path.as_str().to_string(),
            kind: change_kind_wire(change.kind),
        })
        .collect()
}

// ---- engine environment (generic for tests, transport in production) --------

/// Everything a work-view engine operation needs. Generic over the remote
/// traits so tests drive the same code against an in-memory remote.
pub(super) struct WorkViewEngineEnv<'a, O: RemoteObjects, R: RemoteRef> {
    pub(super) crypto: &'a WorkspaceCrypto,
    pub(super) workspace_id: WorkspaceId,
    pub(super) device_id: DeviceId,
    pub(super) objects: &'a O,
    pub(super) refs: &'a R,
    /// The workspace root the view directory must resolve under. Used to walk the
    /// view-directory chain no-follow before any materialization touches disk.
    pub(super) workspace_root: PathBuf,
    pub(super) state_root: PathBuf,
}

#[derive(Debug)]
pub(super) enum WorkViewRpcError {
    /// The workspace has no synced head yet; a view has no base to fork from.
    NoSyncedHead,
    /// The workspace head advanced while accept was publishing; safe to retry.
    HeadAdvanced,
    /// The view directory routes through a symlink or non-directory component (a
    /// view-path segment, or the `.work` root itself, swapped for a symlink to an
    /// external directory). Materializing there could create, replace, or delete
    /// files outside the workspace root, so the operation is refused rather than
    /// following the escape.
    ViewDirEscape,
    Engine(String),
}

impl WorkViewRpcError {
    fn engine(error: impl std::fmt::Display) -> Self {
        Self::Engine(error.to_string())
    }
}

/// Validate — and create, no-follow — every directory from the workspace root
/// down to the view directory before any materialization touches disk.
///
/// `checked_view_dir` only guarantees the path is *lexically* under `.work`; a
/// component of that path (or the `.work` root itself) can still be a symlink to
/// a directory OUTSIDE the workspace and pass the lexical check. The engine
/// view directory is the materialization root, so [`prepare_parent_chain`] walks
/// the `.work` root and every view-path segment with `symlink_metadata` and refuses
/// (`Blocked`) on any symlink or file. That closes the escape where a symlinked
/// component would let materialization read, replace, or delete files outside the
/// workspace root.
fn prepare_view_dir_chain(workspace_root: &Path, view_dir: &Path) -> Result<(), WorkViewRpcError> {
    let relative = view_dir
        .strip_prefix(workspace_root)
        .map_err(|_| WorkViewRpcError::ViewDirEscape)?;
    let mut chain = relative.to_path_buf();
    chain.push(".bowline-view-content");
    let chain = chain.to_str().ok_or(WorkViewRpcError::ViewDirEscape)?;
    match prepare_parent_chain(
        workspace_root,
        &WorkspacePath::new(chain),
        ParentChainMode::CreateMissing,
    ) {
        ParentChain::Ready => Ok(()),
        ParentChain::Blocked => Err(WorkViewRpcError::ViewDirEscape),
    }
}

fn view_engine_dir(state_root: &Path, workspace_root: &Path, view_dir: &Path) -> PathBuf {
    let relative = view_dir
        .strip_prefix(workspace_root)
        .unwrap_or(view_dir)
        .to_string_lossy();
    let mut identity = blake3::Hasher::new();
    identity.update(workspace_root.to_string_lossy().as_bytes());
    identity.update(&[0]);
    identity.update(relative.as_bytes());
    let identity = identity.finalize().to_hex();
    state_root
        .join(VIEW_ENGINE_STATE_DIR)
        .join(identity.as_str())
}

fn view_engine(
    workspace_root: &Path,
    state_root: &Path,
    workspace_id: &WorkspaceId,
    env_crypto: &WorkspaceCrypto,
    device_id: &DeviceId,
    view_dir: &Path,
) -> Result<(ManifestStore, EngineContext), WorkViewRpcError> {
    // Trust boundary: refuse a view directory that routes through a symlinked
    // component before materializing project content. Engine state is kept under
    // the daemon-owned state root, never inside the project.
    prepare_view_dir_chain(workspace_root, view_dir)?;
    let engine_dir = view_engine_dir(state_root, workspace_root, view_dir);
    std::fs::create_dir_all(&engine_dir).map_err(WorkViewRpcError::engine)?;
    let store = ManifestStore::open(engine_dir.join(VIEW_ENGINE_DB_FILE))
        .map_err(WorkViewRpcError::engine)?;
    let capabilities = probe_endpoint_capabilities(view_dir);
    let ctx = EngineContext {
        process_identity: super::super::sync::engine_process_identity(),
        workspace_identity: workspace_id.clone(),
        crypto: env_crypto.clone(),
        device_id: device_id.clone(),
        names: capabilities.names,
        timestamps: capabilities.timestamps,
        engine_state_dir: engine_dir,
        endpoint_probe_root: view_dir.to_path_buf(),
        workspace_root: view_dir.to_path_buf(),
        config: EngineConfig::default(),
        project_view: true,
        counters: EngineCounters::shared(),
    };
    Ok((store, ctx))
}

fn materialize_existing_view<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    overlay: &ManifestKey,
    reset_state: bool,
) -> Result<(), WorkViewRpcError> {
    if reset_state {
        prepare_view_dir_chain(&env.workspace_root, view_dir)?;
        let engine_dir = view_engine_dir(&env.state_root, &env.workspace_root, view_dir);
        let database = engine_dir.join(VIEW_ENGINE_DB_FILE);
        if database.is_file() {
            let store = ManifestStore::open(&database).map_err(WorkViewRpcError::engine)?;
            let mut tracked = store
                .all_files()
                .map_err(WorkViewRpcError::engine)?
                .into_keys()
                .collect::<Vec<_>>();
            tracked.sort_by(|left, right| {
                right
                    .as_str()
                    .split('/')
                    .count()
                    .cmp(&left.as_str().split('/').count())
                    .then_with(|| right.cmp(left))
            });
            for path in tracked {
                let absolute = view_dir.join(path.as_str());
                let metadata = match std::fs::symlink_metadata(&absolute) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(WorkViewRpcError::engine(error)),
                };
                let removed = if metadata.is_dir() {
                    std::fs::remove_dir(&absolute)
                } else {
                    std::fs::remove_file(&absolute)
                };
                match removed {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(error) => return Err(WorkViewRpcError::engine(error)),
                }
            }
        }
        match std::fs::remove_dir_all(&engine_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(WorkViewRpcError::engine(error)),
        }
    }
    let (mut store, view_ctx) = view_engine(
        &env.workspace_root,
        &env.state_root,
        &env.workspace_id,
        env.crypto,
        &env.device_id,
        view_dir,
    )?;
    materialize_view(&mut store, &view_ctx, env.objects, overlay)
        .map_err(WorkViewRpcError::engine)?;
    Ok(())
}

/// Publish a project-scoped manifest as a tree and return its root key.
///
/// A work-view RPC owns no engine store, so it keeps no node ledger and seals
/// every node. Convergent sealing keeps that correct — re-PUTting an object that
/// already exists with identical bytes is a verified no-op — and a project view
/// is bounded by one project rather than the workspace.
fn publish_manifest<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    manifest: &Manifest,
) -> Result<ManifestKey, WorkViewRpcError> {
    publish_tree(PublishTreeRequest {
        objects: env.objects,
        crypto: env.crypto,
        counters: &EngineCounters::default(),
        manifest,
        ledger: &mut UnledgeredNodes,
    })
    .map_err(WorkViewRpcError::engine)
}

/// Create: materialize the current workspace head into the view directory and
/// return it as the view's base (= initial overlay) manifest key.
pub(super) fn create_project_view<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    project_path: &str,
) -> Result<ManifestKey, WorkViewRpcError> {
    let head = env
        .refs
        .read_ref()
        .map_err(WorkViewRpcError::engine)?
        .ok_or(WorkViewRpcError::NoSyncedHead)?;
    let workspace = fetch_manifest(env.objects, env.crypto, &head.manifest_key)
        .map_err(WorkViewRpcError::engine)?;
    let project = project_manifest(&workspace, project_path);
    let project_key = publish_manifest(env, &project)?;
    let (mut store, ctx) = view_engine(
        &env.workspace_root,
        &env.state_root,
        &env.workspace_id,
        env.crypto,
        &env.device_id,
        view_dir,
    )?;
    materialize_view(&mut store, &ctx, env.objects, &project_key)
        .map_err(WorkViewRpcError::engine)?;
    Ok(project_key)
}

#[cfg(test)]
fn create_view<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
) -> Result<ManifestKey, WorkViewRpcError> {
    create_project_view(env, view_dir, "apps/web")
}

#[derive(Debug)]
pub(super) struct AcceptOutcome {
    pub(super) overlay: ManifestKey,
    pub(super) base: ManifestKey,
    pub(super) published: ManifestKey,
    pub(super) conflict_asides: Vec<String>,
    /// Paths the overlay deleted but the current workspace head modified after
    /// the base. Their deletion did not land (the newer local edit stays
    /// canonical); they are excluded from `accepted_paths` and reported here.
    pub(super) discarded_deletions: Vec<String>,
    /// Paths that diverged both ways where no aside may be written (`.git/**`, or
    /// a name past the path budget). The local entry stays canonical and the
    /// view's version is dropped, so like a discarded deletion they are excluded
    /// from `accepted_paths` and reported here.
    pub(super) aside_refused_paths: Vec<String>,
    pub(super) accepted_paths: Vec<String>,
    pub(super) unresolved_paths: Vec<WorkUnresolvedPath>,
    pub(super) local_rebase_pending: bool,
    pub(super) local_rebase_error: Option<String>,
}

/// Accept: capture, three-way merge (ancestor = base, ours = current head,
/// theirs = overlay — filtered to `paths` when partial), publish the merged
/// manifest through the ordinary CAS. A merge that changes nothing (no matched
/// paths, or the head already carries the overlay) publishes nothing and
/// returns the current head.
pub(super) fn accept_project_view_dir<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    base: &ManifestKey,
    overlay: &ManifestKey,
    project_path: &str,
    paths: &[String],
    acknowledged_unresolved: &[String],
) -> Result<AcceptOutcome, WorkViewRpcError> {
    if let Some(intent) = read_accept_intent(env, view_dir)? {
        if !intent.matches(base, overlay, project_path, paths) {
            return Err(WorkViewRpcError::engine(
                "a different accept is pending recovery for this work view",
            ));
        }
        if let Some(version) = intent.published_ref_version {
            match env
                .refs
                .lookup_ref_version(version)
                .map_err(WorkViewRpcError::engine)?
            {
                RefVersionLookup::Found(key) if key.as_str() == intent.published_manifest_key => {
                    return finish_accepted_intent(env, view_dir, &intent);
                }
                RefVersionLookup::Unknown => {
                    return Ok(intent.outcome(
                        true,
                        Some(
                            "the accepted workspace ref is outside the recoverable history window"
                                .to_string(),
                        ),
                    ));
                }
                RefVersionLookup::Found(_) | RefVersionLookup::NotAdvanced => {}
            }
        } else {
            let current = env.refs.read_ref().map_err(WorkViewRpcError::engine)?;
            if current
                .as_ref()
                .is_some_and(|head| head.manifest_key.as_str() == intent.published_manifest_key)
            {
                return finish_accepted_intent(env, view_dir, &intent);
            }
        }
        clear_accept_intent(env, view_dir)?;
    }
    let captured = capture_view(env, view_dir, overlay)?;
    ensure_unresolved_acknowledged(&captured.unresolved_paths, acknowledged_unresolved)?;
    for attempt in 1..=WORK_VIEW_ACCEPT_MAX_ATTEMPTS {
        match accept_project_view_dir_once(
            env,
            AcceptProjectViewRequest {
                view_dir,
                base,
                requested_overlay: overlay,
                overlay: &captured.overlay,
                project_path,
                paths,
                unresolved_paths: &captured.unresolved_paths,
            },
        ) {
            Err(WorkViewRpcError::HeadAdvanced) if attempt < WORK_VIEW_ACCEPT_MAX_ATTEMPTS => {
                continue;
            }
            outcome => return outcome,
        }
    }
    Err(WorkViewRpcError::HeadAdvanced)
}

struct AcceptProjectViewRequest<'a> {
    view_dir: &'a Path,
    base: &'a ManifestKey,
    requested_overlay: &'a ManifestKey,
    overlay: &'a ManifestKey,
    project_path: &'a str,
    paths: &'a [String],
    unresolved_paths: &'a [WorkUnresolvedPath],
}

fn accept_project_view_dir_once<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    request: AcceptProjectViewRequest<'_>,
) -> Result<AcceptOutcome, WorkViewRpcError> {
    let overlay = request.overlay.clone();
    let head = env
        .refs
        .read_ref()
        .map_err(WorkViewRpcError::engine)?
        .ok_or(WorkViewRpcError::NoSyncedHead)?;
    let workspace = fetch_manifest(env.objects, env.crypto, &head.manifest_key)
        .map_err(WorkViewRpcError::engine)?;
    let base_manifest = fetch_project_manifest(env.objects, env.crypto, request.base)
        .map_err(WorkViewRpcError::engine)?;
    let overlay_manifest = fetch_project_manifest(env.objects, env.crypto, &overlay)
        .map_err(WorkViewRpcError::engine)?;
    let partial_overlay_snapshot;
    let accepted_paths;
    let effective_overlay = if request.paths.is_empty() {
        accepted_paths = diff_manifests(&base_manifest, &overlay_manifest)
            .into_iter()
            .map(|change| change.path.as_str().to_string())
            .collect();
        &overlay_manifest
    } else {
        let (partial, accepted) = partial_overlay(&base_manifest, &overlay_manifest, request.paths)
            .map_err(WorkViewRpcError::engine)?;
        partial_overlay_snapshot = partial;
        accepted_paths = accepted;
        &partial_overlay_snapshot
    };
    let workspace_base = lift_project_manifest(&base_manifest, request.project_path);
    let workspace_overlay = lift_project_manifest(effective_overlay, request.project_path);
    let merge = three_way_merge(
        &workspace_base,
        &workspace,
        &workspace_overlay,
        &aside_prefix(&overlay),
    );
    let discarded_deletions: Vec<String> = merge
        .discarded_deletions
        .iter()
        .map(|path| project_relative_path(path, request.project_path))
        .collect();
    let aside_refused_paths: Vec<String> = merge
        .aside_refused_paths
        .iter()
        .map(|path| project_relative_path(path, request.project_path))
        .collect();
    // Neither a discarded deletion nor a refused aside landed, so neither may be
    // reported as accepted: strip both from the set the caller records against
    // the view.
    let accepted_paths: Vec<String> = accepted_paths
        .into_iter()
        .filter(|path| !discarded_deletions.contains(path) && !aside_refused_paths.contains(path))
        .collect();
    let next_base_snapshot = project_manifest(&merge.merged, request.project_path);
    let next_base = publish_manifest(env, &next_base_snapshot)?;
    let next_overlay = if request.paths.is_empty() {
        overlay.clone()
    } else {
        let rebased = rebase_partial_overlay(
            &next_base_snapshot,
            &base_manifest,
            &overlay_manifest,
            &accepted_paths,
        );
        publish_manifest(env, &rebased)?
    };
    let candidate = if merge.merged == workspace {
        // Nothing to publish: the head already carries the accepted state (or the
        // only overlay change was a deletion the live workspace overrode).
        head.manifest_key.clone()
    } else {
        publish_manifest(env, &merge.merged)?
    };
    let intent = AcceptIntent {
        base_manifest_key: request.base.as_str().to_string(),
        requested_overlay_manifest_key: request.requested_overlay.as_str().to_string(),
        captured_overlay_manifest_key: overlay.as_str().to_string(),
        project_path: request.project_path.to_string(),
        paths: request.paths.to_vec(),
        next_overlay_manifest_key: next_overlay.as_str().to_string(),
        next_base_manifest_key: next_base.as_str().to_string(),
        published_manifest_key: candidate.as_str().to_string(),
        published_ref_version: Some(if merge.merged == workspace {
            head.version
        } else {
            head.version.saturating_add(1)
        }),
        conflict_asides: merge
            .conflict_asides
            .iter()
            .map(|path| project_relative_path(path, request.project_path))
            .collect(),
        discarded_deletions,
        aside_refused_paths,
        accepted_paths,
        unresolved_paths: request.unresolved_paths.to_vec(),
    };
    // The receipt is durable before authority changes remotely. If the daemon
    // terminates after CAS, the next identical request can prove whether this
    // exact candidate landed and finish the local rebase idempotently.
    write_accept_intent(env, request.view_dir, &intent)?;
    if merge.merged != workspace {
        match env
            .refs
            .compare_and_swap(Some(head.version), &candidate)
            .map_err(WorkViewRpcError::engine)?
        {
            CasOutcome::Advanced(_) => {}
            CasOutcome::Lost(_) => {
                clear_accept_intent(env, request.view_dir)?;
                return Err(WorkViewRpcError::HeadAdvanced);
            }
            CasOutcome::Ambiguous => {
                // Same resolution as the core push loop: adopt only if the current
                // head equals the candidate key.
                let current = env.refs.read_ref().map_err(WorkViewRpcError::engine)?;
                if current.map(|observed| observed.manifest_key) != Some(candidate.clone()) {
                    clear_accept_intent(env, request.view_dir)?;
                    return Err(WorkViewRpcError::HeadAdvanced);
                }
            }
        }
    }
    finish_accepted_intent(env, request.view_dir, &intent)
}

#[cfg(test)]
fn accept_view_dir<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    base: &ManifestKey,
    overlay: &ManifestKey,
    paths: &[String],
) -> Result<AcceptOutcome, WorkViewRpcError> {
    accept_project_view_dir(env, view_dir, base, overlay, "apps/web", paths, &[])
}

// ---- RPC glue ---------------------------------------------------------------

struct WorkRpcContext {
    crypto: WorkspaceCrypto,
    workspace_id: bowline_core::ids::WorkspaceId,
    device_id: DeviceId,
    workspace_root: PathBuf,
    state_root: PathBuf,
}

fn work_rpc_context(state: &DaemonServerState) -> RpcResult<WorkRpcContext> {
    let Some(args) = state.sync_args() else {
        return Err(rpc_error(
            DaemonRpcErrorCode::Unavailable,
            "work-view operations require a configured daemon workspace",
            false,
        ));
    };
    let workspace_key = require_local_workspace_key(args).map_err(|error| {
        rpc_error(
            DaemonRpcErrorCode::Unavailable,
            &format!("workspace key is unavailable for work views: {error}"),
            true,
        )
    })?;
    Ok(WorkRpcContext {
        crypto: workspace_key.workspace_crypto(&args.workspace_id),
        workspace_id: args.workspace_id.clone(),
        device_id: args.device_id.clone(),
        workspace_root: args.root.clone(),
        state_root: args.state_root.clone(),
    })
}

fn work_view_operation_lock(
    state: &DaemonServerState,
    workspace_id: &bowline_core::ids::WorkspaceId,
    view_dir: &Path,
) -> RpcResult<Arc<Mutex<()>>> {
    state
        .work_view_operation_lock(workspace_id, view_dir)
        .map_err(|()| {
            rpc_error(
                DaemonRpcErrorCode::Internal,
                "work-view operation lock registry is unavailable",
                false,
            )
        })
}

fn enter_work_view_operation(lock: &Mutex<()>) -> RpcResult<MutexGuard<'_, ()>> {
    lock.lock().map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::Internal,
            "work-view operation lock is unavailable",
            false,
        )
    })
}

fn work_rpc_result<T>(result: Result<T, WorkViewRpcError>) -> RpcResult<T> {
    result.map_err(|error| match error {
        WorkViewRpcError::NoSyncedHead => rpc_error(
            DaemonRpcErrorCode::Unavailable,
            "the workspace has no synced head yet; wait for the first sync to publish",
            true,
        ),
        WorkViewRpcError::HeadAdvanced => rpc_error(
            DaemonRpcErrorCode::Unavailable,
            "the workspace advanced while accept was publishing; retry",
            true,
        ),
        WorkViewRpcError::ViewDirEscape => rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "work-view directory routes through a symlink or non-directory component; refusing to materialize outside the workspace",
            false,
        ),
        WorkViewRpcError::Engine(message) => rpc_error(
            DaemonRpcErrorCode::Internal,
            &format!("work-view engine operation failed: {message}"),
            false,
        ),
    })
}

fn require_verified_peer(peer_credential_checked: bool) -> RpcResult<()> {
    if peer_credential_checked {
        return Ok(());
    }
    Err(rpc_error(
        DaemonRpcErrorCode::PermissionDenied,
        "work-view operations require a verified same-user local socket peer",
        false,
    ))
}

macro_rules! with_transport_env {
    ($rpc:expr, $env:ident, $body:expr) => {{
        let rpc = $rpc;
        let key_store = key_store().map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Unavailable,
                &format!("device key store is unavailable: {error}"),
                true,
            )
        })?;
        let control_plane =
            hosted_control_plane(&*key_store, rpc.workspace_id.clone(), rpc.device_id.clone())
                .map_err(|error| {
                    rpc_error(
                        DaemonRpcErrorCode::Unavailable,
                        &format!("hosted workspace service is unavailable: {error}"),
                        true,
                    )
                })?;
        let transport = ManifestTransport::new(
            &control_plane,
            rpc.workspace_id.clone(),
            rpc.device_id.clone(),
        );
        let $env = WorkViewEngineEnv {
            crypto: &rpc.crypto,
            workspace_id: rpc.workspace_id.clone(),
            device_id: rpc.device_id.clone(),
            objects: &transport,
            refs: &transport,
            workspace_root: rpc.workspace_root.clone(),
            state_root: rpc.state_root.clone(),
        };
        $body
    }};
}

pub(super) fn work_create(
    context: &RequestContext,
    state: &DaemonServerState,
    params: serde_json::Value,
    peer_credential_checked: bool,
) -> RpcResult<serde_json::Value> {
    require_verified_peer(peer_credential_checked)?;
    let params = serde_json::from_value::<WorkCreateParams>(params).map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "work.create params are invalid",
            false,
        )
    })?;
    let rpc = work_rpc_context(state)?;
    let view_dir = checked_view_dir(&rpc.workspace_root, &params.view_dir)?;
    let project_path = checked_project_path(&rpc.workspace_root, &view_dir, &params.project_path)?;
    let operation_lock = work_view_operation_lock(state, &rpc.workspace_id, &view_dir)?;
    let _operation = enter_work_view_operation(&operation_lock)?;
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let base = with_transport_env!(&rpc, env, {
        if let Some(overlay) = params.overlay_manifest_key {
            let overlay = ManifestKey::new(overlay);
            work_rpc_result(materialize_existing_view(&env, &view_dir, &overlay, true))?;
            overlay
        } else {
            work_rpc_result(create_project_view(&env, &view_dir, &project_path))?
        }
    });
    serde_json::to_value(WorkCreateResult {
        base_manifest_key: base.as_str().to_string(),
    })
    .map_err(internal_serialization_error)
}

pub(super) fn work_review(
    context: &RequestContext,
    state: &DaemonServerState,
    params: serde_json::Value,
    peer_credential_checked: bool,
) -> RpcResult<serde_json::Value> {
    require_verified_peer(peer_credential_checked)?;
    let params = parse_view_op_params(params, "work.review")?;
    let rpc = work_rpc_context(state)?;
    let view_dir = checked_view_dir(&rpc.workspace_root, &params.view_dir)?;
    checked_project_path(&rpc.workspace_root, &view_dir, &params.project_path)?;
    let operation_lock = work_view_operation_lock(state, &rpc.workspace_id, &view_dir)?;
    let _operation = enter_work_view_operation(&operation_lock)?;
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let outcome = with_transport_env!(
        &rpc,
        env,
        work_rpc_result(review_view_dir(
            &env,
            &view_dir,
            &ManifestKey::new(params.base_manifest_key.clone()),
            &ManifestKey::new(params.overlay_manifest_key.clone()),
        ))
    )?;
    serde_json::to_value(WorkReviewResult {
        overlay_manifest_key: outcome.overlay.as_str().to_string(),
        changes: changes_wire(outcome.changes),
        unresolved_paths: outcome.unresolved_paths,
    })
    .map_err(internal_serialization_error)
}

pub(super) fn work_accept(
    context: &RequestContext,
    state: &DaemonServerState,
    params: serde_json::Value,
    peer_credential_checked: bool,
) -> RpcResult<serde_json::Value> {
    require_verified_peer(peer_credential_checked)?;
    let params = parse_view_op_params(params, "work.accept")?;
    let rpc = work_rpc_context(state)?;
    let view_dir = checked_view_dir(&rpc.workspace_root, &params.view_dir)?;
    let project_path = checked_project_path(&rpc.workspace_root, &view_dir, &params.project_path)?;
    let operation_lock = work_view_operation_lock(state, &rpc.workspace_id, &view_dir)?;
    let _operation = enter_work_view_operation(&operation_lock)?;
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let outcome = with_transport_env!(
        &rpc,
        env,
        work_rpc_result(accept_project_view_dir(
            &env,
            &view_dir,
            &ManifestKey::new(params.base_manifest_key.clone()),
            &ManifestKey::new(params.overlay_manifest_key.clone()),
            &project_path,
            &params.paths,
            &params.acknowledged_unresolved,
        ))
    )?;
    serde_json::to_value(WorkAcceptResult {
        overlay_manifest_key: outcome.overlay.as_str().to_string(),
        base_manifest_key: outcome.base.as_str().to_string(),
        published_manifest_key: outcome.published.as_str().to_string(),
        conflict_asides: outcome.conflict_asides,
        discarded_deletions: outcome.discarded_deletions,
        aside_refused_paths: outcome.aside_refused_paths,
        accepted_paths: outcome.accepted_paths,
        unresolved_paths: outcome.unresolved_paths,
        local_rebase_pending: outcome.local_rebase_pending,
        local_rebase_error: outcome.local_rebase_error,
    })
    .map_err(internal_serialization_error)
}

fn parse_view_op_params(
    params: serde_json::Value,
    method: &'static str,
) -> RpcResult<WorkViewOpParams> {
    serde_json::from_value::<WorkViewOpParams>(params).map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            &format!("{method} params are invalid"),
            false,
        )
    })
}

#[cfg(test)]
#[path = "work_views/tests.rs"]
mod tests;
