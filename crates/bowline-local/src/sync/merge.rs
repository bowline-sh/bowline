use std::{collections::BTreeMap, error::Error, fmt, io};

use bowline_core::{
    git_paths::{GitPathClass, classify_git_path},
    ids::ContentId,
    namespace_snapshot::{
        EntryVisitor, NamespaceCancellation, NamespaceDiff, NamespaceDiffVisitor,
        NamespaceMutation, NamespaceOperationBudget, NamespaceOperationContext, NamespaceReadError,
        NamespaceScope, NamespaceSnapshotBuilder, NamespaceSnapshotReader, NamespaceVisitControl,
    },
    workspace_graph::{
        FileExecutability, NamespaceEntry, NamespaceEntryKind, WorkspaceRelativePath,
        workspace_content_id,
    },
};

use super::{
    CandidateBase, PreparedContent, SnapshotContent,
    coalescer::SnapshotCandidate,
    conflicts::ConflictRecord,
    line_merge::{TextMergeOutcome, merge_text_lines},
    manifest_id_for_snapshot,
    merge_plugins::{
        ExternalMergeDecision, MergePluginRegistry, built_in_merge_plugin_conflicts_by_default,
        structured_merge_output_is_valid,
    },
    namespace::{PageNamespaceBuilder, operation_budget},
};

mod conflict_span;
mod entry_match;
mod env_merge;
use conflict_span::conflict_span;
use entry_match::{entries_match_except_executability, optional_entries_match_for_merge};
pub(crate) use env_merge::is_env_path;
use env_merge::{EnvMergeOutcome, merge_env_bytes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean(Box<SnapshotCandidate>),
    Conflicted(Vec<ConflictRecord>),
}

pub trait MergeContentReader {
    fn visit_entries(
        &self,
        visitor: &mut dyn FnMut(&NamespaceEntry),
    ) -> Result<(), NamespaceReadError>;
    fn read_file_for_path(&self, path: &str) -> io::Result<Option<Vec<u8>>>;
}

impl MergeContentReader for SnapshotContent {
    fn visit_entries(
        &self,
        visitor: &mut dyn FnMut(&NamespaceEntry),
    ) -> Result<(), NamespaceReadError> {
        struct MergeVisitor<'a>(&'a mut dyn FnMut(&NamespaceEntry));
        impl EntryVisitor for MergeVisitor<'_> {
            fn visit(
                &mut self,
                entry: &NamespaceEntry,
                _context: &mut NamespaceOperationContext<'_>,
            ) -> Result<NamespaceVisitControl, NamespaceReadError> {
                (self.0)(entry);
                Ok(NamespaceVisitControl::Continue)
            }
        }

        let mut context = NamespaceOperationContext::uncancelled(operation_budget(
            self.manifest().entry_count,
            self.manifest().entry_count,
            0,
        ));
        self.namespace_reader().visit_prefix(
            &WorkspaceRelativePath::new(""),
            &mut MergeVisitor(visitor),
            &mut context,
        )?;
        Ok(())
    }

    fn read_file_for_path(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        SnapshotContent::read_file_for_path(self, path)
    }
}

