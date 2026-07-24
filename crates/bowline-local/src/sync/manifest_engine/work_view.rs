//! Work views rehosted on manifest identities (Plan 112 Step 3).
//!
//! A work view is nothing but two manifest keys and a lifecycle state carried in
//! the [`super::aux_index`]: a **base** (the workspace manifest it forked from)
//! and an **overlay** (another manifest whose entries are the view's current
//! truth — "base ⊕ overlay" materialized). Every operation reuses the Plan 109
//! engine rather than re-implementing sync:
//!
//! - **create**: register a record whose overlay starts equal to the base.
//! - **materialize**: [`pull`] the overlay manifest into the view directory.
//! - **edit**: the view directory is an ordinary directory; an agent edits it.
//! - **capture**: [`push`] the edited view against a *view-local* ref, uploading
//!   blobs to the shared object store and producing a new overlay key.
//! - **review**: a manifest diff of base vs overlay.
//! - **accept**: a three-way merge (ancestor = base, ours = current workspace,
//!   theirs = overlay) producing the manifest the ordinary push publishes;
//!   genuine both-sides divergence becomes a deterministic conflict-aside entry,
//!   exactly as the core loop's conflict-asides do.
//! - **discard**: drop the overlay reference (mark the record discarded).
//!
//! The view's ref is *not* hosted state — it is the overlay key stored in the
//! synced aux index, so the hosted service still holds only opaque blobs and the
//! one workspace CAS head (Plan 112 STOP condition: no per-view hosted state).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use super::aux_index::{AuxIndex, WorkViewId, WorkViewLifecycle, WorkViewRecord};
use super::manifest::{
    DecodeLimits, Manifest, ManifestError, ManifestKey, WorkspaceCrypto, WorkspacePath,
    open_manifest, physical_manifest_key,
};
use super::pull_apply::{PullDeps, PullError, PullOutcome, pull};
use super::push::{
    CasOutcome, EngineContext, PushDeps, PushError, PushOutcome, RefObservation, RemoteObjects,
    RemoteRef, TransportError, push_verifying_dirty_files,
};
use super::store::{ManifestStore, ManifestStoreError};

// ---- view-local ref ---------------------------------------------------------

/// A CAS ref private to one work view. It is seeded from the overlay key in the
/// aux index and lives only in memory for the duration of a materialize/capture
/// session; the durable overlay pointer is the aux-index record, never hosted
/// state. Reusing [`RemoteRef`] here lets [`pull`]/[`push`] drive a view exactly
/// as they drive the workspace, with no second apply path.
pub struct ViewLocalRef {
    state: RefCell<Option<RefObservation>>,
}

impl ViewLocalRef {
    /// A view whose head is `overlay` at `version`. A freshly created view whose
    /// overlay equals its base uses the base key here.
    pub fn seeded(overlay: ManifestKey, version: u64) -> Self {
        Self {
            state: RefCell::new(Some(RefObservation {
                version,
                manifest_key: overlay,
            })),
        }
    }

    /// The current overlay key after a capture advanced it.
    pub fn current_key(&self) -> Option<ManifestKey> {
        self.state
            .borrow()
            .as_ref()
            .map(|observed| observed.manifest_key.clone())
    }
}

impl RemoteRef for ViewLocalRef {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        Ok(self.state.borrow().clone())
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        let current = self.state.borrow().clone();
        let current_version = current.as_ref().map(|observed| observed.version);
        if current_version != expected_version {
            return Ok(CasOutcome::Lost(
                current.expect("lost implies a current view head"),
            ));
        }
        let version = current_version.unwrap_or(0) + 1;
        let observed = RefObservation {
            version,
            manifest_key: new_manifest_key.clone(),
        };
        *self.state.borrow_mut() = Some(observed.clone());
        Ok(CasOutcome::Advanced(observed))
    }
}

// ---- create / lifecycle (aux-index record shaping) --------------------------

/// A newly created record: the overlay starts equal to the base, so the view
/// begins as a faithful copy of the workspace it forked from. Its overlay
/// advances the first time the view is captured after an edit.
#[cfg(test)]
pub fn new_work_view_record(base: ManifestKey) -> WorkViewRecord {
    WorkViewRecord {
        project_id: bowline_core::ids::ProjectId::new("proj_test"),
        project_path: "project".to_string(),
        name: "test-view".to_string(),
        owner_device_id: bowline_core::ids::DeviceId::new("dev_test"),
        created_at: "2026-07-23T00:00:00Z".to_string(),
        updated_at: "2026-07-23T00:00:00Z".to_string(),
        base_manifest_key: base.clone(),
        overlay_manifest_key: base,
        lifecycle: WorkViewLifecycle::Active,
    }
}

