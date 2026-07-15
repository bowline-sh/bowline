use bowline_core::{
    ids::WorkspaceId,
    namespace_snapshot::{
        NamespaceMutation, NamespaceOperationBudget, NamespaceOperationContext,
        NamespaceSnapshotBuilder, NamespaceVisitControl, SnapshotMetadata,
    },
    workspace_graph::{SnapshotKind, WorkspaceRelativePath},
};

use super::{CoalesceError, DEFAULT_SCHEMA_VERSION};
use crate::sync::{
    ScanScope, SnapshotContent,
    namespace::{MetadataIdentityKey, NAMESPACE_PAGE_MAX_BYTES, PageNamespaceBuilder},
};

pub(super) fn namespace_builder_for_scan(
    workspace_id: &WorkspaceId,
    prior_snapshot: Option<&SnapshotContent>,
    scan_scope: &ScanScope,
    workspace_content_key: [u8; 32],
    operation: &mut NamespaceOperationContext<'_>,
) -> Result<PageNamespaceBuilder, CoalesceError> {
    if !matches!(scan_scope, ScanScope::Full(_))
        && let Some(prior) = prior_snapshot
    {
        return PageNamespaceBuilder::incremental(prior.namespace_snapshot(), operation)
            .map_err(CoalesceError::Namespace);
    }
    let empty_identity = super::super::build_manifest_identity(workspace_id, &[], "");
    let empty = PageNamespaceBuilder::new(
        SnapshotMetadata {
            schema_version: DEFAULT_SCHEMA_VERSION,
            snapshot_id: empty_identity.snapshot_id,
            workspace_id: workspace_id.clone(),
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            semantic_manifest_digest: empty_identity.semantic_manifest_digest,
            entry_count: 0,
            refs: Vec::new(),
        },
        MetadataIdentityKey::derive(workspace_id, workspace_content_key),
    )
    .finish(operation)?;
    PageNamespaceBuilder::incremental(&empty, operation).map_err(CoalesceError::Namespace)
}

pub(super) fn remove_owned_prior_scope(
    builder: &mut PageNamespaceBuilder,
    prior_snapshot: Option<&SnapshotContent>,
    scan_scope: &ScanScope,
    operation: &mut NamespaceOperationContext<'_>,
) -> Result<(), CoalesceError> {
    let Some(prior) = prior_snapshot else {
        return Ok(());
    };
    match scan_scope {
        ScanScope::Full(_) => return Ok(()),
        ScanScope::DirtySubtrees { roots, .. } => {
            for root in roots {
                builder.apply(
                    NamespaceMutation::RemovePrefix(WorkspaceRelativePath::new(root)),
                    operation,
                )?;
            }
        }
        ScanScope::RootShallow => {}
    }
    let removes_root_level = matches!(scan_scope, ScanScope::RootShallow)
        || matches!(
            scan_scope,
            ScanScope::DirtySubtrees {
                root_shallow: true,
                ..
            }
        );
    if !removes_root_level {
        return Ok(());
    }
    let mut read_operation = NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(prior.manifest().entry_count, 0, 0).with_metadata_limits(
            prior.namespace_store().namespace_page_count(),
            0,
            0,
            prior.namespace_store().total_encoded_bytes(),
        ),
    );
    let mut build_error = None;
    prior.namespace_reader().visit_prefix_descriptors(
        &WorkspaceRelativePath::new(""),
        &mut read_operation,
        &mut |descriptor| {
            let path = descriptor.entry_without_layout.path;
            if !path.contains('/')
                && let Err(error) = builder.apply(
                    NamespaceMutation::Remove(WorkspaceRelativePath::new(path)),
                    operation,
                )
            {
                build_error = Some(error);
                return Ok(NamespaceVisitControl::Stop);
            }
            Ok(NamespaceVisitControl::Continue)
        },
    )?;
    build_error.map_or(Ok(()), |error| Err(CoalesceError::Namespace(error)))
}

pub(super) fn coalescer_namespace_budget(
    prior_snapshot: Option<&SnapshotContent>,
    observed_entries: u64,
    preserved_entries: u64,
) -> NamespaceOperationBudget {
    let prior_entries = prior_snapshot.map_or(0, |snapshot| snapshot.manifest().entry_count);
    let mutations = prior_entries
        .saturating_add(observed_entries)
        .saturating_add(preserved_entries);
    let entry_reads = prior_entries
        .saturating_add(observed_entries)
        .saturating_add(preserved_entries)
        .saturating_mul(8);
    let prior_pages = prior_snapshot.map_or(0, |snapshot| {
        snapshot.namespace_store().namespace_page_count()
    });
    let prior_layouts = prior_snapshot.map_or(0, |snapshot| {
        snapshot.namespace_store().content_layout_count()
    });
    let prior_segments = prior_snapshot.map_or(0, |snapshot| {
        snapshot.namespace_store().segment_page_count()
    });
    let prior_bytes = prior_snapshot.map_or(0, |snapshot| {
        snapshot.namespace_store().total_encoded_bytes()
    });
    let multiplier = mutations.saturating_add(entry_reads).saturating_add(1);
    let page_load_budget = prior_pages
        .saturating_add(mutations.saturating_mul(4))
        .saturating_add(1)
        .saturating_mul(multiplier);
    let layout_load_budget = prior_layouts.saturating_add(mutations).saturating_mul(4);
    let segment_load_budget = prior_segments
        .saturating_add(mutations.saturating_mul(entry_reads.max(1)))
        .saturating_mul(4);
    let created_metadata_budget = page_load_budget
        .saturating_add(layout_load_budget)
        .saturating_add(segment_load_budget)
        .saturating_mul(NAMESPACE_PAGE_MAX_BYTES as u64);
    NamespaceOperationBudget::new(entry_reads, 0, mutations).with_metadata_limits(
        page_load_budget,
        layout_load_budget,
        segment_load_budget,
        prior_bytes
            .saturating_mul(multiplier)
            .saturating_add(created_metadata_budget),
    )
}
