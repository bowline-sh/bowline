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

use super::*;

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use bowline_daemon::manifest_transport::ManifestTransport;
use bowline_local::sync::manifest_engine::work_view::{
    ChangeKind, WorkViewChange, aside_prefix, capture_overlay, diff_manifests, fetch_manifest,
    fetch_project_manifest, lift_project_manifest, materialize_view, project_manifest, review_view,
    three_way_merge,
};
use bowline_local::sync::manifest_engine::work_view_cli::partial_overlay;
use bowline_local::sync::manifest_engine::{
    CasOutcome, EngineConfig, EngineContext, EngineCounters, KeyEpoch, Manifest, ManifestKey,
    ManifestStore, ManifestUpload, ParentChain, ParentChainMode, RemoteObjects, RemoteRef,
    WorkspaceCrypto, WorkspacePath, physical_manifest_key, prepare_parent_chain,
    project_view_verification_paths, seal_manifest, stat_walk_project_view,
};
use serde::{Deserialize, Serialize};

use crate::daemon::sync::require_local_workspace_key;

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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkReviewResult {
    overlay_manifest_key: String,
    changes: Vec<WorkChangeWire>,
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
    accepted_paths: Vec<String>,
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
    )
    .map_err(WorkViewRpcError::engine)?
    {
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
    let ctx = EngineContext {
        crypto: env_crypto.clone(),
        device_id: device_id.clone(),
        engine_state_dir: engine_dir,
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
        env.crypto,
        &env.device_id,
        view_dir,
    )?;
    materialize_view(&mut store, &view_ctx, env.objects, overlay)
        .map_err(WorkViewRpcError::engine)?;
    Ok(())
}

fn publish_manifest<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    manifest: &Manifest,
) -> Result<ManifestKey, WorkViewRpcError> {
    let plaintext = manifest
        .to_canonical_bytes()
        .map_err(WorkViewRpcError::engine)?;
    let content_id = env.crypto.manifest_content_id(&plaintext);
    let sealed = seal_manifest(env.crypto, &plaintext).map_err(WorkViewRpcError::engine)?;
    let key = physical_manifest_key(sealed.as_bytes());
    env.objects
        .put_manifest(ManifestUpload {
            key: &key,
            content_id: &content_id,
            key_epoch: env.crypto.key_epoch(),
            sealed: sealed.as_bytes(),
        })
        .map_err(WorkViewRpcError::engine)?;
    Ok(key)
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

/// Capture any edits in the view directory as a new overlay manifest, returning
/// the (possibly unchanged) overlay key. A clean view captures nothing.
pub(super) fn capture_view<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    current_overlay: &ManifestKey,
) -> Result<ManifestKey, WorkViewRpcError> {
    let (mut store, ctx) = view_engine(
        &env.workspace_root,
        &env.state_root,
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
    dirty.extend(project_view_verification_paths(&policy, &ancestor));
    if dirty.is_empty() {
        return Ok(current_overlay.clone());
    }
    match capture_overlay(&mut store, &ctx, env.objects, current_overlay, &dirty)
        .map_err(WorkViewRpcError::engine)?
    {
        Some(new_overlay) => Ok(new_overlay),
        None => Ok(current_overlay.clone()),
    }
}

pub(super) struct ReviewOutcome {
    pub(super) overlay: ManifestKey,
    pub(super) changes: Vec<WorkViewChange>,
}

/// Review: capture the view's current edits, then manifest-diff base vs overlay.
pub(super) fn review_view_dir<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    base: &ManifestKey,
    overlay: &ManifestKey,
) -> Result<ReviewOutcome, WorkViewRpcError> {
    let overlay = capture_view(env, view_dir, overlay)?;
    let changes =
        review_view(env.objects, env.crypto, base, &overlay).map_err(WorkViewRpcError::engine)?;
    Ok(ReviewOutcome { overlay, changes })
}

pub(super) struct AcceptOutcome {
    pub(super) overlay: ManifestKey,
    pub(super) base: ManifestKey,
    pub(super) published: ManifestKey,
    pub(super) conflict_asides: Vec<String>,
    /// Paths the overlay deleted but the current workspace head modified after
    /// the base. Their deletion did not land (the newer local edit stays
    /// canonical); they are excluded from `accepted_paths` and reported here.
    pub(super) discarded_deletions: Vec<String>,
    pub(super) accepted_paths: Vec<String>,
}

