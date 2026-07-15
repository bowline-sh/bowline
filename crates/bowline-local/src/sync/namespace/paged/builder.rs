use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use bowline_core::{
    ids::{NamespacePageId, ProjectId, SnapshotId},
    namespace_snapshot::{
        NamespaceBuildError, NamespaceMutation, NamespaceOperationContext, NamespaceReadError,
        NamespaceSnapshotBuilder, SnapshotMetadata,
    },
    workspace_graph::{
        NamespaceEntry, SnapshotDraft, SnapshotKind, WorkspaceRef, WorkspaceRelativePath,
    },
};

use super::{
    codec::NamespaceEntryValue,
    layout::{PackLengthResolver, store_content_layout},
    reader::PageNamespaceReader,
    snapshot::{BuiltPagedNamespaceSnapshot, ChangedPageSummary},
    tree::{TreeMutation, build_tree, mutate_tree, paths_for_prefix},
    types::{MetadataIdentityKey, MetadataRecordKind, PageStore},
};
use crate::sync::namespace::semantic_manifest_identity_from_reader;
use crate::sync::namespace::{semantic_manifest_identity_with_context, validated_path};

pub struct PageNamespaceBuilder {
    metadata: SnapshotMetadata,
    state: BuilderState,
    mutations_applied: u64,
    pack_lengths: Option<Arc<dyn PackLengthResolver>>,
    pages_loaded_at_start: u64,
    identity_key: MetadataIdentityKey,
}

enum BuilderState {
    Collecting(BTreeMap<WorkspaceRelativePath, NamespaceEntry>),
    Persistent {
        base_root: NamespacePageId,
        root: NamespacePageId,
        store: PageStore,
        prior_namespace_ids: Arc<BTreeSet<NamespacePageId>>,
        prior_content_layouts: u64,
        prior_segment_pages: u64,
    },
}

impl PageNamespaceBuilder {
    pub fn new(metadata: SnapshotMetadata, identity_key: MetadataIdentityKey) -> Self {
        Self {
            metadata,
            state: BuilderState::Collecting(BTreeMap::new()),
            mutations_applied: 0,
            pack_lengths: None,
            pages_loaded_at_start: 0,
            identity_key,
        }
    }

    pub fn with_pack_length_resolver(mut self, resolver: Arc<dyn PackLengthResolver>) -> Self {
        self.pack_lengths = Some(resolver);
        self
    }

    pub fn full(
        metadata: SnapshotMetadata,
        identity_key: MetadataIdentityKey,
        entries: Vec<NamespaceEntry>,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<BuiltPagedNamespaceSnapshot, NamespaceBuildError> {
        let mut builder = Self::new(metadata, identity_key);
        let BuilderState::Collecting(ordered) = &mut builder.state else {
            unreachable!("new page builder starts in collecting mode");
        };
        for entry in entries {
            context.ensure_active()?;
            let path = validated_path(&entry.path)?;
            if ordered.insert(path, entry).is_some() {
                return Err(NamespaceReadError::DuplicatePath { field: "path" }.into());
            }
        }
        builder.finish(context)
    }

    pub fn from_draft(
        mut draft: SnapshotDraft,
        identity_key: MetadataIdentityKey,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<BuiltPagedNamespaceSnapshot, NamespaceBuildError> {
        draft
            .entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        if draft
            .entries
            .windows(2)
            .any(|entries| entries[0].path == entries[1].path)
        {
            return Err(NamespaceReadError::DuplicatePath { field: "path" }.into());
        }
        let identity =
            semantic_manifest_identity_with_context(&draft.workspace_id, &draft.entries, context)?;
        if draft.snapshot_id != *identity.snapshot_id() {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "snapshot draft ID does not match its semantic entries",
            }
            .into());
        }
        let metadata = SnapshotMetadata {
            schema_version: draft.schema_version,
            snapshot_id: draft.snapshot_id,
            workspace_id: draft.workspace_id,
            project_id: draft.project_id,
            kind: draft.kind,
            base_snapshot_id: draft.base_snapshot_id,
            semantic_manifest_digest: identity.digest().clone(),
            entry_count: draft.entries.len() as u64,
            refs: draft.refs,
        };
        Self::full(metadata, identity_key, draft.entries, context)
    }

