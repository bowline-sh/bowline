use std::{collections::BTreeSet, fs};

use bowline_core::{
    fs_atomic::{AtomicWriteOptions, write_atomic},
    ids::WorkViewId,
    namespace_snapshot::{
        EntryVisitor, NamespaceMutation, NamespaceOperationBudget, NamespaceOperationContext,
        NamespaceReadError, NamespaceSnapshotBuilder, NamespaceVisitControl,
    },
    workspace_graph::{NamespaceEntry, WorkspaceRelativePath},
};

use crate::{
    metadata::{
        MetadataCacheRecord, MetadataCacheState, MetadataLogicalId, MetadataRecordKind,
        MetadataRecordRef, MetadataStore, SnapshotPinId, SnapshotPinOwner, SnapshotPinOwnerKind,
        SnapshotPinReason, SnapshotPinRecord, SnapshotRecord,
    },
    sync::{SnapshotContent, namespace, namespace::NAMESPACE_PAGE_MAX_BYTES},
};

use super::WorkViewError;

const MAX_PAGE_VISITS_PER_ENTRY: u64 = 4;
const MAX_RADIX_DEPTH: u64 = 4_096;
const MAX_LAYOUT_VISITS_PER_ENTRY: u64 = 1;
const MAX_SEGMENT_PAGE_VISITS_PER_ENTRY: u64 = 1_000_000;

pub(super) fn build_exposed_snapshot(
    base: &SnapshotContent,
    entries: Vec<NamespaceEntry>,
) -> Result<SnapshotContent, WorkViewError> {
    let mutation_limit = base
        .manifest()
        .entry_count
        .saturating_add(entries.len() as u64);
    let entry_limit = base
        .manifest()
        .entry_count
        .saturating_mul(8)
        .saturating_add(entries.len() as u64)
        .saturating_add(32);
    let mut context = NamespaceOperationContext::uncancelled(
        crate::sync::namespace::operation_budget(entry_limit, entry_limit, mutation_limit),
    );
    let retained = entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let mut builder = crate::sync::namespace::PageNamespaceBuilder::incremental(
        base.namespace_snapshot(),
        &mut context,
    )?;
    let base_entries = collect_descriptor_entries(base)?;
    for entry in base_entries {
        if !retained.contains(&entry.path) {
            builder.apply(
                NamespaceMutation::Remove(WorkspaceRelativePath::new(entry.path)),
                &mut context,
            )?;
        }
    }
    for entry in entries {
        builder.apply(NamespaceMutation::Upsert(entry), &mut context)?;
    }
    let namespace = builder.finish(&mut context)?;
    Ok(SnapshotContent::from_built(
        namespace,
        std::collections::BTreeMap::new(),
    ))
}

