use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use bowline_control_plane::WorkspaceRef as RemoteWorkspaceRef;
use bowline_core::{
    git_paths::is_git_derivable_volatile_path,
    git_worktree_link::{
        WorktreeLinkFile, is_out_of_root_admin_target, is_worktree_admin_pointer,
        worktree_link_file, worktree_registration_prefix,
    },
    ids::{ContentId, DeviceId, WorkspaceId},
    namespace_snapshot::{
        NamespaceBuildError, NamespaceCancellation, NamespaceMutation, NamespaceOperationContext,
        NamespaceReadError, NamespaceSnapshotBuilder,
    },
    policy::MaterializationMode,
    workspace_graph::{
        ContentLayout, HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind,
        SNAPSHOT_SCHEMA_VERSION, SnapshotKind, WorkspaceRef, is_safe_workspace_symlink_target,
        normalize_workspace_path,
    },
};

use crate::metadata::PreparationOwnerMarker;
use crate::scanner::{
    ScanError, ScanReport, merge_scoped_and_shallow_reports,
    scan_workspace_root_shallow_with_checkpoint, scan_workspace_scoped_with_checkpoint,
    scan_workspace_with_checkpoint,
};

pub(crate) use super::prepared_content::{
    PrepareSnapshotPathRequest, PrepareSnapshotReaderRequest, prepare_snapshot_path,
    prepare_snapshot_reader,
};

use super::{
    CandidateBase, FullScanReason, PreparedContent, ScanScope, SnapshotContent,
    change_index::LocalChangeIndex,
    manifest_id_for_snapshot,
    manifest_identity::ManifestIdentityReport,
    namespace::PageNamespaceBuilder,
    prepared_content::{
        PrepareContentRequest, next_preparation_owner_marker, prepare_content,
        retain_one_prepared_source,
    },
    stat_cache::{
        CacheDecision, ScanStats, StatCacheDeleteScope, StatCacheDivergence, StatCacheSession,
        StatCacheWriteBack, VerifyDecision, verify_shard_for_path, verify_shard_for_timestamp,
    },
};

mod namespace_builder;
use namespace_builder::{
    coalescer_namespace_budget, namespace_builder_for_scan, remove_owned_prior_scope,
};

const DEFAULT_SCHEMA_VERSION: u16 = SNAPSHOT_SCHEMA_VERSION;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCandidate {
    pub base: CandidateBase,
    pub device_id: DeviceId,
    pub manifest_id: bowline_core::ids::ManifestId,
    pub snapshot: SnapshotContent,
    pub scan_report: ScanReport,
    pub scan_scope: ScanScope,
    pub stat_cache_hit_paths: BTreeSet<String>,
    pub stat_cache_divergences: Vec<StatCacheDivergence>,
    pub scan_stats: ScanStats,
    pub manifest_identity: ManifestIdentityReport,
    pub stat_cache_write_back: Option<StatCacheWriteBack>,
    pub causation_ids: Vec<String>,
    pub skipped_unsafe_symlinks: BTreeSet<String>,
    pub created_at: String,
}

#[derive(Clone, Copy)]
pub struct CoalesceContext<'a> {
    pub paths: &'a BTreeSet<String>,
    pub prior_snapshot: Option<&'a SnapshotContent>,
    pub namespace_cancellation: Option<&'a dyn NamespaceCancellation>,
    pub preserved_entries: &'a [NamespaceEntry],
    pub file_overrides: &'a BTreeMap<String, Vec<u8>>,
    pub base_locators: &'a BTreeMap<ContentId, ContentLayout>,
    pub preparation_root: Option<&'a Path>,
}

impl<'a> CoalesceContext<'a> {
    pub(crate) fn empty() -> Self {
        Self {
            paths: &EMPTY_PATH_SET,
            prior_snapshot: None,
            namespace_cancellation: None,
            preserved_entries: &[],
            file_overrides: &EMPTY_FILE_OVERRIDES,
            base_locators: &EMPTY_BASE_LOCATORS,
            preparation_root: None,
        }
    }
}

static EMPTY_PATH_SET: BTreeSet<String> = BTreeSet::new();
static EMPTY_FILE_OVERRIDES: BTreeMap<String, Vec<u8>> = BTreeMap::new();
static EMPTY_BASE_LOCATORS: BTreeMap<ContentId, ContentLayout> = BTreeMap::new();

