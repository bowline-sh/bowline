use std::{collections::BTreeSet, path::Path};

use bowline_core::{
    namespace_snapshot::{NamespaceMutation, NamespaceSnapshotBuilder},
    work_views::{WorkView, WorkViewLifecycle, WorkViewSyncState},
    workspace_graph::{NamespaceEntry, NamespaceEntryKind, normalize_workspace_path},
};

use crate::{
    metadata::{MetadataStore, WorkViewBaseDescriptor},
    sync::{SnapshotContent, namespace::PageNamespaceBuilder},
};
use bowline_storage::LocalContentCache;

use super::{WorkViewError, candidate::PolicyDriftRecord, paths::append_work_event};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkViewAcceptReview {
    MergeConflict { path_count: usize },
    PolicyDrift { records: Vec<PolicyDriftRecord> },
}

pub(crate) struct PartialExposedBaseAdvance<'a> {
    pub store: &'a MetadataStore,
    pub work_view: &'a WorkView,
    pub selected_paths: &'a BTreeSet<String>,
    pub target_snapshot: &'a SnapshotContent,
    pub cache_root: &'a Path,
    pub workspace_content_key: [u8; 32],
    pub captured_at: &'a str,
}

pub(crate) struct PreparedPartialExposedBase {
    updated: WorkView,
    descriptor: WorkViewBaseDescriptor,
    snapshot: SnapshotContent,
    changed: bool,
    captured_at: String,
}

#[cfg(test)]
pub(crate) fn advance_partial_exposed_base(
    input: PartialExposedBaseAdvance<'_>,
) -> Result<WorkView, WorkViewError> {
    let database_path = input.store.database_path()?;
    let prepared = prepare_partial_exposed_base(input)?;
    if !prepared.changed {
        return Ok(prepared.updated);
    }
    let mut store = MetadataStore::open(database_path)?;
    input_store_publish(&mut store, &prepared)?;
    Ok(prepared.updated)
}

pub(crate) fn prepare_partial_exposed_base(
    input: PartialExposedBaseAdvance<'_>,
) -> Result<PreparedPartialExposedBase, WorkViewError> {
    let mut descriptor = input
        .store
        .work_view_exposed_base(&input.work_view.workspace_id, &input.work_view.id)?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: input.work_view.base_snapshot_id.as_str().to_string(),
            reason: "authoritative exposed base is missing".to_string(),
        })?;
    let prefix = normalize_workspace_path(&descriptor.project_prefix)
        .trim_matches('/')
        .to_string();
    let previous_descriptor = descriptor.clone();
    let existing = super::namespace::load_exposed_snapshot(input.store, &descriptor)?;
    let mutation_limit = input.selected_paths.len().saturating_mul(4) as u64;
    let entry_limit = existing
        .manifest()
        .entry_count
        .saturating_mul(8)
        .saturating_add(32);
    let mut context = bowline_core::namespace_snapshot::NamespaceOperationContext::uncancelled(
        crate::sync::namespace::operation_budget(entry_limit, entry_limit, mutation_limit),
    );
    let mut builder =
        PageNamespaceBuilder::incremental(existing.namespace_snapshot(), &mut context)?;
    let cache = LocalContentCache::open(input.cache_root)?;
    let target_entries =
        selected_target_entries(input.target_snapshot, &prefix, input.selected_paths)?;
    let target_manifest = input.target_snapshot.manifest();
    for selected in input.selected_paths {
        let path = if prefix.is_empty() {
            selected.clone()
        } else {
            format!("{prefix}/{selected}")
        };
        builder.apply(
            NamespaceMutation::Remove(bowline_core::workspace_graph::WorkspaceRelativePath::new(
                path,
            )),
            &mut context,
        )?;
    }
    for entry in &target_entries {
        let Some(relative) = project_relative(&entry.path, &prefix) else {
            continue;
        };
        if !input.selected_paths.contains(relative)
            && !(entry.kind == NamespaceEntryKind::Directory
                && input
                    .selected_paths
                    .iter()
                    .any(|selected| path_contains(relative, selected)))
        {
            continue;
        }
        match entry.kind {
            NamespaceEntryKind::File => {
                retain_target_content(&cache, entry, relative, &input)?;
            }
            NamespaceEntryKind::Directory => {}
            _ => {
                return Err(WorkViewError::SnapshotMaterialization {
                    snapshot_id: target_manifest.snapshot_id.as_str().to_string(),
                    reason: format!("accepted entry kind is unsupported for `{relative}`"),
                });
            }
        }
        builder.apply(NamespaceMutation::Upsert(entry.clone()), &mut context)?;
    }
    let namespace = builder.finish(&mut context)?;
    let snapshot = SnapshotContent::from_built(namespace, std::collections::BTreeMap::new());
    let entries = super::namespace::collect_descriptor_entries(&snapshot)?;
    descriptor.policy_fingerprint = super::exposure::entry_policy_fingerprint(entries.iter());
    let manifest = snapshot.manifest();
    descriptor.exposed_snapshot_id = manifest.snapshot_id.clone();
    descriptor.exposed_namespace_root_id = manifest.namespace_root_id.clone();
    descriptor.exposed_semantic_manifest_digest = manifest.semantic_manifest_digest.clone();
    descriptor.exposed_entry_count = manifest.entry_count;
    let mut updated = input.work_view.clone();
    updated.lifecycle = WorkViewLifecycle::Active;
    updated.sync_state = WorkViewSyncState::LocalOnly;
    updated.updated_at = input.captured_at.to_string();
    let changed = !(descriptor == previous_descriptor && updated == *input.work_view);
    Ok(PreparedPartialExposedBase {
        updated,
        descriptor,
        snapshot,
        changed,
        captured_at: input.captured_at.to_string(),
    })
}

