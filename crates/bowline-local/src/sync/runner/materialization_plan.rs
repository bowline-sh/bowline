use std::collections::BTreeSet;

use bowline_core::{
    git_paths::{GitPathClass, classify_git_path},
    namespace_snapshot::{
        NamespaceDiff, NamespaceDiffVisitor, NamespaceOperationBudget, NamespaceOperationContext,
        NamespaceOperationCounters, NamespaceReadError, NamespaceScope, NamespaceVisitControl,
    },
    workspace_graph::{NamespaceEntry, NamespaceEntryKind, WorkspaceRelativePath},
};

use super::helpers::{
    is_excluded_materialization_path, is_nonportable_derivable_git_entry,
    portable_git_worktree_link_entry,
};
use crate::sync::SnapshotContent;

use super::SyncRunnerError;

#[derive(Clone, Copy)]
pub(super) enum MaterializationTargetPhase {
    Directory,
    ImmutableObject,
    OrdinaryWrite,
    PointerState,
}

pub(super) struct MaterializationDeletions {
    pub(super) first: Vec<NamespaceEntry>,
    pub(super) last: Vec<NamespaceEntry>,
}

pub(super) fn visit_materialization_targets(
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    preserved_paths: &BTreeSet<String>,
    intentionally_absent_paths: &BTreeSet<String>,
    selected_path: Option<&str>,
    phase: MaterializationTargetPhase,
    mut visit: impl FnMut(NamespaceEntry) -> Result<(), SyncRunnerError>,
) -> Result<NamespaceOperationCounters, SyncRunnerError> {
    let mut context = NamespaceOperationContext::uncancelled(target_stream_budget(target));
    let mut callback_error = None;
    let mut visit_entry = |entry: NamespaceEntry| {
        let unchanged_from_base = match base
            .map(|base| base.entry_for_path(&entry.path))
            .transpose()
        {
            Ok(base_entry) => base_entry
                .flatten()
                .is_some_and(|base_entry| materialization_identity_matches(&base_entry, &entry)),
            Err(error) => {
                callback_error = Some(error.into());
                return NamespaceVisitControl::Stop;
            }
        };
        if !unchanged_from_base
            && target_entry_matches_phase(
                &entry,
                preserved_paths,
                intentionally_absent_paths,
                phase,
            )
            && let Err(error) = visit(entry)
        {
            callback_error = Some(error);
            return NamespaceVisitControl::Stop;
        }
        NamespaceVisitControl::Continue
    };
    if let Some(path) = selected_path {
        if let Some(descriptor) = target
            .namespace_reader()
            .descriptor(&WorkspaceRelativePath::new(path), &mut context)?
        {
            visit_entry(descriptor.entry_without_layout);
        }
    } else {
        target.namespace_reader().visit_prefix_descriptors(
            &WorkspaceRelativePath::new(""),
            &mut context,
            &mut |descriptor| Ok(visit_entry(descriptor.entry_without_layout)),
        )?;
    }
    if let Some(error) = callback_error {
        return Err(error);
    }
    Ok(context.counters())
}

fn materialization_identity_matches(base: &NamespaceEntry, target: &NamespaceEntry) -> bool {
    base.kind == target.kind
        && base.classification == target.classification
        && base.mode == target.mode
        && base.content_id == target.content_id
        && base.symlink_target == target.symlink_target
        && base.byte_len == target.byte_len
        && base.executability == target.executability
}

pub(super) fn materialization_deletions(
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    preserved_paths: &BTreeSet<String>,
    intentionally_absent_paths: &BTreeSet<String>,
    selected_path: Option<&str>,
) -> Result<MaterializationDeletions, NamespaceReadError> {
    let Some(base) = base else {
        return Ok(MaterializationDeletions {
            first: Vec::new(),
            last: Vec::new(),
        });
    };
    let mut context =
        NamespaceOperationContext::uncancelled(materialization_budget(Some(base), target));
    let mut plan = MaterializationPlan::empty();
    if let Some(path) = selected_path {
        plan = plan_materialization_for_path_with_context(
            Some(base),
            target,
            preserved_paths,
            intentionally_absent_paths,
            path,
            &mut context,
        )?;
    } else {
        let mut deletions = DeletionPlanner {
            preserved_paths,
            intentionally_absent_paths,
            plan: &mut plan,
        };
        base.namespace_reader().diff_paged(
            &target.namespace_reader(),
            &NamespaceScope::All,
            &mut deletions,
            &mut context,
        )?;
        plan.sort();
    }
    Ok(MaterializationDeletions {
        first: plan.deletes_first,
        last: plan.deletes_last,
    })
}

#[derive(Debug)]
pub(super) struct MaterializationPlan {
    pub(super) deletes_first: Vec<NamespaceEntry>,
    pub(super) dirs: Vec<NamespaceEntry>,
    pub(super) writes: Vec<NamespaceEntry>,
    pub(super) deletes_last: Vec<NamespaceEntry>,
}

