use super::{
    PolicyDriftRecord, WorkCandidateUniverse, WorkViewError,
    accept_transaction::{AcceptRecovery, AcceptTransaction},
    paths::{
        clean_accept_explicit_include, clean_accept_policy, ensure_no_symlink_ancestors,
        ensure_path_inside, expand_display_path, is_clean_accept_policy_eligible,
        is_ignored_clean_accept_policy, is_owner_only_work_view_policy,
        is_source_control_metadata_path, main_project_root, work_namespace_root,
        workspace_path_for_project_file,
    },
    safe_materialization::SafeMaterializationRoot,
    writer_lock::ProjectWriterLock,
};
use crate::{
    metadata::MetadataStore,
    sync::{
        MergeContentReader, MergeTreeInput, MergeTreeOutcome, MergedNamespace, PreparedContent,
        merge_required_content_paths, merge_tree,
    },
    work_views::content_identity::{
        FileIdentity, clone_file_at_start, open_stable_regular_file, verify_stable_regular_file,
    },
};
use bowline_core::{
    ids::ContentId,
    workspace_graph::{
        FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind,
        normalize_workspace_path,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

mod completion;
mod materialization;
pub(crate) use completion::finalize_snapshot_accept_under_claim;
pub(crate) use materialization::tree_fence;
use materialization::{apply_merged, copy_tree, executability, project_relative};

#[cfg(test)]
#[derive(Debug)]
pub(super) enum SnapshotAcceptOutcome {
    Clean,
    Conflicted(Vec<String>),
    PolicyDrift(Vec<PolicyDriftRecord>),
}
pub(crate) enum SnapshotAcceptPrepareOutcome {
    Prepared(Box<PreparedSnapshotAccept>),
    AlreadyPublished(PublishedSnapshotAccept),
    Conflicted(Vec<String>),
    PolicyDrift(Vec<PolicyDriftRecord>),
    HydrationRequired(BTreeSet<String>),
}
pub(crate) struct PreparedSnapshotAccept {
    transaction: AcceptTransaction,
    main_root: std::path::PathBuf,
    main_fence: String,
    merged: MergedNamespace,
    branch_paths: BTreeSet<String>,
    work_view: bowline_core::work_views::WorkView,
}
pub(crate) struct PublishedSnapshotAccept {
    transaction: AcceptTransaction,
}
impl PublishedSnapshotAccept {
    pub(crate) fn complete(self) -> Result<(), WorkViewError> {
        self.transaction.complete()?;
        Ok(())
    }
}

impl PreparedSnapshotAccept {
    pub(crate) fn merged_entries(&self) -> &[NamespaceEntry] {
        &self.merged.entries
    }

    pub(crate) fn prepared_content(&self) -> &BTreeMap<ContentId, PreparedContent> {
        &self.merged.prepared_content
    }

    pub(crate) fn branch_paths(&self) -> &BTreeSet<String> {
        &self.branch_paths
    }

    pub(crate) fn work_view(&self) -> &bowline_core::work_views::WorkView {
        &self.work_view
    }

    pub(crate) fn check_main_fence(&self) -> io::Result<()> {
        if tree_fence(&self.main_root)? == self.main_fence {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "main changed while work-view accept was staged",
            ))
        }
    }

    pub(crate) fn main_fence_unchanged(&self) -> Result<bool, WorkViewError> {
        Ok(tree_fence(&self.main_root)? == self.main_fence)
    }

    pub(crate) fn publish(
        self,
        authorize: impl FnOnce() -> io::Result<()>,
    ) -> Result<PublishedSnapshotAccept, WorkViewError> {
        self.transaction.publish(|| {
            authorize()?;
            self.check_main_fence()
        })?;
        Ok(PublishedSnapshotAccept {
            transaction: self.transaction,
        })
    }
}