pub fn coalesce_workspace_scan(
    root: &Path,
    workspace_id: WorkspaceId,
    base_ref: &RemoteWorkspaceRef,
    device_id: DeviceId,
    workspace_content_key: [u8; 32],
    created_at: impl Into<String>,
) -> Result<SnapshotCandidate, CoalesceError> {
    coalesce_workspace_scan_excluding(
        root,
        workspace_id,
        base_ref,
        device_id,
        workspace_content_key,
        created_at,
        CoalesceContext::empty(),
    )
}

pub fn coalesce_workspace_scan_excluding(
    root: &Path,
    workspace_id: WorkspaceId,
    base_ref: &RemoteWorkspaceRef,
    device_id: DeviceId,
    workspace_content_key: [u8; 32],
    created_at: impl Into<String>,
    context: CoalesceContext<'_>,
) -> Result<SnapshotCandidate, CoalesceError> {
    coalesce_workspace_scan_cached(CoalesceScanRequest {
        root,
        workspace_id,
        base_ref,
        device_id,
        workspace_content_key,
        created_at: created_at.into(),
        context,
        stat_cache: None,
        scan_scope: ScanScope::default(),
    })
}

pub struct CoalesceScanRequest<'a> {
    pub root: &'a Path,
    pub workspace_id: WorkspaceId,
    pub base_ref: &'a RemoteWorkspaceRef,
    pub device_id: DeviceId,
    pub workspace_content_key: [u8; 32],
    pub created_at: String,
    pub context: CoalesceContext<'a>,
    pub stat_cache: Option<&'a mut StatCacheSession>,
    pub scan_scope: ScanScope,
}

pub fn coalesce_workspace_scan_cached(
    request: CoalesceScanRequest<'_>,
) -> Result<SnapshotCandidate, CoalesceError> {
    coalesce_workspace_scan_cached_with_checkpoint(request, || Ok(()))
}

pub fn coalesce_workspace_scan_cached_with_checkpoint(
    request: CoalesceScanRequest<'_>,
    mut checkpoint: impl FnMut() -> Result<(), ScanError>,
) -> Result<SnapshotCandidate, CoalesceError> {
    let CoalesceScanRequest {
        root,
        workspace_id,
        base_ref,
        device_id,
        workspace_content_key,
        created_at,
        context,
        stat_cache,
        scan_scope,
    } = request;
    let report = match &scan_scope {
        ScanScope::Full(_) => scan_workspace_with_checkpoint(root, &mut checkpoint)?,
        // Combined tick: the scoped subtree pass and the root-shallow pass each
        // scan their own frontier, then merge by explicit ownership (KTD-15).
        ScanScope::DirtySubtrees {
            roots,
            root_shallow: true,
        } => merge_scoped_and_shallow_reports(
            scan_workspace_scoped_with_checkpoint(root, roots, &mut checkpoint)?,
            scan_workspace_root_shallow_with_checkpoint(root, &mut checkpoint)?,
            roots,
        ),
        ScanScope::DirtySubtrees {
            roots,
            root_shallow: false,
        } => scan_workspace_scoped_with_checkpoint(root, roots, &mut checkpoint)?,
        ScanScope::RootShallow => {
            scan_workspace_root_shallow_with_checkpoint(root, &mut checkpoint)?
        }
    };
    let verify_shard = if matches!(scan_scope, ScanScope::Full(FullScanReason::VerifyDue)) {
        Some(verify_shard_for_timestamp(&created_at))
    } else {
        None
    };
    coalesce_workspace_report_with_cache(
        CoalesceWorkspaceReportRequest {
            root,
            report,
            workspace_id,
            base_ref,
            device_id,
            workspace_content_key,
            created_at,
            context,
        },
        stat_cache,
        LocalChangeIndex::delete_scope_for(&scan_scope),
        verify_shard,
        scan_scope.clone(),
    )
}

struct CoalesceWorkspaceReportRequest<'a> {
    root: &'a Path,
    report: ScanReport,
    workspace_id: WorkspaceId,
    base_ref: &'a RemoteWorkspaceRef,
    device_id: DeviceId,
    workspace_content_key: [u8; 32],
    created_at: String,
    context: CoalesceContext<'a>,
}