fn selected_target_entries(
    snapshot: &SnapshotContent,
    prefix: &str,
    selected_paths: &BTreeSet<String>,
) -> Result<Vec<NamespaceEntry>, WorkViewError> {
    let mut relative_paths = selected_paths.clone();
    for selected in selected_paths {
        let mut ancestor = std::path::PathBuf::from(selected);
        while ancestor.pop() {
            let normalized = normalize_workspace_path(&ancestor.display().to_string());
            if normalized.is_empty() {
                break;
            }
            relative_paths.insert(normalized);
        }
    }
    let mut entries = Vec::new();
    for relative in relative_paths {
        let path = if prefix.is_empty() {
            relative
        } else {
            format!("{prefix}/{relative}")
        };
        if let Some(entry) = super::namespace::get_entry(snapshot, &path)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

#[cfg(test)]
fn input_store_publish(
    store: &mut MetadataStore,
    prepared: &PreparedPartialExposedBase,
) -> Result<(), WorkViewError> {
    super::namespace::persist_exposed_snapshot(
        store,
        &prepared.snapshot,
        &prepared.updated.id,
        &prepared.captured_at,
    )?;
    store.insert_work_view_with_exposed_base(&prepared.updated, &prepared.descriptor)?;
    append_work_event(
        store,
        bowline_core::events::EventName::WorkAccepted,
        &prepared.updated,
        &prepared.captured_at,
    );
    Ok(())
}

pub(crate) fn publish_partial_exposed_base_under_claim(
    store: &mut MetadataStore,
    prepared: PreparedPartialExposedBase,
    claim: &crate::metadata::WorkViewAcceptClaimHandle,
    now: &str,
) -> Result<Option<WorkView>, WorkViewError> {
    super::namespace::persist_exposed_snapshot(
        store,
        &prepared.snapshot,
        &prepared.updated.id,
        &prepared.captured_at,
    )?;
    if store.insert_work_view_with_exposed_base_under_accept_claim(
        &prepared.updated,
        &prepared.descriptor,
        claim,
        now,
    )? != crate::metadata::WorkViewAcceptClaimTransition::Applied
    {
        return Ok(None);
    }
    if prepared.changed {
        append_work_event(
            store,
            bowline_core::events::EventName::WorkAccepted,
            &prepared.updated,
            &prepared.captured_at,
        );
    }
    Ok(Some(prepared.updated))
}

fn retain_target_content(
    cache: &LocalContentCache,
    entry: &bowline_core::workspace_graph::NamespaceEntry,
    relative: &str,
    input: &PartialExposedBaseAdvance<'_>,
) -> Result<(), WorkViewError> {
    let content_id =
        entry
            .content_id
            .as_ref()
            .ok_or_else(|| WorkViewError::SnapshotMaterialization {
                snapshot_id: input
                    .target_snapshot
                    .manifest()
                    .snapshot_id
                    .as_str()
                    .to_string(),
                reason: format!("accepted content id is unavailable for `{relative}`"),
            })?;
    let bytes = input
        .target_snapshot
        .read_file_for_path(&entry.path)?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: input
                .target_snapshot
                .manifest()
                .snapshot_id
                .as_str()
                .to_string(),
            reason: format!("accepted content is unavailable for `{relative}`"),
        })?;
    if bowline_core::workspace_graph::workspace_content_id(input.workspace_content_key, &bytes)
        != *content_id
    {
        return Err(WorkViewError::SnapshotMaterialization {
            snapshot_id: input
                .target_snapshot
                .manifest()
                .snapshot_id
                .as_str()
                .to_string(),
            reason: format!("accepted content identity mismatched for `{relative}`"),
        });
    }
    cache.put_content(content_id, &bytes)?;
    cache.get_content(content_id, input.workspace_content_key)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn finalize_review_ready(
    store: &MetadataStore,
    work_view: &WorkView,
    review: &WorkViewAcceptReview,
    generated_at: &str,
) -> Result<WorkView, WorkViewError> {
    let updated = review_ready_work_view(work_view, review, generated_at);
    if updated != *work_view {
        store.upsert_work_view(&updated)?;
        append_work_event(
            store,
            bowline_core::events::EventName::WorkReviewReady,
            &updated,
            generated_at,
        );
    }
    Ok(updated)
}

pub(crate) fn finalize_review_ready_under_claim(
    store: &MetadataStore,
    work_view: &WorkView,
    review: &WorkViewAcceptReview,
    claim: &crate::metadata::WorkViewAcceptClaimHandle,
    generated_at: &str,
) -> Result<Option<WorkView>, WorkViewError> {
    let updated = review_ready_work_view(work_view, review, generated_at);
    if store.upsert_work_view_under_accept_claim(&updated, claim, generated_at)?
        != crate::metadata::WorkViewAcceptClaimTransition::Applied
    {
        return Ok(None);
    }
    if updated != *work_view {
        append_work_event(
            store,
            bowline_core::events::EventName::WorkReviewReady,
            &updated,
            generated_at,
        );
    }
    Ok(Some(updated))
}

fn review_ready_work_view(
    work_view: &WorkView,
    review: &WorkViewAcceptReview,
    generated_at: &str,
) -> WorkView {
    let mut updated = work_view.clone();
    updated.lifecycle = WorkViewLifecycle::ReviewReady;
    updated.sync_state = WorkViewSyncState::Attention;
    updated.attention = review_attention(review);
    updated.updated_at = generated_at.to_string();
    updated
}

fn review_attention(review: &WorkViewAcceptReview) -> Vec<String> {
    match review {
        WorkViewAcceptReview::MergeConflict { path_count } => vec![format!(
            "Work-view accept has {path_count} merge conflict path(s)."
        )],
        WorkViewAcceptReview::PolicyDrift { records } => {
            let mut codes = records
                .iter()
                .map(|record| record.reason.code())
                .collect::<BTreeSet<_>>();
            let shown = codes.iter().take(4).copied().collect::<Vec<_>>();
            let omitted = codes.len().saturating_sub(shown.len());
            let mut message = format!(
                "Work-view accept requires policy review: {}.",
                shown.join(", ")
            );
            if omitted > 0 {
                message.push_str(&format!(" {omitted} additional reason code(s) omitted."));
            }
            codes.clear();
            vec![message]
        }
    }
}

fn project_relative<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        Some(path.trim_matches('/'))
    } else {
        path.strip_prefix(prefix)?.strip_prefix('/')
    }
}