fn rebase_partial_overlay(
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
) -> Result<AcceptOutcome, WorkViewRpcError> {
    let captured_overlay = capture_view(env, view_dir, overlay)?;
    for attempt in 1..=WORK_VIEW_ACCEPT_MAX_ATTEMPTS {
        match accept_project_view_dir_once(
            env,
            view_dir,
            base,
            &captured_overlay,
            project_path,
            paths,
        ) {
            Err(WorkViewRpcError::HeadAdvanced) if attempt < WORK_VIEW_ACCEPT_MAX_ATTEMPTS => {
                continue;
            }
            outcome => return outcome,
        }
    }
    Err(WorkViewRpcError::HeadAdvanced)
}

fn accept_project_view_dir_once<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    base: &ManifestKey,
    overlay: &ManifestKey,
    project_path: &str,
    paths: &[String],
) -> Result<AcceptOutcome, WorkViewRpcError> {
    let overlay = overlay.clone();
    let head = env
        .refs
        .read_ref()
        .map_err(WorkViewRpcError::engine)?
        .ok_or(WorkViewRpcError::NoSyncedHead)?;
    let workspace = fetch_manifest(env.objects, env.crypto, &head.manifest_key)
        .map_err(WorkViewRpcError::engine)?;
    let base_manifest =
        fetch_project_manifest(env.objects, env.crypto, base).map_err(WorkViewRpcError::engine)?;
    let overlay_manifest = fetch_project_manifest(env.objects, env.crypto, &overlay)
        .map_err(WorkViewRpcError::engine)?;
    let partial_overlay_snapshot;
    let accepted_paths;
    let effective_overlay = if paths.is_empty() {
        accepted_paths = diff_manifests(&base_manifest, &overlay_manifest)
            .into_iter()
            .map(|change| change.path.as_str().to_string())
            .collect();
        &overlay_manifest
    } else {
        let (partial, accepted) = partial_overlay(&base_manifest, &overlay_manifest, paths)
            .map_err(WorkViewRpcError::engine)?;
        partial_overlay_snapshot = partial;
        accepted_paths = accepted;
        &partial_overlay_snapshot
    };
    let workspace_base = lift_project_manifest(&base_manifest, project_path);
    let workspace_overlay = lift_project_manifest(effective_overlay, project_path);
    let merge = three_way_merge(
        &workspace_base,
        &workspace,
        &workspace_overlay,
        &aside_prefix(&overlay),
    );
    let discarded_deletions: Vec<String> = merge
        .discarded_deletions
        .iter()
        .map(|path| project_relative_path(path, project_path))
        .collect();
    // A discarded deletion did not land, so it must not be reported as accepted:
    // strip it from the accepted set the caller records against the view.
    let accepted_paths: Vec<String> = accepted_paths
        .into_iter()
        .filter(|path| !discarded_deletions.contains(path))
        .collect();
    let next_base_snapshot = project_manifest(&merge.merged, project_path);
    let next_base = publish_manifest(env, &next_base_snapshot)?;
    let next_overlay = if paths.is_empty() {
        overlay
    } else {
        let rebased = rebase_partial_overlay(
            &next_base_snapshot,
            &base_manifest,
            &overlay_manifest,
            &accepted_paths,
        );
        publish_manifest(env, &rebased)?
    };
    let published = if merge.merged == workspace {
        // Nothing to publish: the head already carries the accepted state (or the
        // only overlay change was a deletion the live workspace overrode).
        head.manifest_key
    } else {
        let key = publish_manifest(env, &merge.merged)?;
        match env
            .refs
            .compare_and_swap(Some(head.version), &key)
            .map_err(WorkViewRpcError::engine)?
        {
            CasOutcome::Advanced(_) => {}
            CasOutcome::Lost(_) => return Err(WorkViewRpcError::HeadAdvanced),
            CasOutcome::Ambiguous => {
                // Same resolution as the core push loop: adopt only if the current
                // head equals the candidate key.
                let current = env.refs.read_ref().map_err(WorkViewRpcError::engine)?;
                if current.map(|observed| observed.manifest_key) != Some(key.clone()) {
                    return Err(WorkViewRpcError::HeadAdvanced);
                }
            }
        }
        key
    };
    if !paths.is_empty() {
        materialize_existing_view(env, view_dir, &next_overlay, true)?;
    }
    Ok(AcceptOutcome {
        overlay: next_overlay,
        base: next_base,
        published,
        conflict_asides: merge
            .conflict_asides
            .into_iter()
            .map(|path| project_relative_path(&path, project_path))
            .collect(),
        discarded_deletions,
        accepted_paths,
    })
}