/// Register a new work view in the aux index, returning the mutated index. The
/// caller uploads the index and republishes the workspace manifest (ordinary
/// push) to persist it.
#[cfg(test)]
pub fn register_work_view(aux: &mut AuxIndex, id: WorkViewId, base: ManifestKey) {
    aux.upsert(id, new_work_view_record(base));
}

/// Transition a record's lifecycle, refusing an illegal transition rather than
/// silently overwriting a terminal state.
pub fn set_lifecycle(
    aux: &mut AuxIndex,
    id: &WorkViewId,
    lifecycle: WorkViewLifecycle,
) -> Result<(), WorkViewError> {
    let record = aux
        .work_views
        .get_mut(id)
        .ok_or_else(|| WorkViewError::UnknownView {
            id: id.as_str().to_string(),
        })?;
    if record.lifecycle != WorkViewLifecycle::Active {
        return Err(WorkViewError::NotActive {
            id: id.as_str().to_string(),
        });
    }
    record.lifecycle = lifecycle;
    Ok(())
}

// ---- materialize / capture (engine reuse) -----------------------------------

/// Materialize a view: pull its overlay manifest into the view directory. The
/// `view_store`/`view_ctx` are scoped to the view directory (their own
/// `manifest_engine.sqlite3`); `objects` is the shared workspace object store so
/// the overlay's blobs are fetchable.
pub fn materialize_view<O: RemoteObjects>(
    view_store: &mut ManifestStore,
    view_ctx: &EngineContext,
    objects: &O,
    overlay: &ManifestKey,
) -> Result<PullOutcome, WorkViewError> {
    let state = view_store.engine_state().map_err(WorkViewError::Store)?;
    let version = state
        .highest_verified_ref_version
        .unwrap_or(0)
        .saturating_add(1);
    let refs = ViewLocalRef::seeded(overlay.clone(), version);
    let deps = PullDeps {
        ctx: view_ctx,
        objects,
        refs: &refs,
    };
    pull(view_store, &deps).map_err(WorkViewError::Pull)
}

/// Capture the current view directory as a new overlay: push the view's edits
/// against a view-local ref seeded at `current_overlay`, uploading changed blobs
/// to the shared object store. Returns the new overlay key, or `None` when the
/// view is unchanged (nothing to capture).
pub fn capture_overlay<O: RemoteObjects>(
    view_store: &mut ManifestStore,
    view_ctx: &EngineContext,
    objects: &O,
    current_overlay: &ManifestKey,
    dirty: &BTreeSet<WorkspacePath>,
) -> Result<Option<ManifestKey>, WorkViewError> {
    let state = view_store.engine_state().map_err(WorkViewError::Store)?;
    // Seed the view ref at the version the view's own store last applied, so the
    // capture's CAS precondition matches (materialize seeds version 1).
    let version = state.last_ref_version.unwrap_or(1);
    let refs = ViewLocalRef::seeded(current_overlay.clone(), version);
    let deps = PushDeps {
        ctx: view_ctx,
        objects,
        refs: &refs,
    };
    match push_verifying_dirty_files(view_store, &deps, dirty).map_err(WorkViewError::Push)? {
        PushOutcome::Advanced { manifest_key, .. } => Ok(Some(manifest_key)),
        PushOutcome::NoChange { .. } => Ok(None),
        PushOutcome::RefLost { .. } => Err(WorkViewError::ViewRefLost),
    }
}

// ---- review (manifest diff) -------------------------------------------------

/// One reviewable change between the base and the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewChange {
    pub path: WorkspacePath,
    pub kind: ChangeKind,
}

/// The kind of change a path underwent from base to overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Diff two decoded manifests, returning the sorted per-path changes. This is the
/// whole of "review": a manifest diff, no filesystem access.
pub fn diff_manifests(base: &Manifest, overlay: &Manifest) -> Vec<WorkViewChange> {
    let mut paths: BTreeSet<&WorkspacePath> = base.entries.keys().collect();
    paths.extend(overlay.entries.keys());
    let mut changes = Vec::new();
    for path in paths {
        let kind = match (base.entries.get(path), overlay.entries.get(path)) {
            (None, Some(_)) => Some(ChangeKind::Added),
            (Some(_), None) => Some(ChangeKind::Deleted),
            (Some(before), Some(after)) if before != after => Some(ChangeKind::Modified),
            _ => None,
        };
        if let Some(kind) = kind {
            changes.push(WorkViewChange {
                path: path.clone(),
                kind,
            });
        }
    }
    changes
}