#[cfg(test)]
pub(super) fn accept_snapshot(
    store: &MetadataStore,
    work_view: &bowline_core::work_views::WorkView,
    selected: &BTreeSet<String>,
    cache_root: Option<&Path>,
) -> Result<SnapshotAcceptOutcome, WorkViewError> {
    let key = [0_u8; 32];
    match prepare_snapshot_accept(store, work_view, selected, cache_root, key, None)? {
        SnapshotAcceptPrepareOutcome::AlreadyPublished(published) => {
            published.complete()?;
            Ok(SnapshotAcceptOutcome::Clean)
        }
        SnapshotAcceptPrepareOutcome::Conflicted(conflicts) => {
            Ok(SnapshotAcceptOutcome::Conflicted(conflicts))
        }
        SnapshotAcceptPrepareOutcome::PolicyDrift(drift) => {
            Ok(SnapshotAcceptOutcome::PolicyDrift(drift))
        }
        SnapshotAcceptPrepareOutcome::HydrationRequired(_) => {
            Err(WorkViewError::SnapshotMaterialization {
                snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                reason: "test accept unexpectedly required remote hydration".to_string(),
            })
        }
        SnapshotAcceptPrepareOutcome::Prepared(prepared) => {
            prepared.publish(|| Ok(()))?.complete()?;
            Ok(SnapshotAcceptOutcome::Clean)
        }
    }
}

pub(crate) fn prepare_snapshot_accept(
    store: &MetadataStore,
    work_view: &bowline_core::work_views::WorkView,
    selected: &BTreeSet<String>,
    cache_root: Option<&Path>,
    workspace_content_key: [u8; 32],
    canonical_current: Option<&crate::sync::SnapshotContent>,
) -> Result<SnapshotAcceptPrepareOutcome, WorkViewError> {
    let main = main_project_root(store, work_view)?.ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let work = expand_display_path(&work_view.visible_path);
    let namespace =
        work_namespace_root(store, work_view)?.ok_or(WorkViewError::MissingWorkspaceRoot)?;
    validate_roots(store, &work, &namespace)?;
    let _lock = ProjectWriterLock::acquire(
        &namespace,
        &work_view.workspace_id,
        &work_view.project_id,
        &work_view.project_path,
    )?;
    let transaction = AcceptTransaction::open(&namespace, &main, work_view.id.as_str())?;
    if transaction.recover()? == AcceptRecovery::Published {
        return Ok(SnapshotAcceptPrepareOutcome::AlreadyPublished(
            PublishedSnapshotAccept { transaction },
        ));
    }
    let descriptor = store
        .work_view_exposed_base(&work_view.workspace_id, &work_view.id)?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "authoritative exposed base is missing".to_string(),
        })?;
    let exposed_snapshot = super::namespace::load_exposed_snapshot(store, &descriptor)?;
    let base = Reader::from_exposed_snapshot(
        &exposed_snapshot,
        &descriptor.project_prefix,
        selected,
        cache_root,
    )?;
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    let universe = WorkCandidateUniverse::new(base.entries.iter().cloned());
    let (mut work_reader, mut policy_drift) = Reader::from_tree(
        store,
        work_view,
        &workspace_root,
        &work,
        selected,
        &universe,
        workspace_content_key,
    )?;
    policy_drift.extend(policy_drift_for_exposed(PolicyDriftInput {
        store,
        view: work_view,
        workspace: &workspace_root,
        work_root: &work,
        main_root: &main,
        base: &base,
        work: &work_reader,
        universe: &universe,
    })?);
    if !policy_drift.is_empty() {
        return Ok(SnapshotAcceptPrepareOutcome::PolicyDrift(policy_drift));
    }
    work_reader.align_exposed_directories(&base);
    let current_paths = base
        .entries
        .iter()
        .chain(&work_reader.entries)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let mut current = match canonical_current {
        Some(snapshot) => Reader::from_snapshot(snapshot, &work_view.project_path, &current_paths)?,
        None => Reader::from_paths(
            store,
            work_view,
            &workspace_root,
            &main,
            &base,
            &work_reader,
            workspace_content_key,
        )?,
    };
    current.align_exposed_directories(&base);
    let branch_paths = base
        .entries
        .iter()
        .chain(&work_reader.entries)
        .chain(&current.entries)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let fence = tree_fence(&main)?;
    let merge_input = MergeTreeInput {
        base: &base,
        left: &work_reader,
        right: &current,
        workspace_content_key,
    };
    if canonical_current.is_some() {
        let required = merge_required_content_paths(&merge_input).map_err(|error| {
            WorkViewError::SnapshotMaterialization {
                snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                reason: format!("three-way merge planning failed: {error}"),
            }
        })?;
        let missing = required
            .into_iter()
            .filter(|relative| {
                let Some(snapshot_path) = current.snapshot_paths.get(relative) else {
                    return false;
                };
                canonical_current
                    .and_then(|snapshot| snapshot.prepared_content_for_path(snapshot_path).ok())
                    .flatten()
                    .is_none()
            })
            .map(|relative| {
                if descriptor.project_prefix.is_empty() {
                    relative
                } else {
                    format!(
                        "{}/{relative}",
                        descriptor.project_prefix.trim_end_matches('/')
                    )
                }
            })
            .collect::<BTreeSet<_>>();
        if !missing.is_empty() {
            return Ok(SnapshotAcceptPrepareOutcome::HydrationRequired(missing));
        }
    }
    let merged =
        match merge_tree(merge_input).map_err(|error| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: format!("three-way merge failed: {error}"),
        })? {
            MergeTreeOutcome::Clean(merged) => merged,
            MergeTreeOutcome::Conflicted(conflicts) => {
                return Ok(SnapshotAcceptPrepareOutcome::Conflicted(
                    conflicts
                        .into_iter()
                        .flat_map(|record| record.paths)
                        .collect(),
                ));
            }
        };
    transaction.stage(|staged| {
        copy_tree(&main, staged)?;
        apply_merged(staged, &base, &work_reader, &current, &merged)
    })?;
    Ok(SnapshotAcceptPrepareOutcome::Prepared(Box::new(
        PreparedSnapshotAccept {
            transaction,
            main_root: main,
            main_fence: fence,
            merged,
            branch_paths,
            work_view: work_view.clone(),
        },
    )))
}

