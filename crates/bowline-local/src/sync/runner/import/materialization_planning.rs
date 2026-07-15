use bowline_core::{
    namespace_snapshot::{
        NamespaceDiff, NamespaceDiffVisitor, NamespaceOperationBudget, NamespaceOperationContext,
        NamespaceReadError, NamespaceScope, NamespaceSnapshotReader, NamespaceVisitControl,
    },
    workspace_graph::{
        FileExecutability, NamespaceEntry, NamespaceEntryKind, SnapshotManifest,
        WorkspaceRelativePath,
    },
};

use super::super::SyncRunnerError;
use super::super::helpers::is_nonportable_derivable_git_entry;
use crate::{
    metadata::{
        MaterializationPriorityClass, MaterializationTaskId, MaterializationTaskRecord,
        MaterializationTaskState,
    },
    sync::{
        SnapshotContent,
        materialization::{
            MaterializationPlanningContext, intentionally_absent_in_ordinary_directory,
            planned_task_for_entry, required_in_ordinary_directory,
        },
    },
};

pub(super) fn materialization_task_records_with_context(
    target: &SnapshotContent,
    base: Option<&SnapshotContent>,
    generated_at: &str,
    operation: &mut NamespaceOperationContext<'_>,
) -> Result<Vec<MaterializationTaskRecord>, SyncRunnerError> {
    let reader = target.namespace_reader();
    let metadata = reader.metadata();
    let planning = MaterializationPlanningContext::default();
    let mut tasks = Vec::new();
    reader.visit_prefix_descriptors(
        &WorkspaceRelativePath::new(""),
        operation,
        &mut |descriptor| {
            let entry = descriptor.entry_without_layout;
            if required_in_ordinary_directory(&entry) {
                let planned = planned_task_for_entry(metadata, &entry, &planning);
                tasks.push(materialization_task_record(
                    target.manifest(),
                    planned,
                    entry.executability == FileExecutability::Executable,
                    generated_at,
                ));
            }
            Ok(NamespaceVisitControl::Continue)
        },
    )?;
    tasks.sort_by(|left, right| {
        left.priority_class
            .cmp(&right.priority_class)
            .then_with(|| left.expected_byte_len.cmp(&right.expected_byte_len))
            .then_with(|| left.path.cmp(&right.path))
    });

    if let Some(base) = base {
        let mut deletions = CleanupTaskPlanner {
            target: target.manifest(),
            generated_at,
            tasks: Vec::new(),
        };
        base.namespace_reader().diff_paged(
            &reader,
            &NamespaceScope::All,
            &mut deletions,
            operation,
        )?;
        deletions
            .tasks
            .sort_by(|left, right| left.path.cmp(&right.path));
        tasks.extend(deletions.tasks);
    }
    Ok(tasks)
}

pub(super) fn materialization_task_matches_target(
    task: &MaterializationTaskRecord,
    target: &SnapshotContent,
) -> Result<bool, SyncRunnerError> {
    let (namespace_pages, metadata_bytes) =
        crate::sync::namespace::lazy_namespace_read_limits(target.manifest().entry_count);
    let mut operation = NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(1, 0, 0).with_metadata_limits(
            namespace_pages,
            0,
            0,
            metadata_bytes,
        ),
    );
    let entry = target
        .namespace_reader()
        .descriptor(&WorkspaceRelativePath::new(&task.path), &mut operation)?
        .map(|descriptor| descriptor.entry_without_layout);
    if task.expected_kind == NamespaceEntryKind::Tombstone {
        return Ok(entry
            .as_ref()
            .is_none_or(intentionally_absent_in_ordinary_directory));
    }
    Ok(entry.is_some_and(|entry| {
        entry.kind == task.expected_kind && entry.content_id == task.expected_content_id
    }))
}

fn materialization_task_record(
    target: &SnapshotManifest,
    task: crate::sync::materialization::PlannedMaterializationTask,
    expected_executable: bool,
    generated_at: &str,
) -> MaterializationTaskRecord {
    MaterializationTaskRecord {
        id: task.id,
        workspace_id: task.workspace_id,
        project_id: target.project_id.clone(),
        snapshot_id: task.snapshot_id,
        path: task.path.as_str().to_string(),
        expected_kind: task.expected_kind,
        expected_content_id: task.expected_content_id,
        expected_byte_len: task.expected_bytes.unwrap_or(0),
        expected_executable,
        priority_class: task.priority,
        state: MaterializationTaskState::Queued,
        attempt_count: 0,
        claim_generation: 0,
        not_before: None,
        claim_token: None,
        claimed_by: None,
        claimed_at: None,
        lease_expires_at: None,
        last_error_kind: None,
        last_error: None,
        created_at: generated_at.to_string(),
        updated_at: generated_at.to_string(),
    }
}

