#[cfg(test)]
use std::cmp::Ordering;

use bowline_core::{
    git_paths::is_git_derivable_volatile_path,
    git_worktree_link::worktree_link_file,
    ids::{ContentId, SnapshotId, WorkspaceId},
    namespace_snapshot::SnapshotMetadata,
    policy::MaterializationMode,
    workspace_graph::{NamespaceEntry, NamespaceEntryKind},
};

#[cfg(test)]
use bowline_core::{
    namespace_snapshot::{
        EntryVisitor, NamespaceOperationContext, NamespaceReadError, NamespaceSnapshotReader,
        NamespaceVisitControl,
    },
    workspace_graph::WorkspaceRelativePath,
};

use super::short_hash;
use crate::metadata::{MaterializationPriorityClass, MaterializationTaskId};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaterializationPath(String);

impl MaterializationPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationSource {
    LocalCache,
    RemotePack,
    MetadataOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMaterializationTask {
    pub id: MaterializationTaskId,
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub path: MaterializationPath,
    pub expected_content_id: Option<ContentId>,
    pub expected_kind: NamespaceEntryKind,
    pub expected_mode: MaterializationMode,
    pub expected_bytes: Option<u64>,
    pub priority: MaterializationPriorityClass,
    pub source: MaterializationSource,
}

#[derive(Debug, Default)]
pub struct MaterializationPlanningContext<'a> {
    pub active_project: Option<&'a str>,
    pub explicit_path: Option<&'a str>,
    pub recent_projects: &'a [&'a str],
    pub cache_resident_content: &'a [ContentId],
}

#[cfg(test)]
pub fn plan_required_materialization(
    namespace: &dyn NamespaceSnapshotReader,
    context: &MaterializationPlanningContext<'_>,
    operation: &mut NamespaceOperationContext<'_>,
) -> Result<Vec<PlannedMaterializationTask>, NamespaceReadError> {
    struct Planner<'a, 'b> {
        metadata: &'a SnapshotMetadata,
        planning: &'b MaterializationPlanningContext<'b>,
        tasks: Vec<PlannedMaterializationTask>,
    }
    impl EntryVisitor for Planner<'_, '_> {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            if required_in_ordinary_directory(entry) {
                self.tasks
                    .push(planned_task_for_entry(self.metadata, entry, self.planning));
            }
            Ok(NamespaceVisitControl::Continue)
        }
    }

    let mut planner = Planner {
        metadata: namespace.metadata(),
        planning: context,
        tasks: Vec::new(),
    };
    namespace.visit_prefix(&WorkspaceRelativePath::new(""), &mut planner, operation)?;
    planner.tasks.sort_by(compare_tasks);
    Ok(planner.tasks)
}

pub fn required_in_ordinary_directory(entry: &NamespaceEntry) -> bool {
    !matches!(
        entry.mode,
        MaterializationMode::StructureOnly
            | MaterializationMode::LocalRegenerate
            | MaterializationMode::LocalCache
            | MaterializationMode::Ignore
            | MaterializationMode::LocalOnly
            | MaterializationMode::Blocked
    ) && !matches!(
        entry.kind,
        NamespaceEntryKind::Placeholder | NamespaceEntryKind::Tombstone
    ) && (!is_git_derivable_volatile_path(&entry.path)
        || worktree_link_file(&entry.path, entry.kind).is_some())
}

pub fn intentionally_absent_in_ordinary_directory(entry: &NamespaceEntry) -> bool {
    entry.kind != NamespaceEntryKind::Directory && !required_in_ordinary_directory(entry)
}

