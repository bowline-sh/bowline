use std::collections::{BTreeMap, BTreeSet};

use bowline_core::{
    ids::SnapshotId,
    namespace_snapshot::{
        NamespaceBuildError, NamespaceMutation, NamespaceOperationContext,
        NamespaceSnapshotBuilder, NamespaceVisitControl,
    },
    workspace_graph::{
        NamespaceEntry, NamespaceEntryKind, SnapshotKind, WorkspaceRelativePath,
        normalize_workspace_path,
    },
};

use super::{SnapshotContent, SyncRunnerError};

pub(super) fn splice_current_page_graph(
    current: &SnapshotContent,
    prefix: &str,
    branch_paths: &BTreeSet<String>,
    merged: &[NamespaceEntry],
    base_snapshot_id: Option<SnapshotId>,
) -> Result<crate::sync::namespace::BuiltPagedNamespaceSnapshot, SyncRunnerError> {
    let mutation_budget = current
        .manifest()
        .entry_count
        .saturating_add(branch_paths.len() as u64)
        .saturating_add(merged.len() as u64);
    let entry_reads = current
        .manifest()
        .entry_count
        .saturating_mul(8)
        .saturating_add(mutation_budget.saturating_mul(8));
    let mut operation = NamespaceOperationContext::uncancelled(
        crate::sync::namespace::operation_budget(entry_reads, 0, mutation_budget),
    );
    let reader = current.namespace_reader();
    let mut ancestors = merged_ancestors(&reader, merged, prefix, &mut operation)?;
    retain_directories_with_hidden_descendants(
        &reader,
        branch_paths,
        prefix,
        &mut operation,
        &mut ancestors,
    )?;
    let mut builder = crate::sync::namespace::PageNamespaceBuilder::incremental(
        current.namespace_snapshot(),
        &mut operation,
    )
    .map_err(namespace_build_error)?;
    builder.retarget_snapshot(
        None,
        SnapshotKind::WorkspaceHead,
        base_snapshot_id,
        Vec::new(),
    );
    for path in branch_paths {
        builder
            .apply(
                NamespaceMutation::Remove(WorkspaceRelativePath::new(prefixed_path(prefix, path))),
                &mut operation,
            )
            .map_err(namespace_build_error)?;
    }
    for entry in merged {
        let mut entry = entry.clone();
        entry.path = prefixed_path(prefix, &entry.path);
        builder
            .apply(NamespaceMutation::Upsert(entry), &mut operation)
            .map_err(namespace_build_error)?;
    }
    for directory in ancestors.into_values() {
        builder
            .apply(NamespaceMutation::Upsert(directory), &mut operation)
            .map_err(namespace_build_error)?;
    }
    builder
        .finish(&mut operation)
        .map_err(namespace_build_error)
}

fn merged_ancestors(
    reader: &crate::sync::namespace::PageNamespaceReader<'_>,
    merged: &[NamespaceEntry],
    prefix: &str,
    operation: &mut NamespaceOperationContext<'_>,
) -> Result<BTreeMap<String, NamespaceEntry>, SyncRunnerError> {
    let mut ancestors = BTreeMap::new();
    for entry in merged {
        let full_path = prefixed_path(prefix, &entry.path);
        let mut parent = std::path::Path::new(&full_path).parent();
        while let Some(path) = parent {
            let normalized = normalize_workspace_path(&path.display().to_string());
            if normalized.is_empty() {
                break;
            }
            if !ancestors.contains_key(&normalized)
                && let Some(descriptor) =
                    reader.descriptor(&WorkspaceRelativePath::new(&normalized), operation)?
                && descriptor.entry_without_layout.kind == NamespaceEntryKind::Directory
            {
                ancestors.insert(normalized.clone(), descriptor.entry_without_layout);
            }
            parent = path.parent();
        }
    }
    Ok(ancestors)
}

fn retain_directories_with_hidden_descendants(
    reader: &crate::sync::namespace::PageNamespaceReader<'_>,
    branch_paths: &BTreeSet<String>,
    prefix: &str,
    operation: &mut NamespaceOperationContext<'_>,
    ancestors: &mut BTreeMap<String, NamespaceEntry>,
) -> Result<(), SyncRunnerError> {
    for path in branch_paths {
        let full_path = prefixed_path(prefix, path);
        let Some(descriptor) =
            reader.descriptor(&WorkspaceRelativePath::new(&full_path), operation)?
        else {
            continue;
        };
        if descriptor.entry_without_layout.kind != NamespaceEntryKind::Directory {
            continue;
        }
        let mut retained = false;
        reader.visit_prefix_descriptors(
            &WorkspaceRelativePath::new(&full_path),
            operation,
            &mut |candidate| {
                if candidate.entry_without_layout.path != full_path
                    && relative_to_prefix(&candidate.entry_without_layout.path, prefix)
                        .is_some_and(|relative| !branch_paths.contains(relative))
                {
                    retained = true;
                    return Ok(NamespaceVisitControl::Stop);
                }
                Ok(NamespaceVisitControl::Continue)
            },
        )?;
        if retained {
            ancestors.insert(full_path, descriptor.entry_without_layout);
        }
    }
    Ok(())
}

fn prefixed_path(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.to_string()
    } else {
        format!("{prefix}/{path}")
    }
}

fn relative_to_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('/'))
}

fn namespace_build_error(error: NamespaceBuildError) -> SyncRunnerError {
    match error {
        NamespaceBuildError::Read(error) => SyncRunnerError::NamespaceRead(error),
    }
}