pub struct MergeTreeInput<'a> {
    pub base: &'a dyn MergeContentReader,
    pub left: &'a dyn MergeContentReader,
    pub right: &'a dyn MergeContentReader,
    pub workspace_content_key: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeTreeOutcome {
    Clean(MergedNamespace),
    Conflicted(Vec<ConflictRecord>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedNamespace {
    pub entries: Vec<NamespaceEntry>,
    pub prepared_content: BTreeMap<ContentId, PreparedContent>,
}

pub fn merge_tree(input: MergeTreeInput<'_>) -> Result<MergeTreeOutcome, MergeError> {
    let plugins = MergePluginRegistry::built_in();
    merge_tree_with_plugins(input, &plugins)
}

pub(crate) fn merge_required_content_paths(
    input: &MergeTreeInput<'_>,
) -> Result<std::collections::BTreeSet<String>, MergeError> {
    Ok(indexed_merge_paths(input)?
        .into_iter()
        .filter_map(|(path, sides)| path_requires_content_merge(&path, &sides).then_some(path))
        .collect())
}

pub(crate) fn stale_merge_required_content_paths(
    input: &MergeTreeInput<'_>,
) -> Result<std::collections::BTreeSet<String>, MergeError> {
    Ok(indexed_merge_paths(input)?
        .into_iter()
        .filter_map(|(path, sides)| {
            let remote_changed_file = sides.remote.as_ref().is_some_and(|remote| {
                remote.kind == NamespaceEntryKind::File
                    && !optional_entries_match_for_merge(sides.base.as_ref(), Some(remote))
            });
            (remote_changed_file || path_requires_content_merge(&path, &sides)).then_some(path)
        })
        .collect())
}

fn indexed_merge_paths(
    input: &MergeTreeInput<'_>,
) -> Result<BTreeMap<String, MergePath>, MergeError> {
    let mut paths = BTreeMap::<String, MergePath>::new();
    index_paths(&mut paths, Side::Base, input.base)?;
    index_paths(&mut paths, Side::Local, input.left)?;
    index_paths(&mut paths, Side::Remote, input.right)?;
    Ok(paths)
}

fn path_requires_content_merge(path: &str, sides: &MergePath) -> bool {
    if classify_git_path(path).is_some() {
        return false;
    }
    let base = sides.base.as_ref();
    let local = sides.local.as_ref();
    let remote = sides.remote.as_ref();
    if optional_entries_match_for_merge(local, remote)
        || optional_entries_match_for_merge(base, local)
        || optional_entries_match_for_merge(base, remote)
    {
        return false;
    }
    let (Some(local), Some(remote)) = (local, remote) else {
        return false;
    };
    if local.kind != NamespaceEntryKind::File || remote.kind != NamespaceEntryKind::File {
        return false;
    }
    if entries_match_except_executability(local, remote)
        || base.is_some_and(|entry| entries_match_except_executability(entry, local))
        || base.is_some_and(|entry| entries_match_except_executability(entry, remote))
    {
        return false;
    }
    true
}

pub fn merge_snapshots(
    base: &SnapshotContent,
    local: &SnapshotCandidate,
    remote: &SnapshotContent,
    remote_base: CandidateBase,
    workspace_content_key: [u8; 32],
    created_at: impl Into<String>,
) -> Result<MergeOutcome, MergeError> {
    let plugins = MergePluginRegistry::built_in();
    merge_snapshots_with_plugins(
        base,
        local,
        remote,
        MergeSnapshotsOptions {
            remote_base,
            workspace_content_key,
            created_at: created_at.into(),
            plugins: &plugins,
            cancellation: None,
        },
    )
}

pub(crate) struct MergeSnapshotsOptions<'a> {
    pub(crate) remote_base: CandidateBase,
    pub(crate) workspace_content_key: [u8; 32],
    pub(crate) created_at: String,
    pub(crate) plugins: &'a MergePluginRegistry,
    pub(crate) cancellation: Option<&'a dyn NamespaceCancellation>,
}

pub(crate) fn merge_snapshots_with_plugins(
    base: &SnapshotContent,
    local: &SnapshotCandidate,
    remote: &SnapshotContent,
    options: MergeSnapshotsOptions<'_>,
) -> Result<MergeOutcome, MergeError> {
    let merged = merge_paged_snapshots(
        base,
        &local.snapshot,
        remote,
        options.workspace_content_key,
        options.plugins,
        options.cancellation,
    )?;
    let (mut namespace, prepared_content) = match merged {
        PagedMergeOutcome::Clean {
            namespace,
            prepared_content,
        } => (*namespace, prepared_content),
        PagedMergeOutcome::Conflicted(conflicts) => {
            return Ok(MergeOutcome::Conflicted(conflicts));
        }
    };
    let snapshot_id = namespace.snapshot_id.clone();
    let mut refs = namespace.metadata.refs.clone();
    for reference in &mut refs {
        if reference.name == "workspace" {
            reference.target_snapshot_id = snapshot_id.clone();
        }
    }
    namespace.metadata.refs = refs;
    namespace.metadata.base_snapshot_id = None;
    let manifest_identity = super::ManifestIdentityReport {
        snapshot_id: namespace.snapshot_id.clone(),
        semantic_manifest_digest: namespace.semantic_manifest_digest.clone(),
        entries_hashed: namespace.changed.semantic_entries_hashed,
    };
    let snapshot = SnapshotContent::from_built(namespace, prepared_content);

    Ok(MergeOutcome::Clean(Box::new(SnapshotCandidate {
        base: options.remote_base,
        device_id: local.device_id.clone(),
        manifest_id: manifest_id_for_snapshot(&snapshot_id),
        snapshot,
        scan_report: local.scan_report.clone(),
        scan_scope: local.scan_scope.clone(),
        stat_cache_hit_paths: local.stat_cache_hit_paths.clone(),
        stat_cache_divergences: local.stat_cache_divergences.clone(),
        scan_stats: Default::default(),
        manifest_identity,
        stat_cache_write_back: None,
        causation_ids: {
            let mut ids = local.causation_ids.clone();
            ids.push(format!("merge:{}", remote.manifest.snapshot_id.as_str()));
            ids
        },
        skipped_unsafe_symlinks: local.skipped_unsafe_symlinks.clone(),
        created_at: options.created_at,
    })))
}

enum PagedMergeOutcome {
    Clean {
        namespace: Box<super::namespace::BuiltPagedNamespaceSnapshot>,
        prepared_content: BTreeMap<ContentId, PreparedContent>,
    },
    Conflicted(Vec<ConflictRecord>),
}

fn merge_paged_snapshots(
    base: &SnapshotContent,
    local: &SnapshotContent,
    remote: &SnapshotContent,
    workspace_content_key: [u8; 32],
    plugins: &MergePluginRegistry,
    cancellation: Option<&dyn NamespaceCancellation>,
) -> Result<PagedMergeOutcome, MergeError> {
    let local_changes = paged_changes(base, local, cancellation)?;
    let remote_changes = paged_changes(base, remote, cancellation)?;
    let changed_paths = local_changes
        .keys()
        .chain(remote_changes.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let entry_limit = changed_paths.len().saturating_mul(16).saturating_add(16) as u64;
    let mut context = merge_operation_context(
        operation_budget(entry_limit, entry_limit, changed_paths.len() as u64),
        cancellation,
    );
    let mut builder = PageNamespaceBuilder::incremental(remote.namespace_snapshot(), &mut context)?;
    let mut prepared_content = BTreeMap::new();
    let mut conflicts = Vec::new();

    for path in changed_paths {
        let workspace_path = WorkspaceRelativePath::new(&path);
        let base_entry = base.namespace_reader().get(&workspace_path, &mut context)?;
        let local_entry = local_changes
            .get(&path)
            .cloned()
            .unwrap_or_else(|| base_entry.clone());
        let remote_entry = remote_changes
            .get(&path)
            .cloned()
            .unwrap_or_else(|| base_entry.clone());
        let sides = MergePath {
            base: base_entry,
            local: local_entry,
            remote: remote_entry.clone(),
        };
        match merge_path(
            &path,
            &sides,
            base,
            local,
            remote,
            workspace_content_key,
            plugins,
        )? {
            PathMerge::Entry { entry, bytes } => {
                if remote_entry.as_ref() != Some(&entry) {
                    builder.apply(NamespaceMutation::Upsert(entry.clone()), &mut context)?;
                }
                if let Some(bytes) = bytes {
                    let content_id = entry
                        .content_id
                        .clone()
                        .ok_or(MergeError::MissingContentId)?;
                    prepared_content.insert(
                        content_id.clone(),
                        PreparedContent::memory(content_id, bytes),
                    );
                }
            }
            PathMerge::Deleted => {
                if remote_entry.is_some() {
                    builder.apply(NamespaceMutation::Remove(workspace_path), &mut context)?;
                }
            }
            PathMerge::Conflict(conflict) => conflicts.push(conflict),
        }
    }
    if !conflicts.is_empty() {
        return Ok(PagedMergeOutcome::Conflicted(conflicts));
    }
    let namespace = builder.finish(&mut context)?;
    Ok(PagedMergeOutcome::Clean {
        namespace: Box::new(namespace),
        prepared_content,
    })
}

fn paged_changes(
    base: &SnapshotContent,
    side: &SnapshotContent,
    cancellation: Option<&dyn NamespaceCancellation>,
) -> Result<BTreeMap<String, Option<NamespaceEntry>>, MergeError> {
    struct Collector(BTreeMap<String, Option<NamespaceEntry>>);
    impl NamespaceDiffVisitor for Collector {
        fn visit(&mut self, difference: NamespaceDiff) -> Result<(), NamespaceReadError> {
            match difference {
                NamespaceDiff::Added(entry) => {
                    self.0.insert(entry.path.clone(), Some(entry));
                }
                NamespaceDiff::Removed(entry) => {
                    self.0.insert(entry.path, None);
                }
                NamespaceDiff::Modified { after, .. } => {
                    self.0.insert(after.path.clone(), Some(after));
                }
            }
            Ok(())
        }
    }
    let limit = base
        .manifest()
        .entry_count
        .saturating_add(side.manifest().entry_count)
        .saturating_mul(4)
        .saturating_add(16);
    let mut context = merge_operation_context(operation_budget(limit, limit, 0), cancellation);
    let mut collector = Collector(BTreeMap::new());
    base.namespace_reader().diff_paged(
        &side.namespace_reader(),
        &NamespaceScope::All,
        &mut collector,
        &mut context,
    )?;
    Ok(collector.0)
}

fn merge_operation_context<'a>(
    budget: NamespaceOperationBudget,
    cancellation: Option<&'a dyn NamespaceCancellation>,
) -> NamespaceOperationContext<'a> {
    match cancellation {
        Some(cancellation) => NamespaceOperationContext::new(budget, cancellation),
        None => NamespaceOperationContext::uncancelled(budget),
    }
}