pub(crate) fn planned_task_for_entry(
    manifest: &SnapshotMetadata,
    entry: &NamespaceEntry,
    context: &MaterializationPlanningContext<'_>,
) -> PlannedMaterializationTask {
    let priority = priority_for(entry, context);
    let source = if entry.kind != NamespaceEntryKind::File {
        MaterializationSource::MetadataOnly
    } else if entry.content_id.as_ref().is_some_and(|content_id| {
        context
            .cache_resident_content
            .iter()
            .any(|cached| cached == content_id)
    }) {
        MaterializationSource::LocalCache
    } else {
        MaterializationSource::RemotePack
    };
    let expected_content = entry
        .content_id
        .as_ref()
        .map_or("metadata", ContentId::as_str);
    PlannedMaterializationTask {
        id: MaterializationTaskId::new(format!(
            "mat_{}",
            short_hash([
                manifest.workspace_id.as_str().as_bytes(),
                manifest.snapshot_id.as_str().as_bytes(),
                entry.path.as_bytes(),
                expected_content.as_bytes(),
            ])
        )),
        workspace_id: manifest.workspace_id.clone(),
        snapshot_id: manifest.snapshot_id.clone(),
        path: MaterializationPath(entry.path.clone()),
        expected_content_id: entry.content_id.clone(),
        expected_kind: entry.kind,
        expected_mode: entry.mode,
        expected_bytes: entry.byte_len,
        priority,
        source,
    }
}

fn priority_for(
    entry: &NamespaceEntry,
    context: &MaterializationPlanningContext<'_>,
) -> MaterializationPriorityClass {
    if is_correctness_critical(&entry.path) {
        return MaterializationPriorityClass::CorrectnessCritical;
    }
    if context
        .active_project
        .is_some_and(|prefix| path_is_in(&entry.path, prefix))
    {
        return MaterializationPriorityClass::ActiveProject;
    }
    if context
        .explicit_path
        .is_some_and(|prefix| path_is_in(&entry.path, prefix))
    {
        return MaterializationPriorityClass::RequestedPath;
    }
    if context
        .recent_projects
        .iter()
        .any(|prefix| path_is_in(&entry.path, prefix))
    {
        return MaterializationPriorityClass::RecentProject;
    }
    if entry.byte_len.unwrap_or(0) <= 1024 * 1024 {
        MaterializationPriorityClass::SmallFile
    } else {
        MaterializationPriorityClass::BackgroundLarge
    }
}

fn path_is_in(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_matches('/');
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_correctness_critical(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        ".bowlineignore"
            | ".bowlinesetup"
            | "package.json"
            | "Cargo.toml"
            | "pyproject.toml"
            | "go.mod"
    ) || path.starts_with(".git/objects/")
}