pub(super) fn persist_exposed_snapshot(
    store: &mut MetadataStore,
    snapshot: &SnapshotContent,
    work_view_id: &WorkViewId,
    created_at: &str,
) -> Result<(), WorkViewError> {
    store.register_metadata_identity_key(
        &snapshot.manifest().workspace_id,
        snapshot.namespace_store().identity_key().as_bytes(),
        created_at,
    )?;
    let database_path = store.database_path()?;
    let cache_root = database_path
        .parent()
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: snapshot.manifest().snapshot_id.as_str().to_string(),
            reason: "metadata database has no parent directory".to_string(),
        })?
        .join("metadata-pages");
    fs::create_dir_all(&cache_root)?;
    let mut records = Vec::new();
    let mut context = read_context(snapshot.manifest().entry_count);
    snapshot
        .namespace_store()
        .visit_new_reachable_plaintext_records(
            &snapshot.namespace_snapshot().namespace_root_id,
            &mut context,
            |record| {
                records.push(record);
                Ok::<(), NamespaceReadError>(())
            },
        )?;
    let workspace_id = &snapshot.manifest().workspace_id;
    for record in &records {
        let kind = local_metadata_kind(record.summary.kind);
        let logical_id = MetadataLogicalId::new(&record.summary.logical_id);
        let cache_path = cache_root.join(format!("{}.page", logical_id.as_str()));
        write_atomic(
            &cache_path,
            &record.plaintext,
            AtomicWriteOptions {
                unix_mode: Some(0o600),
                reject_symlink: true,
                replace_existing: true,
            },
        )?;
        store.put_metadata_cache_record(&MetadataCacheRecord {
            workspace_id: workspace_id.clone(),
            logical_id,
            kind,
            cache_path: Some(cache_path.display().to_string()),
            encoded_bytes: record.plaintext.len() as u64,
            state: MetadataCacheState::Present,
            last_accessed_at: created_at.to_string(),
        })?;
    }
    for record in &records {
        let parent = MetadataRecordRef {
            kind: local_metadata_kind(record.summary.kind),
            logical_id: MetadataLogicalId::new(&record.summary.logical_id),
        };
        let children = record
            .summary
            .child_logical_ids
            .iter()
            .map(|child| MetadataRecordRef {
                kind: metadata_kind_for_logical_id(child),
                logical_id: MetadataLogicalId::new(child),
            })
            .collect::<Vec<_>>();
        store.replace_metadata_record_edges(workspace_id, &parent, &children)?;
    }
    let manifest = snapshot.manifest();
    let record = SnapshotRecord {
        id: manifest.snapshot_id.clone(),
        workspace_id: manifest.workspace_id.clone(),
        project_id: manifest.project_id.clone(),
        kind: manifest.kind,
        base_snapshot_id: manifest.base_snapshot_id.clone(),
        root_id: manifest.namespace_root_id.clone(),
        semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
        entry_count: manifest.entry_count,
        refs: manifest.refs.clone(),
        created_at: created_at.to_string(),
    };
    store.commit_snapshot_root(&record, &[], created_at)?;
    store.acquire_snapshot_pin(&SnapshotPinRecord {
        id: SnapshotPinId::new(format!("work-view-exposed:{}", work_view_id.as_str())),
        workspace_id: workspace_id.clone(),
        snapshot_id: manifest.snapshot_id.clone(),
        root_id: manifest.namespace_root_id.clone(),
        reason: SnapshotPinReason::WorkView,
        owner: SnapshotPinOwner {
            kind: SnapshotPinOwnerKind::WorkView,
            id: work_view_id.as_str().to_string(),
        },
        expires_at: None,
        created_at: created_at.to_string(),
    })?;
    Ok(())
}

pub(super) fn load_exposed_snapshot(
    store: &MetadataStore,
    descriptor: &crate::metadata::WorkViewBaseDescriptor,
) -> Result<SnapshotContent, WorkViewError> {
    let snapshot = store
        .snapshot(&descriptor.workspace_id, &descriptor.exposed_snapshot_id)?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: descriptor.exposed_snapshot_id.as_str().to_string(),
            reason: "work-view exposed snapshot root is missing".to_string(),
        })?;
    if snapshot.root_id != descriptor.exposed_namespace_root_id
        || snapshot.semantic_manifest_digest != descriptor.exposed_semantic_manifest_digest
        || snapshot.entry_count != descriptor.exposed_entry_count
    {
        return Err(WorkViewError::SnapshotMaterialization {
            snapshot_id: descriptor.exposed_snapshot_id.as_str().to_string(),
            reason: "work-view exposed descriptor does not match its immutable root".to_string(),
        });
    }
    crate::sync::load_cached_snapshot(store, &snapshot).map_err(Into::into)
}

fn local_metadata_kind(kind: namespace::MetadataRecordKind) -> MetadataRecordKind {
    match kind {
        namespace::MetadataRecordKind::NamespacePage => MetadataRecordKind::NamespacePage,
        namespace::MetadataRecordKind::ContentLayout => MetadataRecordKind::ContentLayout,
        namespace::MetadataRecordKind::SegmentPage => MetadataRecordKind::SegmentPage,
    }
}

fn metadata_kind_for_logical_id(logical_id: &str) -> MetadataRecordKind {
    if logical_id.starts_with("nsp_") {
        MetadataRecordKind::NamespacePage
    } else if logical_id.starts_with("ctl_") {
        MetadataRecordKind::ContentLayout
    } else {
        MetadataRecordKind::SegmentPage
    }
}