pub fn project_manifest(snapshot: &Manifest, project_path: &str) -> Manifest {
    let prefix = project_path.trim_matches('/');
    let child_prefix = (!prefix.is_empty()).then(|| format!("{prefix}/"));
    let entries = snapshot
        .entries
        .iter()
        .filter_map(|(path, entry)| {
            let path = path.as_str();
            let relative = if prefix.is_empty() {
                path
            } else if path == prefix {
                ""
            } else {
                path.strip_prefix(child_prefix.as_deref()?)?
            };
            let workspace_internal = prefix.is_empty()
                && (relative == ".work"
                    || relative.starts_with(".work/")
                    || relative == ".bowline"
                    || relative.starts_with(".bowline/")
                    || relative == ".bowline-meta"
                    || relative.starts_with(".bowline-meta/"));
            if relative.is_empty() || workspace_internal {
                return None;
            }
            Some((WorkspacePath::new(relative), entry.clone()))
        })
        .collect();
    Manifest::new(snapshot.key_epoch, entries)
}

pub fn lift_project_manifest(snapshot: &Manifest, project_path: &str) -> Manifest {
    let prefix = project_path.trim_matches('/');
    if prefix.is_empty() {
        // A root project shares the workspace namespace. Project views allow
        // these names as content because nested projects may legitimately own
        // them, but an ordinary workspace manifest must never publish Bowline's
        // own private state.
        return project_manifest(snapshot, "");
    }
    Manifest::new(
        snapshot.key_epoch,
        snapshot
            .entries
            .iter()
            .map(|(path, entry)| {
                (
                    WorkspacePath::new(format!("{prefix}/{}", path.as_str())),
                    entry.clone(),
                )
            })
            .collect(),
    )
}

/// Fetch + verify + decode a manifest by key from the shared object store.
pub fn fetch_manifest<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    key: &ManifestKey,
) -> Result<Manifest, WorkViewError> {
    let sealed = objects
        .get_manifest(key)
        .map_err(WorkViewError::Transport)?;
    if &physical_manifest_key(&sealed) != key {
        return Err(WorkViewError::ManifestKeyMismatch);
    }
    let decoded = open_manifest(crypto, &sealed, &DecodeLimits::default())
        .map_err(WorkViewError::Manifest)?;
    Ok(decoded.manifest)
}

/// Fetch a project-scoped manifest, where workspace-reserved names are valid
/// project content.
pub fn fetch_project_manifest<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    key: &ManifestKey,
) -> Result<Manifest, WorkViewError> {
    let sealed = objects
        .get_manifest(key)
        .map_err(WorkViewError::Transport)?;
    if &physical_manifest_key(&sealed) != key {
        return Err(WorkViewError::ManifestKeyMismatch);
    }
    let decoded = open_manifest(crypto, &sealed, &DecodeLimits::project_view())
        .map_err(WorkViewError::Manifest)?;
    Ok(decoded.manifest)
}

/// Review a work view: fetch base + overlay and diff them.
pub fn review_view<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    base_manifest_key: &ManifestKey,
    overlay_manifest_key: &ManifestKey,
) -> Result<Vec<WorkViewChange>, WorkViewError> {
    let base = fetch_project_manifest(objects, crypto, base_manifest_key)?;
    let overlay = fetch_project_manifest(objects, crypto, overlay_manifest_key)?;
    Ok(diff_manifests(&base, &overlay))
}

// ---- accept (three-way merge) -----------------------------------------------

/// The outcome of accepting a work view into the workspace: the merged manifest
/// the caller publishes via the ordinary push CAS, plus the paths where the
/// workspace and the overlay both diverged from the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptMerge {
    pub merged: Manifest,
    /// Paths where both sides changed content and the overlay's version was
    /// preserved as a conflict-aside rather than overwriting canonical local
    /// bytes.
    pub conflict_asides: Vec<WorkspacePath>,
    /// Paths the overlay deleted but the workspace independently modified after
    /// the base. The newer workspace edit stays canonical — Bowline never
    /// destroys a live local file to honor a stale deletion — so the deletion is
    /// discarded and surfaced here. Unlike a conflict-aside there is no overlay
    /// content to materialize, so without this slot the discarded deletion would
    /// be invisible and accept would falsely report the path as landed.
    pub discarded_deletions: Vec<WorkspacePath>,
}