struct CleanupTaskPlanner<'a> {
    target: &'a SnapshotManifest,
    generated_at: &'a str,
    tasks: Vec<MaterializationTaskRecord>,
}

impl NamespaceDiffVisitor for CleanupTaskPlanner<'_> {
    fn visit(&mut self, difference: NamespaceDiff) -> Result<(), NamespaceReadError> {
        let entry = match difference {
            NamespaceDiff::Removed(before) => Some(before),
            NamespaceDiff::Modified { before, after }
                if intentionally_absent_in_ordinary_directory(&after) =>
            {
                Some(before)
            }
            NamespaceDiff::Added(_) | NamespaceDiff::Modified { .. } => None,
        };
        if let Some(entry) = entry.filter(|entry| !is_nonportable_derivable_git_entry(entry)) {
            self.tasks.push(deletion_materialization_task(
                self.target,
                &entry,
                self.generated_at,
            ));
        }
        Ok(())
    }
}

fn deletion_materialization_task(
    target: &SnapshotManifest,
    entry: &NamespaceEntry,
    generated_at: &str,
) -> MaterializationTaskRecord {
    MaterializationTaskRecord {
        id: MaterializationTaskId::new(format!(
            "mat_{}",
            crate::sync::short_hash([
                target.workspace_id.as_str().as_bytes(),
                target.snapshot_id.as_str().as_bytes(),
                entry.path.as_bytes(),
                b"delete",
            ])
        )),
        workspace_id: target.workspace_id.clone(),
        project_id: target.project_id.clone(),
        snapshot_id: target.snapshot_id.clone(),
        path: entry.path.clone(),
        expected_kind: NamespaceEntryKind::Tombstone,
        expected_content_id: None,
        expected_byte_len: 0,
        expected_executable: false,
        priority_class: MaterializationPriorityClass::Cleanup,
        state: MaterializationTaskState::Queued,
        attempt_count: 0,
        claim_generation: 0,
        not_before: None,
        claim_token: None,
        claimed_by: None,
        claimed_at: None,
        lease_expires_at: None,
        last_error_kind: None,
        last_error: None,
        created_at: generated_at.to_string(),
        updated_at: generated_at.to_string(),
    }
}

pub(super) fn materialization_task_budget(
    target: &SnapshotContent,
    base: Option<&SnapshotContent>,
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

#[cfg(test)]
mod tests {
    use bowline_core::{ids::WorkspaceId, namespace_snapshot::NamespaceCancellation};

    use super::*;
    use crate::sync::runner::tests::snapshot_with_files;

    #[test]
    fn task_planning_streams_descriptors_without_loading_layouts() {
        let target = snapshot_with_files(
            WorkspaceId::new("ws_streamed_tasks"),
            &[
                ("a.txt", b"a".as_slice()),
                ("b.txt", b"b".as_slice()),
                ("c.txt", b"c".as_slice()),
            ],
        );
        let mut operation = NamespaceOperationContext::uncancelled(
            NamespaceOperationBudget::new(3, 0, 0).with_metadata_limits(
                target.namespace_store().namespace_page_count(),
                0,
                0,
                target.namespace_store().total_encoded_bytes(),
            ),
        );

        let tasks = materialization_task_records_with_context(
            &target,
            None,
            "2026-07-14T00:00:00Z",
            &mut operation,
        )
        .expect("streamed task plan");

        assert_eq!(tasks.len(), 3);
        assert_eq!(operation.counters().entries_visited, 3);
        assert_eq!(operation.counters().layout_records_loaded, 0);
    }

    #[test]
    fn task_planning_propagates_cancellation_with_counters() {
        struct Cancelled;
        impl NamespaceCancellation for Cancelled {
            fn is_cancelled(&self) -> bool {
                true
            }
        }

        let target = snapshot_with_files(
            WorkspaceId::new("ws_cancelled_tasks"),
            &[("src/main.rs", b"fn main() {}".as_slice())],
        );
        let mut operation = NamespaceOperationContext::new(
            NamespaceOperationBudget::new(1, 0, 0).with_metadata_limits(1, 0, 0, 1024 * 1024),
            &Cancelled,
        );

        let error = materialization_task_records_with_context(
            &target,
            None,
            "2026-07-14T00:00:00Z",
            &mut operation,
        )
        .expect_err("cancelled task plan");

        assert!(matches!(
            error,
            SyncRunnerError::NamespaceRead(NamespaceReadError::Cancelled)
        ));
        assert!(operation.counters().cancellation_checks > 0);
    }
}