struct PolicyDriftInput<'a> {
    store: &'a MetadataStore,
    view: &'a bowline_core::work_views::WorkView,
    workspace: &'a Path,
    work_root: &'a Path,
    main_root: &'a Path,
    base: &'a Reader,
    work: &'a Reader,
    universe: &'a WorkCandidateUniverse,
}

fn policy_drift_for_exposed(
    input: PolicyDriftInput<'_>,
) -> Result<Vec<PolicyDriftRecord>, WorkViewError> {
    let work_by_path = input
        .work
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut drift = Vec::new();
    for base_entry in &input.base.entries {
        let changed = work_by_path
            .get(base_entry.path.as_str())
            .is_none_or(|entry| entry.content_id != base_entry.content_id);
        if !changed {
            continue;
        }
        let workspace_path =
            workspace_path_for_project_file(input.view, Path::new(&base_entry.path));
        let work_path = input.work_root.join(&base_entry.path);
        let main_path = input.main_root.join(&base_entry.path);
        let source = if work_path.exists() {
            Some(work_path.as_path())
        } else if main_path.exists() {
            Some(main_path.as_path())
        } else {
            None
        };
        let policy = clean_accept_policy(
            input.store,
            input.workspace,
            &input.view.workspace_id,
            &workspace_path,
            source,
        )?;
        if let Some(reason) = input.universe.classify_drift(
            &base_entry.path,
            policy.classification,
            policy.mode,
            &policy.access,
            clean_accept_explicit_include(input.workspace, &workspace_path)?,
        ) {
            drift.push(PolicyDriftRecord {
                path: base_entry.path.clone(),
                reason,
            });
        }
    }
    Ok(drift)
}

fn validate_roots(
    store: &MetadataStore,
    work: &Path,
    namespace: &Path,
) -> Result<(), WorkViewError> {
    ensure_path_inside(work, namespace, "work view must live under .work")?;
    let workspace = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    ensure_no_symlink_ancestors(namespace, &workspace, "work view namespace escapes .work")?;
    ensure_no_symlink_ancestors(work, namespace, "work view root escapes .work")?;
    Ok(())
}

#[derive(Debug)]
struct Reader {
    entries: Vec<NamespaceEntry>,
    sources: BTreeMap<String, ReaderSource>,
    snapshot: Option<crate::sync::SnapshotContent>,
    snapshot_paths: BTreeMap<String, String>,
    cache_root: Option<std::path::PathBuf>,
}

#[derive(Debug)]
enum ReaderSource {
    File(fs::File),
}