fn path_contains(parent: &str, child: &str) -> bool {
    child == parent
        || child
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
pub(crate) fn advance_partial_exposed_base_from_live_tree(
    store: &MetadataStore,
    work_view: &WorkView,
    selected_paths: &BTreeSet<String>,
    captured_at: &str,
) -> Result<WorkView, WorkViewError> {
    use std::collections::BTreeMap;

    use bowline_core::workspace_graph::{
        RefKind, SnapshotDraft, SnapshotKind, WorkspaceRef as SnapshotRef, workspace_content_id,
    };

    let work_root = super::paths::expand_display_path(&work_view.visible_path);
    let plan = super::plan_live_tree_exposure(&work_root, "")?;
    let key = *blake3::hash(work_view.id.as_str().as_bytes()).as_bytes();
    let mut files = BTreeMap::new();
    let mut entries = Vec::new();
    for planned in plan.entries {
        if !selected_paths.contains(&planned.relative_path)
            && !(planned.entry.kind == NamespaceEntryKind::Directory
                && selected_paths
                    .iter()
                    .any(|selected| path_contains(&planned.relative_path, selected)))
        {
            continue;
        }
        let mut entry = planned.entry;
        entry.path = if work_view.project_path.is_empty() {
            planned.relative_path.clone()
        } else {
            format!("{}/{}", work_view.project_path, planned.relative_path)
        };
        if entry.kind == NamespaceEntryKind::File {
            let bytes = std::fs::read(planned.source_path)?;
            let content_id = workspace_content_id(key, &bytes);
            entry.content_id = Some(content_id.clone());
            entry.content_layout = None;
            entry.byte_len = Some(bytes.len() as u64);
            files.insert(content_id, bytes);
        }
        entries.push(entry);
    }
    let identity =
        crate::sync::rebuild_manifest_identity(&work_view.workspace_id, &entries, captured_at);
    let snapshot_id = identity.snapshot_id;
    let target = crate::sync::SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            workspace_id: work_view.workspace_id.clone(),
            project_id: Some(work_view.project_id.clone()),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: Some(work_view.base_snapshot_id.clone()),
            entries,
            refs: vec![SnapshotRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id,
                kind: RefKind::Workspace,
            }],
        },
        files,
        key,
    )?;
    advance_partial_exposed_base(PartialExposedBaseAdvance {
        store,
        work_view,
        selected_paths,
        target_snapshot: &target,
        cache_root: &store.content_cache_root()?,
        workspace_content_key: key,
        captured_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_containment_observes_component_boundaries() {
        assert!(path_contains("src", "src/lib.rs"));
        assert!(!path_contains("src", "src-old/lib.rs"));
    }
}