#[cfg(test)]
fn coalesce_workspace_report(
    request: CoalesceWorkspaceReportRequest<'_>,
) -> Result<SnapshotCandidate, CoalesceError> {
    coalesce_workspace_report_with_cache(
        request,
        None,
        StatCacheDeleteScope::All,
        None,
        ScanScope::Full(FullScanReason::CliRequested),
    )
}

fn coalesce_workspace_report_with_cache(
    request: CoalesceWorkspaceReportRequest<'_>,
    mut stat_cache: Option<&mut StatCacheSession>,
    delete_scope: StatCacheDeleteScope<'_>,
    verify_shard: Option<u64>,
    scan_scope: ScanScope,
) -> Result<SnapshotCandidate, CoalesceError> {
    let mut files = BTreeMap::<ContentId, PreparedContent>::new();
    let mut stat_cache_hit_paths = BTreeSet::<String>::new();
    let mut stat_cache_divergences = Vec::<StatCacheDivergence>::new();
    let created_at = request.created_at.clone();
    let mut skipped_unsafe_symlinks = BTreeSet::<String>::new();
    let preparation_owner_marker = preparation_owner_marker_for(&request, &created_at);
    let local_worktree_prefixes = out_of_root_worktree_prefixes(&request)?;
    let mut scan_report = request.report;
    scan_report
        .paths
        .sort_by(|left, right| left.path.cmp(&right.path));
    let namespace_budget = coalescer_namespace_budget(
        request.context.prior_snapshot,
        scan_report.paths.len() as u64,
        request.context.preserved_entries.len() as u64,
    );
    let mut namespace_operation = match request.context.namespace_cancellation {
        Some(cancellation) => NamespaceOperationContext::new(namespace_budget, cancellation),
        None => NamespaceOperationContext::uncancelled(namespace_budget),
    };
    let mut namespace_builder = namespace_builder_for_scan(
        &request.workspace_id,
        request.context.prior_snapshot,
        &scan_scope,
        request.workspace_content_key,
        &mut namespace_operation,
    )?;
    remove_owned_prior_scope(
        &mut namespace_builder,
        request.context.prior_snapshot,
        &scan_scope,
        &mut namespace_operation,
    )?;
    for observed in scan_report.path_observations() {
        let path = normalize_workspace_path(&observed.path);
        if path_is_under_any_prefix(&path, &local_worktree_prefixes) {
            continue;
        }
        let portable_git_worktree_link =
            portable_git_worktree_link_file(&path, observed.is_dir, observed.is_symlink);
        if !syncs_to_workspace_head(observed.policy.mode) {
            continue;
        }
        if is_private_state_path(&path) {
            continue;
        }
        if is_git_derivable_volatile_path(&path) && portable_git_worktree_link.is_none() {
            continue;
        }
        let override_bytes = request.context.file_overrides.get(&path);
        if request.context.paths.contains(&path) && override_bytes.is_none() {
            continue;
        }
        let (kind, content_id, prepared, locator, byte_len, hydration_state, symlink_target) =
            if observed.is_dir {
                (
                    NamespaceEntryKind::Directory,
                    None,
                    None,
                    None,
                    None,
                    HydrationState::StructureOnly,
                    None,
                )
            } else if observed.is_symlink {
                let target = match read_workspace_symlink_target(&request.root.join(&path)) {
                    Ok(target) => target,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(CoalesceError::ReadPath {
                            path: path.clone(),
                            source,
                        });
                    }
                };
                if should_skip_unsafe_symlink(&path, &target, &mut skipped_unsafe_symlinks) {
                    continue;
                }
                (
                    NamespaceEntryKind::Symlink,
                    None,
                    None,
                    None,
                    Some(target.len() as u64),
                    HydrationState::Local,
                    Some(target),
                )
            } else {
                let absolute_path = request.root.join(&path);
                if override_bytes.is_some()
                    && let Some(session) = stat_cache.as_deref_mut()
                {
                    session.record_conflict_override();
                }
                let verify_this_path =
                    verify_shard.is_some_and(|shard| verify_shard_for_path(&path) == shard);
                let verify_cached_content_id = if override_bytes.is_none() && verify_this_path {
                    match (stat_cache.as_deref_mut(), observed.stat.as_ref()) {
                        (Some(session), Some(stat)) => match session.decide_verify(&path, stat) {
                            VerifyDecision::Compare { cached_content_id } => {
                                Some(cached_content_id)
                            }
                            VerifyDecision::Rehash(_) => None,
                        },
                        _ => None,
                    }
                } else {
                    None
                };
                let cache_hit = if override_bytes.is_none() && !verify_this_path {
                    match (stat_cache.as_deref_mut(), observed.stat.as_ref()) {
                        (Some(session), Some(stat)) => match session.decide(&path, stat) {
                            CacheDecision::Hit {
                                content_id,
                                byte_len,
                            } => Some((content_id, byte_len)),
                            CacheDecision::Rehash(_) => None,
                        },
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some((content_id, byte_len)) = cache_hit {
                    stat_cache_hit_paths.insert(path.clone());
                    let reused_locator = request.context.base_locators.get(&content_id).cloned();
                    (
                        NamespaceEntryKind::File,
                        Some(content_id),
                        None,
                        reused_locator,
                        Some(byte_len),
                        HydrationState::Local,
                        None,
                    )
                } else {
                    let prepared = match prepare_content(PrepareContentRequest {
                        workspace_id: &request.workspace_id,
                        workspace_content_key: request.workspace_content_key,
                        workspace_root: request.root,
                        preparation_root: request.context.preparation_root,
                        relative_path: &path,
                        absolute_path: &absolute_path,
                        scan_fingerprint: observed.stat,
                        override_bytes,
                        portable_git_worktree_link: portable_git_worktree_link.is_some(),
                        created_at: &created_at,
                        owner_marker: preparation_owner_marker.as_ref(),
                    }) {
                        Ok(prepared) => prepared,
                        Err(CoalesceError::PrepareContent { source, .. })
                            if source.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let byte_len = Some(prepared.logical_len);
                    let content_id = prepared.content_id.clone();
                    if let Some(cached_content_id) = verify_cached_content_id
                        && cached_content_id != content_id
                    {
                        if let Some(session) = stat_cache.as_deref_mut() {
                            session.record_divergence();
                        }
                        stat_cache_divergences.push(StatCacheDivergence {
                            path: path.clone(),
                            cached_content_id,
                            observed_content_id: content_id.clone(),
                        });
                    }
                    if override_bytes.is_none()
                        && let (Some(session), Some(stat)) =
                            (stat_cache.as_deref_mut(), observed.stat)
                    {
                        session.record_hashed(
                            &path,
                            stat,
                            content_id.clone(),
                            prepared.logical_len,
                            created_at.clone(),
                        );
                    }
                    let reused_locator = request.context.base_locators.get(&content_id).cloned();
                    (
                        NamespaceEntryKind::File,
                        Some(content_id),
                        Some(prepared),
                        reused_locator,
                        byte_len,
                        HydrationState::Local,
                        None,
                    )
                }
            };
        if let Some(prepared) = prepared {
            let Some(content_id) = content_id.clone() else {
                return Err(CoalesceError::MissingFileContentId { path: path.clone() });
            };
            retain_one_prepared_source(&mut files, content_id, prepared)?;
        }
        namespace_builder.apply(
            NamespaceMutation::Upsert(NamespaceEntry {
                path,
                kind,
                classification: observed.policy.classification,
                mode: observed.policy.mode,
                access: observed.policy.access.clone(),
                content_id,
                content_layout: locator,
                symlink_target,
                byte_len,
                // Bytes are re-read here, but executability comes from scan-time
                // metadata; a chmod racing the scan lands on the next scan cycle.
                executability: observed.executability,
                hydration_state,
            }),
            &mut namespace_operation,
        )?;
    }
    apply_preserved_entries(
        &mut namespace_builder,
        request.context,
        &scan_report,
        &mut namespace_operation,
    )?;
    let (namespace, manifest_identity) =
        finalize_workspace_head_namespace(namespace_builder.finish(&mut namespace_operation)?);
    let snapshot_id = namespace.snapshot_id.clone();

    let (scan_stats, stat_cache_write_back) =
        finish_stat_cache(stat_cache, delete_scope, &scan_report);

    let mut snapshot = SnapshotContent::from_built(namespace, files);
    if let Some(owner_marker) = preparation_owner_marker {
        snapshot.attach_preparation_owner_marker(owner_marker);
    }
    Ok(SnapshotCandidate {
        base: CandidateBase::from_remote(request.base_ref),
        device_id: request.device_id,
        manifest_id: manifest_id_for_snapshot(&snapshot_id),
        snapshot,
        scan_report,
        scan_scope,
        stat_cache_hit_paths,
        stat_cache_divergences,
        scan_stats,
        manifest_identity,
        stat_cache_write_back,
        causation_ids: vec![format!("scan:{}", request.base_ref.version)],
        skipped_unsafe_symlinks,
        created_at,
    })
}

fn finalize_workspace_head_namespace(
    mut namespace: super::namespace::BuiltPagedNamespaceSnapshot,
) -> (
    super::namespace::BuiltPagedNamespaceSnapshot,
    ManifestIdentityReport,
) {
    let snapshot_id = namespace.snapshot_id.clone();
    namespace.metadata.schema_version = DEFAULT_SCHEMA_VERSION;
    namespace.metadata.project_id = None;
    namespace.metadata.kind = SnapshotKind::WorkspaceHead;
    namespace.metadata.base_snapshot_id = None;
    namespace.metadata.refs = vec![WorkspaceRef {
        name: "workspace".to_string(),
        target_snapshot_id: snapshot_id.clone(),
        kind: RefKind::Workspace,
    }];
    let identity = ManifestIdentityReport {
        snapshot_id,
        semantic_manifest_digest: namespace.semantic_manifest_digest.clone(),
        entries_hashed: namespace.changed.semantic_entries_hashed,
    };
    (namespace, identity)
}

fn apply_preserved_entries(
    builder: &mut PageNamespaceBuilder,
    context: CoalesceContext<'_>,
    scan_report: &ScanReport,
    operation: &mut NamespaceOperationContext<'_>,
) -> Result<(), CoalesceError> {
    for entry in context.preserved_entries {
        if is_git_derivable_volatile_path(&entry.path)
            && worktree_link_file(&entry.path, entry.kind).is_none()
        {
            continue;
        }
        if has_observed_non_directory_ancestor(&entry.path, scan_report) {
            continue;
        }
        let path_has_override = context.file_overrides.contains_key(&entry.path);
        if (context.paths.contains(&entry.path) && !path_has_override)
            || scan_report.path_observation(&entry.path).is_none()
        {
            builder.apply(NamespaceMutation::Upsert(entry.clone()), operation)?;
        }
    }
    Ok(())
}

fn preparation_owner_marker_for(
    request: &CoalesceWorkspaceReportRequest<'_>,
    created_at: &str,
) -> Option<PreparationOwnerMarker> {
    request
        .context
        .preparation_root
        .map(|_| next_preparation_owner_marker(&request.workspace_id, created_at))
}

fn out_of_root_worktree_prefixes(
    request: &CoalesceWorkspaceReportRequest<'_>,
) -> Result<BTreeSet<String>, CoalesceError> {
    let mut prefixes = BTreeSet::new();
    for observed in &request.report.paths {
        if observed.is_dir || observed.is_symlink {
            continue;
        }
        let path = normalize_workspace_path(&observed.path);
        let Some(prefix) = worktree_registration_prefix_for_gitdir(&path) else {
            continue;
        };
        let bytes = match fs::read(request.root.join(&path)) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(CoalesceError::ReadPath {
                    path: path.clone(),
                    source,
                });
            }
        };
        if is_out_of_root_admin_target(&bytes, request.root) {
            prefixes.insert(prefix);
        }
    }
    Ok(prefixes)
}

