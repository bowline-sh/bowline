use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::metadata::{
    MetadataStore, ProjectRecord, SnapshotRecord, WORK_VIEW_BASE_DESCRIPTOR_VERSION,
    WorkViewBaseDescriptor, default_database_path,
};
use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkCreateCommandOutput, WorkListCommandOutput},
    events::EventName,
    ids::{SnapshotId, WorkspaceId},
    status::{RepairCommand, WorkspaceStatus},
    work_views::{
        OVERLAY_HEAD_EMPTY, WorkCommandAction, WorkView, WorkViewLifecycle, WorkViewRetention,
        WorkViewRetentionState, WorkViewSyncState, WorkViewVisibility,
    },
};

use super::{
    WorkCreateOptions, WorkListOptions, WorkViewError,
    accept_transaction::{sync_parent, sync_tree},
    create_publish_checkpoint, materialize,
    paths::{
        append_work_event, display_path, ensure_fresh_materialization_path,
        ensure_no_symlink_ancestors, expand_display_path, open_store,
        project_has_pending_local_writes, remove_materialization_tree, status_for_work_views,
        validate_work_view_name, visible_path, work_view_id,
    },
    plan_snapshot_exposure,
};

#[cfg(test)]
pub(super) const MAX_INLINE_EXPOSED_BASE_BYTES: u64 = 1024 * 1024;

struct NewWorkViewParams {
    options: WorkCreateOptions,
    work_view_id_override: Option<bowline_core::ids::WorkViewId>,
    store: MetadataStore,
    workspace_id: WorkspaceId,
    root: String,
    project: ProjectRecord,
    workspace_content_key: Option<[u8; 32]>,
}

struct CreationBase {
    snapshot_id: SnapshotId,
    snapshot: Option<SnapshotRecord>,
    historical: bool,
}

struct CreationMaterialization {
    exposure_plan: CreationExposurePlan,
    cache_root: PathBuf,
    descriptor: WorkViewBaseDescriptor,
    exposed_snapshot: crate::sync::SnapshotContent,
}

enum CreationExposurePlan {
    Historical(super::exposure::SnapshotExposurePlan),
    Live {
        plan: super::exposure::WorkViewExposurePlan,
        workspace_content_key: [u8; 32],
    },
}

pub fn create_work_view(
    options: WorkCreateOptions,
) -> Result<WorkCreateCommandOutput, WorkViewError> {
    create_work_view_with_id(options, None)
}

pub(crate) fn create_work_view_with_id(
    options: WorkCreateOptions,
    work_view_id_override: Option<bowline_core::ids::WorkViewId>,
) -> Result<WorkCreateCommandOutput, WorkViewError> {
    create_work_view_with_id_and_key(options, work_view_id_override, None)
}