impl MergeContentReader for Reader {
    fn visit_entries(
        &self,
        visitor: &mut dyn FnMut(&NamespaceEntry),
    ) -> Result<(), bowline_core::namespace_snapshot::NamespaceReadError> {
        for entry in &self.entries {
            visitor(entry);
        }
        Ok(())
    }
    fn read_file_for_path(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        match self.sources.get(path) {
            Some(ReaderSource::File(file)) => {
                let mut file = clone_file_at_start(file)?;
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut bytes)?;
                Ok(Some(bytes))
            }
            None => {
                if let (Some(snapshot), Some(snapshot_path)) =
                    (self.snapshot.as_ref(), self.snapshot_paths.get(path))
                {
                    if let Some(bytes) = snapshot.read_file_for_path(snapshot_path)? {
                        return Ok(Some(bytes));
                    }
                    if let Some(cache_root) = &self.cache_root
                        && let Some(entry) = snapshot
                            .entry_for_path(snapshot_path)
                            .map_err(io::Error::other)?
                        && let Some(content_id) = entry.content_id
                    {
                        return bowline_storage::LocalContentCache::open(cache_root)
                            .map_err(io::Error::other)?
                            .get_previously_verified_content(&content_id)
                            .map(Some)
                            .map_err(io::Error::other);
                    }
                }
                Ok(None)
            }
        }
    }
}