fn merge_tree_with_plugins(
    input: MergeTreeInput<'_>,
    plugins: &MergePluginRegistry,
) -> Result<MergeTreeOutcome, MergeError> {
    let mut paths = BTreeMap::<String, MergePath>::new();
    index_paths(&mut paths, Side::Base, input.base)?;
    index_paths(&mut paths, Side::Local, input.left)?;
    index_paths(&mut paths, Side::Remote, input.right)?;

    let mut merged_entries = Vec::new();
    let mut prepared_content = BTreeMap::<ContentId, PreparedContent>::new();
    let mut conflicts = Vec::new();

    for (path, sides) in paths {
        match merge_path(
            &path,
            &sides,
            input.base,
            input.left,
            input.right,
            input.workspace_content_key,
            plugins,
        )? {
            PathMerge::Entry { entry, bytes } => {
                if let Some(bytes) = bytes {
                    let content_id = entry
                        .content_id
                        .clone()
                        .ok_or(MergeError::MissingContentId)?;
                    prepared_content.insert(
                        content_id.clone(),
                        PreparedContent::memory(content_id, bytes),
                    );
                }
                merged_entries.push(entry);
            }
            PathMerge::Deleted => {}
            PathMerge::Conflict(conflict) => conflicts.push(conflict),
        }
    }

    if !conflicts.is_empty() {
        return Ok(MergeTreeOutcome::Conflicted(conflicts));
    }

    merged_entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(MergeTreeOutcome::Clean(MergedNamespace {
        entries: merged_entries,
        prepared_content,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Base,
    Local,
    Remote,
}

#[derive(Default)]
struct MergePath {
    base: Option<NamespaceEntry>,
    local: Option<NamespaceEntry>,
    remote: Option<NamespaceEntry>,
}

enum PathMerge {
    Entry {
        entry: NamespaceEntry,
        bytes: Option<Vec<u8>>,
    },
    Deleted,
    Conflict(ConflictRecord),
}

fn index_paths(
    paths: &mut BTreeMap<String, MergePath>,
    side: Side,
    snapshot: &dyn MergeContentReader,
) -> Result<(), MergeError> {
    snapshot.visit_entries(&mut |entry| {
        let slot = paths.entry(entry.path.clone()).or_default();
        match side {
            Side::Base => slot.base = Some(entry.clone()),
            Side::Local => slot.local = Some(entry.clone()),
            Side::Remote => slot.remote = Some(entry.clone()),
        }
    })?;
    Ok(())
}

fn merge_path(
    path: &str,
    sides: &MergePath,
    base: &dyn MergeContentReader,
    local: &dyn MergeContentReader,
    remote: &dyn MergeContentReader,
    workspace_content_key: [u8; 32],
    plugins: &MergePluginRegistry,
) -> Result<PathMerge, MergeError> {
    let git_path_class = classify_git_path(path);
    if git_path_class == Some(GitPathClass::DerivableVolatile) {
        return Ok(PathMerge::Deleted);
    }
    let base_entry = sides.base.as_ref();
    let local_entry = sides.local.as_ref();
    let remote_entry = sides.remote.as_ref();
    if optional_entries_match_for_merge(local_entry, remote_entry) {
        return matching_entry_from_sides(local_entry, local, remote_entry, remote, path);
    }
    if optional_entries_match_for_merge(base_entry, local_entry) {
        return selected_entry_from_snapshot(remote_entry, remote, path);
    }
    if optional_entries_match_for_merge(base_entry, remote_entry) {
        return selected_entry_from_snapshot(local_entry, local, path);
    }

    let Some(local_entry) = local_entry else {
        return Ok(PathMerge::Conflict(ConflictRecord::delete_edit(path)));
    };
    let Some(remote_entry) = remote_entry else {
        return Ok(PathMerge::Conflict(ConflictRecord::delete_edit(path)));
    };
    if local_entry.kind != remote_entry.kind || local_entry.kind != NamespaceEntryKind::File {
        return Ok(PathMerge::Conflict(ConflictRecord::path_conflict(path)));
    }
    if git_path_class.is_some() {
        return Ok(PathMerge::Conflict(ConflictRecord::opaque_git(path)));
    }
    if entries_match_except_executability(local_entry, remote_entry) {
        return entry_from_snapshot_with_executability(
            remote_entry,
            remote,
            path,
            resolve_merged_executability(base_entry, local_entry, remote_entry),
        );
    }
    if sides
        .base
        .as_ref()
        .is_some_and(|base_entry| entries_match_except_executability(base_entry, local_entry))
    {
        return entry_from_snapshot_with_executability(
            remote_entry,
            remote,
            path,
            resolve_merged_executability(base_entry, local_entry, remote_entry),
        );
    }
    if sides
        .base
        .as_ref()
        .is_some_and(|base_entry| entries_match_except_executability(base_entry, remote_entry))
    {
        return entry_from_snapshot_with_executability(
            local_entry,
            local,
            path,
            resolve_merged_executability(base_entry, local_entry, remote_entry),
        );
    }
    let base_bytes = base
        .read_file_for_path(path)
        .map_err(|source| MergeError::ReadPreparedContent {
            path: path.to_string(),
            source,
        })?
        .unwrap_or_default();
    let local_bytes = local
        .read_file_for_path(path)
        .map_err(|source| MergeError::ReadPreparedContent {
            path: path.to_string(),
            source,
        })?
        .ok_or(MergeError::MissingSideBytes {
            path: path.to_string(),
        })?;
    let remote_bytes = remote
        .read_file_for_path(path)
        .map_err(|source| MergeError::ReadPreparedContent {
            path: path.to_string(),
            source,
        })?
        .ok_or(MergeError::MissingSideBytes {
            path: path.to_string(),
        })?;
    if built_in_merge_plugin_conflicts_by_default(path) {
        return Ok(PathMerge::Conflict(ConflictRecord::structured(path)));
    }
    if is_env_path(path) {
        let merged = match merge_env_bytes(path, &base_bytes, &local_bytes, &remote_bytes) {
            EnvMergeOutcome::KeyConflict => {
                return Ok(PathMerge::Conflict(ConflictRecord::env_key(path)));
            }
            EnvMergeOutcome::Text(TextMergeOutcome::Clean { bytes, .. }) => bytes,
            EnvMergeOutcome::Text(outcome) => {
                return Ok(PathMerge::Conflict(ConflictRecord::env_text_merge(
                    path,
                    &text_merge_failure_code(&outcome),
                )));
            }
        };
        let entry = merged_file_entry(
            remote_entry,
            base_entry,
            local_entry,
            workspace_content_key,
            &merged,
        );
        return Ok(PathMerge::Entry {
            entry,
            bytes: Some(merged),
        });
    }
    match plugins.merge_external(path, &base_bytes, &local_bytes, &remote_bytes) {
        ExternalMergeDecision::NoMatch => {}
        ExternalMergeDecision::Merged(merged) => {
            // merge_external accepts Merged only after validating plugin output.
            let entry = merged_file_entry(
                remote_entry,
                base_entry,
                local_entry,
                workspace_content_key,
                &merged,
            );
            return Ok(PathMerge::Entry {
                entry,
                bytes: Some(merged),
            });
        }
        ExternalMergeDecision::Conflict(reason) => {
            return Ok(PathMerge::Conflict(ConflictRecord::merge_plugin(
                path, &reason,
            )));
        }
    }
    let merged = match merge_text_lines(&base_bytes, &local_bytes, &remote_bytes) {
        TextMergeOutcome::Clean { bytes, .. } => bytes,
        outcome @ TextMergeOutcome::NotText { .. } => {
            return Ok(PathMerge::Conflict(ConflictRecord::binary_text_merge(
                path,
                &text_merge_failure_code(&outcome),
            )));
        }
        outcome => {
            return Ok(PathMerge::Conflict(ConflictRecord::text_merge_span(
                path,
                &text_merge_failure_code(&outcome),
                conflict_span(path, &base_bytes, &local_bytes, &remote_bytes),
            )));
        }
    };
    if !structured_merge_output_is_valid(path, &merged) {
        return Ok(PathMerge::Conflict(ConflictRecord::structured(path)));
    };

    let entry = merged_file_entry(
        remote_entry,
        base_entry,
        local_entry,
        workspace_content_key,
        &merged,
    );
    Ok(PathMerge::Entry {
        entry,
        bytes: Some(merged),
    })
}

fn text_merge_failure_code(outcome: &TextMergeOutcome) -> String {
    match outcome {
        TextMergeOutcome::Clean { .. } => "clean".to_string(),
        TextMergeOutcome::Conflict { reason, .. } => reason.as_str().to_string(),
        TextMergeOutcome::NotText { reason } => reason.as_str().to_string(),
        TextMergeOutcome::ResourceLimit { phase, budget } => {
            format!("{}-{}", phase.as_str(), budget.as_str())
        }
        TextMergeOutcome::InternalError { reason } => reason.as_str().to_string(),
    }
}

fn merged_file_entry(
    remote_entry: &NamespaceEntry,
    base: Option<&NamespaceEntry>,
    local_entry: &NamespaceEntry,
    workspace_content_key: [u8; 32],
    merged: &[u8],
) -> NamespaceEntry {
    let mut entry = remote_entry.clone();
    entry.content_id = Some(workspace_content_id(workspace_content_key, merged));
    entry.content_layout = None;
    entry.byte_len = Some(merged.len() as u64);
    entry.executability = resolve_merged_executability(base, local_entry, remote_entry);
    entry
}

fn resolve_merged_executability(
    base: Option<&NamespaceEntry>,
    local: &NamespaceEntry,
    remote: &NamespaceEntry,
) -> FileExecutability {
    if local.executability == remote.executability {
        return local.executability;
    }
    let base = base.map(|entry| entry.executability).unwrap_or_default();
    if local.executability == base {
        remote.executability
    } else {
        local.executability
    }
}

fn matching_entry_from_sides(
    local_entry: Option<&NamespaceEntry>,
    local: &dyn MergeContentReader,
    remote_entry: Option<&NamespaceEntry>,
    remote: &dyn MergeContentReader,
    path: &str,
) -> Result<PathMerge, MergeError> {
    let Some(local_entry) = local_entry else {
        return Ok(PathMerge::Deleted);
    };
    let Some(remote_entry) = remote_entry else {
        return Ok(PathMerge::Deleted);
    };
    entry_from_snapshot(local_entry, local, path)
        .or_else(|_| entry_from_snapshot(remote_entry, remote, path))
}

fn selected_entry_from_snapshot(
    entry: Option<&NamespaceEntry>,
    snapshot: &dyn MergeContentReader,
    path: &str,
) -> Result<PathMerge, MergeError> {
    let Some(entry) = entry else {
        return Ok(PathMerge::Deleted);
    };
    entry_from_snapshot(entry, snapshot, path)
}

fn entry_from_snapshot(
    entry: &NamespaceEntry,
    snapshot: &dyn MergeContentReader,
    path: &str,
) -> Result<PathMerge, MergeError> {
    let bytes = selected_entry_bytes(entry, snapshot, path)?;
    Ok(PathMerge::Entry {
        entry: entry.clone(),
        bytes,
    })
}

fn entry_from_snapshot_with_executability(
    entry: &NamespaceEntry,
    snapshot: &dyn MergeContentReader,
    path: &str,
    executability: FileExecutability,
) -> Result<PathMerge, MergeError> {
    let mut entry = entry.clone();
    entry.executability = executability;
    let bytes = selected_entry_bytes(&entry, snapshot, path)?;
    Ok(PathMerge::Entry { entry, bytes })
}

fn selected_entry_bytes(
    entry: &NamespaceEntry,
    snapshot: &dyn MergeContentReader,
    path: &str,
) -> Result<Option<Vec<u8>>, MergeError> {
    if entry.kind != NamespaceEntryKind::File {
        return Ok(None);
    }
    if let Some(bytes) =
        snapshot
            .read_file_for_path(path)
            .map_err(|source| MergeError::ReadPreparedContent {
                path: path.to_string(),
                source,
            })?
    {
        return Ok(Some(bytes));
    }
    if entry.content_layout.is_some() {
        return Ok(None);
    }
    Err(MergeError::MissingSideBytes {
        path: path.to_string(),
    })
}

#[derive(Debug)]
pub enum MergeError {
    MissingContentId,
    MissingSideBytes {
        path: String,
    },
    ReadPreparedContent {
        path: String,
        source: std::io::Error,
    },
    NamespaceRead(NamespaceReadError),
    NamespaceBuild(bowline_core::namespace_snapshot::NamespaceBuildError),
}

impl fmt::Display for MergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingContentId => {
                formatter.write_str("merged file entry is missing content ID")
            }
            Self::MissingSideBytes { path } => {
                write!(formatter, "merge side bytes are missing for `{path}`")
            }
            Self::ReadPreparedContent { path, source } => {
                write!(
                    formatter,
                    "failed to read prepared merge side `{path}`: {source}"
                )
            }
            Self::NamespaceRead(source) => write!(formatter, "failed to read namespace: {source}"),
            Self::NamespaceBuild(source) => {
                write!(formatter, "failed to build merged namespace: {source}")
            }
        }
    }
}

impl Error for MergeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadPreparedContent { source, .. } => Some(source),
            Self::NamespaceRead(source) => Some(source),
            Self::NamespaceBuild(source) => Some(source),
            Self::MissingContentId | Self::MissingSideBytes { .. } => None,
        }
    }
}

impl From<NamespaceReadError> for MergeError {
    fn from(source: NamespaceReadError) -> Self {
        Self::NamespaceRead(source)
    }
}

impl From<bowline_core::namespace_snapshot::NamespaceBuildError> for MergeError {
    fn from(source: bowline_core::namespace_snapshot::NamespaceBuildError) -> Self {
        Self::NamespaceBuild(source)
    }
}

#[cfg(test)]
mod tests;