fn worktree_registration_prefix_for_gitdir(path: &str) -> Option<String> {
    if !is_worktree_admin_pointer(path) {
        return None;
    }
    if !path.ends_with("/gitdir") {
        return None;
    }
    worktree_registration_prefix(path)
}

fn path_is_under_any_prefix(path: &str, prefixes: &BTreeSet<String>) -> bool {
    prefixes.iter().any(|prefix| path.starts_with(prefix))
}

fn finish_stat_cache(
    stat_cache: Option<&mut StatCacheSession>,
    delete_scope: StatCacheDeleteScope<'_>,
    scan_report: &ScanReport,
) -> (ScanStats, Option<StatCacheWriteBack>) {
    let scan_stats = stat_cache
        .as_deref()
        .map(|session| session.stats().clone())
        .unwrap_or_default();
    let write_back = stat_cache.map(|session| {
        session.finish_with_delete_scope_matching(delete_scope, |path| {
            scan_report.path_observation(path).is_some()
        })
    });
    (scan_stats, write_back)
}

pub fn syncs_to_workspace_head(mode: MaterializationMode) -> bool {
    matches!(
        mode,
        MaterializationMode::WorkspaceSync
            | MaterializationMode::EncryptedSync
            | MaterializationMode::ProjectEnv
            | MaterializationMode::Lazy
    )
}