#[cfg(test)]
fn compare_tasks(
    left: &PlannedMaterializationTask,
    right: &PlannedMaterializationTask,
) -> Ordering {
    left.priority
        .cmp(&right.priority)
        .then_with(|| {
            left.expected_bytes
                .unwrap_or(0)
                .cmp(&right.expected_bytes.unwrap_or(0))
        })
        .then_with(|| left.path.cmp(&right.path))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bowline_core::{
        ids::{ContentId, PackId},
        namespace_snapshot::NamespaceOperationBudget,
        policy::{MaterializationMode, PathClassification},
        workspace_graph::{
            ContentLayout, ContentLocator, ContentStorage, FileExecutability, HydrationState,
            SNAPSHOT_SCHEMA_VERSION, SnapshotDraft, SnapshotKind,
        },
    };

    use crate::sync::{SnapshotContent, rebuild_manifest_identity};

    use super::*;

    fn snapshot(entries: Vec<NamespaceEntry>) -> SnapshotContent {
        let workspace_id = WorkspaceId::new("ws_materialize");
        let snapshot_id = rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
        SnapshotContent::new(
            SnapshotDraft {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
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
        .expect("page-backed materialization snapshot")
    }

    fn plan(
        snapshot: &SnapshotContent,
        planning: &MaterializationPlanningContext<'_>,
    ) -> Vec<PlannedMaterializationTask> {
        let mut operation = NamespaceOperationContext::uncancelled(NamespaceOperationBudget::new(
            snapshot.manifest().entry_count,
            0,
            0,
        ));
        plan_required_materialization(&snapshot.namespace_reader(), planning, &mut operation)
            .expect("materialization plan")
    }

    fn file(path: &str, mode: MaterializationMode, bytes: u64) -> NamespaceEntry {
        let content_id = ContentId::new(format!("cid_{}", path.replace('/', "_")));
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode,
            access: Vec::new(),
            content_id: Some(content_id.clone()),
            content_layout: Some(
                ContentLayout::single_segment(ContentLocator {
                    content_id,
                    storage: ContentStorage::Packed,
                    raw_size: bytes,
                    pack_id: Some(PackId::new("pack_materialize")),
                    offset: Some(0),
                    length: Some(bytes),
                })
                .expect("test layout"),
            ),
            symlink_target: None,
            byte_len: Some(bytes),
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::Cold,
        }
    }

    #[test]
    fn lazy_is_priority_not_an_absence_contract() {
        let snapshot = snapshot(vec![
            file(
                "archive/video.mov",
                MaterializationMode::Lazy,
                8 * 1024 * 1024,
            ),
            file("app/src/main.rs", MaterializationMode::WorkspaceSync, 20),
        ]);
        let tasks = plan(
            &snapshot,
            &MaterializationPlanningContext {
                active_project: Some("app"),
                ..MaterializationPlanningContext::default()
            },
        );
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].path.as_str(), "app/src/main.rs");
        assert_eq!(tasks[1].path.as_str(), "archive/video.mov");
        assert_eq!(
            tasks[1].priority,
            MaterializationPriorityClass::BackgroundLarge
        );
    }

    #[test]
    fn intentionally_local_paths_never_enter_the_queue() {
        let modes = [
            MaterializationMode::StructureOnly,
            MaterializationMode::LocalRegenerate,
            MaterializationMode::LocalCache,
            MaterializationMode::Ignore,
            MaterializationMode::LocalOnly,
            MaterializationMode::Blocked,
        ];
        let entries = modes
            .into_iter()
            .enumerate()
            .map(|(index, mode)| file(&format!("excluded/{index}"), mode, 10))
            .collect();
        assert!(
            plan(
                &snapshot(entries),
                &MaterializationPlanningContext::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn excluded_directory_remains_scaffolding_for_included_children() {
        let mut directory = file("generated", MaterializationMode::LocalRegenerate, 0);
        directory.kind = NamespaceEntryKind::Directory;
        directory.content_id = None;
        directory.content_layout = None;
        directory.byte_len = None;

        assert!(!intentionally_absent_in_ordinary_directory(&directory));
        assert!(intentionally_absent_in_ordinary_directory(&file(
            "generated/cache.json",
            MaterializationMode::LocalRegenerate,
            10,
        )));
    }

    #[test]
    fn volatile_git_paths_never_enter_the_queue() {
        let snapshot = snapshot(vec![
            file(".git/logs/HEAD", MaterializationMode::WorkspaceSync, 10),
            file(".git/config.lock", MaterializationMode::WorkspaceSync, 10),
            file("src/main.rs", MaterializationMode::WorkspaceSync, 10),
        ]);

        let tasks = plan(&snapshot, &MaterializationPlanningContext::default());

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].path.as_str(), "src/main.rs");
    }

    #[test]
    fn stable_classes_precede_size_and_path_order() {
        let snapshot = snapshot(vec![
            file(
                "z/large.bin",
                MaterializationMode::WorkspaceSync,
                2 * 1024 * 1024,
            ),
            file("app/z.rs", MaterializationMode::WorkspaceSync, 50),
            file("package.json", MaterializationMode::WorkspaceSync, 500),
            file("a/small.rs", MaterializationMode::WorkspaceSync, 10),
        ]);
        let tasks = plan(
            &snapshot,
            &MaterializationPlanningContext {
                active_project: Some("app"),
                ..MaterializationPlanningContext::default()
            },
        );
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.path.as_str())
                .collect::<Vec<_>>(),
            ["package.json", "app/z.rs", "a/small.rs", "z/large.bin"]
        );
    }
}