pub(crate) fn create_work_view_with_id_and_key(
    options: WorkCreateOptions,
    work_view_id_override: Option<bowline_core::ids::WorkViewId>,
    workspace_content_key: Option<[u8; 32]>,
) -> Result<WorkCreateCommandOutput, WorkViewError> {
    validate_work_view_name(&options.name)?;
    let db_path = options.db_path.clone();
    let store = open_store(db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let root = store
        .current_workspace_root()?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let project = store
        .current_project_by_path(&options.project_path)?
        .ok_or_else(|| WorkViewError::MissingProject {
            path: options.project_path.clone(),
        })?;
    let mut existing = store.work_views_by_name(&workspace.id, Some(&project.id), &options.name)?;
    if let [pending] = existing.as_slice()
        && is_pending_creation(pending, options.base_snapshot_selector.as_deref())
    {
        let pending_visible_path = expand_display_path(&pending.visible_path);
        let staging_path = creation_staging_path(&pending_visible_path, &pending.id);
        if pending_visible_path.is_dir() {
            let checkpoint_path = create_publish_checkpoint::checkpoint_path(&staging_path)?;
            let verified = create_publish_checkpoint::verify(
                &checkpoint_path,
                &pending.id,
                &pending_visible_path,
            )
            .map_err(|_| WorkViewError::UnsafeWorkViewPath {
                path: pending_visible_path.display().to_string(),
                reason: "pending work-view publication has no valid durable checkpoint",
            })?;
            if !verified {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: pending_visible_path.display().to_string(),
                    reason: "pending work-view publication has no matching durable checkpoint",
                });
            }
            sync_parent(&pending_visible_path)?;
            let mut published = pending.clone();
            published.lifecycle = WorkViewLifecycle::Active;
            published.host_materializations = vec![display_path(&pending_visible_path)];
            published.updated_at.clone_from(&options.generated_at);
            store.upsert_work_view(&published)?;
            append_work_event(
                &store,
                EventName::WorkCreated,
                &published,
                &options.generated_at,
            );
            cleanup_creation_checkpoint(&checkpoint_path);
            return Ok(existing_work_view_output(published, options.generated_at));
        }
        remove_materialization_tree(&staging_path);
        if store.delete_unpublished_work_view(&pending.workspace_id, &pending.id)? {
            existing.clear();
        }
    }
    if let [work_view] = existing.as_slice()
        && matches!(
            work_view.lifecycle,
            WorkViewLifecycle::Active | WorkViewLifecycle::ReviewReady
        )
        && options
            .base_snapshot_selector
            .as_deref()
            .is_none_or(|selector| {
                selector.strip_prefix("rp_").unwrap_or(selector)
                    == work_view.base_snapshot_id.as_str()
            })
        && expand_display_path(&work_view.visible_path).is_dir()
    {
        let visible_path = expand_display_path(&work_view.visible_path);
        let staging_path = creation_staging_path(&visible_path, &work_view.id);
        let checkpoint_path = create_publish_checkpoint::checkpoint_path(&staging_path)?;
        cleanup_creation_checkpoint(&checkpoint_path);
        return Ok(existing_work_view_output(
            work_view.clone(),
            options.generated_at,
        ));
    }
    if !existing.is_empty() {
        return Err(WorkViewError::NameCollision {
            name: options.name,
            project_path: project.path,
        });
    }
    create_new_work_view(NewWorkViewParams {
        options,
        work_view_id_override,
        store,
        workspace_id: workspace.id,
        root,
        project,
        workspace_content_key,
    })
}

fn create_new_work_view(
    mut params: NewWorkViewParams,
) -> Result<WorkCreateCommandOutput, WorkViewError> {
    let visible_path = visible_path(&params.root, &params.project.path, &params.options.name);
    ensure_no_symlink_ancestors(
        &visible_path,
        &expand_display_path(&params.root),
        "work view materialization escapes workspace",
    )?;
    let base = resolve_creation_base(&params)?;
    let historical_base = base.historical;
    let work_view = new_work_view(&params, &visible_path, base.snapshot_id);
    let materialization = prepare_creation_materialization(
        &params,
        &work_view,
        base.snapshot.as_ref(),
        base.historical,
    )?;
    ensure_fresh_materialization_path(&visible_path)?;
    let mut pending_work_view = work_view.clone();
    pending_work_view.lifecycle = WorkViewLifecycle::ReviewReady;
    pending_work_view.host_materializations.clear();
    persist_new_work_view(
        &mut params.store,
        &pending_work_view,
        &materialization.descriptor,
        &materialization.exposed_snapshot,
        &params.options.generated_at,
    )?;
    publish_new_work_view(&params, &work_view, &visible_path, materialization)?;
    let (status, mut next_actions) = work_create_freshness_status(historical_base);
    next_actions.push(RepairCommand::inspect(
        "Open the work view".to_string(),
        Some(format!(
            "cd {}",
            bowline_core::shell::quote_word(&work_view.visible_path)
        )),
    ));
    Ok(WorkCreateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::WorkCreate,
        generated_at: params.options.generated_at,
        action: WorkCommandAction::Created,
        work_view,
        status,
        next_actions,
    })
}