/// Three-way merge the overlay into the current workspace manifest, ancestor =
/// the base the view forked from. The rules match the core loop's merge matrix
/// projected onto manifests (no clock ordering; local bytes stay canonical on a
/// genuine conflict):
///
/// - overlay unchanged vs base → keep the workspace entry.
/// - workspace unchanged vs base → take the overlay entry (this is the whole
///   fast path when the workspace has not advanced since the view forked).
/// - both changed identically → either (take overlay).
/// - both changed differently, overlay kept content → keep the workspace entry
///   canonical; materialize the overlay entry at a deterministic conflict-aside
///   path keyed by `prefix` so two overlays' asides for one path never collide.
/// - both changed differently, overlay deleted the path → keep the newer
///   workspace entry canonical and record the discarded deletion; there is no
///   overlay content to aside, so the deletion is surfaced, not silently lost.
pub fn three_way_merge(
    base: &Manifest,
    workspace: &Manifest,
    overlay: &Manifest,
    prefix: &str,
) -> AcceptMerge {
    let mut merged = workspace.entries.clone();
    let mut conflict_asides = Vec::new();
    let mut discarded_deletions = Vec::new();

    let mut paths: BTreeSet<&WorkspacePath> = base.entries.keys().collect();
    paths.extend(workspace.entries.keys());
    paths.extend(overlay.entries.keys());

    for path in paths {
        let b = base.entries.get(path);
        let w = workspace.entries.get(path);
        let t = overlay.entries.get(path);

        if t == b {
            continue; // overlay did not touch this path; keep the workspace entry
        }
        if w == b {
            // Workspace untouched since fork: adopt the overlay's decision.
            match t {
                Some(entry) => {
                    merged.insert(path.clone(), entry.clone());
                }
                None => {
                    merged.remove(path);
                }
            }
            continue;
        }
        if w == t {
            continue; // both diverged but converged on the same result
        }
        // Genuine conflict: the workspace bytes stay canonical either way.
        match t {
            // Overlay kept content: preserve it as a deterministic aside so no
            // work is lost.
            Some(entry) => {
                let aside = conflict_aside_path(path, prefix);
                merged.insert(aside.clone(), entry.clone());
                conflict_asides.push(aside);
            }
            // Overlay deleted a path the workspace modified after the base. The
            // live workspace edit is newer, so the deletion cannot silently win;
            // there is nothing to aside, so record the discarded deletion.
            None => discarded_deletions.push(path.clone()),
        }
    }

    AcceptMerge {
        merged: Manifest::new(workspace.key_epoch, merged),
        conflict_asides,
        discarded_deletions,
    }
}

/// Accept a work view: fetch the three manifests and three-way merge them. The
/// returned [`AcceptMerge`] carries the manifest the workspace engine publishes;
/// the aux-index record is marked accepted by the caller after the push lands.
pub fn accept_view<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    record: &WorkViewRecord,
    workspace: &Manifest,
) -> Result<AcceptMerge, WorkViewError> {
    let base = fetch_project_manifest(objects, crypto, &record.base_manifest_key)?;
    let overlay = fetch_project_manifest(objects, crypto, &record.overlay_manifest_key)?;
    let prefix = aside_prefix(&record.overlay_manifest_key);
    Ok(three_way_merge(&base, workspace, &overlay, &prefix))
}

/// Deterministic conflict-aside path for accept: `<path> (overlay <prefix>)`.
/// No wall-clock component (Plan 108: conflict names are deterministic).
fn conflict_aside_path(path: &WorkspacePath, prefix: &str) -> WorkspacePath {
    WorkspacePath::new(format!("{} (overlay {prefix})", path.as_str()))
}

/// The first eight hex characters of the overlay manifest key, used to
/// disambiguate conflict-asides from different overlays. Public so accept
/// callers that pre-filter the overlay (partial accept) derive the same
/// deterministic aside names as [`accept_view`].
pub fn aside_prefix(key: &ManifestKey) -> String {
    key.as_str()
        .strip_prefix("m_")
        .unwrap_or(key.as_str())
        .chars()
        .take(8)
        .collect()
}

// ---- errors -----------------------------------------------------------------

#[derive(Debug)]
pub enum WorkViewError {
    Store(ManifestStoreError),
    Push(PushError),
    Pull(PullError),
    Manifest(ManifestError),
    Transport(TransportError),
    ManifestKeyMismatch,
    ViewRefLost,
    UnknownView { id: String },
    NotActive { id: String },
}

impl fmt::Display for WorkViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "work view store failed: {error}"),
            Self::Push(error) => write!(formatter, "work view capture failed: {error}"),
            Self::Pull(error) => write!(formatter, "work view materialize failed: {error}"),
            Self::Manifest(error) => write!(formatter, "work view manifest failed: {error}"),
            Self::Transport(error) => write!(formatter, "work view {error}"),
            Self::ManifestKeyMismatch => {
                formatter.write_str("work view manifest key does not match its object")
            }
            Self::ViewRefLost => {
                formatter.write_str("work view capture lost its private ref (concurrent capture)")
            }
            Self::UnknownView { id } => write!(formatter, "unknown work view: {id}"),
            Self::NotActive { id } => write!(formatter, "work view is not active: {id}"),
        }
    }
}

impl Error for WorkViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Push(error) => Some(error),
            Self::Pull(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "work_view/tests.rs"]
mod tests;