impl Reader {
    fn align_exposed_directories(&mut self, base: &Self) {
        let base_directories = base
            .entries
            .iter()
            .filter(|entry| entry.kind == NamespaceEntryKind::Directory)
            .map(|entry| (entry.path.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        for entry in &mut self.entries {
            if entry.kind == NamespaceEntryKind::Directory
                && let Some(base_entry) = base_directories.get(entry.path.as_str())
            {
                // Directories are structural merge nodes; policy drift is checked separately.
                *entry = (*base_entry).clone();
            }
        }
    }

    fn from_snapshot(
        snapshot: &crate::sync::SnapshotContent,
        prefix: &str,
        paths: &BTreeSet<String>,
    ) -> Result<Self, WorkViewError> {
        let prefix = normalize_workspace_path(prefix)
            .trim_matches('/')
            .to_string();
        let mut entries = Vec::new();
        let mut snapshot_paths = BTreeMap::new();
        for relative in paths {
            let snapshot_path = if prefix.is_empty() {
                relative.clone()
            } else {
                format!("{prefix}/{relative}")
            };
            let Some(entry) = super::namespace::get_entry(snapshot, &snapshot_path)? else {
                continue;
            };
            if !matches!(
                entry.kind,
                NamespaceEntryKind::File | NamespaceEntryKind::Directory
            ) {
                return Err(WorkViewError::SnapshotMaterialization {
                    snapshot_id: snapshot.manifest().snapshot_id.as_str().to_string(),
                    reason: format!("unsupported canonical entry kind for `{relative}`"),
                });
            }
            let mut relative_entry = entry.clone();
            relative_entry.path = relative.clone();
            if entry.kind == NamespaceEntryKind::File {
                snapshot_paths.insert(relative.clone(), entry.path);
            }
            entries.push(relative_entry);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Self {
            entries,
            sources: BTreeMap::new(),
            snapshot: Some(snapshot.clone()),
            snapshot_paths,
            cache_root: None,
        })
    }
    fn from_exposed_snapshot(
        snapshot: &crate::sync::SnapshotContent,
        prefix: &str,
        selected: &BTreeSet<String>,
        cache_root: Option<&Path>,
    ) -> Result<Self, WorkViewError> {
        let prefix = normalize_workspace_path(prefix)
            .trim_matches('/')
            .to_string();
        let candidates = super::namespace::collect_prefix(
            snapshot,
            &bowline_core::workspace_graph::WorkspaceRelativePath::new(&prefix),
        )?;
        let mut entries = Vec::new();
        let mut snapshot_paths = BTreeMap::new();
        for entry in candidates {
            let relative = project_relative(&entry.path, &prefix)
                .map(str::to_string)
                .ok_or_else(|| WorkViewError::SnapshotMaterialization {
                    snapshot_id: "exposed-base".to_string(),
                    reason: format!("exposed path `{}` is outside project", entry.path),
                })?;
            if !path_is_selected(&relative, entry.kind, selected) {
                continue;
            }
            if !matches!(
                entry.kind,
                NamespaceEntryKind::File | NamespaceEntryKind::Directory
            ) {
                return Err(WorkViewError::SnapshotMaterialization {
                    snapshot_id: "exposed-base".to_string(),
                    reason: format!("unsupported exposed entry kind for `{relative}`"),
                });
            }
            let snapshot_path = entry.path.clone();
            let mut relative_entry = entry;
            relative_entry.path = relative.clone();
            if relative_entry.kind == NamespaceEntryKind::File {
                snapshot_paths.insert(relative, snapshot_path);
            }
            entries.push(relative_entry);
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            entries,
            sources: BTreeMap::new(),
            snapshot: Some(snapshot.clone()),
            snapshot_paths,
            cache_root: cache_root.map(Path::to_path_buf),
        })
    }
    fn from_tree(
        store: &MetadataStore,
        view: &bowline_core::work_views::WorkView,
        workspace: &Path,
        root: &Path,
        selected: &BTreeSet<String>,
        universe: &WorkCandidateUniverse,
        workspace_content_key: [u8; 32],
    ) -> Result<(Self, Vec<PolicyDriftRecord>), WorkViewError> {
        let mut reader = Self {
            entries: Vec::new(),
            sources: BTreeMap::new(),
            snapshot: None,
            snapshot_paths: BTreeMap::new(),
            cache_root: None,
        };
        let mut drift = Vec::new();
        for observed in tree_paths(root)? {
            let file = observed.path;
            let relative = normalize_workspace_path(
                &file
                    .strip_prefix(root)
                    .map_err(io::Error::other)?
                    .display()
                    .to_string(),
            );
            let kind = if observed.metadata.is_dir() {
                NamespaceEntryKind::Directory
            } else {
                NamespaceEntryKind::File
            };
            if !path_is_selected(&relative, kind, selected)
                || is_source_control_metadata_path(Path::new(&relative))
            {
                continue;
            }
            let workspace_path = workspace_path_for_project_file(view, Path::new(&relative));
            let policy = clean_accept_policy(
                store,
                workspace,
                &view.workspace_id,
                &workspace_path,
                Some(&file),
            )?;
            if !universe.contains(&relative)
                && let Some(reason) = universe.classify_new_path(
                    policy.classification,
                    policy.mode,
                    &policy.access,
                    clean_accept_explicit_include(workspace, &workspace_path)?,
                )
            {
                drift.push(PolicyDriftRecord {
                    path: relative,
                    reason,
                });
                continue;
            }
            if is_ignored_clean_accept_policy(policy.classification, policy.mode)
                || !is_clean_accept_policy_eligible(policy.classification, policy.mode)
            {
                continue;
            }
            if kind == NamespaceEntryKind::Directory {
                reader.entries.push(directory_entry(&relative, policy));
                continue;
            }
            let metadata = observed.metadata;
            let expected = FileIdentity::from_metadata(&metadata)?;
            let (identity, mut source) = open_stable_regular_file(&file, Some(expected))?;
            let content_id = bowline_core::workspace_graph::workspace_content_id_reader(
                workspace_content_key,
                &mut source,
            )?;
            verify_stable_regular_file(&file, &source, identity)?;
            reader.entries.push(file_entry(
                &relative,
                content_id,
                metadata.len(),
                &metadata,
                policy,
            ));
            reader.sources.insert(relative, ReaderSource::File(source));
        }
        reader.entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((reader, drift))
    }

    fn from_paths(
        store: &MetadataStore,
        view: &bowline_core::work_views::WorkView,
        workspace: &Path,
        root: &Path,
        base: &Reader,
        work: &Reader,
        workspace_content_key: [u8; 32],
    ) -> Result<Self, WorkViewError> {
        let paths = base
            .entries
            .iter()
            .chain(&work.entries)
            .map(|entry| entry.path.clone())
            .collect::<BTreeSet<_>>();
        let mut reader = Self {
            entries: Vec::new(),
            sources: BTreeMap::new(),
            snapshot: None,
            snapshot_paths: BTreeMap::new(),
            cache_root: None,
        };
        for relative in paths {
            SafeMaterializationRoot::new(root)?
                .reject_main_symlink_ancestors(Path::new(&relative))?;
            let file = root.join(&relative);
            let metadata = match fs::symlink_metadata(&file) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(WorkViewError::UnsafeWorkViewPath {
                        path: file.display().to_string(),
                        reason: "symlinks are not followed into work-view snapshots",
                    });
                }
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !metadata.is_file() && !metadata.is_dir() {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: file.display().to_string(),
                    reason: "unsupported filesystem entry in work-view accept",
                });
            }
            if metadata.is_dir() {
                let workspace_path = workspace_path_for_project_file(view, Path::new(&relative));
                let policy = clean_accept_policy(
                    store,
                    workspace,
                    &view.workspace_id,
                    &workspace_path,
                    Some(&file),
                )?;
                reader.entries.push(directory_entry(&relative, policy));
                continue;
            }
            let workspace_path = workspace_path_for_project_file(view, Path::new(&relative));
            let policy = clean_accept_policy(
                store,
                workspace,
                &view.workspace_id,
                &workspace_path,
                Some(&file),
            )?;
            let expected = FileIdentity::from_metadata(&metadata)?;
            let (identity, mut source) = open_stable_regular_file(&file, Some(expected))?;
            let content_id = bowline_core::workspace_graph::workspace_content_id_reader(
                workspace_content_key,
                &mut source,
            )?;
            verify_stable_regular_file(&file, &source, identity)?;
            reader.entries.push(file_entry(
                &relative,
                content_id,
                metadata.len(),
                &metadata,
                policy,
            ));
            reader.sources.insert(relative, ReaderSource::File(source));
        }
        reader.entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(reader)
    }
}