fn resolve_creation_base(params: &NewWorkViewParams) -> Result<CreationBase, WorkViewError> {
    let latest_snapshot_id = params
        .store
        .project_latest_snapshot_id(&params.workspace_id, &params.project.id)?;
    let base_snapshot_id = match params.options.base_snapshot_selector.as_deref() {
        Some(selector) => {
            let snapshot_id = selector.strip_prefix("rp_").unwrap_or(selector);
            if let Some(latest_snapshot_id) = latest_snapshot_id.as_ref()
                && snapshot_id == latest_snapshot_id.as_str()
            {
                latest_snapshot_id.clone()
            } else {
                resolve_base_snapshot_selector(&params.store, &params.workspace_id, selector)?
            }
        }
        None => latest_snapshot_id
            .clone()
            .ok_or_else(|| WorkViewError::MissingBaseSnapshot {
                path: params.project.path.clone(),
            })?,
    };
    let historical_base = latest_snapshot_id.as_ref() != Some(&base_snapshot_id);
    if !historical_base
        && project_has_pending_local_writes(
            &params.store,
            &params.workspace_id,
            &params.project.id,
            &params.project.path,
        )?
    {
        return Err(WorkViewError::DirtyProject {
            path: params.project.path.clone(),
        });
    }
    let snapshot = params
        .store
        .snapshot(&params.workspace_id, &base_snapshot_id)?;
    if snapshot.as_ref().is_some_and(|snapshot| {
        snapshot
            .project_id
            .as_ref()
            .is_some_and(|project_id| project_id != &params.project.id)
    }) {
        return Err(WorkViewError::SnapshotMaterialization {
            snapshot_id: base_snapshot_id.as_str().to_string(),
            reason: "retained snapshot belongs to a different project".to_string(),
        });
    }
    if snapshot.is_none() && !cfg!(test) {
        return Err(WorkViewError::SnapshotMaterialization {
            snapshot_id: base_snapshot_id.as_str().to_string(),
            reason: "retained snapshot manifest was not found locally".to_string(),
        });
    }
    Ok(CreationBase {
        snapshot_id: base_snapshot_id,
        snapshot,
        historical: historical_base,
    })
}

fn new_work_view(
    params: &NewWorkViewParams,
    visible_path: &Path,
    base_snapshot_id: SnapshotId,
) -> WorkView {
    WorkView {
        id: params.work_view_id_override.clone().unwrap_or_else(|| {
            work_view_id(
                params.workspace_id.as_str(),
                params.project.id.as_str(),
                &params.options.name,
            )
        }),
        workspace_id: params.workspace_id.clone(),
        project_id: params.project.id.clone(),
        project_path: params.project.path.clone(),
        name: params.options.name.clone(),
        visible_path: display_path(visible_path),
        base_snapshot_id: base_snapshot_id.clone(),
        overlay_head: OVERLAY_HEAD_EMPTY.to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: WorkViewLifecycle::Active,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: false,
        },
        owner_device_id: params.options.owner_device_id.clone(),
        followed_by: Vec::new(),
        host_materializations: vec![display_path(visible_path)],
        attention: Vec::new(),
        created_at: params.options.generated_at.clone(),
        updated_at: params.options.generated_at.clone(),
    }
}

