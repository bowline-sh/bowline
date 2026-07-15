use bowline_core::{
    ids::{SnapshotId, WorkspaceId},
    namespace_snapshot::{
        NamespaceOperationBudget, NamespaceOperationContext, NamespaceVisitControl,
    },
    workspace_graph::WorkspaceRelativePath,
};

use crate::{metadata::MetadataStore, sync::namespace::NAMESPACE_PAGE_MAX_BYTES};

use super::HistoryError;

pub(super) fn snapshot_contains_prefix(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    prefix: &str,
) -> Result<bool, HistoryError> {
    let Some(snapshot) = store.snapshot(workspace_id, snapshot_id)? else {
        return Ok(false);
    };
    let cached = crate::sync::load_cached_snapshot(store, &snapshot)?;
    let namespace_pages = snapshot.entry_count.saturating_mul(8).saturating_add(1);
    let mut context = NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(1, 0, 0).with_metadata_limits(
            namespace_pages,
            0,
            0,
            namespace_pages.saturating_mul(NAMESPACE_PAGE_MAX_BYTES as u64),
        ),
    );
    let mut found = false;
    cached.namespace_reader().visit_prefix_descriptors(
        &WorkspaceRelativePath::new(prefix),
        &mut context,
        &mut |_descriptor| {
            found = true;
            Ok(NamespaceVisitControl::Stop)
        },
    )?;
    Ok(found)
}