    pub fn incremental(
        snapshot: &BuiltPagedNamespaceSnapshot,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Self, NamespaceBuildError> {
        let (prior_namespace_ids, prior_content_layouts, prior_segment_pages) =
            if snapshot.reachable_namespace_ids.is_empty() && snapshot.store.has_source() {
                let records = snapshot.store.reachable_record_summaries_with_context(
                    &snapshot.namespace_root_id,
                    context,
                )?;
                let namespace_ids = records
                    .iter()
                    .filter(|record| record.kind == MetadataRecordKind::NamespacePage)
                    .map(|record| NamespacePageId::new(&record.logical_id))
                    .collect::<BTreeSet<_>>();
                let content_layouts = records
                    .iter()
                    .filter(|record| record.kind == MetadataRecordKind::ContentLayout)
                    .count() as u64;
                let segment_pages = records
                    .iter()
                    .filter(|record| record.kind == MetadataRecordKind::SegmentPage)
                    .count() as u64;
                (Arc::new(namespace_ids), content_layouts, segment_pages)
            } else {
                (
                    Arc::clone(&snapshot.reachable_namespace_ids),
                    snapshot.reachable_content_layouts,
                    snapshot.reachable_segment_pages,
                )
            };
        Ok(Self {
            metadata: snapshot.metadata.clone(),
            state: BuilderState::Persistent {
                base_root: snapshot.namespace_root_id.clone(),
                root: snapshot.namespace_root_id.clone(),
                store: PageStore::overlay(snapshot.store.clone()),
                prior_namespace_ids,
                prior_content_layouts,
                prior_segment_pages,
            },
            mutations_applied: 0,
            pack_lengths: None,
            pages_loaded_at_start: context.counters().namespace_pages_loaded,
            identity_key: snapshot.store.identity_key(),
        })
    }

    pub fn mutation_count(&self) -> u64 {
        self.mutations_applied
    }

    pub(crate) fn retarget_snapshot(
        &mut self,
        project_id: Option<ProjectId>,
        kind: SnapshotKind,
        base_snapshot_id: Option<SnapshotId>,
        refs: Vec<WorkspaceRef>,
    ) {
        self.metadata.project_id = project_id;
        self.metadata.kind = kind;
        self.metadata.base_snapshot_id = base_snapshot_id;
        self.metadata.refs = refs;
    }

    fn finish_collecting(
        mut self,
        entries: BTreeMap<WorkspaceRelativePath, NamespaceEntry>,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<BuiltPagedNamespaceSnapshot, NamespaceBuildError> {
        let ordered_entries = entries.into_values().collect::<Vec<_>>();
        let identity = semantic_manifest_identity_with_context(
            &self.metadata.workspace_id,
            &ordered_entries,
            context,
        )?;
        let mut store = PageStore::with_identity_key(self.identity_key);
        let mut keyed_values = Vec::with_capacity(ordered_entries.len());
        for entry in &ordered_entries {
            context.ensure_active()?;
            validate_layout_identity(entry)?;
            let layout_id = entry
                .content_layout
                .as_ref()
                .map(|layout| {
                    store_content_layout(
                        self.metadata.workspace_id.as_str(),
                        layout,
                        &mut store,
                        self.pack_lengths.as_deref(),
                        context,
                    )
                })
                .transpose()?;
            keyed_values.push((
                entry.path.as_bytes().to_vec(),
                NamespaceEntryValue::from_entry(entry, layout_id),
            ));
        }
        let root = build_tree(
            self.metadata.workspace_id.as_str(),
            keyed_values,
            &mut store,
            context,
        )?;
        self.metadata.snapshot_id = identity.snapshot_id().clone();
        self.metadata.semantic_manifest_digest = identity.digest().clone();
        self.metadata.entry_count = ordered_entries.len() as u64;
        let mut changed = full_summary(&store);
        changed.mutations_applied = self.mutations_applied;
        changed.semantic_entries_hashed = identity.entries_hashed();
        let reachable_namespace_ids = Arc::new(store.namespace_page_ids());
        let reachable_content_layouts = store.content_layout_count();
        let reachable_segment_pages = store.segment_page_count();
        Ok(BuiltPagedNamespaceSnapshot {
            metadata: self.metadata,
            namespace_root_id: root,
            semantic_manifest_digest: identity.digest().clone(),
            snapshot_id: identity.snapshot_id().clone(),
            store,
            changed,
            reachable_namespace_ids,
            reachable_content_layouts,
            reachable_segment_pages,
        })
    }

    fn finish_persistent(
        mut self,
        state: PersistentState,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<BuiltPagedNamespaceSnapshot, NamespaceBuildError> {
        let PersistentState {
            base_root,
            root,
            store,
            prior_namespace_ids,
            prior_content_layouts,
            prior_segment_pages,
        } = state;
        if root == base_root {
            let mut changed = unchanged_summary(
                &store,
                prior_namespace_ids.len() as u64,
                prior_content_layouts,
                prior_segment_pages,
            );
            changed.mutations_applied = self.mutations_applied;
            changed.namespace_pages_loaded_during_build = context
                .counters()
                .namespace_pages_loaded
                .saturating_sub(self.pages_loaded_at_start);
            return Ok(BuiltPagedNamespaceSnapshot {
                metadata: self.metadata.clone(),
                namespace_root_id: root,
                semantic_manifest_digest: self.metadata.semantic_manifest_digest.clone(),
                snapshot_id: self.metadata.snapshot_id.clone(),
                store,
                changed,
                reachable_namespace_ids: prior_namespace_ids,
                reachable_content_layouts: prior_content_layouts,
                reachable_segment_pages: prior_segment_pages,
            });
        }

        let provisional = BuiltPagedNamespaceSnapshot {
            metadata: self.metadata.clone(),
            namespace_root_id: root.clone(),
            semantic_manifest_digest: self.metadata.semantic_manifest_digest.clone(),
            snapshot_id: self.metadata.snapshot_id.clone(),
            store: store.clone(),
            changed: ChangedPageSummary::default(),
            reachable_namespace_ids: Arc::clone(&prior_namespace_ids),
            reachable_content_layouts: prior_content_layouts,
            reachable_segment_pages: prior_segment_pages,
        };
        let reader = PageNamespaceReader::new(&provisional);
        let identity = semantic_manifest_identity_from_reader(&reader, context)?;
        self.metadata.snapshot_id = identity.snapshot_id().clone();
        self.metadata.semantic_manifest_digest = identity.digest().clone();
        self.metadata.entry_count = identity.entries_hashed();
        let (mut changed, reachable_namespace_ids) =
            incremental_summary(&store, &root, &prior_namespace_ids, context)?;
        changed.mutations_applied = self.mutations_applied;
        changed.semantic_entries_hashed = identity.entries_hashed();
        changed.namespace_pages_loaded_during_build = context
            .counters()
            .namespace_pages_loaded
            .saturating_sub(self.pages_loaded_at_start);
        let reachable_content_layouts = changed
            .content_layouts_created
            .saturating_add(changed.content_layouts_reused);
        let reachable_segment_pages = changed
            .segment_pages_created
            .saturating_add(changed.segment_pages_reused);
        Ok(BuiltPagedNamespaceSnapshot {
            metadata: self.metadata,
            namespace_root_id: root,
            semantic_manifest_digest: identity.digest().clone(),
            snapshot_id: identity.snapshot_id().clone(),
            store,
            changed,
            reachable_namespace_ids: Arc::new(reachable_namespace_ids),
            reachable_content_layouts,
            reachable_segment_pages,
        })
    }
}

struct PersistentState {
    base_root: NamespacePageId,
    root: NamespacePageId,
    store: PageStore,
    prior_namespace_ids: Arc<BTreeSet<NamespacePageId>>,
    prior_content_layouts: u64,
    prior_segment_pages: u64,
}

impl NamespaceSnapshotBuilder for PageNamespaceBuilder {
    type Output = BuiltPagedNamespaceSnapshot;

    fn apply(
        &mut self,
        mutation: NamespaceMutation,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceBuildError> {
        context.charge_mutation()?;
        match &mut self.state {
            BuilderState::Collecting(entries) => apply_collecting(entries, mutation, context)?,
            BuilderState::Persistent { root, store, .. } => apply_persistent(
                &self.metadata,
                self.pack_lengths.as_deref(),
                root,
                store,
                mutation,
                context,
            )?,
        }
        self.mutations_applied = self.mutations_applied.saturating_add(1);
        Ok(())
    }

    fn finish(
        mut self,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Self::Output, NamespaceBuildError> {
        let state = std::mem::replace(&mut self.state, BuilderState::Collecting(BTreeMap::new()));
        match state {
            BuilderState::Collecting(entries) => self.finish_collecting(entries, context),
            BuilderState::Persistent {
                base_root,
                root,
                store,
                prior_namespace_ids,
                prior_content_layouts,
                prior_segment_pages,
            } => self.finish_persistent(
                PersistentState {
                    base_root,
                    root,
                    store,
                    prior_namespace_ids,
                    prior_content_layouts,
                    prior_segment_pages,
                },
                context,
            ),
        }
    }
}

fn apply_collecting(
    entries: &mut BTreeMap<WorkspaceRelativePath, NamespaceEntry>,
    mutation: NamespaceMutation,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<(), NamespaceBuildError> {
    match mutation {
        NamespaceMutation::Upsert(entry) => {
            entries.insert(validated_path(&entry.path)?, entry);
        }
        NamespaceMutation::Remove(path) => {
            validated_path(path.as_str())?;
            entries.remove(&path);
        }
        NamespaceMutation::RemovePrefix(prefix) => {
            if !prefix.is_empty() {
                validated_path(prefix.as_str())?;
            }
            let mut removed = Vec::new();
            for path in entries.keys() {
                context.charge_entries(1)?;
                if path.is_equal_to_or_below(&prefix) {
                    removed.push(path.clone());
                }
            }
            for path in removed {
                entries.remove(&path);
            }
        }
    }
    Ok(())
}

fn apply_persistent(
    metadata: &SnapshotMetadata,
    pack_lengths: Option<&dyn PackLengthResolver>,
    root: &mut NamespacePageId,
    store: &mut PageStore,
    mutation: NamespaceMutation,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<(), NamespaceBuildError> {
    match mutation {
        NamespaceMutation::Upsert(entry) => {
            let path = validated_path(&entry.path)?;
            validate_layout_identity(&entry)?;
            let layout_id = entry
                .content_layout
                .as_ref()
                .map(|layout| {
                    store_content_layout(
                        metadata.workspace_id.as_str(),
                        layout,
                        store,
                        pack_lengths,
                        context,
                    )
                })
                .transpose()?;
            *root = mutate_tree(
                metadata.workspace_id.as_str(),
                root,
                path.as_str().as_bytes(),
                TreeMutation::Upsert(NamespaceEntryValue::from_entry(&entry, layout_id)),
                store,
                context,
            )?;
        }
        NamespaceMutation::Remove(path) => {
            validated_path(path.as_str())?;
            *root = mutate_tree(
                metadata.workspace_id.as_str(),
                root,
                path.as_str().as_bytes(),
                TreeMutation::Remove,
                store,
                context,
            )?;
        }
        NamespaceMutation::RemovePrefix(prefix) => {
            if !prefix.is_empty() {
                validated_path(prefix.as_str())?;
            }
            let paths = paths_for_prefix(
                metadata.workspace_id.as_str(),
                root,
                store,
                prefix.as_str().as_bytes(),
                context,
            )?;
            for path in paths {
                *root = mutate_tree(
                    metadata.workspace_id.as_str(),
                    root,
                    &path,
                    TreeMutation::Remove,
                    store,
                    context,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_layout_identity(entry: &NamespaceEntry) -> Result<(), NamespaceBuildError> {
    if let Some(layout) = &entry.content_layout
        && (entry.content_id.as_ref() != Some(layout.logical_content_id())
            || entry.byte_len != Some(layout.logical_length()))
    {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "namespace entry content fields do not match its layout",
        }
        .into());
    }
    Ok(())
}

fn full_summary(store: &PageStore) -> ChangedPageSummary {
    ChangedPageSummary {
        namespace_pages_created: store.namespace_page_count(),
        content_layouts_created: store.content_layout_count(),
        segment_pages_created: store.segment_page_count(),
        metadata_bytes_created: store.total_encoded_bytes(),
        namespace_pages_encoded: store.local_namespace_page_count(),
        content_layouts_encoded: store.local_content_layout_count(),
        segment_pages_encoded: store.local_segment_page_count(),
        ..ChangedPageSummary::default()
    }
}

fn incremental_summary(
    store: &PageStore,
    root: &NamespacePageId,
    prior_namespace_ids: &BTreeSet<NamespacePageId>,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<(ChangedPageSummary, BTreeSet<NamespacePageId>), NamespaceReadError> {
    let records = store.reachable_record_summaries_with_context(root, context)?;
    let mut summary = ChangedPageSummary::default();
    let mut current_namespace_ids = BTreeSet::new();
    for record in records {
        let local = store.is_local_record(&record);
        match (record.kind, local) {
            (MetadataRecordKind::NamespacePage, true) => summary.namespace_pages_created += 1,
            (MetadataRecordKind::NamespacePage, false) => summary.namespace_pages_reused += 1,
            (MetadataRecordKind::ContentLayout, true) => summary.content_layouts_created += 1,
            (MetadataRecordKind::ContentLayout, false) => summary.content_layouts_reused += 1,
            (MetadataRecordKind::SegmentPage, true) => summary.segment_pages_created += 1,
            (MetadataRecordKind::SegmentPage, false) => summary.segment_pages_reused += 1,
        }
        if local {
            summary.metadata_bytes_created = summary
                .metadata_bytes_created
                .saturating_add(record.encoded_bytes);
        }
        if record.kind == MetadataRecordKind::NamespacePage {
            current_namespace_ids.insert(NamespacePageId::new(&record.logical_id));
        }
    }
    summary.namespace_pages_removed = prior_namespace_ids
        .difference(&current_namespace_ids)
        .count() as u64;
    summary.namespace_pages_encoded = store.local_namespace_page_count();
    summary.content_layouts_encoded = store.local_content_layout_count();
    summary.segment_pages_encoded = store.local_segment_page_count();
    Ok((summary, current_namespace_ids))
}

fn unchanged_summary(
    store: &PageStore,
    reachable_namespace_pages: u64,
    reachable_content_layouts: u64,
    reachable_segment_pages: u64,
) -> ChangedPageSummary {
    ChangedPageSummary {
        namespace_pages_reused: reachable_namespace_pages,
        content_layouts_reused: reachable_content_layouts,
        segment_pages_reused: reachable_segment_pages,
        namespace_pages_encoded: store.local_namespace_page_count(),
        content_layouts_encoded: store.local_content_layout_count(),
        segment_pages_encoded: store.local_segment_page_count(),
        ..ChangedPageSummary::default()
    }
}