fn prepare_creation_materialization(
    params: &NewWorkViewParams,
    work_view: &WorkView,
    snapshot_record: Option<&SnapshotRecord>,
    historical: bool,
) -> Result<CreationMaterialization, WorkViewError> {
    #[cfg(test)]
    if snapshot_record.is_none() {
        return prepare_test_live_creation_materialization(params, work_view);
    }
    let snapshot_record =
        snapshot_record.ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "retained snapshot manifest was not found locally".to_string(),
        })?;
    let snapshot = crate::sync::load_cached_snapshot(&params.store, snapshot_record)?;
    let entries = materialize::snapshot_exposed_entries(&snapshot, &work_view.project_path)?;
    let snapshot_plan = plan_snapshot_exposure(
        &expand_display_path(&params.root),
        &work_view.project_path,
        entries,
    )?;
    let exposure_plan = if historical {
        CreationExposurePlan::Historical(snapshot_plan)
    } else if let Some((plan, workspace_content_key)) =
        validate_live_tree_matches_snapshot(params, work_view, &snapshot_plan)?
    {
        CreationExposurePlan::Live {
            plan,
            workspace_content_key,
        }
    } else {
        CreationExposurePlan::Historical(snapshot_plan)
    };
    let cache_root = state_cache_root(params.options.db_path.as_deref())?;
    let exposed_entries = match &exposure_plan {
        CreationExposurePlan::Historical(plan) => plan.entries.clone(),
        CreationExposurePlan::Live { plan, .. } => plan
            .entries
            .iter()
            .map(|planned| planned.entry.clone())
            .collect(),
    };
    let policy_fingerprint = match &exposure_plan {
        CreationExposurePlan::Historical(plan) => plan.policy_fingerprint.clone(),
        CreationExposurePlan::Live { plan, .. } => plan.policy_fingerprint.clone(),
    };
    let exposed_snapshot =
        super::namespace::build_exposed_snapshot(&snapshot, exposed_entries.clone())?;
    let descriptor = exposed_base_descriptor(
        work_view,
        policy_fingerprint,
        &exposed_snapshot,
        &params.options.generated_at,
    );
    Ok(CreationMaterialization {
        exposure_plan,
        cache_root,
        descriptor,
        exposed_snapshot,
    })
}

fn validate_live_tree_matches_snapshot(
    params: &NewWorkViewParams,
    work_view: &WorkView,
    snapshot_plan: &super::exposure::SnapshotExposurePlan,
) -> Result<Option<(super::exposure::WorkViewExposurePlan, [u8; 32])>, WorkViewError> {
    use std::collections::BTreeMap;

    use bowline_core::workspace_graph::NamespaceEntryKind;

    let workspace_root = expand_display_path(&params.root);
    let mut live = super::plan_live_tree_exposure(&workspace_root, &work_view.project_path)?;
    let cache = bowline_storage::LocalContentCache::open(state_cache_root(
        params.options.db_path.as_deref(),
    )?)?;
    let mut canonical = snapshot_plan
        .entries
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut live_key = None;
    for planned in &mut live.entries {
        let canonical_entry = canonical
            .remove(&planned.entry.path)
            .filter(|entry| entry.kind == planned.entry.kind)
            .ok_or_else(|| WorkViewError::FreshCanonicalSnapshotRequired {
                path: planned.entry.path.clone(),
            })?;
        if planned.entry.kind == NamespaceEntryKind::File {
            let content_id = canonical_entry.content_id.as_ref().ok_or_else(|| {
                WorkViewError::FreshCanonicalSnapshotRequired {
                    path: planned.entry.path.clone(),
                }
            })?;
            match super::content_identity::verified_content_matches_path(
                &cache,
                content_id,
                &planned.source_path,
            ) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(WorkViewError::FreshCanonicalSnapshotRequired {
                        path: planned.entry.path.clone(),
                    });
                }
                Err(WorkViewError::ExposedBaseContentUnavailable { .. }) => {
                    let key = match live_key {
                        Some(key) => key,
                        None => {
                            let key = match params.workspace_content_key {
                                Some(key) => key,
                                None => workspace_content_key(&params.workspace_id, work_view)?,
                            };
                            live_key = Some(key);
                            key
                        }
                    };
                    if !super::content_identity::workspace_content_matches_path(
                        key,
                        content_id,
                        &planned.source_path,
                    )? {
                        return Err(WorkViewError::FreshCanonicalSnapshotRequired {
                            path: planned.entry.path.clone(),
                        });
                    }
                }
                Err(error) => return Err(error),
            }
        }
        planned.entry = canonical_entry;
    }
    if let Some((path, _)) = canonical.into_iter().next() {
        return Err(WorkViewError::FreshCanonicalSnapshotRequired { path });
    }
    Ok(live_key.map(|key| (live, key)))
}