fn portable_git_worktree_link_file(
    path: &str,
    is_dir: bool,
    is_symlink: bool,
) -> Option<WorktreeLinkFile> {
    if is_dir || is_symlink {
        return None;
    }
    worktree_link_file(path, NamespaceEntryKind::File)
}

fn is_private_state_path(path: &str) -> bool {
    path == ".bowline"
        || path.starts_with(".bowline/")
        || path
            .split('/')
            .any(|part| part.starts_with(".bowline-materialize-") && part.ends_with(".tmp"))
}

fn has_observed_non_directory_ancestor(path: &str, scan_report: &ScanReport) -> bool {
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        if scan_report
            .path_observation(parent)
            .is_some_and(|observation| !observation.is_dir)
        {
            return true;
        }
        current = parent;
    }
    false
}

fn read_workspace_symlink_target(path: &Path) -> Result<String, std::io::Error> {
    Ok(path_to_slash_string(&fs::read_link(path)?))
}

fn should_skip_unsafe_symlink(
    path: &str,
    target: &str,
    skipped_unsafe_symlinks: &mut BTreeSet<String>,
) -> bool {
    if is_safe_workspace_symlink_target(target) {
        return false;
    }
    skipped_unsafe_symlinks.insert(path.to_string());
    true
}

fn path_to_slash_string(path: &Path) -> String {
    path.components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Debug)]