impl MaterializationPlan {
    fn empty() -> Self {
        Self {
            deletes_first: Vec::new(),
            dirs: Vec::new(),
            writes: Vec::new(),
            deletes_last: Vec::new(),
        }
    }

    fn sort(&mut self) {
        self.deletes_first
            .sort_by_key(|entry| std::cmp::Reverse(entry.path.len()));
        self.deletes_last
            .sort_by_key(|entry| std::cmp::Reverse(entry.path.len()));
        self.dirs.sort_by(|left, right| left.path.cmp(&right.path));
        self.writes.sort_by(|left, right| {
            write_class(left)
                .cmp(&write_class(right))
                .then_with(|| left.path.cmp(&right.path))
        });
    }

    fn add_deletion(&mut self, entry: NamespaceEntry) {
        if classify_git_path(&entry.path) == Some(GitPathClass::ImmutableObject) {
            self.deletes_last.push(entry);
        } else {
            self.deletes_first.push(entry);
        }
    }

    fn add_target(&mut self, entry: NamespaceEntry) {
        if entry.kind == NamespaceEntryKind::Directory {
            self.dirs.push(entry);
        } else if matches!(
            entry.kind,
            NamespaceEntryKind::File | NamespaceEntryKind::Symlink
        ) {
            match classify_git_path(&entry.path) {
                Some(GitPathClass::DerivableVolatile)
                    if portable_git_worktree_link_entry(&entry).is_some() =>
                {
                    self.writes.push(entry);
                }
                Some(GitPathClass::DerivableVolatile) => {}
                Some(GitPathClass::ImmutableObject)
                | Some(GitPathClass::PointerState)
                | Some(GitPathClass::OrdinaryState)
                | None => self.writes.push(entry),
            }
        }
    }
}

#[cfg(test)]
pub(super) fn plan_materialization(
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    excluded_paths: &BTreeSet<String>,
) -> Result<MaterializationPlan, NamespaceReadError> {
    plan_materialization_with_path_policy(base, target, excluded_paths, &BTreeSet::new())
}

#[cfg(test)]
pub(super) fn plan_materialization_with_path_policy(
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    preserved_paths: &BTreeSet<String>,
    intentionally_absent_paths: &BTreeSet<String>,
) -> Result<MaterializationPlan, NamespaceReadError> {
    let mut context = NamespaceOperationContext::uncancelled(materialization_budget(base, target));
    plan_materialization_with_context(
        base,
        target,
        preserved_paths,
        intentionally_absent_paths,
        &mut context,
    )
}

#[cfg(test)]
pub(super) fn plan_materialization_with_context(
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    preserved_paths: &BTreeSet<String>,
    intentionally_absent_paths: &BTreeSet<String>,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<MaterializationPlan, NamespaceReadError> {
    let mut plan = MaterializationPlan::empty();
    target.namespace_reader().visit_prefix_descriptors(
        &WorkspaceRelativePath::new(""),
        context,
        &mut |descriptor| {
            let entry = descriptor.entry_without_layout;
            if !is_nonportable_derivable_git_entry(&entry)
                && !is_excluded_materialization_path(&entry.path, preserved_paths)
                && !is_excluded_materialization_path(&entry.path, intentionally_absent_paths)
            {
                plan.add_target(entry);
            }
            Ok(NamespaceVisitControl::Continue)
        },
    )?;

    if let Some(base) = base {
        let mut deletions = DeletionPlanner {
            preserved_paths,
            intentionally_absent_paths,
            plan: &mut plan,
        };
        base.namespace_reader().diff_paged(
            &target.namespace_reader(),
            &NamespaceScope::All,
            &mut deletions,
            context,
        )?;
    }
    plan.sort();
    Ok(plan)
}

pub(super) fn plan_materialization_for_path_with_context(
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    preserved_paths: &BTreeSet<String>,
    intentionally_absent_paths: &BTreeSet<String>,
    selected_path: &str,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<MaterializationPlan, NamespaceReadError> {
    let path = WorkspaceRelativePath::new(selected_path);
    let target_entry = target
        .namespace_reader()
        .descriptor(&path, context)?
        .map(|descriptor| descriptor.entry_without_layout);
    let target_is_absent = target_entry.as_ref().is_none_or(|entry| {
        is_excluded_materialization_path(&entry.path, intentionally_absent_paths)
    });
    let mut plan = MaterializationPlan::empty();
    if target_is_absent {
        if let Some(base_entry) = base
            .map(|base| base.namespace_reader().descriptor(&path, context))
            .transpose()?
            .flatten()
            .map(|descriptor| descriptor.entry_without_layout)
            .filter(|entry| {
                !is_nonportable_derivable_git_entry(entry)
                    && !is_excluded_materialization_path(&entry.path, preserved_paths)
            })
        {
            plan.add_deletion(base_entry);
        }
    } else if let Some(entry) = target_entry.filter(|entry| {
        !is_nonportable_derivable_git_entry(entry)
            && !is_excluded_materialization_path(&entry.path, preserved_paths)
    }) {
        plan.add_target(entry);
    }
    plan.sort();
    Ok(plan)
}

struct DeletionPlanner<'a> {
    preserved_paths: &'a BTreeSet<String>,
    intentionally_absent_paths: &'a BTreeSet<String>,
    plan: &'a mut MaterializationPlan,
}

impl NamespaceDiffVisitor for DeletionPlanner<'_> {
    fn visit(&mut self, difference: NamespaceDiff) -> Result<(), NamespaceReadError> {
        let candidate = match difference {
            NamespaceDiff::Removed(before) => Some(before),
            NamespaceDiff::Modified { before, after }
                if is_excluded_materialization_path(
                    &after.path,
                    self.intentionally_absent_paths,
                ) =>
            {
                Some(before)
            }
            NamespaceDiff::Added(_) | NamespaceDiff::Modified { .. } => None,
        };
        if let Some(entry) = candidate.filter(|entry| {
            !is_nonportable_derivable_git_entry(entry)
                && !is_excluded_materialization_path(&entry.path, self.preserved_paths)
        }) {
            self.plan.add_deletion(entry);
        }
        Ok(())
    }
}