fn workspace_content_key(
    workspace_id: &WorkspaceId,
    work_view: &WorkView,
) -> Result<[u8; 32], WorkViewError> {
    let key_store = crate::device_keys::default_device_key_store().map_err(|_| {
        WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "workspace key is unavailable for canonical live-tree verification".to_string(),
        }
    })?;
    let material = key_store
        .load_workspace_key(workspace_id)
        .map_err(|_| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "workspace key is unavailable for canonical live-tree verification".to_string(),
        })?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "workspace key is unavailable for canonical live-tree verification".to_string(),
        })?;
    crate::device_keys::workspace_key_bytes(&material.key_bytes).map_err(|_| {
        WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "workspace key is unavailable for canonical live-tree verification".to_string(),
        }
    })
}

#[cfg(test)]
fn prepare_test_live_creation_materialization(
    params: &NewWorkViewParams,
    work_view: &WorkView,
) -> Result<CreationMaterialization, WorkViewError> {
    use bowline_core::workspace_graph::{NamespaceEntryKind, workspace_content_id};
    use bowline_storage::LocalContentCache;

    let workspace_root = expand_display_path(&params.root);
    let live = super::plan_live_tree_exposure(&workspace_root, &work_view.project_path)?;
    let cache_root = state_cache_root(params.options.db_path.as_deref())?;
    let cache = LocalContentCache::open(&cache_root)?;
    let key = [0_u8; 32];
    let mut entries = Vec::with_capacity(live.entries.len());
    let mut files = std::collections::BTreeMap::new();
    for planned in live.entries {
        let mut entry = planned.entry;
        if entry.kind == NamespaceEntryKind::File {
            if entry.byte_len.unwrap_or_default() > MAX_INLINE_EXPOSED_BASE_BYTES {
                return Err(WorkViewError::FreshCanonicalSnapshotRequired {
                    path: entry.path.clone(),
                });
            }
            let identity = planned.source_identity.ok_or_else(|| {
                WorkViewError::ContentChangedDuringCapture {
                    path: planned.source_path.display().to_string(),
                }
            })?;
            let bytes =
                super::content_identity::capture_stable_bytes(&planned.source_path, identity)?;
            let content_id = workspace_content_id(key, &bytes);
            cache.put_content(&content_id, &bytes)?;
            cache.get_content(&content_id, key)?;
            entry.content_id = Some(content_id.clone());
            files.insert(content_id, bytes);
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let identity =
        crate::sync::rebuild_manifest_identity(&work_view.workspace_id, &entries, "test");
    let exposed_snapshot = crate::sync::SnapshotContent::new(
        bowline_core::workspace_graph::SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: identity.snapshot_id,
            workspace_id: work_view.workspace_id.clone(),
            project_id: Some(work_view.project_id.clone()),
            kind: bowline_core::workspace_graph::SnapshotKind::Base,
            base_snapshot_id: Some(work_view.base_snapshot_id.clone()),
            entries: entries.clone(),
            refs: Vec::new(),
        },
        files,
        key,
    )?;
    let descriptor = exposed_base_descriptor(
        work_view,
        live.policy_fingerprint.clone(),
        &exposed_snapshot,
        &params.options.generated_at,
    );
    Ok(CreationMaterialization {
        exposure_plan: CreationExposurePlan::Historical(super::exposure::SnapshotExposurePlan {
            entries,
            policy_fingerprint: live.policy_fingerprint,
        }),
        cache_root,
        descriptor,
        exposed_snapshot,
    })
}

fn publish_new_work_view(
    params: &NewWorkViewParams,
    work_view: &WorkView,
    visible_path: &Path,
    materialization: CreationMaterialization,
) -> Result<(), WorkViewError> {
    let staging_path = creation_staging_path(visible_path, &work_view.id);
    remove_materialization_tree(&staging_path);
    let staging_result = (|| -> std::io::Result<()> {
        if let Some(parent) = staging_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir(&staging_path)
    })();
    if let Err(error) = staging_result {
        cleanup_unpublished_creation(&params.store, work_view, &staging_path);
        return Err(error.into());
    }
    let materialization_result = match &materialization.exposure_plan {
        CreationExposurePlan::Historical(plan) => materialize::materialize_snapshot_exposure_plan(
            plan,
            &work_view.project_path,
            &materialization.cache_root,
            &staging_path,
        ),
        CreationExposurePlan::Live {
            plan,
            workspace_content_key,
        } => {
            materialize::materialize_live_exposure_plan(plan, *workspace_content_key, &staging_path)
        }
    };
    if let Err(error) = materialization_result {
        cleanup_unpublished_creation(&params.store, work_view, &staging_path);
        return Err(error);
    }
    if let Err(error) = sync_tree(&staging_path) {
        cleanup_unpublished_creation(&params.store, work_view, &staging_path);
        return Err(error.into());
    }
    let checkpoint_path = create_publish_checkpoint::checkpoint_path(&staging_path)?;
    if let Err(error) =
        create_publish_checkpoint::write(&checkpoint_path, &work_view.id, &staging_path)
    {
        cleanup_unpublished_creation(&params.store, work_view, &staging_path);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&staging_path, visible_path) {
        remove_materialization_tree(&staging_path);
        if !visible_path.is_dir() {
            cleanup_creation_checkpoint(&checkpoint_path);
            cleanup_unpublished_creation(&params.store, work_view, &staging_path);
        }
        return Err(error.into());
    }
    sync_parent(visible_path)?;
    params.store.upsert_work_view(work_view)?;
    append_work_event(
        &params.store,
        EventName::WorkCreated,
        work_view,
        &params.options.generated_at,
    );
    cleanup_creation_checkpoint(&checkpoint_path);
    Ok(())
}

fn exposed_base_descriptor(
    work_view: &bowline_core::work_views::WorkView,
    policy_fingerprint: String,
    snapshot: &crate::sync::SnapshotContent,
    created_at: &str,
) -> WorkViewBaseDescriptor {
    let manifest = snapshot.manifest();
    WorkViewBaseDescriptor {
        format_version: WORK_VIEW_BASE_DESCRIPTOR_VERSION,
        workspace_id: work_view.workspace_id.clone(),
        project_id: work_view.project_id.clone(),
        work_view_id: work_view.id.clone(),
        base_snapshot_id: work_view.base_snapshot_id.clone(),
        project_prefix: work_view.project_path.clone(),
        policy_fingerprint,
        exposed_snapshot_id: manifest.snapshot_id.clone(),
        exposed_namespace_root_id: manifest.namespace_root_id.clone(),
        exposed_semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
        exposed_entry_count: manifest.entry_count,
        created_at: created_at.to_string(),
    }
}

fn existing_work_view_output(work_view: WorkView, generated_at: String) -> WorkCreateCommandOutput {
    let next_actions = vec![RepairCommand::inspect(
        "Open the work view".to_string(),
        Some(format!(
            "cd {}",
            bowline_core::shell::quote_word(&work_view.visible_path)
        )),
    )];
    let status = status_for_work_views(std::slice::from_ref(&work_view));
    WorkCreateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::WorkCreate,
        generated_at,
        action: WorkCommandAction::Reused,
        work_view,
        status,
        next_actions,
    }
}