fn project_relative_path(path: &WorkspacePath, project_path: &str) -> String {
    let prefix = project_path.trim_matches('/');
    if prefix.is_empty() {
        return path.as_str().to_string();
    }
    path.as_str()
        .strip_prefix(&format!("{prefix}/"))
        .unwrap_or(path.as_str())
        .to_string()
}

#[cfg(test)]
fn accept_view_dir<O: RemoteObjects, R: RemoteRef>(
    env: &WorkViewEngineEnv<'_, O, R>,
    view_dir: &Path,
    base: &ManifestKey,
    overlay: &ManifestKey,
    paths: &[String],
) -> Result<AcceptOutcome, WorkViewRpcError> {
    accept_project_view_dir(env, view_dir, base, overlay, "apps/web", paths)
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
        crypto: WorkspaceCrypto::new(
            &args.workspace_id,
            workspace_key.bytes,
            KeyEpoch::new(workspace_key.key_epoch),
        ),
        workspace_id: bowline_core::ids::WorkspaceId::new(args.workspace_id.clone()),
        device_id: DeviceId::new(args.device_id.clone()),
        workspace_root: args.root.clone(),
        state_root: args.state_root.clone(),
    })
}

/// A view directory must live under the workspace's `.work/` tree — the daemon
/// never materializes to an arbitrary caller-supplied path.
fn checked_view_dir(root: &Path, view_dir: &str) -> RpcResult<PathBuf> {
    let path = PathBuf::from(view_dir);
    // A `..` component lets a lexically-valid path (`.work/../../x`) escape the
    // tree, so reject parent-dir traversal outright rather than trusting the
    // prefix check alone.
    let has_traversal = path
        .components()
        .any(|component| matches!(component, Component::ParentDir));
    // Require at least one component under `.work` so the `.work` root itself
    // cannot be used as a view directory (which would corrupt the overlay tree).
    let inside_work_tree = path.strip_prefix(root).is_ok_and(|relative| {
        let mut parts = relative.components();
        parts
            .next()
            .is_some_and(|first| first.as_os_str() == ".work")
            && parts.next().is_some()
    });
    if !path.is_absolute() || has_traversal || !inside_work_tree {
        return Err(rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "work-view directory must be inside the workspace .work tree",
            false,
        ));
    }
    Ok(path)
}

fn checked_project_path(root: &Path, view_dir: &Path, project_path: &str) -> RpcResult<String> {
    let project = Path::new(project_path);
    let canonical: PathBuf = project.components().collect();
    let normalized = project_path.is_empty()
        || (!project.is_absolute()
            && !project_path.starts_with('/')
            && project
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && canonical.to_str() == Some(project_path));
    let expected = view_dir
        .strip_prefix(root.join(".work"))
        .ok()
        .and_then(Path::parent);
    if !normalized || expected != Some(project) {
        return Err(rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "project path must be normalized and match the work-view directory scope",
            false,
        ));
    }
    Ok(project_path.to_string())
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
        ))
    )?;
    serde_json::to_value(WorkAcceptResult {
        overlay_manifest_key: outcome.overlay.as_str().to_string(),
        base_manifest_key: outcome.base.as_str().to_string(),
        published_manifest_key: outcome.published.as_str().to_string(),
        conflict_asides: outcome.conflict_asides,
        discarded_deletions: outcome.discarded_deletions,
        accepted_paths: outcome.accepted_paths,
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