pub(super) fn collect_descriptor_entries(
    snapshot: &SnapshotContent,
) -> Result<Vec<NamespaceEntry>, WorkViewError> {
    let mut entries = Vec::new();
    let mut context = read_context(snapshot.manifest().entry_count);
    snapshot.namespace_reader().visit_prefix_descriptors(
        &WorkspaceRelativePath::new(""),
        &mut context,
        &mut |descriptor| {
            entries.push(descriptor.entry_without_layout);
            Ok(NamespaceVisitControl::Continue)
        },
    )?;
    Ok(entries)
}

pub(super) fn collect_prefix(
    snapshot: &SnapshotContent,
    prefix: &WorkspaceRelativePath,
) -> Result<Vec<NamespaceEntry>, WorkViewError> {
    struct Collector(Vec<NamespaceEntry>);

    impl EntryVisitor for Collector {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            self.0.push(entry.clone());
            Ok(NamespaceVisitControl::Continue)
        }
    }

    let mut collector = Collector(Vec::new());
    let mut context = read_context(snapshot.manifest().entry_count);
    snapshot.visit_prefix(prefix, &mut context, &mut collector)?;
    Ok(collector.0)
}

pub(super) fn get_entry(
    snapshot: &SnapshotContent,
    path: &str,
) -> Result<Option<NamespaceEntry>, WorkViewError> {
    use bowline_core::namespace_snapshot::NamespaceSnapshotReader;

    let mut context = read_context(1);
    snapshot
        .namespace_reader()
        .get(&WorkspaceRelativePath::new(path), &mut context)
        .map_err(Into::into)
}

pub(super) fn get_descriptor_entry(
    snapshot: &SnapshotContent,
    path: &str,
) -> Result<Option<NamespaceEntry>, WorkViewError> {
    let mut context = read_context(1);
    snapshot
        .namespace_reader()
        .descriptor(&WorkspaceRelativePath::new(path), &mut context)
        .map(|descriptor| descriptor.map(|descriptor| descriptor.entry_without_layout))
        .map_err(Into::into)
}

pub(super) fn read_context(entry_count: u64) -> NamespaceOperationContext<'static> {
    let namespace_pages = entry_count
        .saturating_mul(MAX_PAGE_VISITS_PER_ENTRY)
        .saturating_add(MAX_RADIX_DEPTH);
    let layout_records = entry_count.saturating_mul(MAX_LAYOUT_VISITS_PER_ENTRY);
    let segment_pages = entry_count.saturating_mul(MAX_SEGMENT_PAGE_VISITS_PER_ENTRY);
    let metadata_records = namespace_pages
        .saturating_add(layout_records)
        .saturating_add(segment_pages);
    NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(entry_count, 0, 0).with_metadata_limits(
            namespace_pages,
            layout_records,
            segment_pages,
            metadata_records.saturating_mul(NAMESPACE_PAGE_MAX_BYTES as u64),
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bowline_core::{
        ids::WorkspaceId,
        namespace_snapshot::{NamespaceResource, NamespaceSnapshotReader},
        policy::{MaterializationMode, PathClassification},
        workspace_graph::{
            FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind, SnapshotDraft,
            SnapshotKind,
        },
    };

    use super::*;

    #[test]
    fn zero_entry_budget_rejects_an_exact_page_lookup() {
        let workspace_id = WorkspaceId::new("ws_budget");
        let entries = vec![NamespaceEntry {
            path: "app".to_string(),
            kind: NamespaceEntryKind::Directory,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: Vec::new(),
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: None,
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }];
        let snapshot_id =
            crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
        let snapshot = SnapshotContent::new(
            SnapshotDraft {
                schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
                snapshot_id,
                workspace_id,
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries,
                refs: Vec::new(),
            },
            BTreeMap::new(),
            [7; 32],
        )
        .expect("page-backed snapshot");
        let mut context = read_context(0);

        let error = snapshot
            .namespace_reader()
            .get(&WorkspaceRelativePath::new("app"), &mut context)
            .expect_err("zero-entry budget must fail");

        assert!(matches!(
            error,
            NamespaceReadError::BudgetExceeded {
                resource: NamespaceResource::EntriesVisited,
                ..
            }
        ));
    }
}