fn is_pending_creation(work_view: &WorkView, requested_base: Option<&str>) -> bool {
    work_view.lifecycle == WorkViewLifecycle::ReviewReady
        && work_view.host_materializations.is_empty()
        && requested_base.is_none_or(|selector| {
            selector.strip_prefix("rp_").unwrap_or(selector) == work_view.base_snapshot_id.as_str()
        })
}

fn creation_staging_path(
    visible_path: &Path,
    work_view_id: &bowline_core::ids::WorkViewId,
) -> PathBuf {
    let digest = blake3::hash(work_view_id.as_str().as_bytes()).to_hex();
    visible_path
        .parent()
        .unwrap_or(visible_path)
        .join(format!(".bowline-create-{}", &digest[..24]))
}

fn cleanup_unpublished_creation(store: &MetadataStore, work_view: &WorkView, staging_path: &Path) {
    remove_materialization_tree(staging_path);
    if let Ok(checkpoint_path) = create_publish_checkpoint::checkpoint_path(staging_path) {
        cleanup_creation_checkpoint(&checkpoint_path);
    }
    match store.delete_unpublished_work_view(&work_view.workspace_id, &work_view.id) {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "bowline work-view unpublished-create cleanup lost ownership for {}",
            work_view.id.as_str()
        ),
        Err(error) => {
            eprintln!(
                "bowline work-view unpublished-create cleanup failed for {}: {error}",
                work_view.id.as_str()
            );
        }
    }
}