pub enum CoalesceError {
    Scan(ScanError),
    Namespace(bowline_core::namespace_snapshot::NamespaceBuildError),
    ReadPath {
        path: String,
        source: std::io::Error,
    },
    MissingFileContentId {
        path: String,
    },
    PrepareContent {
        path: String,
        field: &'static str,
        source: std::io::Error,
    },
    SourceChangedDuringPreparation {
        path: String,
    },
    PreparationLimitExceeded {
        path: String,
        limit_bytes: u64,
    },
}

impl fmt::Display for CoalesceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scan(error) => error.fmt(formatter),
            Self::Namespace(error) => error.fmt(formatter),
            Self::ReadPath { path, source } => {
                write!(formatter, "failed to read syncable path `{path}`: {source}")
            }
            Self::MissingFileContentId { path } => {
                write!(
                    formatter,
                    "file path `{path}` was read without a content id"
                )
            }
            Self::PrepareContent {
                path,
                field,
                source,
            } => write!(
                formatter,
                "failed to prepare syncable path `{path}` at {field}: {source}"
            ),
            Self::SourceChangedDuringPreparation { path } => {
                write!(
                    formatter,
                    "syncable path `{path}` changed during preparation"
                )
            }
            Self::PreparationLimitExceeded { path, limit_bytes } => write!(
                formatter,
                "syncable path `{path}` exceeds preparation limit of {limit_bytes} bytes"
            ),
        }
    }
}

impl Error for CoalesceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::Namespace(error) => Some(error),
            Self::ReadPath { source, .. } => Some(source),
            Self::PrepareContent { source, .. } => Some(source),
            Self::MissingFileContentId { .. }
            | Self::SourceChangedDuringPreparation { .. }
            | Self::PreparationLimitExceeded { .. } => None,
        }
    }
}

impl From<ScanError> for CoalesceError {
    fn from(error: ScanError) -> Self {
        Self::Scan(error)
    }
}

impl From<NamespaceBuildError> for CoalesceError {
    fn from(error: NamespaceBuildError) -> Self {
        Self::Namespace(error)
    }
}

impl From<NamespaceReadError> for CoalesceError {
    fn from(error: NamespaceReadError) -> Self {
        Self::Namespace(error.into())
    }
}

#[cfg(test)]
mod tests;