fn file_entry(
    path: &str,
    content_id: ContentId,
    byte_len: u64,
    metadata: &fs::Metadata,
    policy: crate::policy::PathPolicyDecision,
) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: policy.classification,
        mode: policy.mode,
        access: policy.access,
        content_id: Some(content_id),
        content_layout: None,
        symlink_target: None,
        byte_len: Some(byte_len),
        executability: executability(metadata),
        hydration_state: HydrationState::Local,
    }
}

fn directory_entry(path: &str, policy: crate::policy::PathPolicyDecision) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::Directory,
        classification: policy.classification,
        mode: policy.mode,
        access: policy.access,
        content_id: None,
        content_layout: None,
        symlink_target: None,
        byte_len: None,
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}

struct ObservedTreePath {
    path: std::path::PathBuf,
    metadata: fs::Metadata,
}

fn tree_paths(root: &Path) -> Result<Vec<ObservedTreePath>, WorkViewError> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut children = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(fs::DirEntry::file_name);
        for child in children {
            if paths.len() >= 1_000_000 {
                return Err(WorkViewError::SnapshotMaterialization {
                    snapshot_id: "work-candidate".to_string(),
                    reason: "work-view candidate exceeds the one-million-entry budget".to_string(),
                });
            }
            let path = child.path();
            let relative = path.strip_prefix(root).map_err(io::Error::other)?;
            if relative.components().count() > 4_096 {
                return Err(WorkViewError::SnapshotMaterialization {
                    snapshot_id: "work-candidate".to_string(),
                    reason: "work-view candidate exceeds the path-depth budget".to_string(),
                });
            }
            if is_source_control_metadata_path(relative) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: path.display().to_string(),
                    reason: "symlinks are not followed into work-view snapshots",
                });
            }
            if !metadata.is_file() && !metadata.is_dir() {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: path.display().to_string(),
                    reason: "unsupported filesystem entry in work-view accept",
                });
            }
            let is_dir = metadata.is_dir();
            paths.push(ObservedTreePath {
                path: path.clone(),
                metadata,
            });
            if is_dir {
                pending.push(path);
            }
        }
    }
    paths.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(paths)
}

fn path_is_selected(path: &str, kind: NamespaceEntryKind, selected: &BTreeSet<String>) -> bool {
    selected.is_empty()
        || selected.contains(path)
        || (kind == NamespaceEntryKind::Directory
            && selected.iter().any(|selected_path| {
                selected_path
                    .strip_prefix(path)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            }))
}

#[cfg(test)]
mod tests;