fn cleanup_creation_checkpoint(checkpoint_path: &Path) {
    if let Err(error) = create_publish_checkpoint::remove(checkpoint_path) {
        eprintln!(
            "bowline work-view creation checkpoint cleanup failed at {}: {error}",
            checkpoint_path.display()
        );
    }
}

fn work_create_freshness_status(historical_base: bool) -> (WorkspaceStatus, Vec<RepairCommand>) {
    if !historical_base {
        return (WorkspaceStatus::healthy(), Vec::new());
    }
    (
        WorkspaceStatus {
            level: bowline_core::status::StatusLevel::Attention,
            attention_items: vec![
                "Work view is based on a historical snapshot; inspect freshness before handing it to an agent."
                    .to_string(),
            ],
        },
        vec![RepairCommand::inspect("Inspect freshness".to_string(), Some("bowline status --watch".to_string()))],
    )
}

fn resolve_base_snapshot_selector(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    selector: &str,
) -> Result<bowline_core::ids::SnapshotId, WorkViewError> {
    let snapshot_id = selector.strip_prefix("rp_").unwrap_or(selector);
    let snapshot_id = bowline_core::ids::SnapshotId::new(snapshot_id.to_string());
    if store.snapshot(workspace_id, &snapshot_id)?.is_some() {
        return Ok(snapshot_id);
    }
    if store
        .completed_sync_operation_for_snapshot(workspace_id, &snapshot_id)?
        .is_some()
    {
        return Ok(snapshot_id);
    }
    Err(WorkViewError::UnknownBaseSnapshot {
        selector: selector.to_string(),
    })
}

fn state_cache_root(db_path: Option<&Path>) -> Result<PathBuf, WorkViewError> {
    let db_path = match db_path {
        Some(path) => path.to_path_buf(),
        None => default_database_path().map_err(|_| WorkViewError::MissingMetadataDb)?,
    };
    let Some(state_root) = db_path.parent() else {
        return Err(WorkViewError::MissingMetadataDb);
    };
    Ok(state_root.join("cache"))
}

fn persist_new_work_view(
    store: &mut MetadataStore,
    work_view: &WorkView,
    descriptor: &WorkViewBaseDescriptor,
    snapshot: &crate::sync::SnapshotContent,
    captured_at: &str,
) -> Result<(), WorkViewError> {
    super::namespace::persist_exposed_snapshot(store, snapshot, &work_view.id, captured_at)?;
    store.insert_work_view_with_exposed_base(work_view, descriptor)?;
    Ok(())
}

pub fn list_work_views(options: WorkListOptions) -> Result<WorkListCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let work_views = store.work_views(
        &workspace.id,
        options.include_hidden,
        options.current_device_id.as_ref(),
    )?;
    let status = status_for_work_views(&work_views);
    Ok(WorkListCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Work,
        generated_at: options.generated_at,
        action: WorkCommandAction::Listed,
        workspace_id: workspace.id,
        work_views,
        include_hidden: options.include_hidden,
        status,
        next_actions: vec![RepairCommand::mutating(
            "Start a work view".to_string(),
            Some("bowline work create <name>".to_string()),
        )],
    })
}