fn materialization_budget(
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
) -> NamespaceOperationBudget {
    let base_entries = base.map_or(0, |snapshot| snapshot.manifest().entry_count);
    let base_pages = base.map_or(0, |snapshot| {
        snapshot.namespace_store().namespace_page_count()
    });
    let base_layouts = base.map_or(0, |snapshot| {
        snapshot.namespace_store().content_layout_count()
    });
    let base_segments = base.map_or(0, |snapshot| {
        snapshot.namespace_store().segment_page_count()
    });
    let base_bytes = base.map_or(0, |snapshot| {
        snapshot.namespace_store().total_encoded_bytes()
    });
    let diff_entries = base_entries.saturating_add(target.manifest().entry_count);
    let diff_read_multiplier = diff_entries.saturating_add(2);
    let page_count = base_pages.saturating_add(target.namespace_store().namespace_page_count());
    let layout_count = base_layouts.saturating_add(target.namespace_store().content_layout_count());
    let segment_count = base_segments.saturating_add(target.namespace_store().segment_page_count());
    let encoded_bytes = base_bytes.saturating_add(target.namespace_store().total_encoded_bytes());
    NamespaceOperationBudget::new(target.manifest().entry_count, diff_entries, 0)
        .with_metadata_limits(
            page_count.saturating_mul(diff_read_multiplier),
            layout_count.saturating_mul(diff_entries.max(1)),
            segment_count.saturating_mul(diff_entries.max(1)),
            encoded_bytes.saturating_mul(diff_read_multiplier),
        )
}

fn write_class(entry: &NamespaceEntry) -> u8 {
    match classify_git_path(&entry.path) {
        Some(GitPathClass::ImmutableObject) => 0,
        Some(GitPathClass::PointerState) => 2,
        Some(GitPathClass::DerivableVolatile) | Some(GitPathClass::OrdinaryState) | None => 1,
    }
}

fn target_entry_matches_phase(
    entry: &NamespaceEntry,
    preserved_paths: &BTreeSet<String>,
    intentionally_absent_paths: &BTreeSet<String>,
    phase: MaterializationTargetPhase,
) -> bool {
    if is_nonportable_derivable_git_entry(entry)
        || is_excluded_materialization_path(&entry.path, preserved_paths)
        || is_excluded_materialization_path(&entry.path, intentionally_absent_paths)
    {
        return false;
    }
    match phase {
        MaterializationTargetPhase::Directory => entry.kind == NamespaceEntryKind::Directory,
        MaterializationTargetPhase::ImmutableObject => {
            matches!(
                entry.kind,
                NamespaceEntryKind::File | NamespaceEntryKind::Symlink
            ) && classify_git_path(&entry.path) == Some(GitPathClass::ImmutableObject)
        }
        MaterializationTargetPhase::PointerState => {
            matches!(
                entry.kind,
                NamespaceEntryKind::File | NamespaceEntryKind::Symlink
            ) && classify_git_path(&entry.path) == Some(GitPathClass::PointerState)
        }
        MaterializationTargetPhase::OrdinaryWrite => {
            if !matches!(
                entry.kind,
                NamespaceEntryKind::File | NamespaceEntryKind::Symlink
            ) {
                return false;
            }
            match classify_git_path(&entry.path) {
                Some(GitPathClass::DerivableVolatile) => {
                    portable_git_worktree_link_entry(entry).is_some()
                }
                Some(GitPathClass::OrdinaryState) | None => true,
                Some(GitPathClass::ImmutableObject) | Some(GitPathClass::PointerState) => false,
            }
        }
    }
}

fn target_stream_budget(target: &SnapshotContent) -> NamespaceOperationBudget {
    let (namespace_pages, metadata_bytes) =
        crate::sync::namespace::lazy_namespace_read_limits(target.manifest().entry_count);
    NamespaceOperationBudget::new(target.manifest().entry_count, 0, 0).with_metadata_limits(
        namespace_pages,
        0,
        0,
        metadata_bytes,
    )
}
